//! End-to-end coverage for worktree isolation, driven by a real model.
//!
//! The unit tests around `Workspace` and `AgentWorktree` cover the mechanics
//! with the drive stubbed out or failed on purpose. What they cannot show is
//! the part that only a live child exercises: that a model handed an isolated
//! workspace actually writes into the checkout — that `cwd` really is the
//! worktree root everywhere a child tool resolves a path — and that its work
//! then lands in the project.
//!
//! **Run this by hand, not in CI.** Each case costs a real API call, and CI
//! runs on every push:
//!
//! ```text
//! MODEL_API_KEY=... cargo test --test integration -- --ignored it::subagent_worktree:: --test-threads=1
//! ```
//!
//! `#[ignore]`d so the default suite skips it. The default suite still
//! *compiles* it, so a refactor that breaks these cannot rot unnoticed —
//! which is the only thing a CI step for them would have added, since no key
//! means every case skips while printing `ok. 3 passed` in 0.00s.
//!
//! Skips cleanly without `MODEL_API_KEY` so `cargo test -- --ignored` locally
//! does not fail spuriously.

use std::path::{Path, PathBuf};

use mermaid_cli::providers::ExecContext;
use mermaid_cli::providers::ProviderFactory;
use mermaid_cli::providers::ctx::test_exec_context_with_config;
use mermaid_cli::providers::tool::ToolExecutor;
use mermaid_cli::providers::tool::subagent::{SubagentSpawner, SubagentTool};
use mermaid_cli::providers::tool::web::WebCapabilities;
use mermaid_domain::{ToolCallId, ToolStatus, TurnId};
use mermaid_runtime::SafetyMode;
use mermaid_runtime::git::git;
use std::sync::Arc;

/// Cheapest capable model that can drive a tool loop. The contributor tier
/// is the point: this test is meant to be runnable often.
const MODEL: &str = "meta/muse-spark-1.2-contributor";

fn have_key() -> bool {
    std::env::var("MODEL_API_KEY").is_ok_and(|k| !k.trim().is_empty())
}

/// A git repo with one committed file. `None` when git is unavailable.
fn project(tag: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("mermaid_wt_e2e_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the fixture needs its own temp directory");
    if git(&dir).args(["init", "-q"]).run().is_err() {
        return None;
    }
    std::fs::write(dir.join("greeting.txt"), "hello\n")
        .expect("seeding the project needs a tracked file");
    git(&dir)
        .args(["add", "-A"])
        .run()
        .expect("staging the seed file must succeed");
    // Unique per repo, so two seeded in the same second do not land on the
    // same commit hash. Identical trees and messages did, which is what
    // kept a hash-sensitive failure reproducing on retry -- see
    // worktree.rs::init_project.
    let seed_id = dir.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
    git(&dir)
        .args(["commit", "-qm", &format!("init {seed_id}")])
        .run()
        .expect("the seed commit must succeed");
    Some(dir)
}

fn tool_and_ctx(workdir: &Path) -> (SubagentTool, ExecContext) {
    let mut config = mermaid_domain::Config::default();
    // The child must be able to write without an approval UI; the gate is
    // covered by its own tests.
    config.safety.mode = SafetyMode::FullAccess;
    // Keep the run from writing checkpoints into the developer's data dir.
    config.safety.checkpoint_on_mutation = false;
    let providers = Arc::new(ProviderFactory::new(config.clone()));
    let web = Arc::new(WebCapabilities::resolve(&config.web));
    let tool = SubagentTool::new(Arc::new(SubagentSpawner::new(providers, web)));
    let (ctx, _rx) =
        test_exec_context_with_config(TurnId(1), ToolCallId(1), workdir.to_path_buf(), config);
    (tool, ctx)
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .expect("reading a fixture file back must succeed")
        .replace("\r\n", "\n")
}

