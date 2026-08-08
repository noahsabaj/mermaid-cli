//! Where a subagent's file mutations land.
//!
//! A subagent has always run in the parent's directory. That is right for
//! the common case — a child that reads, or one child that writes — and
//! wrong for fan-out: `MAX_INFLIGHT` children editing one working copy
//! produce a tree that matches no single agent's intent, and the parent
//! cannot tell which of them is responsible for what.
//!
//! [`Workspace`] makes that choice explicit and gives the two modes one
//! shape, so `subagent.rs` threads a single value instead of a bare `cwd`
//! plus assumptions about it:
//!
//! - [`Isolation::Shared`] — the parent's directory, as before. Reads,
//!   recon, anything where a child's writes should be immediately visible.
//! - [`Isolation::Worktree`] — a private checkout ([`AgentWorktree`]). The
//!   child's writes are invisible to everyone until it finishes, then land
//!   as one patch under a lock.
//!
//! The mechanics live in `mermaid_runtime::worktree`; the policy lives
//! here — when to isolate, when to merge, what the parent gets told.

use std::path::{Path, PathBuf};

use crate::providers::ExecContext;
use mermaid_runtime::worktree::{AgentWorktree, MergeOutcome};

/// How a child's file mutations relate to the parent's working copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Isolation {
    /// Share the parent's directory.
    #[default]
    Shared,
    /// Run in a private git worktree and merge back on success.
    Worktree,
}

impl Isolation {
    /// Parse a config or tool-argument value. `None` for anything else, so
    /// callers can report the valid set themselves.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "shared" | "none" => Some(Self::Shared),
            "worktree" | "isolated" => Some(Self::Worktree),
            _ => None,
        }
    }

    /// The accepted spellings, for error messages.
    pub(crate) const NAMES: &'static str = "shared, worktree";
}

/// A child's working root, plus whatever has to happen to it afterwards.
#[derive(Debug)]
pub(crate) enum Workspace {
    Shared { root: PathBuf },
    Isolated(Box<AgentWorktree>),
}

/// What a merge needs from the session that spawned the child.
///
/// A detached (Ctrl+B) child outlives its turn and its `ExecContext`, so the
/// merge cannot borrow one. This is the small, owned, `Send` slice of it that
/// a background task can carry to the end.
#[derive(Debug, Clone)]
pub(crate) struct MergeContext {
    checkpoint: bool,
    origin: mermaid_runtime::CheckpointOrigin,
}

impl MergeContext {
    pub(crate) fn from_exec(ctx: &ExecContext) -> Self {
        Self {
            checkpoint: ctx.config.safety.checkpoint_on_mutation,
            origin: ctx.checkpoint_origin(),
        }
    }
}

/// What became of an isolated child's work. Reported to the parent model,
/// which otherwise has no way to know its child wrote to a copy.
#[derive(Debug, Default)]
pub(crate) struct WorkspaceReport {
    /// One line appended to the child's report, or empty for a shared child.
    pub(crate) note: String,
    /// Whether work was left somewhere the parent should act on.
    pub(crate) needs_attention: bool,
}

impl Workspace {
    /// Build the workspace a child will run in.
    ///
    /// Isolation failures are returned, never downgraded to `Shared`: a
    /// silent fallback would put a fan-out that asked for isolation back
    /// into the collisions it asked to avoid, and the parent would have no
    /// signal that it happened.
    pub(crate) async fn create(
        isolation: Isolation,
        workdir: PathBuf,
        agent_id: &str,
    ) -> Result<Self, String> {
        match isolation {
            Isolation::Shared => Ok(Self::Shared { root: workdir }),
            Isolation::Worktree => {
                let agent_id = agent_id.to_string();
                // git is a subprocess and a big repo's checkout is not
                // instant; keep it off the reactor.
                blocking(move || AgentWorktree::create(&workdir, &agent_id))
                    .await
                    .map(|wt| Self::Isolated(Box::new(wt)))
                    .map_err(|e| format!("could not isolate this agent: {e:#}"))
            },
        }
    }

    /// The directory the child runs in.
    pub(crate) fn root(&self) -> &Path {
        match self {
            Self::Shared { root } => root,
            Self::Isolated(wt) => wt.root(),
        }
    }

    pub(crate) fn is_isolated(&self) -> bool {
        matches!(self, Self::Isolated(_))
    }

