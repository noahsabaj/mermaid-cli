//! The `ask_user_question` tool — the model's structured path to ask the user
//! a decision it genuinely cannot make on its own.
//!
//! The model supplies 1–4 questions, each with a short header chip and a set of
//! labeled options (single- or multi-select). The tool parks on the
//! `QuestionBroker` (interactive runs) while the TUI renders a selectable modal,
//! then formats the user's answers back into the tool result. Headless runs have
//! no human to ask, so the tool returns a proceed-with-best-judgment result
//! rather than blocking. Asking mutates nothing, so the tool is ungated (it
//! never touches the policy gate) and runs in every safety mode.

use std::time::Instant;

use async_trait::async_trait;

use crate::domain::{Question, QuestionAnswer, QuestionResolution, ToolDefinition, ToolOutcome};

use super::super::ctx::ExecContext;
use super::ToolExecutor;

pub struct AskUserQuestionTool;

/// Format the resolved answers into the text the model sees, keyed by question
/// so a batched call never scrambles which answer belongs to which question.
fn format_answers(answers: &[QuestionAnswer]) -> String {
    let mut out = String::from("The user answered your question(s):\n");
    for a in answers {
        let value = if a.selected.is_empty() {
            "(no selection)".to_string()
        } else {
            a.selected.join(", ")
        };
        out.push_str(&format!("- {} -> {}\n", a.question, value));
        if let Some(note) = &a.note {
            out.push_str(&format!("  (note: {})\n", note));
        }
    }
    out
}

/// One-line UI summary of the answers.
fn summarize_answers(answers: &[QuestionAnswer]) -> String {
    match answers {
        [] => "no questions".to_string(),
        [one] => {
            if one.selected.is_empty() {
                "no selection".to_string()
            } else {
                one.selected.join(", ")
            }
        },
        many => format!("{} questions answered", many.len()),
    }
}

