//! Plan-mode gating: which writes and which build commands a plan may run.

use std::path::Path;

use super::RiskClass;
use super::shell::*;

pub use mermaid_model::safety::{PLAN_DENIAL_MARKER, READ_ONLY_DENIAL_MARKER};

/// True when `command` is a build/test invocation plan mode auto-allows even
/// though it spawns processes: every segment is either read-only or a known
/// build tool running a known build/test subcommand. Grounding a plan in a
/// real compile or test run makes plans materially better, and these commands
/// only write build caches (`target/`, test artifacts) — not the sources the
/// plan is about.
///
/// Deliberately anchored, like `Allow` policy overrides:
/// - any command/process substitution refuses (`cargo test $(curl evil)`);
/// - wrappers refuse (`sudo cargo test` — the wrapper, not cargo, is the head);
/// - a file-writing redirect refuses via `classify_segment` (`cargo test >
///   src/lib.rs`); safe-device redirects (`2>/dev/null`) stay allowed;
/// - the worst-segment rule holds: `cargo test && rm -rf .` refuses because
///   the second segment classifies as a mutation.
///
/// The subcommand tables are curatable the same way `READ_ONLY_BINARIES` is —
/// additions need the audit tests below.
///
/// Dialect-dispatched on [`HostShell::current`](super::HostShell::current):
/// the command is parsed in the grammar of the interpreter that will run it,
/// same as risk classification.
#[must_use]
pub fn is_plan_safe_build_command(command: &str) -> bool {
    match super::HostShell::current() {
        super::HostShell::PowerShell => {
            super::shell::powershell::is_plan_safe_build_command_ps(command)
        },
        super::HostShell::Posix => is_plan_safe_build_command_posix(command),
    }
}

pub(in crate::policy) fn is_plan_safe_build_command_posix(command: &str) -> bool {
    let split = split_command(command);
    // Build/test invocations have no legitimate heredoc shape — refusing them
    // outright keeps this carve-out anchored.
    if !split.heredocs.is_empty() {
        return false;
    }
    let segments = split.segments;
    if segments.is_empty() {
        return false;
    }
    if segments
        .iter()
        .any(|seg| !extract_substitutions(seg).is_empty())
    {
        return false;
    }
    segments.iter().all(|seg| {
        let tokens = tokenize(seg);
        match classify_segment(&tokens) {
            RiskClass::ReadOnly => true,
            // `shell_max` ranks Process above ShellMutation, so a Process
            // segment can absorb a file-writing redirect (`cargo test >
            // src/lib.rs` classifies Process) — scan for writes explicitly.
            RiskClass::Process => {
                !segment_has_file_write(&tokens) && segment_is_safe_build(&tokens)
            },
            _ => false,
        }
    })
}

/// True when `raw` (a tool-supplied path, absolute or workdir-relative) names
/// the plan file. Lexical normalization only — the plan file may not exist
/// yet (the first write creates it), so `canonicalize` is not an option, and
/// `..`/`.` components must not smuggle a different file past the exemption.
#[must_use]
pub fn is_plan_file_path(workdir: &Path, raw: &str, plan_file: &Path) -> bool {
    fn normalize(p: &Path) -> std::path::PathBuf {
        use std::path::Component;
        let mut out = std::path::PathBuf::new();
        for c in p.components() {
            match c {
                Component::CurDir => {},
                Component::ParentDir => {
                    out.pop();
                },
                other => out.push(other.as_os_str()),
            }
        }
        out
    }
    let p = Path::new(raw);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        workdir.join(p)
    };
    normalize(&abs) == normalize(plan_file)
}

/// Builtins that move the shell's own working directory — the POSIX
/// spellings and the PowerShell cmdlets/aliases (model commands run under
/// PowerShell on Windows).
///
/// They are `ReadOnly` for risk purposes (nothing outside the shell
/// changes), but any lexical path match against a fixed workdir becomes
/// unsound once one of these runs. Compare case-insensitively — PowerShell
/// resolves command names that way, so `CD`/`Set-Location` must refuse too;
/// for POSIX that can only over-refuse (a unix binary literally named `CD`),
/// the safe direction.
pub(crate) const CWD_CHANGING_BUILTINS: &[&str] = &[
    "cd",
    "pushd",
    "popd",
    "set-location",
    "push-location",
    "pop-location",
    "sl",
];

