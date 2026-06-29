//! Pure, I/O-free sizing logic for Ollama's `num_ctx` and `num_predict`.
//!
//! Ollama defaults `num_ctx` to a tiny window (~4096) and never receives an
//! output cap from Mermaid, so long prompts and reasoning models truncate. This
//! module derives both numbers from the model's *real* capabilities (probed via
//! `/api/show`) and the host's memory, so users never have to touch Ollama
//! config.
//!
//! Everything here is pure: detection (memory, the `/api/show` probe) happens in
//! the caller and is passed in via [`NumCtxInputs`]. The same inputs are built at
//! both call sites — the request path (`providers/model/ollama.rs`) and the
//! compaction/UI path (`effect/mod.rs`) — so the effective window can never
//! disagree between what Ollama is told and what compaction/the status bar
//! assume.

use crate::constants::{
    DEFAULT_OLLAMA_MAX_AUTO_NUM_CTX, OLLAMA_KV_DTYPE_BYTES, OLLAMA_MEMORY_BUDGET_FRACTION,
    OLLAMA_MIN_AUTO_NUM_CTX, OLLAMA_MIN_NUM_PREDICT, OLLAMA_NUM_CTX_ROUNDING,
    OLLAMA_NUM_PREDICT_MARGIN,
};
use crate::models::reasoning::ReasoningLevel;

/// How the effective `num_ctx` was chosen — surfaced in `/context` and the
/// quick-fix hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NumCtxSource {
    /// Per-model override set via `/context <n>` / `/context max`.
    Override,
    /// Global `[ollama] num_ctx` from config.
    GlobalConfig,
    /// Auto-fit to detected memory, capped at the model's max window.
    Auto,
    /// Auto, but memory/dimensions couldn't be detected, so the conservative
    /// fallback cap was used.
    AutoFallback,
}

impl NumCtxSource {
    /// Human label for `/context` and hints.
    pub fn label(self) -> &'static str {
        match self {
            NumCtxSource::Override => "override",
            NumCtxSource::GlobalConfig => "config",
            NumCtxSource::Auto => "auto",
            NumCtxSource::AutoFallback => "auto (fallback)",
        }
    }

    /// Whether the window was auto-fitted (vs an explicit user/config value) —
    /// drives whether the quick-fix offers to raise it.
    pub fn is_auto(self) -> bool {
        matches!(self, NumCtxSource::Auto | NumCtxSource::AutoFallback)
    }
}

/// Resolved effective `num_ctx` plus how it was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumCtxResolution {
    pub value: usize,
    pub source: NumCtxSource,
}

/// Architecture dimensions from `/api/show` `model_info`, used to estimate the
/// KV-cache cost per token. All fields are required for the estimate; a missing
/// one collapses to "unknown" and the resolver falls back to the conservative cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ModelDims {
    pub block_count: usize,
    pub head_count: usize,
    pub head_count_kv: usize,
    pub embedding_length: usize,
}

/// KV-cache bytes per token (both K and V tensors, fp16). `None` if any
/// dimension is missing/zero.
///
/// `2 (K+V) * block_count * head_count_kv * head_dim * bytes`, where
/// `head_dim = embedding_length / head_count`. Using `head_count_kv` (not
/// `head_count`) accounts for grouped-query attention, which most modern models
/// use to shrink the KV cache.
pub fn kv_bytes_per_token(dims: &ModelDims) -> Option<usize> {
    if dims.block_count == 0
        || dims.head_count == 0
        || dims.head_count_kv == 0
        || dims.embedding_length == 0
    {
        return None;
    }
    let head_dim = dims.embedding_length / dims.head_count;
    if head_dim == 0 {
        return None;
    }
    2usize
        .checked_mul(dims.block_count)?
        .checked_mul(dims.head_count_kv)?
        .checked_mul(head_dim)?
        .checked_mul(OLLAMA_KV_DTYPE_BYTES)
}

/// Largest number of tokens whose KV cache fits `budget_bytes` after reserving a
/// headroom fraction and subtracting the model weights. `None` if
/// `kv_bytes_per_token` is zero.
pub fn max_tokens_for_memory(
    budget_bytes: u64,
    model_weight_bytes: u64,
    kv_bytes_per_token: usize,
) -> Option<usize> {
    if kv_bytes_per_token == 0 {
        return None;
    }
    let usable = (budget_bytes as f64 * OLLAMA_MEMORY_BUDGET_FRACTION) as u64;
    let for_kv = usable.saturating_sub(model_weight_bytes);
    Some((for_kv / kv_bytes_per_token as u64) as usize)
}

