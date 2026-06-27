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

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::constants::MAX_RESPONSE_CHARS as MAX_FILE_READ_BYTES;
use crate::domain::{ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata};
use crate::render::diff::{DIFF_ADDED_MARKER, DIFF_REMOVED_MARKER};

use super::super::ctx::{ExecContext, ProgressEvent};
use super::ToolExecutor;
use super::path_safety::{relative_within, resolve_path_safe};

/// Small helper for building a `ToolDefinition` with a typical
/// JSON-schema-shaped input_schema. Keeps the per-tool definitions
/// readable.
fn defn(name: &str, description: &str, input_schema: serde_json::Value) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
    }
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
            "Read the contents of one or more files from disk. Prefer relative paths; absolute paths must resolve inside the project directory or the call is rejected.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File to read (single)." },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Multiple files to read in parallel."
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
        let workdir = ctx.workdir.clone();
        let mut combined = String::new();

        for (idx, raw_path) in paths.iter().enumerate() {
            // Race the file read against the turn's cancel token. If
            // the user Ctrl+C's mid-read, we bail immediately.
            tokio::select! {
                biased;
                _ = ctx.token.cancelled() => {
                    return ToolOutcome::cancelled();
                },
                read = read_one(&workdir, raw_path) => {
                    match read {
                        Ok(content) => {
                            if paths.len() > 1 {
                                let _ = ctx.progress.send(ProgressEvent::Status(
                                    format!("read {}/{}: {}", idx + 1, paths.len(), raw_path),
                                )).await;
                                combined.push_str(&format!(
                                    "=== {} ===\n{}\n\n",
                                    raw_path, content
                                ));
                            } else {
                                combined = content;
                            }
                        },
                        Err(e) => {
                            return ToolOutcome::error(
                                format!("{}: {}", raw_path, e),
                                start.elapsed().as_secs_f64(),
                            );
                        },
                    }
                },
            }
        }

        let duration_secs = start.elapsed().as_secs_f64();
        let line_count = combined.lines().count();
        let byte_count = combined.len();
        let truncated = combined.contains("[TRUNCATED: file exceeded read cap]");
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

/// `edit_file` — exact-match string replacement. Used for targeted
/// edits rather than full file rewrites. Errors if the `old_string`
/// doesn't appear exactly once.
pub struct EditFileTool;

#[async_trait]
impl ToolExecutor for EditFileTool {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn schema(&self) -> ToolDefinition {
        defn(
            "edit_file",
            "Replace exactly one occurrence of `old_string` with `new_string` in the file at `path`. Fails if `old_string` doesn't appear or appears more than once — add surrounding context until the match is unique.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string", "description": "Exact text to replace. Must appear exactly once." },
                    "new_string": { "type": "string", "description": "Replacement text." }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        )
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let Some(raw_path) = args.get("path").and_then(|v| v.as_str()) else {
            return err("edit_file requires 'path'", 0.0);
        };
        let Some(old_string) = args.get("old_string").and_then(|v| v.as_str()) else {
            return err("edit_file requires 'old_string'", 0.0);
        };
        let Some(new_string) = args.get("new_string").and_then(|v| v.as_str()) else {
            return err("edit_file requires 'new_string'", 0.0);
        };

