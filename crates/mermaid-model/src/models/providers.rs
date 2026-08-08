//! Provider profiles for the OpenAI-compatible adapter.
//!
//! Every OpenAI-compatible provider (Groq, Together, Fireworks, OpenRouter,
//! vLLM, DeepInfra, Cerebras, SambaNova, LMStudio, llama.cpp, …) speaks
//! roughly the same `/v1/chat/completions` shape. The differences fit into
//! two small dimensions:
//!
//!   1. How they want **reasoning depth** in the request. The de-facto
//!      standard is a string `reasoning_effort: "low"|"medium"|"high"`
//!      field; OpenRouter wraps it in a `reasoning: {effort: …}` object
//!      and adds a few extras; some providers ignore reasoning entirely.
//!   2. Where they put **reasoning content** in the streaming response.
//!      Some emit `delta.reasoning_content`, some `delta.reasoning`, and
//!      a couple stuff `<think>...</think>` tags inline in `delta.content`.
//!
//! `ProviderProfile` captures both dimensions plus base URL, auth env
//! var, and any analytics headers (OpenRouter wants `HTTP-Referer` +
//! `X-Title`). A `pub const REGISTRY` lists the known providers; users
//! can override the URL / auth env / headers per-provider via
//! `[providers.<name>]` in `config.toml` and add fully custom providers
//! by reusing a known profile.

use serde::Deserialize;
use serde_json::{Value, json};

use super::reasoning::{ReasoningChunk, ReasoningLevel};

/// Static description of one OpenAI-compatible provider.
#[derive(Debug, Clone)]
pub struct ProviderProfile {
    /// Provider identifier as it appears in model IDs (e.g. `"groq"` for
    /// `groq/qwen-qwq-32b`). Lowercased; matched case-insensitively.
    pub name: &'static str,
    /// Default base URL for `/chat/completions` and friends. The trailing
    /// `/v1` (or equivalent) is included so adapter code just appends
    /// `/chat/completions` etc.
    pub base_url: &'static str,
    /// Default env var holding the API key. User config can override.
    pub api_key_env: &'static str,
    /// Where to get an API key, appended to the "missing key" error so the
    /// message is actionable. `None` falls back to just naming the env var.
    pub key_hint: Option<&'static str>,
    /// Headers always sent in addition to `Authorization: Bearer ...`.
    /// OpenRouter requires `HTTP-Referer` + `X-Title` for its analytics
    /// dashboard; everyone else uses an empty list.
    pub extra_headers: &'static [(&'static str, &'static str)],
    /// How to render `ReasoningLevel` into the request body.
    pub reasoning_strategy: ReasoningStrategy,
    /// Where reasoning content lives in the streaming response.
    pub reasoning_extraction: ReasoningExtraction,
    /// Which completion-budget parameter this provider accepts in
    /// `/chat/completions`.
    pub max_tokens_param: MaxTokensParam,
    /// Model IDs that support tools but must be forced to single tool-call
    /// mode because the provider default enables unsupported parallel calls.
    pub disable_parallel_tool_calls_for: &'static [&'static str],
}

/// Provider-specific spelling for the completion-token budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxTokensParam {
    /// OpenAI-compatible legacy spelling.
    MaxTokens,
    /// Newer OpenAI-compatible spelling used by Cerebras.
    MaxCompletionTokens,
}

/// How to put `ReasoningLevel` onto the wire for a given provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningStrategy {
    /// Provider exposes no reasoning controls (Together, DeepInfra
    /// pass-through). Adapter sends nothing extra.
    None,
    /// Standard `reasoning_effort: "low"|"medium"|"high"` field
    /// (OpenAI Chat Completions, Groq for gpt-oss, Cerebras for
    /// gpt-oss-120b, Fireworks for Qwen 3, etc.).
    Effort,
    /// OpenRouter's normalized `reasoning: {effort: "..."}` nested
    /// object. Supports `low`, `medium`, `high`, `max`. `None` becomes
    /// `{exclude: true}` (suppresses reasoning).
    OpenRouterShape,
}

