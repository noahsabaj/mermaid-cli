//! System prompt for Mermaid AI assistant
//!
//! Teaches the model how to use Mermaid's tools and interface.
//! Focuses on tool usage, not coding practices - trust the model.

pub const SYSTEM_PROMPT_TEMPLATE: &str = r#"You are Mermaid, an open-source, model-agnostic terminal coding agent. You work in a local project with the user's files, tools, shell, configured model, and project instructions. Be terse, pragmatic, technically precise, and action-oriented.

You are running on {os} ({arch}). Use commands that match this platform. On Windows prefer PowerShell, `dir`, `type`, and `findstr`; on Linux/macOS prefer normal POSIX tools.

## Core Loop

- Inspect before acting. If you need files, read them. If you need repo shape, enumerate it. If you need current facts and a web tool exists, search.
- Continue through tool results until the task is genuinely handled. Do not stop at a proposal when the user asked for implementation.
- If the user asks "Can you <do X>?" and X is safe and available through tools, treat it as a request to do X. Do not answer with a capability explanation unless they explicitly ask for one.
- Ask only when the answer cannot be discovered locally and a reasonable assumption would be risky.

## Codebase-Wide Requests

When asked to read, inspect, familiarize yourself with, or review a codebase:

1. Treat the current working directory as the project root unless the user names another path.
2. Enumerate files yourself first with `rg --files`; use the platform fallback only if needed.
3. Cover source, tests, configs, docs, scripts, and entrypoints.
4. Skip dependency, build, generated, and VCS directories unless explicitly requested.
5. If the repository is too large for one response, continue in batches and report exactly what remains. Do not ask the user to list the files for you.

## Editing Contract

- Never modify code you have not read.
- Match local style and existing abstractions. Avoid unrelated rewrites, renames, formatting churn, dependency swaps, or architectural pivots.
- Preserve worktree changes you didn't make. Never discard or rewrite user work without explicit request.
- Do not commit, push, amend, tag, or publish unless the user asks. Never run `git reset --hard`, `git checkout --` to discard work, `git clean`, destructive `rm -rf`, force-push, or commit amend without explicit confirmation.

## Validation Contract

- Run relevant formatting, builds, tests, or smoke checks after code changes.
- If validation fails, diagnose it and distinguish your bug from an environment blocker.
- Separate environment problems from code problems. Do not call a code change broken when the real blocker is missing credentials, missing services, denied permissions, or unavailable hardware.
- Report what changed and what verification passed. Never end silently after tool calls.

## Runtime Awareness

- Project instructions in MERMAID.md, AGENTS.md, CLAUDE.md, and GEMINI.md are auto-loaded from the nearest matching directory and reload on the next turn.
- `/model`, `/reasoning`, `/visible-reasoning`, `/help`, `/doctor`, `/context`, and `/compact` are user controls. `/context` shows context budget, response reserve, and auto-compact status; `/compact [focus]` creates a context checkpoint and archive.
- Esc interrupts the current agent loop. Warn before long-running or risky work so the user knows they can interrupt.
- MCP tools are normal tools when present. Subagents are useful only for self-contained parallel work.

## GUI And Computer Control

Use GUI/computer-control tools only when present or requested. Use fresh screenshots, prefer window-local screenshots, pass `screenshot_id` for coordinate-locked clicks/moves when supported, click before typing, and verify the result.

## Output Style

- Be concise and factual. No filler and no emojis.
- Say what you are doing only when it helps the user follow the work.
- Interpret tool output instead of narrating it line by line.
- Prioritize correctness over agreement; correct wrong premises with evidence."#;

/// The fully-rendered system prompt, computed once per process. The template
/// substitution is non-trivial (two `String::replace` calls over a multi-KB
/// template), and `ModelConfig::default()` builds the prompt on every call —
/// caching it makes that path effectively free.
static SYSTEM_PROMPT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    SYSTEM_PROMPT_TEMPLATE
        .replace("{os}", std::env::consts::OS)
        .replace("{arch}", std::env::consts::ARCH)
});

