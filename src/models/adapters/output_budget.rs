//! Model-scaled output-token budgeting, shared across adapters.
//!
//! `max_tokens == 0` means **AUTO**: give the model the room the context window
//! leaves, or omit the cap entirely so the provider applies its own per-response
//! maximum — never a guessed constant. A positive value is the user's explicit
//! **hard cap**. This generalizes the sizing Ollama has always done (see
//! [`super::ollama_sizing`]) to every provider.

use crate::models::reasoning::ReasoningLevel;

/// Output tokens reserved for reasoning/thinking so a thinking model doesn't
/// burn its whole answer budget before responding. Scales with the effort
/// level. Used by compaction to reserve response room in the window; the AUTO
/// budget itself needs no reserve (it already hands over the full window room).
pub fn reasoning_output_reserve(level: ReasoningLevel) -> usize {
    match level {
        ReasoningLevel::None | ReasoningLevel::Minimal => 0,
        ReasoningLevel::Low => 1_024,
        ReasoningLevel::Medium => 4_096,
        ReasoningLevel::High | ReasoningLevel::XHigh | ReasoningLevel::Max => 8_192,
    }
}

/// How a provider accepts its per-request output cap. Providers whose cap
/// field is *omittable* (OpenAI-compatible, Gemini) don't come through here:
/// their adapters implement AUTO by omitting the field when `max_tokens == 0`
/// (`openai_compat.rs`/`gemini.rs` request builders), which yields the
/// provider's own per-response maximum with no computed number at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputCapMode {
    /// The field is mandatory (Anthropic). AUTO → a computed positive value.
    Required,
    /// Ollama's `num_predict`, sent when the window is known so generation stops
    /// before the window fills. AUTO → the full remaining window room.
    NumPredict,
}

/// Inputs for [`resolve_output_budget`].
#[derive(Debug, Clone, Copy)]
pub struct OutputBudgetInputs {
    /// The requested cap — `ChatRequest.max_tokens`. `0` = AUTO.
    pub requested_cap: usize,
    /// Effective context window in tokens, if known.
    pub window: Option<usize>,
    /// Estimated prompt tokens already occupying the window.
    pub prompt_estimate: usize,
    /// The model's real per-response output ceiling, if known (live-discovered
    /// or learned from a 400). Bounds every resolved value — sending above it
    /// is a guaranteed rejection on providers that enforce one.
    pub provider_max_output: Option<usize>,
    /// Tokens held back from the window for prompt-estimate slop / safety.
    pub margin: usize,
    /// Minimum output when a positive value must be sent.
    pub floor: usize,
}

/// Resolve the per-request output cap. `Some(n)` = send `n`; `None` = omit the
/// field entirely (let the provider apply its own per-response maximum).
pub fn resolve_output_budget(inputs: &OutputBudgetInputs, mode: OutputCapMode) -> Option<usize> {
    // Room the window leaves after the prompt. `Some(0)` when the window is
    // already full; `None` when the window is unknown — kept distinct so a full
    // window clamps output while an unknown window does not.
    let room = inputs.window.map(|w| {
        w.saturating_sub(inputs.prompt_estimate)
            .saturating_sub(inputs.margin)
    });

    // Explicit hard cap: honor it, bounded by the room that actually fits and
    // by the model's known output ceiling (above it is a guaranteed 400).
    if inputs.requested_cap > 0 {
        let mut capped = room.map_or(inputs.requested_cap, |r| inputs.requested_cap.min(r));
        if let Some(ceiling) = inputs.provider_max_output {
            capped = capped.min(ceiling);
        }
        return Some(match mode {
            OutputCapMode::NumPredict => capped.max(inputs.floor),
            OutputCapMode::Required => capped.max(1),
        });
    }

    // AUTO.
    match mode {
        // Ollama must be told a number when the window is known (else it runs
        // to the window edge and truncates), bounded by any known ceiling
        // (Ollama Cloud enforces one — the minimax incident). A known ceiling
        // with an unknown window is still sent: it's the one number that
        // prevents the 400. Both unknown → omit (Ollama's own default).
        OutputCapMode::NumPredict => match (room, inputs.provider_max_output) {
            (Some(r), Some(ceiling)) => Some(r.min(ceiling).max(inputs.floor)),
            (Some(r), None) => Some(r.max(inputs.floor)),
            (None, Some(ceiling)) => Some(ceiling.max(inputs.floor)),
            (None, None) => None,
        },
        // A required field: the model's real ceiling, clamped to what fits.
        // Downstream (`anthropic::legacy_budget_for`) handles the
        // thinking-budget fit against this value.
        OutputCapMode::Required => {
            let ceiling = inputs
                .provider_max_output
                .unwrap_or(inputs.floor.max(8_192));
            Some(room.map_or(ceiling, |r| ceiling.min(r)).max(1))
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(requested_cap: usize, window: Option<usize>, prompt: usize) -> OutputBudgetInputs {
        OutputBudgetInputs {
            requested_cap,
            window,
            prompt_estimate: prompt,
            provider_max_output: None,
            margin: 256,
            floor: 512,
        }
    }

    #[test]
    fn auto_num_predict_uses_full_room_or_omits() {
        // Known window → all the room the window leaves (this is the GLM/Ollama fix).
        assert_eq!(
            resolve_output_budget(&inputs(0, Some(131_072), 1_000), OutputCapMode::NumPredict),
            Some(131_072 - 1_000 - 256)
        );
        // Unknown window → omit (Ollama applies its own default).
        assert_eq!(
            resolve_output_budget(&inputs(0, None, 1_000), OutputCapMode::NumPredict),
            None
        );
        // Full window → the floor, never zero.
        assert_eq!(
            resolve_output_budget(&inputs(0, Some(1_100), 1_000), OutputCapMode::NumPredict),
            Some(512)
        );
    }

    #[test]
    fn auto_required_uses_documented_ceiling_clamped_to_room() {
        let mut i = inputs(0, Some(200_000), 1_000);
        i.provider_max_output = Some(64_000);
        // Ceiling fits well within the window → send the ceiling.
        assert_eq!(
            resolve_output_budget(&i, OutputCapMode::Required),
            Some(64_000)
        );
        // Tight window clamps below the ceiling.
        i.prompt_estimate = 190_000;
        assert_eq!(
            resolve_output_budget(&i, OutputCapMode::Required),
            Some(200_000 - 190_000 - 256)
        );
    }

    #[test]
    fn hard_cap_is_honored_and_room_bounded_without_reserve() {
        // A user-set positive value is exact — no reasoning reserve added.
        assert_eq!(
            resolve_output_budget(
                &inputs(4_096, Some(131_072), 1_000),
                OutputCapMode::Required
            ),
            Some(4_096)
        );
        // Bounded by the room that fits.
        assert_eq!(
            resolve_output_budget(
                &inputs(4_096, Some(8_192), 7_000),
                OutputCapMode::NumPredict
            ),
            Some(8_192 - 7_000 - 256)
        );
        // Full window still floors NumPredict so something generates.
        assert_eq!(
            resolve_output_budget(
                &inputs(4_096, Some(8_192), 8_100),
                OutputCapMode::NumPredict
            ),
            Some(512)
        );
    }
}
