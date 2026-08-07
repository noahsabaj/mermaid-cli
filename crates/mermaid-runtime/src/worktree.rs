//! Isolated git worktrees for subagents.
//!
//! Parallel subagents that share one working copy collide. Per-path write
//! locks (`providers::tool::path_lock`) stop two children from *losing* each
//! other's bytes, but nothing stops child A's build from compiling child B's
//! half-finished edit, and nothing stops two children from making changes
//! that are individually fine and jointly incoherent.
//!
//! An isolated child gets its own checkout under the Mermaid data dir and
//! never sees the user's working copy. Its result comes back as a patch,
//! applied under a lock once the child is done — so overlapping work fails
//! loudly at merge time instead of interleaving silently mid-run.
//!
//! ## Lifecycle
//!
//! 1. [`AgentWorktree::create`] adds a detached worktree at the project's
//!    `HEAD`, replays the project's uncommitted state into it, and commits
//!    that as the **base**. The child therefore starts from what the user
//!    currently has, not from the last commit.
//! 2. The child runs, rooted at [`AgentWorktree::root`].
//! 3. [`AgentWorktree::merge_into_project`] diffs the worktree against the
//!    base and applies that patch to the project. On success it re-anchors
//!    the base, so a continuation of the same child merges only its *new*
//!    work rather than replaying what already landed.
//! 4. [`AgentWorktree::destroy`] removes the checkout.
//!
//! ## What is deliberately not carried in
//!
//! Ignored files. `target/`, `node_modules/`, and `.env` stay behind, which
//! is what makes a worktree cheap to create and is also why a child that
//! needs to build pays a cold-cache build. Untracked-but-not-ignored files
//! *are* carried, since those are usually the new files the user is midway
//! through writing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

use crate::checkpoint::project_hash;
use crate::data_dir;
use crate::git::{git, is_work_tree};

/// Commit subject for the synthetic commits made inside a worktree. These
/// live only as long as the worktree does: they are reachable from its
/// detached HEAD and from no branch, so `git gc` collects them once the
/// worktree is removed.
const BASE_SUBJECT: &str = "mermaid: subagent base";

/// Paths inside a checkout that belong to Mermaid, not to the child.
///
/// A child's session transcript is written to `<workdir>/.mermaid/
/// conversations/` by the runtime as the child runs. In a shared workspace
/// that lands in the project the same way it always has; in a checkout,
/// `git add -A` would sweep it into the child's patch and merge Mermaid's own
/// bookkeeping into the user's repository — files no agent wrote and nobody
/// asked for.
///
/// Scoped deliberately narrow. The rest of `.mermaid/` is the user's:
/// `config.toml` and `memory/` are theirs to commit, so a child asked to edit
/// them must still be able to.
const RUNTIME_OWNED: &[&str] = &[".mermaid/conversations"];

/// `git add -A` restricted to the child's own work. Everything except
/// [`RUNTIME_OWNED`].
fn stage_child_work(top: &Path) -> Result<()> {
    let mut cmd = git(top).args(["add", "-A", "--", "."]);
    for path in RUNTIME_OWNED {
        cmd = cmd.arg(format!(":(exclude){path}"));
    }
    cmd.run()
}

/// Disambiguates checkout directories beyond the agent id.
///
/// Agent ids are minted per spawner and restart at `a1`, so two Mermaid
/// processes in one repo — two terminals, or a session plus a daemon task —
/// both want `.../a1`. Git then resolves the name collision by inventing
/// `a11`, `a12` and the two fight over each other's bookkeeping, which shows
/// up as `index.lock: File exists` on whichever loses. Process id plus a
/// counter makes the directory unique without giving up having the agent id
/// in the path.
///
/// Concurrent `worktree add` / `remove` / `prune` on one repo need no lock of
/// ours once the names are distinct; git serializes its own bookkeeping.
/// `concurrent_creates_on_one_repo_all_succeed` and
/// `creating_and_destroying_at_once_does_not_corrupt_the_repo` hold that.
static WORKTREE_SEQ: AtomicU64 = AtomicU64::new(0);

