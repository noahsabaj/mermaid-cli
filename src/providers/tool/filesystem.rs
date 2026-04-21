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
use crate::domain::ToolOutcome;

use super::super::ctx::{ExecContext, ProgressEvent};
use super::ToolExecutor;

/// `read_file` — read one or more files and return their contents
/// joined with section markers (matches the v0.6 multi-file output
/// shape so the model's prompt-engineering expectations carry over).
pub struct ReadFileTool;

#[async_trait]
impl ToolExecutor for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let paths = match extract_paths(&args) {
            Ok(p) => p,
            Err(e) => {
                return ToolOutcome::Error {
                    error: e,
                    duration_secs: 0.0,
                };
            },
        };
        if paths.is_empty() {
            return ToolOutcome::Error {
                error: "read_file requires at least one path".to_string(),
                duration_secs: 0.0,
            };
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
                    return ToolOutcome::Cancelled;
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
                            return ToolOutcome::Error {
                                error: format!("{}: {}", raw_path, e),
                                duration_secs: start.elapsed().as_secs_f64(),
                            };
                        },
                    }
                },
            }
        }

        ToolOutcome::Finished {
            output: combined,
            images: None,
            duration_secs: start.elapsed().as_secs_f64(),
        }
    }
}

/// `edit_file` — exact-match string replacement. Used for targeted
/// edits rather than full file rewrites. Errors if the `old_string`
/// doesn't appear exactly once (matching v0.6 semantics).
pub struct EditFileTool;