impl ReasoningStrategy {
    /// Render a `ReasoningLevel` to the JSON fragment that should be
    /// merged into the `/chat/completions` request body. Returns `None`
    /// if there's nothing to add (strategy is `None`, or the level is
    /// `None` for a provider that signals via field omission).
    #[must_use]
    pub fn render(&self, level: ReasoningLevel) -> Option<Value> {
        match self {
            Self::None => None,
            Self::Effort => match level {
                // `none` is the explicit off-tier on GPT-5.1+. Providers
                // that don't understand it either silently ignore or 400 —
                // which is a clearer failure than omitting the field when
                // the user explicitly asked for it.
                ReasoningLevel::None => Some(json!({"reasoning_effort": "none"})),
                ReasoningLevel::Minimal => Some(json!({"reasoning_effort": "minimal"})),
                ReasoningLevel::Low => Some(json!({"reasoning_effort": "low"})),
                ReasoningLevel::Medium => Some(json!({"reasoning_effort": "medium"})),
                ReasoningLevel::High => Some(json!({"reasoning_effort": "high"})),
                // XHigh renders verbatim to "xhigh" — the dedicated OpenAI
                // GPT-5.2+ tier. Non-OpenAI Effort providers (Groq,
                // Cerebras, Fireworks) will 400 on "xhigh"; that's
                // preferable to silently downgrading the user's explicit
                // choice.
                ReasoningLevel::XHigh => Some(json!({"reasoning_effort": "xhigh"})),
                // Max collapses to "high" on Effort-shape providers.
                // OpenAI's Effort enum doesn't have a "max" value (goes
                // `...high | xhigh` and stops); users wanting OpenAI's
                // top tier should pick `XHigh` explicitly. Providers
                // with a genuine "max" tier (Anthropic, OpenRouter) use
                // their own strategy, not this one.
                ReasoningLevel::Max => Some(json!({"reasoning_effort": "high"})),
            },
            Self::OpenRouterShape => match level {
                ReasoningLevel::None => Some(json!({"reasoning": {"exclude": true}})),
                ReasoningLevel::Minimal => Some(json!({"reasoning": {"effort": "low"}})),
                ReasoningLevel::Low => Some(json!({"reasoning": {"effort": "low"}})),
                ReasoningLevel::Medium => Some(json!({"reasoning": {"effort": "medium"}})),
                ReasoningLevel::High => Some(json!({"reasoning": {"effort": "high"}})),
                // OpenRouter has no `xhigh` tier. Since XHigh sits between
                // High and Max, snap DOWN to `high` — the user picked
                // something above high but below max; giving them max would
                // over-deliver.
                ReasoningLevel::XHigh => Some(json!({"reasoning": {"effort": "high"}})),
                ReasoningLevel::Max => Some(json!({"reasoning": {"effort": "max"}})),
            },
        }
    }
}

/// Where reasoning content shows up in a streaming response delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningExtraction {
    /// Provider doesn't stream reasoning content (OpenAI Chat Completions
    /// for o-series — encrypted server-side).
    None,
    /// Reasoning arrives in `delta.<field>` of every streaming chunk.
    /// Common values: `"reasoning_content"` (vLLM, DeepInfra, DeepSeek)
    /// and `"reasoning"` (Groq parsed mode, OpenRouter).
    DeltaContentField(&'static str),
    /// Reasoning is `<think>...</think>` inline in `delta.content`.
    /// Together-R1, Groq raw mode, Fireworks `/think` suffix all do this.
    /// Adapter strips tags and reroutes inside-tag bytes to the
    /// reasoning channel via a streaming state machine.
    InlineThinkTags,
}

