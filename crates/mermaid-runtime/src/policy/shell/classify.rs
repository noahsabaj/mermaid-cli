//! Risk classification for a shell command, given the lexer's segments.

use super::super::RiskClass;
use super::destructive::*;
use super::lexer::*;
use super::tables::*;

pub(crate) fn shell_severity(risk: RiskClass) -> u8 {
    match risk {
        RiskClass::ReadOnly => 0,
        RiskClass::ShellMutation => 1,
        RiskClass::Process => 2,
        RiskClass::Network | RiskClass::SystemMutation => 3,
        RiskClass::Destructive => 4,
        _ => 1,
    }
}

pub(crate) fn shell_max(a: RiskClass, b: RiskClass) -> RiskClass {
    if shell_severity(a) >= shell_severity(b) {
        a
    } else {
        b
    }
}

/// Classify a single pipeline segment's command head (basename of argv[0]).
pub(crate) fn classify_head(head: &str, segment: &[String]) -> RiskClass {
    if NETWORK_BINARIES.contains(&head) {
        return RiskClass::Network;
    }
    if head == "git" {
        let sub = segment
            .iter()
            .skip(1)
            .find(|t| !t.starts_with('-'))
            .map(|s| s.as_str());
        return match sub {
            Some(s) if GIT_READ_ONLY.contains(&s) => RiskClass::ReadOnly,
            Some("clone") | Some("fetch") | Some("pull") | Some("push") => RiskClass::Network,
            _ => RiskClass::ShellMutation,
        };
    }
    // `awk` is Turing-complete: field/pattern forms only read, but a program
    // can write (`print > f`), exec (`system()`, `| "cmd"`), or edit in place
    // (gawk `-i inplace`). Inspect the program so the ubiquitous read-only
    // idiom (`awk '{print $1}'`) isn't blanket-blocked while writes stay gated.
    if matches!(head, "awk" | "gawk" | "mawk" | "nawk") {
        return classify_awk(segment);
    }
    // `find` is read-only only without an action primitive: `-exec`/`-ok` run an
    // arbitrary command, `-delete`/`-fprint*`/`-fls` write or delete. argv0-only
    // classification rated all of these ReadOnly (RC-2).
    if head == "find" {
        return classify_find(segment);
    }
    // `sort -o <file>` / `--output=` writes through an argument, not a redirect,
    // so the redirect scan never sees it (RC-2).
    if head == "sort" && sort_writes_file(segment) {
        return RiskClass::ShellMutation;
    }
    // `yq -i` / `--inplace` rewrites the file in place — a mutation the argv0
    // read-only rating would otherwise auto-run (`jq` has no such flag, so it
    // stays read-only). Same shape as the `sort -o` guard above.
    if head == "yq" && segment_has_flag(segment, 'i', "inplace") {
        return RiskClass::ShellMutation;
    }
    // `date -s` / `--set` sets the system clock — a control action, not the
    // read that displaying a date (`date`, `date +%s`, `date -d …`) is.
    if head == "date" && segment_has_flag(segment, 's', "set") {
        return RiskClass::ShellMutation;
    }
    if system_install_shape(head, segment) {
        return RiskClass::SystemMutation;
    }
    if PROCESS_BINARIES.contains(&head) {
        return RiskClass::Process;
    }
    if READ_ONLY_BINARIES.contains(&head) {
        return RiskClass::ReadOnly;
    }
    // PowerShell cmdlet heads, matched case-insensitively like PowerShell
    // itself. Remote/download cmdlets rate Network, arbitrary-code launchers
    // rate Process, the audited pure readers rate ReadOnly; everything else
    // (Set-*, Remove-*, New-*, Out-File, scriptblock pipelines) falls through
    // to the mutation default below.
    let ps_head = head.to_ascii_lowercase();
    if matches!(
        ps_head.as_str(),
        "invoke-webrequest"
            | "invoke-restmethod"
            | "iwr"
            | "irm"
            | "invoke-command"
            | "icm"
            | "enter-pssession"
            | "new-pssession"
    ) {
        return RiskClass::Network;
    }
    if matches!(
        ps_head.as_str(),
        "invoke-expression" | "iex" | "invoke-item" | "ii" | "start-process" | "saps" | "start"
    ) {
        return RiskClass::Process;
    }
    if PS_READ_ONLY_CMDLETS.contains(&ps_head.as_str()) {
        return RiskClass::ReadOnly;
    }
    // Unknown binary ⇒ assume it can mutate. This is the safe default.
    RiskClass::ShellMutation
}

