//! Ollama provider wrapping `OllamaAdapter`.
//!
//! The adapter owns the wire format (NDJSON framing, gpt-oss
//! reasoning dispatch, truncation marker, retry). The wrapper
//! translates `ChatRequest` ↔ `ModelConfig` and bridges the
//! adapter's legacy `StreamCallback` to the typed `StreamEvent`
//! sink. Adapter-internals stay where they are; the architecture
//! boundary is at `ModelProvider::chat`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::ChatRequest;
use crate::models::adapters::ollama::{OllamaAdapter, OllamaModelInfo};
use crate::models::adapters::ollama_sizing::{
    NumCtxInputs, converge_num_ctx, default_ollama_num_predict, kv_bytes_per_token,
    resolve_ollama_num_ctx,
};
use crate::models::{
    BackendConfig, Model, ModelConfig, ModelError, ReasoningChunk, Result, StreamCallback,
    StreamEvent as ModelStreamEvent,
};
use crate::runtime::{NewProviderProbe, RuntimeStore};

use super::super::capabilities::Capabilities;
use super::super::ctx::{FinalResponse, StreamContext, StreamEvent};
use super::{
    ContextSizing, ModelPlacement, ModelProvider, learn_output_cap, load_limits_from_db,
    output_cap_from_error, probe_is_stale, retry_cap,
};

/// Ollama adapter fronted by `ModelProvider`.
pub struct OllamaProvider {
    adapter: OllamaAdapter,
    capabilities: Capabilities,
    /// Shared app `Config` so `build_model_config` can read Ollama
    /// hardware options (`num_ctx`, `num_gpu`, `num_thread`, `numa`) at
    /// call time. Before F11 these were silently dropped because the
    /// wrapper built `ModelConfig` only from `ChatRequest` fields.
    config: Arc<crate::app::Config>,
    /// Cached `/api/show` probe (context window + dims + weight). Filled once per
    /// process per model — the provider itself is cached by `ProviderFactory`, so
    /// this fires a single network probe per model. Backed by the cross-session
    /// `provider_probes` table. Probe *failures* are not cached (left empty for a
    /// cheap retry next turn).
    ctx_cell: tokio::sync::OnceCell<OllamaModelInfo>,
}

impl OllamaProvider {
    /// Backward-compatible constructor that uses a default app config.
    /// Call `with_app_config` instead when you have one available so
    /// Ollama hardware options actually reach the adapter.
    pub async fn new(model_name: &str, backend: Arc<BackendConfig>) -> Result<Self> {
        Self::with_app_config(model_name, backend, Arc::new(crate::app::Config::default())).await
    }

    /// Construct with an explicit `app::Config` reference. Used by
    /// `ProviderFactory::build_provider` so `config.ollama.{num_gpu,
    /// num_ctx, num_thread, numa}` make it into the Ollama request's
    /// `options` block.
    pub async fn with_app_config(
        model_name: &str,
        backend: Arc<BackendConfig>,
        config: Arc<crate::app::Config>,
    ) -> Result<Self> {
        let adapter = OllamaAdapter::new(model_name, backend).await?;
        let capabilities = Capabilities::from_legacy(adapter.capabilities());
        Ok(Self {
            adapter,
            capabilities,
            config,
            ctx_cell: tokio::sync::OnceCell::new(),
        })
    }

    /// Probe the model's capabilities, cache-first. In-process `OnceCell` backed
    /// by the cross-session `provider_probes` table; failures aren't cached.
    async fn probe(&self) -> Option<OllamaModelInfo> {
        self.ctx_cell
            .get_or_try_init(|| async { self.load_probe().await.ok_or(()) })
            .await
            .ok()
            .cloned()
    }

    async fn load_probe(&self) -> Option<OllamaModelInfo> {
        let model = self.adapter.name().to_string();
        if let Some(info) = load_probe_from_db(model.clone()).await {
            return Some(info);
        }
        let info = self.adapter.show_model_info().await?;
        save_probe_to_db(model, info.clone()).await;
        Some(info)
    }