        let start = std::time::Instant::now();
        let abs = match resolve_path_safe(&ctx.workdir, raw_path) {
            Ok(p) => p,
            Err(e) => return err(&format!("edit_file: {}", e), 0.0),
        };
        let rel = match relative_within(&ctx.workdir, raw_path) {
            Ok(r) => r,
            Err(e) => return err(&format!("edit_file: {}", e), 0.0),
        };
        let pending_action = serde_json::json!({
            "tool": "edit_file",
            "args": {
                "path": raw_path,
                "old_string": old_string,
                "new_string": new_string,
            },
            "workdir": ctx.workdir.display().to_string(),
            "turn_id": ctx.turn.0,
            "call_id": ctx.call_id.0,
            "task_id": ctx.task_id.clone(),
        });
        if let Some(outcome) = mutation_policy_outcome(
            &ctx,
            "edit_file",
            raw_path,
            std::slice::from_ref(&abs),
            pending_action,
        )
        .await
        {
            return outcome;
        }
        if ctx.config.safety.checkpoint_on_mutation
            && let Err(e) = crate::runtime::create_checkpoint_for_task(
                &ctx.workdir,
                std::slice::from_ref(&abs),
                Some(serde_json::json!({
                    "tool": "edit_file",
                    "path": raw_path,
                })),
                ctx.task_id.clone(),
            )
        {
            return err(&format!("edit_file checkpoint failed: {}", e), 0.0);
        }
        let old_owned = old_string.to_string();
        let new_owned = new_string.to_string();
        let workdir = ctx.workdir.clone();
        let display_path = raw_path.to_string();

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::cancelled(),
            result = tokio::task::spawn_blocking(move || edit_blocking(&workdir, &rel, &old_owned, &new_owned)) => {
                match result {
                    Ok(Ok(edit)) => {
                        let duration_secs = start.elapsed().as_secs_f64();
                        after_file_mutation(&ctx, "edit_file", &display_path);
                        ToolOutcome::success(
                            format!("Edited {} ({} replacement{})",
                            display_path,
                            edit.replacements,
                            if edit.replacements == 1 { "" } else { "s" }),
                            diff_summary(edit.added, edit.removed, duration_secs),
                            duration_secs,
                        )
                        .with_metadata(ToolRunMetadata {
                            detail: ToolMetadata::EditFile {
                                path: display_path,
                                replacements: edit.replacements,
                            },
                            display_diff: Some(edit.display_diff),
                            diff_truncated: edit.truncated,
                            ..ToolRunMetadata::default()
                        })
                    },
                    Ok(Err(e)) => err(&format!("edit_file({}): {}", display_path, e),
                                       start.elapsed().as_secs_f64()),
                    Err(e) => err(&format!("edit_file join error: {}", e),
                                   start.elapsed().as_secs_f64()),
                }
            }
        }
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
            "Remove a file from disk. Fails on directories — use `execute_command rm -rf` for those.",
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
        let abs = match resolve_path_safe(&ctx.workdir, raw_path) {
            Ok(p) => p,
            Err(e) => return err(&format!("delete_file: {}", e), 0.0),
        };
        let rel = match relative_within(&ctx.workdir, raw_path) {
            Ok(r) => r,
            Err(e) => return err(&format!("delete_file: {}", e), 0.0),
        };
        let pending_action = serde_json::json!({
            "tool": "delete_file",
            "args": { "path": raw_path },
            "workdir": ctx.workdir.display().to_string(),
            "turn_id": ctx.turn.0,
            "call_id": ctx.call_id.0,
            "task_id": ctx.task_id.clone(),
        });
        if let Some(outcome) = mutation_policy_outcome(
            &ctx,
            "delete_file",
            raw_path,
            std::slice::from_ref(&abs),
            pending_action,
        )
        .await
        {
            return outcome;
        }
        if ctx.config.safety.checkpoint_on_mutation
            && let Err(e) = crate::runtime::create_checkpoint_for_task(
                &ctx.workdir,
                std::slice::from_ref(&abs),
                Some(serde_json::json!({
                    "tool": "delete_file",
                    "path": raw_path,
                })),
                ctx.task_id.clone(),
            )
        {
            return err(&format!("delete_file checkpoint failed: {}", e), 0.0);
        }
        let display = raw_path.to_string();
        let workdir = ctx.workdir.clone();

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::cancelled(),
            result = tokio::task::spawn_blocking(move || crate::runtime::remove_file_beneath(&workdir, &rel)) => {
                match result {
                    Ok(Ok(())) => {
                        let duration_secs = start.elapsed().as_secs_f64();
                        after_file_mutation(&ctx, "delete_file", &display);
                        ToolOutcome::success(
                            format!("Deleted {}", display),
                            "file deleted",
                            duration_secs,
                        )
                        .with_metadata(ToolRunMetadata {
                            detail: ToolMetadata::DeleteFile { path: display },
                            ..ToolRunMetadata::default()
                        })
                    },
                    Ok(Err(e)) => err(&format!("delete_file({}): {}", display, e),
                                       start.elapsed().as_secs_f64()),
                    Err(e) => err(&format!("delete_file join error: {}", e),
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
            "Create a directory (and any missing parents) at the given path.",
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
        let abs = match resolve_path_safe(&ctx.workdir, raw_path) {
            Ok(p) => p,
            Err(e) => return err(&format!("create_directory: {}", e), 0.0),
        };
        let rel = match relative_within(&ctx.workdir, raw_path) {
            Ok(r) => r,
            Err(e) => return err(&format!("create_directory: {}", e), 0.0),
        };
        let pending_action = serde_json::json!({
            "tool": "create_directory",
            "args": { "path": raw_path },
            "workdir": ctx.workdir.display().to_string(),
            "turn_id": ctx.turn.0,
            "call_id": ctx.call_id.0,
            "task_id": ctx.task_id.clone(),
        });
        if let Some(outcome) = mutation_policy_outcome(
            &ctx,
            "create_directory",
            raw_path,
            std::slice::from_ref(&abs),
            pending_action,
        )
        .await
        {
            return outcome;
        }
        if ctx.config.safety.checkpoint_on_mutation
            && let Err(e) = crate::runtime::create_checkpoint_for_task(
                &ctx.workdir,
                std::slice::from_ref(&abs),
                Some(serde_json::json!({
                    "tool": "create_directory",
                    "path": raw_path,
                })),
                ctx.task_id.clone(),
            )
        {
            return err(&format!("create_directory checkpoint failed: {}", e), 0.0);
        }
        let display = raw_path.to_string();
        let workdir = ctx.workdir.clone();

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::cancelled(),
            result = tokio::task::spawn_blocking(move || crate::runtime::create_dir_all_beneath(&workdir, &rel)) => {
                match result {
                    Ok(Ok(())) => {
                        let duration_secs = start.elapsed().as_secs_f64();
                        after_file_mutation(&ctx, "create_directory", &display);
                        ToolOutcome::success(
                            format!("Created directory {}", display),
                            "directory created",
                            duration_secs,
                        )
                        .with_metadata(ToolRunMetadata {
                            detail: ToolMetadata::CreateDirectory { path: display },
                            ..ToolRunMetadata::default()
                        })
                    },
                    Ok(Err(e)) => err(&format!("create_directory({}): {}", display, e),
                                       start.elapsed().as_secs_f64()),
                    Err(e) => err(&format!("create_directory join error: {}", e),
                                   start.elapsed().as_secs_f64()),
                }
            }
        }
    }
}