/// A subagent's private checkout.
#[derive(Debug)]
pub struct AgentWorktree {
    /// Where the child works. Not the worktree top level when the parent
    /// session was rooted in a subdirectory of the repo — see `create`.
    root: PathBuf,
    /// Top level of the private checkout (what `git worktree remove` takes).
    top: PathBuf,
    /// The project the work merges back into.
    project_top: PathBuf,
    /// Commit the child's changes are measured against. Advances on each
    /// successful merge.
    base: String,
}

/// What happened when a child's work was applied to the project.
#[derive(Debug)]
pub enum MergeOutcome {
    /// The child changed nothing.
    Empty,
    /// Applied cleanly. `files` is how many paths the patch touched.
    Applied { files: usize },
    /// The patch would not apply to the project as it now stands — most
    /// likely another agent (or the user) touched the same lines. The
    /// project is **untouched**: the patch is saved for inspection and the
    /// worktree is kept so the work is recoverable.
    Conflicted { patch: PathBuf, reason: String },
}

impl AgentWorktree {
    /// Create an isolated checkout for `agent_id`, seeded with `workdir`'s
    /// current uncommitted state.
    ///
    /// `workdir` is the session's directory; it may be the repo top level or
    /// any directory under it. The child is rooted at the matching relative
    /// path inside the worktree, so a session running in `crates/foo` gives
    /// its children a `crates/foo` too and relative paths in the prompt
    /// still mean what they say.
    pub fn create(workdir: &Path, agent_id: &str) -> Result<Self> {
        anyhow::ensure!(
            is_work_tree(workdir),
            "worktree isolation needs a git repository, and {} is not inside one",
            workdir.display()
        );
        let project_top = PathBuf::from(
            git(workdir)
                .args(["rev-parse", "--show-toplevel"])
                .output()
                .context("could not locate the repository top level")?,
        );
        // Canonicalize what git reported. Its answer is a real path but not
        // necessarily *the* path: on Windows it resolves the long name where
        // `%TEMP%` may hand out an 8.3 short one, and anywhere else it may
        // differ from the caller's route through a symlink. Every path this
        // type hands out is rooted here, and `pending_files` feeds both the
        // checkpoint and the merge's write locks — which the file tools take
        // on canonicalized paths. Two spellings of one file are two lock
        // keys, which is no lock at all.
        let project_top = std::fs::canonicalize(&project_top).unwrap_or(project_top);
        // An unborn HEAD has no commit to branch a worktree from.
        anyhow::ensure!(
            git(&project_top)
                .args(["rev-parse", "--verify", "--quiet", "HEAD"])
                .success()
                .unwrap_or(false),
            "worktree isolation needs at least one commit; this repository has none yet"
        );

        let top = worktree_dir(&project_top, agent_id);
        if let Some(parent) = top.parent() {
            std::fs::create_dir_all(parent)?;
        }

        git(&project_top)
            .args(["worktree", "add", "--detach", "--no-checkout"])
            .arg(&top)
            .arg("HEAD")
            .run()
            .context("could not create the isolated worktree")?;
        // `--no-checkout` then `checkout` keeps the add cheap on a big repo
        // and gives a clearer error if the checkout itself is what fails.
        git(&top)
            .args(["checkout", "--detach", "HEAD"])
            .run()
            .context("could not populate the isolated worktree")?;

        let mut worktree = Self {
            root: rebase_path(workdir, &project_top, &top)?,
            top,
            project_top,
            base: String::new(),
        };
        if let Err(e) = worktree.seed_uncommitted() {
            // Never leave a half-seeded checkout behind: the child would
            // silently work against a state that matches neither HEAD nor
            // the user's tree.
            worktree.destroy_ignoring_errors();
            return Err(e);
        }
        worktree.base = worktree.commit_state()?;
        std::fs::create_dir_all(&worktree.root)?;
        Ok(worktree)
    }

    /// Where the child should run.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Top level of the project this work merges back into. Callers
    /// serializing merges key their lock on this.
    pub fn project_root(&self) -> &Path {
        &self.project_top
    }

