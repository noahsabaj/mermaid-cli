//! Per-path write serialization for file-mutating tools.
//!
//! The reducer turns N model tool calls in one turn into N `Cmd::ExecuteTool`s
//! that the effect runner spawns concurrently into the turn's `JoinSet`. Two
//! `write_file` / `delete_file` / `create_directory` / `apply_patch` calls
//! targeting the SAME path would then race: the atomic temp+rename means the
//! last rename silently wins (lost update), and a read-modify-write edit is a
//! TOCTOU. This module hands each mutating tool an async lock keyed on the
//! canonical resolved path, so writers to one path serialize while writers to
//! different paths still run in parallel.
//!
//! The reducer stays pure — this lives entirely in the tool/effect layer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// Canonical-path → live lock. The value is a `Weak` so an entry evaporates once
/// no tool holds or awaits it (the owning `Arc` lives in the returned guard).
/// Guarded by a std `Mutex` held only for the O(1) registry lookup — never
/// across the async wait, so it can't stall the runtime.
type LockRegistry = Mutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>;

static REGISTRY: LazyLock<LockRegistry> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Fetch (or create) the lock `Arc` for `key`. On a miss, inserts a fresh lock
/// and opportunistically prunes entries whose `Arc` is gone, keeping the map
/// bounded without a reaper. The std-`Mutex` critical section is synchronous, so
/// two callers for the same key can't both insert: the second sees the first's
/// still-live `Arc` (the caller keeps it alive across its whole tool run).
fn lock_for(key: &Path) -> Arc<AsyncMutex<()>> {
    let mut map = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = map.get(key).and_then(Weak::upgrade) {
        return existing;
    }
    let fresh = Arc::new(AsyncMutex::new(()));
    map.retain(|_, w| w.strong_count() > 0);
    map.insert(key.to_path_buf(), Arc::downgrade(&fresh));
    fresh
}

/// Acquire the write lock for a single canonical path, held until the returned
/// guard drops (typically the end of the tool's `execute`).
pub(crate) async fn lock_path(canonical: &Path) -> OwnedMutexGuard<()> {
    lock_for(canonical).lock_owned().await
}

/// Acquire write locks for several paths at once (e.g. a multi-file
/// `apply_patch`). The caller MUST pass a **sorted, de-duplicated** slice: a
/// single global acquisition order is what keeps two overlapping multi-path
/// operations from deadlocking.
pub(crate) async fn lock_paths(sorted_unique: &[PathBuf]) -> Vec<OwnedMutexGuard<()>> {
    let mut guards = Vec::with_capacity(sorted_unique.len());
    for path in sorted_unique {
        guards.push(lock_for(path).lock_owned().await);
    }
    guards
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn same_path_serializes_no_lost_update() {
        // Each task does load → yield → store under the lock. Without mutual
        // exclusion the yield lets another task read the same value, and updates
        // are lost (final < 8). The lock makes each critical section atomic.
        let key = PathBuf::from("/lock-test/same");
        let counter = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let key = key.clone();
            let counter = Arc::clone(&counter);
            handles.push(tokio::spawn(async move {
                let _g = lock_path(&key).await;
                let v = counter.load(Ordering::Relaxed);
                tokio::task::yield_now().await;
                counter.store(v + 1, Ordering::Relaxed);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(counter.load(Ordering::Relaxed), 8, "no updates may be lost");
    }

    #[tokio::test]
    async fn different_paths_do_not_serialize() {
        // Holding one path's lock must not block acquiring another's; a shared
        // lock would deadlock this single task (tokio Mutex is not reentrant).
        let res = tokio::time::timeout(Duration::from_secs(5), async {
            let _a = lock_path(Path::new("/lock-test/a")).await;
            let _b = lock_path(Path::new("/lock-test/b")).await;
        })
        .await;
        assert!(res.is_ok(), "distinct paths must not serialize/deadlock");
    }

    #[tokio::test]
    async fn registry_prunes_dropped_locks() {
        let key = PathBuf::from("/lock-test/prune/me");
        {
            let _g = lock_path(&key).await;
            assert!(REGISTRY.lock().unwrap().contains_key(&key));
        } // guard + Arc dropped ⇒ the Weak now dangles
        // A miss on another key triggers the prune pass in `lock_for`.
        let _other = lock_path(Path::new("/lock-test/prune/other")).await;
        assert!(
            !REGISTRY.lock().unwrap().contains_key(&key),
            "a dropped lock's entry must be pruned"
        );
    }
}