/// True when `command`'s ONLY effect is writing the plan file: every segment
/// classifies read-only once its plan-file redirects are set aside, no
/// command/process substitution appears anywhere (expanding heredoc bodies
/// included), and at least one redirect actually targets the plan file.
///
/// The plan-mode escape hatch for models that author the plan via shell
/// (`echo … > plan.md`, `cat > plan.md <<'EOF'`) instead of `write_file` —
/// observed doom-looping for minutes against the generic denial. Anchored in
/// the `is_plan_safe_build_command` style (worst-segment rule, fail-closed on
/// anything unprovable):
/// - substitutions refuse outright (`echo $(date) > plan.md`); quoted-
///   delimiter heredoc bodies are exempt — they are provably literal, and
///   plans legitimately quote shell snippets;
/// - `tee`/`dd` refuse (multi-target argv parsing buys nothing over `>`);
/// - a cwd-changing builtin refuses: `cd`/`pushd`/`popd` classify `ReadOnly`
///   (they only move the shell's own cwd), so `cd /tmp && echo x > plan.md`
///   passed every check above while the redirect landed in a different
///   directory entirely. The match below is lexical and cannot model a cwd
///   that moves mid-command, so the honest answer is to refuse;
/// - every redirect must resolve to a safe device or the plan file; `$VAR`,
///   `~`, globs, and dangling `>` all fail the lexical match (fail-closed);
/// - `>>` append is allowed — same file, legitimate incremental authoring;
/// - with the plan-file redirects stripped, the segment must classify
///   `ReadOnly` (unknown heads fail-safe to `ShellMutation` and refuse).
///
/// Residual power is content-level only: arbitrary bytes into the plan file,
/// which `write_file`'s carve-out already grants.
///
/// Dialect-dispatched on [`HostShell::current`](super::HostShell::current) —
/// on Windows the PowerShell spelling accepts backslash plan paths the POSIX
/// tokenizer would mangle, keeping the plan denial's "a shell redirect
/// writing ONLY that file also works" promise true there.
#[must_use]
pub fn is_plan_file_only_write(command: &str, workdir: &Path, plan_file: &Path) -> bool {
    match super::HostShell::current() {
        super::HostShell::PowerShell => {
            super::shell::powershell::is_plan_file_only_write_ps(command, workdir, plan_file)
        },
        super::HostShell::Posix => is_plan_file_only_write_posix(command, workdir, plan_file),
    }
}

pub(in crate::policy) fn is_plan_file_only_write_posix(
    command: &str,
    workdir: &Path,
    plan_file: &Path,
) -> bool {
    let split = split_command(command);
    if split.segments.is_empty() {
        return false;
    }
    if split
        .segments
        .iter()
        .any(|seg| !extract_substitutions(seg).is_empty())
    {
        return false;
    }
    if split.heredocs.iter().any(|hd| {
        hd.expands && (hd.body.contains("$(") || hd.body.contains('`') || hd.body.contains("<("))
    }) {
        return false;
    }
    let mut saw_plan_redirect = false;
    for seg in &split.segments {
        let tokens = tokenize(seg);
        let mut kept: Vec<String> = Vec::with_capacity(tokens.len());
        let mut skip_next = false;
        for (i, tok) in tokens.iter().enumerate() {
            if skip_next {
                skip_next = false;
                continue;
            }
            let t = tok.as_str();
            if t == "tee" || t == "dd" {
                return false;
            }
            // A cwd change would silently relocate the redirect target that
            // `is_plan_file_path` matches lexically against `workdir`.
            if CWD_CHANGING_BUILTINS
                .iter()
                .any(|b| basename(t).eq_ignore_ascii_case(b))
            {
                return false;
            }
            if redirect_target_after(t).is_some() {
                match redirect_write_target(&tokens, i) {
                    Some(target) if is_safe_device_write(target) => {},
                    Some(target) if is_plan_file_path(workdir, target, plan_file) => {
                        saw_plan_redirect = true;
                        // Strip the redirect so the remainder must stand on
                        // its own as read-only: glued (`>path`) is one token,
                        // a bare operator consumes the following target too.
                        if redirect_target_after(t).is_some_and(|g| !g.is_empty()) {
                            continue;
                        }
                        skip_next = true;
                        continue;
                    },
                    _ => return false,
                }
            }
            kept.push(tok.clone());
        }
        if classify_segment(&kept) != RiskClass::ReadOnly {
            return false;
        }
    }
    saw_plan_redirect
}