    /// Land a finished child's work in the project.
    ///
    /// Takes the workspace and hands it back so the merge can run on the
    /// blocking pool without leaving a hole behind — `git apply` on a large
    /// patch is subprocess-bound and has no business on the reactor.
    ///
    /// A merge takes the write lock on every file it will touch, from the
    /// same registry the file-mutating tools use. Locking the project root
    /// instead would have been simpler and wrong in both directions: it
    /// would serialize merges that touch disjoint files, and it would not
    /// exclude a `write_file` the parent issued in the same turn, since that
    /// tool locks the individual path. Taking the real paths gives mutual
    /// exclusion against both other merges and ordinary writes, and lets
    /// unrelated merges proceed together.
    pub(crate) async fn merge(self, cx: &MergeContext) -> (Self, WorkspaceReport) {
        let Self::Isolated(wt) = self else {
            return (self, WorkspaceReport::default());
        };

        // Read the target list before locking. The child is finished, so its
        // checkout is static and the list cannot go stale under us.
        let (wt, pending) = blocking_owned(wt, |wt| wt.pending_files()).await;
        let files = match pending {
            Ok(files) => files,
            Err(e) => {
                let note = format!(
                    "Ran in an isolated worktree, but reading back its changes failed: \
                     {e:#}. Nothing was applied; the worktree is kept at {}.",
                    wt.root().display()
                );
                return (
                    Self::Isolated(wt),
                    WorkspaceReport {
                        note,
                        needs_attention: true,
                    },
                );
            },
        };
        if files.is_empty() {
            return (
                Self::Isolated(wt),
                WorkspaceReport {
                    note: "Ran in an isolated worktree and changed no files.".to_string(),
                    needs_attention: false,
                },
            );
        }
        // `pending_files` returns them sorted and deduplicated, which is what
        // keeps two overlapping merges from deadlocking on each other.
        let _guards = super::path_lock::lock_paths(&files).await;

        // A merged patch mutates the project like any other tool call, so it
        // owes the same `/restore` guarantee. Snapshot first — after the
        // apply there is nothing left to snapshot.
        if cx.checkpoint
            && let Err(e) = checkpoint_pending(wt.project_root(), files, cx).await
        {
            return (
                Self::Isolated(wt),
                WorkspaceReport {
                    note: format!(
                        "Ran in an isolated worktree, but checkpointing the project before \
                         merging failed: {e:#}. Nothing was applied — merging without a \
                         restore point would leave the change unrecoverable."
                    ),
                    needs_attention: true,
                },
            );
        }

        let (wt, outcome) = blocking_owned(wt, |wt| wt.merge_into_project()).await;

        let report = match outcome {
            Ok(MergeOutcome::Empty) => WorkspaceReport {
                note: "Ran in an isolated worktree and changed no files.".to_string(),
                needs_attention: false,
            },
            Ok(MergeOutcome::Applied { files }) => WorkspaceReport {
                note: format!(
                    "Ran in an isolated worktree; its changes to {files} \
                     file{} are now applied to the project.",
                    if files == 1 { "" } else { "s" }
                ),
                needs_attention: false,
            },
            // The patch path goes on its own line. Data dirs contain spaces
            // on macOS (`Library/Application Support`), so a path inline in a
            // sentence is ambiguous to read and unpasteable into a shell.
            Ok(MergeOutcome::Conflicted { patch, reason }) => WorkspaceReport {
                note: format!(
                    "Ran in an isolated worktree, but its changes do NOT apply to the \
                     project as it now stands ({reason}) — most likely the same lines \
                     changed underneath it. The project is untouched. The rejected \
                     patch is saved at:\n{}\nReview it before redoing this work.",
                    patch.display()
                ),
                needs_attention: true,
            },
            Err(e) => WorkspaceReport {
                note: format!(
                    "Ran in an isolated worktree, but merging its work failed: {e:#}. \
                     The project is untouched and the worktree is kept at {}.",
                    wt.root().display()
                ),
                needs_attention: true,
            },
        };
        (Self::Isolated(wt), report)
    }

    /// Note for a child whose work was deliberately not merged.
    pub(crate) fn unmerged_note(&self) -> String {
        match self {
            Self::Shared { .. } => String::new(),
            Self::Isolated(wt) => format!(
                "Its isolated worktree is kept at {} — the work is NOT in the project. \
                 Continue this agent to finish it, or apply the worktree by hand.",
                wt.root().display()
            ),
        }
    }

    /// Remove the checkout. A shared workspace owns nothing to discard —
    /// notably not the parent's directory.
    pub(crate) async fn discard(self) {
        if let Self::Isolated(wt) = self {
            let _ = tokio::task::spawn_blocking(move || wt.destroy()).await;
        }
    }
}