impl ReasoningExtraction {
    /// Pull reasoning content out of a streaming delta JSON. Returns
    /// `None` if this strategy doesn't extract from the JSON body
    /// (`None` and `InlineThinkTags`) or if the delta has no reasoning.
    /// `InlineThinkTags` is handled separately at the byte-stream level
    /// in the adapter; this method returns `None` for it.
    #[must_use]
    pub fn parse_delta(&self, delta: &Value) -> Option<ReasoningChunk> {
        match self {
            Self::None | Self::InlineThinkTags => None,
            Self::DeltaContentField(field) => {
                let text = delta.get(field).and_then(|v| v.as_str())?;
                if text.is_empty() {
                    None
                } else {
                    Some(ReasoningChunk {
                        text: text.to_string(),
                        signature: None,
                    })
                }
            },
        }
    }
}

/// User-friendly string form for `compat = "..."` in config.toml when a
/// fully custom provider needs to declare which profile shape to follow.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CompatStyle {
    /// Standard OpenAI Chat Completions shape, no reasoning extras
    /// (matches Together, DeepInfra, Cerebras for non-gpt-oss models).
    Openai,
    /// Same shape but with `reasoning_effort` on requests.
    OpenaiEffort,
    /// OpenRouter's normalized reasoning object.
    Openrouter,
}

impl CompatStyle {
    #[must_use]
    pub fn reasoning_strategy(self) -> ReasoningStrategy {
        match self {
            Self::Openai => ReasoningStrategy::None,
            Self::OpenaiEffort => ReasoningStrategy::Effort,
            Self::Openrouter => ReasoningStrategy::OpenRouterShape,
        }
    }
}