/// True when the segment writes a real file: `tee`/`dd`, or an output
/// redirect whose target is not one of the safe discard devices. Mirrors the
/// redirect handling in `classify_segment`, which folds these into the
/// severity ranking rather than reporting them separately.
pub(crate) fn segment_has_file_write(tokens: &[String]) -> bool {
    tokens.iter().enumerate().any(|(i, tok)| {
        let t = tok.as_str();
        if t == "tee" || t == "dd" {
            return true;
        }
        if redirect_target_after(t).is_some() {
            return !matches!(
                redirect_write_target(tokens, i),
                Some(target) if is_safe_device_write(target)
            );
        }
        false
    })
}

/// One pipeline segment whose head is a known build tool running a known
/// build/test subcommand. The head must be argv[0] directly — a wrapper
/// (`sudo`, `env`, `xargs`) in front refuses even though `classify_segment`
/// would look through it, because the wrapper changes what actually runs.
pub(crate) fn segment_is_safe_build(tokens: &[String]) -> bool {
    let Some(head) = tokens.first().map(|t| basename(t)) else {
        return false;
    };
    // First positional token after argv[0]; cargo's `+toolchain` selector is
    // a channel pin, not a subcommand.
    let mut positional = tokens
        .iter()
        .skip(1)
        .map(String::as_str)
        .filter(|t| !t.starts_with('-') && !t.starts_with('+'));
    let sub = positional.next();
    let second = positional.next();
    match head {
        "cargo" => match sub {
            Some(
                "check" | "build" | "test" | "clippy" | "doc" | "bench" | "tree" | "metadata"
                | "fetch" | "verify-project",
            ) => true,
            // `cargo nextest run` — nextest's only non-mutating verb.
            Some("nextest") => matches!(second, Some("run") | Some("list")),
            // `cargo fmt` rewrites sources; only the check form is a read.
            Some("fmt") => tokens.iter().any(|t| t == "--check"),
            _ => false,
        },
        "go" => matches!(sub, Some("build" | "test" | "vet")),
        // npm-family: the bare test verb and the conventional check scripts.
        // `install`/`ci` mutate node_modules and reach the network — refused.
        "npm" | "pnpm" | "yarn" | "bun" => match sub {
            Some("test") => true,
            Some("run") => matches!(
                second,
                Some("test" | "build" | "lint" | "check" | "typecheck")
            ),
            _ => false,
        },
        // Recipes are opaque, so only the conventional build/verify targets
        // (or the bare default) are allowed — `make deploy` refuses.
        "make" => matches!(
            sub,
            None | Some("all" | "build" | "test" | "check" | "lint")
        ),
        _ => false,
    }
}

/// The environment variable the exec tool exports so a shell command can name
/// the session scratchpad. It is expanded to its literal value before the
/// containment proof runs — we know this one variable's value, and refusing
/// it outright would make the advertised handle unusable, which is exactly
/// how a prompt that says "use the scratchpad" met a gate that refused every
/// spelling of it.
pub const SCRATCHPAD_ENV_VAR: &str = "MERMAID_SCRATCHPAD";

