//! Plan-mode gating: which writes and which build commands a plan may run.

use std::path::Path;

use super::RiskClass;
use super::shell::*;

/// Marker embedded verbatim in every read-only policy-denial `reason` (see
/// `PolicyEngine::decide`). Exposed so the message-history layer can detect a
/// denial that a since-loosened safety mode has superseded, without
/// re-hardcoding the wording in a second place.
pub const READ_ONLY_DENIAL_MARKER: &str = "read-only safety mode";

/// Marker embedded verbatim in every plan-mode policy-denial `reason` (the
/// policy gate rewrites the read-only mode-default deny to a plan-flavored one
/// while a plan is being drafted). Sibling of [`READ_ONLY_DENIAL_MARKER`]: the
/// message-history layer matches `"blocked by policy: "` + this marker to
/// neutralize denials once plan mode ends.
pub const PLAN_DENIAL_MARKER: &str = "plan mode";

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
pub fn is_plan_safe_build_command(command: &str) -> bool {
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

/// Builtins that move the shell's own working directory. They are `ReadOnly`
/// for risk purposes (nothing outside the shell changes), but any lexical
/// path match against a fixed workdir becomes unsound once one of these runs.
pub(crate) const CWD_CHANGING_BUILTINS: &[&str] = &["cd", "pushd", "popd"];

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
pub fn is_plan_file_only_write(command: &str, workdir: &Path, plan_file: &Path) -> bool {
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
            if CWD_CHANGING_BUILTINS.contains(&basename(t)) {
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
