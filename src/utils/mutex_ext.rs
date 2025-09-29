use std::sync::{Mutex, PoisonError};

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
            }
        }
    }

    fn lock_mut_safe(&self) -> std::sync::MutexGuard<'_, T> {
        match self.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                // Mutex was poisoned, but we can still get the guard
                eprintln!("[WARNING] Mutex was poisoned, recovering guard");
                poisoned.into_inner()
            }
        }
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

/// Helper to try locking a mutex with Result
pub fn try_lock<'a, T>(mutex: &'a Mutex<T>, context: &str) -> anyhow::Result<std::sync::MutexGuard<'a, T>> {
    mutex.lock().map_err(|e| {
        anyhow::anyhow!("{}: failed to acquire lock: {}", context, e)
    })
}