    /// The commit the child's work is currently measured against.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Absolute project paths the child's pending work would touch.
    ///
    /// Callers checkpoint these before [`Self::merge_into_project`], so a
    /// merged patch is as recoverable through `/restore` as any other tool's
    /// mutation. Reading them separately (rather than out of the merge
    /// result) is what lets the snapshot happen *before* the files change.
    ///
    /// Sorted and deduplicated, so a caller can feed them straight to a
    /// multi-path lock acquisition without risking a deadlock against
    /// another caller holding the same paths in a different order.
    pub fn pending_files(&self) -> Result<Vec<PathBuf>> {
        let patch = self.pending_patch()?;
        let mut absolute: Vec<PathBuf> = patch_paths(&patch)
            .into_iter()
            .map(|rel| self.project_top.join(rel))
            .collect();
        // Re-sort after joining rather than trusting that a shared prefix
        // preserved the relative order the parse produced.
        absolute.sort();
        absolute.dedup();
        Ok(absolute)
    }

    /// Apply the child's work to the project.
    ///
    /// Callers must serialize this across concurrent children — two patches
    /// applying at once reintroduce exactly the interleaving the worktree
    /// exists to prevent.
    pub fn merge_into_project(&mut self) -> Result<MergeOutcome> {
        let patch = self.pending_patch()?;
        if patch.is_empty() {
            return Ok(MergeOutcome::Empty);
        }
        let files = count_patch_files(&patch);

        // Dry-run first. `git apply` without `--check` can apply some hunks
        // and reject others, and a partial application of an agent's work is
        // worse than none: the user gets a tree matching no intended state.
        let applies = git(&self.project_top)
            .args(["apply", "--check", "--binary", "-"])
            .stdin_bytes(patch.clone())
            .success()?;
        if !applies {
            let reason = git(&self.project_top)
                .args(["apply", "--check", "--binary", "-"])
                .stdin_bytes(patch.clone())
                .output()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "patch does not apply".to_string());
            let saved = self.save_patch(&patch)?;
            return Ok(MergeOutcome::Conflicted {
                patch: saved,
                reason,
            });
        }

        git(&self.project_top)
            .args(["apply", "--binary", "-"])
            .stdin_bytes(patch)
            .run()
            .context("applying the agent's patch failed after it passed --check")?;