/// Machine-scoped package operations — see `RiskClass::SystemMutation`.
/// `sudo`/`env` wrappers are stripped by the caller, so `head` is the
/// manager itself; matching is case-insensitive for the Windows managers.
/// Project-local installs (`npm install`, `cargo add`, `yarn add`)
/// deliberately return false — they land inside the project and stay
/// Process.
pub(crate) fn system_install_shape(head: &str, segment: &[String]) -> bool {
    let head = head.to_ascii_lowercase();
    let sub = segment
        .iter()
        .skip(1)
        .find(|t| !t.starts_with('-'))
        .map(|s| s.to_ascii_lowercase());
    let sub = sub.as_deref();
    let global_flag = segment.iter().skip(1).any(|t| {
        t == "--global" || (t.starts_with('-') && !t.starts_with("--") && t[1..].contains('g'))
    });
    const INSTALL_VERBS: &[&str] = &[
        "install",
        "add",
        "uninstall",
        "remove",
        "update",
        "upgrade",
        "link",
    ];
    match head.as_str() {
        // JS package managers: only the GLOBAL forms are machine-scoped.
        "npm" | "pnpm" | "bun" => sub.is_some_and(|s| INSTALL_VERBS.contains(&s)) && global_flag,
        // yarn v1 spells it `yarn global add`.
        "yarn" => {
            sub == Some("global")
                || (sub.is_some_and(|s| INSTALL_VERBS.contains(&s)) && global_flag)
        },
        // Toolchain installers that land in machine-wide bin dirs.
        "cargo" => matches!(sub, Some("install" | "uninstall")),
        "go" => sub == Some("install"),
        "gem" => matches!(sub, Some("install" | "uninstall" | "update")),
        // pipx exists to install global tools; pip's venv membership is
        // undetectable from the command string, so it fails toward vetting
        // (ask/auto/read_only behavior is unchanged — installs were already
        // gated there).
        "pipx" => true,
        "pip" | "pip2" | "pip3" => matches!(sub, Some("install" | "uninstall")),
        "dotnet" => {
            sub == Some("tool")
                && segment
                    .iter()
                    .skip(1)
                    .filter(|t| !t.starts_with('-'))
                    .nth(1)
                    .is_some_and(|s| {
                        matches!(
                            s.to_ascii_lowercase().as_str(),
                            "install" | "uninstall" | "update"
                        )
                    })
        },
        // OS package managers: any mutating verb is machine-scoped.
        "brew" | "apt" | "apt-get" | "dnf" | "yum" | "zypper" | "apk" | "snap" | "flatpak"
        | "choco" | "scoop" | "winget" | "port" => matches!(
            sub,
            Some(
                "install"
                    | "uninstall"
                    | "remove"
                    | "purge"
                    | "upgrade"
                    | "update"
                    | "add"
                    | "dist-upgrade"
            )
        ),
        // pacman mutates via -S/-R/-U flag groups.
        "pacman" => segment
            .iter()
            .skip(1)
            .any(|t| t.starts_with("-S") || t.starts_with("-R") || t.starts_with("-U")),
        _ => false,
    }
}

/// Classify an `awk` invocation by inspecting its program + flags. Read-only
/// unless it can write, exec, or run un-inspectable external code. Every awk
/// side effect needs one of a small set of surface markers, so a conservative
/// scan for them can't miss a mutation (worst case it OVER-blocks a benign
/// `$1 > 5` comparison — the safe direction):
///   - file write: `print`/`printf` `> f` / `>> f` ⇒ contains `>`
///   - command exec: `system(...)`, `print | "cmd"`, `"cmd" | getline`
///     ⇒ contains `system` or `|`
///   - in-place / extension load: gawk `-i` (`--include`) ⇒ arbitrary code
///   - external program: `-f file` / `--file` ⇒ can't be inspected
///
/// `-F`/`-v` (and long forms) carry DATA, not code — a `>`/`|`/`system` in a
/// field separator or variable value is a literal string, never executed — so
/// those tokens are skipped before the marker scan.
pub(crate) fn classify_awk(segment: &[String]) -> RiskClass {
    for tok in segment.iter().skip(1) {
        let t = tok.as_str();
        // Field separator / variable assignment: value is data, scan-exempt.
        if t.starts_with("-F")
            || t.starts_with("-v")
            || t.starts_with("--field-separator")
            || t.starts_with("--assign")
        {
            continue;
        }
        // Extension load (`-i`, gawk `--include`) or external program
        // (`-f`/`--file`): arbitrary or un-inspectable code.
        if t == "-i"
            || (t.starts_with("-i") && t.len() > 2)
            || t == "-f"
            || (t.starts_with("-f") && t.len() > 2)
            || t.starts_with("--include")
            || t.starts_with("--file")
        {
            return RiskClass::ShellMutation;
        }
        // Program / data / inline-source tokens: any output redirect is a
        // write; a command pipe or `system()` is code execution.
        if t.contains('>') {
            return RiskClass::ShellMutation;
        }
        if t.contains('|') || t.contains("system") {
            return RiskClass::Process;
        }
    }
    RiskClass::ReadOnly
}