    /// Assemble the sizing inputs from the probe, host memory, config, and the
    /// request's per-model override. Built here and in the effect layer from the
    /// same (cached) sources so the effective window can't diverge between the
    /// request and compaction.
    async fn num_ctx_inputs(
        &self,
        info: &OllamaModelInfo,
        override_num_ctx: Option<u32>,
        override_offload: Option<bool>,
    ) -> NumCtxInputs {
        // Live `/context offload` toggle wins; otherwise the persisted default.
        // The provider's `config` is frozen at startup (the factory never
        // rebuilds), so the toggle rides on the request instead of `config`.
        let allow_ram_offload = override_offload.unwrap_or(self.config.ollama.allow_ram_offload);
        // Only fetch the budget we'll actually use: VRAM when keeping the model
        // on the GPU (default), system RAM when offload is allowed.
        let (vram_bytes, system_ram_bytes) = if allow_ram_offload {
            (None, crate::utils::system_ram_bytes())
        } else {
            (crate::utils::gpu_vram_bytes().await, None)
        };
        NumCtxInputs {
            model_max: info.context_length,
            dims: info.dims,
            model_weight_bytes: info.weight_bytes,
            per_model_override: override_num_ctx,
            global_num_ctx: self.config.ollama.num_ctx,
            allow_ram_offload,
            vram_bytes,
            system_ram_bytes,
            max_auto_cap: self.config.ollama.max_auto_num_ctx,
            is_cloud: crate::ollama::is_cloud_model(self.adapter.name()),
        }
    }
}

#[async_trait]
impl ModelProvider for OllamaProvider {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn resolve_context_window(&self, request: &ChatRequest) -> ContextSizing {
        let info = self.probe().await.unwrap_or_default();
        let inputs = self
            .num_ctx_inputs(
                &info,
                request.ollama_num_ctx,
                request.ollama_allow_ram_offload,
            )
            .await;
        let model_max = inputs.model_max;
        // Local Ollama imposes no per-response ceiling (num_predict is ours),
        // but Ollama Cloud maps num_predict → max_tokens and 400s above the
        // model's real cap. A cap learned from such a 400 lives in the limits
        // cache; feed it forward so sizing computes min(window room, cap).
        let max_output =
            load_limits_from_db("ollama".to_string(), Model::name(&self.adapter).to_string())
                .await
                .and_then(|l| l.max_output_tokens);
        match resolve_ollama_num_ctx(&inputs) {
            Some(r) => ContextSizing {
                model_max,
                effective: Some(r.value),
                source: Some(r.source),
                max_output,
            },
            // No model_max and nothing configured → omit num_ctx (Ollama default).
            None => ContextSizing {
                model_max,
                effective: None,
                source: None,
                max_output,
            },
        }
    }

    async fn verify_placement(&self, current_num_ctx: Option<usize>) -> Option<ModelPlacement> {
        let (vram, total) = self.adapter.model_placement().await?;
        // A zero total means Ollama reported the model but not its footprint —
        // can't judge placement, so leave it unknown rather than guess.
        if total == 0 {
            return None;
        }
        // On a spill, compute the largest num_ctx that would fit, from the model's
        // KV cost (probe dims) and the *measured* overflow — the auto-converge
        // target. `None` if it already fits or shrinking can't help.
        let suggested_num_ctx = if vram < total {
            let info = self.probe().await.unwrap_or_default();
            current_num_ctx
                .zip(info.dims)
                .and_then(|(current, dims)| {
                    let kv = kv_bytes_per_token(&dims)?;
                    converge_num_ctx(current, vram, total, kv)
                })
                .map(|n| n as u32)
        } else {
            None
        };
        Some(ModelPlacement {
            size_vram_bytes: vram,
            total_bytes: total,
            suggested_num_ctx,
        })
    }

