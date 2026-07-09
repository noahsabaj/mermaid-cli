//! `apply_patch` — multi-hunk, context-anchored file editing with a graduated
//! fuzzy matcher, adapted from OpenAI Codex's `apply-patch` crate. This is
//! Mermaid's sole file editor: it replaced the brittle exact-match `edit_file`,
//! which failed on any whitespace or curly-quote drift.
//!
//! The parser/matcher/apply logic lives in the submodules; this file is the
//! `ToolExecutor` glue — resolve + lock + checkpoint + apply + render — reusing
//! the same safety gate, per-path write lock, shadow-git checkpoint, confined
//! atomic writes, and diff renderer as the other filesystem tools.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::constants::MAX_PATCH_FILE_BYTES;
use crate::domain::{ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata};
use crate::render::diff::{DisplayDiff, MAX_DISPLAY_DIFF_LINES, generate_display_diff};
// The pure patch engine (parser + graduated fuzzy matcher + applier) lives in
// the runtime crate so the approval-replay path can reuse it without duplication.
use crate::runtime::apply_patch::{Hunk, UpdateFileChunk, derive_new_contents, parse_patch};

use super::super::ctx::ExecContext;
use super::ToolExecutor;
use super::filesystem::{after_file_mutation, diff_summary, mutation_policy_outcome};
use super::path_safety::{relative_within, resolve_path_safe};

const APPLY_PATCH_DESCRIPTION: &str = "Edit files with a patch. Pass `patch` as one string in this exact envelope:\n*** Begin Patch\n*** Update File: <path>\n@@ <optional anchor line, e.g. a function signature>\n <unchanged context line>\n-<line to remove>\n+<line to add>\n*** End Patch\nUse '*** Add File: <path>' then '+'-prefixed lines to create a file; '*** Delete File: <path>' to remove one; '*** Move to: <path>' immediately after an Update File line to rename. Include a few unchanged context lines (prefixed with a space) around each change so the edit can be located; matching tolerates whitespace/quote drift.";

/// The `apply_patch` tool: apply a `*** Begin Patch … *** End Patch` envelope.
pub struct ApplyPatchTool;

#[async_trait]
impl ToolExecutor for ApplyPatchTool {
    fn name(&self) -> &'static str {
        "apply_patch"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch".to_string(),
            description: APPLY_PATCH_DESCRIPTION.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "The full patch envelope, from '*** Begin Patch' to '*** End Patch'."
                    }
                },
                "required": ["patch"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let start = std::time::Instant::now();
        let Some(patch) = args.get("patch").and_then(|v| v.as_str()) else {
            return ToolOutcome::error("apply_patch requires 'patch' (string)", 0.0);
        };
        let hunks = match parse_patch(patch) {
            Ok(h) => h,
            Err(e) => return ToolOutcome::error(format!("apply_patch: {e}"), 0.0),
        };
        let (ops, abs_paths) = match plan_ops(&ctx, &hunks) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::error(format!("apply_patch: {e}"), 0.0),
        };

        let summary_path = ops
            .first()
            .map(PlannedOp::display)
            .unwrap_or_default()
            .to_string();
        let pending_action = serde_json::json!({
            "tool": "apply_patch",
            "args": { "patch": patch },
            "workdir": ctx.workdir.display().to_string(),
            "turn_id": ctx.turn.0,
            "call_id": ctx.call_id.0,
            "task_id": ctx.task_id.clone(),
        });
        if let Some(outcome) = mutation_policy_outcome(
            &ctx,
            "apply_patch",
            &summary_path,
            &abs_paths,
            pending_action,
        )
        .await
        {
            return outcome;
        }

        // Serialize writers to every affected path (sorted ⇒ deadlock-free),
        // raced against cancellation so a contended lock stays responsive.
        let _guards = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            g = super::path_lock::lock_paths(&abs_paths) => g,
        };
        if ctx.config.safety.checkpoint_on_mutation
            && let Err(e) = crate::runtime::create_checkpoint_for_task(
                &ctx.workdir,
                &abs_paths,
                Some(serde_json::json!({ "tool": "apply_patch" })),
                ctx.task_id.clone(),
            )
        {
            return ToolOutcome::error(format!("apply_patch checkpoint failed: {e}"), 0.0);
        }

        let workdir = ctx.workdir.clone();
        let report = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            result = tokio::task::spawn_blocking(move || apply_all_blocking(&workdir, &ops)) => {
                match result {
                    Ok(Ok(report)) => report,
                    Ok(Err(e)) => {
                        return ToolOutcome::error(format!("apply_patch: {e}"), start.elapsed().as_secs_f64());
                    },
                    Err(e) => {
                        return ToolOutcome::error(format!("apply_patch join error: {e}"), start.elapsed().as_secs_f64());
                    },
                }
            }
        };
        after_file_mutation(&ctx, "apply_patch", &summary_path);
        build_outcome(report, start.elapsed().as_secs_f64())
    }
}

