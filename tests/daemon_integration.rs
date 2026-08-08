//! Live integration exercise for the daemon / persistence hardening fixes whose
//! unit coverage is only sequential / in-process: the atomic approval claim
//! (#118), the restart reconcile (#120), the startup GC (#130), and the daemon
//! singleton flock (#131).
//!
//! `#118` runs in the default suite — it's deterministic (in-process threads).
//! The three `#[ignore]`d tests spawn a real `mermaidd` and assert its
//! on-startup wiring; run them with:
//!
//!     cargo test --test daemon_integration -- --ignored
//!
//! They are kept out of CI's default run so real-process timing can never flake
//! it. Everything is isolated under a unique `XDG_DATA_HOME`, so parallel runs
//! and a developer's real data dir never collide. Unix-only — the daemon is
//! `#[cfg(unix)]`.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use mermaid_runtime::{NewApproval, NewTask, RuntimeStore, TaskStatus};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Generous so a slow/loaded machine can't flake the readiness/exit polls; the
/// happy path resolves in well under a second.
const TIMEOUT: Duration = Duration::from_secs(20);

/// A fresh, empty base dir used as the daemon's `XDG_DATA_HOME`. Unique per call
/// (pid + counter) so parallel tests and reruns never share state.
fn fresh_base(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "mermaid_daemon_it_{tag}_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create temp base dir");
    base
}

/// `XDG_DATA_HOME=base` ⇒ the runtime lives in `base/mermaid/` — matching
/// `data_dir()` / `open_default()` (`ProjectDirs::from("","","mermaid")`) on
/// Linux. Returns `(db_path, socket_path)`.
fn data_paths(base: &Path) -> (PathBuf, PathBuf) {
    let dir = base.join("mermaid");
    (dir.join("runtime.sqlite3"), dir.join("mermaidd.sock"))
}

fn spawn_daemon(base: &Path, stderr_log: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_mermaidd"))
        .env("XDG_DATA_HOME", base)
        .env("HOME", base) // belt-and-suspenders: isolate any HOME-based fallback
        .env_remove("MERMAID_DAEMON_ENABLE_TCP") // never bind the shared TCP port
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(stderr_log).expect("create stderr log"),
        ))
        .spawn()
        .expect("spawn mermaidd")
}

/// Kills the daemon on drop so a panicking assertion never leaks a process.
/// `wait()` blocks until it's reaped, so any [`DirGuard`] declared *earlier*
/// (and thus dropped *later*) removes the data dir only after the daemon's files
/// are released.
struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Removes a test's base dir on drop — even if an assertion panics. Declare it
/// *before* any `DaemonGuard` so it drops last (after the daemons are dead).
struct DirGuard(PathBuf);
impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {},
            Err(_) => return None,
        }
        if start.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn make_approval(store: &RuntimeStore, action: &str) -> String {
    store
        .approvals()
        .create(NewApproval {
            task_id: None,
            proposed_action: action.to_string(),
            risk_classification: "Write".to_string(),
            policy_decision: "Ask".to_string(),
            args_summary: None,
            checkpoint_id: None,
            pending_action_json: None,
        })
        .expect("create approval")
        .id
}

#[test]
fn approval_claim_has_exactly_one_winner_under_thread_contention() {
    // #118: many processes can race `mermaid approve <id>`. The claim is an
    // atomic `UPDATE ... WHERE user_decision IS NULL`, so exactly one may win and
    // run the un-rollback-able effect. Unit tests only check this sequentially;
    // here N threads — each its own connection, so this is real SQLite
    // contention — hit the same id at once behind a barrier.
    let base = fresh_base("claim");
    let _dir = DirGuard(base.clone());
    let (db, _sock) = data_paths(&base);

    let id = {
        let store = RuntimeStore::open(&db).expect("open store");
        make_approval(&store, "write_file secret.txt")
    };

    const THREADS: usize = 8;
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let db = db.clone();
            let id = id.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let store = RuntimeStore::open(&db).expect("open store in thread");
                barrier.wait(); // release every claimer at once to force the race
                store
                    .approvals()
                    .claim(&id)
                    .expect("claim must not error — busy_timeout serializes writers")
            })
        })
        .collect();

    let winners = handles
        .into_iter()
        .map(|h| h.join().expect("claimer thread panicked"))
        .filter(|&won| won)
        .count();
    assert_eq!(
        winners, 1,
        "exactly one concurrent claim may win the #118 race, got {winners}"
    );

    let store = RuntimeStore::open(&db).expect("reopen store");
    let appr = store
        .approvals()
        .get(&id)
        .expect("get approval")
        .expect("approval exists");
    assert_eq!(
        appr.user_decision.as_deref(),
        Some("approving"),
        "the single winner must hold the intermediate claim"
    );
}

