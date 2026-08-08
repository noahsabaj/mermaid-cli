//! Deferred MCP tools and the `tool_search` meta-tool (pure).
//!
//! With deferral on (the default, `mcp_defer_tools`), the reducer does not
//! advertise every MCP server's tools on every request. Instead it
//! advertises one `tool_search` tool whose results PROMOTE matching tools
//! to direct advertisement on subsequent requests. Everything here is a
//! pure function of `State`, so `tool_search` calls are intercepted in the
//! reducer (no `Cmd::ExecuteTool`, no effect round-trip) and replay
//! deterministically.

use std::collections::BTreeSet;

use super::cmd::ToolDefinition;
use super::state::{McpServerEntry, McpServerStatus, McpToolSpec, State};

/// Advertised name of the meta-tool. Never collides with MCP tools (those
/// are always `mcp__`-prefixed) or built-ins (registry-owned names).
pub const TOOL_SEARCH_NAME: &str = "tool_search";

/// Maximum schemas returned by one `tool_search` call.
pub const MAX_TOOL_SEARCH_RESULTS: usize = 8;

/// Cap on the promoted set. At the cap, matches are still returned (the
/// model has the schema in-context for its immediate step) but are not
/// promoted into subsequent requests.
pub const MAX_PROMOTED_TOOLS: usize = 32;

/// Whether this server's tools are deferred: per-server `defer` override
/// wins, else the global `mcp_defer_tools` (unset = on).
fn server_defers(state: &State, entry: &McpServerEntry) -> bool {
    entry
        .config
        .defer
        .unwrap_or_else(|| state.settings.mcp_deferral_enabled())
}

/// Ready servers sorted by name — the stable iteration base every
/// advertising/search path shares (byte-stable requests, #F68).
fn ready_servers(state: &State) -> Vec<(&String, &McpServerEntry)> {
    let mut servers: Vec<_> = state
        .mcp
        .servers
        .iter()
        .filter(|(_, entry)| matches!(entry.status, McpServerStatus::Ready))
        .collect();
    servers.sort_by(|a, b| a.0.cmp(b.0));
    servers
}

/// Allowed (per-server filter) tools of one server. Filtering matches the
/// RAW tool name — users write `enabled_tools`/`disabled_tools` against
/// the names the server itself advertises.
fn allowed_tools(entry: &McpServerEntry) -> impl Iterator<Item = &McpToolSpec> {
    entry
        .tools
        .iter()
        .filter(|tool| entry.config.tool_allowed(&tool.raw_name))
}

/// Deferred tools not yet promoted, as (server name, spec) in stable order.
pub fn deferred_unpromoted(state: &State) -> Vec<(&String, &McpToolSpec)> {
    ready_servers(state)
        .into_iter()
        .filter(|(_, entry)| server_defers(state, entry))
        .flat_map(|(name, entry)| allowed_tools(entry).map(move |tool| (name, tool)))
        .filter(|(_, tool)| !state.mcp.promoted.contains(&tool.name))
        .collect()
}

/// The MCP portion of `ChatRequest.tools`: non-deferred servers' tools plus
/// promoted tools, with one `tool_search` definition LAST while any
/// deferred tool remains unpromoted. The effect runner prepends built-ins.
pub fn mcp_tool_definitions(state: &State) -> Vec<ToolDefinition> {
    let mut defs = Vec::new();
    for (_, entry) in ready_servers(state) {
        let defers = server_defers(state, entry);
        for tool in allowed_tools(entry) {
            if !defers || state.mcp.promoted.contains(&tool.name) {
                defs.push(ToolDefinition {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                });
            }
        }
    }
    if !deferred_unpromoted(state).is_empty() {
        defs.push(tool_search_definition(state));
    }
    defs
}

/// Build the `tool_search` definition, its description enumerating the
/// deferred servers so the model knows what is discoverable.
pub fn tool_search_definition(state: &State) -> ToolDefinition {
    let deferred = deferred_unpromoted(state);
    let mut per_server: Vec<(String, usize)> = Vec::new();
    for (server, _) in &deferred {
        match per_server.last_mut() {
            Some((name, count)) if name == *server => *count += 1,
            _ => per_server.push(((*server).clone(), 1)),
        }
    }
    let listing = per_server
        .iter()
        .map(|(name, count)| format!("{name} ({count} tools)"))
        .collect::<Vec<_>>()
        .join(", ");
    ToolDefinition {
        name: TOOL_SEARCH_NAME.to_string(),
        description: format!(
            "Search tools provided by connected MCP servers. Matching tools \
             become directly callable on your next step. Deferred servers: {listing}."
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search terms matched against tool names and descriptions. Empty lists the first tools alphabetically."
                }
            },
            "required": ["query"]
        }),
    }
}

