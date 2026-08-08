//! Plan-mode domain helpers: the tool definitions `build_chat_request`
//! advertises conditionally (the executors are registered as internal, so
//! `describe_all` never emits them), and the Tasks-section parser that seeds
//! the checklist when a plan is approved.

use super::checklist::ChecklistSpec;
use super::cmd::ToolDefinition;

/// Wire name of the tool the model calls to present the finished plan for
/// approval. Advertised only while `session.plan` is `Some`.
pub const EXIT_PLAN_MODE_TOOL: &str = "exit_plan_mode";

/// Wire name of the tool the model calls to propose entering plan mode.
/// Advertised only while NOT planning (and never to subagents).
pub const ENTER_PLAN_MODE_TOOL: &str = "enter_plan_mode";

pub fn exit_plan_mode_definition() -> ToolDefinition {
    ToolDefinition {
        name: EXIT_PLAN_MODE_TOOL.to_string(),
        description: "Present the finished plan for approval. Call this when the plan file is \
            complete and decision-complete — never ask \"should I proceed?\" in chat; this tool \
            IS that question. It re-reads the plan file from disk (the user may have edited it — \
            their version wins) and shows an approval dialog. Approval ends plan mode and seeds \
            the task checklist from the plan's Tasks section; a request for changes returns the \
            user's feedback and plan mode continues — revise the plan file (complete \
            replacement) and call this again."
            .to_string(),
        input_schema: serde_json::json!({ "type": "object", "properties": {} }),
    }
}

pub fn enter_plan_mode_definition() -> ToolDefinition {
    ToolDefinition {
        name: ENTER_PLAN_MODE_TOOL.to_string(),
        description: "Propose entering plan mode: a read-only state where you explore, ask, and \
            write a plan the user approves before any implementation. Use it when the task is \
            large, risky, or underspecified enough that the user should review a written design \
            before the working tree changes. Entering is immediate and safe (read-only floor). \
            Use sparingly — small, well-understood tasks don't need a plan."
            .to_string(),
        input_schema: serde_json::json!({ "type": "object", "properties": {} }),
    }
}

/// Parse the plan's `## Tasks` section into checklist specs: one spec per
/// top-level list item (numbered `1.` / `1)` or bulleted `-` / `*`), in
/// order. Continuation/indent lines and prose are ignored — the plan file
/// remains the detail carrier; the checklist carries the headline steps.
pub fn parse_plan_tasks(body: &str) -> Vec<ChecklistSpec> {
    let mut specs = Vec::new();
    let mut in_tasks = false;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if let Some(heading) = trimmed.strip_prefix("## ") {
            in_tasks = heading.trim().eq_ignore_ascii_case("tasks");
            continue;
        }
        if !in_tasks {
            continue;
        }
        // Only top-level items: an indented line is a continuation/sub-item.
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(item) = strip_list_marker(trimmed) else {
            continue;
        };
        if item.is_empty() {
            continue;
        }
        specs.push(ChecklistSpec {
            subject: item.to_string(),
            active_form: item.to_string(),
            description: None,
            in_progress: false,
        });
    }
    specs
}

/// Strip a leading list marker (`- `, `* `, `1. `, `12) `); `None` when the
/// line isn't a list item.
fn strip_list_marker(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix("- ").or_else(|| s.strip_prefix("* ")) {
        return Some(rest.trim());
    }
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let rest = &s[digits..];
        if let Some(r) = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')')) {
            return Some(r.trim());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numbered_and_bulleted_tasks_from_the_tasks_section_only() {
        let body = "\
## Summary
Do the thing.

## Tasks
1. Add the config flag
2) Wire the broker
- Update the docs
  continuation detail line is ignored
* Ship it

## Verification
1. This numbered item is NOT a task
";
        let specs = parse_plan_tasks(body);
        let subjects: Vec<&str> = specs.iter().map(|s| s.subject.as_str()).collect();
        assert_eq!(
            subjects,
            [
                "Add the config flag",
                "Wire the broker",
                "Update the docs",
                "Ship it"
            ]
        );
        assert!(specs.iter().all(|s| !s.in_progress));
        assert!(specs.iter().all(|s| s.active_form == s.subject));
    }

    #[test]
    fn no_tasks_section_yields_no_specs() {
        assert!(parse_plan_tasks("## Summary\n1. not a task\n").is_empty());
        assert!(parse_plan_tasks("").is_empty());
    }
}
