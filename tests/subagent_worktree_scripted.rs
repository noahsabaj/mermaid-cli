//! Worktree isolation driven by a scripted model.
//!
//! The live suite (`subagent_worktree.rs`) proves a real model can use an
//! isolated workspace. It cannot pin the cases that matter most and that a
//! real model will not produce on demand:
//!
//!   * two children editing the same line, so exactly one merge conflicts
//!   * disjoint children merging concurrently without interfering
//!   * what the child was actually *told* about being isolated
//!
//! These run offline, in milliseconds, on every `cargo nextest run` — no key,
//! no network, no cost. Everything below the provider seam is the production
//! path: real reducer, real effect runner, real tool registry, real spawner.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mermaid_cli::providers::ProviderFactory;
use mermaid_cli::providers::ctx::test_exec_context_with_config;
use mermaid_cli::providers::model::ModelProvider;
use mermaid_cli::providers::tool::ToolExecutor;
use mermaid_cli::providers::tool::subagent::{SubagentSpawner, SubagentTool};
use mermaid_cli::providers::tool::web::WebCapabilities;
use mermaid_domain::{ToolCallId, ToolStatus, TurnId};
use mermaid_runtime::SafetyMode;
use mermaid_runtime::git::git;

#[path = "harness/stub_model.rs"]
mod stub_model;
use stub_model::{ScriptedModel, Turn};

/// The id the stub is seeded under. Nothing builds a provider for it.
const STUB: &str = "stub/scripted";

