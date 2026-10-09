//! The model-capability catalog: HINTS, never requirements.
//!
//! Providers don't expose thinking shapes, temperature support or effort
//! ceilings over an API, so adapters learn them the other way: send the
//! request optimistically and, when the provider rejects a parameter, take it
//! back and remember that (`adapters::learning`). This table only lets a
//! known model skip that one wasted round trip. A model with no row must
//! work exactly as well, just one rejection later — so every column's
//! "no row" value means "no opinion", and each adapter's no-opinion default
//! is the optimistic one (send the parameter, pick the newest shape).
//!
//! What the catalog can still do that learning can't: pick a wire shape
//! where the provider's failure mode is silence. Ollama's gpt-oss, for one,
//! accepts `think: true` without complaint and ignores it; only the hint
//! makes it `think: "high"`. A rejection is a signal; an ignored parameter
//! isn't, so those rows stay.
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

/// Which thinking/reasoning wire shape to TRY FIRST for the model's requests.
/// Consumed by the anthropic, gemini, and ollama adapters — each reacts only
/// to its own variants and treats everything else as `ProviderDefault`. A
/// shape the provider rejects is stepped past (`adapters::learning`), so a
/// wrong hint costs a round trip, not a broken request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingShape {
    /// No catalog opinion: the adapter tries its newest shape first
    /// (anthropic → adaptive; gemini → `thinkingLevel`; ollama → `think:
    /// bool` gated by the live probe; openai-compat → the `ProviderProfile`
    /// strategy).
    ProviderDefault,
    /// The model takes no thinking controls at all (Gemini 2.0 and older,
    /// Claude 2): send none rather than learn that from two rejections.
    Unsupported,
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
/// trio: `> NoEffort` ⇒ effort accepted, `>= Max` ⇒ "max" accepted,
/// `>= XHigh` ⇒ "xhigh" accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EffortCeiling {
    /// `output_config.effort` is rejected outright (4.5-family Sonnet/Haiku
    /// and older) — the request must carry no effort field.
    NoEffort,
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
    /// `false` = known to reject a top-level `temperature`, so don't send
    /// one; `true` = no known objection. Consumed by the anthropic and
    /// openai-compat request builders (the 4.6+ adaptive line and the
    /// o-series/gpt-5 reasoning models 400 on it).
    pub supports_temperature: bool,
    /// Highest accepted effort tier — consumed by the anthropic adapter only.
    /// `None` = no opinion: send the tier the user asked for.
    pub effort_ceiling: Option<EffortCeiling>,
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
        effort_ceiling: Some(effort_ceiling),
        vision: true,
        context_window: None,
    }
}