/// Snapshot every project file the pending merge would touch, anchored to
/// the conversation position that spawned the child — so `/restore` treats a
/// merged agent patch exactly like a `write_file` from the parent.
async fn checkpoint_pending(
    project_root: &Path,
    files: Vec<PathBuf>,
    cx: &MergeContext,
) -> anyhow::Result<()> {
    let project = project_root.to_path_buf();
    let origin = cx.origin.clone();
    let action = serde_json::json!({ "tool": "agent" });
    blocking(move || {
        mermaid_runtime::create_checkpoint_for_task(&project, &files, Some(action), origin)?;
        Ok(())
    })
    .await
}

/// Run a blocking closure on the pool, flattening the join error — a panic
/// in a git subprocess wrapper is a bug, and callers all report the same way.
async fn blocking<T, F>(f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => Err(anyhow::anyhow!("worktree task failed: {e}")),
    }
}

/// Move a value into the blocking pool and get it back with the result.
async fn blocking_owned<V, R, F>(value: V, f: F) -> (V, R)
where
    V: Send + 'static,
    R: Send + 'static,
    F: FnOnce(&mut V) -> R + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut value = value;
        let result = f(&mut value);
        (value, result)
    })
    .await
    .expect("worktree blocking task panicked")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ToolCallId, TurnId};
    use crate::providers::ctx::test_exec_context;

    #[test]
    fn isolation_parses_its_spellings_and_rejects_the_rest() {
        assert_eq!(Isolation::parse("shared"), Some(Isolation::Shared));
        assert_eq!(Isolation::parse("  Worktree "), Some(Isolation::Worktree));
        assert_eq!(Isolation::parse("isolated"), Some(Isolation::Worktree));
        assert_eq!(Isolation::parse("yes"), None);
        assert_eq!(Isolation::default(), Isolation::Shared);
    }

    #[tokio::test]
    async fn a_shared_workspace_is_the_parents_directory_and_owns_nothing() {
        let dir = std::env::temp_dir().join(format!("mermaid_ws_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::create(Isolation::Shared, dir.clone(), "a1")
            .await
            .unwrap();
        assert_eq!(ws.root(), dir);
        assert!(!ws.is_isolated());
        // Merging a shared workspace is a no-op, not an error.
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        let (ws, report) = ws.merge(&MergeContext::from_exec(&ctx)).await;
        assert!(report.note.is_empty());
        assert!(ws.unmerged_note().is_empty());
        ws.discard().await;
        assert!(dir.exists(), "discarding must not delete the parent's cwd");
    }

    /// A project repo plus a `MergeContext`. Checkpointing is off: these
    /// tests are about the workspace, and leaving it on would write real
    /// checkpoints into the user's data dir on every run.
    fn project(tag: &str) -> Option<(PathBuf, MergeContext)> {
        use mermaid_runtime::git::git;
        let dir = std::env::temp_dir().join(format!("mermaid_wsi_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if git(&dir).args(["init", "-q"]).run().is_err() {
            return None;
        }
        std::fs::write(dir.join("app.rs"), "fn main() {}\n").unwrap();
        git(&dir).args(["add", "-A"]).run().unwrap();
        git(&dir).args(["commit", "-qm", "init"]).run().unwrap();

        let mut config = crate::domain::Config::default();
        config.safety.checkpoint_on_mutation = false;
        let (ctx, _rx) = crate::providers::ctx::test_exec_context_with_config(
            TurnId(1),
            ToolCallId(1),
            dir.clone(),
            config,
        );
        Some((dir, MergeContext::from_exec(&ctx)))
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap().replace("\r\n", "\n")
    }

    #[tokio::test]
    async fn an_isolated_child_writes_out_of_sight_then_lands_on_merge() {
        let Some((project, cx)) = project("lands") else {
            return;
        };
        let ws = Workspace::create(Isolation::Worktree, project.clone(), "a1")
            .await
            .unwrap();
        assert!(ws.is_isolated());
        assert_ne!(
            ws.root(),
            project,
            "an isolated child must not share the cwd"
        );

        std::fs::write(ws.root().join("app.rs"), "fn main() { work(); }\n").unwrap();
        assert_eq!(
            read(&project.join("app.rs")),
            "fn main() {}\n",
            "the parent's copy must not move while the child runs"
        );

        let (ws, report) = ws.merge(&cx).await;
        assert!(!report.needs_attention, "{report:?}");
        assert!(report.note.contains("1 file"), "{}", report.note);
        assert_eq!(read(&project.join("app.rs")), "fn main() { work(); }\n");
        ws.discard().await;
    }

    #[tokio::test]
    async fn a_patch_that_cannot_land_is_flagged_for_the_parent() {
        let Some((project, cx)) = project("flagged") else {
            return;
        };
        let ws = Workspace::create(Isolation::Worktree, project.clone(), "a1")
            .await
            .unwrap();
        std::fs::write(ws.root().join("app.rs"), "fn main() { agent(); }\n").unwrap();
        // Someone else rewrites the same lines while the child works.
        std::fs::write(project.join("app.rs"), "fn main() { user(); }\n").unwrap();

        let (ws, report) = ws.merge(&cx).await;
        // The parent has to learn this, or it will build on work that is
        // not in the tree.
        assert!(report.needs_attention, "{report:?}");
        assert!(report.note.contains("do NOT apply"), "{}", report.note);
        assert!(report.note.contains(".patch"), "{}", report.note);
        assert_eq!(read(&project.join("app.rs")), "fn main() { user(); }\n");
        ws.discard().await;
    }

    #[tokio::test]
    async fn a_checkout_that_vanished_is_reported_not_read_as_empty() {
        let Some((project, cx)) = project("vanished") else {
            return;
        };
        let ws = Workspace::create(Isolation::Worktree, project.clone(), "a1")
            .await
            .unwrap();
        std::fs::write(ws.root().join("app.rs"), "fn main() { work(); }\n").unwrap();

        // Something took the checkout out from under us mid-flight. The
        // dangerous reading is "changed no files", which would tell the
        // parent its child did nothing rather than that its work was lost.
        std::fs::remove_dir_all(ws.root()).unwrap();

        let (ws, report) = ws.merge(&cx).await;
        assert!(report.needs_attention, "{report:?}");
        assert!(
            report.note.contains("reading back its changes failed"),
            "{}",
            report.note
        );
        assert!(
            !report.note.contains("changed no files"),
            "a lost checkout must not read as an empty one: {}",
            report.note
        );
        assert_eq!(
            read(&project.join("app.rs")),
            "fn main() {}\n",
            "the project must be untouched"
        );
        ws.discard().await;
    }

    /// Unix-only: forcing a checkpoint failure needs an unreadable file, and
    /// file modes are the one lever Windows does not give us. The repo
    /// already scopes its mode-dependent tests this way.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_merge_that_cannot_be_checkpointed_applies_nothing() {
        use std::os::unix::fs::PermissionsExt;

        let Some((project, _)) = project("nocheckpoint") else {
            return;
        };
        // Checkpointing on, unlike the other tests here: this is about what
        // happens when that snapshot cannot be taken.
        let mut config = crate::domain::Config::default();
        config.safety.checkpoint_on_mutation = true;
        let (ctx, _rx) = crate::providers::ctx::test_exec_context_with_config(
            TurnId(9),
            ToolCallId(9),
            project.clone(),
            config,
        );
        let cx = MergeContext::from_exec(&ctx);

        let ws = Workspace::create(Isolation::Worktree, project.clone(), "a1")
            .await
            .unwrap();
        std::fs::write(ws.root().join("app.rs"), "fn main() { work(); }\n").unwrap();
        // The project-side file the checkpoint must snapshot before the merge
        // overwrites it. Unreadable, so the snapshot fails.
        let target = project.join("app.rs");
        let original = std::fs::metadata(&target).unwrap().permissions();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000)).unwrap();

        let (ws, report) = ws.merge(&cx).await;

        std::fs::set_permissions(&target, original).unwrap();
        assert!(report.needs_attention, "{report:?}");
        assert!(
            report
                .note
                .contains("checkpointing the project before merging failed"),
            "{}",
            report.note
        );
        // The invariant: merging without a restore point would leave the
        // change unrecoverable, so nothing is applied.
        assert_eq!(
            read(&target),
            "fn main() {}\n",
            "a merge with no restore point behind it must not land"
        );
        ws.discard().await;
    }

    #[tokio::test]
    async fn discarding_an_isolated_workspace_removes_its_checkout() {
        let Some((project, _cx)) = project("discard") else {
            return;
        };
        let ws = Workspace::create(Isolation::Worktree, project.clone(), "a1")
            .await
            .unwrap();
        let root = ws.root().to_path_buf();
        assert!(root.exists());
        ws.discard().await;
        assert!(!root.exists(), "a discarded checkout must not linger");
        assert!(project.exists(), "the project itself is never touched");
    }

    #[tokio::test]
    async fn isolation_outside_a_repo_reports_instead_of_falling_back() {
        let dir = std::env::temp_dir().join(format!("mermaid_ws_norepo_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = Workspace::create(Isolation::Worktree, dir, "a1")
            .await
            .unwrap_err();
        assert!(err.contains("could not isolate"), "{err}");
        assert!(err.contains("git repository"), "{err}");
    }
}