/// Result of a `tool_search` call: the text returned to the model and the
/// sanitized names to promote into `state.mcp.promoted`.
pub struct SearchOutcome {
    /// Model-facing content: a JSON array of matching tool schemas plus a
    /// summary line.
    pub text: String,
    /// One-line human summary for the transcript tool row.
    pub summary: String,
    /// Names to insert into the promoted set (already capped).
    pub promote: Vec<String>,
}

/// Execute a `tool_search` query purely against state. `args` is the raw
/// tool-call argument value; a missing/non-string `query` is treated as
/// empty (list mode) rather than an error.
pub fn run_tool_search(state: &State, args: &serde_json::Value) -> SearchOutcome {
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let terms: Vec<&str> = query.split_whitespace().collect();

    let deferred = deferred_unpromoted(state);
    let matches: Vec<&McpToolSpec> = deferred
        .iter()
        .map(|(_, tool)| *tool)
        .filter(|tool| {
            if terms.is_empty() {
                return true;
            }
            let haystack = format!(
                "{} {} {}",
                tool.name.to_lowercase(),
                tool.raw_name.to_lowercase(),
                tool.description.to_lowercase()
            );
            terms.iter().all(|term| haystack.contains(term))
        })
        .take(MAX_TOOL_SEARCH_RESULTS)
        .collect();

    if matches.is_empty() {
        let text = if deferred.is_empty() {
            "No deferred MCP tools are available.".to_string()
        } else {
            format!(
                "No MCP tools match \"{query}\". Try broader terms; an empty query lists tools."
            )
        };
        return SearchOutcome {
            summary: "tool_search: 0 matches".to_string(),
            text,
            promote: Vec::new(),
        };
    }

    let budget = MAX_PROMOTED_TOOLS.saturating_sub(state.mcp.promoted.len());
    let promote: Vec<String> = matches
        .iter()
        .take(budget)
        .map(|tool| tool.name.clone())
        .collect();
    let promoted_all = promote.len() == matches.len();

    let schemas: Vec<serde_json::Value> = matches
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect();
    let mut text = serde_json::to_string_pretty(&schemas).unwrap_or_else(|_| "[]".to_string());
    if promoted_all {
        text.push_str("\n\nThese tools are now directly callable.");
    } else {
        text.push_str(
            "\n\nThe promoted-tool limit was reached; unpromoted matches above are \
             usable from their schemas but stay deferred.",
        );
    }
    let note = if terms.is_empty() {
        " (empty query lists alphabetically; narrow your query for more)"
    } else {
        ""
    };
    let summary = format!(
        "tool_search: {} match(es), {} promoted{note}",
        matches.len(),
        promote.len()
    );
    SearchOutcome {
        text,
        summary,
        promote,
    }
}

