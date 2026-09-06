//! What a fresh daemon does before it serves: reconcile state a crashed
//! predecessor stranded, rebuild the session index, prune old rows and logs.

/// Recover state stranded by a previous daemon's crash (#120, #118) and prune
/// old runtime rows + checkpoint dirs (#130). Must run singleton-guarded — by
/// the unix flock or the windows first-pipe-instance — so it executes once per
/// live daemon. All best-effort.
pub(super) fn startup_recovery() {
    let daemon = crate::app::load_config().unwrap_or_default().daemon;
    if let Ok(store) = mermaid_runtime::RuntimeStore::open_default() {
        match store.reconcile_after_restart() {
            Ok((tasks, claims)) if tasks + claims > 0 => {
                tracing::info!(
                    tasks,
                    claims,
                    "reconciled state stranded by a previous daemon"
                );
            },
            Ok(_) => {},
            Err(error) => tracing::warn!(error = %error, "startup reconcile failed"),
        }
        // The session index is a cache over `.mermaid/conversations/`; rebuild
        // it from disk for every project this store knows, so a row a crashed
        // run never wrote, or a file deleted behind the daemon's back, is put
        // right here rather than by a manual command.
        match crate::session::rebuild_session_index(&store) {
            Ok(report) if report.backfilled + report.pruned > 0 => tracing::info!(
                projects = report.projects,
                backfilled = report.backfilled,
                pruned = report.pruned,
                "rebuilt the session index from disk"
            ),
            Ok(_) => {},
            Err(error) => tracing::warn!(error = %error, "session index rebuild failed"),
        }
        match store.gc(daemon.retention_days, daemon.outcomes_retention_days) {
            Ok(removed) if removed > 0 => tracing::info!(removed, "gc pruned old runtime rows"),
            Ok(_) => {},
            Err(error) => tracing::warn!(error = %error, "startup gc failed"),
        }
    }
    if let Ok(removed) = mermaid_runtime::gc_old_checkpoint_dirs(daemon.retention_days)
        && removed > 0
    {
        tracing::info!(removed, "gc removed old checkpoint directories");
    }
    // Subagent worktrees are removed when their child is evicted, but a
    // crash between creating one and cleaning it up strands the checkout —
    // and agent ids are per-session, so nothing reclaims it by name.
    if let Ok(removed) = mermaid_runtime::gc_orphaned_worktrees(daemon.retention_days)
        && removed > 0
    {
        tracing::info!(removed, "gc removed orphaned subagent worktrees");
    }
    if let Ok(removed) = sweep_stale_bg_logs(daemon.retention_days)
        && removed > 0
    {
        tracing::info!(removed, "reaped stale background-command logs");
    }
    // Per-session scratch directories: interactive sessions sweep with the
    // built-in default on startup; the daemon adds a knob-driven pass so
    // long-lived daemon-only boxes still converge.
    if let Ok(removed) =
        crate::session::scratchpad::sweep_stale(daemon.scratchpad_retention_days.max(0) as u64)
        && removed > 0
    {
        tracing::info!(removed, "reaped stale session scratchpads");
    }
}

/// Reap background-command tee logs (`mermaid-bg-<pid>-<nanos>.log`) left in the
/// private temp dir by detached (Ctrl+B) commands of prior sessions. A live
/// detached process keeps appending to its log, so an mtime older than the
/// retention window means the writer is long gone.
pub(super) fn sweep_stale_bg_logs(retention_days: i64) -> std::io::Result<u64> {
    sweep_stale_bg_logs_in(&mermaid_model::utils::private_temp_dir()?, retention_days)
}

/// The `sweep_stale_bg_logs` body over an explicit directory, for testing.
/// Best-effort: files it can't stat or remove (e.g. one a live process still
/// holds open on Windows) are skipped.
pub(super) fn sweep_stale_bg_logs_in(
    dir: &std::path::Path,
    retention_days: i64,
) -> std::io::Result<u64> {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(
            retention_days.max(0) as u64 * 24 * 60 * 60,
        ))
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut removed = 0u64;
    for entry in std::fs::read_dir(dir)?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !(name.starts_with("mermaid-bg-") && name.ends_with(".log")) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|mtime| mtime < cutoff)
            .unwrap_or(false);
        if stale && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}
