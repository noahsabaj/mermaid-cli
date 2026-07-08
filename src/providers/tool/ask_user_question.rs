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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;

use crate::domain::{
    OptionPreview, Question, QuestionAnswer, QuestionKind, QuestionOption, QuestionResolution,
    TextValidate, ToolDefinition, ToolOutcome,
};

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

/// Parse the model's flat option JSON into a `QuestionOption`.
fn parse_option(v: &serde_json::Value) -> Option<QuestionOption> {
    let label = v.get("label").and_then(|x| x.as_str())?.to_string();
    let description = v
        .get("description")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let preview = v.get("preview").and_then(parse_preview);
    // Honor the Claude-Code convention: a trailing "(Recommended)" flags it.
    let recommended = label.to_lowercase().contains("(recommended)");
    Some(QuestionOption {
        label,
        description,
        recommended,
        preview,
    })
}

fn parse_preview(v: &serde_json::Value) -> Option<OptionPreview> {
    let content = v.get("content").and_then(|x| x.as_str())?.to_string();
    let language = v
        .get("language")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let diff = v.get("diff").and_then(|x| x.as_bool()).unwrap_or(false);
    Some(OptionPreview {
        content,
        language,
        diff,
    })
}

/// Parse a `text` question's `validate`: the string "number"/"any", any other
/// string as a regex pattern, or `{ "regex": "..." }`.
fn parse_validate(v: Option<&serde_json::Value>) -> TextValidate {
    match v {
        None => TextValidate::Any,
        Some(val) => {
            if let Some(s) = val.as_str() {
                match s {
                    "number" => TextValidate::Number,
                    "any" | "" => TextValidate::Any,
                    pat => TextValidate::Regex(pat.to_string()),
                }
            } else if let Some(pat) = val.get("regex").and_then(|x| x.as_str()) {
                TextValidate::Regex(pat.to_string())
            } else {
                TextValidate::Any
            }
        },
    }
}

/// Parse one flat question object into a `Question`.
fn parse_question(v: &serde_json::Value) -> Result<Question, String> {
    let header = v
        .get("header")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let question = v
        .get("question")
        .and_then(|x| x.as_str())
        .ok_or("each question needs a `question` string")?
        .to_string();
    let kind = match v.get("kind").and_then(|x| x.as_str()).unwrap_or("select") {
        "select" => QuestionKind::Select,
        "multiSelect" | "multiselect" => QuestionKind::MultiSelect,
        "rank" => QuestionKind::Rank,
        "text" => QuestionKind::Text {
            validate: parse_validate(v.get("validate")),
        },
        "number" => QuestionKind::Number {
            min: v.get("min").and_then(|x| x.as_f64()),
            max: v.get("max").and_then(|x| x.as_f64()),
            step: v.get("step").and_then(|x| x.as_f64()),
            slider: v.get("slider").and_then(|x| x.as_bool()).unwrap_or(false),
        },
        "date" => QuestionKind::Date,
        "path" => QuestionKind::Path {
            must_exist: v
                .get("mustExist")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
        },
        other => return Err(format!("unknown question kind: {other}")),
    };
    let options = v
        .get("options")
        .and_then(|x| x.as_array())
        .map(|arr| arr.iter().filter_map(parse_option).collect::<Vec<_>>())
        .unwrap_or_default();
    let memory_key = v
        .get("memoryKey")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let q = Question {
        header,
        question,
        kind,
        options,
        memory_key,
    };
    if q.is_choice() && q.options.is_empty() {
        return Err(format!(
            "question \"{}\" is a choice kind but has no options",
            q.question
        ));
    }
    Ok(q)
}

/// A remembered answer, persisted across sessions keyed by a question's
/// `memory_key`.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct StoredAnswer {
    selected: Vec<String>,
    #[serde(default)]
    note: Option<String>,
}

fn prefs_path() -> Option<PathBuf> {
    crate::app::get_config_dir()
        .ok()
        .map(|d| d.join("question_prefs.json"))
}

fn load_prefs_at(path: &Path) -> HashMap<String, StoredAnswer> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_prefs_at(path: &Path, map: &HashMap<String, StoredAnswer>) {
    if let Ok(json) = serde_json::to_string_pretty(map) {
        let _ = std::fs::write(path, json);
    }
}

fn load_prefs() -> HashMap<String, StoredAnswer> {
    prefs_path().map(|p| load_prefs_at(&p)).unwrap_or_default()
}