/// `write_file` — write a single file, creating parent dirs as needed.
pub struct WriteFileTool;

#[async_trait]
impl ToolExecutor for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn schema(&self) -> ToolDefinition {
        defn(
            "write_file",
            "Write (overwrite) a file at `path` with `content`. Creates parent directories automatically. Prefer `edit_file` for small targeted changes.",
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
        let abs_path = match resolve_path_safe(&ctx.workdir, path) {
            Ok(p) => p,
            Err(e) => return ToolOutcome::error(format!("write_file: {}", e), 0.0),
        };
        // Workdir-relative name for the confined fd write (the actual byte path).
        let rel = match relative_within(&ctx.workdir, path) {
            Ok(r) => r,
            Err(e) => return ToolOutcome::error(format!("write_file: {}", e), 0.0),
        };
        let pending_action = serde_json::json!({
            "tool": "write_file",
            "args": { "path": path, "content": content },
            "workdir": ctx.workdir.display().to_string(),
            "turn_id": ctx.turn.0,
            "call_id": ctx.call_id.0,
            "task_id": ctx.task_id.clone(),
        });
        if let Some(outcome) = mutation_policy_outcome(
            &ctx,
            "write_file",
            path,
            std::slice::from_ref(&abs_path),
            pending_action,
        )
        .await
        {
            return outcome;
        }
        if ctx.config.safety.checkpoint_on_mutation
            && let Err(e) = crate::runtime::create_checkpoint_for_task(
                &ctx.workdir,
                std::slice::from_ref(&abs_path),
                Some(serde_json::json!({
                    "tool": "write_file",
                    "path": path,
                })),
                ctx.task_id.clone(),
            )
        {
            return ToolOutcome::error(format!("write_file checkpoint failed: {}", e), 0.0);
        }
        let display_path = path.to_string();
        let line_count = content.lines().count();
        let byte_count = content.len();
        let old_content = std::fs::read_to_string(&abs_path).ok();
        let created = Some(old_content.is_none());
        let diff = generate_display_diff(old_content.as_deref().unwrap_or(""), content);
        let content = content.to_string();
        let workdir = ctx.workdir.clone();

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::cancelled(),
            result = tokio::task::spawn_blocking(move || write_one_blocking(&workdir, &rel, &content)) => {
                match result {
                    Ok(Ok(actual_line_count)) => {
                        let duration_secs = start.elapsed().as_secs_f64();
                        after_file_mutation(&ctx, "write_file", &display_path);
                        ToolOutcome::success(
                            format!("Wrote {} ({} lines)", display_path, actual_line_count),
                            format!("{} {} written", actual_line_count, plural(actual_line_count, "line", "lines")),
                            duration_secs,
                        )
                        .with_metadata(ToolRunMetadata {
                            detail: ToolMetadata::WriteFile {
                                path: display_path,
                                line_count,
                                byte_count,
                                created,
                            },
                            line_count: Some(line_count),
                            byte_count: Some(byte_count),
                            display_diff: Some(diff.display_diff),
                            diff_truncated: diff.truncated,
                            ..ToolRunMetadata::default()
                        })
                    },
                    Ok(Err(e)) => ToolOutcome::error(
                        format!("write_file({}): {}", display_path, e),
                        start.elapsed().as_secs_f64(),
                    ),
                    Err(e) => ToolOutcome::error(
                        format!("write_file join error: {}", e),
                        start.elapsed().as_secs_f64(),
                    ),
                }
            }
        }
    }
}

