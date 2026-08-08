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

use crate::ids::{ToolCallId, TurnId};

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
    /// How the question is answered — choice kinds (`Select`/`MultiSelect`/
    /// `Rank`) use `options`; input kinds (`Text`/`Number`/`Date`/`Path`) use a
    /// single typed value.
    #[serde(default)]
    pub kind: QuestionKind,
    /// The selectable options (choice kinds only), in display order. A
    /// recommended option is listed first and flagged. Empty for input kinds.
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    /// Stable key for remembering the user's answer across sessions. When set
    /// and the user opts to remember, a later question with the same key
    /// auto-answers without prompting.
    #[serde(default)]
    pub memory_key: Option<String>,
}

impl Question {
    /// Choice kinds present a list of options.
    #[must_use]
    pub fn is_choice(&self) -> bool {
        matches!(
            self.kind,
            QuestionKind::Select | QuestionKind::MultiSelect | QuestionKind::Rank
        )
    }
    #[must_use]
    pub fn is_multi(&self) -> bool {
        matches!(self.kind, QuestionKind::MultiSelect)
    }
    #[must_use]
    pub fn is_rank(&self) -> bool {
        matches!(self.kind, QuestionKind::Rank)
    }
    /// Input kinds present a single typed value field.
    #[must_use]
    pub fn is_input(&self) -> bool {
        !self.is_choice()
    }
}

/// How a question is answered. Choice kinds carry `options`; input kinds carry
/// their own validation parameters. Shaped like `ToolMetadata` (a tagged union)
/// so new kinds slot in without reshaping the schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum QuestionKind {
    /// Pick exactly one option.
    #[default]
    Select,
    /// Pick any number of options (checkboxes + explicit Submit).
    MultiSelect,
    /// Reorder the options into a ranked list.
    Rank,
    /// Free-text with an optional validator.
    Text {
        #[serde(default)]
        validate: TextValidate,
    },
    /// A number with optional bounds and step; `slider` adds a bar.
    Number {
        #[serde(default)]
        min: Option<f64>,
        #[serde(default)]
        max: Option<f64>,
        #[serde(default)]
        step: Option<f64>,
        #[serde(default)]
        slider: bool,
    },
    /// An ISO date, `YYYY-MM-DD`.
    Date,
    /// A filesystem path; `must_exist` is advisory (the pure reducer performs no
    /// filesystem I/O, so existence isn't enforced live).
    Path {
        #[serde(default)]
        must_exist: bool,
    },
}

/// Validator for a `Text` question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "rule", content = "pattern", rename_all = "camelCase")]
pub enum TextValidate {
    /// Any non-empty text.
    #[default]
    Any,
    /// Must parse as a number.
    Number,
    /// Must match this regular expression.
    Regex(String),
}

/// Validate a typed input value against its question kind. `Ok(())` means valid
/// (an empty value is treated as "skipped"). Pure — performs no filesystem I/O,
/// so `Path { must_exist }` is not enforced here.
///
/// # Errors
///
/// Returns the message to show under the field: out of a `Number` question's
/// `min`/`max` range, unparseable as a number for `Number` or
/// [`TextValidate::Number`], not matching a [`TextValidate::Regex`] pattern,
/// or not `YYYY-MM-DD` for [`QuestionKind::Date`]. A regex that itself fails
/// to compile is `Ok` — an author's broken pattern must not lock the user out
/// of answering.
pub fn validate_input(kind: &QuestionKind, value: &str) -> Result<(), String> {
    let v = value.trim();
    if v.is_empty() {
        return Ok(());
    }
    match kind {
        QuestionKind::Number { min, max, .. } => {
            let n: f64 = v.parse().map_err(|_| "must be a number".to_string())?;
            if let Some(lo) = min
                && n < *lo
            {
                return Err(format!("must be >= {lo}"));
            }
            if let Some(hi) = max
                && n > *hi
            {
                return Err(format!("must be <= {hi}"));
            }
            Ok(())
        },
        QuestionKind::Text { validate } => match validate {
            TextValidate::Any => Ok(()),
            TextValidate::Number => v
                .parse::<f64>()
                .map(|_| ())
                .map_err(|_| "must be a number".to_string()),
            TextValidate::Regex(pat) => match regex::Regex::new(pat) {
                Ok(re) if re.is_match(v) => Ok(()),
                Ok(_) => Err(format!("must match /{pat}/")),
                Err(_) => Ok(()),
            },
        },
        QuestionKind::Date => chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d")
            .map(|_| ())
            .map_err(|_| "must be a date (YYYY-MM-DD)".to_string()),
        _ => Ok(()),
    }
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
    /// When true, the user opted to remember these answers across sessions
    /// (toggled with `r`); the tool persists answers keyed by `memory_key`.
    pub remember: bool,
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
    /// Typed value for input kinds (Text/Number/Date/Path).
    pub value: String,
    /// Current option ordering for a Rank question (indices into `options`);
    /// empty means the default `0..n` order.
    pub order: Vec<usize>,
    /// Rank: whether the item under the cursor is "picked up" for moving.
    pub grabbed: bool,
}