/// A git repo with one committed file. `None` when git is unavailable.
fn project(tag: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("mermaid_wts_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the fixture needs its own temp directory");
    if git(&dir).args(["init", "-q"]).run().is_err() {
        return None;
    }
    std::fs::write(dir.join("app.rs"), "fn main() {}\n")
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

fn config() -> mermaid_domain::Config {
    let mut config = mermaid_domain::Config::default();
    // The child must write without an approval UI; the gate has its own tests.
    config.safety.mode = SafetyMode::FullAccess;
    // Never write checkpoints into the developer's real data dir.
    config.safety.checkpoint_on_mutation = false;
    config
}

/// A spawner whose model calls are answered by `models`, one stub per child.
fn spawner(models: Vec<Arc<ScriptedModel>>) -> Arc<SubagentSpawner> {
    let config = config();
    let seeds: Vec<(String, Arc<dyn ModelProvider>)> = models
        .into_iter()
        .enumerate()
        .map(|(i, m)| (format!("{STUB}-{i}"), m as Arc<dyn ModelProvider>))
        .collect();
    let providers = Arc::new(ProviderFactory::with_seeded_providers(
        config.clone(),
        seeds,
    ));
    let web = Arc::new(WebCapabilities::resolve(&config.web));
    Arc::new(SubagentSpawner::new(providers, web))
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .expect("reading a fixture file back must succeed")
        .replace("\r\n", "\n")
}

/// Script for a child that writes `content` to `path` and reports.
fn writes(path: &str, content: &str) -> Arc<ScriptedModel> {
    ScriptedModel::new([
        Turn::tool(
            "write_file",
            serde_json::json!({ "path": path, "content": content }),
        ),
        Turn::say(&format!("Wrote {path}.")),
    ])
}

#[tokio::test]
async fn an_isolated_child_is_told_that_it_is_isolated() {
    let Some(project) = project("preamble") else {
        return;
    };
    let model = writes("app.rs", "fn main() { edited(); }\n");
    let tool = SubagentTool::new(spawner(vec![model.clone()]));
    let (ctx, _rx) =
        test_exec_context_with_config(TurnId(1), ToolCallId(1), project.clone(), config());

    let outcome = tool
        .execute(
            serde_json::json!({
                "prompt": "edit the app",
                "isolation": "worktree",
                "model": format!("{STUB}-0"),
            }),
            ctx,
        )
        .await;
    assert_eq!(
        outcome.status,
        ToolStatus::Success,
        "{}",
        outcome.model_content
    );

    // Without this the child reports edits the user cannot see and reads as
    // a liar — or goes hunting for the "real" project to fix that.
    let first = model.requests().first().cloned().expect("one model call");
    assert!(
        first.system_prompt.contains("private copy of the project"),
        "an isolated child's system prompt must say so: {}",
        first.system_prompt
    );
    assert!(
        first.system_prompt.contains("do not commit"),
        "the child must be told finishing is what lands the work"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
async fn a_shared_child_is_not_told_it_is_isolated() {
    let Some(project) = project("nopreamble") else {
        return;
    };
    let model = writes("app.rs", "fn main() { shared(); }\n");
    let tool = SubagentTool::new(spawner(vec![model.clone()]));
    let (ctx, _rx) =
        test_exec_context_with_config(TurnId(1), ToolCallId(1), project.clone(), config());

    let outcome = tool
        .execute(
            serde_json::json!({
                "prompt": "edit the app",
                "model": format!("{STUB}-0"),
            }),
            ctx,
        )
        .await;
    assert_eq!(
        outcome.status,
        ToolStatus::Success,
        "{}",
        outcome.model_content
    );

    let first = model.requests().first().cloned().expect("one model call");
    assert!(
        !first.system_prompt.contains("private copy of the project"),
        "a shared child must not be told it is isolated"
    );
    // It wrote straight through, as it always has.
    assert_eq!(read(&project.join("app.rs")), "fn main() { shared(); }\n");

    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
async fn two_children_editing_one_file_produce_exactly_one_conflict() {
    let Some(project) = project("conflict") else {
        return;
    };
    // The case worktree isolation exists to make visible. Shared, these two
    // interleave and the loser's work vanishes with nothing said. Isolated,
    // one merge lands and the other is reported, with its patch saved.
    let first = writes("app.rs", "fn main() { first(); }\n");
    let second = writes("app.rs", "fn main() { second(); }\n");
    let spawner = spawner(vec![first, second]);

    let mut handles = Vec::new();
    for i in 0..2 {
        let tool = SubagentTool::new(spawner.clone());
        let (ctx, _rx) =
            test_exec_context_with_config(TurnId(1), ToolCallId(i + 1), project.clone(), config());
        handles.push(tokio::spawn(async move {
            tool.execute(
                serde_json::json!({
                    "prompt": "edit the app",
                    "isolation": "worktree",
                    "model": format!("{STUB}-{i}"),
                }),
                ctx,
            )
            .await
        }));
    }

    let mut landed = 0;
    let mut conflicted = 0;
    for handle in handles {
        let outcome = handle.await.unwrap();
        if outcome.status == ToolStatus::Success {
            landed += 1;
            assert!(outcome.model_content.contains("applied to the project"));
        } else {
            conflicted += 1;
            let msg = outcome.error_message().unwrap_or_default();
            // A child whose work did not land must not read as success, and
            // the work must be recoverable.
            assert!(msg.contains("did not land"), "{msg}");
            assert!(msg.contains("do NOT apply"), "{msg}");
            // The path is on its own line precisely because data dirs
            // contain spaces (macOS `Library/Application Support`), so read
            // the whole line rather than splitting on whitespace.
            let patch = msg
                .lines()
                .map(str::trim)
                .find(|line| line.ends_with(".patch"))
                .expect("the rejected patch must be named on its own line");
            assert!(Path::new(patch).exists(), "saved patch missing: {patch}");
        }
    }
    assert_eq!((landed, conflicted), (1, 1), "exactly one of the two lands");

    // Whichever won, the file is one child's work — never a blend of both.
    let final_text = read(&project.join("app.rs"));
    assert!(
        final_text == "fn main() { first(); }\n" || final_text == "fn main() { second(); }\n",
        "the project must hold one child's intent, not a merge of two: {final_text}"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
async fn disjoint_children_all_land_concurrently() {
    let Some(project) = project("disjoint") else {
        return;
    };
    // The payoff case: several writing children at once, none colliding.
    let names = ["alpha", "beta", "gamma", "delta"];
    let models: Vec<_> = names
        .iter()
        .map(|n| writes(&format!("{n}.rs"), &format!("// {n}\n")))
        .collect();
    let spawner = spawner(models);

    let mut handles = Vec::new();
    for (i, _) in names.iter().enumerate() {
        let tool = SubagentTool::new(spawner.clone());
        let (ctx, _rx) = test_exec_context_with_config(
            TurnId(1),
            ToolCallId(i as u64 + 1),
            project.clone(),
            config(),
        );
        handles.push(tokio::spawn(async move {
            tool.execute(
                serde_json::json!({
                    "prompt": "write your file",
                    "isolation": "worktree",
                    "model": format!("{STUB}-{i}"),
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
            "every disjoint child should land: {}",
            outcome.model_content
        );
    }

    for name in names {
        let path = project.join(format!("{name}.rs"));
        assert!(path.exists(), "{name}.rs should have landed");
        assert_eq!(read(&path), format!("// {name}\n"));
    }
    // The file none of them touched is untouched.
    assert_eq!(read(&project.join("app.rs")), "fn main() {}\n");

    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
async fn a_child_that_writes_nothing_reports_an_empty_merge() {
    let Some(project) = project("empty") else {
        return;
    };
    let model = ScriptedModel::new([Turn::say("Nothing needed changing.")]);
    let tool = SubagentTool::new(spawner(vec![model]));
    let (ctx, _rx) =
        test_exec_context_with_config(TurnId(1), ToolCallId(1), project.clone(), config());

    let outcome = tool
        .execute(
            serde_json::json!({
                "prompt": "look around",
                "isolation": "worktree",
                "model": format!("{STUB}-0"),
            }),
            ctx,
        )
        .await;
    assert_eq!(outcome.status, ToolStatus::Success);
    assert!(
        outcome.model_content.contains("changed no files"),
        "{}",
        outcome.model_content
    );
    assert_eq!(read(&project.join("app.rs")), "fn main() {}\n");

    let _ = std::fs::remove_dir_all(&project);
}
