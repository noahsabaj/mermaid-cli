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

use crate::domain::{ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata};
use crate::render::diff::{DisplayDiff, MAX_DISPLAY_DIFF_LINES, generate_display_diff};
use mermaid_model::constants::MAX_PATCH_FILE_BYTES;
// The pure patch engine (parser + graduated fuzzy matcher + applier) lives in
// the runtime crate so the approval-replay path can reuse it without duplication.
use mermaid_runtime::apply_patch::{Hunk, UpdateFileChunk, derive_new_contents, parse_patch};

use super::super::ctx::ExecContext;
use super::ToolExecutor;
use super::filesystem::{MutationGate, after_file_mutation, diff_summary, mutation_policy_outcome};
use super::path_safety::{AllowedRoots, ResolvedInRoot, resolve_in_roots};

const APPLY_PATCH_DESCRIPTION: &str = "Edit files with a patch. Pass `patch` as one string in this exact envelope:\n*** Begin Patch\n*** Update File: <path>\n@@ <optional anchor line, e.g. a function signature>\n <unchanged context line>\n-<line to remove>\n+<line to add>\n*** End Patch\nUse '*** Add File: <path>' then '+'-prefixed lines to create a file; '*** Delete File: <path>' to remove one; '*** Move to: <path>' immediately after an Update File line to rename. Include a few unchanged context lines (prefixed with a space) around each change so the edit can be located; matching tolerates whitespace/quote drift. Paths must resolve inside the project directory or the session scratchpad.";

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
        let (ops, paths) = match plan_ops(&ctx, &hunks) {
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
        // Only project files are checkpointable; the gate bypasses entirely
        // when EVERY hunk lands in the session scratchpad.
        let plan_write = match mutation_policy_outcome(
            &ctx,
            "apply_patch",
            &summary_path,
            &paths.project,
            pending_action,
            paths.all_scratch,
        )
        .await
        {
            MutationGate::Blocked(outcome) => return *outcome,
            MutationGate::Proceed { plan_write } => plan_write,
        };

        // Serialize writers to every affected path (sorted ⇒ deadlock-free),
        // raced against cancellation so a contended lock stays responsive.
        let _guards = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            g = super::path_lock::lock_paths(&paths.all) => g,
        };
        // Scratchpad files are session-private and ephemeral — checkpoint only
        // the project-rooted subset, and skip entirely when there is none.
        if ctx.config.safety.checkpoint_on_mutation
            && !paths.project.is_empty()
            && let Err(e) = mermaid_runtime::create_checkpoint_for_task(
                &ctx.workdir,
                &paths.project,
                Some(serde_json::json!({ "tool": "apply_patch" })),
                ctx.checkpoint_origin(),
            )
        {
            return ToolOutcome::error(format!("apply_patch checkpoint failed: {e}"), 0.0);
        }

        let report = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            result = tokio::task::spawn_blocking(move || apply_all_blocking(&ops)) => {
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
        build_outcome(report, start.elapsed().as_secs_f64(), plan_write)
    }
}

