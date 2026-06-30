//! Atomic file writes.
//!
//! Plain `fs::write` truncates the target to zero length and then writes the
//! new contents in place. A crash / kill / disk-full between those two steps
//! leaves the file empty or half-written — catastrophic for session,
//! checkpoint, and plugin-lockfile state that is rewritten in full on every
//! save. [`write_atomic`] writes to a temp sibling, fsyncs it, then renames over
//! the target, so a reader always sees either the old complete file or the new
//! complete file.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A sibling temp file untouched for at least this long is treated as orphaned by
/// a crashed writer and swept on the next write to the same target. The window is
/// deliberately generous: `write_atomic` rewrites small session/checkpoint/
/// lockfile state in a single pass, so a legitimate in-flight temp (ours or
/// another live writer's) is always far younger than this and is never collected.
const STALE_TEMP_SECS: u64 = 3600;

/// Write `bytes` to `path` atomically: temp file in the same directory →
/// `sync_all` → rename over the destination. The rename is atomic on the same
/// filesystem (and replaces an existing target on both Unix and Windows).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("tmp");

    // Best-effort: clear temp siblings stranded by a previous crashed write to
    // this same target. A crash between the `File::create(tmp)` and `rename`
    // below leaves `.<stem>.<pid>.<n>.tmp` behind forever (cleanup otherwise runs
    // only on rename-error or success), so without this repeated crashes would
    // litter the directory. The sweep only removes clearly abandoned (stale) temps
    // and never the destination or a fresh/in-flight temp.
    sweep_stale_temps(parent, stem, Duration::from_secs(STALE_TEMP_SECS));

    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{}.{}.{}.tmp", stem, std::process::id(), n));

    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }

    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // Best-effort durability of the rename itself. Opening a directory as a
    // File is not supported on Windows, so this is a silent no-op there.
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Best-effort sweep of orphaned temp siblings for the target named `stem` in
/// `parent`, left behind when a writer crashed between creating the temp and
/// renaming it over the destination. Only files matching THIS target's temp
/// pattern (`.{stem}.{pid}.{n}.tmp`) and older than `max_age` are removed.
///
/// Safety: the destination is named exactly `stem`, which can never start with
/// the dotted `.{stem}.` prefix, so it is structurally unmatched; and a live,
/// in-flight temp (ours or another concurrent writer's) is younger than
/// `max_age` and so is never collected. Every error is swallowed — a sweep
/// failure must not fail the write.
fn sweep_stale_temps(parent: &Path, stem: &str, max_age: Duration) {
    let prefix = format!(".{stem}.");
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(&prefix) || !name.ends_with(".tmp") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .map(|age| age >= max_age)
            .unwrap_or(false);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_existing_and_no_temp_left() {
        let dir = std::env::temp_dir().join(format!("mermaid_atomic_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let target = dir.join("conv.json");
        write_atomic(&target, b"first").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "first");
        write_atomic(&target, b"second").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "second");
        // No leftover temp files.
        let leftovers = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_removes_only_matching_stale_temps() {
        let dir = std::env::temp_dir().join(format!("mermaid_atomic_sweep_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);

        let target = dir.join("conv.json");
        write_atomic(&target, b"live").unwrap();

        // An orphaned temp for THIS target (a crashed prior write).
        let orphan = dir.join(".conv.json.99999.0.tmp");
        fs::write(&orphan, b"half-written").unwrap();
        // A temp for a DIFFERENT target — must be left alone.
        let other = dir.join(".other.json.99999.0.tmp");
        fs::write(&other, b"someone else").unwrap();
        // An unrelated regular file — must be left alone.
        let unrelated = dir.join("notes.txt");
        fs::write(&unrelated, b"keep me").unwrap();

        // max_age = ZERO ⇒ any matching temp qualifies as stale.
        sweep_stale_temps(&dir, "conv.json", Duration::ZERO);

        assert!(!orphan.exists(), "matching stale temp must be swept");
        assert!(other.exists(), "a different target's temp must survive");
        assert!(unrelated.exists(), "unrelated files must survive");
        assert!(target.exists(), "the destination must never be swept");
        assert_eq!(fs::read_to_string(&target).unwrap(), "live");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_preserves_fresh_in_flight_temps() {
        let dir = std::env::temp_dir().join(format!("mermaid_atomic_fresh_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);

        // A freshly created temp stands in for a concurrent, in-flight write.
        let fresh = dir.join(".conv.json.12345.7.tmp");
        fs::write(&fresh, b"being written").unwrap();

        // A long window must never collect a just-created temp.
        sweep_stale_temps(&dir, "conv.json", Duration::from_secs(STALE_TEMP_SECS));

        assert!(fresh.exists(), "a fresh/in-flight temp must not be swept");
        let _ = fs::remove_dir_all(&dir);
    }
}
