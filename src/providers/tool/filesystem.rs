//! Filesystem tools ported to `ToolExecutor`.
//!
//! This is the proof-of-pattern tool impl for C3: `ReadFileTool` and
//! `WriteFileTool`. They hook the `ExecContext::token` so Ctrl+C
//! cancels mid-read (relevant for large files on slow storage), and
//! they emit `ProgressEvent::Status` breadcrumbs for multi-file
//! operations the old code couldn't surface without an observer
//! callback.
//!
//! The implementations don't try to out-clever the existing tool
//! behavior in `src/agents/filesystem.rs`. Same semantics, same error
//! shapes — just wrapped in the new trait so future tools only have
//! to learn this surface.

use mermaid_domain::ProgressEvent;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use mermaid_domain::{ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata};
use mermaid_model::constants::MAX_RESPONSE_CHARS as MAX_FILE_READ_BYTES;

use super::super::ctx::ExecContext;
use super::ToolExecutor;
use super::path_safety::{
    AllowedRoots, PathContainment, ResolvedInRoot, relative_within, resolve_in_roots,
    resolve_path_within,
};

/// Small helper for building a `ToolDefinition` with a typical
/// JSON-schema-shaped `input_schema`. Keeps the per-tool definitions
/// readable.
fn defn(name: &str, description: &str, input_schema: serde_json::Value) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
    }
}

/// Aggregate cap for a multi-file `read_file` result (#F45). Each file is
/// individually bounded at `MAX_RESPONSE_CHARS` by `read_one`, but a batch of up
/// to `MAX_BATCH_TOOL_ITEMS` files could otherwise sum to ~12.8 MB in a single
/// tool result — far past any sane model-context budget. This bounds the
/// combined total; single-file reads are already bounded and unaffected.
const MAX_READ_AGGREGATE_CHARS: usize = mermaid_model::constants::MAX_RESPONSE_CHARS;

/// Entries kept for same-turn duplicate-read suppression, across all live
/// scopes. Hashes only — never content — so the bound is about map hygiene,
/// not memory pressure.
const READ_DEDUP_CAP: usize = 128;

/// One remembered read: which context read which path, in which turn, and
/// the content hash that proves the repeat is byte-identical.
struct ReadDedupEntry {
    scope: String,
    path: String,
    turn: u64,
    hash: [u8; 32],
    line_count: usize,
}

/// Same-turn duplicate reads, process-global (mirrors `web.rs`'s snapshot
/// store). The `20260806` field logs show the same file read up to 14 times
/// per session at full length — sometimes twice within one turn, where the
/// earlier result is by construction still in the model's request. Only
/// that provably-safe window is deduped: across turns a re-read may be a
/// legitimate refresh (post-edit, post-compaction). Equality is proven by
/// content hash — not mtime, which lies on some drives — so a change made
/// by ANY path (`write_file`, `apply_patch`, `execute_command`, the user's
/// editor) yields full content again with no invalidation hooks to forget.
static READ_DEDUP: std::sync::OnceLock<
    std::sync::Mutex<std::collections::VecDeque<ReadDedupEntry>>,
> = std::sync::OnceLock::new();

/// The identity a dedup entry belongs to. Session/task ids separate
/// concurrent contexts (a subagent's context is not the parent's — each
/// must receive its own full read); the workdir separates anonymous test
/// harness contexts, which reuse small turn ids.
fn read_dedup_scope(ctx: &ExecContext) -> String {
    format!(
        "{}|{}|{}",
        ctx.session_id.as_deref().unwrap_or(""),
        ctx.task_id.as_deref().unwrap_or(""),
        ctx.workdir.display(),
    )
}

/// Returns the short reuse note when `content` is byte-identical to what an
/// earlier read of `path` already returned THIS turn; otherwise records the
/// read and returns `None` (full content flows). The note names the line
/// count and the recovery paths so the model is taught, not stonewalled.
fn duplicate_read_note(ctx: &ExecContext, path: &str, content: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    let hash: [u8; 32] = Sha256::digest(content.as_bytes()).into();
    let line_count = content.lines().count();
    let scope = read_dedup_scope(ctx);
    let mut store = READ_DEDUP
        .get_or_init(|| std::sync::Mutex::new(std::collections::VecDeque::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = store
        .iter_mut()
        .find(|e| e.scope == scope && e.path == path)
    {
        let identical_this_turn = entry.turn == ctx.turn.0 && entry.hash == hash;
        entry.turn = ctx.turn.0;
        entry.hash = hash;
        entry.line_count = line_count;
        return identical_this_turn.then(|| {
            format!(
                "{path}: unchanged since your read earlier this turn — the full \
                 content ({line_count} lines) is already in this turn's tool \
                 results; reuse it. A read after the file changes, or in a later \
                 turn, returns the full content again."
            )
        });
    }
    store.push_back(ReadDedupEntry {
        scope,
        path: path.to_string(),
        turn: ctx.turn.0,
        hash,
        line_count,
    });
    if store.len() > READ_DEDUP_CAP {
        store.pop_front();
    }
    None
}

/// `read_file` — read one or more files and return their contents
/// joined with section markers.
pub struct ReadFileTool;

#[async_trait]
impl ToolExecutor for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn schema(&self) -> ToolDefinition {
        defn(
            "read_file",
            "Read the contents of one or more files from disk. Relative paths resolve relative to the project directory; absolute paths may resolve anywhere on disk.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File to read (single)." },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Multiple files to read sequentially, in order."
                    }
                },
                "oneOf": [
                    { "required": ["path"] },
                    { "required": ["paths"] }
                ]
            }),
        )
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let paths = match extract_paths(&args) {
            Ok(p) => p,
            Err(e) => return ToolOutcome::error(e, 0.0),
        };
        if paths.is_empty() {
            return ToolOutcome::error("read_file requires at least one path", 0.0);
        }

        let start = std::time::Instant::now();
        let roots = AllowedRoots::new(&ctx.workdir, ctx.scratchpad.as_deref());
        let mut combined = String::new();
        let mut any_truncated = false;

        for (idx, raw_path) in paths.iter().enumerate() {
            let target = match resolve_read_target(&roots, raw_path) {
                Ok(target) => target,
                Err(e) => {
                    return ToolOutcome::error(
                        format!("{raw_path}: {e}"),
                        start.elapsed().as_secs_f64(),
                    );
                },
            };
            // A path outside every allowed root is an external side effect:
            // it goes through the policy gate before a byte is read, exactly
            // as an external write does.
            if let Some(abs) = &target.external
                && let Some(blocked) = external_read_gate(&ctx, raw_path, abs).await
            {
                return blocked;
            }
            // Race the file read against the turn's cancel token. If
            // the user Ctrl+C's mid-read, we bail immediately.
            tokio::select! {
                biased;
                _ = ctx.token.cancelled() => {
                    return ToolOutcome::cancelled();
                },
                read = read_one(target.root, target.rel) => {
                    match read {
                        Ok((content, was_truncated)) => {
                            // A byte-identical repeat of a read this same turn
                            // collapses to a short reuse note — the earlier
                            // result is still in the model's request. The note
                            // is not a truncation: nothing was cut that isn't
                            // already present in full.
                            let (content, was_truncated) =
                                duplicate_read_note(&ctx, raw_path, &content)
                                    .map_or((content, was_truncated), |note| (note, false));
                            any_truncated |= was_truncated;
                            if paths.len() > 1 {
                                let _ = ctx.progress.send(ProgressEvent::Status(
                                    format!("read {}/{}: {}", idx + 1, paths.len(), raw_path),
                                )).await;
                                combined.push_str(&format!(
                                    "=== {raw_path} ===\n{content}\n\n"
                                ));
                            } else {
                                combined = content;
                            }
                        },
                        Err(e) => {
                            return ToolOutcome::error(
                                format!("{raw_path}: {e}"),
                                start.elapsed().as_secs_f64(),
                            );
                        },
                    }
                },
            }
        }

        // F45: bound the COMBINED multi-file result. Each file is already capped
        // at MAX_RESPONSE_CHARS by read_one, but a batch of files can still sum to
        // ~12.8 MB in one tool result — past any sane context budget. Only the
        // multi-file accumulation needs this (single-file output is already
        // bounded); truncate_middle keeps the head AND tail with an elision marker.
        if paths.len() > 1 && combined.len() > MAX_READ_AGGREGATE_CHARS {
            combined = mermaid_model::utils::truncate_middle(&combined, MAX_READ_AGGREGATE_CHARS);
            any_truncated = true;
        }

        let duration_secs = start.elapsed().as_secs_f64();
        let line_count = combined.lines().count();
        let byte_count = combined.len();
        // The REAL truncation flag from the bounded read — not a sniff for the
        // marker string, which a file containing that literal text would
        // falsely trip (#78).
        let truncated = any_truncated;
        ToolOutcome::success(
            combined,
            format!(
                "{} {} read",
                line_count,
                plural(line_count, "line", "lines")
            ),
            duration_secs,
        )
        .with_metadata(ToolRunMetadata {
            detail: ToolMetadata::ReadFile {
                paths,
                line_count,
                byte_count,
                truncated,
            },
            line_count: Some(line_count),
            byte_count: Some(byte_count),
            ..ToolRunMetadata::default()
        })
    }
}

/// `delete_file` — unlink a file. Errors on directories (use
/// `execute_command rm -rf` for those — the model shouldn't be
/// blowing away directories as a routine op).
pub struct DeleteFileTool;