/// Substitute `$MERMAID_SCRATCHPAD` / `${MERMAID_SCRATCHPAD}` with `scratch`.
/// Returns `None` when the path is not valid UTF-8, so the caller fails closed
/// rather than proving anything about a lossy spelling.
fn expand_scratch_var(command: &str, scratch: &Path) -> Option<String> {
    let value = scratch.to_str()?;
    Some(
        command
            .replace(&format!("${{{SCRATCHPAD_ENV_VAR}}}"), value)
            .replace(&format!("${SCRATCHPAD_ENV_VAR}"), value),
    )
}

/// One token of a scratch-only command. Rules, all fail-closed:
/// - `..` anywhere: rejected (can climb out of the scratch cwd).
/// - `:/` anywhere: rejected (URL / remote-host / list-of-paths shapes).
/// - Drive-designator shape (`C:x`, `c:\x`): rejected on every platform —
///   on Windows it targets a drive root or a per-drive cwd, never scratch.
/// - No path separator: fine — a bare word, flag, or PATH-resolved argv0.
/// - Rooted: must sit lexically inside the scratchpad. `has_root`, not
///   `is_absolute` — on Windows `/etc/passwd` is rooted but not "absolute"
///   (no drive prefix), yet still escapes the scratch cwd via the drive
///   root, so every rooted token gets the containment check.
/// - Relative with a separator: accepted only as a PLAIN path (no leading
///   `-`, no `=`) so flag-embedded paths (`-C/etc`, `--directory=/etc`,
///   `VAR=/etc`) can't smuggle a target past the rooted check.
#[must_use]
pub fn token_provably_in_scratch(token: &str, scratch: &Path) -> bool {
    if token.contains("..") || token.contains(":/") {
        return false;
    }
    let bytes = token.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return false;
    }
    if !token.contains(['/', '\\']) {
        return true;
    }
    if Path::new(token).has_root() {
        return Path::new(token).starts_with(scratch);
    }
    !token.starts_with('-') && !token.contains('=')
}

/// Whether `segment` carries one of `head`'s escape flags from
/// [`SCRATCH_TOOL_ESCAPE_FLAGS`]. Mirrors `writes_through_a_flag`: short flags
/// match inside a bundle and attached, long flags bare or `=`-valued.
fn escapes_through_a_flag(head: &str, segment: &[String]) -> bool {
    SCRATCH_TOOL_ESCAPE_FLAGS
        .iter()
        .filter(|(h, _, _)| *h == head)
        .any(|(_, shorts, longs)| {
            shorts.iter().any(|c| segment_has_flag(segment, *c, "\0"))
                || longs.iter().any(|l| segment_has_flag(segment, '\0', l))
        })
}

/// True when `command`, run with its working directory inside `scratch`,
/// provably touches nothing outside it.
///
/// This is the authorization for plan mode's scratchpad carve-out. The OS
/// write-confinement runs beneath it as defense-in-depth, NOT in place of it:
/// the kill-switch spares `AF_UNIX` (so `systemd-run --user` would escape into
/// an unconfined child) and Landlock carries no mode/owner/xattr right (so
/// `chmod -R go+w ~/.ssh` would not be confined). Both are unreachable here
/// because neither binary can be a segment head.
///
/// What is permitted, and why each is safe to permit:
/// - separators `|`, `&&`, `||`, `;` — [`split_command`] gives us each segment
///   and every one is proven independently, so a chain cannot smuggle a head
///   the proof never saw;
/// - `$MERMAID_SCRATCHPAD` — one variable whose value we set ourselves,
///   expanded to its literal path before anything else runs;
/// - redirects whose target resolves inside the scratchpad, plus the safe
///   discard devices.
///
/// What is refused: command/process substitution, backticks, any other `$`,
/// `~`, globs, heredocs, `tee`/`dd`, cwd-changing builtins (they would
/// relocate every relative path this proof resolved against the scratch cwd),
/// and any head outside [`READ_ONLY_BINARIES`] + [`SCRATCH_TOOLS`].
///
/// Dialect-dispatched on [`HostShell::current`](super::HostShell::current),
/// same as risk classification. PowerShell has no scratch carve-out yet: it
/// returns `false`, so the gate falls through to the plan denial.
#[must_use]
pub fn is_scratch_only_command(command: &str, scratch: &Path) -> bool {
    match super::HostShell::current() {
        super::HostShell::PowerShell => false,
        super::HostShell::Posix => is_scratch_only_command_posix(command, scratch),
    }
}