/// One resolved file operation, ready to apply under the confined pathguard.
enum PlannedOp {
    Add {
        rel: PathBuf,
        display: String,
        contents: String,
    },
    Delete {
        rel: PathBuf,
        display: String,
    },
    Update {
        src_rel: PathBuf,
        dst_rel: PathBuf,
        src_display: String,
        dst_display: String,
        chunks: Vec<UpdateFileChunk>,
    },
}

impl PlannedOp {
    fn display(&self) -> &str {
        match self {
            PlannedOp::Add { display, .. } | PlannedOp::Delete { display, .. } => display,
            PlannedOp::Update { dst_display, .. } => dst_display,
        }
    }
}

/// Resolve each hunk's path(s) into workdir-relative ops, rejecting any escape,
/// and collect the sorted, de-duplicated absolute paths (for locking + one
/// checkpoint).
fn plan_ops(ctx: &ExecContext, hunks: &[Hunk]) -> Result<(Vec<PlannedOp>, Vec<PathBuf>), String> {
    let mut ops = Vec::new();
    let mut abs: Vec<PathBuf> = Vec::new();
    let remember = |p: PathBuf, set: &mut Vec<PathBuf>| {
        if !set.contains(&p) {
            set.push(p);
        }
    };
    for hunk in hunks {
        match hunk {
            Hunk::AddFile { path, contents } => {
                let raw = path.to_string_lossy().to_string();
                remember(resolve_path_safe(&ctx.workdir, &raw)?, &mut abs);
                ops.push(PlannedOp::Add {
                    rel: relative_within(&ctx.workdir, &raw)?,
                    display: raw,
                    contents: contents.clone(),
                });
            },
            Hunk::DeleteFile { path } => {
                let raw = path.to_string_lossy().to_string();
                remember(resolve_path_safe(&ctx.workdir, &raw)?, &mut abs);
                ops.push(PlannedOp::Delete {
                    rel: relative_within(&ctx.workdir, &raw)?,
                    display: raw,
                });
            },
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                let raw = path.to_string_lossy().to_string();
                remember(resolve_path_safe(&ctx.workdir, &raw)?, &mut abs);
                let src_rel = relative_within(&ctx.workdir, &raw)?;
                let (dst_rel, dst_display) = match move_path {
                    Some(mv) => {
                        let mv_raw = mv.to_string_lossy().to_string();
                        remember(resolve_path_safe(&ctx.workdir, &mv_raw)?, &mut abs);
                        (relative_within(&ctx.workdir, &mv_raw)?, mv_raw)
                    },
                    None => (src_rel.clone(), raw.clone()),
                };
                ops.push(PlannedOp::Update {
                    src_rel,
                    dst_rel,
                    src_display: raw,
                    dst_display,
                    chunks: chunks.clone(),
                });
            },
        }
    }
    abs.sort();
    Ok((ops, abs))
}

/// Apply every planned op under `workdir`, accumulating a report + display diff.
fn apply_all_blocking(workdir: &Path, ops: &[PlannedOp]) -> Result<ApplyReport, String> {
    let mut report = ApplyReport::default();
    for op in ops {
        match op {
            PlannedOp::Add {
                rel,
                display,
                contents,
            } => {
                // A created file ends with a trailing newline (POSIX text
                // convention), matching how the update path re-adds one.
                let body = if contents.is_empty() || contents.ends_with('\n') {
                    contents.clone()
                } else {
                    format!("{contents}\n")
                };
                ensure_parent(workdir, rel)?;
                crate::runtime::write_atomic_beneath(workdir, rel, body.as_bytes())
                    .map_err(|e| format!("{display}: {e}"))?;
                report.added.push(display.clone());
                report.push_diff(&format!("A {display}"), &generate_display_diff("", &body));
            },
            PlannedOp::Delete { rel, display } => {
                crate::runtime::remove_file_beneath(workdir, rel)
                    .map_err(|e| format!("{display}: {e}"))?;
                report.deleted.push(display.clone());
                report.push_line(format!("=== D {display} ==="));
            },
            PlannedOp::Update {
                src_rel,
                dst_rel,
                src_display,
                dst_display,
                chunks,
            } => {
                let original = read_capped_beneath(workdir, src_rel, MAX_PATCH_FILE_BYTES)
                    .map_err(|e| format!("{src_display}: {e}"))?
                    .ok_or_else(|| {
                        format!(
                            "{src_display}: file too large to patch safely (> {MAX_PATCH_FILE_BYTES} bytes)"
                        )
                    })?;
                let applied = derive_new_contents(&original, chunks)
                    .map_err(|e| format!("{dst_display}: {e}"))?;
                report.fuzzy |= applied.fuzzy;
                ensure_parent(workdir, dst_rel)?;
                crate::runtime::write_atomic_beneath(
                    workdir,
                    dst_rel,
                    applied.new_contents.as_bytes(),
                )
                .map_err(|e| format!("{dst_display}: {e}"))?;
                let diff = generate_display_diff(&original, &applied.new_contents);
                if src_rel == dst_rel {
                    report.modified.push(dst_display.clone());
                    report.push_diff(&format!("M {dst_display}"), &diff);
                } else {
                    crate::runtime::remove_file_beneath(workdir, src_rel)
                        .map_err(|e| format!("{src_display}: {e}"))?;
                    report
                        .renamed
                        .push((src_display.clone(), dst_display.clone()));
                    report.push_diff(&format!("R {src_display} -> {dst_display}"), &diff);
                }
            },
        }
    }
    Ok(report)
}

