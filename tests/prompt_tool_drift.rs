//! Drift guard: every tool the system prompt names must exist in the registry.
//!
//! Lives here, not beside the prompt. `mermaid-domain` is the pure MVU core and
//! cannot depend on `providers` — that is the whole point of the crate
//! boundary — but this assertion needs both the prompt text and the real
//! `ToolRegistry`. An integration test is the one place that sees both.

use mermaid_cli::providers::tool::ToolRegistry;

/// Systematized version of the old `subagent` regression: every core
/// tool name the prompt advertises must resolve in the registry, so the
/// prose inventory can't drift from the dispatchable surface.
#[test]
fn advertised_tools_exist_in_the_registry() {
    let prompt = mermaid_domain::prompts::get_system_prompt();
    let registry = ToolRegistry::default();
    for name in [
        "read_file",
        "write_file",
        "apply_patch",
        "delete_file",
        "create_directory",
        "execute_command",
        "memory",
        "ask_user_question",
    ] {
        assert!(
            prompt.contains(&format!("`{name}`")),
            "prompt must advertise `{name}`"
        );
        assert!(
            registry.get(name).is_some(),
            "advertised tool `{name}` must be registered"
        );
    }
}