pub(in crate::policy) fn is_scratch_only_command_posix(command: &str, scratch: &Path) -> bool {
    // The scratch root must be absolute for `starts_with` containment to mean
    // anything; a relative root would match by prefix from anywhere.
    if !scratch.has_root() {
        return false;
    }
    let Some(command) = expand_scratch_var(command, scratch) else {
        return false;
    };
    // Everything opaque to token-level reasoning, checked on the RAW string so
    // even a quoted occurrence fails closed. `$` survives only as the variable
    // already expanded above; anything left is unknown text.
    if command.contains(['$', '`', '~', '*', '?', '[', ']']) {
        return false;
    }
    let split = split_command(&command);
    if split.segments.is_empty() || !split.heredocs.is_empty() {
        return false;
    }
    if split
        .segments
        .iter()
        .any(|seg| !extract_substitutions(seg).is_empty())
    {
        return false;
    }
    split
        .segments
        .iter()
        .all(|seg| segment_is_scratch_only(seg, scratch))
}

fn segment_is_scratch_only(segment: &str, scratch: &Path) -> bool {
    let tokens = tokenize(segment);
    let mut kept: Vec<String> = Vec::with_capacity(tokens.len());
    let mut skip_next = false;
    for (i, tok) in tokens.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        let t = tok.as_str();
        // `tee`/`dd` write through an argument the redirect scan never sees.
        if t == "tee" || t == "dd" {
            return false;
        }
        // A cwd change relocates every relative path proven against the
        // scratch cwd, so the proof would be describing a different directory
        // than the one the command runs in.
        if CWD_CHANGING_BUILTINS
            .iter()
            .any(|b| basename(t).eq_ignore_ascii_case(b))
        {
            return false;
        }
        if redirect_target_after(t).is_some() {
            match redirect_write_target(&tokens, i) {
                Some(target)
                    if is_safe_device_write(target)
                        || token_provably_in_scratch(target, scratch) =>
                {
                    // Strip the redirect so the remainder stands on its own:
                    // glued (`>path`) is one token, a bare operator consumes
                    // the following target too.
                    if redirect_target_after(t).is_some_and(|g| !g.is_empty()) {
                        continue;
                    }
                    skip_next = true;
                    continue;
                },
                _ => return false,
            }
        }
        kept.push(tok.clone());
    }
    let Some(head) = kept.first().map(|h| basename(h).to_string()) else {
        return false;
    };
    // Every surviving token must stay inside the scratchpad.
    if !kept.iter().all(|t| token_provably_in_scratch(t, scratch)) {
        return false;
    }
    if SCRATCH_TOOLS.contains(&head.as_str()) {
        return !escapes_through_a_flag(&head, &kept);
    }
    // Anything else must stand on its own as read-only. This runs the full
    // classifier, so `READ_ONLY_WRITE_FLAGS` (`sort -o`, `git --output`) and
    // the unknown-head fail-safe both still apply.
    classify_segment(&kept) == RiskClass::ReadOnly
}

#[cfg(test)]
mod scratch_tests {
    use super::*;