fn ensure_parent(workdir: &Path, rel: &Path) -> Result<(), String> {
    if let Some(parent) = rel.parent()
        && !parent.as_os_str().is_empty()
    {
        crate::runtime::create_dir_all_beneath(workdir, parent)
            .map_err(|e| format!("{}: {e}", rel.display()))?;
    }
    Ok(())
}

/// Bounded read of `rel` beneath `workdir` via the confined helper. Returns
/// `None` when the file exceeds `cap` (so the caller refuses rather than patch a
/// partially-read file).
fn read_capped_beneath(workdir: &Path, rel: &Path, cap: usize) -> std::io::Result<Option<String>> {
    use std::io::Read;
    let file = crate::runtime::open_beneath(workdir, rel, crate::runtime::OpenIntent::Read)?;
    let mut buf = Vec::new();
    file.take(cap as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() > cap {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// Accumulated result of applying a patch: which files changed, whether any
/// hunk matched fuzzily, and a bounded concatenated display diff.
#[derive(Default)]
struct ApplyReport {
    added: Vec<String>,
    modified: Vec<String>,
    deleted: Vec<String>,
    renamed: Vec<(String, String)>,
    fuzzy: bool,
    added_lines: usize,
    removed_lines: usize,
    diff_lines: Vec<String>,
    diff_truncated: bool,
}

impl ApplyReport {
    fn push_line(&mut self, line: String) {
        if self.diff_lines.len() < MAX_DISPLAY_DIFF_LINES {
            self.diff_lines.push(line);
        } else {
            self.diff_truncated = true;
        }
    }

    fn push_diff(&mut self, header: &str, diff: &DisplayDiff) {
        self.added_lines += diff.added;
        self.removed_lines += diff.removed;
        self.push_line(format!("=== {header} ==="));
        for line in diff.display_diff.lines() {
            self.push_line(line.to_string());
        }
        self.diff_truncated |= diff.truncated;
    }
}

fn build_outcome(report: ApplyReport, duration_secs: f64) -> ToolOutcome {
    let total =
        report.added.len() + report.modified.len() + report.deleted.len() + report.renamed.len();
    let mut lines = vec![format!("Applied patch: {total} file(s)")];
    lines.extend(report.added.iter().map(|p| format!("A {p}")));
    lines.extend(report.modified.iter().map(|p| format!("M {p}")));
    lines.extend(report.renamed.iter().map(|(a, b)| format!("R {a} -> {b}")));
    lines.extend(report.deleted.iter().map(|p| format!("D {p}")));
    if report.fuzzy {
        lines.push(
            "note: one or more hunks matched with fuzzy (whitespace/Unicode) context; verify the result."
                .to_string(),
        );
    }
    let model_content = lines.join("\n");
    ToolOutcome::success(
        model_content,
        diff_summary(report.added_lines, report.removed_lines, duration_secs),
        duration_secs,
    )
    .with_metadata(ToolRunMetadata {
        detail: ToolMetadata::ApplyPatch {
            added: report.added,
            modified: report.modified,
            deleted: report.deleted,
            renamed: report.renamed,
            fuzzy: report.fuzzy,
        },
        display_diff: Some(report.diff_lines.join("\n")),
        diff_truncated: report.diff_truncated,
        ..ToolRunMetadata::default()
    })
}

#[cfg(test)]
mod tests;