#[test]
#[ignore = "spawns a real mermaidd; run with: cargo test --test daemon_integration -- --ignored"]
fn daemon_singleton_flock_rejects_a_second_start() {
    // #131: a daemon-lifetime advisory flock makes two concurrent starts safe —
    // the second must refuse rather than race the connect-probe → unlink → bind
    // dance and knock the first off its socket.
    let base = fresh_base("flock");
    let _dir = DirGuard(base.clone());
    let (_db, sock) = data_paths(&base);
    let a_err = base.join("a.stderr");
    let b_err = base.join("b.stderr");

    let _a = DaemonGuard(spawn_daemon(&base, &a_err));
    assert!(
        wait_until(|| sock.exists(), TIMEOUT),
        "daemon A never bound its socket; stderr:\n{}",
        read(&a_err)
    );

    // A holds the flock now (it binds the socket only after acquiring it), so B
    // must fail at the lock rather than start. B is guarded so a hang here (the
    // bug case) still gets cleaned up.
    let mut b = DaemonGuard(spawn_daemon(&base, &b_err));
    let status = match wait_for_exit(&mut b.0, TIMEOUT) {
        Some(status) => status,
        None => panic!(
            "daemon B never exited — the #131 singleton flock failed to reject a second start"
        ),
    };
    assert!(
        !status.success(),
        "daemon B must exit non-zero while A holds the lock; got {status:?}"
    );
    let b_stderr = read(&b_err);
    assert!(
        b_stderr.contains("lock held") || b_stderr.to_lowercase().contains("already"),
        "B should report the singleton-lock rejection (#131); its stderr was:\n{b_stderr}"
    );
}

#[test]
#[ignore = "spawns a real mermaidd; run with: cargo test --test daemon_integration -- --ignored"]
fn daemon_startup_reconciles_running_task_and_gcs_old_archived_row() {
    // #120 + #130: on startup the daemon recovers state a crashed predecessor
    // left stranded — a task stuck `Running` is failed, and archived rows past
    // the retention window are pruned while active ones are kept. Both run in the
    // same startup block, before the control socket is bound.
    let base = fresh_base("recover");
    let _dir = DirGuard(base.clone());
    let (db, sock) = data_paths(&base);
    let err = base.join("d.stderr");

    let (running_task, old_appr, fresh_appr) = {
        let store = RuntimeStore::open(&db).expect("open store for seeding");
        let task = store
            .tasks()
            .create(NewTask::new("stuck task", "/tmp/project", "test/model").daemon_owned())
            .expect("create task");
        store
            .tasks()
            .update_status(&task.id, TaskStatus::Running, None)
            .expect("mark running");

        let old = make_approval(&store, "old archived action");
        let fresh = make_approval(&store, "fresh archived action");
        store
            .approvals()
            .archive(&[old.clone(), fresh.clone()], "test")
            .expect("archive both");
        (task.id, old, fresh)
    };

    // Backdate the "old" approval far past the 30-day retention window. A
    // year-2000 stamp sorts before any plausible cutoff regardless of sub-second
    // / timezone formatting, so the GC's `archived_at < cutoff` is unambiguous.
    {
        let conn = rusqlite::Connection::open(&db).expect("open db to backdate");
        let changed = conn
            .execute(
                "UPDATE approvals SET archived_at = ?2 WHERE id = ?1",
                rusqlite::params![old_appr, "2000-01-01T00:00:00+00:00"],
            )
            .expect("backdate archived_at");
        assert_eq!(changed, 1, "backdate must hit exactly the old approval");
    }

    let _daemon = DaemonGuard(spawn_daemon(&base, &err));
    assert!(
        wait_until(|| sock.exists(), TIMEOUT),
        "daemon never became ready (socket never bound); stderr:\n{}",
        read(&err)
    );

    // The socket exists only after reconcile + gc committed, so this reads the
    // recovered state.
    let store = RuntimeStore::open(&db).expect("reopen store to verify");
    let task = store
        .tasks()
        .get(&running_task)
        .expect("get task")
        .expect("task row exists");
    assert_eq!(
        task.status,
        TaskStatus::Failed,
        "#120: a task left Running by a crashed daemon must be reconciled to Failed on restart"
    );
    assert!(
        store.approvals().get(&old_appr).expect("get old").is_none(),
        "#130: the long-archived approval must be GC'd on startup"
    );
    assert!(
        store
            .approvals()
            .get(&fresh_appr)
            .expect("get fresh")
            .is_some(),
        "#130: a freshly-archived approval must be kept"
    );
}
