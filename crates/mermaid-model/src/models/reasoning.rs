//! Reasoning depth abstraction for multi-provider model adapters.
//!
//! All major LLM providers have converged on string-enum reasoning effort
//! controls (OpenAI `reasoning_effort`, Anthropic adaptive `effort`,
//! Gemini 3 `thinking_level`, Groq, Fireworks, vLLM-on-gpt-oss, OpenRouter
//! normalized). Numeric token-budget knobs (legacy Anthropic `budget_tokens`,
//! legacy Gemini `thinking_budget`) are being phased out.
//!
//! Mermaid exposes a single `ReasoningLevel` enum at the user surface; each
//! adapter maps it to provider-native shapes via dedicated functions. The
//! enum shape mirrors OpenAI Codex's `ReasoningEffort` (the closest Rust
//! prior art), with `XHigh` renamed to `Max` for cleaner Mermaid UX. Six
//! variants beats three because Anthropic and OpenAI's frontier models
//! both ship the extreme tier and the `None` / `Minimal` distinction
//! matters for cost-conscious workloads.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// Reasoning depth requested for a model call.
///
/// `Default` is `Medium` — matches OpenAI's `reasoning_effort` default and
/// is the level that produces useful chain-of-thought without burning
/// excessive latency / tokens for routine prompts.
///
/// String form is lowercase (`"none"`, `"minimal"`, `"low"`, `"medium"`,
/// `"high"`, `"max"`) so config files and CLI flags read naturally.
/// `ValueEnum` is derived so clap can parse `--reasoning <level>`
/// directly from the user-facing strings — no custom parser layer.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Default,
    ValueEnum,
)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
pub enum ReasoningLevel {
    /// Reasoning explicitly disabled. On providers that always reason
    /// (Grok 4, deepseek-reasoner direct), maps to "lowest available".
    None,
    /// Minimum reasoning the provider exposes. OpenAI GPT-5 calls this
    /// `minimal`; absent on most other providers (collapses to `None` or
    /// `Low`).
    Minimal,
    /// Light reasoning for simple multi-step prompts.
    Low,
    /// The default. Useful chain-of-thought without excessive cost.
    #[default]
    Medium,
    /// Heavy reasoning for hard prompts.
    High,
    /// Between `High` and `Max` — OpenAI GPT-5.2+ `xhigh`, Anthropic Opus 4.7
    /// `xhigh` (gated per-model). A step up from `High` for providers that
    /// expose a dedicated `xhigh` tier. Providers without it snap down via
    /// `nearest_effort` to `High`.
    XHigh,
    /// Maximum reasoning the model exposes. Anthropic's adaptive `max`,
    /// OpenRouter's `max`. On OpenAI Chat Completions Effort this collapses
    /// to `high` (no `max` tier in that shape); on Gemini 3 collapses to
    /// `high` (no `max` tier there either).
    Max,
}

impl ReasoningLevel {
    /// Parse a lowercase level name. Mirrors `SafetyMode::parse`.
    ///
    /// Exists so callers that only need string -> level do not reach for
    /// `clap::ValueEnum::from_str`: the slash-command parser lives in the pure
    /// core, and the core must not take a CLI-argument dependency to read one
    /// word.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "none" | "off" => Some(Self::None),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "max" | "maximum" => Some(Self::Max),
            _ => None,
        }
    }

    /// Ordering rank, used by `nearest_effort()`. `None < Minimal < Low <
    /// Medium < High < Max`.
    fn rank(self) -> u8 {
        match self {
            ReasoningLevel::None => 0,
            ReasoningLevel::Minimal => 1,
            ReasoningLevel::Low => 2,
            ReasoningLevel::Medium => 3,
            ReasoningLevel::High => 4,
            // XHigh sits between High and Max (one provider-specific tier
            // above High, below the provider's nominal "max" where one
            // exists). OpenRouter-style "max" is strictly higher.
            ReasoningLevel::XHigh => 5,
            ReasoningLevel::Max => 6,
        }
    }

    /// Lowercase string form (matches the serde representation). Provided
    /// as a method so call sites that want a `&'static str` don't have to
    /// round-trip through serde.
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningLevel::None => "none",
            ReasoningLevel::Minimal => "minimal",
            ReasoningLevel::Low => "low",
            ReasoningLevel::Medium => "medium",
            ReasoningLevel::High => "high",
            ReasoningLevel::Max => "max",
            ReasoningLevel::XHigh => "xhigh",
        }
    }
}

/// Describes the reasoning controls a model exposes.
///
/// Adapters declare this via `ModelCapabilities::supports_reasoning`.
/// The user-facing `ReasoningLevel` is mapped onto whatever shape the
/// model actually accepts via `nearest_effort()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningCapability {
    /// Model has no reasoning controls. Any `ReasoningLevel` is silently
    /// ignored by the adapter.
    Unsupported,
    /// Model has on/off reasoning only (most Ollama-hosted models like
    /// deepseek-r1, qwen3, kimi-k2-thinking — `think: true/false`).
    /// `ReasoningLevel::None` → off; anything else → on.
    Binary,
    /// Model exposes a discrete enum of supported levels (gpt-oss,
    /// OpenAI o-series + GPT-5, Anthropic adaptive thinking, Groq,
    /// Fireworks, vLLM-on-gpt-oss). `nearest_effort()` maps the
    /// requested level onto this set.
    Levels(Vec<ReasoningLevel>),
    /// Model expects a numeric token budget (legacy Anthropic
    /// `budget_tokens`, legacy Gemini 2.5 `thinking_budget`). Adapters
    /// translate `ReasoningLevel` onto a value in `[min, max]`.
    Budget { min: usize, max: usize },
}