    async fn supports_vision(&self) -> Option<bool> {
        Some(self.adapter.vision_supported().await)
    }

    async fn chat(&self, request: ChatRequest, ctx: StreamContext) -> Result<FinalResponse> {
        // Resolve the effective window first (cache-first probe). Idempotent with
        // the effect layer's call — both read the same cached probe + memory, so
        // what we send as num_ctx matches what compaction assumes.
        let sizing = self.resolve_context_window(&request).await;
        let config =
            build_model_config(&request, &self.config, sizing.effective, sizing.max_output);
        // Ordered relay (F2): the adapter's sync callback pushes into an
        // `UnboundedSender` (synchronous, FIFO). A single relay task drains
        // into the bounded sink in order, avoiding the per-event `tokio::
        // spawn` race that could deliver `Done` before prior tool calls.
        let (relay_tx, relay_handle) = super::stream_bridge::ordered_relay(ctx.sink.clone());
        let callback = stream_callback_for(relay_tx.clone());

        // Race adapter.chat against the cancellation token. When
        // cancelled, the adapter's stream loop observes the sink
        // closing (we drop `callback`) and exits at its next await.
        // This is the crucial structural win vs. the old
        // `check_interrupt` polling: the adapter doesn't need to
        // know anything about turn IDs — the sink either drains or
        // doesn't, and the tokens handle everything else.
        let chat_fut = async {
            match self
                .adapter
                .chat(&request.messages, &config, Some(callback.clone()))
                .await
            {
                Ok(response) => Ok(response),
                Err(err) => {
                    // Learn-from-400: Ollama Cloud rejects a num_predict above
                    // the model's real output cap and names the cap in the
                    // body. Learn it (persisted — later turns size below it up
                    // front), clamp, retry ONCE. A request that 400s streamed
                    // no events, so the relay is untouched and reusable.
                    let Some(cap) = output_cap_from_error(&err) else {
                        return Err(err);
                    };
                    let sent = config
                        .ollama_options()
                        .num_predict
                        .map_or(0, |v| v.max(0) as usize);
                    if retry_cap(sent, cap).is_none() {
                        return Err(err);
                    }
                    let model = Model::name(&self.adapter).to_string();
                    learn_output_cap("ollama".to_string(), model.clone(), cap).await;
                    let _ = relay_tx.send(StreamEvent::Status(format!(
                        "{model} rejected the output budget; learned its {cap}-token cap and retrying"
                    )));
                    let retry_config =
                        build_model_config(&request, &self.config, sizing.effective, Some(cap));
                    self.adapter
                        .chat(&request.messages, &retry_config, Some(callback.clone()))
                        .await
                },
            }
        };

        let response = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => {
                // Terminal event for a cancelled turn comes from the
                // runner's `drop_scope` once the `TurnScope` drains, so
                // we neither emit `StreamEvent::Done` here nor surface
                // an `UpstreamError`. `ModelError::Cancelled` is the
                // sentinel the runner swallows.
                return Err(ModelError::Cancelled);
            },
            r = chat_fut => r?,
        };

        // F3: the wrapper's `Done` is now the sole terminal event —
        // the adapter no longer emits one from the callback. Carrying
        // `provider_continuation` out of `ModelResponse` here is what
        // lets multi-turn extended thinking round-trip.
        let usage = response.usage.clone();
        let provider_continuation = response.provider_continuation.clone();
        let stop_reason = response.stop_reason.clone();
        // Terminal Done through the ordered relay, then drain (see stream_bridge).
        let _ = relay_tx.send(StreamEvent::Done {
            usage: usage.clone(),
            provider_continuation: provider_continuation.clone(),
            stop_reason: stop_reason.clone(),
        });
        drop(relay_tx);
        crate::utils::join_logged(relay_handle.take(), "stream_relay").await;

