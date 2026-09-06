//! Best-effort process hardening, applied as early in `main` as possible.
//!
//! Disables core dumps — which can capture in-memory secrets (API keys, the
//! transcript) — and debugger attachment where the platform has a switch for
//! it. Every step is best-effort and independently ignorable; a failure here
//! must never abort startup. These are process-wide calls, so calling from any
//! thread is safe.
//!
//! What each platform can do:
//!
//! - Linux: `RLIMIT_CORE = 0` and `PR_SET_DUMPABLE = 0` (the latter also
//!   refuses a non-privileged `ptrace` attach and hides `/proc/<pid>/mem`).
//! - macOS: `RLIMIT_CORE = 0` and `ptrace(PT_DENY_ATTACH)`, the same switch
//!   every keychain-touching Apple binary flips.
//! - Windows: `SetErrorMode(SEM_NOGPFAULTERRORBOX | SEM_FAILCRITICALERRORS)`,
//!   which keeps a crash from raising the Windows Error Reporting dialog
//!   (whose "debug" path hands the process image to whatever handler is
//!   registered). There is no per-process switch against a debugger with the
//!   same rights as the user, and no per-process switch for WER's dump
//!   collection — that is a registry policy — so Windows applies less, and
//!   says so through [`Hardening`].
//!
//! Env scrubbing (`LD_*`/`DYLD_*`) is a deliberate non-goal here: it needs a
//! single-threaded context to be sound under the Rust 2024 `set_var` rules,
//! and the loader has already consulted those variables by the time `main`
//! runs.

/// What [`harden_process`] managed to apply, so the caller can log a process
/// that starts with none of it rather than assume the protection is there.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hardening {
    /// Human-readable names of the measures that took effect, in the order
    /// they were tried. Empty when the platform has no measure or every call
    /// failed.
    pub applied: Vec<&'static str>,
}

impl Hardening {
    /// `true` when nothing took effect: the caller's cue to warn.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }
}

/// Apply the platform process hardening. Call once, as the first statement of
/// `main`. Never panics; report what applied through the return value.
#[must_use]
pub fn harden_process() -> Hardening {
    let mut hardening = Hardening::default();
    #[cfg(target_os = "linux")]
    linux::harden(&mut hardening);
    #[cfg(target_os = "macos")]
    macos::harden(&mut hardening);
    #[cfg(windows)]
    windows::harden(&mut hardening);
    hardening
}

/// `RLIMIT_CORE = 0`, soft and hard: a crash must not write a core file that
/// could contain API keys or transcript text.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn disable_core_dumps(hardening: &mut Hardening) {
    use rustix::process::{Resource, Rlimit, setrlimit};
    if setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )
    .is_ok()
    {
        hardening.applied.push("core dumps disabled");
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use rustix::process::{DumpableBehavior, set_dumpable_behavior};

    pub fn harden(hardening: &mut super::Hardening) {
        super::disable_core_dumps(hardening);
        // PR_SET_DUMPABLE=0 also blocks a non-privileged ptrace attach and keeps
        // /proc/<pid>/mem inaccessible.
        if set_dumpable_behavior(DumpableBehavior::NotDumpable).is_ok() {
            hardening.applied.push("ptrace attach refused");
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    pub fn harden(hardening: &mut super::Hardening) {
        super::disable_core_dumps(hardening);
        // PT_DENY_ATTACH: the kernel refuses later ptrace attaches to this
        // process and kills it if a debugger is already attached. `libc`
        // exposes the constant; rustix has no binding for this request.
        // SAFETY: `ptrace` with PT_DENY_ATTACH takes no pointers and acts only
        // on the calling process.
        let rc = unsafe { libc::ptrace(libc::PT_DENY_ATTACH, 0, std::ptr::null_mut(), 0) };
        if rc == 0 {
            hardening.applied.push("ptrace attach refused");
        }
    }
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SetErrorMode,
    };

    pub fn harden(hardening: &mut super::Hardening) {
        // No WER dialog on a crash (its "debug" button hands the process to
        // the registered JIT debugger), and no "insert disk" style modal on a
        // missing volume. `SetErrorMode` returns the previous mode and cannot
        // fail; OR-ing the previous mode keeps whatever the parent set.
        // SAFETY: takes a flag word, touches only this process's error mode.
        let previous = unsafe { SetErrorMode(0) };
        unsafe {
            SetErrorMode(previous | SEM_NOGPFAULTERRORBOX | SEM_FAILCRITICALERRORS);
        }
        hardening.applied.push("crash dialog suppressed");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn harden_process_is_best_effort_and_repeatable() {
        // Must not panic, and a second call is harmless.
        let _ = super::harden_process();
        let _ = super::harden_process();
    }

    /// Every supported platform has at least one measure; a `Hardening`
    /// that comes back empty on one of them is the regression this pins.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn harden_applies_something_on_every_supported_platform() {
        let hardening = super::harden_process();
        assert!(!hardening.is_empty(), "{hardening:?}");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn harden_disables_core_dumps() {
        let hardening = super::harden_process();
        assert!(
            hardening.applied.contains(&"core dumps disabled"),
            "{hardening:?}"
        );
        let lim = rustix::process::getrlimit(rustix::process::Resource::Core);
        assert_eq!(lim.current, Some(0), "core-dump soft limit must be zero");
    }

    #[cfg(windows)]
    #[test]
    fn harden_suppresses_the_crash_dialog() {
        use windows_sys::Win32::System::Diagnostics::Debug::{SEM_NOGPFAULTERRORBOX, SetErrorMode};
        let _ = super::harden_process();
        // Reading the mode is `SetErrorMode(0)` followed by restoring what it
        // returned; the flag must already be set.
        let mode = unsafe { SetErrorMode(0) };
        unsafe {
            SetErrorMode(mode);
        }
        assert_ne!(mode & SEM_NOGPFAULTERRORBOX, 0, "mode {mode:#x}");
    }
}