        // Re-anchor: a continuation of this child must merge only what it
        // does next, not replay what just landed.
        self.base = self.commit_state()?;
        Ok(MergeOutcome::Applied { files })
    }

    /// Remove the checkout. Best-effort: a worktree we cannot delete is
    /// disk we can reclaim later (see [`gc_orphaned_worktrees`]), never a
    /// reason to fail the agent whose work already merged.
    pub fn destroy(self) {
        self.destroy_ignoring_errors();
    }

    fn destroy_ignoring_errors(&self) {
        remove_worktree(&self.project_top, &self.top);
    }

    /// Replay the project's uncommitted state into the fresh checkout, so
    /// the child sees the user's work in progress and not just `HEAD`.
    fn seed_uncommitted(&self) -> Result<()> {
        // Tracked modifications, staged and unstaged alike. `--binary` so a
        // changed image or fixture survives the round trip.
        let tracked = git(&self.project_top)
            .args(["diff", "HEAD", "--binary"])
            .output_bytes()
            .context("could not read the project's uncommitted changes")?;
        if !tracked.is_empty() {
            git(&self.top)
                .args(["apply", "--binary", "-"])
                .stdin_bytes(tracked)
                .run()
                .context("could not replay the project's uncommitted changes into the worktree")?;
        }

        // Untracked but not ignored: usually the new files the user is
        // partway through writing, which a child would otherwise "helpfully"
        // recreate from scratch.
        let listing = git(&self.project_top)
            .args(["ls-files", "--others", "--exclude-standard", "-z"])
            .output_bytes()?;
        for rel in listing.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let rel = Path::new(std::str::from_utf8(rel).context("non-UTF-8 path in the repo")?);
            // `ls-files` emits repo-relative paths, but a symlinked or
            // otherwise surprising entry must not escape the checkout.
            if rel.is_absolute()
                || rel
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
            {
                continue;
            }
            // The parent session's own transcript is not work in progress;
            // copying it in would only give the child a stale sibling of its
            // own log and put it in the base commit.
            if RUNTIME_OWNED
                .iter()
                .any(|owned| rel.starts_with(Path::new(owned)))
            {
                continue;
            }
            let from = self.project_top.join(rel);
            let to = self.top.join(rel);
            if !from.is_file() {
                continue;
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&from, &to)
                .with_context(|| format!("could not seed untracked file {}", rel.display()))?;
        }
        Ok(())
    }

    /// Stage everything and commit it, returning the new commit id. Used
    /// both to anchor the base and to re-anchor after a merge.
    fn commit_state(&self) -> Result<String> {
        stage_child_work(&self.top)?;
        if !git(&self.top)
            .args(["diff", "--cached", "--quiet"])
            .success()?
        {
            git(&self.top)
                .args(["commit", "-q", "-m", BASE_SUBJECT])
                .run()?;
        }
        git(&self.top).args(["rev-parse", "HEAD"]).output()
    }

    /// The child's work as a patch against the base.
    fn pending_patch(&self) -> Result<Vec<u8>> {
        // Staging first is what puts new files' blobs in the object database
        // and makes them visible to `diff`; without it a created file shows
        // up nowhere in the patch.
        stage_child_work(&self.top)?;
        git(&self.top)
            .args(["diff", "--cached", "--binary", &self.base])
            .output_bytes()
    }

    /// Park a patch that would not apply next to the worktree it came from.
    fn save_patch(&self, patch: &[u8]) -> Result<PathBuf> {
        let path = self.top.with_extension("patch");
        std::fs::write(&path, patch)
            .with_context(|| format!("could not save the patch to {}", path.display()))?;
        Ok(path)
    }
}

/// Where a given agent's checkout lives. Under the data dir rather than in
/// the project, so it stays out of the user's globs, builds, and `git
/// status`.
fn worktree_dir(project_top: &Path, agent_id: &str) -> PathBuf {
    let sanitized: String = agent_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    // See `WORKTREE_SEQ`: the agent id alone is not unique across processes.
    let unique = format!(
        "{sanitized}-{}-{}",
        std::process::id(),
        WORKTREE_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    data_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("mermaid"))
        .join("worktrees")
        .join(project_hash(project_top))
        .join(unique)
}

/// Re-root `path` from under `from_root` to under `to_root`.
fn rebase_path(path: &Path, from_root: &Path, to_root: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let from = std::fs::canonicalize(from_root).unwrap_or_else(|_| from_root.to_path_buf());
    match canonical.strip_prefix(&from) {
        Ok(rel) => Ok(to_root.join(rel)),
        // Not under the top level at all: the session dir is the repo root
        // reached by some other path. Use the worktree top as-is.
        Err(_) => Ok(to_root.to_path_buf()),
    }
}

/// Tear a worktree down. Tries git's own bookkeeping first so the entry in
/// `.git/worktrees` goes with it, then falls back to deleting the directory.
fn remove_worktree(project_top: &Path, top: &Path) {
    let _ = git(project_top)
        .args(["worktree", "remove", "--force"])
        .arg(top)
        .run();
    if top.exists() {
        let _ = std::fs::remove_dir_all(top);
    }
    let _ = git(project_top).args(["worktree", "prune"]).run();
}

/// How many files a patch touches, counted from its `diff --git` headers.
fn count_patch_files(patch: &[u8]) -> usize {
    patch
        .split(|b| *b == b'\n')
        .filter(|line| line.starts_with(b"diff --git "))
        .count()
}