/// A typed chunk of reasoning content emitted during streaming.
///
/// Replaces the in-band `"Thinking..."` / `"...done thinking."` text
/// markers the legacy text callback used. `signature` is reserved for
/// providers that emit verifiable thinking traces (Anthropic's
/// `signature` field, OpenAI's `encrypted_content`); `None` for now.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReasoningChunk {
    pub text: String,
    pub signature: Option<String>,
}

/// Map a requested reasoning level onto the closest level the model
/// actually supports.
///
/// Algorithm (lifted from OpenAI Codex's `nearest_effort`):
///   1. If the model has no supported levels, return `None` — caller
///      decides whether to error or silently disable reasoning.
///   2. Prefer the highest supported level whose rank is ≤ requested.
///      This is the "graceful downgrade" semantic Claude Code uses
///      ("falls back to the highest supported level at or below the one
///      you set").
///   3. If every supported level is above the requested rank, fall back
///      to the lowest supported level. Better to honor the user's intent
///      to enable reasoning than to silently disable it.
pub fn nearest_effort(
    requested: ReasoningLevel,
    supported: &[ReasoningLevel],
) -> Option<ReasoningLevel> {
    if supported.is_empty() {
        return None;
    }

    let target = requested.rank();
    let at_or_below = supported.iter().filter(|l| l.rank() <= target).max();
    if let Some(level) = at_or_below {
        return Some(*level);
    }

    // No level at or below the request — return the lowest supported.
    supported.iter().min().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_level_default_is_medium() {
        assert_eq!(ReasoningLevel::default(), ReasoningLevel::Medium);
    }

    #[test]
    fn reasoning_level_serde_roundtrip() {
        for level in [
            ReasoningLevel::None,
            ReasoningLevel::Minimal,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::Max,
            ReasoningLevel::XHigh,
        ] {
            let json = serde_json::to_string(&level).unwrap();
            let back: ReasoningLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(level, back);
            assert_eq!(json.trim_matches('"'), level.as_str());
        }
    }

    #[test]
    fn reasoning_level_ord_matches_rank() {
        assert!(ReasoningLevel::None < ReasoningLevel::Minimal);
        assert!(ReasoningLevel::Minimal < ReasoningLevel::Low);
        assert!(ReasoningLevel::Low < ReasoningLevel::Medium);
        assert!(ReasoningLevel::Medium < ReasoningLevel::High);
        // XHigh is strictly between High and Max.
        assert!(ReasoningLevel::High < ReasoningLevel::XHigh);
        assert!(ReasoningLevel::XHigh < ReasoningLevel::Max);
    }

    #[test]
    fn nearest_effort_empty_returns_none() {
        assert_eq!(nearest_effort(ReasoningLevel::Medium, &[]), None);
    }

    #[test]
    fn nearest_effort_exact_match() {
        let supported = vec![
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
        ];
        assert_eq!(
            nearest_effort(ReasoningLevel::Medium, &supported),
            Some(ReasoningLevel::Medium),
        );
    }

    #[test]
    fn nearest_effort_downgrades_to_highest_at_or_below() {
        // Requested High, supported only Low + Medium → Medium.
        let supported = vec![ReasoningLevel::Low, ReasoningLevel::Medium];
        assert_eq!(
            nearest_effort(ReasoningLevel::High, &supported),
            Some(ReasoningLevel::Medium),
        );
    }

    #[test]
    fn nearest_effort_upgrades_when_all_above_request() {
        // Requested None, supported only Medium + High → Medium (lowest).
        // Better to honor "enable reasoning" intent than silently disable.
        let supported = vec![ReasoningLevel::Medium, ReasoningLevel::High];
        assert_eq!(
            nearest_effort(ReasoningLevel::None, &supported),
            Some(ReasoningLevel::Medium),
        );
    }

    #[test]
    fn nearest_effort_max_request_with_lower_ceiling() {
        // Requested Max, supported up through High → High.
        let supported = vec![
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
        ];
        assert_eq!(
            nearest_effort(ReasoningLevel::Max, &supported),
            Some(ReasoningLevel::High),
        );
    }

    #[test]
    fn reasoning_chunk_construction() {
        let chunk = ReasoningChunk {
            text: "thinking through the problem".to_string(),
            signature: None,
        };
        assert_eq!(chunk.text, "thinking through the problem");
        assert!(chunk.signature.is_none());
    }

    /// `ValueEnum` derive lets clap parse `--reasoning <level>` directly.
    /// Verifies all 7 lowercase strings round-trip through the parser
    /// — protects against accidental rename in `serde rename_all`
    /// drifting from `value rename_all`.
    #[test]
    fn clap_value_enum_parses_all_seven_levels() {
        use clap::ValueEnum as _;
        for (s, expected) in [
            ("none", ReasoningLevel::None),
            ("minimal", ReasoningLevel::Minimal),
            ("low", ReasoningLevel::Low),
            ("medium", ReasoningLevel::Medium),
            ("high", ReasoningLevel::High),
            ("max", ReasoningLevel::Max),
            ("xhigh", ReasoningLevel::XHigh),
        ] {
            let parsed = ReasoningLevel::from_str(s, true).expect(s);
            assert_eq!(parsed, expected);
        }
    }

    #[test]
    fn clap_value_enum_rejects_unknown_levels() {
        use clap::ValueEnum as _;
        assert!(ReasoningLevel::from_str("foobar", true).is_err());
        assert!(ReasoningLevel::from_str("medium ", true).is_err());
    }
}