// ─── helpers ────────────────────────────────────────────────────────

fn extract_paths(args: &serde_json::Value) -> Result<Vec<String>, String> {
    // Accept both shapes: `{path: "x"}` and `{paths: ["x", "y"]}`.
    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
        return Ok(vec![p.to_string()]);
    }
    if let Some(arr) = args.get("paths").and_then(|v| v.as_array()) {
        if arr.len() > crate::constants::MAX_BATCH_TOOL_ITEMS {
            return Err(format!(
                "read_file: too many paths ({}); cap is {} per call — split the request",
                arr.len(),
                crate::constants::MAX_BATCH_TOOL_ITEMS
            ));
        }
        let mut out = Vec::with_capacity(arr.len());
        for v in arr {
            let Some(s) = v.as_str() else {
                return Err("read_file 'paths' must be an array of strings".to_string());
            };
            out.push(s.to_string());
        }
        return Ok(out);
    }
    Err("read_file requires 'path' or 'paths'".to_string())
}

async fn read_one(workdir: &Path, raw: &str) -> std::io::Result<String> {
    // Canonical containment gate — rejects escapes (incl. existing symlinks that
    // resolve outside the project) before we touch the file.
    resolve_path_safe(workdir, raw)
        .map_err(|msg| std::io::Error::new(std::io::ErrorKind::PermissionDenied, msg))?;
    // Workdir-relative name for the confined fd read, so the bytes come from the
    // inode the kernel resolved under RESOLVE_BENEATH rather than whatever a
    // concurrently-swapped symlink now points at (#77).
    let rel = relative_within(workdir, raw)
        .map_err(|msg| std::io::Error::new(std::io::ErrorKind::PermissionDenied, msg))?;
    let workdir = workdir.to_path_buf();
    let content = tokio::task::spawn_blocking(move || {
        let file = crate::runtime::open_beneath(&workdir, &rel, crate::runtime::OpenIntent::Read)?;
        // Bounded read: never pull more than the cap (+1 probe byte) into RAM,
        // so a model pointing `read_file` at a multi-gigabyte file can't OOM the
        // process — a full read would have slurped the whole thing first (#15).
        let (data, truncated) = crate::utils::read_capped(file, MAX_FILE_READ_BYTES)?;
        let mut s = String::from_utf8_lossy(&data).into_owned();
        if truncated {
            // Char-boundary-safe truncation with a marker footer.
            let cut = s.floor_char_boundary(MAX_FILE_READ_BYTES);
            s.truncate(cut);
            s.push_str("\n\n[TRUNCATED: file exceeded read cap]");
        }
        Ok::<_, std::io::Error>(s)
    })
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))??;
    Ok(content)
}