/// `find` only reads the tree unless it carries an action primitive. `-exec`/
/// `-execdir`/`-ok`/`-okdir` run an arbitrary command (Process); `-delete`/
/// `-fprint`/`-fprint0`/`-fprintf`/`-fls` write or delete (`ShellMutation`).
pub(crate) fn classify_find(segment: &[String]) -> RiskClass {
    let mut worst = RiskClass::ReadOnly;
    for tok in segment.iter().skip(1) {
        match tok.as_str() {
            "-exec" | "-execdir" | "-ok" | "-okdir" => return RiskClass::Process,
            "-delete" | "-fprint" | "-fprint0" | "-fprintf" | "-fls" => {
                worst = shell_max(worst, RiskClass::ShellMutation);
            },
            _ => {},
        }
    }
    worst
}

/// True when a `sort` invocation writes its output to a file via `-o`/`--output`
/// (incl. the glued `-oFILE` and bundled `-bo FILE` getopt forms, where the
/// last flag char consumes the path).
pub(crate) fn sort_writes_file(segment: &[String]) -> bool {
    segment.iter().skip(1).any(|t| {
        let t = t.as_str();
        if t == "--output" || t.starts_with("--output=") {
            return true;
        }
        match t.strip_prefix('-') {
            Some(short) if !t.starts_with("--") && !short.is_empty() => {
                short.starts_with('o') || short.ends_with('o')
            },
            _ => false,
        }
    })
}

/// Classify `command` for the shell it will ACTUALLY run under.
///
/// On Windows the exec tool hands every model command to PowerShell
/// (`shell_invocation` picks `pwsh`/`powershell`, never `sh`), so risk must
/// be read from PowerShell grammar; elsewhere commands run under `sh` and
/// keep the POSIX classifier. Keep this predicate in lockstep with
/// `shell_invocation` — classifying for a different interpreter than the one
/// that executes is exactly the bug that made plan/read-only mode deny every
/// read-only PowerShell pipeline on Windows while missing PS-only writes
/// (`*> file`).
pub(in crate::policy) fn classify_command_for_host_shell(command: &str) -> RiskClass {
    if cfg!(target_os = "windows") {
        super::powershell::classify_powershell_command(command)
    } else {
        classify_shell_command(command)
    }
}

/// Classify a shell command by splitting it into the command segments
/// `sh -c` would run (so flag reordering, extra whitespace, absolute paths,
/// and chaining — including glued operators and newlines — can't downgrade the
/// risk) and taking the most dangerous segment.
pub(crate) fn classify_shell_command(command: &str) -> RiskClass {
    classify_shell_command_depth(command, 0)
}

pub(crate) fn classify_shell_command_depth(command: &str, depth: u8) -> RiskClass {
    if contains_destructive_pattern(command) {
        return RiskClass::Destructive;
    }
    let mut worst = RiskClass::ReadOnly;
    let split = split_command(command);
    for segment in &split.segments {
        worst = shell_max(worst, classify_segment(&tokenize(segment)));
        // Descend into any command/process substitution the segment hides, so a
        // mutation wrapped in `$(…)`/backticks can't classify as the benign head
        // that precedes it (#F1). Worst segment — outer or inner — wins.
        if depth < MAX_SUBST_DEPTH {
            for body in extract_substitutions(segment) {
                worst = shell_max(worst, classify_shell_command_depth(&body, depth + 1));
            }
        } else if !extract_substitutions(segment).is_empty() {
            // At the recursion cap with substitutions still nested below, we can no
            // longer prove the hidden payload is benign — so fail SAFE instead of
            // riding the (possibly ReadOnly) outer classification. Forcing at least
            // ShellMutation means a deeply-nested `$(…$(rm -rf /)…)` can never
            // auto-run in read_only/auto; it routes to deny / approval / classify.
            // (Backstop: `contains_destructive_pattern` above already fails safe on
            // deep nesting, but this keeps the classifier independently sound.)
            worst = shell_max(worst, RiskClass::ShellMutation);
        }
    }
    // Heredoc bodies are data, not commands — but an EXPANDING body
    // (`<<EOF`, unquoted delimiter) really executes its `$(…)`/backticks, so
    // those substitutions classify like any other. Quote-BLIND extraction:
    // heredoc bodies have no shell quoting context, so `'$(…)'` still
    // expands there. Quoted-delimiter bodies are pure literals — skipped
    // entirely (the raw-text destructive scan above still covers them).
    for hd in &split.heredocs {
        if !hd.expands {
            continue;
        }
        let bodies = extract_substitutions_quote_blind(&hd.body);
        if depth < MAX_SUBST_DEPTH {
            for body in &bodies {
                worst = shell_max(worst, classify_shell_command_depth(body, depth + 1));
            }
        } else if !bodies.is_empty() {
            worst = shell_max(worst, RiskClass::ShellMutation);
        }
        // Belt-and-braces: substitution syntax the extractor somehow missed
        // (malformed nesting, exotic quoting) floors the segment — same
        // spirit as the recursion-cap fail-safe above.
        if bodies.is_empty()
            && (hd.body.contains("$(") || hd.body.contains('`') || hd.body.contains("<("))
        {
            worst = shell_max(worst, RiskClass::ShellMutation);
        }
    }
    worst
}

