//! The `memory` tool — the agent's autonomous write path into durable
//! semantic memory.
//!
//! Reading needs no tool: the memory index is always in context and the full
//! facts are fetched with `read_file` on the listed paths. This tool only
//! *changes* memory, with three actions:
//!   - `remember` — create a new atomic fact.
//!   - `update`   — replace one fact's body (clean replace, never a merge).
//!   - `forget`   — delete a fact.
//!
//! The directory chosen by `scope` is authoritative (private by default,
//! `shared`/`global` opt-in). Writes are ungated in every safety mode except
//! read-only (see `ToolCategory::Memory` in the policy engine) — there is no
//! approval modal — so the gate here only blocks read-only.

use async_trait::async_trait;

use crate::app::memory::{self, MemoryScope};
use crate::domain::{ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata};

use super::super::ctx::ExecContext;
use super::ToolExecutor;

pub struct MemoryTool;

fn scope_from_args(args: &serde_json::Value) -> MemoryScope {
    if args
        .get("global")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        MemoryScope::Global
    } else if args
        .get("shared")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        MemoryScope::ProjectShared
    } else {
        MemoryScope::ProjectPrivate
    }
}

fn str_arg(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn tags_arg(args: &serde_json::Value) -> Vec<String> {
    args.get("tags")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[async_trait]
impl ToolExecutor for MemoryTool {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "memory".to_string(),
            description: "Manage your durable, cross-session memory of semantic facts: preferences, project conventions, decisions and their rationale, and hard-won gotchas. The memory index is always in your context; use this tool to change it. \
                `action=remember` saves a new fact; `action=update` replaces one fact's body (pass its `id`); `action=forget` deletes a fact (pass its `id`). \
                Keep each fact atomic — one idea per memory — and never merge or re-summarize the whole corpus. \
                Default scope is project-private (machine-local, not committed); set `shared=true` for team facts committed to the repo's .mermaid/memory, or `global=true` for facts that apply across all projects. \
                Save durable knowledge, not transient task state, and never store secrets, tokens, or PII."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["remember", "update", "forget"],
                        "description": "What to do."
                    },
                    "name": {
                        "type": "string",
                        "description": "Short title for the fact; also the filename. Required for remember."
                    },
                    "content": {
                        "type": "string",
                        "description": "The fact itself (Markdown). Required for remember and update."
                    },
                    "description": {
                        "type": "string",
                        "description": "One-line summary shown in the always-loaded index. Defaults to the first line of content."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional keywords."
                    },
                    "shared": {
                        "type": "boolean",
                        "description": "Store in the committed project-shared scope (.mermaid/memory) instead of private."
                    },
                    "global": {
                        "type": "boolean",
                        "description": "Store in the global, cross-project scope."
                    },
                    "id": {
                        "type": "string",
                        "description": "Name/id of the fact to update or forget (as shown in the index)."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let start = std::time::Instant::now();
        let action = str_arg(&args, "action").unwrap_or_default();

        // Ungated except read-only: this returns `Some(block)` only when the
        // policy denies (read-only mode); it never touches the approval broker.
        if let Some(blocked) = super::policy_gate::gate_external(
            &ctx,
            "memory",
            crate::runtime::ToolCategory::Memory,
            format!("memory {action}"),
            &args,
        )
        .await
        {
            return blocked;
        }

        let secs = || start.elapsed().as_secs_f64();
        match action.as_str() {
            "remember" => {
                let Some(name) = str_arg(&args, "name") else {
                    return ToolOutcome::error("memory remember requires 'name'", secs());
                };
                let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
                    return ToolOutcome::error("memory remember requires 'content'", secs());
                };
                let scope = scope_from_args(&args);
                let description = str_arg(&args, "description").unwrap_or_else(|| {
                    content
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("")
                        .to_string()
                });
                let tags = tags_arg(&args);
                match memory::write_memory(&ctx.workdir, scope, &name, &description, &tags, content)
                {
                    Ok(path) => finish(start, "remember", &name, scope, &path),
                    Err(e) => ToolOutcome::error(format!("memory remember failed: {e}"), secs()),
                }
            },
            "update" => {
                let Some(id) = str_arg(&args, "id").or_else(|| str_arg(&args, "name")) else {
                    return ToolOutcome::error("memory update requires 'id'", secs());
                };
                let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
                    return ToolOutcome::error("memory update requires 'content'", secs());
                };
                let Some(existing) = memory::find(&ctx.workdir, &id) else {
                    return ToolOutcome::error(
                        format!("memory update: no memory named '{id}'"),
                        secs(),
                    );
                };
                let description =
                    str_arg(&args, "description").unwrap_or_else(|| existing.description.clone());
                let tags = tags_arg(&args);
                match memory::write_memory(
                    &ctx.workdir,
                    existing.scope,
                    &existing.name,
                    &description,
                    &tags,
                    content,
                ) {
                    Ok(path) => {
                        // If the slug differs from the original file's stem
                        // (e.g. a hand-named file), drop the stale file so we
                        // don't leave a duplicate.
                        if path != existing.path {
                            let _ = std::fs::remove_file(&existing.path);
                        }
                        finish(start, "update", &existing.name, existing.scope, &path)
                    },
                    Err(e) => ToolOutcome::error(format!("memory update failed: {e}"), secs()),
                }
            },
            "forget" => {
                let Some(id) = str_arg(&args, "id").or_else(|| str_arg(&args, "name")) else {
                    return ToolOutcome::error("memory forget requires 'id'", secs());
                };
                match memory::delete_memory(&ctx.workdir, &id) {
                    Ok(Some(path)) => ToolOutcome::success(
                        format!("Forgot memory '{id}' ({})", path.display()),
                        format!("forgot {id}"),
                        secs(),
                    )
                    .with_metadata(ToolRunMetadata {
                        detail: ToolMetadata::Custom {
                            name: "memory".to_string(),
                            data: serde_json::json!({
                                "action": "forget",
                                "id": id,
                                "path": path.display().to_string(),
                            }),
                        },
                        ..ToolRunMetadata::default()
                    }),
                    Ok(None) => {
                        ToolOutcome::error(format!("memory forget: no memory named '{id}'"), secs())
                    },
                    Err(e) => ToolOutcome::error(format!("memory forget failed: {e}"), secs()),
                }
            },
            other => ToolOutcome::error(
                format!("memory: unknown action '{other}' (expected remember, update, or forget)"),
                secs(),
            ),
        }
    }
}

/// Shared success path for remember/update.
fn finish(
    start: std::time::Instant,
    action: &str,
    name: &str,
    scope: MemoryScope,
    path: &std::path::Path,
) -> ToolOutcome {
    let verb = if action == "remember" {
        "Remembered"
    } else {
        "Updated"
    };
    ToolOutcome::success(
        format!("{verb} '{name}' [{}] → {}", scope.as_str(), path.display()),
        format!("{action} {name} [{}]", scope.as_str()),
        start.elapsed().as_secs_f64(),
    )
    .with_metadata(ToolRunMetadata {
        detail: ToolMetadata::Custom {
            name: "memory".to_string(),
            data: serde_json::json!({
                "action": action,
                "name": name,
                "scope": scope.as_str(),
                "path": path.display().to_string(),
            }),
        },
        ..ToolRunMetadata::default()
    })
}
