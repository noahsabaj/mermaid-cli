use std::sync::{Arc, Mutex};
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Extension trait for Mutex to provide safe lock operations
pub trait MutexExt<T> {
    /// Lock the mutex, recovering from poison errors
    /// This is safer than unwrap() as it handles poisoned mutexes gracefully
    fn lock_safe(&self) -> T
    where
        T: Clone;

    /// Lock the mutex for mutation, recovering from poison errors
    fn lock_mut_safe(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T: Clone> MutexExt<T> for Mutex<T> {
    fn lock_safe(&self) -> T {
        match self.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => {
                // Mutex was poisoned due to a panic in another thread
                // We can still access the data, but we log the issue
                eprintln!("[WARNING] Mutex was poisoned, recovering data");
                poisoned.into_inner().clone()
            },
        }
    }

    fn lock_mut_safe(&self) -> std::sync::MutexGuard<'_, T> {
        match self.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                // Mutex was poisoned, but we can still get the guard
                eprintln!("[WARNING] Mutex was poisoned, recovering guard");
                poisoned.into_inner()
            },
        }
    }
}

/// Safe lock for Arc<Mutex<T>> - standalone function since we can't impl on Arc
pub fn lock_arc_mutex_safe<T>(mutex: &Arc<Mutex<T>>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            eprintln!("[WARNING] Arc<Mutex> was poisoned, recovering guard");
            poisoned.into_inner()
        },
    }
}

/// Helper to lock a mutex with expect-style error message
#[macro_export]
macro_rules! lock_or_panic {
    ($mutex:expr, $msg:expr) => {
        $mutex.lock().unwrap_or_else(|e| {
            panic!("{}: mutex poisoned: {}", $msg, e);
        })
    };
}

/// Extension trait for RwLock to provide convenient async lock operations
/// Unlike Mutex, RwLock doesn't poison, so no recovery logic needed
#[allow(dead_code)]
pub trait RwLockExt<T> {
    /// Acquire a read lock (shared access, multiple readers allowed)
    async fn read_safe<'a>(&'a self) -> RwLockReadGuard<'a, T>
    where
        T: 'a;

    /// Acquire a write lock (exclusive access, blocks all readers and writers)
    async fn write_safe<'a>(&'a self) -> RwLockWriteGuard<'a, T>
    where
        T: 'a;
}

impl<T> RwLockExt<T> for RwLock<T> {
    async fn read_safe<'a>(&'a self) -> RwLockReadGuard<'a, T>
    where
        T: 'a,
    {
        self.read().await
    }

    async fn write_safe<'a>(&'a self) -> RwLockWriteGuard<'a, T>
    where
        T: 'a,
    {
        self.write().await
    }
}

/// Helper for Arc<RwLock<T>> - common pattern in concurrent code
impl<T> RwLockExt<T> for Arc<RwLock<T>> {
    async fn read_safe<'a>(&'a self) -> RwLockReadGuard<'a, T>
    where
        T: 'a,
    {
        self.read().await
    }

    async fn write_safe<'a>(&'a self) -> RwLockWriteGuard<'a, T>
    where
        T: 'a,
    {
        self.write().await
    }
}
