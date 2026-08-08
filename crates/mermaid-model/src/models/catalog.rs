//! The data-driven model-capability catalog: one FIRST-MATCH-WINS const table
//! replacing the model-name string gates that were scattered across the
//! adapters (anthropic thinking/temperature/effort tiers, gemini thinking
//! dispatch, ollama's gpt-oss think-string, openai-compat reasoning-model and
//! vision markers) and the domain's static context windows.
//!
//! Each COLUMN is consulted only by the consumers named on its field doc, so
//! a cross-provider match can never change behavior an adapter didn't already
//! have. Ollama's `/api/show` probes remain authoritative for local models'
//! vision/thinking/context — the catalog never overrides a probe.
//!
//! Deliberate behavior deltas from the old scattered gates (both accuracy
//! fixes, pinned by tests): matching is uniformly case-insensitive (the old
//! openai-compat reasoning gate was case-sensitive), and the gpt-5/gpt-4.1
//! static 400k context window now applies through any provider (previously
//! gated on `provider == "openai"`).

/// How a catalog row matches a model id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchRule {
    /// Anchors at the start of the BARE name (any `provider/` prefix stripped
    /// via the last `/`) — the historical `starts_with` gate semantics.
    Prefix(&'static str),
    /// Searches the FULL id including provider prefixes — the historical
    /// vision-marker semantics (the same model appears under many ids).
    Substring(&'static str),
}

impl MatchRule {
    /// Whether this rule matches the (lowercased) full id / bare name pair.
    fn matches(&self, full: &str, bare: &str) -> bool {
        match self {
            Self::Prefix(p) => bare.starts_with(p),
            Self::Substring(s) => full.contains(s),
        }
    }
}

/// Which thinking/reasoning wire shape the model's requests use.
/// Consumed by the anthropic, gemini, and ollama adapters — each reacts only
/// to its own variants and treats everything else as `ProviderDefault`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingShape {
    /// No catalog opinion: the adapter falls back to its provider default
    /// (anthropic → legacy `budget_tokens`; gemini → omit `thinkingConfig`;
    /// ollama → `think: bool` gated by the live probe; openai-compat →
    /// the `ProviderProfile` strategy).
    ProviderDefault,
    /// Anthropic 4.6+ adaptive thinking: `thinking: {type: "adaptive"}` +
    /// `output_config.effort` (legacy `budget_tokens` is rejected/deprecated).
    AnthropicAdaptive,
    /// Anthropic legacy thinking: `thinking: {type: "enabled", budget_tokens}`.
    AnthropicBudget,
    /// Gemini 3.x `thinkingLevel` enum (cannot truly disable).
    GeminiLevel,
    /// Gemini 2.5 integer `thinkingBudget`, clamped to the model's range.
    GeminiBudget {
        /// Lowest non-zero budget the model accepts.
        min: i32,
        /// Whether `thinkingBudget: 0` is valid for this model.
        can_disable: bool,
    },
    /// Ollama gpt-oss: `think: "low"|"medium"|"high"` string enum instead of
    /// the usual `think: bool`.
    OllamaEffortString,
}

/// Highest `output_config.effort` tier the model accepts (ordered). Consumed
/// by the anthropic adapter only. Every xhigh-capable model also accepts
/// `max` (verified against the effort doc), so one ordered ceiling encodes
/// the old `supports_effort` / `supports_max_effort` / `supports_xhigh_effort`
/// trio: `> None` ⇒ effort accepted, `>= Max` ⇒ "max" accepted,
/// `>= XHigh` ⇒ "xhigh" accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EffortCeiling {
    /// `output_config.effort` is rejected outright (4.5-family Sonnet/Haiku
    /// and older) — the request must carry no effort field.
    None,
    /// Accepts up to `"high"`.
    High,
    /// Accepts up to `"max"` (Opus 4.6 / Sonnet 4.6).
    Max,
    /// Accepts `"xhigh"` (Opus 4.7/4.8, Fable 5, Mythos).
    XHigh,
}