#[async_trait]
impl ToolExecutor for DeleteFileTool {
    fn name(&self) -> &'static str {
        "delete_file"
    }

    fn schema(&self) -> ToolDefinition {
        defn(
            "delete_file",
            "Remove a file from disk. Paths may be relative to the project directory or absolute paths on disk. Fails on directories — use `execute_command rm -rf` for those.",
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        )
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let Some(raw_path) = args.get("path").and_then(|v| v.as_str()) else {
            return err("delete_file requires 'path'", 0.0);
        };
        let start = std::time::Instant::now();
        let roots = AllowedRoots::new(&ctx.workdir, ctx.scratchpad.as_deref());
        let ResolvedInRoot {
            abs,
            rel,
            root,
            containment,
        } = match resolve_in_roots(&roots, raw_path) {
            Ok(r) => r,
            Err(e) => return err(&format!("delete_file: {e}"), 0.0),
        };
        let pending_action = serde_json::json!({
            "tool": "delete_file",
            "args": { "path": raw_path },
            "workdir": ctx.workdir.display().to_string(),
            "turn_id": ctx.turn.0,
            "call_id": ctx.call_id.0,
            "task_id": ctx.task_id.clone(),
        });
        if let MutationGate::Blocked(outcome) = mutation_policy_outcome(
            &ctx,
            "delete_file",
            raw_path,
            std::slice::from_ref(&abs),
            pending_action,
            containment,
        )
        .await
        {
            return *outcome;
        }
        // Serialize writers to this canonical path: sibling tool calls in the same
        // turn run concurrently, so without this the checkpoint + delete could race
        // another writer to the same file. Distinct paths still overlap. Raced
        // against cancellation so a contended lock stays Ctrl+C-responsive.
        let _write_guard = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            g = super::path_lock::lock_path(&abs) => g,
        };
        // Scratchpad files are session-private and ephemeral — never
        // checkpointed into the project's restore history.
        if ctx.config.safety.checkpoint_on_mutation
            && containment != PathContainment::Scratchpad
            && let Err(e) = mermaid_runtime::create_checkpoint_for_task(
                &ctx.workdir,
                std::slice::from_ref(&abs),
                Some(serde_json::json!({
                    "tool": "delete_file",
                    "path": raw_path,
                })),
                ctx.checkpoint_origin(),
            )
        {
            return err(&format!("delete_file checkpoint failed: {e}"), 0.0);
        }
        let display = raw_path.to_string();

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::cancelled(),
            result = tokio::task::spawn_blocking(move || mermaid_runtime::remove_file_beneath(&root, &rel)) => {
                match result {
                    Ok(Ok(())) => {
                        let duration_secs = start.elapsed().as_secs_f64();
                        after_file_mutation(&ctx, "delete_file", &display);
                        ToolOutcome::success(
                            format!("Deleted {display}"),
                            "file deleted",
                            duration_secs,
                        )
                        .with_metadata(ToolRunMetadata {
                            detail: ToolMetadata::DeleteFile { path: display },
                            ..ToolRunMetadata::default()
                        })
                    },
                    Ok(Err(e)) => err(&format!("delete_file({display}): {e}"),
                                       start.elapsed().as_secs_f64()),
                    Err(e) => err(&format!("delete_file join error: {e}"),
                                   start.elapsed().as_secs_f64()),
                }
            }
        }
    }
}

/// `create_directory` — `mkdir -p` semantics.
pub struct CreateDirectoryTool;

#[async_trait]
impl ToolExecutor for CreateDirectoryTool {
    fn name(&self) -> &'static str {
        "create_directory"
    }

    fn schema(&self) -> ToolDefinition {
        defn(
            "create_directory",
            "Create a directory (and any missing parents) at the given path. Paths may be relative to the project directory or absolute paths on disk.",
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        )
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let Some(raw_path) = args.get("path").and_then(|v| v.as_str()) else {
            return err("create_directory requires 'path'", 0.0);
        };
        let start = std::time::Instant::now();
        let roots = AllowedRoots::new(&ctx.workdir, ctx.scratchpad.as_deref());
        let ResolvedInRoot {
            abs,
            rel,
            root,
            containment,
        } = match resolve_in_roots(&roots, raw_path) {
            Ok(r) => r,
            Err(e) => return err(&format!("create_directory: {e}"), 0.0),
        };
        let pending_action = serde_json::json!({
            "tool": "create_directory",
            "args": { "path": raw_path },
            "workdir": ctx.workdir.display().to_string(),
            "turn_id": ctx.turn.0,
            "call_id": ctx.call_id.0,
            "task_id": ctx.task_id.clone(),
        });
        if let MutationGate::Blocked(outcome) = mutation_policy_outcome(
            &ctx,
            "create_directory",
            raw_path,
            std::slice::from_ref(&abs),
            pending_action,
            containment,
        )
        .await
        {
            return *outcome;
        }
        // Serialize writers to this canonical path (see delete_file). mkdir -p is
        // idempotent, but a uniform gate keeps ordering consistent and cheap.
        let _write_guard = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            g = super::path_lock::lock_path(&abs) => g,
        };
        // Scratchpad dirs are session-private and ephemeral — never checkpointed.
        if ctx.config.safety.checkpoint_on_mutation
            && containment != PathContainment::Scratchpad
            && let Err(e) = mermaid_runtime::create_checkpoint_for_task(
                &ctx.workdir,
                std::slice::from_ref(&abs),
                Some(serde_json::json!({
                    "tool": "create_directory",
                    "path": raw_path,
                })),
                ctx.checkpoint_origin(),
            )
        {
            return err(&format!("create_directory checkpoint failed: {e}"), 0.0);
        }
        let display = raw_path.to_string();

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::cancelled(),
            result = tokio::task::spawn_blocking(move || mermaid_runtime::create_dir_all_beneath(&root, &rel)) => {
                match result {
                    Ok(Ok(())) => {
                        let duration_secs = start.elapsed().as_secs_f64();
                        after_file_mutation(&ctx, "create_directory", &display);
                        ToolOutcome::success(
                            format!("Created directory {display}"),
                            "directory created",
                            duration_secs,
                        )
                        .with_metadata(ToolRunMetadata {
                            detail: ToolMetadata::CreateDirectory { path: display },
                            ..ToolRunMetadata::default()
                        })
                    },
                    Ok(Err(e)) => err(&format!("create_directory({display}): {e}"),
                                       start.elapsed().as_secs_f64()),
                    Err(e) => err(&format!("create_directory join error: {e}"),
                                   start.elapsed().as_secs_f64()),
                }
            }
        }
    }
}

/// `write_file` — write a single file, creating parent dirs as needed.
pub struct WriteFileTool;

#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
#[async_trait]
impl ToolExecutor for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn schema(&self) -> ToolDefinition {
        defn(
            "write_file",
            "Write (overwrite) a file at `path` with `content`. Creates parent directories automatically. Paths may be relative to the project directory or absolute. Prefer `apply_patch` for small targeted changes.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        )
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
            return ToolOutcome::error("write_file requires 'path' (string)", 0.0);
        };
        let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
            return ToolOutcome::error("write_file requires 'content' (string)", 0.0);
        };

        let start = std::time::Instant::now();
        let roots = AllowedRoots::new(&ctx.workdir, ctx.scratchpad.as_deref());
        // `rel` is the root-relative name for the confined fd write (the actual
        // byte path).
        let ResolvedInRoot {
            abs: abs_path,
            rel,
            root,
            containment,
        } = match resolve_in_roots(&roots, path) {
            Ok(r) => r,
            Err(e) => return ToolOutcome::error(format!("write_file: {e}"), 0.0),
        };
        let pending_action = serde_json::json!({
            "tool": "write_file",
            "args": { "path": path, "content": content },
            "workdir": ctx.workdir.display().to_string(),
            "turn_id": ctx.turn.0,
            "call_id": ctx.call_id.0,
            "task_id": ctx.task_id.clone(),
        });
        let plan_write = match mutation_policy_outcome(
            &ctx,
            "write_file",
            path,
            std::slice::from_ref(&abs_path),
            pending_action,
            containment,
        )
        .await
        {
            MutationGate::Blocked(outcome) => return *outcome,
            MutationGate::Proceed { plan_write } => plan_write,
        };
        // Serialize writers to this canonical path: two write_file/edit calls to
        // the same file in one turn run concurrently, so without this the last
        // atomic rename silently wins (lost update). Distinct paths still overlap.
        // The owned guard is Send and held across the spawn_blocking below.
        let _write_guard = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            g = super::path_lock::lock_path(&abs_path) => g,
        };
        // Scratchpad files are session-private and ephemeral — never checkpointed.
        if ctx.config.safety.checkpoint_on_mutation
            && containment != PathContainment::Scratchpad
            && let Err(e) = mermaid_runtime::create_checkpoint_for_task(
                &ctx.workdir,
                std::slice::from_ref(&abs_path),
                Some(serde_json::json!({
                    "tool": "write_file",
                    "path": path,
                })),
                ctx.checkpoint_origin(),
            )
        {
            return ToolOutcome::error(format!("write_file checkpoint failed: {e}"), 0.0);
        }
        let display_path = path.to_string();
        let line_count = content.lines().count();
        let byte_count = content.len();
        let content = content.to_string();

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::cancelled(),
            // The prior-content read (for the display diff) now happens INSIDE
            // this blocking job and BOUNDED (#F44/RC-L) — never a synchronous
            // unbounded `read_to_string` on the async worker thread.
            result = tokio::task::spawn_blocking(move || write_with_diff_blocking(&root, &abs_path, &rel, &content)) => {
                match result {
                    Ok(Ok(write)) => {
                        let duration_secs = start.elapsed().as_secs_f64();
                        after_file_mutation(&ctx, "write_file", &display_path);
                        ToolOutcome::success(
                            format!("Wrote {} ({} lines)", display_path, write.line_count),
                            format!("{} {} written", write.line_count, plural(write.line_count, "line", "lines")),
                            duration_secs,
                        )
                        .with_metadata(ToolRunMetadata {
                            detail: ToolMetadata::WriteFile {
                                path: display_path,
                                line_count,
                                byte_count,
                                created: Some(write.created),
                            },
                            line_count: Some(line_count),
                            byte_count: Some(byte_count),
                            display_diff: Some(write.diff.display_diff),
                            diff_truncated: write.diff.truncated,
                            lines_added: write.diff.added,
                            lines_removed: write.diff.removed,
                            plan_file_written: plan_write,
                            ..ToolRunMetadata::default()
                        })
                    },
                    Ok(Err(e)) => ToolOutcome::error(
                        format!("write_file({display_path}): {e}"),
                        start.elapsed().as_secs_f64(),
                    ),
                    Err(e) => ToolOutcome::error(
                        format!("write_file join error: {e}"),
                        start.elapsed().as_secs_f64(),
                    ),
                }
            }
        }
    }
}