        Ok(FinalResponse {
            usage,
            provider_continuation,
            tool_calls: response.tool_calls.unwrap_or_default(),
            stop_reason,
        })
    }
}

// ─── helpers ────────────────────────────────────────────────────────

/// `num_ctx` is the resolved effective window (auto-fitted, or override/global);
/// passing it here keeps a single source of truth — the direct `config.ollama.
/// num_ctx` forward is gone because the resolver already considered it.
/// `provider_max_output` is a known per-response ceiling (learned from an
/// Ollama Cloud 400), bounding the derived `num_predict`.
fn build_model_config(
    request: &ChatRequest,
    app_config: &crate::app::Config,
    num_ctx: Option<usize>,
    provider_max_output: Option<usize>,
) -> ModelConfig {
    let mut mc = ModelConfig {
        model: request.model_id.clone(),
        temperature: request.temperature,
        max_tokens: request.max_tokens,
        reasoning: request.reasoning,
        system_prompt: Some(request.system_prompt.clone()),
        dynamic_system_suffix: request.instructions.clone(),
        tools: request.tools.iter().map(|t| t.to_openai_json()).collect(),
        ..Default::default()
    };
    // Effective context window (auto-fitted to memory / override / global).
    if let Some(n) = num_ctx {
        mc.set_backend_option("ollama".into(), "num_ctx".into(), n.to_string());
    }
    // Output cap: AUTO (max_tokens == 0) gets the full room num_ctx leaves
    // after the prompt; an explicit max_tokens is an exact cap bounded by that
    // room. Without a cap Ollama generates unbounded and only stops when the
    // window fills — the truncation bug. `None` = AUTO with an unknown window →
    // omit num_predict so Ollama applies its own default.
    if let Some(num_predict) = default_ollama_num_predict(
        request.max_tokens,
        num_ctx,
        estimate_prompt_tokens(request),
        provider_max_output,
    ) {
        mc.set_backend_option(
            "ollama".into(),
            "num_predict".into(),
            num_predict.to_string(),
        );
    }

    // F11: forward Ollama hardware options from the user's app config (num_ctx is
    // now handled above via the resolver).
    if let Some(v) = app_config.ollama.num_gpu {
        mc.set_backend_option("ollama".into(), "num_gpu".into(), v.to_string());
    }
    if let Some(v) = app_config.ollama.num_thread {
        mc.set_backend_option("ollama".into(), "num_thread".into(), v.to_string());
    }
    if let Some(v) = app_config.ollama.numa {
        mc.set_backend_option("ollama".into(), "numa".into(), v.to_string());
    }
    mc
}

/// Rough prompt-token estimate (≈4 chars/token) for bounding `num_predict`
/// against the remaining room in `num_ctx`. Approximate by design — it only
/// gates the output cap, never the prompt itself.
fn estimate_prompt_tokens(request: &ChatRequest) -> usize {
    let chars = request.system_prompt.len()
        + request.instructions.as_deref().map_or(0, str::len)
        + request
            .messages
            .iter()
            .map(|m| m.content.len())
            .sum::<usize>();
    chars / 4
}

/// Load a cached probe from `provider_probes` (within TTL). Runs the blocking
/// SQLite read off the async runtime. Best-effort: any failure → `None`.
async fn load_probe_from_db(model: String) -> Option<OllamaModelInfo> {
    tokio::task::spawn_blocking(move || {
        let store = RuntimeStore::open_default().ok()?;
        let rec = store
            .provider_probes()
            .get("ollama", &model, "context_probe")
            .ok()??;
        if probe_is_stale(&rec.probed_at) {
            return None;
        }
        serde_json::from_str::<OllamaModelInfo>(&rec.capability_value).ok()
    })
    .await
    .ok()
    .flatten()
}