/// One catalog row: a match rule plus the capability columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapEntry {
    /// How this row matches (see [`MatchRule`]); first matching row wins.
    pub rule: MatchRule,
    /// Thinking wire shape — consumed by anthropic/gemini/ollama builders.
    pub thinking: ThinkingShape,
    /// Whether the model accepts a top-level `temperature`. Consumed by the
    /// anthropic and openai-compat request builders (the 4.6+ adaptive line
    /// and the o-series/gpt-5 reasoning models 400 on it).
    pub supports_temperature: bool,
    /// Highest accepted effort tier — consumed by the anthropic adapter only.
    pub effort_ceiling: EffortCeiling,
    /// Advertised vision capability — consumed by openai-compat
    /// `derive_capabilities` only (anthropic/gemini hardcode true; ollama
    /// probes `/api/show`). Never gates the send.
    pub vision: bool,
    /// Documented static context window — ONLY for models whose provider
    /// API exposes no limits (OpenAI's gpt rows). Everything else resolves
    /// live via `resolve_context_window`; `None` = unknown until discovery.
    pub context_window: Option<usize>,
}

/// Shorthand for a row that only marks vision support (the generic
/// multimodal-family markers).
const fn vision_marker(marker: &'static str) -> ModelCapEntry {
    ModelCapEntry {
        rule: MatchRule::Substring(marker),
        vision: true,
        ..UNKNOWN_MODEL
    }
}

/// Shorthand for an anthropic-family row (vision; window/output resolved
/// live from the Models API — no static pins, they rot).
const fn claude(
    rule: MatchRule,
    thinking: ThinkingShape,
    supports_temperature: bool,
    effort_ceiling: EffortCeiling,
) -> ModelCapEntry {
    ModelCapEntry {
        rule,
        thinking,
        supports_temperature,
        effort_ceiling,
        vision: true,
        context_window: None,
    }
}

/// The default row for models no rule matches: provider-default thinking,
/// temperature accepted, no effort, no vision, no static window.
pub const UNKNOWN_MODEL: ModelCapEntry = ModelCapEntry {
    rule: MatchRule::Substring(""),
    thinking: ThinkingShape::ProviderDefault,
    supports_temperature: true,
    effort_ceiling: EffortCeiling::None,
    vision: false,
    context_window: None,
};

use EffortCeiling as E;
use MatchRule::{Prefix, Substring};
use ThinkingShape as T;