/// The default row for models no rule matches: no opinion on anything, so
/// every adapter takes its optimistic path and learns from rejections. No
/// vision is advertised (it never gates the send) and no static window
/// (limits resolve live).
pub const UNKNOWN_MODEL: ModelCapEntry = ModelCapEntry {
    rule: MatchRule::Substring(""),
    thinking: ThinkingShape::ProviderDefault,
    supports_temperature: true,
    effort_ceiling: None,
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
        E::NoEffort,
    ),
    claude(
        Prefix("claude-haiku-4-5"),
        T::AnthropicBudget,
        true,
        E::NoEffort,
    ),
    // Opus 4.1, and (via the "-2" prefix) date-suffixed Opus 4 ids like
    // claude-opus-4-20250514 — both document a 32k output ceiling.
    claude(
        Prefix("claude-opus-4-1"),
        T::AnthropicBudget,
        true,
        E::NoEffort,
    ),
    claude(
        Prefix("claude-opus-4-2"),
        T::AnthropicBudget,
        true,
        E::NoEffort,
    ),
    // Date-suffixed Sonnet 4 ids (claude-sonnet-4-20250514), same trick.
    claude(
        Prefix("claude-sonnet-4-2"),
        T::AnthropicBudget,
        true,
        E::NoEffort,
    ),
    // Claude 3.x family (3.5 has an 8k output ceiling; limits resolve live).
    claude(Prefix("claude-3"), T::AnthropicBudget, true, E::NoEffort),
    // Claude 2 / Instant predate thinking and effort entirely.
    ModelCapEntry {
        rule: Prefix("claude-2"),
        thinking: T::Unsupported,
        effort_ceiling: Some(E::NoEffort),
        ..UNKNOWN_MODEL
    },
    ModelCapEntry {
        rule: Prefix("claude-instant"),
        thinking: T::Unsupported,
        effort_ceiling: Some(E::NoEffort),
        ..UNKNOWN_MODEL
    },
    // --- Claude via gateways (full-id substrings): vision markers only. An
    // unlisted claude id reached directly (a model newer than this table)
    // must NOT fall into a legacy row here — it gets no opinion and the
    // adapter's optimistic newest shapes, learning down from a rejection.
    vision_marker("claude-opus"),
    vision_marker("claude-sonnet"),
    vision_marker("claude-haiku"),
    vision_marker("claude-fable"),
    vision_marker("claude-mythos"),
    vision_marker("claude-3"),
    vision_marker("claude-4"),
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
    // GPT-6 (gpt-6-astra, gpt-6.1-sol, ...): the documented window is 1.05M
    // (OpenAI's model comparison, read 2026-10-09). Without a row the window
    // is unknown, since OpenAI's /v1/models exposes no limits, and automatic
    // compaction (Mermaid's or OpenAI's) never triggers.
    ModelCapEntry {
        rule: Prefix("gpt-6"),
        supports_temperature: false,
        vision: true,
        context_window: Some(1_050_000),
        ..UNKNOWN_MODEL
    },
    ModelCapEntry {
        rule: Substring("gpt-6"),
        vision: true,
        context_window: Some(1_050_000),
        ..UNKNOWN_MODEL
    },
    // gpt-5.6 rows must stay ABOVE the gpt-5 rows — they share the prefix
    // and first match wins. OpenAI's /v1/models exposes no limits, so these
    // windows are static-but-documented (1.05M, OpenAI's model comparison).
    ModelCapEntry {
        rule: Prefix("gpt-5.6"),
        supports_temperature: false,
        vision: true,
        context_window: Some(1_050_000),
        ..UNKNOWN_MODEL
    },
    ModelCapEntry {
        rule: Substring("gpt-5.6"),
        vision: true,
        context_window: Some(1_050_000),
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
    // Gemini 2.0 and older 400 on any thinkingConfig; saying so up front
    // saves them two rejections.
    ModelCapEntry {
        rule: Prefix("gemini-2.0"),
        thinking: T::Unsupported,
        vision: true,
        ..UNKNOWN_MODEL
    },
    ModelCapEntry {
        rule: Prefix("gemini-1"),
        thinking: T::Unsupported,
        vision: true,
        ..UNKNOWN_MODEL
    },
    // Any other gemini id: no thinking opinion (a newer model gets the
    // newest shape), but the whole family is vision-capable.
    vision_marker("gemini"),
    // --- Ollama gpt-oss (bare-name prefix; matches tags like gpt-oss:20b).
    // The one hint learning can't replace: gpt-oss accepts `think: true` and
    // silently ignores it, so no rejection ever says to use the string. ---
    ModelCapEntry {
        rule: Prefix("gpt-oss"),
        thinking: T::OllamaEffortString,
        ..UNKNOWN_MODEL
    },
    // --- xAI Grok (prefix rows must stay above the generic grok marker) ---
    ModelCapEntry {
        rule: Prefix("grok-4.6"),
        vision: true,
        context_window: Some(500_000),
        ..UNKNOWN_MODEL
    },
    ModelCapEntry {
        rule: Substring("grok-4.6"),
        vision: true,
        context_window: Some(500_000),
        ..UNKNOWN_MODEL
    },
    vision_marker("grok"),
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
    effort_ceiling: None,
    vision: false,
    context_window: None,
};

