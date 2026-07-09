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

/// Aggregate cap for a multi-file `read_file` result (#F45). Each file is
/// individually bounded at `MAX_RESPONSE_CHARS` by `read_one`, but a batch of up
/// to `MAX_BATCH_TOOL_ITEMS` files could otherwise sum to ~12.8 MB in a single
/// tool result — far past any sane model-context budget. This bounds the
/// combined total; single-file reads are already bounded and unaffected.
const MAX_READ_AGGREGATE_CHARS: usize = crate::constants::MAX_RESPONSE_CHARS;

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
        let workdir = ctx.workdir.clone();
        let mut combined = String::new();
        let mut any_truncated = false;

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
                        Ok((content, was_truncated)) => {
                            any_truncated |= was_truncated;
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

        // F45: bound the COMBINED multi-file result. Each file is already capped
        // at MAX_RESPONSE_CHARS by read_one, but a batch of files can still sum to
        // ~12.8 MB in one tool result — past any sane context budget. Only the
        // multi-file accumulation needs this (single-file output is already
        // bounded); truncate_middle keeps the head AND tail with an elision marker.
        if paths.len() > 1 && combined.len() > MAX_READ_AGGREGATE_CHARS {
            combined = crate::utils::truncate_middle(&combined, MAX_READ_AGGREGATE_CHARS);
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
        // Serialize writers to this canonical path: sibling tool calls in the same
        // turn run concurrently, so without this the checkpoint + delete could race
        // another writer to the same file. Distinct paths still overlap. Raced
        // against cancellation so a contended lock stays Ctrl+C-responsive.
        let _write_guard = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            g = super::path_lock::lock_path(&abs) => g,
        };
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
        // Serialize writers to this canonical path (see delete_file). mkdir -p is
        // idempotent, but a uniform gate keeps ordering consistent and cheap.
        let _write_guard = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            g = super::path_lock::lock_path(&abs) => g,
        };
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
        // Serialize writers to this canonical path: two write_file/edit calls to
        // the same file in one turn run concurrently, so without this the last
        // atomic rename silently wins (lost update). Distinct paths still overlap.
        // The owned guard is Send and held across the spawn_blocking below.
        let _write_guard = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            g = super::path_lock::lock_path(&abs_path) => g,
        };
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
        let content = content.to_string();
        let workdir = ctx.workdir.clone();

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::cancelled(),
            // The prior-content read (for the display diff) now happens INSIDE
            // this blocking job and BOUNDED (#F44/RC-L) — never a synchronous
            // unbounded `read_to_string` on the async worker thread.
            result = tokio::task::spawn_blocking(move || write_with_diff_blocking(&workdir, &abs_path, &rel, &content)) => {
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

/// Read one file (bounded). Returns the (possibly marker-footed) text and the
/// REAL truncation flag from the bounded read, so the caller propagates that
/// rather than sniffing the output for the marker string — which a file whose
/// own content contains that literal text would otherwise falsely trip (#78).
async fn read_one(workdir: &Path, raw: &str) -> std::io::Result<(String, bool)> {
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
    let result = tokio::task::spawn_blocking(move || {
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
        Ok::<_, std::io::Error>((s, truncated))
    })
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))??;
    Ok(result)
}

/// Write `content` to `rel` beneath `workdir` through the symlink-confined
/// *atomic* writer, creating parent dirs the same confined way. The bytes are
/// written to a temp and `renameat`-swapped over the target, all beneath the
/// directory fd the kernel resolved under `RESOLVE_BENEATH`: a parent dir
/// swapped for an escaping symlink can't redirect the write (#77), and a
/// crash/kill/disk-full mid-write leaves the previous file intact rather than a
/// truncated or half-written one.
fn write_one_blocking(workdir: &Path, rel: &Path, content: &str) -> std::io::Result<usize> {
    if let Some(parent) = rel.parent()
        && !parent.as_os_str().is_empty()
    {
        crate::runtime::create_dir_all_beneath(workdir, parent)?;
    }
    crate::runtime::write_atomic_beneath(workdir, rel, content.as_bytes())?;
    Ok(content.lines().count())
}

struct WriteResult {
    line_count: usize,
    created: bool,
    diff: crate::render::diff::DisplayDiff,
}

/// Write `content` and build the display diff against the prior file in ONE
/// blocking job (#F44/RC-L). The prior content is read BOUNDED via
/// [`crate::utils::read_file_capped`] — overwriting a multi-gigabyte file must
/// not slurp it into RAM on the async worker just to render a diff. A prior file
/// larger than the read cap (or otherwise unreadable) is elided from the diff
/// rather than read whole.
fn write_with_diff_blocking(
    workdir: &Path,
    abs_path: &Path,
    rel: &Path,
    content: &str,
) -> std::io::Result<WriteResult> {
    let (old_content, created, elide_diff) =
        match crate::utils::read_file_capped(abs_path, MAX_FILE_READ_BYTES) {
            Ok((data, false)) => (String::from_utf8_lossy(&data).into_owned(), false, false),
            // Existing file is past the read cap — don't pull it all into RAM.
            Ok((_, true)) => (String::new(), false, true),
            // Missing file → a fresh create; the diff shows the whole content added.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), true, false),
            // Any other read error: don't fail the write over a diff preview.
            Err(_) => (String::new(), false, true),
        };
    let diff = if elide_diff {
        crate::render::diff::DisplayDiff {
            display_diff: format!(
                "[diff preview skipped: existing file exceeds the {}-byte cap]",
                MAX_FILE_READ_BYTES
            ),
            added: 0,
            removed: 0,
            truncated: true,
        }
    } else {
        crate::render::diff::generate_display_diff(&old_content, content)
    };
    let line_count = write_one_blocking(workdir, rel, content)?;
    Ok(WriteResult {
        line_count,
        created,
        diff,
    })
}

pub(super) async fn mutation_policy_outcome(
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

pub(super) fn after_file_mutation(ctx: &ExecContext, tool: &str, path: &str) {
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

fn err(msg: &str, duration_secs: f64) -> ToolOutcome {
    ToolOutcome::error(msg, duration_secs)
}

fn plural(count: usize, singular: &'static str, plural: &'static str) -> &'static str {
    if count == 1 { singular } else { plural }
}

pub(super) fn diff_summary(added: usize, removed: usize, duration_secs: f64) -> String {
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
        assert!(outcome.is_success(), "expected success: {:?}", outcome);
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
            other => panic!("expected ReadFile metadata, got {:?}", other),
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
        assert!(outcome.is_success(), "expected success: {:?}", outcome);
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
            other => panic!("expected WriteFile metadata, got {:?}", other),
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
        assert!(outcome.is_success(), "expected success: {:?}", outcome);
        match &outcome.metadata.detail {
            ToolMetadata::ReadFile { truncated, .. } => assert!(
                !truncated,
                "a file whose content contains the marker must not be flagged truncated"
            ),
            other => panic!("expected ReadFile metadata, got {:?}", other),
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
        assert!(outcome.is_success(), "expected success: {:?}", outcome);
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