    fn scratch() -> &'static Path {
        Path::new("/tmp/mermaid-1000/proj/sess/scratchpad")
    }

    fn ok(cmd: &str) -> bool {
        is_scratch_only_command_posix(cmd, scratch())
    }

    #[test]
    fn the_deb_inspection_that_started_this_is_allowed() {
        // The command shape from the field report: read-only probes chained
        // with a fallback that extracts into the scratchpad. Every piece of
        // this was refused before — `|`, `&&`, `;`, `$MERMAID_SCRATCHPAD`,
        // and `ar`/`tar` as unknown heads.
        assert!(ok("ar t /tmp/mermaid-1000/proj/sess/scratchpad/pkg.deb"));
        assert!(ok(
            "ar x $MERMAID_SCRATCHPAD/pkg.deb && tar -tJf control.tar.xz"
        ));
        assert!(ok(
            "dpkg-deb -c ${MERMAID_SCRATCHPAD}/pkg.deb | head -n 100"
        ));
        assert!(ok("tar -xJf data.tar.xz; ls -la"));
        assert!(ok("ar t pkg.deb > $MERMAID_SCRATCHPAD/listing.txt"));
        assert!(ok("file pkg.deb 2>/dev/null"));
    }

    #[test]
    fn escapes_are_refused() {
        // Head not on either allowlist -- these are the AF_UNIX and metadata
        // escapes the OS sandbox does NOT contain, so the head allowlist is
        // what has to stop them.
        //
        // `busctl` is the load-bearing case: it carries NO path-shaped
        // argument, so token containment has nothing to reject and the head
        // allowlist is the only thing standing between a plan and an
        // unconfined child over D-Bus. Deleting the allowlist leaves the
        // `systemd-run` line below still passing (its `/bin/sh` is a rooted
        // token outside scratch) while this one silently starts to run --
        // which is why both spellings are here.
        assert!(!ok("busctl --user call x y z w"));
        assert!(!ok("systemd-run --user /bin/sh -c true"));
        assert!(!ok("dbus-send --session --print-reply x"));
        assert!(!ok("chmod -R go+w /home/u/.ssh"));
        assert!(!ok("kill -9 -1"));
        assert!(!ok("docker run -v /:/host alpine"));
        assert!(!ok("curl https://evil.example"));

        // Allowlisted head, escape flag.
        assert!(!ok("tar -C /etc -xf pkg.tar"));
        assert!(!ok("tar --directory=/etc -xf pkg.tar"));
        assert!(!ok("tar -I /bin/sh -xf pkg.tar"));
        assert!(!ok("tar --to-command=id -xf pkg.tar"));
        assert!(!ok("unzip -d /etc pkg.zip"));

        // Allowlisted head, argument leaving the scratchpad.
        assert!(!ok("tar -xf /etc/shadow"));
        assert!(!ok("ar x ../../escape.deb"));
        assert!(!ok("tar -xf /tmp/other/pkg.tar"));

        // Opaque constructs.
        assert!(!ok("ar x $(curl evil)"));
        assert!(!ok("ar x `curl evil`"));
        assert!(!ok("ar x ~/pkg.deb"));
        assert!(!ok("ar x *.deb"));
        assert!(!ok("ar x $HOME/pkg.deb"));

        // Redirect leaving the scratchpad, and the write-through-argument pair.
        assert!(!ok("ar t pkg.deb > /etc/passwd"));
        assert!(!ok("ar t pkg.deb > ../out.txt"));
        assert!(!ok("ar t pkg.deb | tee /etc/passwd"));
        assert!(!ok("dd if=pkg.deb of=/dev/sda"));

        // A cwd change would relocate every relative path just proven.
        assert!(!ok("cd /etc && tar -xf pkg.tar"));

        // Worst-segment: one bad segment poisons the chain.
        assert!(!ok("ar t pkg.deb && rm -rf /"));
        assert!(!ok("ls && systemd-run --user true"));
    }

    #[test]
    fn a_relative_scratch_root_proves_nothing() {
        // `starts_with` on a relative root would match by prefix from
        // anywhere, so the proof must refuse to run at all.
        assert!(!is_scratch_only_command_posix(
            "ls",
            Path::new("scratchpad")
        ));
    }

    #[test]
    fn read_only_write_flags_still_apply_to_readers() {
        // The reader half goes through `classify_segment`, so the existing
        // output-flag table keeps working inside the scratchpad.
        assert!(ok("sort listing.txt"));
        assert!(!ok("sort -o /etc/passwd listing.txt"));
        assert!(!ok("git log --output=/etc/passwd"));
    }
}