/// Write `content` to `rel` beneath `workdir` through the symlink-confined
/// opener, creating parent dirs the same way. The bytes land on the inode the
/// kernel resolved under `RESOLVE_BENEATH`, so a parent dir swapped for an
/// escaping symlink after the path check can't redirect the write (#77).
fn write_one_blocking(workdir: &Path, rel: &Path, content: &str) -> std::io::Result<usize> {
    if let Some(parent) = rel.parent()
        && !parent.as_os_str().is_empty()
    {
        crate::runtime::create_dir_all_beneath(workdir, parent)?;
    }
    let mut file =
        crate::runtime::open_beneath(workdir, rel, crate::runtime::OpenIntent::WriteTruncate)?;
    std::io::Write::write_all(&mut file, content.as_bytes())?;
    Ok(content.lines().count())
}

async fn mutation_policy_outcome(
    ctx: &ExecContext,
    tool: &str,
    path: &str,
    checkpoint_paths: &[PathBuf],
    pending_action: serde_json::Value,
) -> Option<ToolOutcome> {
    let mut request = crate::runtime::ActionRequest::new(
        tool,
        crate::runtime::ToolCategory::Edit,
        format!("{} {}", tool, path),
    );
    request.path = Some(path.to_string());
    // File mutations are replayable: an Ask decision checkpoints, records an
    // approval, and blocks (handled inside the gate).
    match super::policy_gate::gate(ctx, request, checkpoint_paths, pending_action, true).await {
        super::policy_gate::Gate::Block(outcome) => Some(outcome),
        super::policy_gate::Gate::Proceed { .. } => {
            let _ = crate::runtime::run_plugin_hooks(
                "before_file_mutation",
                &serde_json::json!({
                    "task_id": ctx.task_id.clone(),
                    "turn_id": ctx.turn.0,
                    "call_id": ctx.call_id.0,
                    "tool": tool,
                    "path": path,
                }),
            );
            None
        },
    }
}

