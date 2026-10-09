//! `background_process` — the model's handle on the processes `execute_command`
//! left running (`mode="background"`, or a foreground command the user moved
//! to the background with Ctrl+B).
//!
//! Before this tool the model could start a long job but not follow it: a
//! foreground command is killed at `COMMAND_MAX_TIMEOUT_SECS`, and `/logs` and
//! `/stop` are the user's commands. A model watching a long build or test run
//! had to poll with `sleep` and `cat` of the log path.
//!
//! Scope is deliberately narrow: the tool reaches only processes this Mermaid
//! process started, recorded in [`JOBS`] by [`track`] when `execute_command`
//! returns, and only those of the calling session. It never takes a raw pid
//! and never reads the runtime store, so it cannot signal a process someone
//! else started or one a tampered database row names.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use mermaid_domain::{ToolDefinition, ToolOutcome};

use super::super::super::ctx::ExecContext;
use super::super::ToolExecutor;
use super::background::process_running;

/// Most output one `read` / `wait` / `stop` returns. A chatty job can write
/// megabytes between reads; the newest bytes are the ones that matter, and the
/// full log stays on disk at the path the result names.
const OUTPUT_MAX_BYTES: usize = 32 * 1024;

/// `wait` default and ceiling. The ceiling is an hour: long enough for a real
/// build or training run, short enough that a forgotten wait still returns.
const WAIT_DEFAULT_SECS: u64 = 300;
const WAIT_MAX_SECS: u64 = 3600;

/// How often `wait` checks the process and its log.
const WAIT_POLL: Duration = Duration::from_millis(500);

/// One process `execute_command` left running.
#[derive(Debug, Clone)]
struct Job {
    id: String,
    pid: u32,
    command: String,
    log_path: PathBuf,
    session_id: Option<String>,
    started: Instant,
    /// Log bytes already returned to the model; the next read starts here.
    read_offset: u64,
    /// Seen exited once. Never signal it again: the pid may be reused.
    exited: bool,
}

/// Every process this Mermaid process started, oldest first.
static JOBS: LazyLock<Mutex<Vec<Job>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn jobs() -> std::sync::MutexGuard<'static, Vec<Job>> {
    JOBS.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record the process an `execute_command` outcome left running, if any.
pub(crate) fn track(outcome: &ToolOutcome, session_id: Option<&str>) {
    if !outcome.is_success() {
        return;
    }
    let Some(process) = outcome.metadata.process.as_ref() else {
        return;
    };
    let job = Job {
        id: process.id.clone(),
        pid: process.pid,
        command: process.command.clone(),
        log_path: PathBuf::from(&process.log_path),
        session_id: session_id.map(str::to_string),
        started: Instant::now(),
        read_offset: 0,
        exited: false,
    };
    let mut jobs = jobs();
    jobs.retain(|existing| existing.id != job.id);
    jobs.push(job);
}

/// A job is visible to the session that started it. A context without a
/// session id (tests, some headless paths) sees and is seen by everyone.
fn visible(job: &Job, session_id: Option<&str>) -> bool {
    match (job.session_id.as_deref(), session_id) {
        (Some(owner), Some(caller)) => owner == caller,
        _ => true,
    }
}

/// Accept `bg-1234`, `1234`, or the number 1234.
fn job_id(args: &serde_json::Value) -> Option<String> {
    match args.get("id")? {
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                None
            } else if s.starts_with("bg-") {
                Some(s.to_string())
            } else {
                Some(format!("bg-{s}"))
            }
        },
        serde_json::Value::Number(n) => n.as_u64().map(|n| format!("bg-{n}")),
        _ => None,
    }
}

fn find(id: &str, session_id: Option<&str>) -> Result<Job, String> {
    jobs()
        .iter()
        .find(|job| job.id == id && visible(job, session_id))
        .cloned()
        .ok_or_else(|| {
            format!(
                "no background process {id} in this session; action \"list\" shows the ones you can reach"
            )
        })
}

fn update(id: &str, f: impl FnOnce(&mut Job)) {
    if let Some(job) = jobs().iter_mut().find(|job| job.id == id) {
        f(job);
    }
}

/// Whether the process still runs. A job once seen exited stays exited: the
/// pid could belong to an unrelated process by now.
async fn alive(job: &Job) -> bool {
    if job.exited {
        return false;
    }
    let running = process_running(job.pid).await;
    if !running {
        update(&job.id, |j| j.exited = true);
    }
    running
}