impl PendingQuestionSet {
    #[must_use]
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
            remember: false,
        }
    }

    /// Skip the review screen only for the atomic case: a single single-select
    /// question, where picking an option is the whole answer (Claude Code
    /// resolves it immediately). Every other shape (multi-question, or any
    /// multi-select) confirms via the Submit/review screen.
    #[must_use]
    pub fn skips_review(&self) -> bool {
        self.questions.len() == 1 && !self.questions[0].is_multi()
    }

    /// Number of navigable rows for a Select/MultiSelect question at `idx`:
    /// options, the Other row, and (multi-select only) the Submit row.
    #[must_use]
    pub fn row_count(&self, idx: usize) -> usize {
        let q = &self.questions[idx];
        q.options.len() + 1 + usize::from(q.is_multi())
    }

    /// Row index of the "Other" free-text row for the question at `idx`.
    #[must_use]
    pub fn other_row(&self, idx: usize) -> usize {
        self.questions[idx].options.len()
    }

    /// Row index of the Submit row for a multi-select question, if any.
    #[must_use]
    pub fn submit_row(&self, idx: usize) -> Option<usize> {
        let q = &self.questions[idx];
        q.is_multi().then_some(q.options.len() + 1)
    }

    /// Build the final answers from current selections. Each answer carries the
    /// selected option labels plus any typed "Other" text; a question left
    /// untouched yields an empty `selected` (surfaced to the model as "(no
    /// selection)").
    #[must_use]
    pub fn build_answers(&self) -> Vec<QuestionAnswer> {
        self.questions
            .iter()
            .zip(&self.selections)
            .map(|(q, sel)| {
                let selected = if q.is_input() {
                    let v = sel.value.trim();
                    if v.is_empty() {
                        Vec::new()
                    } else {
                        vec![v.to_string()]
                    }
                } else if q.is_rank() {
                    rank_order(q, sel)
                        .iter()
                        .filter_map(|&i| q.options.get(i).map(|o| o.label.clone()))
                        .collect()
                } else {
                    let mut s: Vec<String> = sel
                        .chosen
                        .iter()
                        .filter_map(|&i| q.options.get(i).map(|o| o.label.clone()))
                        .collect();
                    let other = sel.other_text.trim();
                    if !other.is_empty() {
                        s.push(other.to_string());
                    }
                    s
                };
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

/// The current ranked ordering for a Rank question: the selection's `order`,
/// or the default `0..n` when it hasn't been reordered yet.
#[must_use]
pub fn rank_order(q: &Question, sel: &QuestionSelection) -> Vec<usize> {
    if sel.order.is_empty() {
        (0..q.options.len()).collect()
    } else {
        sel.order.clone()
    }
}

/// How a question set resolved. Delivered via `Cmd::ResolveQuestion` and the
/// `QuestionBroker` to the parked tool task.
#[derive(Debug, Clone, PartialEq)]
pub enum QuestionResolution {
    /// The user submitted answers (some questions may be unanswered).
    Answered {
        answers: Vec<QuestionAnswer>,
        /// The user asked to remember these answers across sessions (`r`).
        remember: bool,
    },
    /// The user dismissed the prompt (Esc / Cancel) or the turn was cancelled.
    Dismissed,
    /// The user chose "Chat about this" — bounce the set back to the model to
    /// reformulate, rather than answering.
    Reformulate,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn num(min: f64, max: f64) -> QuestionKind {
        QuestionKind::Number {
            min: Some(min),
            max: Some(max),
            step: None,
            slider: false,
        }
    }

    #[test]
    fn validate_number_bounds() {
        assert!(validate_input(&num(0.0, 5.0), "3").is_ok());
        assert!(validate_input(&num(0.0, 5.0), "9").is_err());
        assert!(validate_input(&num(0.0, 5.0), "x").is_err());
        // Empty is treated as "skipped", always valid.
        assert!(validate_input(&num(0.0, 5.0), "").is_ok());
    }

    #[test]
    fn validate_date_and_regex() {
        assert!(validate_input(&QuestionKind::Date, "2026-07-07").is_ok());
        assert!(validate_input(&QuestionKind::Date, "nope").is_err());
        let re = QuestionKind::Text {
            validate: TextValidate::Regex("^a+$".to_string()),
        };
        assert!(validate_input(&re, "aaa").is_ok());
        assert!(validate_input(&re, "abc").is_err());
    }
}