/// The catalog. ORDER MATTERS — first match wins. Keep more-specific rules
/// above the family catch-alls they share a prefix/substring with (pinned by
/// the ordering tests): `gemini-2.5-flash-lite` before `gemini-2.5-flash`,
/// the specific `claude-*` prefixes before the bare `claude-` catch-all,
/// `gpt-5` Prefix (temperature rejected) before `gpt-5` Substring (accepted).
pub const CATALOG: &[ModelCapEntry] = &[
    // --- Anthropic, specific families (bare-name prefixes) ---
    claude(
        Prefix("claude-opus-4-7"),
        T::AnthropicAdaptive,
        false,
        E::XHigh,
    ),
    claude(
        Prefix("claude-opus-4-8"),
        T::AnthropicAdaptive,
        false,
        E::XHigh,
    ),
    claude(
        Prefix("claude-fable-5"),
        T::AnthropicAdaptive,
        false,
        E::XHigh,
    ),
    claude(
        Prefix("claude-mythos"),
        T::AnthropicAdaptive,
        false,
        E::XHigh,
    ),
    claude(
        Prefix("claude-opus-4-6"),
        T::AnthropicAdaptive,
        true,
        E::Max,
    ),
    claude(
        Prefix("claude-sonnet-4-6"),
        T::AnthropicAdaptive,
        true,
        E::Max,
    ),
    claude(Prefix("claude-opus-4-5"), T::AnthropicBudget, true, E::High),
    claude(
        Prefix("claude-sonnet-4-5"),
        T::AnthropicBudget,
        true,
        E::None,
    ),
    claude(
        Prefix("claude-haiku-4-5"),
        T::AnthropicBudget,
        true,
        E::None,
    ),
    // Opus 4.1, and (via the "-2" prefix) date-suffixed Opus 4 ids like
    // claude-opus-4-20250514 — both document a 32k output ceiling.
    claude(Prefix("claude-opus-4-1"), T::AnthropicBudget, true, E::None),
    claude(Prefix("claude-opus-4-2"), T::AnthropicBudget, true, E::None),
    // Claude 3.5 family: 8k output ceiling.
    claude(Prefix("claude-3-5"), T::AnthropicBudget, true, E::None),
    // --- Claude via gateways (full-id substrings; the vision-marker set) ---
    claude(
        Substring("claude-opus-4"),
        T::AnthropicBudget,
        true,
        E::None,
    ),
    claude(
        Substring("claude-sonnet-4"),
        T::AnthropicBudget,
        true,
        E::None,
    ),
    claude(
        Substring("claude-haiku-4"),
        T::AnthropicBudget,
        true,
        E::None,
    ),
    claude(Substring("claude-3"), T::AnthropicBudget, true, E::None),
    claude(Substring("claude-4"), T::AnthropicBudget, true, E::None),
    // Bare catch-all for any other anthropic-direct id (claude-2, future
    // names) — NOT in the gateway vision-marker set (claude-2 was never
    // advertised as vision-capable).
    ModelCapEntry {
        rule: Prefix("claude-"),
        thinking: T::AnthropicBudget,
        supports_temperature: true,
        effort_ceiling: E::None,
        vision: false,
        context_window: None,
    },
    // --- OpenAI reasoning models (temperature rejected) ---
    ModelCapEntry {
        rule: Prefix("o1"),
        ..OPENAI_REASONING
    },
    ModelCapEntry {
        rule: Prefix("o3"),
        ..OPENAI_REASONING
    },
    ModelCapEntry {
        rule: Prefix("o4"),
        ..OPENAI_REASONING
    },
    // Meta Model API's /v1/models exposes no limit metadata (Model schema is
    // id/object/created/owned_by/metadata — verified against the API
    // reference 2026-07-09), so like the gpt rows below the documented window
    // is static. Prefix so future muse-spark revisions inherit the family
    // window instead of regressing to unknown.
    ModelCapEntry {
        rule: Prefix("muse-spark"),
        vision: true,
        context_window: Some(crate::constants::META_MUSE_SPARK_CONTEXT_WINDOW),
        ..UNKNOWN_MODEL
    },
    // gpt-5.6 rows must stay ABOVE the gpt-5 rows — they share the prefix
    // and first match wins. OpenAI's /v1/models exposes no limits, so these
    // windows are static-but-documented (1.5M).
    ModelCapEntry {
        rule: Prefix("gpt-5.6"),
        supports_temperature: false,
        vision: true,
        context_window: Some(1_500_000),
        ..UNKNOWN_MODEL
    },
    ModelCapEntry {
        rule: Substring("gpt-5.6"),
        vision: true,
        context_window: Some(1_500_000),
        ..UNKNOWN_MODEL
    },
    ModelCapEntry {
        rule: Prefix("gpt-5"),
        supports_temperature: false,
        vision: true,
        context_window: Some(400_000),
        ..UNKNOWN_MODEL
    },
    // A gpt-5 id NOT at the start of the bare name (historically not a
    // "reasoning model", so temperature stays) — still vision + 400k.
    ModelCapEntry {
        rule: Substring("gpt-5"),
        vision: true,
        context_window: Some(400_000),
        ..UNKNOWN_MODEL
    },
    ModelCapEntry {
        rule: Substring("gpt-4.1"),
        vision: true,
        context_window: Some(400_000),
        ..UNKNOWN_MODEL
    },
    vision_marker("gpt-4o"),
    vision_marker("chatgpt-4o"),
    vision_marker("gpt-4-turbo"),
    vision_marker("gpt-4-vision"),
    // --- Gemini thinking dispatch (bare-name prefixes) ---
    ModelCapEntry {
        rule: Prefix("gemini-3"),
        thinking: T::GeminiLevel,
        vision: true,
        ..UNKNOWN_MODEL
    },
    ModelCapEntry {
        rule: Prefix("gemini-2.5-pro"),
        thinking: T::GeminiBudget {
            min: 128,
            can_disable: false,
        },
        vision: true,
        ..UNKNOWN_MODEL
    },
    // Flash-Lite must stay ABOVE Flash — they share the prefix.
    ModelCapEntry {
        rule: Prefix("gemini-2.5-flash-lite"),
        thinking: T::GeminiBudget {
            min: 512,
            can_disable: true,
        },
        vision: true,
        ..UNKNOWN_MODEL
    },
    ModelCapEntry {
        rule: Prefix("gemini-2.5-flash"),
        thinking: T::GeminiBudget {
            min: 0,
            can_disable: true,
        },
        vision: true,
        ..UNKNOWN_MODEL
    },
    // Older/other gemini ids: no thinkingConfig (2.0 and earlier 400 on it),
    // but the whole family is vision-capable.
    vision_marker("gemini"),
    // --- Ollama gpt-oss (bare-name prefix; matches tags like gpt-oss:20b) ---
    ModelCapEntry {
        rule: Prefix("gpt-oss"),
        thinking: T::OllamaEffortString,
        ..UNKNOWN_MODEL
    },
    // --- Generic multimodal families / vision markers ---
    vision_marker("-vision"),
    vision_marker("-vl"),
    vision_marker("vl-"),
    vision_marker("llava"),
    vision_marker("pixtral"),
    vision_marker("internvl"),
    vision_marker("molmo"),
    vision_marker("llama-3.2-11b"),
    vision_marker("llama-3.2-90b"),
    vision_marker("llama-4"),
    vision_marker("phi-3.5-vision"),
    vision_marker("phi-4-multimodal"),
    vision_marker("mistral-small-3"),
    vision_marker("gemma-3"),
];