#[async_trait]
impl ToolExecutor for EditFileTool {
    fn name(&self) -> &'static str {
        "edit_file"
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
        let abs = resolve_path(&ctx.workdir, raw_path);
        let old_owned = old_string.to_string();
        let new_owned = new_string.to_string();
        let abs_clone = abs.clone();
        let display_path = raw_path.to_string();

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::Cancelled,
            result = tokio::task::spawn_blocking(move || edit_blocking(&abs_clone, &old_owned, &new_owned)) => {
                match result {
                    Ok(Ok(replacements)) => ToolOutcome::Finished {
                        output: format!("Edited {} ({} replacement{})",
                            display_path,
                            replacements,
                            if replacements == 1 { "" } else { "s" }),
                        images: None,
                        duration_secs: start.elapsed().as_secs_f64(),
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

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let Some(raw_path) = args.get("path").and_then(|v| v.as_str()) else {
            return err("delete_file requires 'path'", 0.0);
        };
        let start = std::time::Instant::now();
        let abs = resolve_path(&ctx.workdir, raw_path);
        let display = raw_path.to_string();

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::Cancelled,
            result = tokio::task::spawn_blocking(move || std::fs::remove_file(&abs)) => {
                match result {
                    Ok(Ok(())) => ToolOutcome::Finished {
                        output: format!("Deleted {}", display),
                        images: None,
                        duration_secs: start.elapsed().as_secs_f64(),
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

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let Some(raw_path) = args.get("path").and_then(|v| v.as_str()) else {
            return err("create_directory requires 'path'", 0.0);
        };
        let start = std::time::Instant::now();
        let abs = resolve_path(&ctx.workdir, raw_path);
        let display = raw_path.to_string();

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::Cancelled,
            result = tokio::task::spawn_blocking(move || std::fs::create_dir_all(&abs)) => {
                match result {
                    Ok(Ok(())) => ToolOutcome::Finished {
                        output: format!("Created directory {}", display),
                        images: None,
                        duration_secs: start.elapsed().as_secs_f64(),
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

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
            return ToolOutcome::Error {
                error: "write_file requires 'path' (string)".to_string(),
                duration_secs: 0.0,
            };
        };
        let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
            return ToolOutcome::Error {
                error: "write_file requires 'content' (string)".to_string(),
                duration_secs: 0.0,
            };
        };

        let start = std::time::Instant::now();
        let abs_path = resolve_path(&ctx.workdir, path);
        let content = content.to_string();

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::Cancelled,
            result = tokio::task::spawn_blocking(move || write_one_blocking(&abs_path, &content)) => {
                match result {
                    Ok(Ok(line_count)) => ToolOutcome::Finished {
                        output: format!("Wrote {} ({} lines)", path, line_count),
                        images: None,
                        duration_secs: start.elapsed().as_secs_f64(),
                    },
                    Ok(Err(e)) => ToolOutcome::Error {
                        error: format!("write_file({}): {}", path, e),
                        duration_secs: start.elapsed().as_secs_f64(),
                    },
                    Err(e) => ToolOutcome::Error {
                        error: format!("write_file join error: {}", e),
                        duration_secs: start.elapsed().as_secs_f64(),
                    },
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
        let mut out = Vec::with_capacity(arr.len());
        for v in arr {
            let Some(s) = v.as_str() else {
                return Err(
                    "read_file 'paths' must be an array of strings".to_string()
                );
            };
            out.push(s.to_string());
        }
        return Ok(out);
    }
    Err("read_file requires 'path' or 'paths'".to_string())
}

fn resolve_path(workdir: &Path, raw: &str) -> PathBuf {
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else {
        workdir.join(p)
    }
}

async fn read_one(workdir: &Path, raw: &str) -> std::io::Result<String> {
    let abs = resolve_path(workdir, raw);
    let abs_clone = abs.clone();
    let content = tokio::task::spawn_blocking(move || {
        let data = std::fs::read(&abs_clone)?;
        if data.len() > MAX_FILE_READ_BYTES {
            // Match v0.6 truncation shape — char-boundary-safe.
            let mut s = String::from_utf8_lossy(&data).into_owned();
            let cut = s.floor_char_boundary(MAX_FILE_READ_BYTES);
            s.truncate(cut);
            s.push_str("\n\n[TRUNCATED: file exceeded read cap]");
            Ok::<_, std::io::Error>(s)
        } else {
            Ok(String::from_utf8_lossy(&data).into_owned())
        }
    })
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))??;
    let _ = abs;
    Ok(content)
}

fn write_one_blocking(path: &Path, content: &str) -> std::io::Result<usize> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(content.lines().count())
}

fn edit_blocking(path: &Path, old_string: &str, new_string: &str) -> std::io::Result<usize> {
    let current = std::fs::read_to_string(path)?;
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
    std::fs::write(path, updated)?;
    Ok(1)
}

fn err(msg: &str, duration_secs: f64) -> ToolOutcome {
    ToolOutcome::Error {
        error: msg.to_string(),
        duration_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ToolCallId, TurnId};
    use crate::providers::ctx::test_exec_context;
    use std::fs;

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
            .execute(
                serde_json::json!({"path": "a.txt"}),
                ctx,
            )
            .await;
        match outcome {
            ToolOutcome::Finished { output, .. } => assert_eq!(output, "hello"),
            _ => panic!("expected Finished"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_missing_path_errors() {
        let dir = temp_root("read_missing_path");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = ReadFileTool
            .execute(serde_json::json!({}), ctx)
            .await;
        assert!(matches!(outcome, ToolOutcome::Error { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_nonexistent_errors() {
        let dir = temp_root("read_nonex");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = ReadFileTool
            .execute(
                serde_json::json!({"path": "does_not_exist.txt"}),
                ctx,
            )
            .await;
        assert!(matches!(outcome, ToolOutcome::Error { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_with_multiple_paths_joins_contents() {
        let dir = temp_root("read_multi");
        fs::write(dir.join("a.txt"), "alpha").expect("write");
        fs::write(dir.join("b.txt"), "beta").expect("write");
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let outcome = ReadFileTool
            .execute(
                serde_json::json!({"paths": ["a.txt", "b.txt"]}),
                ctx,
            )
            .await;
        match outcome {
            ToolOutcome::Finished { output, .. } => {
                assert!(output.contains("=== a.txt ==="));
                assert!(output.contains("alpha"));
                assert!(output.contains("=== b.txt ==="));
                assert!(output.contains("beta"));
            },
            _ => panic!("expected Finished"),
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
        assert!(matches!(outcome, ToolOutcome::Cancelled));
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
        match outcome {
            ToolOutcome::Finished { output, .. } => {
                assert!(output.contains("3 lines"));
            },
            _ => panic!("expected Finished"),
        }
        let written = fs::read_to_string(dir.join("out.txt")).expect("read");
        assert!(written.contains("line1"));
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
        assert!(matches!(outcome, ToolOutcome::Finished { .. }));
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
        assert!(matches!(outcome, ToolOutcome::Error { .. }));
        let _ = fs::remove_dir_all(&dir);
    }
}