/// One resolved file operation, ready to apply under the confined pathguard.
/// Each op carries the root it resolved into (project workdir or session
/// scratchpad); `rel` paths are relative to that root.
enum PlannedOp {
    Add {
        root: PathBuf,
        rel: PathBuf,
        display: String,
        contents: String,
    },
    Delete {
        root: PathBuf,
        rel: PathBuf,
        display: String,
    },
    Update {
        src_root: PathBuf,
        src_rel: PathBuf,
        dst_root: PathBuf,
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

/// Path bookkeeping for one patch: every affected canonical path (for
/// locking), the project-rooted subset (for gating + checkpointing), and
/// whether every hunk landed in the session scratchpad (which ungates the
/// mutation, see `mutation_policy_outcome`).
struct PatchPaths {
    all: Vec<PathBuf>,
    project: Vec<PathBuf>,
    all_scratch: bool,
}

/// Resolve each hunk's path(s) into root-relative ops (project workdir or
/// session scratchpad), rejecting any escape, and collect the sorted,
/// de-duplicated absolute paths.
fn plan_ops(ctx: &ExecContext, hunks: &[Hunk]) -> Result<(Vec<PlannedOp>, PatchPaths), String> {
    let roots = AllowedRoots::new(&ctx.workdir, ctx.scratchpad.as_deref());
    let mut ops = Vec::new();
    let mut all: Vec<PathBuf> = Vec::new();
    let mut project: Vec<PathBuf> = Vec::new();
    fn remember(r: &ResolvedInRoot, all: &mut Vec<PathBuf>, project: &mut Vec<PathBuf>) {
        if !all.contains(&r.abs) {
            all.push(r.abs.clone());
        }
        if !r.in_scratchpad && !project.contains(&r.abs) {
            project.push(r.abs.clone());
        }
    }
    for hunk in hunks {
        match hunk {
            Hunk::AddFile { path, contents } => {
                let raw = path.to_string_lossy().to_string();
                let resolved = resolve_in_roots(&roots, &raw)?;
                remember(&resolved, &mut all, &mut project);
                ops.push(PlannedOp::Add {
                    root: resolved.root,
                    rel: resolved.rel,
                    display: raw,
                    contents: contents.clone(),
                });
            },
            Hunk::DeleteFile { path } => {
                let raw = path.to_string_lossy().to_string();
                let resolved = resolve_in_roots(&roots, &raw)?;
                remember(&resolved, &mut all, &mut project);
                ops.push(PlannedOp::Delete {
                    root: resolved.root,
                    rel: resolved.rel,
                    display: raw,
                });
            },
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                let raw = path.to_string_lossy().to_string();
                let src = resolve_in_roots(&roots, &raw)?;
                remember(&src, &mut all, &mut project);
                let (dst_root, dst_rel, dst_display) = match move_path {
                    Some(mv) => {
                        let mv_raw = mv.to_string_lossy().to_string();
                        let dst = resolve_in_roots(&roots, &mv_raw)?;
                        remember(&dst, &mut all, &mut project);
                        (dst.root, dst.rel, mv_raw)
                    },
                    None => (src.root.clone(), src.rel.clone(), raw.clone()),
                };
                ops.push(PlannedOp::Update {
                    src_root: src.root,
                    src_rel: src.rel,
                    dst_root,
                    dst_rel,
                    src_display: raw,
                    dst_display,
                    chunks: chunks.clone(),
                });
            },
        }
    }
    all.sort();
    project.sort();
    let all_scratch = !all.is_empty() && project.is_empty();
    Ok((
        ops,
        PatchPaths {
            all,
            project,
            all_scratch,
        },
    ))
}

/// Apply every planned op under its resolved root, accumulating a report +
/// display diff.
fn apply_all_blocking(ops: &[PlannedOp]) -> Result<ApplyReport, String> {
    let mut report = ApplyReport::default();
    for op in ops {
        match op {
            PlannedOp::Add {
                root,
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
                ensure_parent(root, rel)?;
                mermaid_runtime::write_atomic_beneath(root, rel, body.as_bytes())
                    .map_err(|e| format!("{display}: {e}"))?;
                report.added.push(display.clone());
                report.push_diff(&format!("A {display}"), &generate_display_diff("", &body));
            },
            PlannedOp::Delete { root, rel, display } => {
                mermaid_runtime::remove_file_beneath(root, rel)
                    .map_err(|e| format!("{display}: {e}"))?;
                report.deleted.push(display.clone());
                report.push_line(format!("=== D {display} ==="));
            },
            PlannedOp::Update {
                src_root,
                src_rel,
                dst_root,
                dst_rel,
                src_display,
                dst_display,
                chunks,
            } => {
                let original = read_capped_beneath(src_root, src_rel, MAX_PATCH_FILE_BYTES)
                    .map_err(|e| format!("{src_display}: {e}"))?
                    .ok_or_else(|| {
                        format!(
                            "{src_display}: file too large to patch safely (> {MAX_PATCH_FILE_BYTES} bytes)"
                        )
                    })?;
                let applied = derive_new_contents(&original, chunks)
                    .map_err(|e| format!("{dst_display}: {e}"))?;
                report.fuzzy |= applied.fuzzy;
                ensure_parent(dst_root, dst_rel)?;
                mermaid_runtime::write_atomic_beneath(
                    dst_root,
                    dst_rel,
                    applied.new_contents.as_bytes(),
                )
                .map_err(|e| format!("{dst_display}: {e}"))?;
                let diff = generate_display_diff(&original, &applied.new_contents);
                if src_root == dst_root && src_rel == dst_rel {
                    report.modified.push(dst_display.clone());
                    report.push_diff(&format!("M {dst_display}"), &diff);
                } else {
                    mermaid_runtime::remove_file_beneath(src_root, src_rel)
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

fn ensure_parent(root: &Path, rel: &Path) -> Result<(), String> {
    if let Some(parent) = rel.parent()
        && !parent.as_os_str().is_empty()
    {
        mermaid_runtime::create_dir_all_beneath(root, parent)
            .map_err(|e| format!("{}: {e}", rel.display()))?;
    }
    Ok(())
}

/// Bounded read of `rel` beneath `root` via the confined helper. Returns
/// `None` when the file exceeds `cap` (so the caller refuses rather than patch a
/// partially-read file).
fn read_capped_beneath(root: &Path, rel: &Path, cap: usize) -> std::io::Result<Option<String>> {
    use std::io::Read;
    let file = mermaid_runtime::open_beneath(root, rel, mermaid_runtime::OpenIntent::Read)?;
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

fn build_outcome(report: ApplyReport, duration_secs: f64, plan_write: bool) -> ToolOutcome {
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
        lines_added: report.added_lines,
        lines_removed: report.removed_lines,
        plan_file_written: plan_write,
        ..ToolRunMetadata::default()
    })
}

#[cfg(test)]
mod tests;
