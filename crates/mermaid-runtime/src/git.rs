//! Hardened `git` invocation.
//!
//! Every `git` call Mermaid makes on a user's behalf runs through this
//! builder, so the hardening is uniform instead of re-derived per call site:
//!
//! - **No repo-provided hooks** (`core.hooksPath` pointed at a nonexistent
//!   path). A checkpoint, a worktree, or a plugin fetch must never execute
//!   code the repo happens to carry. A missing hooks dir means git runs no
//!   hooks — including on Windows git, where `/dev/null` is not a device but
//!   is still an absent path.
//! - **No external transports** (`protocol.ext.allow=never`). `ext::` URLs
//!   hand git a shell command to run; a submodule or remote carrying one is
//!   remote code execution.
//! - **No credential prompts** (`GIT_TERMINAL_PROMPT=0`). A fetch against a
//!   private remote fails fast instead of blocking a background task on a
//!   terminal read nobody is watching.
//! - **A fixed committer identity**, so a commit works on a machine with no
//!   `user.email` configured and never attributes Mermaid's bookkeeping to
//!   the user.
//!
//! Callers pick how much output they need: [`GitCommand::run`] discards it,
//! [`GitCommand::success`] reports the exit status as a bool (for the
//! `--quiet` predicates), [`GitCommand::output`] returns trimmed stdout, and
//! [`GitCommand::output_bytes`] returns it raw — `git diff --binary` emits
//! base85 payloads and diff context lifted verbatim out of files that need
//! not be UTF-8.

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

/// Config flags forced on every invocation. See the module docs.
const HARDENING: [&str; 4] = [
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "protocol.ext.allow=never",
];

/// Identity used for commits Mermaid makes on its own behalf (checkpoint
/// snapshots, subagent worktree bases). Never the user's.
const AUTHOR_NAME: &str = "Mermaid";
const AUTHOR_EMAIL: &str = "mermaid@localhost";

/// A `git` invocation with Mermaid's hardening already applied.
pub struct GitCommand {
    cmd: Command,
    /// Echoed into error messages — `Command` won't give the args back.
    display: Vec<String>,
    stdin: Option<Vec<u8>>,
}

