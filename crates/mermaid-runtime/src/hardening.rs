//! Best-effort process hardening, applied as early in `main` as possible.
//!
//! Disables core dumps — which can capture in-memory secrets (API keys, the
//! transcript) — and ptrace attachment. Every step is best-effort and
//! independently ignorable; a failure here must never abort startup. These are
//! process-wide syscalls, so calling from any thread is safe.
//!
//! Linux-only for now (the primary target). macOS `PT_DENY_ATTACH` and env
//! scrubbing (`LD_*`/`DYLD_*`) are deliberate follow-ups: the latter needs a
//! single-threaded context to be sound under the Rust 2024 `set_var` rules.

/// Apply the platform process hardening. Call once, as the first statement of
/// `main`. Never panics.
pub fn harden_process() {
    #[cfg(target_os = "linux")]
    linux::harden();
}

#[cfg(target_os = "linux")]
mod linux {
    use rustix::process::{DumpableBehavior, Resource, Rlimit, set_dumpable_behavior, setrlimit};

    pub fn harden() {
        // No core dumps: a crash must not write a core file that could contain
        // API keys or transcript text. Pin both the soft and hard limit to 0.
        let _ = setrlimit(
            Resource::Core,
            Rlimit {
                current: Some(0),
                maximum: Some(0),
            },
        );
        // PR_SET_DUMPABLE=0 also blocks a non-privileged ptrace attach and keeps
        // /proc/<pid>/mem inaccessible.
        let _ = set_dumpable_behavior(DumpableBehavior::NotDumpable);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn harden_process_is_best_effort_and_repeatable() {
        // Must not panic, and a second call is harmless.
        super::harden_process();
        super::harden_process();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn harden_disables_core_dumps() {
        super::harden_process();
        let lim = rustix::process::getrlimit(rustix::process::Resource::Core);
        assert_eq!(lim.current, Some(0), "core-dump soft limit must be zero");
    }
}