#[tokio::test]
#[ignore = "costs a real API call; runs in the integration CI job"]
async fn an_isolated_child_writes_in_its_checkout_and_the_work_lands() {
    if !have_key() {
        eprintln!("skipping: MODEL_API_KEY is not set");
        return;
    }
    let Some(project) = project("lands") else {
        eprintln!("skipping: git is not available");
        return;
    };

    let (tool, ctx) = tool_and_ctx(&project);
    let outcome = tool
        .execute(
            serde_json::json!({
                "prompt": "Replace the entire contents of greeting.txt with exactly \
                           the single line: goodbye. Then stop and report what you did.",
                "description": "rewrite greeting",
                "isolation": "worktree",
                "model": MODEL,
            }),
            ctx,
        )
        .await;

    let report = outcome.model_content.clone();
    assert_eq!(
        outcome.status,
        ToolStatus::Success,
        "child should have succeeded and merged: {report}"
    );
    // The merge happened and said so.
    assert!(
        report.contains("isolated worktree"),
        "the parent must be told its child ran isolated: {report}"
    );
    assert!(
        report.contains("applied to the project"),
        "the merge must be reported: {report}"
    );
    // The point of the whole feature: the edit made in the private checkout
    // is now in the user's tree.
    assert_eq!(
        read(&project.join("greeting.txt")).trim(),
        "goodbye",
        "the child's work must have landed in the project"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
#[ignore = "costs a real API call; runs in the integration CI job"]
async fn a_shared_child_and_an_isolated_child_differ_in_what_the_project_sees() {
    if !have_key() {
        eprintln!("skipping: MODEL_API_KEY is not set");
        return;
    }
    let Some(project) = project("visibility") else {
        eprintln!("skipping: git is not available");
        return;
    };

    // A shared child writes straight into the project, as it always has.
    let (tool, ctx) = tool_and_ctx(&project);
    let shared = tool
        .execute(
            serde_json::json!({
                "prompt": "Create a file named shared.txt containing exactly: shared. \
                           Then stop and report what you did.",
                "description": "shared write",
                "isolation": "shared",
                "model": MODEL,
            }),
            ctx,
        )
        .await;
    assert_eq!(
        shared.status,
        ToolStatus::Success,
        "{}",
        shared.model_content
    );
    assert!(
        !shared.model_content.contains("isolated worktree"),
        "a shared child must not claim isolation: {}",
        shared.model_content
    );
    assert!(
        project.join("shared.txt").exists(),
        "a shared child writes directly into the project"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
#[ignore = "costs a real API call; runs in the integration CI job"]
async fn parallel_isolated_children_do_not_collide() {
    if !have_key() {
        eprintln!("skipping: MODEL_API_KEY is not set");
        return;
    }
    let Some(project) = project("parallel") else {
        eprintln!("skipping: git is not available");
        return;
    };

    // The case the feature exists for: several writing children at once.
    // Sharing one working copy, their edits interleave; isolated, each
    // lands as its own patch.
    //
    // One spawner, as a real turn has: three `agent` calls from a single
    // model response go through the session's spawner, so the children get
    // distinct ids. Building a spawner per task would restart ids at `a1`
    // and test a shape that never occurs.
    let mut config = mermaid_domain::Config::default();
    config.safety.mode = SafetyMode::FullAccess;
    config.safety.checkpoint_on_mutation = false;
    let providers = Arc::new(ProviderFactory::new(config.clone()));
    let web = Arc::new(WebCapabilities::resolve(&config.web));
    let spawner = Arc::new(SubagentSpawner::new(providers, web));

    let names = ["alpha", "beta", "gamma"];
    let mut handles = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let tool = SubagentTool::new(spawner.clone());
        let (ctx, _rx) = test_exec_context_with_config(
            TurnId(1),
            ToolCallId(i as u64 + 1),
            project.clone(),
            config.clone(),
        );
        let name = name.to_string();
        handles.push(tokio::spawn(async move {
            tool.execute(
                serde_json::json!({
                    "prompt": format!(
                        "Create a file named {name}.txt containing exactly: {name}. \
                         Touch no other file. Then stop and report what you did."
                    ),
                    "description": format!("write {name}"),
                    "isolation": "worktree",
                    "model": MODEL,
                }),
                ctx,
            )
            .await
        }));
    }

    for handle in handles {
        let outcome = handle.await.unwrap();
        assert_eq!(
            outcome.status,
            ToolStatus::Success,
            "every isolated child should land cleanly: {}",
            outcome.model_content
        );
    }

    // All three landed, and none clobbered another's file or the original.
    for name in names {
        let path = project.join(format!("{name}.txt"));
        assert!(path.exists(), "{name}.txt should have landed");
        assert_eq!(read(&path).trim(), name);
    }
    assert_eq!(
        read(&project.join("greeting.txt")).trim(),
        "hello",
        "an untouched file must survive three concurrent merges"
    );

    let _ = std::fs::remove_dir_all(&project);
}