/// Auto-converge step: when a turn spilled out of VRAM, the largest `num_ctx`
/// that drops enough KV-cache tokens to clear the measured `total - size_vram`
/// overflow (rounded down to a clean step, floored at the usable minimum).
///
/// Works from the *observed* footprint, so it implicitly accounts for compute
/// buffers and Ollama's own headroom rather than re-estimating them.
///
/// Returns `None` when shrinking can't actually make the model fit — crucially,
/// when even cutting all the way to the floor wouldn't clear the overflow. That
/// means the spill is the *weights*, not the KV cache, so shrinking the window
/// would cripple it without fixing anything; the caller must warn instead of
/// flooring uselessly (a tiny window wedges the session — every turn truncates).
/// When it does return a value it is strictly `< current` *and* genuinely clears
/// the overflow, so feeding it back each turn converges to the largest
/// fully-resident window.
pub fn converge_num_ctx(
    current: usize,
    size_vram_bytes: u64,
    total_bytes: u64,
    kv_bytes_per_token: usize,
) -> Option<usize> {
    if kv_bytes_per_token == 0 || total_bytes <= size_vram_bytes {
        return None; // KV cost unknown, or it actually fit — nothing to do.
    }
    let overflow = total_bytes - size_vram_bytes;
    // Round the division up so we cut at least the overflow, never one short.
    let tokens_to_cut = overflow.div_ceil(kv_bytes_per_token as u64) as usize;
    let after_cut = current.saturating_sub(tokens_to_cut);
    // If clearing the overflow would require dropping below the floor, the model
    // is weights-bound: no window size makes it fit, so don't shrink at all.
    // Flooring would neither stop the spill nor leave a usable window.
    if after_cut < OLLAMA_MIN_AUTO_NUM_CTX {
        return None;
    }
    let target = round_down_to(after_cut, OLLAMA_NUM_CTX_ROUNDING).max(OLLAMA_MIN_AUTO_NUM_CTX);
    (target < current).then_some(target)
}

/// Everything [`resolve_ollama_num_ctx`] needs. Built identically at both call
/// sites so the effective window stays consistent.
#[derive(Debug, Clone, Copy, Default)]
pub struct NumCtxInputs {
    /// The model's architectural max window (from `/api/show`). Without it, auto
    /// can't run and the resolver returns `None` (caller omits `num_ctx`).
    pub model_max: Option<usize>,
    /// Architecture dimensions for the KV-cache estimate.
    pub dims: Option<ModelDims>,
    /// Model weight bytes (from `/api/show` `size`), subtracted from the budget.
    pub model_weight_bytes: Option<u64>,
    /// Per-model override (`/context <n>`); highest precedence.
    pub per_model_override: Option<u32>,
    /// Global `[ollama] num_ctx`; beats auto, loses to the override.
    pub global_num_ctx: Option<i32>,
    /// When true, auto-fit budgets against system RAM (allows the slow GPU/CPU
    /// split); when false (default), budgets against VRAM so the model stays on
    /// the GPU.
    pub allow_ram_offload: bool,
    /// Total VRAM in bytes (best-effort), used as the budget when offload is off.
    pub vram_bytes: Option<u64>,
    /// Total system RAM in bytes, used as the budget when offload is on.
    pub system_ram_bytes: Option<u64>,
    /// Optional hard cap on the auto value (`[ollama] max_auto_num_ctx`).
    pub max_auto_cap: Option<usize>,
}