/// Classify one command segment (no top-level chaining operators) by its head
/// and any file-writing redirection.
pub(crate) fn classify_segment(tokens: &[String]) -> RiskClass {
    let mut worst = RiskClass::ReadOnly;
    let mut expect_head = true;
    let mut after_wrapper = false;
    for (i, tok) in tokens.iter().enumerate() {
        let t = tok.as_str();
        // A file redirection (incl. `1>`/`2>>`/`&>`), `tee`, or `dd` writes —
        // EXCEPT redirects to the safe character devices (`2>/dev/null` and
        // friends), which discard data and leave the segment read-only.
        // Blanket-flagging every redirect denied ubiquitous read-only shapes
        // like `ls 2>/dev/null` in read_only mode (user report, v0.14.0).
        if t == "tee" || t == "dd" {
            worst = shell_max(worst, RiskClass::ShellMutation);
        } else if redirect_target_after(t).is_some() {
            match redirect_write_target(tokens, i) {
                Some(target) if is_safe_device_write(target) => {},
                // Unresolvable (dangling `>`) or a real file: a write.
                _ => worst = shell_max(worst, RiskClass::ShellMutation),
            }
        }
        if !expect_head {
            continue;
        }
        let head = basename(t);
        // `command -v/-V NAME` only LOOKS UP name (the POSIX binary-exists
        // test) — nothing is executed, regardless of what NAME is. Plain
        // `command NAME …` executes NAME and falls through to the wrapper
        // skip below.
        if t == "command"
            && tokens[i + 1..]
                .iter()
                .take_while(|a| a.starts_with('-'))
                .any(|a| a == "-v" || a == "-V")
        {
            expect_head = false;
            continue;
        }
        // Skip `FOO=bar` env assignments and benign wrappers; the real head
        // is a later token.
        if (t.contains('=') && !t.starts_with('-') && !t.contains('/')) || WRAPPERS.contains(&head)
        {
            after_wrapper = true;
            continue;
        }
        // A wrapper's own flags (`sudo -u`, `env -i`, `command -p`) precede
        // the real head — a command name can't begin with `-`, so a dash
        // token here was previously misread as an unknown head and escalated
        // to ShellMutation (`command -v rg` denied in read_only). Only
        // skipped AFTER a wrapper so a bare dash-leading segment keeps its
        // fail-safe classification.
        if after_wrapper && t.starts_with('-') {
            continue;
        }
        worst = shell_max(worst, classify_head(head, &tokens[i..]));
        expect_head = false;
    }
    worst
}

