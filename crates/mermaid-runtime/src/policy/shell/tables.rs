//! Static allowlist tables. Data, not logic.

/// Command heads (argv[0] basenames) that only read state and are safe to
/// auto-run. Anything NOT in this set is treated as at least a mutation — the
/// safe default is "unknown ⇒ requires approval", inverting the old
/// allowlist-of-mutations that let `curl`/`kill`/`chmod`/installers run as
/// "read-only".
pub(crate) const READ_ONLY_BINARIES: &[&str] = &[
    "ls",
    "cat",
    "bat",
    "head",
    "tail",
    "wc",
    "stat",
    "file",
    "pwd",
    "echo",
    "printf",
    "grep",
    "egrep",
    "fgrep",
    "rg",
    "ag",
    "ack",
    "fd",
    "tree",
    "du",
    "df",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "whoami",
    "id",
    "date",
    "env",
    "printenv",
    "which",
    "type",
    "uname",
    "hostname",
    "cksum",
    "md5sum",
    "sha1sum",
    "sha256sum",
    "diff",
    "cmp",
    "sort",
    "uniq",
    "cut",
    "tr",
    "column",
    "less",
    "more",
    "jq",
    "yq",
    "true",
    "false",
    "test",
    "[",
    // Text tools that read stdin/args and write only to stdout (a `>` redirect
    // is caught separately). Adding these removes read_only false positives
    // reported after v0.14.0.
    "nl",
    "tac",
    "rev",
    "comm",
    "join",
    "paste",
    "fold",
    "fmt",
    "expand",
    "unexpand",
    // Binary / file inspection — read-only (NOT `strip`, which edits in place;
    // NOT `ldd`, which can execute the inspected binary).
    "xxd",
    "od",
    "hexdump",
    "strings",
    "nm",
    "objdump",
    "readelf",
    "size",
    // More checksum families (siblings of the md5/sha1/sha256 already listed).
    "sha224sum",
    "sha384sum",
    "sha512sum",
    "b2sum",
    // Read-only process / system inspection (NOT `kill`, `nice`, etc.).
    "ps",
    "groups",
    "logname",
    "arch",
    "nproc",
    "uptime",
    "free",
    "vmstat",
    "lscpu",
    "lsblk",
    "lsusb",
    "lspci",
    "tty",
    // Shell navigation / no-op builtins: they change only the shell's own CWD
    // (ephemeral in a one-shot `sh -c`) or print it — they cannot read file
    // contents or mutate anything. Without these, the ubiquitous `cd DIR &&
    // <read>` shape classified as a mutation (unknown head) and blocked the
    // whole compound command in read_only.
    "cd",
    "pushd",
    "popd",
    "dirs",
    // Pure encode/compute utilities: read stdin/args and write only to stdout
    // (a `>` redirect is caught separately, like every other read tool here).
    "base64",
    "seq",
];

/// PowerShell cmdlets (and single-word aliases) that only read state. Matched
/// case-insensitively — PowerShell command names are. The scriptblock-taking
/// pipeline cmdlets (ForEach-Object, Where-Object, Select-Object, Sort-Object,
/// Measure-Object, Format-*) are deliberately absent: a scriptblock or
/// calculated-property argument can run anything, so they classify as a
/// mutation and defer to the gate. Model commands run under PowerShell on
/// Windows, so these heads are as common there as `cat`/`ls` are on unix.
pub(crate) const PS_READ_ONLY_CMDLETS: &[&str] = &[
    "get-content",
    "get-childitem",
    "get-item",
    "get-itemproperty",
    "get-location",
    "get-date",
    "get-command",
    "get-alias",
    "get-variable",
    "get-process",
    "get-service",
    "get-member",
    "get-history",
    "get-psdrive",
    "get-filehash",
    "get-host",
    "get-error",
    "select-string",
    "test-path",
    "resolve-path",
    "split-path",
    "join-path",
    "compare-object",
    "out-string",
    "write-output",
    "write-host",
    "dir",
    // Single-word aliases of the cmdlets above (`cat`/`ls`/`pwd`/`echo`/`ps`
    // style aliases are already in READ_ONLY_BINARIES).
    "gc",
    "gci",
    "gi",
    "gl",
    "gal",
    "gv",
    "gps",
    "gsv",
    "gm",
    "gcm",
    "sls",
];