/// `edit_file` — precise search-and-replace editing on a single file.
pub struct EditFileTool;

#[async_trait]
impl ToolExecutor for EditFileTool {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn schema(&self) -> ToolDefinition {
        defn(
            "edit_file",
            "Perform a precise search-and-replace edit on an existing file. Replaces `target_content` with `replacement_content`. Fails if `target_content` is not found or matches multiple locations (unless `allow_multiple` is true). Matching tolerates minor whitespace and quotation drift. Paths may be relative to the project directory or absolute.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to edit."
                    },
                    "target_content": {
                        "type": "string",
                        "description": "The exact or near-exact block of text to replace. Must match uniquely in the file unless allow_multiple is true."
                    },
                    "replacement_content": {
                        "type": "string",
                        "description": "The new replacement text."
                    },
                    "allow_multiple": {
                        "type": "boolean",
                        "description": "If true, replace all occurrences of target_content instead of requiring uniqueness. Defaults to false."
                    }
                },
                "required": ["path", "target_content", "replacement_content"]
            }),
        )
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let start = std::time::Instant::now();
        let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
            return ToolOutcome::error("edit_file requires 'path' (string)", 0.0);
        };
        let Some(target) = args.get("target_content").and_then(|v| v.as_str()) else {
            return ToolOutcome::error("edit_file requires 'target_content' (string)", 0.0);
        };
        let Some(replacement) = args.get("replacement_content").and_then(|v| v.as_str()) else {
            return ToolOutcome::error("edit_file requires 'replacement_content' (string)", 0.0);
        };
        let allow_multiple = args
            .get("allow_multiple")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let roots = AllowedRoots::new(&ctx.workdir, ctx.scratchpad.as_deref());
        let ResolvedInRoot {
            abs: abs_path,
            rel,
            root,
            containment,
        } = match resolve_in_roots(&roots, path) {
            Ok(r) => r,
            Err(e) => return ToolOutcome::error(format!("edit_file: {e}"), 0.0),
        };

        let pending_action = serde_json::json!({
            "tool": "edit_file",
            "args": {
                "path": path,
                "target_content": target,
                "replacement_content": replacement,
                "allow_multiple": allow_multiple,
            },
            "workdir": ctx.workdir.display().to_string(),
            "turn_id": ctx.turn.0,
            "call_id": ctx.call_id.0,
            "task_id": ctx.task_id.clone(),
        });
        let plan_write = match mutation_policy_outcome(
            &ctx,
            "edit_file",
            path,
            std::slice::from_ref(&abs_path),
            pending_action,
            containment,
        )
        .await
        {
            MutationGate::Blocked(outcome) => return *outcome,
            MutationGate::Proceed { plan_write } => plan_write,
        };

        let _write_guard = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            g = super::path_lock::lock_path(&abs_path) => g,
        };

        if ctx.config.safety.checkpoint_on_mutation
            && containment != PathContainment::Scratchpad
            && let Err(e) = mermaid_runtime::create_checkpoint_for_task(
                &ctx.workdir,
                std::slice::from_ref(&abs_path),
                Some(serde_json::json!({
                    "tool": "edit_file",
                    "path": path,
                })),
                ctx.checkpoint_origin(),
            )
        {
            return ToolOutcome::error(format!("edit_file checkpoint failed: {e}"), 0.0);
        }

        let display_path = path.to_string();
        let target = target.to_string();
        let replacement = replacement.to_string();

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::cancelled(),
            result = tokio::task::spawn_blocking(move || edit_file_blocking(&root, &rel, &target, &replacement, allow_multiple)) => {
                match result {
                    Ok(Ok(edit)) => {
                        let duration_secs = start.elapsed().as_secs_f64();
                        after_file_mutation(&ctx, "edit_file", &display_path);
                        edit_success_outcome(&display_path, edit, plan_write, duration_secs)
                    },
                    Ok(Err(e)) => ToolOutcome::error(
                        format!("edit_file({display_path}): {e}"),
                        start.elapsed().as_secs_f64(),
                    ),
                    Err(e) => ToolOutcome::error(
                        format!("edit_file join error: {e}"),
                        start.elapsed().as_secs_f64(),
                    ),
                }
            }
        }
    }
}

fn edit_success_outcome(
    display_path: &str,
    edit: EditResult,
    plan_write: bool,
    duration_secs: f64,
) -> ToolOutcome {
    let fuzzy_note = if edit.fuzzy {
        "\nnote: matched with fuzzy (whitespace/Unicode) context; verify the result."
    } else {
        ""
    };
    ToolOutcome::success(
        format!("Edited {display_path}{fuzzy_note}"),
        diff_summary(edit.diff.added, edit.diff.removed, duration_secs),
        duration_secs,
    )
    .with_metadata(ToolRunMetadata {
        detail: ToolMetadata::ApplyPatch {
            added: Vec::new(),
            modified: vec![display_path.to_string()],
            deleted: Vec::new(),
            renamed: Vec::new(),
            fuzzy: edit.fuzzy,
        },
        display_diff: Some(edit.diff.display_diff),
        diff_truncated: edit.diff.truncated,
        lines_added: edit.diff.added,
        lines_removed: edit.diff.removed,
        plan_file_written: plan_write,
        ..ToolRunMetadata::default()
    })
}

// ─── helpers ────────────────────────────────────────────────────────

fn extract_paths(args: &serde_json::Value) -> Result<Vec<String>, String> {
    // Accept both shapes: `{path: "x"}` and `{paths: ["x", "y"]}`.
    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
        reject_web_url(p)?;
        return Ok(vec![p.to_string()]);
    }
    if let Some(arr) = args.get("paths").and_then(|v| v.as_array()) {
        if arr.len() > mermaid_model::constants::MAX_BATCH_TOOL_ITEMS {
            return Err(format!(
                "read_file: too many paths ({}); cap is {} per call — split the request",
                arr.len(),
                mermaid_model::constants::MAX_BATCH_TOOL_ITEMS
            ));
        }
        let mut out = Vec::with_capacity(arr.len());
        for v in arr {
            let Some(s) = v.as_str() else {
                return Err("read_file 'paths' must be an array of strings".to_string());
            };
            reject_web_url(s)?;
            out.push(s.to_string());
        }
        return Ok(out);
    }
    Err("read_file requires 'path' or 'paths'".to_string())
}

/// `read_file` reads the local filesystem, but models under a web-gated
/// safety mode were observed pointing it at `https://` URLs and treating the
/// result as a fetch — and the path-resolution error that came back said
/// nothing about the actual mistake. Name the mistake and the right tool;
/// whether `web_fetch` is available is then that tool's own story to tell.
fn reject_web_url(path: &str) -> Result<(), String> {
    let head: String = path
        .trim_start()
        .chars()
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    if head.starts_with("http://") || head.starts_with("https://") {
        return Err(format!(
            "read_file reads local files; '{path}' is a web URL — use web_fetch for URLs"
        ));
    }
    Ok(())
}

/// Read-only carve-out for memory facts. Global and project-private memory
/// live under the OS data dir — outside both allowed roots — and the memory
/// index tells the model to `read_file` the fact's path, so reads resolve
/// against the memory roots too. Absolute paths only, with the same
/// canonical (symlink-resolving) containment as the scratchpad arm. The
/// write tools never consult this: memory mutation goes through the `memory`
/// tool, where the policy gate can see it.
fn resolve_in_memory_roots(workdir: &Path, raw: &str) -> Option<(PathBuf, PathBuf)> {
    if !Path::new(raw).is_absolute() {
        return None;
    }
    for (root, _scope) in crate::app::memory::memory_roots(workdir) {
        if let Ok((_abs, true)) = resolve_path_within(&root, raw)
            && let Ok(rel) = relative_within(&root, raw)
        {
            return Some((root, rel));
        }
    }
    None
}

/// Where a `read_file` target lands once the resolver and the memory-root
/// fallback have spoken.
struct ReadTarget {
    root: PathBuf,
    rel: PathBuf,
    /// `Some(abs)` when the target sits outside every allowed root and must
    /// clear [`external_read_gate`] before it is opened.
    external: Option<PathBuf>,
}

/// Resolve a read target through the canonical containment resolver, and
/// answer its three-way verdict: project and scratchpad reads are ungated;
/// durable-memory reads are ungated too (memory is agent-owned by design and
/// lives outside the project); anything else outside the roots is external and
/// carries its absolute path for the gate.
///
/// `Err` only when the path cannot be resolved at all (a workdir that will not
/// canonicalize, or no existing ancestor) and is not a memory path either.
fn resolve_read_target(roots: &AllowedRoots<'_>, raw: &str) -> std::io::Result<ReadTarget> {
    match resolve_in_roots(roots, raw) {
        Ok(ResolvedInRoot {
            abs,
            rel,
            root,
            containment,
        }) => match containment {
            PathContainment::Project | PathContainment::Scratchpad => Ok(ReadTarget {
                root,
                rel,
                external: None,
            }),
            PathContainment::External => Ok(resolve_in_memory_roots(roots.workdir, raw).map_or(
                ReadTarget {
                    root,
                    rel,
                    external: Some(abs),
                },
                |(root, rel)| ReadTarget {
                    root,
                    rel,
                    external: None,
                },
            )),
        },
        Err(msg) => {
            let (root, rel) = resolve_in_memory_roots(roots.workdir, raw)
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::PermissionDenied, msg))?;
            Ok(ReadTarget {
                root,
                rel,
                external: None,
            })
        },
    }
}