/// Read `path` from `offset` to its end. Returns the bytes and the new end.
/// A log shorter than `offset` was truncated or replaced, so read it whole.
async fn read_from(path: &Path, offset: u64) -> std::io::Result<(Vec<u8>, u64)> {
    let mut file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    let start = if offset > len { 0 } else { offset };
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let mut bytes = Vec::new();
    file.take(len - start).read_to_end(&mut bytes).await?;
    let end = start + bytes.len() as u64;
    Ok((bytes, end))
}

/// The output written since the last read, capped to the newest
/// [`OUTPUT_MAX_BYTES`], and advance the job's read offset past it.
async fn take_new_output(job: &Job) -> String {
    match read_from(&job.log_path, job.read_offset).await {
        Ok((bytes, end)) => {
            update(&job.id, |j| j.read_offset = end);
            if bytes.is_empty() {
                return "(no new output)".to_string();
            }
            let skipped = bytes.len().saturating_sub(OUTPUT_MAX_BYTES);
            let text = String::from_utf8_lossy(&bytes[skipped..]).into_owned();
            if skipped > 0 {
                format!(
                    "[{skipped} earlier bytes skipped; full log: {}]\n{text}",
                    job.log_path.display()
                )
            } else {
                text
            }
        },
        Err(error) => format!("(log {} unreadable: {error})", job.log_path.display()),
    }
}

fn status_line(job: &Job, running: bool) -> String {
    let state = if running {
        "running"
    } else {
        "exited (exit code not recorded)"
    };
    format!(
        "{} (pid {}): {state}, started {}s ago\nCommand: {}\nLog: {}",
        job.id,
        job.pid,
        job.started.elapsed().as_secs(),
        job.command,
        job.log_path.display()
    )
}

/// Status plus new output, the shape `read`, `wait` and `stop` all return.
async fn report(job: &Job, headline: Option<String>) -> String {
    let running = alive(job).await;
    let job = find(&job.id, job.session_id.as_deref()).unwrap_or_else(|_| job.clone());
    let mut out = String::new();
    if let Some(headline) = headline {
        out.push_str(&headline);
        out.push('\n');
    }
    out.push_str(&status_line(&job, running));
    out.push_str("\n\n--- new output ---\n");
    out.push_str(&take_new_output(&job).await);
    out
}

/// Keep the tail of `text` that could still begin a match of a pattern
/// `pattern_len` bytes long, cut on a char boundary.
fn match_carry(text: &str, pattern_len: usize) -> String {
    let mut cut = text.len().saturating_sub(pattern_len.saturating_sub(1));
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text[cut..].to_string()
}

enum WaitEnd {
    Exited,
    Matched,
    TimedOut,
    Cancelled,
}

async fn wait_for(
    job: &Job,
    pattern: Option<&str>,
    timeout: Duration,
    ctx: &ExecContext,
) -> WaitEnd {
    let deadline = Instant::now() + timeout;
    let mut scan = job.read_offset;
    let mut carry = String::new();
    loop {
        if let Some(pattern) = pattern
            && let Ok((bytes, end)) = read_from(&job.log_path, scan).await
        {
            scan = end;
            let text = format!("{carry}{}", String::from_utf8_lossy(&bytes));
            if text.contains(pattern) {
                return WaitEnd::Matched;
            }
            carry = match_carry(&text, pattern.len());
        }
        if !alive(job).await {
            return WaitEnd::Exited;
        }
        if Instant::now() >= deadline {
            return WaitEnd::TimedOut;
        }
        tokio::select! {
            () = ctx.token.cancelled() => return WaitEnd::Cancelled,
            () = tokio::time::sleep(WAIT_POLL.min(deadline.saturating_duration_since(Instant::now()))) => {},
        }
    }
}

pub struct BackgroundProcessTool;