/// Resolve the effective `num_ctx`. Precedence: **per-model override > global
/// config > auto-fit**. Returns `None` only when nothing is known (no override,
/// no global, no probed `model_max`) — the caller then omits `num_ctx` entirely
/// and Ollama uses its own default.
pub fn resolve_ollama_num_ctx(inputs: &NumCtxInputs) -> Option<NumCtxResolution> {
    // 1. Explicit per-model override wins outright.
    if let Some(n) = inputs.per_model_override.filter(|n| *n > 0) {
        return Some(NumCtxResolution {
            value: n as usize,
            source: NumCtxSource::Override,
        });
    }
    // 2. Global config num_ctx.
    if let Some(n) = inputs.global_num_ctx.filter(|n| *n > 0) {
        return Some(NumCtxResolution {
            value: n as usize,
            source: NumCtxSource::GlobalConfig,
        });
    }
    // 3. Auto-fit. Needs the model's max window to bound everything.
    let model_max = inputs.model_max.filter(|m| *m > 0)?;

    let budget_bytes = if inputs.allow_ram_offload {
        inputs.system_ram_bytes
    } else {
        inputs.vram_bytes
    };
    let kv = inputs.dims.as_ref().and_then(kv_bytes_per_token);

    let (raw_target, source) = match (budget_bytes, kv) {
        (Some(b), Some(kv)) => {
            let fit = max_tokens_for_memory(b, inputs.model_weight_bytes.unwrap_or(0), kv)
                .unwrap_or(DEFAULT_OLLAMA_MAX_AUTO_NUM_CTX);
            (fit, NumCtxSource::Auto)
        },
        // Memory or dimensions unknown → conservative fixed fallback.
        _ => (DEFAULT_OLLAMA_MAX_AUTO_NUM_CTX, NumCtxSource::AutoFallback),
    };

    // Round the memory-derived target to a clean step (no-op for the fallback,
    // which is already a clean multiple).
    let target = round_down_to(raw_target, OLLAMA_NUM_CTX_ROUNDING);

    // The model's own max binds exactly (don't round it down and waste tokens).
    let mut value = target.min(model_max);
    if let Some(cap) = inputs.max_auto_cap.filter(|c| *c > 0) {
        value = value.min(cap);
    }
    // Floor at a usable minimum, but never above the model's own max.
    value = value.max(OLLAMA_MIN_AUTO_NUM_CTX.min(model_max));

    Some(NumCtxResolution { value, source })
}

fn round_down_to(v: usize, step: usize) -> usize {
    if step == 0 {
        return v;
    }
    (v / step) * step
}

/// Output tokens reserved for reasoning/thinking so a thinking model doesn't burn
/// the whole answer budget before responding. Scales with the effort level.
pub fn reasoning_output_reserve(level: ReasoningLevel) -> usize {
    match level {
        ReasoningLevel::None | ReasoningLevel::Minimal => 0,
        ReasoningLevel::Low => 1_024,
        ReasoningLevel::Medium => 4_096,
        ReasoningLevel::High | ReasoningLevel::XHigh | ReasoningLevel::Max => 8_192,
    }
}

/// Resolve `num_predict` (Ollama's output cap). Every other adapter forwards
/// `max_tokens`; Ollama is the only one that left output unbounded, so a small
/// `num_ctx` was the only stopping condition. This maps `max_tokens` →
/// `num_predict` plus reasoning headroom, capped by the room left in `num_ctx`
/// after the prompt, floored at `min_output`.
pub fn resolve_ollama_num_predict(
    max_tokens: usize,
    reasoning: ReasoningLevel,
    num_ctx: Option<usize>,
    prompt_estimate: usize,
    min_output: usize,
    margin: usize,
) -> i32 {
    let desired = max_tokens.saturating_add(reasoning_output_reserve(reasoning));
    let bounded = match num_ctx {
        Some(ctx) => {
            let room = ctx.saturating_sub(prompt_estimate).saturating_sub(margin);
            desired.min(room).max(min_output)
        },
        None => desired,
    };
    bounded.min(i32::MAX as usize) as i32
}