/// Shared columns for the o-series reasoning rows.
const OPENAI_REASONING: ModelCapEntry = ModelCapEntry {
    rule: Substring(""),
    thinking: T::ProviderDefault,
    supports_temperature: false,
    effort_ceiling: E::None,
    vision: false,
    context_window: None,
};

/// Look up the capability row for a model id (bare or `provider/`-prefixed,
/// case-insensitive). Unmatched ids get [`UNKNOWN_MODEL`].
pub fn lookup(model_id: &str) -> &'static ModelCapEntry {
    let full = model_id.to_ascii_lowercase();
    let bare = full.rsplit('/').next().unwrap_or(full.as_str());
    CATALOG
        .iter()
        .find(|entry| entry.rule.matches(&full, bare))
        .unwrap_or(&UNKNOWN_MODEL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_model_gets_default_row() {
        let entry = lookup("totally-unknown-model");
        assert_eq!(entry.thinking, T::ProviderDefault);
        assert!(entry.supports_temperature);
        assert_eq!(entry.effort_ceiling, E::None);
        assert!(!entry.vision);
        assert_eq!(entry.context_window, None);
    }

    #[test]
    fn ordering_flash_lite_before_flash() {
        assert_eq!(
            lookup("gemini-2.5-flash-lite").thinking,
            T::GeminiBudget {
                min: 512,
                can_disable: true
            }
        );
        assert_eq!(
            lookup("gemini-2.5-flash-lite-preview").thinking,
            T::GeminiBudget {
                min: 512,
                can_disable: true
            }
        );
        assert_eq!(
            lookup("gemini-2.5-flash").thinking,
            T::GeminiBudget {
                min: 0,
                can_disable: true
            }
        );
    }

    #[test]
    fn ordering_gpt5_prefix_before_substring() {
        // Bare-name gpt-5 = reasoning model (temperature rejected)…
        assert!(!lookup("openai/gpt-5").supports_temperature);
        assert!(!lookup("gpt-5-mini").supports_temperature);
        // …while a mid-name gpt-5 keeps temperature but still gets vision+400k.
        let mid = lookup("some-gpt-5-variant");
        assert!(mid.supports_temperature);
        assert!(mid.vision);
        assert_eq!(mid.context_window, Some(400_000));
    }

    #[test]
    fn ordering_specific_claude_before_catch_all() {
        // Specific family rows win over the bare catch-all…
        assert_eq!(lookup("claude-opus-4-7").effort_ceiling, E::XHigh);
        assert_eq!(
            lookup("claude-3-5-sonnet-20241022").thinking,
            T::AnthropicBudget
        );
        // …the date-suffix trick still lands Opus 4 on its own row (vision)…
        assert!(lookup("claude-opus-4-20250514").vision);
        // …and the catch-all covers the rest with NO vision marker and no
        // static window (limits resolve live).
        let catch_all = lookup("claude-2.1");
        assert!(!catch_all.vision);
        assert_eq!(catch_all.context_window, None);
    }

    #[test]
    fn ordering_gpt56_before_gpt5() {
        // gpt-5.6 rows sit above the gpt-5 rows (shared prefix, first match
        // wins): 1.5M window, temperature rejected on the bare-name prefix.
        let bare = lookup("gpt-5.6");
        assert!(!bare.supports_temperature);
        assert!(bare.vision);
        assert_eq!(bare.context_window, Some(1_500_000));
        // A gateway id hits the Substring row: temperature kept, same window.
        let gateway = lookup("openai/some-gpt-5.6-variant");
        assert!(gateway.supports_temperature);
        assert_eq!(gateway.context_window, Some(1_500_000));
        // Plain gpt-5 still lands on the 400k rows.
        assert_eq!(lookup("gpt-5-mini").context_window, Some(400_000));
    }

    #[test]
    fn prefix_matches_bare_id_after_provider_strip() {
        assert_eq!(
            lookup("anthropic/claude-opus-4-7").thinking,
            T::AnthropicAdaptive
        );
        assert!(!lookup("openai/gpt-5").supports_temperature);
        assert_eq!(lookup("ollama/gpt-oss:20b").thinking, T::OllamaEffortString);
        // A provider prefix does NOT satisfy a Prefix rule mid-string.
        assert_eq!(lookup("myprov/some-model").thinking, T::ProviderDefault);
    }

    #[test]
    fn substring_matches_full_id() {
        // The marker can sit anywhere in the full id, including the
        // gateway-nested provider segment.
        assert!(lookup("qwen/qwen2.5-vl-7b-instruct").vision);
        assert!(lookup("openrouter/anthropic/claude-sonnet-4.5").vision);
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(
            lookup("Claude-Opus-4-7-Special").thinking,
            T::AnthropicAdaptive
        );
        assert_eq!(lookup("GPT-OSS:20b").thinking, T::OllamaEffortString);
        // Deliberate widening vs the old case-sensitive openai gate.
        assert!(!lookup("GPT-5").supports_temperature);
    }

    #[test]
    fn effort_ceiling_ordering_encodes_the_old_trio() {
        assert!(E::XHigh > E::Max && E::Max > E::High && E::High > E::None);
        // xhigh-capable ⊂ max-capable: every XHigh row would also accept max.
        for entry in CATALOG {
            if entry.effort_ceiling == E::XHigh {
                assert!(entry.effort_ceiling >= E::Max);
            }
        }
    }
}