/// Repo-relative paths a patch touches, read from its `diff --git a/x b/y`
/// headers. Takes the `b/` side so a rename reports its destination.
///
/// Paths containing whitespace make the header ambiguous — git quotes those
/// (`diff --git "a/two words.txt" ...`), and a quoted header is skipped
/// rather than mis-split. The cost is a missed checkpoint entry for such a
/// file, never a wrong path.
fn patch_paths(patch: &[u8]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for line in patch.split(|b| *b == b'\n') {
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        let Some(rest) = line.strip_prefix("diff --git ") else {
            continue;
        };
        if rest.starts_with('"') {
            continue;
        }
        let fields: Vec<&str> = rest.split(' ').collect();
        // Exactly two fields means neither side was quoted or space-laden.
        if let [_, b_side] = fields[..]
            && let Some(rel) = b_side.strip_prefix("b/")
            && !rel.is_empty()
        {
            let rel = Path::new(rel);
            if !rel.is_absolute()
                && !rel
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
            {
                paths.push(rel.to_path_buf());
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Best-effort removal of worktree directories older than `max_age_days`.
///
/// A crash between `create` and `destroy` strands a checkout. Agent ids are
/// per-session, so nothing ever reclaims one by name after a restart; this
/// is the sweep that keeps the data dir bounded. Returns how many were
/// removed. Never fails the caller — a directory it cannot read is skipped.
pub fn gc_orphaned_worktrees(max_age_days: i64) -> Result<usize> {
    let root = data_dir()?.join("worktrees");
    let Ok(projects) = std::fs::read_dir(&root) else {
        return Ok(0);
    };
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(
            max_age_days.max(0) as u64 * 24 * 60 * 60,
        ))
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut removed = 0;
    for project in projects.flatten() {
        let Ok(agents) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for agent in agents.flatten() {
            let stale = agent
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|m| m < cutoff);
            if stale && std::fs::remove_dir_all(agent.path()).is_ok() {
                removed += 1;
            }
        }
        // Drop the project bucket once its last agent is gone.
        if std::fs::read_dir(project.path()).is_ok_and(|mut d| d.next().is_none()) {
            let _ = std::fs::remove_dir(project.path());
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mermaid_wt_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A project with one commit and one tracked file. `false` when git is
    /// missing, which no-ops every test here.
    fn init_project(dir: &Path) -> bool {
        if git(dir).args(["init", "-q"]).run().is_err() {
            return false;
        }
        std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        git(dir).args(["add", "-A"]).run().unwrap();
        git(dir).args(["commit", "-qm", "init"]).run().unwrap();
        true
    }

    /// File content with line endings normalized. A repo on a machine with
    /// `core.autocrlf=true` checks out CRLF on both sides of the merge,
    /// which is correct and beside the point of every assertion here.
    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap().replace("\r\n", "\n")
    }

    #[test]
    fn child_starts_from_the_users_uncommitted_state_not_head() {
        let project = unique_dir("seed");
        if !init_project(&project) {
            return;
        }
        // Uncommitted work of both kinds, which a naive `worktree add HEAD`
        // would hide from the child.
        std::fs::write(project.join("tracked.txt"), "one\ntwo\n").unwrap();
        std::fs::write(project.join("untracked.txt"), "new\n").unwrap();

        let wt = AgentWorktree::create(&project, "a1").unwrap();
        assert_eq!(read(&wt.root().join("tracked.txt")), "one\ntwo\n");
        assert_eq!(read(&wt.root().join("untracked.txt")), "new\n");
        wt.destroy();
    }

    #[test]
    fn ignored_files_stay_behind() {
        let project = unique_dir("ignored");
        if !init_project(&project) {
            return;
        }
        std::fs::write(project.join(".gitignore"), "secrets.env\n").unwrap();
        std::fs::write(project.join("secrets.env"), "TOKEN=1\n").unwrap();

        let wt = AgentWorktree::create(&project, "a1").unwrap();
        assert!(
            !wt.root().join("secrets.env").exists(),
            "ignored files must not be copied into a child's checkout"
        );
        wt.destroy();
    }

    #[test]
    fn child_edits_do_not_touch_the_project_until_merge() {
        let project = unique_dir("isolation");
        if !init_project(&project) {
            return;
        }
        let mut wt = AgentWorktree::create(&project, "a1").unwrap();
        std::fs::write(wt.root().join("tracked.txt"), "rewritten\n").unwrap();

        // The whole point: the user's copy is untouched while the child runs.
        assert_eq!(read(&project.join("tracked.txt")), "one\n");

        let outcome = wt.merge_into_project().unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Applied { files: 1 }),
            "{outcome:?}"
        );
        assert_eq!(read(&project.join("tracked.txt")), "rewritten\n");
        wt.destroy();
    }

    #[test]
    fn merge_carries_new_and_deleted_files() {
        let project = unique_dir("addremove");
        if !init_project(&project) {
            return;
        }
        let mut wt = AgentWorktree::create(&project, "a1").unwrap();
        std::fs::write(wt.root().join("added.txt"), "added\n").unwrap();
        std::fs::remove_file(wt.root().join("tracked.txt")).unwrap();

        assert!(matches!(
            wt.merge_into_project().unwrap(),
            MergeOutcome::Applied { files: 2 }
        ));
        assert_eq!(read(&project.join("added.txt")), "added\n");
        assert!(!project.join("tracked.txt").exists());
        wt.destroy();
    }

    #[test]
    fn pending_files_names_what_a_merge_would_touch() {
        let project = unique_dir("pending");
        if !init_project(&project) {
            return;
        }
        let wt = AgentWorktree::create(&project, "a1").unwrap();
        assert!(
            wt.pending_files().unwrap().is_empty(),
            "an idle child has nothing pending"
        );

        std::fs::write(wt.root().join("tracked.txt"), "edited\n").unwrap();
        std::fs::create_dir_all(wt.root().join("sub")).unwrap();
        std::fs::write(wt.root().join("sub").join("added.txt"), "new\n").unwrap();

        let pending = wt.pending_files().unwrap();
        // Absolute, project-side paths — what `create_checkpoint` wants, and
        // what the merge takes its write locks on. Anchored on the canonical
        // root rather than the path this test built: `%TEMP%` hands out 8.3
        // short names on Windows, and elsewhere a symlinked route spells the
        // same directory differently. Those are the spellings that made the
        // lock keys diverge in the first place.
        let root = wt.project_root();
        assert_eq!(pending.len(), 2, "{pending:?}");
        assert!(pending.contains(&root.join("tracked.txt")), "{pending:?}");
        assert!(
            pending.contains(&root.join("sub").join("added.txt")),
            "{pending:?}"
        );
        wt.destroy();
    }

    #[test]
    fn pending_files_are_spelled_the_way_the_file_tools_lock_them() {
        let project = unique_dir("canonical");
        if !init_project(&project) {
            return;
        }
        let wt = AgentWorktree::create(&project, "a1").unwrap();
        std::fs::write(wt.root().join("tracked.txt"), "edited\n").unwrap();

        // The merge takes its write locks on these paths, and the file tools
        // take theirs on canonicalized ones. Two spellings of one file are
        // two keys, so a merge and a concurrent `write_file` would not
        // exclude each other at all — which is silent, and exactly the
        // interleaving isolation exists to prevent.
        let canonical_root = std::fs::canonicalize(&project).unwrap();
        for path in wt.pending_files().unwrap() {
            assert!(
                path.starts_with(&canonical_root),
                "{} is not under the canonical root {}",
                path.display(),
                canonical_root.display()
            );
            assert_eq!(
                std::fs::canonicalize(&path).unwrap(),
                path,
                "a pending path must already be canonical"
            );
        }
        wt.destroy();
    }

    #[test]
    fn patch_paths_takes_the_destination_and_skips_quoted_headers() {
        let patch = b"diff --git a/old.txt b/new.txt\nsimilarity index 100%\n\
                      diff --git a/keep.txt b/keep.txt\n\
                      diff --git \"a/two words.txt\" \"b/two words.txt\"\n";
        // A rename reports where the content ended up, and the ambiguous
        // quoted header is dropped rather than split into a wrong path.
        assert_eq!(
            patch_paths(patch),
            vec![PathBuf::from("keep.txt"), PathBuf::from("new.txt")]
        );
    }

    #[test]
    fn a_child_that_changed_nothing_merges_empty() {
        let project = unique_dir("empty");
        if !init_project(&project) {
            return;
        }
        let mut wt = AgentWorktree::create(&project, "a1").unwrap();
        assert!(matches!(
            wt.merge_into_project().unwrap(),
            MergeOutcome::Empty
        ));
        wt.destroy();
    }

    #[test]
    fn overlapping_edits_conflict_instead_of_clobbering() {
        let project = unique_dir("conflict");
        if !init_project(&project) {
            return;
        }
        let mut wt = AgentWorktree::create(&project, "a1").unwrap();
        std::fs::write(wt.root().join("tracked.txt"), "from the agent\n").unwrap();
        // Someone else — another agent, or the user — rewrites the same file
        // while the child is running.
        std::fs::write(project.join("tracked.txt"), "from the user\n").unwrap();

        let outcome = wt.merge_into_project().unwrap();
        let MergeOutcome::Conflicted { patch, .. } = outcome else {
            panic!("expected a conflict, got {outcome:?}");
        };
        // The competing write survives untouched and the work is recoverable.
        assert_eq!(read(&project.join("tracked.txt")), "from the user\n");
        assert!(patch.exists(), "the rejected patch must be saved");
        wt.destroy();
    }

    #[test]
    fn a_continuation_merges_only_its_new_work() {
        let project = unique_dir("reanchor");
        if !init_project(&project) {
            return;
        }
        let mut wt = AgentWorktree::create(&project, "a1").unwrap();
        std::fs::write(wt.root().join("tracked.txt"), "first pass\n").unwrap();
        wt.merge_into_project().unwrap();

        // Second drive of the same child, in the same checkout. Without the
        // re-anchor the patch would replay the first pass and conflict.
        std::fs::write(wt.root().join("tracked.txt"), "second pass\n").unwrap();
        let outcome = wt.merge_into_project().unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Applied { files: 1 }),
            "{outcome:?}"
        );
        assert_eq!(read(&project.join("tracked.txt")), "second pass\n");
        wt.destroy();
    }

    #[test]
    fn a_session_in_a_subdirectory_gets_a_matching_child_root() {
        let project = unique_dir("subdir");
        if !init_project(&project) {
            return;
        }
        let sub = project.join("crates").join("inner");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("lib.rs"), "fn main() {}\n").unwrap();

        let wt = AgentWorktree::create(&sub, "a1").unwrap();
        assert!(
            wt.root().ends_with(Path::new("crates").join("inner")),
            "child root {} should mirror the session's path in the repo",
            wt.root().display()
        );
        assert_eq!(read(&wt.root().join("lib.rs")), "fn main() {}\n");
        wt.destroy();
    }

    #[test]
    fn destroy_leaves_no_checkout_and_no_git_bookkeeping() {
        let project = unique_dir("destroy");
        if !init_project(&project) {
            return;
        }
        let wt = AgentWorktree::create(&project, "a1").unwrap();
        let top = wt.top.clone();
        wt.destroy();
        assert!(!top.exists());
        let listed = git(&project).args(["worktree", "list"]).output().unwrap();
        assert!(
            !listed.contains("a1"),
            "worktree bookkeeping should be pruned: {listed}"
        );
    }

    #[test]
    fn mermaids_own_session_state_never_merges_into_the_project() {
        let project = unique_dir("runtime_owned");
        if !init_project(&project) {
            return;
        }
        let mut wt = AgentWorktree::create(&project, "a1").unwrap();

        // What the runtime writes as the child runs, alongside a real edit.
        let conversations = wt.root().join(".mermaid").join("conversations");
        std::fs::create_dir_all(&conversations).unwrap();
        std::fs::write(conversations.join("20260807_1.json"), "{}\n").unwrap();
        std::fs::write(wt.root().join("tracked.txt"), "real work\n").unwrap();
        // A user-owned file under the same directory must still merge.
        std::fs::create_dir_all(wt.root().join(".mermaid")).unwrap();
        std::fs::write(wt.root().join(".mermaid").join("config.toml"), "x = 1\n").unwrap();

        let pending = wt.pending_files().unwrap();
        assert!(
            !pending
                .iter()
                .any(|p| p.to_string_lossy().contains("conversations")),
            "Mermaid's own transcript must not be part of the child's work: {pending:?}"
        );

        wt.merge_into_project().unwrap();
        assert_eq!(read(&project.join("tracked.txt")), "real work\n");
        assert_eq!(
            read(&project.join(".mermaid").join("config.toml")),
            "x = 1\n",
            "the user's own .mermaid files must still merge"
        );
        assert!(
            !project.join(".mermaid").join("conversations").exists(),
            "the project must not receive Mermaid's session transcripts"
        );
        wt.destroy();
    }

    #[test]
    fn concurrent_creates_on_one_repo_all_succeed() {
        let project = unique_dir("concurrent");
        if !init_project(&project) {
            return;
        }
        // The fan-out case. `git worktree add` writes the repo's
        // `.git/worktrees/` bookkeeping and checks out through it; without
        // serialization the losers die on `index.lock: File exists`.
        let handles: Vec<_> = (0..6)
            .map(|i| {
                let project = project.clone();
                std::thread::spawn(move || AgentWorktree::create(&project, &format!("a{i}")))
            })
            .collect();

        let mut roots = Vec::new();
        for handle in handles {
            let wt = handle
                .join()
                .unwrap()
                .expect("every concurrent create must succeed");
            roots.push(wt.root().to_path_buf());
            wt.destroy();
        }
        roots.sort();
        let distinct = {
            let mut r = roots.clone();
            r.dedup();
            r.len()
        };
        assert_eq!(distinct, 6, "each child needs its own checkout: {roots:?}");
    }

    #[test]
    fn creating_and_destroying_at_once_does_not_corrupt_the_repo() {
        let project = unique_dir("churn");
        if !init_project(&project) {
            return;
        }
        // `git worktree prune` on teardown revalidates the bookkeeping for
        // every worktree of the repo, so it races an `add` running at the
        // same time. A fan-out where one child finishes while another starts
        // is the ordinary case, not a corner one.
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let project = project.clone();
                std::thread::spawn(move || {
                    let wt = AgentWorktree::create(&project, &format!("c{i}"))?;
                    std::fs::write(wt.root().join("tracked.txt"), format!("{i}\n"))?;
                    wt.destroy();
                    anyhow::Ok(())
                })
            })
            .collect();
        for handle in handles {
            handle
                .join()
                .unwrap()
                .expect("create/destroy churn must not fail");
        }
        // The repo is still usable and knows about no leftover worktrees.
        let listed = git(&project).args(["worktree", "list"]).output().unwrap();
        assert_eq!(
            listed.lines().count(),
            1,
            "only the main worktree should remain: {listed}"
        );
    }

    #[test]
    fn two_agents_with_the_same_id_still_get_separate_checkouts() {
        let project = unique_dir("sameid");
        if !init_project(&project) {
            return;
        }
        // Agent ids restart at `a1` per spawner, so two Mermaid processes in
        // one repo both ask for `a1`. If that resolved to one directory they
        // would silently share a checkout and clobber each other.
        let first = AgentWorktree::create(&project, "a1").unwrap();
        let second = AgentWorktree::create(&project, "a1").unwrap();
        assert_ne!(first.root(), second.root());

        std::fs::write(first.root().join("tracked.txt"), "first\n").unwrap();
        assert_eq!(
            read(&second.root().join("tracked.txt")),
            "one\n",
            "one agent's edit must not appear in another's checkout"
        );
        first.destroy();
        second.destroy();
    }

    #[test]
    fn outside_a_repository_isolation_fails_loudly() {
        // Silently falling back to the shared cwd would reintroduce exactly
        // the collisions the caller asked to avoid.
        let plain = unique_dir("norepo");
        let err = AgentWorktree::create(&plain, "a1").unwrap_err().to_string();
        assert!(err.contains("git repository"), "{err}");
    }
}