/// Gate a read of `abs`, a path outside the project and the scratchpad.
///
/// Filed as `ToolCategory::ExternalDirectory` — an external side effect, the
/// same class an out-of-project `working_dir` gets from the exec tool — so
/// `ask` prompts (allowlistable per directory), `auto` consults the intent
/// classifier, `full_access` proceeds, and the read-only floor (read-only and
/// plan modes) denies. Not replayable: a read has nothing to replay, so a
/// headless `ask` session refuses it unless untrusted tools were allowed.
///
/// Returns the blocking outcome, or `None` to proceed.
async fn external_read_gate(ctx: &ExecContext, raw: &str, abs: &Path) -> Option<ToolOutcome> {
    let mut request = mermaid_runtime::ActionRequest::new(
        "read_file",
        mermaid_runtime::ToolCategory::ExternalDirectory,
        format!("read_file {raw}"),
    );
    request.path = Some(abs.display().to_string());
    let pending_action = serde_json::json!({
        "tool": "read_file",
        "args": { "path": raw },
        "workdir": ctx.workdir.display().to_string(),
        "turn_id": ctx.turn.0,
        "call_id": ctx.call_id.0,
        "task_id": ctx.task_id.clone(),
    });
    match super::policy_gate::gate(ctx, request, &[], pending_action, false, false).await {
        super::policy_gate::Gate::Block(outcome) => Some(outcome),
        super::policy_gate::Gate::Proceed { .. } => None,
    }
}

/// Read one file (bounded) from `rel` beneath `root`. Returns the (possibly
/// marker-footed) text and the REAL truncation flag from the bounded read, so
/// the caller propagates that rather than sniffing the output for the marker
/// string — which a file whose own content contains that literal text would
/// otherwise falsely trip (#78).
///
/// The root-relative path feeds the confined fd read, so the bytes come from
/// the inode the kernel resolved under `RESOLVE_BENEATH` rather than whatever
/// a concurrently-swapped symlink now points at (#77).
async fn read_one(root: PathBuf, rel: PathBuf) -> std::io::Result<(String, bool)> {
    let result = tokio::task::spawn_blocking(move || {
        let file = mermaid_runtime::open_beneath(&root, &rel, mermaid_runtime::OpenIntent::Read)?;
        // Bounded read: never pull more than the cap (+1 probe byte) into RAM,
        // so a model pointing `read_file` at a multi-gigabyte file can't OOM the
        // process — a full read would have slurped the whole thing first (#15).
        let (data, truncated) = mermaid_model::utils::read_capped(file, MAX_FILE_READ_BYTES)?;
        let mut s = String::from_utf8_lossy(&data).into_owned();
        if truncated {
            // Char-boundary-safe truncation with a marker footer.
            let cut = s.floor_char_boundary(MAX_FILE_READ_BYTES);
            s.truncate(cut);
            s.push_str("\n\n[TRUNCATED: file exceeded read cap]");
        }
        Ok::<_, std::io::Error>((s, truncated))
    })
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))??;
    Ok(result)
}

/// Write `content` to `rel` beneath `root` (the project workdir or the session
/// scratchpad) through the symlink-confined *atomic* writer, creating parent
/// dirs the same confined way. The bytes are written to a temp and
/// `renameat`-swapped over the target, all beneath the directory fd the kernel
/// resolved under `RESOLVE_BENEATH`: a parent dir swapped for an escaping
/// symlink can't redirect the write (#77), and a crash/kill/disk-full
/// mid-write leaves the previous file intact rather than a truncated or
/// half-written one.
fn write_one_blocking(root: &Path, rel: &Path, content: &str) -> std::io::Result<usize> {
    if let Some(parent) = rel.parent()
        && !parent.as_os_str().is_empty()
    {
        mermaid_runtime::create_dir_all_beneath(root, parent)?;
    }
    mermaid_runtime::write_atomic_beneath(root, rel, content.as_bytes())?;
    Ok(content.lines().count())
}

struct WriteResult {
    line_count: usize,
    created: bool,
    diff: mermaid_model::diff::DisplayDiff,
}

/// Write `content` and build the display diff against the prior file in ONE
/// blocking job (#F44/RC-L). The prior content is read BOUNDED via
/// [`mermaid_model::utils::read_file_capped`] — overwriting a multi-gigabyte file must
/// not slurp it into RAM on the async worker just to render a diff. A prior file
/// larger than the read cap (or otherwise unreadable) is elided from the diff
/// rather than read whole.
fn write_with_diff_blocking(
    root: &Path,
    abs_path: &Path,
    rel: &Path,
    content: &str,
) -> std::io::Result<WriteResult> {
    let (old_content, created, elide_diff) =
        match mermaid_model::utils::read_file_capped(abs_path, MAX_FILE_READ_BYTES) {
            Ok((data, false)) => (String::from_utf8_lossy(&data).into_owned(), false, false),
            // Existing file is past the read cap — don't pull it all into RAM.
            Ok((_, true)) => (String::new(), false, true),
            // Missing file → a fresh create; the diff shows the whole content added.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), true, false),
            // Any other read error: don't fail the write over a diff preview.
            Err(_) => (String::new(), false, true),
        };
    let diff = if elide_diff {
        mermaid_model::diff::DisplayDiff {
            display_diff: format!(
                "[diff preview skipped: existing file exceeds the {MAX_FILE_READ_BYTES}-byte cap]"
            ),
            added: 0,
            removed: 0,
            truncated: true,
        }
    } else {
        mermaid_model::diff::generate_display_diff(&old_content, content)
    };
    let line_count = write_one_blocking(root, rel, content)?;
    Ok(WriteResult {
        line_count,
        created,
        diff,
    })
}

struct EditResult {
    diff: mermaid_model::diff::DisplayDiff,
    fuzzy: bool,
}

fn edit_file_blocking(
    root: &Path,
    rel: &Path,
    target: &str,
    replacement: &str,
    allow_multiple: bool,
) -> Result<EditResult, String> {
    let file = mermaid_runtime::open_beneath(root, rel, mermaid_runtime::OpenIntent::Read)
        .map_err(|e| format!("cannot open file: {e}"))?;
    let (data, truncated) = mermaid_model::utils::read_capped(file, MAX_FILE_READ_BYTES)
        .map_err(|e| format!("cannot read file: {e}"))?;
    if truncated {
        return Err(format!(
            "file exceeds maximum size limit ({MAX_FILE_READ_BYTES} bytes)"
        ));
    }
    let original = String::from_utf8_lossy(&data).into_owned();
    let applied = mermaid_runtime::replace_content(&original, target, replacement, allow_multiple)
        .map_err(|e| e.to_string())?;

    write_one_blocking(root, rel, &applied.new_contents)
        .map_err(|e| format!("cannot write file: {e}"))?;

    let diff = mermaid_model::diff::generate_display_diff(&original, &applied.new_contents);
    Ok(EditResult {
        diff,
        fuzzy: applied.fuzzy,
    })
}

/// Outcome of gating a file mutation. `Proceed` carries `plan_write`: whether
/// the allowance came from plan mode's plan-file carve-out, which the caller
/// stamps onto `ToolRunMetadata::plan_file_written`.
///
/// The tool NAME is not a usable stand-in for this. "Under the plan floor the
/// only Edit that can succeed is the plan file" stops being true as soon as
/// `[plan] memory = allow` lets a `write_file` to a memory path succeed.
pub(super) enum MutationGate {
    /// Blocked — return this outcome verbatim. Boxed to keep the enum small.
    Blocked(Box<ToolOutcome>),
    Proceed {
        plan_write: bool,
    },
}

/// Gate a file mutation by where its target landed.
///
/// `containment` is the resolver's verdict for the path, matched here so that
/// every mutating tool answers the three-way question in one place:
/// - `Project`: an ordinary edit (`ToolCategory::Edit`).
/// - `Scratchpad`: the gate downgrades an `Ask`/`Classify` to proceed — scratch
///   files are session-private and ephemeral — while read-only mode and `Deny`
///   overrides still block it.
/// - `External`: a path outside both roots is an external side effect
///   (`ToolCategory::ExternalDirectory`), so it prompts in `ask`, goes through
///   the intent classifier in `auto`, and is denied by the read-only floor —
///   the same treatment an out-of-project `working_dir` gets from the exec
///   tool. Before this, an external write classified like an in-project one
///   and `auto` mode allowed it unasked.
pub(super) async fn mutation_policy_outcome(
    ctx: &ExecContext,
    tool: &str,
    path: &str,
    checkpoint_paths: &[PathBuf],
    pending_action: serde_json::Value,
    containment: PathContainment,
) -> MutationGate {
    let category = match containment {
        PathContainment::Project | PathContainment::Scratchpad => {
            mermaid_runtime::ToolCategory::Edit
        },
        PathContainment::External => mermaid_runtime::ToolCategory::ExternalDirectory,
    };
    let mut request = mermaid_runtime::ActionRequest::new(tool, category, format!("{tool} {path}"));
    request.path = Some(path.to_string());
    // File mutations are replayable: an Ask decision checkpoints, records an
    // approval, and blocks (handled inside the gate).
    match super::policy_gate::gate(
        ctx,
        request,
        checkpoint_paths,
        pending_action,
        true,
        containment == PathContainment::Scratchpad,
    )
    .await
    {
        super::policy_gate::Gate::Block(outcome) => MutationGate::Blocked(Box::new(outcome)),
        super::policy_gate::Gate::Proceed { plan_write, .. } => {
            let _ = mermaid_runtime::run_plugin_hooks(
                "before_file_mutation",
                &serde_json::json!({
                    "task_id": ctx.task_id.clone(),
                    "turn_id": ctx.turn.0,
                    "call_id": ctx.call_id.0,
                    "tool": tool,
                    "path": path,
                }),
            );
            MutationGate::Proceed { plan_write }
        },
    }
}