pub(crate) fn is_dangerous_root(arg: &str) -> bool {
    // Collapse a trailing glob/dot/slash so `/etc`, `/etc/`, `/etc/*`, `/etc/.`
    // and `/usr/*` all reduce to the same root, and treat `${VAR}` as `$VAR`.
    // The caller lowercases the whole command before tokenizing, so the old
    // uppercase `$HOME`/`${HOME}` arms were dead code (RC-3); match in lowercase.
    let a = arg.trim_matches(['"', '\'']);
    let a = a.strip_suffix("/*").unwrap_or(a);
    let a = a.strip_suffix("/.").unwrap_or(a);
    let a = a.strip_suffix('/').unwrap_or(a);
    let normalized = a.replace("${", "$").replace('}', "");
    // Collapse interior `..` so `/etc/../etc` can't disguise `/etc` (#F3).
    let collapsed = collapse_parent_refs(&normalized);
    // Strip a trailing slash so a path that collapses to bare `/` via interior
    // `..` (e.g. `/etc/..` → `/`) reduces to "" and trips the root check (#F3).
    let a = collapsed.strip_suffix('/').unwrap_or(&collapsed);
    if a.is_empty() {
        // Was `/`, `/*`, `/.`, or collapsed to the filesystem root.
        return true;
    }
    if matches!(
        a,
        "~" | "$home"
            | "."
            | ".."
            | "*"
            | "/etc"
            | "/usr"
            | "/var"
            | "/home"
            | "/boot"
            | "/lib"
            | "/lib64"
            | "/bin"
            | "/sbin"
            | "/sys"
            | "/dev"
            | "/root"
            | "/opt"
    ) {
        return true;
    }
    // Windows roots. The POSIX shell tokenizer can strip backslashes, so match
    // drive roots leniently in both `c:\…` and stripped `c:…` forms. Best-effort
    // (the gate is the real boundary).
    let aw = a.to_ascii_lowercase();
    matches!(
        aw.as_str(),
        "c:" | "c:\\"
            | "c:/"
            | "\\"
            | "%systemroot%"
            | "%systemdrive%"
            | "%userprofile%"
            | "%homepath%"
    ) || aw.starts_with("c:\\windows")
        || aw.starts_with("c:/windows")
        || aw.starts_with("c:windows")
        || aw.starts_with("c:\\users")
        || aw.starts_with("c:/users")
        || aw.starts_with("c:users")
}

/// Detect a fork bomb: a function defined and then piped into itself in the
/// background. Catches the canonical `:(){ :|:& };:` and renamed variants like
/// `b(){ b|b& };b`. Operates on the whitespace-stripped, lowercased command.
pub(crate) fn is_fork_bomb(nospace: &str) -> bool {
    // Canonical `:` bomb — fast path (`:` isn't an identifier char, so the
    // generic scan below skips it).
    if nospace.contains(":(){") || nospace.contains(":|:&") {
        return true;
    }
    let bytes = nospace.as_bytes();
    let mut search = 0;
    while let Some(rel) = nospace[search..].find("(){") {
        let def_at = search + rel;
        // Walk back over the identifier immediately preceding `(){`. These are
        // ASCII byte comparisons, so `start` lands on a char boundary.
        let mut start = def_at;
        while start > 0 {
            let c = bytes[start - 1];
            if c.is_ascii_alphanumeric() || c == b'_' {
                start -= 1;
            } else {
                break;
            }
        }
        if start < def_at {
            let name = &nospace[start..def_at];
            // The recursive self-pipe into the background: `name|name&`.
            if nospace.contains(&format!("{name}|{name}&")) {
                return true;
            }
        }
        search = def_at + 3;
    }
    false
}

/// True if `segment` (past argv0) carries a specific flag in any spelling:
/// `--<long>` (incl. `--<long>=value`), or a single-dash bundle containing the
/// short char (`-i`, `-Pi`). Used to catch the one write flag on an otherwise
/// read-only tool (`yq -i`, `date -s`) without a bespoke scan per tool.
pub(crate) fn segment_has_flag(segment: &[String], short: char, long: &str) -> bool {
    segment.iter().skip(1).any(|t| {
        if let Some(rest) = t.strip_prefix("--") {
            rest == long || rest.split('=').next() == Some(long)
        } else if let Some(bundle) = t.strip_prefix('-') {
            !bundle.is_empty()
                && bundle.chars().all(|c| c.is_ascii_alphanumeric())
                && bundle.contains(short)
        } else {
            false
        }
    })
}

/// True if any token is a short flag (`-rf`) or long flag (`--recursive`)
/// conveying `want` (`'r'` recursive / `'f'` force).
pub(crate) fn flag_present(tokens: &[String], want: char) -> bool {
    tokens.iter().any(|t| {
        if let Some(long) = t.strip_prefix("--") {
            (want == 'r' && long == "recursive") || (want == 'f' && long == "force")
        } else if let Some(short) = t.strip_prefix('-') {
            !short.is_empty()
                && short.chars().all(|c| c.is_ascii_alphabetic())
                && short.contains(want)
        } else {
            false
        }
    })
}

/// Shell interpreters whose `-c <script>` payload we recurse into so a
/// destructive command can't hide inside a quoted argument.
pub(crate) const SHELL_INTERPRETERS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "ash"];