impl GitCommand {
    /// Start a hardened `git` invocation. Add a working directory with
    /// [`Self::cwd`]; without one the command inherits the process's.
    pub fn new() -> Self {
        let mut cmd = Command::new("git");
        cmd.args(HARDENING)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
            .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
            .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
            .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL);
        Self {
            cmd,
            display: Vec::new(),
            stdin: None,
        }
    }

    /// Run in `dir`.
    pub fn cwd(mut self, dir: &Path) -> Self {
        self.cmd.current_dir(dir);
        self
    }

    /// Append one argument.
    pub fn arg<S: AsRef<OsStr>>(mut self, arg: S) -> Self {
        let arg = arg.as_ref();
        self.display.push(arg.to_string_lossy().into_owned());
        self.cmd.arg(arg);
        self
    }

    /// Append several arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self = self.arg(arg);
        }
        self
    }

    /// Feed `data` to the command's stdin. Lets `git apply` take a patch
    /// without staging it through a temp file whose lifetime we'd have to
    /// manage (and whose contents would briefly sit on disk unredacted).
    pub fn stdin_bytes(mut self, data: Vec<u8>) -> Self {
        self.stdin = Some(data);
        self
    }

    /// Run and require success, discarding output.
    pub fn run(self) -> Result<()> {
        let display = self.display.join(" ");
        let (ok, _, stderr) = self.capture()?;
        anyhow::ensure!(ok, "git {display} failed: {}", stderr.trim());
        Ok(())
    }

    /// Run and report whether it exited zero, discarding output. For the
    /// predicate forms (`diff --quiet`, `rev-parse`) where a nonzero exit is
    /// an answer rather than a failure.
    pub fn success(self) -> Result<bool> {
        let (ok, _, _) = self.capture()?;
        Ok(ok)
    }

    /// Run and return trimmed stdout, requiring success.
    pub fn output(self) -> Result<String> {
        let raw = self.output_bytes()?;
        Ok(String::from_utf8_lossy(&raw).trim().to_string())
    }

    /// Run and return raw stdout, requiring success. Use for `diff --binary`
    /// and anything else that need not be valid UTF-8.
    pub fn output_bytes(self) -> Result<Vec<u8>> {
        let display = self.display.join(" ");
        let (ok, stdout, stderr) = self.capture()?;
        anyhow::ensure!(ok, "git {display} failed: {}", stderr.trim());
        Ok(stdout)
    }

    /// Spawn, feed stdin when set, and collect `(success, stdout, stderr)`.
    ///
    /// stdin is written from this thread while the child runs. That is safe
    /// only because every caller also drains stdout and stderr via
    /// `wait_with_output` afterwards: a child that filled its stdout pipe
    /// while we were still writing its stdin would otherwise deadlock, both
    /// sides blocked on a full pipe.
    fn capture(mut self) -> Result<(bool, Vec<u8>, String)> {
        self.cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        self.cmd.stdin(if self.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let display = self.display.join(" ");
        let mut child = self
            .cmd
            .spawn()
            .with_context(|| format!("failed to run git {display} (is git installed?)"))?;
        if let Some(data) = self.stdin.take() {
            let mut pipe = child
                .stdin
                .take()
                .context("git stdin pipe missing after spawn")?;
            // A `git apply` that rejects the patch early exits before reading
            // all of it, breaking the pipe. That is a patch failure, reported
            // through the exit status below — not an error in its own right.
            let _ = pipe.write_all(&data);
            drop(pipe);
        }
        let out = child
            .wait_with_output()
            .with_context(|| format!("git {display} was not reapable"))?;
        Ok((
            out.status.success(),
            out.stdout,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }
}

impl Default for GitCommand {
    fn default() -> Self {
        Self::new()
    }
}

/// Start a hardened `git` invocation in `dir`. The common shape.
pub fn git(dir: &Path) -> GitCommand {
    GitCommand::new().cwd(dir)
}

/// Whether `dir` sits inside a git work tree. False when git is missing
/// entirely, which is the same practical answer for every caller here.
pub fn is_work_tree(dir: &Path) -> bool {
    git(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .is_ok_and(|out| out == "true")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A throwaway directory unique to this test run + `tag` (tests share a PID).
    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mermaid_git_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// File content with line endings normalized. A repo on a machine with
    /// `core.autocrlf=true` checks out CRLF, which is correct and beside the
    /// point of every assertion here.
    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap().replace("\r\n", "\n")
    }

    /// A repo with one commit. `false` when git is absent — every test here
    /// then no-ops rather than failing a machine that has no git at all.
    fn init_repo(dir: &Path) -> bool {
        if git(dir).args(["init", "-q"]).run().is_err() {
            return false;
        }
        std::fs::write(dir.join("seed.txt"), "seed\n").unwrap();
        git(dir).args(["add", "-A"]).run().unwrap();
        git(dir).args(["commit", "-qm", "seed"]).run().unwrap();
        true
    }

    #[test]
    fn commits_without_a_configured_user_identity() {
        let repo = unique_dir("identity");
        if !init_repo(&repo) {
            return;
        }
        // The point of the forced identity: the commit above succeeds on a
        // machine where `git config user.email` is unset, as CI images are.
        let author = git(&repo)
            .args(["log", "-1", "--format=%an <%ae>"])
            .output()
            .unwrap();
        assert_eq!(author, format!("{AUTHOR_NAME} <{AUTHOR_EMAIL}>"));
    }

    #[test]
    fn success_reports_predicate_exits_without_erroring() {
        let repo = unique_dir("predicate");
        if !init_repo(&repo) {
            return;
        }
        // Clean tree: `diff --quiet` exits 0.
        assert!(git(&repo).args(["diff", "--quiet"]).success().unwrap());
        std::fs::write(repo.join("seed.txt"), "changed\n").unwrap();
        // Dirty tree: exits 1. `success` reports it; `run` would have errored.
        assert!(!git(&repo).args(["diff", "--quiet"]).success().unwrap());
    }

    #[test]
    fn stdin_feeds_a_patch_to_git_apply() {
        let repo = unique_dir("stdin");
        if !init_repo(&repo) {
            return;
        }
        std::fs::write(repo.join("seed.txt"), "changed\n").unwrap();
        let patch = git(&repo)
            .args(["diff", "--binary"])
            .output_bytes()
            .unwrap();
        assert!(!patch.is_empty());

        // Revert, then replay the captured patch through stdin.
        git(&repo)
            .args(["checkout", "--", "seed.txt"])
            .run()
            .unwrap();
        assert_eq!(read(&repo.join("seed.txt")), "seed\n");
        git(&repo).args(["apply"]).stdin_bytes(patch).run().unwrap();
        assert_eq!(read(&repo.join("seed.txt")), "changed\n");
    }

    #[test]
    fn failure_surfaces_the_failing_command_in_the_error() {
        let repo = unique_dir("failure");
        if !init_repo(&repo) {
            return;
        }
        let err = git(&repo)
            .args(["rev-parse", "--verify", "definitely-not-a-ref"])
            .output()
            .unwrap_err()
            .to_string();
        assert!(err.contains("rev-parse"), "{err}");
    }

    #[test]
    fn is_work_tree_distinguishes_a_repo_from_a_plain_directory() {
        let repo = unique_dir("worktree_yes");
        if !init_repo(&repo) {
            return;
        }
        assert!(is_work_tree(&repo));
        // A plain directory under the system temp dir is not in a work tree.
        // (If temp itself were inside a repo this would be wrong, but no
        // platform we support puts it there.)
        assert!(!is_work_tree(&unique_dir("worktree_no")));
    }
}