/// Persist a probe to `provider_probes` for subsequent sessions. Best-effort.
async fn save_probe_to_db(model: String, info: OllamaModelInfo) {
    let _ = tokio::task::spawn_blocking(move || -> Option<()> {
        let value = serde_json::to_string(&info).ok()?;
        let store = RuntimeStore::open_default().ok()?;
        store
            .provider_probes()
            .upsert(NewProviderProbe {
                provider: "ollama".into(),
                model_id: model,
                capability_key: "context_probe".into(),
                capability_value: value,
                confidence: "probed".into(),
                error: None,
            })
            .ok()?;
        Some(())
    })
    .await;
}

/// Build a `StreamCallback` that forwards `ModelStreamEvent`s from the
/// adapter into an `UnboundedSender<StreamEvent>` (ordered relay). The
/// caller wires that sender to a bounded sink via
/// `stream_bridge::ordered_relay`; this keeps event delivery FIFO even
/// though the adapter calls us from a sync context.
fn stream_callback_for(sink: tokio::sync::mpsc::UnboundedSender<StreamEvent>) -> StreamCallback {
    Arc::new(move |event: ModelStreamEvent| {
        let mapped = match event {
            ModelStreamEvent::Text(s) => StreamEvent::Text(s),
            ModelStreamEvent::Reasoning(chunk) => StreamEvent::Reasoning(ReasoningChunk {
                text: chunk.text,
                signature: chunk.signature,
            }),
            ModelStreamEvent::ToolCall(tc) => StreamEvent::ToolCall(tc),
            ModelStreamEvent::Status(s) => StreamEvent::Status(s),
            // No adapter emits `Done` through this callback — the wrapper
            // sends the authoritative terminal `Done` built from the
            // returned `ModelResponse` (F3). Map defensively without
            // inventing usage (the old placeholder misfiled everything
            // as completion tokens).
            ModelStreamEvent::Done { .. } => StreamEvent::Done {
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        };
        // Synchronous send preserves ordering. Ignore errors — the
        // receiver has closed means the turn is already gone.
        let _ = sink.send(mapped);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_model_config_maps_request_fields() {
        let req = ChatRequest {
            model_id: "ollama/test".to_string(),
            messages: vec![],
            system_prompt: "sys".to_string(),
            instructions: Some("instructions text".to_string()),
            reasoning: crate::models::ReasoningLevel::High,
            temperature: 0.3,
            max_tokens: 2048,
            tools: vec![],

            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: None,
            resolved_max_output: None,
        };
        let app_cfg = crate::app::Config::default();
        let cfg = build_model_config(&req, &app_cfg, None, None);
        assert_eq!(cfg.model, "ollama/test");
        assert_eq!(cfg.temperature, 0.3);
        assert_eq!(cfg.max_tokens, 2048);
        assert_eq!(cfg.reasoning, crate::models::ReasoningLevel::High);
        assert_eq!(cfg.system_prompt.as_deref(), Some("sys"));
        assert_eq!(
            cfg.dynamic_system_suffix.as_deref(),
            Some("instructions text")
        );
    }

    /// F11 regression guard: Ollama hardware options in the user's app config
    /// must land in the ModelConfig's backend_options so the adapter's
    /// `build_request_body` emits them under `options`. `num_ctx` now arrives via
    /// the resolver param (not a direct config forward), and `num_predict` is
    /// always derived.
    #[test]
    fn build_model_config_forwards_ollama_hardware_options() {
        let req = ChatRequest {
            model_id: "ollama/test".to_string(),
            messages: vec![],
            system_prompt: "sys".to_string(),
            instructions: None,
            reasoning: crate::models::ReasoningLevel::Medium,
            temperature: 0.7,
            max_tokens: 4096,
            tools: vec![],

            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: None,
            resolved_max_output: None,
        };
        let mut app_cfg = crate::app::Config::default();
        app_cfg.ollama.num_gpu = Some(10);
        app_cfg.ollama.num_thread = Some(8);
        app_cfg.ollama.numa = Some(true);

        // The effective num_ctx (8192) is passed in by the resolver.
        let cfg = build_model_config(&req, &app_cfg, Some(8192), None);
        let opts = cfg.ollama_options();
        assert_eq!(opts.num_ctx, Some(8192));
        assert_eq!(opts.num_gpu, Some(10));
        assert_eq!(opts.num_thread, Some(8));
        assert_eq!(opts.numa, Some(true));
        assert!(opts.num_predict.is_some(), "num_predict is always derived");
    }

    /// An explicit `max_tokens` is an exact cap, bounded by the room left in
    /// num_ctx (no reasoning reserve added — hard means hard).
    #[test]
    fn build_model_config_derives_num_predict() {
        let req = ChatRequest {
            model_id: "ollama/test".to_string(),
            messages: vec![],
            system_prompt: String::new(),
            instructions: None,
            reasoning: crate::models::ReasoningLevel::Max,
            temperature: 0.7,
            max_tokens: 4096,
            tools: vec![],

            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: None,
            resolved_max_output: None,
        };
        let cfg = build_model_config(&req, &crate::app::Config::default(), Some(131_072), None);
        // Exactly the 4096 cap; plenty of room in a 131072 window.
        assert_eq!(cfg.ollama_options().num_predict, Some(4_096));
    }

    /// The minimax incident, recomputed: a learned provider cap bounds the
    /// AUTO num_predict below the full window room, so the retry (and every
    /// later turn) sends a value Ollama Cloud accepts.
    #[test]
    fn build_model_config_caps_num_predict_at_learned_ceiling() {
        let req = ChatRequest {
            model_id: "ollama/minimax-m3:cloud".to_string(),
            messages: vec![],
            system_prompt: String::new(),
            instructions: None,
            reasoning: crate::models::ReasoningLevel::Medium,
            temperature: 0.7,
            max_tokens: 0, // AUTO — the incident's configuration
            tools: vec![],

            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: None,
            resolved_max_output: None,
        };
        let app_cfg = crate::app::Config::default();
        // Without a learned cap, AUTO hands over the full window room —
        // 524_288 minus margin — which Ollama Cloud 400s for minimax-m3.
        let uncapped = build_model_config(&req, &app_cfg, Some(524_288), None);
        assert!(uncapped.ollama_options().num_predict.unwrap() > 131_072);
        // With the learned 131_072 cap, sizing stays at the ceiling.
        let capped = build_model_config(&req, &app_cfg, Some(524_288), Some(131_072));
        assert_eq!(capped.ollama_options().num_predict, Some(131_072));
    }

    #[tokio::test]
    async fn stream_callback_forwards_text_event() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cb = stream_callback_for(tx);
        cb(ModelStreamEvent::Text("hello".to_string()));
        let recv = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("recv")
            .expect("sender alive");
        match recv {
            StreamEvent::Text(s) => assert_eq!(s, "hello"),
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn stream_callback_forwards_status_notice() {
        // The autostart notice rides the same ordered relay as content
        // events; the bridge must map it 1:1 so it reaches the effect
        // layer (→ Msg::TransientStatus → system line / stderr).
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cb = stream_callback_for(tx);
        cb(ModelStreamEvent::Status(
            "Starting the local Ollama server…".to_string(),
        ));
        let recv = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("recv")
            .expect("sender alive");
        match recv {
            StreamEvent::Status(s) => assert!(s.contains("Starting")),
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn stream_callback_done_never_invents_usage() {
        // The wrapper's terminal Done (built from ModelResponse) is the only
        // authoritative usage carrier; a callback Done must map to None
        // rather than misfiling its bare count as completion tokens.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cb = stream_callback_for(tx);
        cb(ModelStreamEvent::Done { tokens: 42 });
        let recv = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("recv")
            .expect("sender");
        match recv {
            StreamEvent::Done { usage, .. } => assert!(usage.is_none()),
            _ => panic!("wrong variant"),
        }
    }
}