fn after_file_mutation(ctx: &ExecContext, tool: &str, path: &str) {
    let _ = crate::runtime::run_plugin_hooks(
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

struct EditResult {
    replacements: usize,
    display_diff: String,
    added: usize,
    removed: usize,
    truncated: bool,
}

/// Read-modify-write `rel` beneath `workdir` entirely through the
/// symlink-confined opener, so both the read and the write bind to the inode the
/// kernel resolved under `RESOLVE_BENEATH` — a swapped-in symlink can't redirect
/// either half (#77).
fn edit_blocking(
    workdir: &Path,
    rel: &Path,
    old_string: &str,
    new_string: &str,
) -> std::io::Result<EditResult> {
    let current = {
        let mut file =
            crate::runtime::open_beneath(workdir, rel, crate::runtime::OpenIntent::Read)?;
        let mut s = String::new();
        std::io::Read::read_to_string(&mut file, &mut s)?;
        s
    };
    let count = current.matches(old_string).count();
    if count == 0 {
        return Err(std::io::Error::other(
            "old_string not found (is the snippet correct? use read_file to verify)",
        ));
    }
    if count > 1 {
        return Err(std::io::Error::other(format!(
            "old_string appears {} times — add more context so the match is unique",
            count
        )));
    }
    let updated = current.replacen(old_string, new_string, 1);
    let diff = generate_display_diff(&current, &updated);
    let mut file =
        crate::runtime::open_beneath(workdir, rel, crate::runtime::OpenIntent::WriteTruncate)?;
    std::io::Write::write_all(&mut file, updated.as_bytes())?;
    Ok(EditResult {
        replacements: 1,
        display_diff: diff.display_diff,
        added: diff.added,
        removed: diff.removed,
        truncated: diff.truncated,
    })
}

fn err(msg: &str, duration_secs: f64) -> ToolOutcome {
    ToolOutcome::error(msg, duration_secs)
}

fn plural(count: usize, singular: &'static str, plural: &'static str) -> &'static str {
    if count == 1 { singular } else { plural }
}

#[derive(Debug, Clone)]
struct DisplayDiff {
    display_diff: String,
    added: usize,
    removed: usize,
    truncated: bool,
}

const DIFF_CONTEXT_LINES: usize = 3;
const MAX_DISPLAY_DIFF_LINES: usize = 220;

fn generate_display_diff(old: &str, new: &str) -> DisplayDiff {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut prefix = 0usize;
    let min_len = old_lines.len().min(new_lines.len());
    while prefix < min_len && old_lines[prefix] == new_lines[prefix] {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < min_len.saturating_sub(prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_changed_end = old_lines.len().saturating_sub(suffix);
    let new_changed_end = new_lines.len().saturating_sub(suffix);
    let old_changed = &old_lines[prefix..old_changed_end];
    let new_changed = &new_lines[prefix..new_changed_end];
    let added = new_changed.len();
    let removed = old_changed.len();

    let context_start = prefix.saturating_sub(DIFF_CONTEXT_LINES);
    let context_end_old = (old_changed_end + DIFF_CONTEXT_LINES).min(old_lines.len());
    // No unified-diff header lines (`---`/`+++`/`@@`) — they're visual clutter
    // in the transcript. Each body line already carries its own line number +
    // marker, which is all the renderer needs.
    let mut lines = Vec::new();

    let mut truncated = false;
    let push_line = |line: String, lines: &mut Vec<String>, truncated: &mut bool| {
        if lines.len() < MAX_DISPLAY_DIFF_LINES {
            lines.push(line);
        } else {
            *truncated = true;
        }
    };

    for (idx, line) in old_lines[context_start..prefix].iter().enumerate() {
        push_line(
            format!("{:>4}   {}", context_start + idx + 1, line),
            &mut lines,
            &mut truncated,
        );
    }
    for (idx, line) in old_changed.iter().enumerate() {
        push_line(
            format!("{:>4}{}{}", prefix + idx + 1, DIFF_REMOVED_MARKER, line),
            &mut lines,
            &mut truncated,
        );
    }
    for (idx, line) in new_changed.iter().enumerate() {
        push_line(
            format!("{:>4}{}{}", prefix + idx + 1, DIFF_ADDED_MARKER, line),
            &mut lines,
            &mut truncated,
        );
    }
    for (idx, line) in old_lines[old_changed_end..context_end_old]
        .iter()
        .enumerate()
    {
        push_line(
            format!("{:>4}   {}", old_changed_end + idx + 1, line),
            &mut lines,
            &mut truncated,
        );
    }
    if truncated {
        lines.push(format!(
            "... diff truncated after {} display lines",
            MAX_DISPLAY_DIFF_LINES
        ));
    }

    DisplayDiff {
        display_diff: lines.join("\n"),
        added,
        removed,
        truncated,
    }
}

fn diff_summary(added: usize, removed: usize, duration_secs: f64) -> String {
    format!(
        "Success, +{} -{}, took {}",
        added,
        removed,
        format_duration_for_diff(duration_secs)
    )
}

fn format_duration_for_diff(seconds: f64) -> String {
    if seconds < 1.0 {
        format!("{}ms", (seconds * 1000.0).round().max(1.0) as u64)
    } else if seconds < 10.0 {
        format!("{:.1}s", seconds)
    } else {
        format!("{}s", seconds.round() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ToolCallId, TurnId};
    use crate::providers::ctx::test_exec_context;
    use std::fs;

    #[test]
    fn resolve_path_safe_contains_to_workdir() {
        let root = std::env::temp_dir().join(format!("mermaid_rps_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();

        // In-root existing + not-yet-existing targets resolve inside root.
        assert!(resolve_path_safe(&root, "sub").is_ok());
        assert!(resolve_path_safe(&root, "sub/new.txt").is_ok());
        let resolved = resolve_path_safe(&root, "sub/new.txt").unwrap();
        let canon_root = fs::canonicalize(&root).unwrap();
        assert!(resolved.starts_with(&canon_root));

        // `..` escape and absolute outside are rejected.
        assert!(resolve_path_safe(&root, "../escape.txt").is_err());
        assert!(resolve_path_safe(&root, "../../etc/passwd").is_err());
        let outside = std::env::temp_dir().join("definitely_outside.txt");
        assert!(resolve_path_safe(&root, &outside.display().to_string()).is_err());

        let _ = fs::remove_dir_all(&root);
    }

    fn temp_root(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("mermaid_providers_fs_{}", name));
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
        assert!(outcome.is_success(), "expected success: {:?}", outcome);
        assert_eq!(outcome.output(), "hello");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_missing_path_errors() {
        let dir = temp_root("read_missing_path");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = ReadFileTool.execute(serde_json::json!({}), ctx).await;
        assert_eq!(outcome.status, crate::domain::ToolStatus::Error);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_nonexistent_errors() {
        let dir = temp_root("read_nonex");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = ReadFileTool
            .execute(serde_json::json!({"path": "does_not_exist.txt"}), ctx)
            .await;
        assert_eq!(outcome.status, crate::domain::ToolStatus::Error);
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
        assert!(outcome.is_success(), "expected success: {:?}", outcome);
        let output = outcome.output();
        assert!(output.contains("=== a.txt ==="));
        assert!(output.contains("alpha"));
        assert!(output.contains("=== b.txt ==="));
        assert!(output.contains("beta"));
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
        assert!(outcome.is_success(), "expected success: {:?}", outcome);
        assert!(outcome.output().contains("3 lines"));
        let written = fs::read_to_string(dir.join("out.txt")).expect("read");
        assert!(written.contains("line1"));
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
        assert!(outcome.is_success(), "expected success: {:?}", outcome);
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
        assert!(outcome.is_success(), "expected success: {:?}", outcome);
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
    async fn edit_file_records_display_diff() {
        let dir = temp_root("edit_diff");
        fs::write(dir.join("main.py"), "alpha\nold\nomega\n").expect("write fixture");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = EditFileTool
            .execute(
                serde_json::json!({
                    "path": "main.py",
                    "old_string": "old",
                    "new_string": "new",
                }),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "expected success: {:?}", outcome);
        let diff = outcome
            .metadata
            .display_diff
            .as_deref()
            .expect("display diff");
        assert!(diff.contains("- old"));
        assert!(diff.contains("+ new"));
        assert!(
            !diff.contains("@@"),
            "diff should not carry hunk headers: {diff}"
        );
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
        assert!(outcome.is_success(), "expected success: {:?}", outcome);
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
        assert_eq!(outcome.status, crate::domain::ToolStatus::Error);
        let _ = fs::remove_dir_all(&dir);
    }

    // ─── F10: absolute-path block ───────────────────────────────────

    /// Reading `/etc/passwd` (or any absolute path outside workdir)
    /// must fail with a clear "outside the project" error. The tool
    /// schema advertises this contract; before F10 it was a lie.
    #[tokio::test]
    async fn read_file_rejects_absolute_path_outside_workdir() {
        let dir = temp_root("read_abs_escape");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        // Pick a path that's definitely outside a fresh /tmp/* workdir.
        let outcome = ReadFileTool
            .execute(serde_json::json!({"path": "/etc/passwd"}), ctx)
            .await;
        let error = outcome.error_message().expect("expected error");
        assert!(
            error.contains("outside the project"),
            "expected security reject, got: {}",
            error
        );
        let _ = fs::remove_dir_all(&dir);
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
        assert!(outcome.is_success(), "expected success: {:?}", outcome);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Relative `..`-escape must also be blocked — they resolve against
    /// the workdir and land outside it, so the lexical normalization
    /// in `resolve_path_safe` catches them.
    #[tokio::test]
    async fn write_file_rejects_relative_parent_escape() {
        let dir = temp_root("write_dotdot_escape");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = WriteFileTool
            .execute(
                serde_json::json!({
                    "path": "../escape.txt",
                    "content": "should not write",
                }),
                ctx,
            )
            .await;
        let error = outcome.error_message().expect("expected error");
        assert!(
            error.contains("outside the project"),
            "expected security reject, got: {}",
            error
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// `create_directory` needs the lexical-normalization fallback
    /// because the target doesn't exist yet (can't canonicalize).
    /// Verify the escape check still fires for non-existent targets.
    #[tokio::test]
    async fn create_directory_rejects_absolute_path_outside_workdir() {
        let dir = temp_root("mkdir_abs_escape");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = CreateDirectoryTool
            .execute(
                serde_json::json!({"path": "/tmp/mermaid_fs_escape_target"}),
                ctx,
            )
            .await;
        let error = outcome.error_message().expect("expected error");
        assert!(
            error.contains("outside the project"),
            "expected security reject, got: {}",
            error
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