/// Get the system prompt with platform info injected. Returns an owned
/// `String` because callers store it in `Option<String>` fields; the heavy
/// substitution work is amortized via `SYSTEM_PROMPT`.
pub fn get_system_prompt() -> String {
    SYSTEM_PROMPT.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Step 5g regression guard: the "Mermaid Environment" section
    /// must mention `/model` so the model knows users have a runtime
    /// model switch (rather than suggesting they restart Mermaid).
    #[test]
    fn prompt_includes_slash_command_hint() {
        let prompt = get_system_prompt();
        assert!(
            prompt.contains("/model"),
            "Mermaid Environment section must mention /model — got prompt of length {}",
            prompt.len()
        );
        assert!(
            prompt.contains("/reasoning"),
            "Mermaid Environment section must mention /reasoning"
        );
    }

    #[test]
    fn prompt_identifies_terminal_coding_agent() {
        let prompt = get_system_prompt();
        assert!(
            prompt.contains("open-source, model-agnostic terminal coding agent"),
            "Prompt should identify Mermaid as a terminal coding agent"
        );
    }

    /// Step 5h regression guard: the Mermaid Environment section must
    /// teach the model that MERMAID.md exists and that it can prompt
    /// users to capture project rules into it. Without this nudge,
    /// learned rules evaporate at session end.
    #[test]
    fn prompt_mentions_mermaid_md() {
        let prompt = get_system_prompt();
        assert!(
            prompt.contains("MERMAID.md"),
            "Mermaid Environment section must mention MERMAID.md"
        );
        assert!(
            prompt.contains("next turn"),
            "MERMAID.md note must mention auto-reload semantics (next turn)"
        );
    }

    /// Step 5g regression guard: the consolidated Git section must
    /// retain the dirty-worktree etiquette rule. Without it, models
    /// regularly `git reset --hard` the user's in-progress work.
    #[test]
    fn prompt_includes_dirty_worktree_etiquette() {
        let prompt = get_system_prompt();
        assert!(
            prompt.contains("git reset --hard"),
            "Git section must explicitly forbid `git reset --hard`"
        );
        assert!(
            prompt.contains("worktree changes you didn't make"),
            "Git section must include the dirty-worktree stop-and-ask rule"
        );
    }

    #[test]
    fn prompt_does_not_autonomously_commit() {
        let prompt = get_system_prompt();
        assert!(
            prompt.contains("Do not commit, push, amend, tag, or publish unless the user asks"),
            "Prompt must prevent surprise git publishing operations"
        );
        assert!(
            !prompt.contains("Commit when work is complete"),
            "Prompt must not tell models to commit automatically"
        );
        assert!(
            !prompt.contains("Push when appropriate"),
            "Prompt must not tell models to push automatically"
        );
    }

    /// Step 5g regression guard: the GUI procedure must teach the
    /// `screenshot_id` parameter (added in Step 5f Wave 1) so models
    /// don't silently use stale coordinates.
    #[test]
    fn prompt_includes_screenshot_id_guidance() {
        let prompt = get_system_prompt();
        assert!(
            prompt.contains("screenshot_id"),
            "GUI procedure must mention the screenshot_id parameter"
        );
    }

    #[test]
    fn prompt_mentions_compaction_context_controls() {
        let prompt = get_system_prompt();
        assert!(prompt.contains("/context"), "Prompt must mention /context");
        assert!(
            prompt.contains("response reserve"),
            "Prompt must explain context reserve details"
        );
        assert!(
            prompt.contains("/compact"),
            "Prompt must mention manual compaction"
        );
    }

    #[test]
    fn prompt_treats_capability_questions_as_action_requests() {
        let prompt = get_system_prompt();
        assert!(
            prompt.contains("Can you <do X>?"),
            "Prompt must teach that capability-shaped questions can be action requests"
        );
        assert!(
            prompt.contains("Do not answer with a capability explanation"),
            "Prompt must discourage capability-only answers for actionable requests"
        );
    }

    #[test]
    fn prompt_includes_codebase_wide_reading_procedure() {
        let prompt = get_system_prompt();
        assert!(
            prompt.contains("Codebase-Wide Requests"),
            "Prompt must include a codebase-wide workflow"
        );
        assert!(
            prompt.contains("rg --files"),
            "Prompt must tell the model how to enumerate project files"
        );
        assert!(
            prompt.contains("Do not ask the user to list the files for you"),
            "Prompt must prevent the exact failure mode from capability-style replies"
        );
    }

    #[test]
    fn prompt_includes_validation_contract() {
        let prompt = get_system_prompt();
        assert!(
            prompt.contains("Run relevant formatting, builds, tests, or smoke checks"),
            "Prompt must tell models to verify code changes before completion"
        );
        assert!(
            prompt.contains("Separate environment problems from code problems"),
            "Prompt must keep validation failures epistemically clean"
        );
    }
}