pub(super) fn after_file_mutation(ctx: &ExecContext, tool: &str, path: &str) {
    let _ = mermaid_runtime::run_plugin_hooks(
        "after_file_mutation",
        &serde_json::json!({
            "task_id": ctx.task_id.clone(),
            "turn_id": ctx.turn.0,
            "call_id": ctx.call_id.0,
            "tool": tool,
            "path": path,
        }),
    );
}

fn err(msg: &str, duration_secs: f64) -> ToolOutcome {
    ToolOutcome::error(msg, duration_secs)
}

fn plural(count: usize, singular: &'static str, plural: &'static str) -> &'static str {
    if count == 1 { singular } else { plural }
}

pub(super) fn diff_summary(added: usize, removed: usize, duration_secs: f64) -> String {
    format!(
        "+{} -{}, took {}",
        added,
        removed,
        format_duration_for_diff(duration_secs)
    )
}

fn format_duration_for_diff(seconds: f64) -> String {
    if seconds < 1.0 {
        format!("{}ms", (seconds * 1000.0).round().max(1.0) as u64)
    } else if seconds < 10.0 {
        format!("{seconds:.1}s")
    } else {
        format!("{}s", seconds.round() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ctx::test_exec_context;
    use mermaid_domain::{ToolCallId, TurnId};
    use std::fs;

    /// Memory facts live outside the project/scratchpad roots and the index
    /// tells the model to `read_file` them — reads must resolve against the
    /// memory roots (here the `ProjectShared` root, reached by putting the
    /// workdir in a subdir of the git root), while unrelated outside paths
    /// stay rejected.
    #[tokio::test]
    async fn read_file_resolves_memory_roots_read_only() {
        let base = std::env::temp_dir().join(format!("mermaid_memread_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let repo = base.join("repo");
        let workdir = repo.join("src");
        fs::create_dir_all(&workdir).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        let mem_dir = repo.join(".mermaid").join("memory");
        fs::create_dir_all(&mem_dir).unwrap();
        let fact = mem_dir.join("fact.md");
        fs::write(&fact, "the fact body").unwrap();

        let roots = AllowedRoots::new(&workdir, None);
        // A memory path sits outside the project yet resolves ungated: memory
        // is agent-owned by design.
        let target = resolve_read_target(&roots, fact.to_str().unwrap()).unwrap();
        assert!(target.external.is_none(), "memory reads are not external");
        let (content, truncated) = read_one(target.root, target.rel).await.unwrap();
        assert!(!truncated);
        assert_eq!(content, "the fact body");

        // A stray path outside every root resolves too, but carries its
        // absolute path for the gate: `execute` must not read it unasked.
        let stray = base.join("stray.txt");
        fs::write(&stray, "nope").unwrap();
        let target = resolve_read_target(&roots, stray.to_str().unwrap()).unwrap();
        assert_eq!(
            target.external.as_deref().map(|p| p.ends_with("stray.txt")),
            Some(true),
            "a path outside every root is external"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_in_roots_contains_to_workdir() {
        let root = std::env::temp_dir().join(format!("mermaid_rps_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        let roots = AllowedRoots::new(&root, None);

        // In-root existing + not-yet-existing targets resolve inside root.
        assert!(resolve_in_roots(&roots, "sub").is_ok());
        let resolved = resolve_in_roots(&roots, "sub/new.txt").unwrap();
        let canon_root = fs::canonicalize(&root).unwrap();
        assert!(resolved.abs.starts_with(&canon_root));

        // External paths and relative .. escapes resolve successfully.
        assert!(resolve_in_roots(&roots, "../escape.txt").is_ok());
        let outside = std::env::temp_dir().join("definitely_outside.txt");
        let r = resolve_in_roots(&roots, &outside.display().to_string()).unwrap();
        assert!(r.abs.ends_with(Path::new("definitely_outside.txt")));

        let _ = fs::remove_dir_all(&root);
    }

    fn temp_root(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("mermaid_providers_fs_{name}"));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("create tmpdir");
        p
    }

    #[tokio::test]
    async fn read_file_returns_content() {
        let dir = temp_root("read_ok");
        fs::write(dir.join("a.txt"), "hello").expect("write");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());

        let tool = ReadFileTool;
        let outcome = tool
            .execute(serde_json::json!({"path": "a.txt"}), ctx)
            .await;
        assert!(outcome.is_success(), "expected success: {outcome:?}");
        assert_eq!(outcome.output(), "hello");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_rejects_web_urls_with_a_web_fetch_hint() {
        // Observed in the field: a model under a web-gated safety mode fed
        // `read_file` an https:// URL and treated the reply as a fetch. The
        // rejection must name the right tool — and a plain local read next to
        // it must keep working (the guard cannot overmatch).
        let dir = temp_root("read_url");
        fs::write(dir.join("a.txt"), "hello").expect("write");
        for args in [
            serde_json::json!({"path": "https://learn.microsoft.com/clipboard"}),
            serde_json::json!({"path": "HTTP://example.com/x"}),
            serde_json::json!({"paths": ["a.txt", "https://example.com/x"]}),
        ] {
            let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
            let outcome = ReadFileTool.execute(args, ctx).await;
            assert_eq!(outcome.status, mermaid_domain::ToolStatus::Error);
            let msg = outcome.error_message().unwrap_or_default();
            assert!(msg.contains("web_fetch"), "must name the right tool: {msg}");
        }
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = ReadFileTool
            .execute(serde_json::json!({"path": "a.txt"}), ctx)
            .await;
        assert!(outcome.is_success(), "plain local reads must still work");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn duplicate_same_turn_read_collapses_to_a_reuse_note() {
        let dir = temp_root("read_dedup");
        fs::write(dir.join("a.txt"), "line one\nline two").expect("write");

        // First read: full content.
        let (ctx, _rx) = test_exec_context(TurnId(9), ToolCallId(1), dir.clone());
        let outcome = ReadFileTool
            .execute(serde_json::json!({"path": "a.txt"}), ctx)
            .await;
        assert_eq!(outcome.output(), "line one\nline two");

        // Byte-identical repeat in the SAME turn: a short reuse note, not
        // the body again — the earlier result rides the same request.
        let (ctx, _rx) = test_exec_context(TurnId(9), ToolCallId(2), dir.clone());
        let outcome = ReadFileTool
            .execute(serde_json::json!({"path": "a.txt"}), ctx)
            .await;
        assert!(outcome.is_success());
        assert!(
            outcome
                .output()
                .contains("unchanged since your read earlier this turn"),
            "{}",
            outcome.output()
        );
        assert!(outcome.output().contains("2 lines"), "{}", outcome.output());
        assert!(
            !outcome.output().contains("line two"),
            "the body must not repeat: {}",
            outcome.output()
        );

        // Matched pair (a): the file CHANGED on disk — by any writer; here a
        // direct fs write stands in for write_file / apply_patch /
        // execute_command — so the same-turn re-read returns full content.
        // Content-hash equality is the invalidation; there is no hook to
        // forget.
        fs::write(dir.join("a.txt"), "line one\nline two\nline three").expect("write");
        let (ctx, _rx) = test_exec_context(TurnId(9), ToolCallId(3), dir.clone());
        let outcome = ReadFileTool
            .execute(serde_json::json!({"path": "a.txt"}), ctx)
            .await;
        assert_eq!(
            outcome.output(),
            "line one\nline two\nline three",
            "a changed file must read in full"
        );

        // ...and the byte-identical repeat of THAT read collapses again.
        let (ctx, _rx) = test_exec_context(TurnId(9), ToolCallId(4), dir.clone());
        let outcome = ReadFileTool
            .execute(serde_json::json!({"path": "a.txt"}), ctx)
            .await;
        assert!(
            outcome.output().contains("unchanged since"),
            "{}",
            outcome.output()
        );

        // Matched pair (b): a LATER turn always reads in full — a cross-turn
        // re-read may be a legitimate refresh (post-compaction, post-edit)
        // and is never suppressed.
        let (ctx, _rx) = test_exec_context(TurnId(10), ToolCallId(5), dir.clone());
        let outcome = ReadFileTool
            .execute(serde_json::json!({"path": "a.txt"}), ctx)
            .await;
        assert_eq!(outcome.output(), "line one\nline two\nline three");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_missing_path_errors() {
        let dir = temp_root("read_missing_path");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = ReadFileTool.execute(serde_json::json!({}), ctx).await;
        assert_eq!(outcome.status, mermaid_domain::ToolStatus::Error);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_nonexistent_errors() {
        let dir = temp_root("read_nonex");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = ReadFileTool
            .execute(serde_json::json!({"path": "does_not_exist.txt"}), ctx)
            .await;
        assert_eq!(outcome.status, mermaid_domain::ToolStatus::Error);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_with_multiple_paths_joins_contents() {
        let dir = temp_root("read_multi");
        fs::write(dir.join("a.txt"), "alpha").expect("write");
        fs::write(dir.join("b.txt"), "beta").expect("write");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = ReadFileTool
            .execute(serde_json::json!({"paths": ["a.txt", "b.txt"]}), ctx)
            .await;
        assert!(outcome.is_success(), "expected success: {outcome:?}");
        let output = outcome.output();
        assert!(output.contains("=== a.txt ==="));
        assert!(output.contains("alpha"));
        assert!(output.contains("=== b.txt ==="));
        assert!(output.contains("beta"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_multi_aggregate_is_capped() {
        // F45: many files in one call can't blow past the aggregate cap. Each
        // file is under the per-file cap, but their sum exceeds the aggregate.
        let dir = temp_root("read_aggregate_cap");
        let chunk = "a".repeat(MAX_READ_AGGREGATE_CHARS * 2 / 3);
        fs::write(dir.join("a.txt"), &chunk).expect("write a");
        fs::write(dir.join("b.txt"), &chunk).expect("write b");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = ReadFileTool
            .execute(serde_json::json!({"paths": ["a.txt", "b.txt"]}), ctx)
            .await;
        assert!(outcome.is_success(), "expected success: {outcome:?}");
        let output = outcome.output();
        assert!(
            output.len() <= MAX_READ_AGGREGATE_CHARS + 64,
            "combined must be capped, got {} bytes",
            output.len()
        );
        assert!(
            output.contains("elided"),
            "expected aggregate head+tail elision marker"
        );
        match &outcome.metadata.detail {
            ToolMetadata::ReadFile { truncated, .. } => {
                assert!(*truncated, "aggregate truncation must set truncated")
            },
            other => panic!("expected ReadFile metadata, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_file_elides_diff_for_oversized_existing_file() {
        // F44: overwriting a file larger than the read cap must NOT slurp it into
        // RAM for a diff — the diff is elided with a marker instead.
        let dir = temp_root("write_oversized_diff");
        let big = "a".repeat(MAX_FILE_READ_BYTES + 1);
        fs::write(dir.join("big.txt"), &big).expect("write fixture");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = WriteFileTool
            .execute(
                serde_json::json!({"path": "big.txt", "content": "small\n"}),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "expected success: {outcome:?}");
        let diff = outcome
            .metadata
            .display_diff
            .as_deref()
            .expect("display diff");
        assert!(
            diff.contains("diff preview skipped"),
            "expected elision marker, got: {diff}"
        );
        assert!(
            outcome.metadata.diff_truncated,
            "oversized diff must set diff_truncated"
        );
        match &outcome.metadata.detail {
            ToolMetadata::WriteFile { created, .. } => {
                assert_eq!(*created, Some(false), "existing file is not 'created'")
            },
            other => panic!("expected WriteFile metadata, got {other:?}"),
        }
        // The file was actually overwritten despite the elided diff.
        let written = fs::read_to_string(dir.join("big.txt")).expect("read");
        assert_eq!(written, "small\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_with_marker_in_content_is_not_flagged_truncated() {
        // #78: a small file whose own content contains the truncation-marker
        // string must NOT be reported as truncated — the flag comes from the
        // bounded read now, not a substring sniff of the output.
        let dir = temp_root("read_marker_content");
        fs::write(
            dir.join("a.txt"),
            "before\n\n[TRUNCATED: file exceeded read cap]\nafter",
        )
        .expect("write");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());

        let outcome = ReadFileTool
            .execute(serde_json::json!({"path": "a.txt"}), ctx)
            .await;
        assert!(outcome.is_success(), "expected success: {outcome:?}");
        match &outcome.metadata.detail {
            ToolMetadata::ReadFile { truncated, .. } => assert!(
                !truncated,
                "a file whose content contains the marker must not be flagged truncated"
            ),
            other => panic!("expected ReadFile metadata, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_respects_cancellation() {
        let dir = temp_root("read_cancel");
        // Write a huge file so the read is slow enough to race cancel.
        // Actually spawn_blocking on read is fast on tmpfs — this test
        // just verifies the select! arm compiles + the token trips
        // the cancel path when pre-cancelled.
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        ctx.token.cancel();
        let outcome = ReadFileTool
            .execute(serde_json::json!({"path": "x.txt"}), ctx)
            .await;
        assert!(outcome.was_cancelled());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_file_creates_and_counts_lines() {
        let dir = temp_root("write_ok");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = WriteFileTool
            .execute(
                serde_json::json!({"path": "out.txt", "content": "line1\nline2\nline3\n"}),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "expected success: {outcome:?}");
        assert!(outcome.output().contains("3 lines"));
        let written = fs::read_to_string(dir.join("out.txt")).expect("read");
        assert!(written.contains("line1"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn concurrent_write_file_same_path_serializes_cleanly() {
        // The per-path write gate must let two writes to the same file in one turn
        // both succeed and leave the file as exactly one clean write (never a
        // corrupt interleave), and must not deadlock.
        let dir = temp_root("write_race");
        let (ctx1, _r1) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let (ctx2, _r2) = test_exec_context(TurnId(1), ToolCallId(2), dir.clone());
        let a = "AAAA\nAAAA\n";
        let b = "BBBB\nBBBB\n";
        let (o1, o2) = tokio::join!(
            WriteFileTool.execute(serde_json::json!({"path": "race.txt", "content": a}), ctx1),
            WriteFileTool.execute(serde_json::json!({"path": "race.txt", "content": b}), ctx2),
        );
        assert!(o1.is_success(), "first write failed: {o1:?}");
        assert!(o2.is_success(), "second write failed: {o2:?}");
        let final_content = fs::read_to_string(dir.join("race.txt")).expect("read");
        assert!(
            final_content == a || final_content == b,
            "file must be exactly one clean write, got {final_content:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_file_new_file_records_added_display_diff() {
        let dir = temp_root("write_new_diff");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = WriteFileTool
            .execute(
                serde_json::json!({"path": "out.txt", "content": "alpha\nbeta\n"}),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "expected success: {outcome:?}");
        let diff = outcome
            .metadata
            .display_diff
            .as_deref()
            .expect("display diff");
        assert!(diff.contains("+ alpha"));
        assert!(diff.contains("+ beta"));
        // No unified-diff header clutter (`---`/`+++`/`@@`).
        assert!(
            !diff.contains("@@"),
            "diff should not carry hunk headers: {diff}"
        );
        assert!(!diff.contains("/dev/null"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_file_existing_file_records_added_and_removed_display_diff() {
        let dir = temp_root("write_existing_diff");
        fs::write(dir.join("out.txt"), "alpha\nold\nomega\n").expect("write fixture");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = WriteFileTool
            .execute(
                serde_json::json!({"path": "out.txt", "content": "alpha\nnew\nomega\n"}),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "expected success: {outcome:?}");
        let diff = outcome
            .metadata
            .display_diff
            .as_deref()
            .expect("display diff");
        assert!(diff.contains("- old"));
        assert!(diff.contains("+ new"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_file_creates_parent_dirs() {
        let dir = temp_root("write_parents");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = WriteFileTool
            .execute(
                serde_json::json!({
                    "path": "sub/nested/out.txt",
                    "content": "deep",
                }),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "expected success: {outcome:?}");
        assert!(dir.join("sub/nested/out.txt").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_file_missing_content_errors() {
        let dir = temp_root("write_missing");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = WriteFileTool
            .execute(serde_json::json!({"path": "x.txt"}), ctx)
            .await;
        assert_eq!(outcome.status, mermaid_domain::ToolStatus::Error);
        let _ = fs::remove_dir_all(&dir);
    }

    // ─── F10: absolute-path block ───────────────────────────────────

    /// Reading `/etc/passwd` (or any absolute path outside workdir)
    /// must fail with a clear "outside the project" error. The tool
    /// schema advertises this contract; before F10 it was a lie.
    /// Reading an absolute path outside workdir succeeds.
    #[tokio::test]
    async fn read_file_allows_absolute_path_outside_workdir() {
        let dir = temp_root("read_abs_escape");
        let external_dir = temp_root("read_abs_external");
        let external_file = external_dir.join("ext.txt");
        fs::write(&external_file, "external content").unwrap();
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = ReadFileTool
            .execute(
                serde_json::json!({"path": external_file.to_string_lossy().to_string()}),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "expected success: {outcome:?}");
        assert_eq!(outcome.output(), "external content");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&external_dir);
    }

    /// Absolute path that lives INSIDE the workdir is allowed.
    #[tokio::test]
    async fn read_file_accepts_absolute_path_inside_workdir() {
        let dir = temp_root("read_abs_inside");
        let file = dir.join("hello.txt");
        fs::write(&file, "ok").expect("write fixture");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = ReadFileTool
            .execute(
                serde_json::json!({"path": file.to_string_lossy().to_string()}),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "expected success: {outcome:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Relative `..`-escape and external paths are supported.
    #[tokio::test]
    async fn write_file_allows_relative_parent_and_external_path() {
        let base = temp_root("write_dotdot_escape");
        let dir = base.join("project");
        fs::create_dir_all(&dir).unwrap();
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = WriteFileTool
            .execute(
                serde_json::json!({
                    "path": "../escape.txt",
                    "content": "written outside",
                }),
                ctx,
            )
            .await;
        assert!(
            outcome.is_success(),
            "expected success for write to ../escape.txt"
        );
        let written = base.join("escape.txt");
        assert!(written.exists());
        assert_eq!(fs::read_to_string(&written).unwrap(), "written outside");
        let _ = fs::remove_dir_all(&base);
    }

    /// `create_directory` needs the lexical-normalization fallback
    /// because the target doesn't exist yet (can't canonicalize).
    /// Verify the escape check still fires for non-existent targets.
    /// `create_directory` creates directories outside workdir when called.
    #[tokio::test]
    async fn create_directory_allows_absolute_path_outside_workdir() {
        let dir = temp_root("mkdir_abs_proj");
        let target = std::env::temp_dir().join(format!("mermaid_fs_target_{}", std::process::id()));
        let _ = fs::remove_dir_all(&target);
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = CreateDirectoryTool
            .execute(
                serde_json::json!({"path": target.to_string_lossy().to_string()}),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "expected success: {outcome:?}");
        assert!(target.exists());
        let _ = fs::remove_dir_all(&target);
        let _ = fs::remove_dir_all(&dir);
    }

    // ─── session-scratchpad dual root ────────────────────────────────

    /// Build an `ExecContext` with an explicit safety mode, NO approval
    /// broker, and (optionally) a materialized scratchpad. Unlike
    /// `test_exec_context` (pinned to `FullAccess`) this exercises the gate.
    fn scratch_ctx(
        mode: mermaid_runtime::SafetyMode,
        workdir: PathBuf,
        scratchpad: Option<PathBuf>,
    ) -> (ExecContext, tokio::sync::mpsc::Receiver<ProgressEvent>) {
        let mut config = mermaid_domain::Config::default();
        config.safety.mode = mode;
        let (mut ctx, rx) = crate::providers::ctx::test_exec_context_with_config(
            TurnId(1),
            ToolCallId(1),
            workdir,
            config,
        );
        ctx.scratchpad = scratchpad;
        (ctx, rx)
    }

    /// Project + scratch fixture pair with a unique, greppable name.
    fn scratch_fixture(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "mermaid_fs_scratch_{}_{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let project = base.join("project");
        let scratch = base.join("scratch");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&scratch).unwrap();
        (project, scratch)
    }

    /// True when any checkpoint manifest on disk references `marker`. The
    /// fixture paths are unique per test+pid, so a hit can only come from
    /// the mutation under test.
    fn any_checkpoint_mentions(marker: &str) -> bool {
        let Ok(data) = mermaid_runtime::data_dir() else {
            return false;
        };
        let Ok(entries) = fs::read_dir(data.join("checkpoints")) else {
            return false;
        };
        entries.flatten().any(|entry| {
            fs::read_to_string(entry.path().join("manifest.json"))
                .is_ok_and(|manifest| manifest.contains(marker))
        })
    }

    /// Scratchpad mutations proceed in Ask mode with NO approval broker
    /// bound (the gate is bypassed entirely) and never take a checkpoint.
    #[tokio::test]
    async fn scratch_mutations_are_ungated_and_never_checkpointed() {
        let (project, scratch) = scratch_fixture("ungated");
        let marker = scratch.display().to_string();

        // write_file into the scratchpad via absolute path.
        let file = scratch.join("notes.txt");
        let (ctx, _rx) = scratch_ctx(
            mermaid_runtime::SafetyMode::Ask,
            project.clone(),
            Some(scratch.clone()),
        );
        let outcome = WriteFileTool
            .execute(
                serde_json::json!({
                    "path": file.to_str().unwrap(),
                    "content": "scratch note\n",
                }),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "scratch write: {outcome:?}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "scratch note\n");

        // create_directory inside the scratchpad.
        let subdir = scratch.join("work/area");
        let (ctx, _rx) = scratch_ctx(
            mermaid_runtime::SafetyMode::Ask,
            project.clone(),
            Some(scratch.clone()),
        );
        let outcome = CreateDirectoryTool
            .execute(serde_json::json!({"path": subdir.to_str().unwrap()}), ctx)
            .await;
        assert!(outcome.is_success(), "scratch mkdir: {outcome:?}");
        assert!(subdir.is_dir());

        // delete_file inside the scratchpad.
        let (ctx, _rx) = scratch_ctx(
            mermaid_runtime::SafetyMode::Ask,
            project.clone(),
            Some(scratch.clone()),
        );
        let outcome = DeleteFileTool
            .execute(serde_json::json!({"path": file.to_str().unwrap()}), ctx)
            .await;
        assert!(outcome.is_success(), "scratch delete: {outcome:?}");
        assert!(!file.exists());

        // None of the mutations checkpointed the ephemeral scratch paths.
        assert!(
            !any_checkpoint_mentions(&marker),
            "scratch mutation must not create a checkpoint"
        );
        let _ = fs::remove_dir_all(project.parent().unwrap());
    }

    /// `ReadOnly` still blocks scratchpad mutations — the bypass only skips
    /// the approval flow, never the mode's mutation ban.
    #[tokio::test]
    async fn scratch_mutation_blocked_in_read_only() {
        let (project, scratch) = scratch_fixture("readonly");
        let file = scratch.join("blocked.txt");
        let (ctx, _rx) = scratch_ctx(
            mermaid_runtime::SafetyMode::ReadOnly,
            project.clone(),
            Some(scratch.clone()),
        );
        let outcome = WriteFileTool
            .execute(
                serde_json::json!({
                    "path": file.to_str().unwrap(),
                    "content": "nope",
                }),
                ctx,
            )
            .await;
        let error = outcome.error_message().expect("expected block");
        assert!(
            error.contains("blocked by policy"),
            "expected policy block, got: {error}"
        );
        assert!(!file.exists());
        let _ = fs::remove_dir_all(project.parent().unwrap());
    }

    /// A path outside scratchpad in Ask mode requires approval.
    #[tokio::test]
    async fn write_outside_both_roots_requires_approval_in_ask_mode() {
        let (project, scratch) = scratch_fixture("outside");
        let outside = project.parent().unwrap().join("elsewhere").join("out.txt");
        let (ctx, _rx) = scratch_ctx(
            mermaid_runtime::SafetyMode::Ask,
            project.clone(),
            Some(scratch.clone()),
        );
        let outcome = WriteFileTool
            .execute(
                serde_json::json!({
                    "path": outside.to_str().unwrap(),
                    "content": "should require approval",
                }),
                ctx,
            )
            .await;
        let error = outcome.error_message().expect("expected approval required");
        assert!(
            error.contains("Approval required"),
            "expected approval block, got: {error}"
        );
        assert!(!outside.exists());
        let _ = fs::remove_dir_all(project.parent().unwrap());
    }

    /// `read_file` follows a materialized scratchpad too.
    #[tokio::test]
    async fn read_file_reads_from_scratchpad() {
        let (project, scratch) = scratch_fixture("read");
        let file = scratch.join("stash.txt");
        fs::write(&file, "stashed").unwrap();
        let (ctx, _rx) = scratch_ctx(
            mermaid_runtime::SafetyMode::Ask,
            project.clone(),
            Some(scratch.clone()),
        );
        let outcome = ReadFileTool
            .execute(serde_json::json!({"path": file.to_str().unwrap()}), ctx)
            .await;
        assert!(outcome.is_success(), "scratch read: {outcome:?}");
        assert_eq!(outcome.output(), "stashed");
        let _ = fs::remove_dir_all(project.parent().unwrap());
    }

    #[tokio::test]
    async fn edit_file_replaces_target_content_successfully() {
        let (project, _scratch) = scratch_fixture("edit_success");
        let file = project.join("src").join("main.rs");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "fn main() {\n    println!(\"old\");\n}\n").unwrap();

        let (ctx, _rx) = scratch_ctx(
            mermaid_runtime::SafetyMode::FullAccess,
            project.clone(),
            None,
        );
        let outcome = EditFileTool
            .execute(
                serde_json::json!({
                    "path": "src/main.rs",
                    "target_content": "    println!(\"old\");",
                    "replacement_content": "    println!(\"new\");",
                }),
                ctx,
            )
            .await;

        assert!(outcome.is_success(), "edit outcome: {outcome:?}");
        let new_content = fs::read_to_string(&file).unwrap();
        assert_eq!(new_content, "fn main() {\n    println!(\"new\");\n}\n");
        let diff = outcome
            .metadata
            .display_diff
            .as_deref()
            .expect("display diff");
        assert!(diff.contains("-     println!(\"old\");"));
        assert!(diff.contains("+     println!(\"new\");"));
        let _ = fs::remove_dir_all(project.parent().unwrap());
    }

    #[tokio::test]
    async fn edit_file_target_not_found_returns_error() {
        let (project, _scratch) = scratch_fixture("edit_not_found");
        let file = project.join("hello.txt");
        fs::write(&file, "line 1\nline 2\n").unwrap();

        let (ctx, _rx) = scratch_ctx(
            mermaid_runtime::SafetyMode::FullAccess,
            project.clone(),
            None,
        );
        let outcome = EditFileTool
            .execute(
                serde_json::json!({
                    "path": "hello.txt",
                    "target_content": "nonexistent text",
                    "replacement_content": "replacement",
                }),
                ctx,
            )
            .await;

        assert!(!outcome.is_success());
        let err = outcome.error_message().unwrap();
        assert!(err.contains("could not find target_content"), "{err}");
        let _ = fs::remove_dir_all(project.parent().unwrap());
    }

    #[tokio::test]
    async fn edit_file_ambiguous_target_errors_unless_allow_multiple() {
        let (project, _scratch) = scratch_fixture("edit_ambig");
        let file = project.join("dup.txt");
        fs::write(&file, "foo\nbar\nfoo\n").unwrap();

        let (ctx, _rx) = scratch_ctx(
            mermaid_runtime::SafetyMode::FullAccess,
            project.clone(),
            None,
        );
        let outcome_ambig = EditFileTool
            .execute(
                serde_json::json!({
                    "path": "dup.txt",
                    "target_content": "foo",
                    "replacement_content": "baz",
                }),
                ctx,
            )
            .await;
        assert!(!outcome_ambig.is_success());
        let err = outcome_ambig.error_message().unwrap();
        assert!(err.contains("found 2 times"), "{err}");

        let (ctx2, _rx2) = scratch_ctx(
            mermaid_runtime::SafetyMode::FullAccess,
            project.clone(),
            None,
        );
        let outcome_allow = EditFileTool
            .execute(
                serde_json::json!({
                    "path": "dup.txt",
                    "target_content": "foo",
                    "replacement_content": "baz",
                    "allow_multiple": true,
                }),
                ctx2,
            )
            .await;
        assert!(outcome_allow.is_success());
        let new_content = fs::read_to_string(&file).unwrap();
        assert_eq!(new_content, "baz\nbar\nbaz\n");
        let _ = fs::remove_dir_all(project.parent().unwrap());
    }

    #[tokio::test]
    async fn edit_file_fuzzy_whitespace_matching_succeeds() {
        let (project, _scratch) = scratch_fixture("edit_fuzzy");
        let file = project.join("fuzzy.txt");
        fs::write(&file, "start\n    loose_whitespace();   \nend\n").unwrap();

        let (ctx, _rx) = scratch_ctx(
            mermaid_runtime::SafetyMode::FullAccess,
            project.clone(),
            None,
        );
        let outcome = EditFileTool
            .execute(
                serde_json::json!({
                    "path": "fuzzy.txt",
                    "target_content": "loose_whitespace();",
                    "replacement_content": "    tight_whitespace();",
                }),
                ctx,
            )
            .await;
        assert!(outcome.is_success());
        assert!(outcome.output().contains("fuzzy"));
        let new_content = fs::read_to_string(&file).unwrap();
        assert_eq!(new_content, "start\n    tight_whitespace();\nend\n");
        let _ = fs::remove_dir_all(project.parent().unwrap());
    }

    // ─── Reads outside the project go through the policy gate ─────────────

    /// A project dir plus a file that sits outside every allowed root.
    fn external_read_fixture(name: &str) -> (PathBuf, PathBuf) {
        let workdir = temp_root(&format!("{name}_project"));
        let external_dir = temp_root(&format!("{name}_external"));
        let external_file = external_dir.join("ext.txt");
        fs::write(&external_file, "external content").unwrap();
        (workdir, external_file)
    }

    fn ctx_in_mode(
        mode: mermaid_runtime::SafetyMode,
        workdir: PathBuf,
    ) -> (ExecContext, tokio::sync::mpsc::Receiver<ProgressEvent>) {
        let mut config = mermaid_domain::Config::default();
        config.safety.mode = mode;
        crate::providers::ctx::test_exec_context_with_config(
            TurnId(1),
            ToolCallId(1),
            workdir,
            config,
        )
    }

    async fn read_external(ctx: ExecContext, external_file: &Path) -> ToolOutcome {
        ReadFileTool
            .execute(
                serde_json::json!({"path": external_file.to_string_lossy().to_string()}),
                ctx,
            )
            .await
    }

    /// The read-only floor: an out-of-project read is an external side effect,
    /// denied in `read_only` and in plan mode. Before the gate, `read_file`
    /// handed back `~/.ssh/id_rsa` in every mode without a prompt.
    #[tokio::test]
    async fn read_outside_the_project_is_denied_under_the_read_only_floor() {
        for mode in [
            mermaid_runtime::SafetyMode::ReadOnly,
            mermaid_runtime::SafetyMode::Plan,
        ] {
            let (workdir, external_file) = external_read_fixture("read_ext_readonly");
            let (ctx, _rx) = ctx_in_mode(mode, workdir.clone());
            let outcome = read_external(ctx, &external_file).await;
            assert_eq!(
                outcome.status,
                mermaid_domain::ToolStatus::Error,
                "{mode:?} must deny an external read: {outcome:?}"
            );
            assert!(
                !outcome.output().contains("external content"),
                "{mode:?} leaked the file body: {}",
                outcome.output()
            );
            let _ = fs::remove_dir_all(&workdir);
            let _ = fs::remove_dir_all(external_file.parent().unwrap());
        }
    }

    /// Inside the project nothing changes: a `read_only` session still reads
    /// its own files without a prompt.
    #[tokio::test]
    async fn read_inside_the_project_stays_ungated_in_read_only_mode() {
        let workdir = temp_root("read_inside_readonly");
        fs::write(workdir.join("notes.md"), "project content").unwrap();
        let (ctx, _rx) = ctx_in_mode(mermaid_runtime::SafetyMode::ReadOnly, workdir.clone());
        let outcome = ReadFileTool
            .execute(serde_json::json!({"path": "notes.md"}), ctx)
            .await;
        assert_eq!(
            outcome.status,
            mermaid_domain::ToolStatus::Success,
            "{outcome:?}"
        );
        assert_eq!(outcome.output(), "project content");
        let _ = fs::remove_dir_all(&workdir);
    }

    /// `ask` mode prompts, and the prompt's "don't ask again" scope is the
    /// file's directory — never the whole tool, so approving one external
    /// read cannot silently cover a credential file later in the session.
    #[tokio::test]
    async fn read_outside_the_project_prompts_in_ask_mode_scoped_to_its_directory() {
        let (workdir, external_file) = external_read_fixture("read_ext_ask");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<mermaid_domain::Msg>(8);
        let broker = crate::providers::ApprovalBroker::new(tx);
        let (mut ctx, _prx) = ctx_in_mode(mermaid_runtime::SafetyMode::Ask, workdir.clone());
        ctx.approval = Some(broker.clone());
        let file_for_task = external_file.clone();
        let handle = tokio::spawn(async move { read_external(ctx, &file_for_task).await });

        let (call_id, tool, scope) = match rx.recv().await.expect("approval requested") {
            mermaid_domain::Msg::ApprovalRequested {
                call_id,
                tool,
                allowlist_scope,
                ..
            } => (call_id, tool, allowlist_scope),
            other => panic!("expected ApprovalRequested, got {other:?}"),
        };
        assert_eq!(tool, "read_file");
        let expected_dir = external_file.canonicalize().unwrap();
        let expected_dir = expected_dir.parent().unwrap();
        assert_eq!(
            scope,
            format!("read_file:{}", expected_dir.display()),
            "external reads are allowlisted per directory"
        );

        broker.resolve(call_id, crate::providers::ApprovalDecision::Approve);
        let outcome = handle.await.unwrap();
        assert_eq!(
            outcome.status,
            mermaid_domain::ToolStatus::Success,
            "{outcome:?}"
        );
        assert_eq!(outcome.output(), "external content");
        let _ = fs::remove_dir_all(&workdir);
        let _ = fs::remove_dir_all(external_file.parent().unwrap());
    }

    /// The user's "No" is final: the read fails and the body never leaves disk.
    #[tokio::test]
    async fn read_outside_the_project_denied_by_the_user_is_an_error() {
        let (workdir, external_file) = external_read_fixture("read_ext_deny");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<mermaid_domain::Msg>(8);
        let broker = crate::providers::ApprovalBroker::new(tx);
        let (mut ctx, _prx) = ctx_in_mode(mermaid_runtime::SafetyMode::Ask, workdir.clone());
        ctx.approval = Some(broker.clone());
        let file_for_task = external_file.clone();
        let handle = tokio::spawn(async move { read_external(ctx, &file_for_task).await });
        let call_id = match rx.recv().await.expect("approval requested") {
            mermaid_domain::Msg::ApprovalRequested { call_id, .. } => call_id,
            other => panic!("expected ApprovalRequested, got {other:?}"),
        };
        broker.resolve(call_id, crate::providers::ApprovalDecision::Deny);
        let outcome = handle.await.unwrap();
        assert_eq!(
            outcome.status,
            mermaid_domain::ToolStatus::Error,
            "{outcome:?}"
        );
        assert!(!outcome.output().contains("external content"));
        let _ = fs::remove_dir_all(&workdir);
        let _ = fs::remove_dir_all(external_file.parent().unwrap());
    }

    /// A symlink planted inside the project that resolves outside it is an
    /// external read: the resolver canonicalizes through the link, so the
    /// gate sees the real target, not the friendly-looking relative path.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_inside_the_project_that_points_outside_is_gated() {
        let (workdir, external_file) = external_read_fixture("read_ext_symlink");
        std::os::unix::fs::symlink(&external_file, workdir.join("innocent.txt")).unwrap();
        let (ctx, _rx) = ctx_in_mode(mermaid_runtime::SafetyMode::ReadOnly, workdir.clone());
        let outcome = ReadFileTool
            .execute(serde_json::json!({"path": "innocent.txt"}), ctx)
            .await;
        assert_eq!(
            outcome.status,
            mermaid_domain::ToolStatus::Error,
            "{outcome:?}"
        );
        assert!(!outcome.output().contains("external content"));
        let _ = fs::remove_dir_all(&workdir);
        let _ = fs::remove_dir_all(external_file.parent().unwrap());
    }

    /// `auto` mode classifies external writes instead of allowing them: with no
    /// classifier bound the gate escalates, so nothing lands outside the
    /// project unasked. Before this, an external write classified like an
    /// in-project edit and `auto` wrote it with a checkpoint and no question.
    #[tokio::test]
    async fn write_outside_the_project_is_not_silently_allowed_in_auto_mode() {
        let (project, scratch) = scratch_fixture("outside_auto");
        let outside = project.parent().unwrap().join("elsewhere").join("out.txt");
        let (ctx, _rx) = scratch_ctx(
            mermaid_runtime::SafetyMode::Auto,
            project.clone(),
            Some(scratch.clone()),
        );
        let outcome = WriteFileTool
            .execute(
                serde_json::json!({
                    "path": outside.to_str().unwrap(),
                    "content": "must not land unasked",
                }),
                ctx,
            )
            .await;
        assert_eq!(
            outcome.status,
            mermaid_domain::ToolStatus::Error,
            "auto mode must escalate an external write: {outcome:?}"
        );
        assert!(!outside.exists(), "the external file was written anyway");
        let _ = fs::remove_dir_all(project.parent().unwrap());
    }

    /// The control: an in-project write in `auto` mode is still an ordinary
    /// edit and proceeds without a prompt.
    #[tokio::test]
    async fn write_inside_the_project_stays_allowed_in_auto_mode() {
        let (project, scratch) = scratch_fixture("inside_auto");
        let (ctx, _rx) = scratch_ctx(
            mermaid_runtime::SafetyMode::Auto,
            project.clone(),
            Some(scratch.clone()),
        );
        let outcome = WriteFileTool
            .execute(
                serde_json::json!({"path": "inside.txt", "content": "fine"}),
                ctx,
            )
            .await;
        assert_eq!(
            outcome.status,
            mermaid_domain::ToolStatus::Success,
            "{outcome:?}"
        );
        assert_eq!(
            fs::read_to_string(project.join("inside.txt")).unwrap(),
            "fine"
        );
        let _ = fs::remove_dir_all(project.parent().unwrap());
    }
}