fn save_prefs(map: &HashMap<String, StoredAnswer>) {
    if let Some(p) = prefs_path() {
        save_prefs_at(&p, map);
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
                Batch up to 4 independent questions in one call rather than asking one at a time. Set each question's `kind`: `select` (pick one), `multiSelect` (pick any), `rank` (reorder options), or an input kind that collects a typed value — `text` (optional `validate`), `number` (`min`/`max`/`step`/`slider`), `date`, or `path`. Choice kinds need `options`; list a recommended option first and mark it by ending its label with \"(Recommended)\". \
                Attach an optional preview to an option (a `content` string plus an optional `diff` flag) to show an ASCII mockup, code, config, or a unified diff side-by-side when that option is focused — a diff of the change an option would make is often clearer than a text description. \
                Set `memoryKey` on a question to let the user remember the answer across sessions, so settled preferences (package manager, code style) aren't re-asked."
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
                                "kind": {
                                    "type": "string",
                                    "enum": ["select", "multiSelect", "rank", "text", "number", "date", "path"],
                                    "description": "How the question is answered. Choice kinds use `options`: `select` (pick one), `multiSelect` (pick any), `rank` (reorder). Input kinds collect a typed value: `text` (optional `validate`), `number` (optional `min`/`max`/`step`/`slider`), `date` (YYYY-MM-DD), `path`."
                                },
                                "validate": {
                                    "type": "string",
                                    "description": "For `text`: \"number\" to require a number, or a regex pattern the answer must match."
                                },
                                "min": { "type": "number", "description": "For `number`: minimum value." },
                                "max": { "type": "number", "description": "For `number`: maximum value." },
                                "step": { "type": "number", "description": "For `number`: increment for Up/Down." },
                                "slider": { "type": "boolean", "description": "For `number`: show a slider bar (needs `min` and `max`)." },
                                "mustExist": { "type": "boolean", "description": "For `path`: hint that the path should already exist." },
                                "memoryKey": { "type": "string", "description": "Stable key to remember this answer across sessions. If the user opts in, a later question with the same key auto-answers without prompting. Use for settled preferences (e.g. package manager, code style)." },
                                "options": {
                                    "type": "array",
                                    "description": "Options for choice kinds (select/multiSelect/rank); omit for input kinds. Two or more; long lists scroll.",
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
                            "required": ["question", "header", "kind"]
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

        let Some(questions_val) = args.get("questions").and_then(|v| v.as_array()) else {
            return ToolOutcome::error("ask_user_question requires a `questions` array", secs());
        };
        if questions_val.is_empty() {
            return ToolOutcome::error("`questions` must contain at least one question", secs());
        }
        let mut questions = Vec::with_capacity(questions_val.len());
        for qv in questions_val {
            match parse_question(qv) {
                Ok(q) => questions.push(q),
                Err(e) => return ToolOutcome::error(format!("invalid question: {e}"), secs()),
            }
        }

        // Cross-session preferences: if every question already has a remembered
        // answer, return them without prompting (works interactively and
        // headlessly) so settled decisions aren't re-asked every session.
        let prefs = load_prefs();
        if questions
            .iter()
            .all(|q| q.memory_key.as_ref().is_some_and(|k| prefs.contains_key(k)))
        {
            let answers: Vec<QuestionAnswer> = questions
                .iter()
                .map(|q| {
                    let stored = &prefs[q.memory_key.as_ref().unwrap()];
                    QuestionAnswer {
                        header: q.header.clone(),
                        question: q.question.clone(),
                        selected: stored.selected.clone(),
                        note: stored.note.clone(),
                    }
                })
                .collect();
            return ToolOutcome::success(
                format_answers(&answers),
                format!("{} (remembered)", summarize_answers(&answers)),
                secs(),
            );
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
            .request(&ctx.token, ctx.turn, ctx.call_id, questions.clone())
            .await
        {
            QuestionResolution::Answered { answers, remember } => {
                if remember {
                    let mut prefs = prefs;
                    for (q, a) in questions.iter().zip(&answers) {
                        if let Some(key) = &q.memory_key
                            && !a.selected.is_empty()
                        {
                            prefs.insert(
                                key.clone(),
                                StoredAnswer {
                                    selected: a.selected.clone(),
                                    note: a.note.clone(),
                                },
                            );
                        }
                    }
                    save_prefs(&prefs);
                }
                ToolOutcome::success(
                    format_answers(&answers),
                    summarize_answers(&answers),
                    secs(),
                )
            },
            QuestionResolution::Dismissed => ToolOutcome::success(
                "The user dismissed the question(s) without answering. Do not re-ask unless you \
                 still need the information; otherwise proceed with your best judgment.",
                "ask_user_question (dismissed)",
                secs(),
            ),
            QuestionResolution::Reformulate => ToolOutcome::success(
                "The user chose to discuss these questions rather than answer them as posed. \
                 Do not re-issue the same questions; engage with what they say next and \
                 reformulate your approach based on their input.",
                "ask_user_question (chat about this)",
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

    #[test]
    fn prefs_round_trip() {
        let dir = std::env::temp_dir().join(format!("mermaid_qprefs_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prefs.json");
        let mut map = HashMap::new();
        map.insert(
            "pkg_mgr".to_string(),
            StoredAnswer {
                selected: vec!["pnpm".to_string()],
                note: None,
            },
        );
        save_prefs_at(&path, &map);
        let loaded = load_prefs_at(&path);
        assert_eq!(
            loaded.get("pkg_mgr").map(|s| s.selected.clone()),
            Some(vec!["pnpm".to_string()])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
