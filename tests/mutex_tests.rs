// Tests for mutex safety utilities

use mermaid_cli::utils::{lock_arc_mutex_safe, MutexExt};
use std::sync::{Arc, Mutex};

#[test]
fn test_mutex_lock_safe() {
    let data = Mutex::new(42);
    let result = data.lock_safe();
    assert_eq!(result, 42);
}

#[test]
fn test_mutex_lock_mut_safe() {
    let data = Mutex::new(vec![1, 2, 3]);
    {
        let mut guard = data.lock_mut_safe();
        guard.push(4);
    }
    assert_eq!(*data.lock().unwrap(), vec![1, 2, 3, 4]);
}

#[test]
fn test_arc_mutex_lock_safe() {
    let data = Arc::new(Mutex::new(String::from("hello")));
    {
        let mut guard = lock_arc_mutex_safe(&data);
        guard.push_str(" world");
    }
    assert_eq!(*data.lock().unwrap(), "hello world");
}

#[test]
fn test_poisoned_mutex_recovery() {
    let data = Arc::new(Mutex::new(0));
    let data_clone = Arc::clone(&data);

    // Create a thread that will panic while holding the lock
    let handle = std::thread::spawn(move || {
        let mut guard = data_clone.lock().unwrap();
        *guard = 42;
        panic!("Intentional panic to poison mutex");
    });

    // Wait for thread to finish (and panic)
    let _ = handle.join();

    // The mutex is now poisoned, but lock_safe should recover
    let mut guard = lock_arc_mutex_safe(&data);
    *guard = 100;
    drop(guard);

    // Verify we can still use the mutex
    assert_eq!(*lock_arc_mutex_safe(&data), 100);
}

#[test]
fn test_concurrent_access() {
    let data = Arc::new(Mutex::new(0));
    let mut handles = vec![];

    for _ in 0..10 {
        let data_clone = Arc::clone(&data);
        handles.push(std::thread::spawn(move || {
            for _ in 0..100 {
                let mut guard = lock_arc_mutex_safe(&data_clone);
                *guard += 1;
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(*lock_arc_mutex_safe(&data), 1000);
}