/// Built-in provider registry. Lookups are case-insensitive on `name`.
/// Add a provider here when its quirks fit the existing strategies; add
/// a new `ReasoningStrategy` variant when a provider needs something
/// the existing ones can't express.
pub const REGISTRY: &[ProviderProfile] = &[
    ProviderProfile {
        name: "openai",
        base_url: "https://api.openai.com/v1",
        api_key_env: "OPENAI_API_KEY",
        key_hint: Some("create one at https://platform.openai.com/api-keys"),
        extra_headers: &[],
        reasoning_strategy: ReasoningStrategy::Effort,
        // Chat Completions doesn't stream reasoning content for o-series
        // (encrypted server-side); only the Responses API does. Step 2
        // targets Chat Completions, so None.
        reasoning_extraction: ReasoningExtraction::None,
        max_tokens_param: MaxTokensParam::MaxTokens,
        disable_parallel_tool_calls_for: &[],
    },
    ProviderProfile {
        name: "groq",
        base_url: "https://api.groq.com/openai/v1",
        api_key_env: "GROQ_API_KEY",
        key_hint: Some("create one at https://console.groq.com/keys"),
        extra_headers: &[],
        reasoning_strategy: ReasoningStrategy::Effort,
        // Default `reasoning_format=parsed` routes reasoning to its own
        // `delta.reasoning` field; we read it from there.
        reasoning_extraction: ReasoningExtraction::DeltaContentField("reasoning"),
        max_tokens_param: MaxTokensParam::MaxTokens,
        disable_parallel_tool_calls_for: &[],
    },
    ProviderProfile {
        name: "openrouter",
        base_url: "https://openrouter.ai/api/v1",
        api_key_env: "OPENROUTER_API_KEY",
        key_hint: Some("create one at https://openrouter.ai/keys"),
        extra_headers: &[
            ("HTTP-Referer", "https://github.com/noahsabaj/mermaid-cli"),
            // Canonical attribution header as of April 2026. OpenRouter
            // still accepts `X-Title` for backward compat, but new code
            // should emit `X-OpenRouter-Title`.
            ("X-OpenRouter-Title", "Mermaid"),
        ],
        reasoning_strategy: ReasoningStrategy::OpenRouterShape,
        reasoning_extraction: ReasoningExtraction::DeltaContentField("reasoning"),
        max_tokens_param: MaxTokensParam::MaxTokens,
        disable_parallel_tool_calls_for: &[],
    },
    ProviderProfile {
        name: "cerebras",
        base_url: "https://api.cerebras.ai/v1",
        api_key_env: "CEREBRAS_API_KEY",
        key_hint: Some("create one at https://cloud.cerebras.ai"),
        extra_headers: &[],
        // Effort-style request param. `gpt-oss-120b` and `zai-glm-4.7`
        // honor it (the latter accepts `none` to disable); other models
        // silently ignore — wire shape is the same.
        reasoning_strategy: ReasoningStrategy::Effort,
        reasoning_extraction: ReasoningExtraction::None,
        max_tokens_param: MaxTokensParam::MaxCompletionTokens,
        disable_parallel_tool_calls_for: &["gpt-oss-120b"],
    },
    ProviderProfile {
        name: "deepinfra",
        base_url: "https://api.deepinfra.com/v1/openai",
        api_key_env: "DEEPINFRA_API_KEY",
        key_hint: Some("create one at https://deepinfra.com/dash/api_keys"),
        extra_headers: &[],
        // Pass-through; reasoning shape per upstream model. Most R1-style
        // models on DeepInfra emit `delta.reasoning_content`.
        reasoning_strategy: ReasoningStrategy::None,
        reasoning_extraction: ReasoningExtraction::DeltaContentField("reasoning_content"),
        max_tokens_param: MaxTokensParam::MaxTokens,
        disable_parallel_tool_calls_for: &[],
    },
    ProviderProfile {
        name: "together",
        base_url: "https://api.together.xyz/v1",
        api_key_env: "TOGETHER_API_KEY",
        key_hint: Some("create one at https://api.together.ai/settings/api-keys"),
        extra_headers: &[],
        reasoning_strategy: ReasoningStrategy::None,
        // DeepSeek-R1 and friends on Together emit `<think>...</think>`
        // inside `delta.content`. Adapter strips and reroutes.
        reasoning_extraction: ReasoningExtraction::InlineThinkTags,
        max_tokens_param: MaxTokensParam::MaxTokens,
        disable_parallel_tool_calls_for: &[],
    },
    ProviderProfile {
        name: "nvidia",
        base_url: "https://integrate.api.nvidia.com/v1",
        api_key_env: "NVIDIA_API_KEY",
        key_hint: Some("create one at https://build.nvidia.com"),
        extra_headers: &[],
        // NVIDIA NIM is a plain OpenAI-compatible endpoint. Its own snippets
        // send no reasoning param, so `None` keeps the request to exactly what
        // NIM documents — no risk of a rejected `reasoning_effort`. Reasoning
        // models like GLM-5.2 still show their trace via the extraction below.
        reasoning_strategy: ReasoningStrategy::None,
        // GLM-5.2 (and Nemotron) stream thinking in `delta.reasoning_content`,
        // the same shape as DeepInfra.
        reasoning_extraction: ReasoningExtraction::DeltaContentField("reasoning_content"),
        max_tokens_param: MaxTokensParam::MaxTokens,
        disable_parallel_tool_calls_for: &[],
    },
    ProviderProfile {
        name: "cloudflare",
        // Documentary placeholder only — never sent a request. Cloudflare Workers
        // AI's real endpoint embeds a per-account id; `providers::factory`
        // synthesizes the actual base_url at runtime from `CLOUDFLARE_ACCOUNT_ID`
        // (or a `[providers.cloudflare].base_url` override) for chat requests AND
        // for discovery surfaces (`doctor`, the best-effort `/models` probe) via
        // `factory::discovery_base_url`.
        base_url: "https://api.cloudflare.com/client/v4/accounts/ACCOUNT_ID/ai/v1",
        api_key_env: "CLOUDFLARE_API_TOKEN",
        key_hint: Some(
            "create a token at https://dash.cloudflare.com/profile/api-tokens and set \
             CLOUDFLARE_ACCOUNT_ID",
        ),
        extra_headers: &[],
        // GLM-5.2 accepts `reasoning_effort` (Cloudflare documents it), so expose the
        // reasoning-level selector via Effort — the same shape Cerebras uses. Non-reasoning
        // Cloudflare models silently ignore the field.
        reasoning_strategy: ReasoningStrategy::Effort,
        // GLM-5.2 streams its thinking in `delta.reasoning_content`, like NVIDIA/DeepInfra.
        reasoning_extraction: ReasoningExtraction::DeltaContentField("reasoning_content"),
        max_tokens_param: MaxTokensParam::MaxTokens,
        disable_parallel_tool_calls_for: &[],
    },
];