/// Convenience wrapper applying the repo's default floor/margin to
/// [`resolve_ollama_num_predict`].
pub fn default_ollama_num_predict(
    max_tokens: usize,
    reasoning: ReasoningLevel,
    num_ctx: Option<usize>,
    prompt_estimate: usize,
) -> i32 {
    resolve_ollama_num_predict(
        max_tokens,
        reasoning,
        num_ctx,
        prompt_estimate,
        OLLAMA_MIN_NUM_PREDICT,
        OLLAMA_NUM_PREDICT_MARGIN,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // A realistic GQA model (~7-9B, like the user's ornith): 28 layers, 28 heads,
    // 4 KV heads, 3584 embedding → head_dim 128 → 56 KB/token.
    fn gqa_dims() -> ModelDims {
        ModelDims {
            block_count: 28,
            head_count: 28,
            head_count_kv: 4,
            embedding_length: 3584,
        }
    }

    #[test]
    fn kv_bytes_per_token_gqa() {
        // 2 * 28 * 4 * (3584/28=128) * 2 = 57_344 bytes.
        assert_eq!(kv_bytes_per_token(&gqa_dims()), Some(57_344));
    }

    #[test]
    fn kv_bytes_per_token_missing_dims_is_none() {
        assert_eq!(kv_bytes_per_token(&ModelDims::default()), None);
        assert_eq!(
            kv_bytes_per_token(&ModelDims {
                block_count: 28,
                head_count: 0, // would divide by zero
                head_count_kv: 4,
                embedding_length: 3584,
            }),
            None
        );
    }

    #[test]
    fn max_tokens_for_memory_subtracts_weights_and_headroom() {
        // 8 GiB budget, ~5.6 GB model, 56 KB/token.
        let budget = 8 * 1024 * 1024 * 1024u64;
        let weight = 5_600_000_000u64;
        let kv = 57_344usize;
        let got = max_tokens_for_memory(budget, weight, kv).unwrap();
        // usable = 0.85 * 8GiB ≈ 7.3 GB; for_kv ≈ 1.7 GB; /56KB ≈ 30k tokens.
        assert!((28_000..=32_000).contains(&got), "expected ~30k, got {got}");
    }

    #[test]
    fn converge_shrinks_to_clear_kv_overflow() {
        // 10k-token window spilled by ~2 GB of KV (2000 tokens @ 1 MB/token).
        let new = converge_num_ctx(10_000, 10_000_000_000, 12_000_000_000, 1_000_000)
            .expect("should shrink");
        assert!(new < 10_000, "must make progress, got {new}");
        // ~2000 tokens cut from 10000 → ~8000, rounded down to the 1024 step.
        assert!((6_000..=8_000).contains(&new), "expected ~7-8k, got {new}");
    }

    #[test]
    fn converge_none_when_overflow_is_weights_bound() {
        // Overflow worth more tokens than the window has above the floor → even
        // flooring wouldn't clear it (the spill is weights, not KV). Must NOT
        // shrink (that would wedge the session); the caller warns instead.
        let new = converge_num_ctx(8_000, 1_000_000_000, 9_000_000_000, 1_000_000);
        assert_eq!(new, None);
    }

    #[test]
    fn converge_some_only_when_shrink_actually_clears_overflow() {
        // A 16k window spilling by ~6k tokens of KV: cutting to ~10k clears it and
        // stays well above the floor → a real, useful shrink.
        let new = converge_num_ctx(16_000, 10_000_000_000, 16_000_000_000, 1_000_000)
            .expect("clearable overflow should shrink");
        assert!(
            (OLLAMA_MIN_AUTO_NUM_CTX..16_000).contains(&new),
            "got {new}"
        );
        // Same overflow on a window only just above the floor can't be cleared
        // without dropping below it → bail.
        assert_eq!(
            converge_num_ctx(5_000, 10_000_000_000, 16_000_000_000, 1_000_000),
            None
        );
    }

    #[test]
    fn converge_none_when_already_at_floor() {
        // Already at the floor and still spilling → caller must warn, not loop.
        let new = converge_num_ctx(
            OLLAMA_MIN_AUTO_NUM_CTX,
            1_000_000_000,
            9_000_000_000,
            1_000_000,
        );
        assert_eq!(new, None);
    }

    #[test]
    fn converge_none_when_it_fits_or_kv_unknown() {
        // Fully resident (total == vram) or under it → nothing to do.
        assert_eq!(
            converge_num_ctx(8_000, 6_000_000_000, 6_000_000_000, 40_000),
            None
        );
        assert_eq!(
            converge_num_ctx(8_000, 6_000_000_000, 5_000_000_000, 40_000),
            None
        );
        // KV cost unknown → can't compute → leave it alone.
        assert_eq!(
            converge_num_ctx(8_000, 1_000_000_000, 9_000_000_000, 0),
            None
        );
    }

    #[test]
    fn override_beats_everything() {
        let res = resolve_ollama_num_ctx(&NumCtxInputs {
            model_max: Some(262_144),
            per_model_override: Some(131_072),
            global_num_ctx: Some(8_192),
            vram_bytes: Some(8 * 1024 * 1024 * 1024),
            dims: Some(gqa_dims()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(res.value, 131_072);
        assert_eq!(res.source, NumCtxSource::Override);
    }

    #[test]
    fn global_config_beats_auto() {
        let res = resolve_ollama_num_ctx(&NumCtxInputs {
            model_max: Some(262_144),
            global_num_ctx: Some(16_384),
            vram_bytes: Some(8 * 1024 * 1024 * 1024),
            dims: Some(gqa_dims()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(res.value, 16_384);
        assert_eq!(res.source, NumCtxSource::GlobalConfig);
    }

    #[test]
    fn auto_fits_vram_and_caps_at_model_max() {
        // Huge VRAM but small model_max → model_max binds exactly.
        let res = resolve_ollama_num_ctx(&NumCtxInputs {
            model_max: Some(8_192),
            dims: Some(gqa_dims()),
            model_weight_bytes: Some(5_600_000_000),
            vram_bytes: Some(80 * 1024 * 1024 * 1024),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(res.value, 8_192);
        assert_eq!(res.source, NumCtxSource::Auto);
    }

    #[test]
    fn auto_fits_vram_below_model_max() {
        // 8 GB VRAM, 256K model → memory binds well under model_max, rounded.
        let res = resolve_ollama_num_ctx(&NumCtxInputs {
            model_max: Some(262_144),
            dims: Some(gqa_dims()),
            model_weight_bytes: Some(5_600_000_000),
            vram_bytes: Some(8 * 1024 * 1024 * 1024),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(res.source, NumCtxSource::Auto);
        assert!(res.value < 262_144 && res.value >= OLLAMA_MIN_AUTO_NUM_CTX);
        assert_eq!(res.value % OLLAMA_NUM_CTX_ROUNDING, 0);
    }

    #[test]
    fn offload_on_uses_system_ram() {
        // No VRAM, but offload allowed and lots of RAM → much bigger window than
        // the VRAM-off path (which would fall back to the cap).
        let res = resolve_ollama_num_ctx(&NumCtxInputs {
            model_max: Some(262_144),
            dims: Some(gqa_dims()),
            model_weight_bytes: Some(5_600_000_000),
            allow_ram_offload: true,
            system_ram_bytes: Some(64 * 1024 * 1024 * 1024),
            vram_bytes: None,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(res.source, NumCtxSource::Auto);
        assert!(
            res.value > DEFAULT_OLLAMA_MAX_AUTO_NUM_CTX,
            "64GB RAM should fit > 32k, got {}",
            res.value
        );
    }

    #[test]
    fn no_memory_detected_uses_fallback_cap() {
        let res = resolve_ollama_num_ctx(&NumCtxInputs {
            model_max: Some(262_144),
            dims: Some(gqa_dims()),
            vram_bytes: None,
            system_ram_bytes: None,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(res.value, DEFAULT_OLLAMA_MAX_AUTO_NUM_CTX);
        assert_eq!(res.source, NumCtxSource::AutoFallback);
    }

    #[test]
    fn max_auto_cap_clamps_auto() {
        let res = resolve_ollama_num_ctx(&NumCtxInputs {
            model_max: Some(262_144),
            dims: Some(gqa_dims()),
            model_weight_bytes: Some(5_600_000_000),
            vram_bytes: Some(80 * 1024 * 1024 * 1024),
            max_auto_cap: Some(16_384),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(res.value, 16_384);
    }

    #[test]
    fn floor_never_exceeds_model_max() {
        // Tiny model_max below the floor → use model_max, not the floor.
        let res = resolve_ollama_num_ctx(&NumCtxInputs {
            model_max: Some(2_048),
            vram_bytes: None,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(res.value, 2_048);
    }

    #[test]
    fn no_model_max_and_no_explicit_is_none() {
        // Probe failed and nothing configured → omit num_ctx (Ollama default).
        assert!(resolve_ollama_num_ctx(&NumCtxInputs::default()).is_none());
    }

    #[test]
    fn num_predict_adds_reasoning_reserve() {
        // Plenty of room → max_tokens + reserve.
        let np =
            resolve_ollama_num_predict(4_096, ReasoningLevel::Max, Some(131_072), 1_000, 512, 256);
        assert_eq!(np, 4_096 + 8_192);
    }

    #[test]
    fn num_predict_no_reserve_for_none() {
        let np =
            resolve_ollama_num_predict(4_096, ReasoningLevel::None, Some(131_072), 1_000, 512, 256);
        assert_eq!(np, 4_096);
    }

    #[test]
    fn num_predict_capped_by_remaining_room() {
        // num_ctx 8k, prompt 7k, margin 256 → room ≈ 744 → capped (above floor).
        let np =
            resolve_ollama_num_predict(4_096, ReasoningLevel::Medium, Some(8_192), 7_000, 512, 256);
        assert_eq!(np, 8_192 - 7_000 - 256);
    }

    #[test]
    fn num_predict_floored_when_room_tiny() {
        // Prompt nearly fills the window → fall back to the floor, not negative.
        let np =
            resolve_ollama_num_predict(4_096, ReasoningLevel::High, Some(8_192), 8_100, 512, 256);
        assert_eq!(np, 512);
    }

    #[test]
    fn num_predict_passthrough_without_ctx() {
        let np = resolve_ollama_num_predict(4_096, ReasoningLevel::Low, None, 1_000, 512, 256);
        assert_eq!(np, 4_096 + 1_024);
    }
}