#[async_trait]
impl ToolExecutor for BackgroundProcessTool {
    fn name(&self) -> &'static str {
        "background_process"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "background_process".to_string(),
            description:
                "Follow a process that execute_command left running (mode=\"background\", \
                or a command the user moved to the background). `read` returns the output written \
                since your last read and whether the process still runs. `wait` blocks until the \
                process exits, `pattern` appears in new output, or `timeout_secs` passes, then \
                returns the same as `read`; Esc ends it early. `stop` ends the process and its \
                children. `list` shows this session's processes. Only processes started in this \
                session can be reached, and their exit codes are not recorded."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "read", "wait", "stop"]
                    },
                    "id": {
                        "type": "string",
                        "description": "Process id from execute_command's result, such as \"bg-1234\". Not used by `list`."
                    },
                    "pattern": {
                        "type": "string",
                        "description": "`wait` only: return as soon as this text appears in output written after your last read."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": WAIT_MAX_SECS,
                        "description": "`wait` only: longest time to wait. Default 300."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    // No policy gate. `list`, `read` and `wait` only read Mermaid's own log
    // files. `stop` signals only a process this session started (see the
    // module doc), which puts the machine back where it was before that
    // already-approved start; the subagent tool's `kill` makes the same call.
    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let started = Instant::now();
        let secs = || Some(started.elapsed().as_secs_f64());
        let session_id = ctx.session_id.clone();
        let session = session_id.as_deref();
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");

        if action == "list" {
            let listed: Vec<Job> = jobs()
                .iter()
                .filter(|job| visible(job, session))
                .cloned()
                .collect();
            if listed.is_empty() {
                return ToolOutcome::success(
                    "No background processes in this session.",
                    "no processes",
                    started.elapsed().as_secs_f64(),
                );
            }
            let mut out = String::new();
            for job in &listed {
                let running = alive(job).await;
                out.push_str(&status_line(job, running));
                out.push_str("\n\n");
            }
            let count = listed.len();
            return ToolOutcome::success(
                out.trim_end().to_string(),
                format!("{count} process{}", if count == 1 { "" } else { "es" }),
                started.elapsed().as_secs_f64(),
            );
        }

        if !matches!(action, "read" | "wait" | "stop") {
            return ToolOutcome::error(
                format!(
                    "background_process: action must be list, read, wait or stop, got {action:?}"
                ),
                secs(),
            );
        }
        let Some(id) = job_id(&args) else {
            return ToolOutcome::error(
                format!("background_process: action \"{action}\" needs `id`, such as \"bg-1234\""),
                secs(),
            );
        };
        let job = match find(&id, session) {
            Ok(job) => job,
            Err(error) => return ToolOutcome::error(error, secs()),
        };

        let (output, summary) = match action {
            "read" => (report(&job, None).await, "read".to_string()),
            "wait" => {
                let timeout_secs = args
                    .get("timeout_secs")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(WAIT_DEFAULT_SECS)
                    .clamp(1, WAIT_MAX_SECS);
                let pattern = args
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .filter(|p| !p.is_empty());
                let headline =
                    match wait_for(&job, pattern, Duration::from_secs(timeout_secs), &ctx).await {
                        WaitEnd::Cancelled => return ToolOutcome::cancelled(),
                        WaitEnd::Exited => "Process exited.".to_string(),
                        WaitEnd::Matched => {
                            format!("Pattern {:?} appeared.", pattern.unwrap_or_default())
                        },
                        WaitEnd::TimedOut => format!("Still running after {timeout_secs}s."),
                    };
                let summary = headline.trim_end_matches('.').to_lowercase();
                (report(&job, Some(headline)).await, summary)
            },
            _ => {
                let headline = if alive(&job).await {
                    mermaid_model::utils::terminate_tree(
                        job.pid,
                        mermaid_model::utils::Grace::Graceful,
                    )
                    .await;
                    update(&job.id, |j| j.exited = true);
                    "Stopped."
                } else {
                    "Already exited; nothing to stop."
                };
                let summary = headline.trim_end_matches('.').to_lowercase();
                (report(&job, Some(headline.to_string())).await, summary)
            },
        };
        ToolOutcome::success(output, summary, started.elapsed().as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ctx::test_exec_context;
    use mermaid_domain::{ToolCallId, TurnId};

    fn ctx_for(session: &str) -> ExecContext {
        let (mut ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), std::env::temp_dir());
        ctx.session_id = Some(session.to_string());
        ctx
    }

    async fn call(session: &str, args: serde_json::Value) -> ToolOutcome {
        BackgroundProcessTool.execute(args, ctx_for(session)).await
    }

    /// Start `command` in the background through the real tool, so the job is
    /// tracked exactly as production tracks it. Returns its id.
    #[cfg(not(target_os = "windows"))]
    async fn start(session: &str, command: &str) -> String {
        let outcome = crate::providers::tool::exec::ExecuteCommandTool
            .execute(
                serde_json::json!({ "command": command, "mode": "background" }),
                ctx_for(session),
            )
            .await;
        assert!(outcome.is_success(), "background start failed: {outcome:?}");
        outcome
            .metadata
            .process
            .as_ref()
            .expect("process")
            .id
            .clone()
    }

    #[test]
    fn job_id_accepts_prefixed_bare_and_numeric_ids() {
        assert_eq!(
            job_id(&serde_json::json!({"id": "bg-12"})).as_deref(),
            Some("bg-12")
        );
        assert_eq!(
            job_id(&serde_json::json!({"id": "12"})).as_deref(),
            Some("bg-12")
        );
        assert_eq!(
            job_id(&serde_json::json!({"id": 12})).as_deref(),
            Some("bg-12")
        );
        assert_eq!(job_id(&serde_json::json!({"id": " "})), None);
        assert_eq!(job_id(&serde_json::json!({})), None);
    }

    #[test]
    fn match_carry_keeps_a_partial_match_on_a_char_boundary() {
        assert_eq!(match_carry("abcdef", 3), "ef");
        assert_eq!(match_carry("ab", 5), "ab");
        // "é" is two bytes; a cut inside it moves back to its start.
        assert_eq!(match_carry("xxé", 2), "é");
    }

    #[test]
    fn a_failed_or_foreground_outcome_is_not_tracked() {
        track(&ToolOutcome::error("boom", None), Some("s"));
        track(&ToolOutcome::success("done", "done", 0.0), Some("s"));
        assert!(
            jobs()
                .iter()
                .all(|job| job.session_id.as_deref() != Some("s"))
        );
    }

    #[tokio::test]
    async fn unknown_id_and_missing_id_are_errors() {
        let outcome = call(
            "unknown",
            serde_json::json!({"action": "read", "id": "bg-1"}),
        )
        .await;
        assert!(!outcome.is_success());
        assert!(
            outcome.error_message().unwrap().contains("list"),
            "{outcome:?}"
        );
        let outcome = call("unknown", serde_json::json!({"action": "stop"})).await;
        assert!(
            outcome.error_message().unwrap().contains("needs `id`"),
            "{outcome:?}"
        );
        let outcome = call(
            "unknown",
            serde_json::json!({"action": "kill", "id": "bg-1"}),
        )
        .await;
        assert!(
            outcome.error_message().unwrap().contains("must be"),
            "{outcome:?}"
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn read_returns_only_output_written_since_the_last_read() {
        let id = start("read", "echo first; sleep 1; echo second; exec sleep 30").await;
        let first = call(
            "read",
            serde_json::json!({"action": "wait", "id": id, "pattern": "first", "timeout_secs": 10}),
        )
        .await;
        assert!(first.output().contains("first"), "{first:?}");
        let second = call("read", serde_json::json!({"action": "wait", "id": id, "pattern": "second", "timeout_secs": 10})).await;
        let out = second.output();
        assert!(out.contains("Pattern \"second\" appeared."), "{out}");
        assert!(out.contains(": running"), "{out}");
        let (_, new_output) = out
            .split_once("--- new output ---")
            .expect("output section");
        assert!(
            new_output.contains("second") && !new_output.contains("first"),
            "{out}"
        );
        let again = call("read", serde_json::json!({"action": "read", "id": id})).await;
        assert!(again.output().contains("(no new output)"), "{again:?}");
        call("read", serde_json::json!({"action": "stop", "id": id})).await;
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn wait_returns_when_the_process_exits() {
        let id = start("exit", "sleep 2; echo finished").await;
        let outcome = call(
            "exit",
            serde_json::json!({"action": "wait", "id": id, "timeout_secs": 20}),
        )
        .await;
        let out = outcome.output();
        assert!(out.starts_with("Process exited."), "{out}");
        assert!(out.contains("finished"), "{out}");
        assert!(out.contains("exit code not recorded"), "{out}");
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn wait_times_out_while_the_process_runs() {
        let id = start("timeout", "exec sleep 30").await;
        let outcome = call(
            "timeout",
            serde_json::json!({"action": "wait", "id": id, "timeout_secs": 1}),
        )
        .await;
        assert!(
            outcome.output().starts_with("Still running after 1s."),
            "{outcome:?}"
        );
        call("timeout", serde_json::json!({"action": "stop", "id": id})).await;
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn stop_ends_the_process_once() {
        let id = start("stop", "exec sleep 30").await;
        let pid: u32 = id.trim_start_matches("bg-").parse().unwrap();
        let outcome = call("stop", serde_json::json!({"action": "stop", "id": id})).await;
        assert!(outcome.output().starts_with("Stopped."), "{outcome:?}");
        assert!(!process_running(pid).await, "process must be gone");
        let again = call("stop", serde_json::json!({"action": "stop", "id": id})).await;
        assert!(again.output().starts_with("Already exited"), "{again:?}");
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn another_session_cannot_see_or_stop_the_process() {
        let id = start("owner", "exec sleep 30").await;
        let outcome = call("intruder", serde_json::json!({"action": "stop", "id": id})).await;
        assert!(!outcome.is_success(), "{outcome:?}");
        let listed = call("intruder", serde_json::json!({"action": "list"})).await;
        assert!(!listed.output().contains(&id), "{listed:?}");
        let listed = call("owner", serde_json::json!({"action": "list"})).await;
        assert!(listed.output().contains(&id), "{listed:?}");
        call("owner", serde_json::json!({"action": "stop", "id": id})).await;
    }
}