/// Look up a built-in provider by name. Case-insensitive.
#[must_use]
pub fn lookup_provider(name: &str) -> Option<&'static ProviderProfile> {
    let lower = name.to_lowercase();
    REGISTRY.iter().find(|p| p.name == lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Registry lookup ---

    #[test]
    fn lookup_known_provider() {
        let p = lookup_provider("groq").expect("groq is in the registry");
        assert_eq!(p.name, "groq");
        assert!(p.base_url.starts_with("https://api.groq.com"));
        assert_eq!(p.api_key_env, "GROQ_API_KEY");
    }

    #[test]
    fn lookup_nvidia_provider() {
        let p = lookup_provider("nvidia").expect("nvidia is in the registry");
        assert_eq!(p.name, "nvidia");
        assert_eq!(p.base_url, "https://integrate.api.nvidia.com/v1");
        assert_eq!(p.api_key_env, "NVIDIA_API_KEY");
        // GLM-5.2 streams its reasoning trace in `delta.reasoning_content`.
        assert_eq!(
            p.reasoning_extraction,
            ReasoningExtraction::DeltaContentField("reasoning_content")
        );
    }

    #[test]
    fn lookup_cloudflare_provider() {
        let p = lookup_provider("cloudflare").expect("cloudflare is in the registry");
        assert_eq!(p.name, "cloudflare");
        assert_eq!(p.api_key_env, "CLOUDFLARE_API_TOKEN");
        // Effort so the reasoning-level selector actually drives GLM-5.2 on Cloudflare
        // (contrast the nvidia entry, which is None and inert).
        assert_eq!(p.reasoning_strategy, ReasoningStrategy::Effort);
        // GLM-5.2 streams its reasoning trace in `delta.reasoning_content`.
        assert_eq!(
            p.reasoning_extraction,
            ReasoningExtraction::DeltaContentField("reasoning_content")
        );
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert!(lookup_provider("OpenAI").is_some());
        assert!(lookup_provider("OPENROUTER").is_some());
    }

    #[test]
    fn lookup_unknown_provider() {
        assert!(lookup_provider("does-not-exist").is_none());
    }

    #[test]
    fn registry_has_eight_providers() {
        assert_eq!(REGISTRY.len(), 8);
    }

    #[test]
    fn openrouter_has_analytics_headers() {
        let p = lookup_provider("openrouter").unwrap();
        let names: Vec<&str> = p.extra_headers.iter().map(|(k, _)| *k).collect();
        assert!(names.contains(&"HTTP-Referer"));
        // Canonical header name as of 2026-04. `X-Title` is still
        // accepted for backward compat but new code emits the rebranded
        // version.
        assert!(names.contains(&"X-OpenRouter-Title"));
    }

    // --- ReasoningStrategy::render ---

    #[test]
    fn effort_renders_string_per_level() {
        let s = ReasoningStrategy::Effort;
        // `None` is now the explicit off-tier per GPT-5.1+; we emit the
        // string rather than omitting the field so the user's choice
        // reaches the provider.
        assert_eq!(
            s.render(ReasoningLevel::None),
            Some(json!({"reasoning_effort": "none"})),
        );
        assert_eq!(
            s.render(ReasoningLevel::Low),
            Some(json!({"reasoning_effort": "low"})),
        );
        assert_eq!(
            s.render(ReasoningLevel::Medium),
            Some(json!({"reasoning_effort": "medium"})),
        );
        assert_eq!(
            s.render(ReasoningLevel::High),
            Some(json!({"reasoning_effort": "high"})),
        );
        // XHigh — OpenAI GPT-5.2+ tier. Sits between High and Max in
        // our enum but on the wire it's OpenAI's actual top string.
        // Providers that don't expose xhigh will 400.
        assert_eq!(
            s.render(ReasoningLevel::XHigh),
            Some(json!({"reasoning_effort": "xhigh"})),
        );
        // Max collapses to high — OpenAI's Effort enum has no "max".
        // Users wanting OpenAI's actual top tier should pick XHigh.
        assert_eq!(
            s.render(ReasoningLevel::Max),
            Some(json!({"reasoning_effort": "high"})),
        );
    }

    #[test]
    fn openrouter_shape_renders_nested_object() {
        let s = ReasoningStrategy::OpenRouterShape;
        // None means "exclude" on OpenRouter — explicitly suppress
        // reasoning rather than fall through to the model default.
        assert_eq!(
            s.render(ReasoningLevel::None),
            Some(json!({"reasoning": {"exclude": true}})),
        );
        assert_eq!(
            s.render(ReasoningLevel::Medium),
            Some(json!({"reasoning": {"effort": "medium"}})),
        );
        assert_eq!(
            s.render(ReasoningLevel::Max),
            Some(json!({"reasoning": {"effort": "max"}})),
        );
        // OpenRouter has no xhigh tier; XHigh (between High and Max)
        // snaps DOWN to `high` — don't over-deliver by bumping to max.
        assert_eq!(
            s.render(ReasoningLevel::XHigh),
            Some(json!({"reasoning": {"effort": "high"}})),
        );
    }

    #[test]
    fn none_strategy_renders_nothing() {
        let s = ReasoningStrategy::None;
        for level in [
            ReasoningLevel::None,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::Max,
        ] {
            assert_eq!(s.render(level), None);
        }
    }

    // --- ReasoningExtraction::parse_delta ---

    #[test]
    fn delta_field_extraction_finds_named_field() {
        let e = ReasoningExtraction::DeltaContentField("reasoning_content");
        let delta = json!({"reasoning_content": "weighing options", "content": ""});
        let chunk = e.parse_delta(&delta).expect("should extract");
        assert_eq!(chunk.text, "weighing options");
        assert!(chunk.signature.is_none());
    }

    #[test]
    fn delta_field_extraction_returns_none_when_absent() {
        let e = ReasoningExtraction::DeltaContentField("reasoning_content");
        let delta = json!({"content": "regular text"});
        assert!(e.parse_delta(&delta).is_none());
    }

    #[test]
    fn delta_field_extraction_returns_none_for_empty_string() {
        let e = ReasoningExtraction::DeltaContentField("reasoning");
        let delta = json!({"reasoning": ""});
        assert!(e.parse_delta(&delta).is_none());
    }

    #[test]
    fn none_extraction_always_returns_none() {
        let e = ReasoningExtraction::None;
        assert!(e.parse_delta(&json!({"reasoning_content": "x"})).is_none());
    }

    #[test]
    fn inline_think_tags_does_not_parse_via_json() {
        // Inline tags are handled at the byte-stream level in the
        // adapter (Wave 6); this method always returns None for them.
        let e = ReasoningExtraction::InlineThinkTags;
        assert!(
            e.parse_delta(&json!({"content": "<think>x</think>"}))
                .is_none()
        );
    }

    // --- CompatStyle ---

    #[test]
    fn compat_style_maps_to_strategy() {
        assert_eq!(
            CompatStyle::Openai.reasoning_strategy(),
            ReasoningStrategy::None
        );
        assert_eq!(
            CompatStyle::OpenaiEffort.reasoning_strategy(),
            ReasoningStrategy::Effort
        );
        assert_eq!(
            CompatStyle::Openrouter.reasoning_strategy(),
            ReasoningStrategy::OpenRouterShape
        );
    }
}