#[async_trait]
impl ToolExecutor for AskUserQuestionTool {
    fn name(&self) -> &'static str {
        "ask_user_question"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ask_user_question".to_string(),
            description: "Ask the user one or more multiple-choice questions when you are genuinely blocked on a decision that is theirs to make — one you cannot resolve from their request, the code, or a sensible default. The terminal renders an interactive selectable prompt and the user's answer comes back as this tool's result. \
                Use it only when the answer changes what you do next; do NOT use it for choices with an obvious default (just pick, say so, and proceed) or for facts you can verify yourself. The user can always type a custom \"Other\" answer, so your options need not be exhaustive. \
                Batch up to 4 independent questions in one call rather than asking one at a time. For each question set `multiSelect` true when the choices are not mutually exclusive (the user may check any number). List a recommended option first and mark it by ending its label with \"(Recommended)\". \
                Attach an optional preview to an option (a `content` string plus an optional `diff` flag) to show an ASCII mockup, code, config, or a unified diff side-by-side when that option is focused — a diff of the change an option would make is often clearer than a text description."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "1-4 questions to ask, shown together.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "header": {
                                    "type": "string",
                                    "description": "Very short label (<=12 chars) shown as a chip, e.g. \"Database\"."
                                },
                                "question": {
                                    "type": "string",
                                    "description": "The full question text. Clear, specific, ends with a question mark."
                                },
                                "multiSelect": {
                                    "type": "boolean",
                                    "description": "True if the user may select multiple options; false for exactly one."
                                },
                                "options": {
                                    "type": "array",
                                    "description": "2-4 options (more allowed). Each is a distinct choice.",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {
                                                "type": "string",
                                                "description": "Concise choice text. End with \"(Recommended)\" to flag your suggestion."
                                            },
                                            "description": {
                                                "type": "string",
                                                "description": "One line explaining the option or its trade-off."
                                            },
                                            "preview": {
                                                "type": "object",
                                                "description": "Optional side-by-side preview shown when this option is focused.",
                                                "properties": {
                                                    "content": { "type": "string", "description": "The preview body, shown as monospace lines." },
                                                    "language": { "type": "string", "description": "Language hint for the content (e.g. \"rust\", \"yaml\")." },
                                                    "diff": { "type": "boolean", "description": "Render content as a unified diff (+ lines green, - lines red). Best for showing the change an option would produce." }
                                                },
                                                "required": ["content"]
                                            }
                                        },
                                        "required": ["label", "description"]
                                    }
                                }
                            },
                            "required": ["question", "header", "options", "multiSelect"]
                        }
                    }
                },
                "required": ["questions"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let start = Instant::now();
        let secs = || start.elapsed().as_secs_f64();

        let Some(questions_val) = args.get("questions") else {
            return ToolOutcome::error(
                "ask_user_question requires a `questions` array",
                secs(),
            );
        };
        let mut questions: Vec<Question> = match serde_json::from_value(questions_val.clone()) {
            Ok(q) => q,
            Err(e) => {
                return ToolOutcome::error(format!("invalid `questions`: {e}"), secs());
            },
        };
        if questions.is_empty() {
            return ToolOutcome::error(
                "`questions` must contain at least one question",
                secs(),
            );
        }
        for q in &mut questions {
            if q.options.is_empty() {
                return ToolOutcome::error(
                    format!("question \"{}\" has no options", q.header),
                    secs(),
                );
            }
            // Honor the Claude-Code convention: a trailing "(Recommended)" in
            // the label flags the option (kept in the label text verbatim).
            for o in &mut q.options {
                if o.label.to_lowercase().contains("(recommended)") {
                    o.recommended = true;
                }
            }
        }

        let Some(broker) = ctx.questions.as_ref() else {
            // Headless / no interactive terminal: nobody to ask. Proceed rather
            // than block an automated run.
            return ToolOutcome::success(
                "No interactive terminal is available, so the user could not be asked. \
                 Proceed using your best judgment and state the assumption you made.",
                "ask_user_question (no interactive terminal)",
                secs(),
            );
        };

        match broker
            .request(&ctx.token, ctx.turn, ctx.call_id, questions)
            .await
        {
            QuestionResolution::Answered(answers) => {
                ToolOutcome::success(format_answers(&answers), summarize_answers(&answers), secs())
            },
            QuestionResolution::Dismissed => ToolOutcome::success(
                "The user dismissed the question(s) without answering. Do not re-ask unless you \
                 still need the information; otherwise proceed with your best judgment.",
                "ask_user_question (dismissed)",
                secs(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::QuestionAnswer;

    #[test]
    fn formats_keyed_answers() {
        let answers = vec![
            QuestionAnswer {
                header: "Database".to_string(),
                question: "Which database?".to_string(),
                selected: vec!["PostgreSQL".to_string()],
                note: None,
            },
            QuestionAnswer {
                header: "Features".to_string(),
                question: "Which features?".to_string(),
                selected: vec!["Auth".to_string(), "Admin".to_string()],
                note: Some("also add profiles".to_string()),
            },
        ];
        let out = format_answers(&answers);
        assert!(out.contains("Which database? -> PostgreSQL"));
        assert!(out.contains("Which features? -> Auth, Admin"));
        assert!(out.contains("(note: also add profiles)"));
    }

    #[test]
    fn empty_selection_reads_as_no_selection() {
        let answers = vec![QuestionAnswer {
            header: "Layout".to_string(),
            question: "Which layout?".to_string(),
            selected: vec![],
            note: None,
        }];
        assert!(format_answers(&answers).contains("-> (no selection)"));
        assert_eq!(summarize_answers(&answers), "no selection");
    }

    #[tokio::test]
    async fn headless_proceeds_without_broker() {
        use crate::domain::{ToolCallId, TurnId};
        let (ctx, _rx) = crate::providers::ctx::test_exec_context(
            TurnId(1),
            ToolCallId(1),
            std::env::temp_dir(),
        );
        // test_exec_context leaves `questions: None` (headless).
        let out = AskUserQuestionTool
            .execute(
                serde_json::json!({
                    "questions": [{
                        "header": "DB",
                        "question": "Which database?",
                        "multiSelect": false,
                        "options": [
                            {"label": "PostgreSQL", "description": "relational"},
                            {"label": "SQLite", "description": "embedded"}
                        ]
                    }]
                }),
                ctx,
            )
            .await;
        assert_eq!(out.status, crate::domain::ToolStatus::Success);
        assert!(out.model_content.contains("Proceed"));
    }
}