/// `git` subcommands that only read repository state. Deliberately excludes
/// `config` (writes global hooks/pager → code-exec), `branch` (`-D` deletes
/// refs), and `tag` (`-d` deletes); the argv0-only classifier can't see their
/// mutating flags, so they classify as a mutation and defer to Ask/Classify.
pub(crate) const GIT_READ_ONLY: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "remote",
    "describe",
    "rev-parse",
    "blame",
    "ls-files",
    "ls-tree",
    "cat-file",
    "shortlog",
    "reflog",
    "whatchanged",
    "grep",
    // Additional pure-read subcommands with no mutating flag form. Still
    // excludes `symbolic-ref` (writes with two args / `-d`) and `ls-remote`
    // (network), consistent with the `config`/`branch`/`tag` exclusions above.
    "rev-list",
    "merge-base",
    "show-ref",
    "for-each-ref",
    "name-rev",
    "show-branch",
    "count-objects",
    "version",
];

/// Binaries that reach the network — never auto-run outside `FullAccess`.
pub(crate) const NETWORK_BINARIES: &[&str] = &[
    "curl", "wget", "nc", "ncat", "netcat", "socat", "ssh", "scp", "sftp", "rsync", "ftp", "telnet",
];

/// Interpreters/build tools that execute arbitrary code or spawn processes.
pub(crate) const PROCESS_BINARIES: &[&str] = &[
    "python",
    "python2",
    "python3",
    "node",
    "deno",
    "bun",
    "ruby",
    "perl",
    "php",
    "bash",
    "sh",
    "zsh",
    "fish",
    "pwsh",
    "powershell",
    "cargo",
    "npm",
    "pnpm",
    "yarn",
    "make",
    "docker",
    "kubectl",
    "go",
    "java",
];

/// Wrapper commands whose real subject is the following token.
pub(crate) const WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "nohup", "time", "nice", "setsid", "stdbuf", "command", "xargs", "then",
    "else", "do",
];

/// If `tok` is an output redirection that writes to a FILE — including the
/// fd-numbered (`1>`, `2>>`) and `&>` forms a bare `starts_with('>')` misses —
/// return the file target after the operator (empty ⇒ the target is the next
/// token). Returns `None` for non-redirects and for fd-dup redirects like
/// `2>&1` (which write no file), so `ls 2>&1` is not mis-flagged as a mutation.
pub(crate) fn redirect_target_after(tok: &str) -> Option<&str> {
    let rest = tok.trim_start_matches(|c: char| c.is_ascii_digit());
    if let Some(r) = rest.strip_prefix("&>") {
        return Some(r.trim_start_matches('>'));
    }
    let after = rest.strip_prefix('>')?;
    if after.starts_with('&') {
        return None;
    }
    Some(after.trim_start_matches('>'))
}

/// Resolve the WRITE TARGET of the output-redirect token at `tokens[i]`: the
/// glued after-part (`2>/dev/null`) or, when the operator stands alone
/// (`2> /dev/null`), the following token.
///
/// The whitespace tokenizer keeps unquoted chain operators glued to the
/// preceding word (`2>/dev/null;` in `ls 2>/dev/null; echo done`), so
/// trailing `;`/`&`/`|` are stripped here — otherwise the target reads as
/// `/dev/null;`, which misses the safe-device list and then matches the
/// sensitive `/dev/` prefix, hard-denying a benign read-only chain (user
/// report, v0.14.0). Stripping never hides a sensitive target: it only
/// normalizes the path the sensitivity checks compare against. Quotes are
/// trimmed to match `is_sensitive_write_target`'s comparison.
pub(crate) fn redirect_write_target(tokens: &[String], i: usize) -> Option<&str> {
    let after = redirect_target_after(&tokens[i])?;
    let raw = if after.is_empty() {
        tokens.get(i + 1).map(String::as_str)?
    } else {
        after
    };
    Some(
        raw.trim_end_matches([';', '&', '|'])
            .trim_matches(['"', '\'']),
    )
}

/// Character pseudo-devices that are safe WRITE targets: `2>/dev/null` is
/// ubiquitous in read-only shell work and discards data by definition. Real
/// block devices (`/dev/sda`, `/dev/nvme0n1`) are deliberately NOT here and
/// keep counting as writes.
pub(crate) fn is_safe_device_write(path: &str) -> bool {
    const SAFE_DEVICES: &[&str] = &[
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/tty",
        "/dev/stdin",
        "/dev/stdout",
        "/dev/stderr",
        "/dev/random",
        "/dev/urandom",
    ];
    SAFE_DEVICES.contains(&path) || path.starts_with("/dev/fd/")
}
