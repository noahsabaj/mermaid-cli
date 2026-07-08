//! Structured interactive questions the model poses to the user via the
//! `ask_user_question` tool.
//!
//! Pure data, mirroring the approval flow (`PendingApproval` in `state.rs`):
//! the tool sends `Msg::QuestionAsked` with a batch of [`Question`]s; the
//! reducer stores a [`PendingQuestionSet`] and renders a modal; the key
//! handler resolves it into `Cmd::ResolveQuestion` carrying a
//! [`QuestionResolution`], which the `QuestionBroker` delivers back to the
//! parked tool task.
//!
//! The schema-facing types (`Question`/`QuestionOption`) are kind-agnostic for
//! now — Stage 1 ships Select + Multi-select. They're deliberately shaped like
//! `ToolMetadata` (a `#[serde(tag=...)]` union) so later stages can add rank,
//! slider, date, and path input kinds without reshaping the tool schema.

use serde::{Deserialize, Serialize};

use super::ids::{ToolCallId, TurnId};

/// One question in a batch. Stage 1: a labeled choice, single- or multi-select.
/// `camelCase` so the model's `multiSelect` field deserializes directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    /// Short chip label shown above the question (e.g. "Database"). Kept short
    /// (~12 cells) by the render layer.
    pub header: String,
    /// The question text itself.
    pub question: String,
    /// When true the user may select any number of options (checkboxes + an
    /// explicit Submit); when false exactly one option resolves the question.
    #[serde(default)]
    pub multi_select: bool,
    /// The selectable options, in display order. Convention: a recommended
    /// option is listed first and flagged.
    pub options: Vec<QuestionOption>,
}

/// One selectable option: a label plus an optional one-line description.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Rendered with a "(Recommended)" tag. Held as a flag (rather than parsing
    /// the label) so the render layer can style it and the answer can note it.
    #[serde(default)]
    pub recommended: bool,
    /// Optional side-by-side preview: an ASCII mockup, config, code, or a
    /// unified diff. Rendered in a right-hand pane when the option is focused.
    #[serde(default)]
    pub preview: Option<OptionPreview>,
}

/// A per-option preview payload (Stage 2). The model supplies the content; the
/// tool only renders it. `diff` switches on `+`/`-` line coloring for showing
/// the change an option would produce — the standout for a coding agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionPreview {
    /// The preview body, rendered as monospace lines.
    pub content: String,
    /// Language hint (e.g. "rust", "yaml"). Reserved for future syntax
    /// highlighting; today the body renders as plain monospace.
    #[serde(default)]
    pub language: Option<String>,
    /// Render `content` as a unified diff: `+` lines green, `-` lines red,
    /// `@@` hunk headers cyan.
    #[serde(default)]
    pub diff: bool,
}

/// A batch of questions awaiting the user's answers, plus live modal selection
/// state. The reducer owns this; it mirrors `PendingApproval`.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingQuestionSet {
    pub turn: TurnId,
    pub call_id: ToolCallId,
    pub questions: Vec<Question>,
    /// Active tab. `0..questions.len()` selects a question; a value equal to
    /// `questions.len()` is the "Review your answers" screen.
    pub active: usize,
    /// Per-question live selection state, parallel to `questions`.
    pub selections: Vec<QuestionSelection>,
    /// Highlighted row on the review screen: 0 = Submit answers, 1 = Cancel.
    pub review_cursor: usize,
    /// When true, keystrokes edit the active question's note (toggled with `n`).
    pub editing_note: bool,
}

/// Live selection state for one question.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QuestionSelection {
    /// Highlighted row for arrow-key navigation. Row layout:
    /// `0..n` options, `n` = the "Other" free-text row, and for multi-select
    /// `n+1` = the Submit row.
    pub cursor: usize,
    /// Chosen option indices. Single-select holds at most one; multi-select any.
    pub chosen: Vec<usize>,
    /// Free-text typed into the "Other" row — the universal escape hatch. When
    /// non-empty it contributes to the answer alongside (multi) or instead of
    /// (single) the chosen options.
    pub other_text: String,
    /// Optional free-text note attached to this question (press `n` to edit).
    /// Rides back with the answer to capture intent the options didn't cover.
    pub note: String,
}

impl PendingQuestionSet {
    pub fn new(turn: TurnId, call_id: ToolCallId, questions: Vec<Question>) -> Self {
        let selections = questions
            .iter()
            .map(|_| QuestionSelection::default())
            .collect();
        Self {
            turn,
            call_id,
            questions,
            active: 0,
            selections,
            review_cursor: 0,
            editing_note: false,
        }
    }

    /// Skip the review screen only for the atomic case: a single single-select
    /// question, where picking an option is the whole answer (Claude Code
    /// resolves it immediately). Every other shape (multi-question, or any
    /// multi-select) confirms via the Submit/review screen.
    pub fn skips_review(&self) -> bool {
        self.questions.len() == 1 && !self.questions[0].multi_select
    }

    /// Number of navigable rows for the question at `idx`: options, the Other
    /// row, and (multi-select only) the Submit row.
    pub fn row_count(&self, idx: usize) -> usize {
        let q = &self.questions[idx];
        q.options.len() + 1 + usize::from(q.multi_select)
    }

    /// Row index of the "Other" free-text row for the question at `idx`.
    pub fn other_row(&self, idx: usize) -> usize {
        self.questions[idx].options.len()
    }

    /// Row index of the Submit row for a multi-select question, if any.
    pub fn submit_row(&self, idx: usize) -> Option<usize> {
        let q = &self.questions[idx];
        q.multi_select.then_some(q.options.len() + 1)
    }

    /// Build the final answers from current selections. Each answer carries the
    /// selected option labels plus any typed "Other" text; a question left
    /// untouched yields an empty `selected` (surfaced to the model as "(no
    /// selection)").
    pub fn build_answers(&self) -> Vec<QuestionAnswer> {
        self.questions
            .iter()
            .zip(&self.selections)
            .map(|(q, sel)| {
                let mut selected: Vec<String> = sel
                    .chosen
                    .iter()
                    .filter_map(|&i| q.options.get(i).map(|o| o.label.clone()))
                    .collect();
                let other = sel.other_text.trim();
                if !other.is_empty() {
                    selected.push(other.to_string());
                }
                let note = sel.note.trim();
                QuestionAnswer {
                    header: q.header.clone(),
                    question: q.question.clone(),
                    selected,
                    note: (!note.is_empty()).then(|| note.to_string()),
                }
            })
            .collect()
    }
}

/// How a question set resolved. Delivered via `Cmd::ResolveQuestion` and the
/// `QuestionBroker` to the parked tool task.
#[derive(Debug, Clone, PartialEq)]
pub enum QuestionResolution {
    /// The user submitted answers (some questions may be unanswered).
    Answered(Vec<QuestionAnswer>),
    /// The user dismissed the prompt (Esc / Cancel) or the turn was cancelled.
    Dismissed,
}

/// One question's resolved answer, keyed by its header + text so the model can
/// unambiguously match answers to questions when several are batched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionAnswer {
    pub header: String,
    pub question: String,
    /// Selected option labels; includes the typed "Other" text when used.
    /// Empty means the user skipped this question.
    pub selected: Vec<String>,
    /// Optional free-text note the user attached to this question.
    #[serde(default)]
    pub note: Option<String>,
}