/// Insert promotions, preserving the cap invariant.
pub fn apply_promotions(promoted: &mut BTreeSet<String>, names: Vec<String>) {
    for name in names {
        if promoted.len() >= MAX_PROMOTED_TOOLS {
            break;
        }
        promoted.insert(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpServerEntry;
    use crate::{Config, McpServerConfig};
    use chrono::TimeZone;
    use std::path::PathBuf;

    fn spec(server: &str, tool: &str, desc: &str) -> McpToolSpec {
        McpToolSpec {
            name: format!("mcp__{server}__{tool}"),
            raw_name: tool.to_string(),
            description: desc.to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            read_only_hint: false,
        }
    }

    fn state_with_server(
        defer_global: Option<bool>,
        entries: Vec<(&str, McpServerEntry)>,
    ) -> State {
        let now = chrono::Local.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let config = Config {
            mcp_defer_tools: defer_global,
            ..Config::default()
        };
        let mut state = State::new(
            config,
            PathBuf::from("/tmp"),
            "test/model".into(),
            now,
            PathBuf::from("/tmp"),
        );
        for (name, entry) in entries {
            state.mcp.servers.insert(name.to_string(), entry);
        }
        state
    }

    fn ready_entry(tools: Vec<McpToolSpec>) -> McpServerEntry {
        McpServerEntry {
            config: McpServerConfig::default(),
            status: McpServerStatus::Ready,
            tools,
        }
    }

    #[test]
    fn deferral_on_advertises_only_tool_search() {
        let state = state_with_server(
            None,
            vec![("srv", ready_entry(vec![spec("srv", "alpha", "does alpha")]))],
        );
        let defs = mcp_tool_definitions(&state);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, TOOL_SEARCH_NAME);
        assert!(defs[0].description.contains("srv (1 tools)"));
    }

    #[test]
    fn deferral_off_advertises_tools_without_tool_search() {
        let state = state_with_server(
            Some(false),
            vec![("srv", ready_entry(vec![spec("srv", "alpha", "does alpha")]))],
        );
        let defs = mcp_tool_definitions(&state);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "mcp__srv__alpha");
    }

    #[test]
    fn per_server_defer_false_overrides_global_on() {
        let mut entry = ready_entry(vec![spec("srv", "alpha", "does alpha")]);
        entry.config.defer = Some(false);
        let state = state_with_server(None, vec![("srv", entry)]);
        let defs = mcp_tool_definitions(&state);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "mcp__srv__alpha");
    }

    #[test]
    fn promoted_tools_are_advertised_and_leave_the_deferred_pool() {
        let mut state = state_with_server(
            None,
            vec![(
                "srv",
                ready_entry(vec![
                    spec("srv", "alpha", "does alpha"),
                    spec("srv", "beta", "does beta"),
                ]),
            )],
        );
        state.mcp.promoted.insert("mcp__srv__alpha".to_string());
        let defs = mcp_tool_definitions(&state);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["mcp__srv__alpha", TOOL_SEARCH_NAME]);
    }

    #[test]
    fn all_promoted_drops_tool_search() {
        let mut state = state_with_server(
            None,
            vec![("srv", ready_entry(vec![spec("srv", "alpha", "does alpha")]))],
        );
        state.mcp.promoted.insert("mcp__srv__alpha".to_string());
        let defs = mcp_tool_definitions(&state);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "mcp__srv__alpha");
    }

    #[test]
    fn search_matches_all_terms_across_name_and_description() {
        let state = state_with_server(
            None,
            vec![(
                "srv",
                ready_entry(vec![
                    spec("srv", "list_issues", "List repository issues"),
                    spec("srv", "create_pr", "Open a pull request"),
                ]),
            )],
        );
        let out = run_tool_search(&state, &serde_json::json!({"query": "repository issues"}));
        assert_eq!(out.promote, vec!["mcp__srv__list_issues".to_string()]);
        assert!(out.text.contains("mcp__srv__list_issues"));
        assert!(!out.text.contains("create_pr"));
    }

    #[test]
    fn empty_query_lists_head_alphabetically() {
        let tools: Vec<McpToolSpec> = (0..12)
            .map(|i| spec("srv", &format!("tool{i:02}"), "desc"))
            .collect();
        let state = state_with_server(None, vec![("srv", ready_entry(tools))]);
        let out = run_tool_search(&state, &serde_json::json!({"query": ""}));
        assert_eq!(out.promote.len(), MAX_TOOL_SEARCH_RESULTS);
        assert!(out.summary.contains("narrow your query"));
    }

    #[test]
    fn no_match_returns_guidance_without_promotion() {
        let state = state_with_server(
            None,
            vec![("srv", ready_entry(vec![spec("srv", "alpha", "does alpha")]))],
        );
        let out = run_tool_search(&state, &serde_json::json!({"query": "zzz-nothing"}));
        assert!(out.promote.is_empty());
        assert!(out.text.contains("No MCP tools match"));
    }

    #[test]
    fn promotion_cap_is_enforced() {
        let tools: Vec<McpToolSpec> = (0..40)
            .map(|i| spec("srv", &format!("tool{i:02}"), "desc"))
            .collect();
        let mut state = state_with_server(None, vec![("srv", ready_entry(tools))]);
        for i in 0..(MAX_PROMOTED_TOOLS - 2) {
            state.mcp.promoted.insert(format!("mcp__srv__pre{i}"));
        }
        let out = run_tool_search(&state, &serde_json::json!({"query": "desc"}));
        assert_eq!(out.promote.len(), 2);
        assert!(out.text.contains("promoted-tool limit"));
        apply_promotions(&mut state.mcp.promoted, out.promote);
        assert_eq!(state.mcp.promoted.len(), MAX_PROMOTED_TOOLS);
    }
}
