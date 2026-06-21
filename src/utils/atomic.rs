//! Atomic file writes.
//!
//! Plain `fs::write` truncates the target to zero length and then writes the
//! new contents in place. A crash / kill / disk-full between those two steps
//! leaves the file empty or half-written — catastrophic for session and
//! checkpoint state that is rewritten in full on every save. [`write_atomic`]
//! writes to a temp sibling, fsyncs it, then renames over the target, so a
//! reader always sees either the old complete file or the new complete file.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` atomically: temp file in the same directory →
/// `sync_all` → rename over the destination. The rename is atomic on the same
/// filesystem (and replaces an existing target on both Unix and Windows).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("tmp");
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
}
