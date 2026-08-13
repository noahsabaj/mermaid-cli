use crate::cmd::Cmd;
use crate::reducer::push_system;
use crate::state::State;

/// `/agents` — list detached background agents, or kill them
/// (`kill <id>` / `kill all`). Kills validate against the registry here
/// (immediate feedback for a bad id), mark the row "cancelling…", and hand
/// the actual token fire to the effect layer via `Cmd::KillBackgroundAgent`;
/// the dying child's `Msg::BackgroundAgentFinished { cancelled: true, .. }`
/// posts the closing note and clears the row.
pub fn handle_slash_agents(state: &mut State, cmds: &mut Vec<Cmd>, arg: Option<&str>) {
    let arg = arg.map(str::trim).filter(|s| !s.is_empty());
    match arg {
        None => {
            if state.runtime.background_agents.is_empty() {
                push_system(
                    state,
                    cmds,
                    "No background agents. (ctrl+b detaches a running agent)",
                );
                return;
            }
            let now_sys = std::time::SystemTime::from(state.now);
            let mut lines = vec![format!(
                "Background agents ({}) — /agents kill <id> cancels one, /agents kill all cancels every one",
                state.runtime.background_agents.len()
            )];
            for agent in &state.runtime.background_agents {
                let elapsed = now_sys
                    .duration_since(agent.started)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                lines.push(format!(
                    "  {}  {} — {} · {}s · ~{} tokens",
                    agent.agent_id,
                    agent.description,
                    agent.activity,
                    elapsed,
                    crate::compaction::format_compact_count(agent.tokens),
                ));
            }
            push_system(state, cmds, lines.join("\n"));
        },
        Some("kill all") => {
            if state.runtime.background_agents.is_empty() {
                push_system(state, cmds, "No background agents to kill.");
                return;
            }
            for agent in &mut state.runtime.background_agents {
                agent.activity = "cancelling…".to_string();
            }
            cmds.push(Cmd::KillBackgroundAgent { agent_id: None });
        },
        Some(rest) if rest.starts_with("kill ") || rest == "kill" => {
            let id = rest.strip_prefix("kill").unwrap_or_default().trim();
            if id.is_empty() {
                push_system(
                    state,
                    cmds,
                    "Usage: /agents kill <id> (or: /agents kill all)",
                );
                return;
            }
            let Some(agent) = state
                .runtime
                .background_agents
                .iter_mut()
                .find(|a| a.agent_id == id)
            else {
                push_system(state, cmds, format!("No background agent '{id}'."));
                return;
            };
            agent.activity = "cancelling…".to_string();
            cmds.push(Cmd::KillBackgroundAgent {
                agent_id: Some(id.to_string()),
            });
        },
        Some(_) => {
            push_system(
                state,
                cmds,
                "Usage: /agents (list), /agents kill <id>, /agents kill all",
            );
        },
    }
}