/// Look up the capability row for a model id (bare or `provider/`-prefixed,
/// case-insensitive). Unmatched ids get [`UNKNOWN_MODEL`].
#[must_use]
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
    fn unknown_model_gets_no_opinions() {
        // Every column says "no opinion", so each adapter takes its
        // optimistic path: send what the user asked for, learn from a 400.
        let entry = lookup("totally-unknown-model");
        assert_eq!(entry.thinking, T::ProviderDefault);
        assert!(entry.supports_temperature);
        assert_eq!(entry.effort_ceiling, None);
        assert!(!entry.vision);
        assert_eq!(entry.context_window, None);
    }

    #[test]
    fn a_claude_newer_than_the_table_is_not_pinned_to_legacy_rows() {
        // The rows this replaced sent any unlisted claude id down the legacy
        // path (budget thinking, no effort): a model released after the
        // table was written got the worst behavior. Now it gets none.
        for id in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-opus-4-9",
            "claude-next",
        ] {
            let entry = lookup(id);
            assert_eq!(entry.thinking, T::ProviderDefault, "{id}");
            assert_eq!(entry.effort_ceiling, None, "{id}");
            assert!(entry.supports_temperature, "{id}");
        }
        // Gateway ids keep their vision marker, newer families included.
        assert!(lookup("openrouter/anthropic/claude-opus-4.9").vision);
        assert!(lookup("openrouter/anthropic/claude-opus-5.5").vision);
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
    fn ordering_specific_claude_rows() {
        assert_eq!(lookup("claude-opus-4-7").effort_ceiling, Some(E::XHigh));
        assert_eq!(
            lookup("claude-3-5-sonnet-20241022").thinking,
            T::AnthropicBudget
        );
        // The date-suffix trick lands Opus 4 and Sonnet 4 on their own rows.
        let opus4 = lookup("claude-opus-4-20250514");
        assert!(opus4.vision);
        assert_eq!(opus4.effort_ceiling, Some(E::NoEffort));
        assert_eq!(
            lookup("claude-sonnet-4-20250514").thinking,
            T::AnthropicBudget
        );
        // Claude 2 takes no thinking controls, no vision, no static window.
        let claude2 = lookup("claude-2.1");
        assert_eq!(claude2.thinking, T::Unsupported);
        assert!(!claude2.vision);
        assert_eq!(claude2.context_window, None);
    }

    #[test]
    fn old_gemini_takes_no_thinking_config() {
        assert_eq!(lookup("gemini-2.0-flash").thinking, T::Unsupported);
        assert_eq!(lookup("gemini-1.5-pro").thinking, T::Unsupported);
        // A gemini newer than the table has no opinion (newest shape first).
        assert_eq!(lookup("gemini-4-pro").thinking, T::ProviderDefault);
        assert!(lookup("gemini-4-pro").vision);
    }

    #[test]
    fn ordering_gpt56_before_gpt5() {
        // gpt-5.6 rows sit above the gpt-5 rows (shared prefix, first match
        // wins): 1.05M window, temperature rejected on the bare-name prefix.
        let bare = lookup("gpt-5.6");
        assert!(!bare.supports_temperature);
        assert!(bare.vision);
        assert_eq!(bare.context_window, Some(1_050_000));
        // A gateway id hits the Substring row: temperature kept, same window.
        let gateway = lookup("openai/some-gpt-5.6-variant");
        assert!(gateway.supports_temperature);
        assert_eq!(gateway.context_window, Some(1_050_000));
        // Plain gpt-5 still lands on the 400k rows.
        assert_eq!(lookup("gpt-5-mini").context_window, Some(400_000));
        for gpt6 in ["gpt-6-astra", "openai/gpt-6.1-sol", "GPT-6-LUNA"] {
            let row = lookup(gpt6);
            assert_eq!(row.context_window, Some(1_050_000), "{gpt6}");
            assert!(row.vision, "{gpt6}");
            assert!(!row.supports_temperature, "{gpt6}");
        }
    }

    #[test]
    fn ordering_grok_4_6_before_generic_grok() {
        // grok-4.6 rows sit above the generic grok marker (shared substring, first match
        // wins): 500k window, vision true. The Prefix row covers bare names; the
        // Substring row covers gateway-nested ids.
        let bare = lookup("grok-4.6");
        assert!(bare.vision);
        assert_eq!(bare.context_window, Some(500_000));
        let bare_suffixed = lookup("grok-4.6-mini");
        assert_eq!(bare_suffixed.context_window, Some(500_000));
        // A gateway id hits the Substring row: same window, vision true.
        let gateway = lookup("openai/some-grok-4.6-variant");
        assert!(gateway.vision);
        assert_eq!(gateway.context_window, Some(500_000));
        // Generic grok still gets vision but no static window (live discovery).
        let generic = lookup("grok-3");
        assert!(generic.vision);
        assert_eq!(generic.context_window, None);
        assert!(lookup("xai/grok-4").vision);
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
        assert!(E::XHigh > E::Max && E::Max > E::High && E::High > E::NoEffort);
        // xhigh-capable ⊂ max-capable: every XHigh row would also accept max.
        for entry in CATALOG {
            if let Some(ceiling) = entry.effort_ceiling
                && ceiling == E::XHigh
            {
                assert!(ceiling >= E::Max);
            }
        }
    }
}
