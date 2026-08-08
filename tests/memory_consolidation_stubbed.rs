//! `/memory consolidate` under a scripted model.
//!
//! Consolidation hands the model the user's whole memory corpus and then acts
//! on a JSON `{"prune": [...]}` list by **deleting files**. `parse_prune_plan`
//! has a unit test; nothing covered acting on the result. Every failure mode
//! here is silent data loss, and every one of them needs a model that answers
//! badly on cue:
//!
//!   * output that will not parse
//!   * a plan naming ids that do not exist (a hallucinating model)
//!   * a plan naming every id (an over-eager one)
//!
//! There is deliberately no "secrets must not reach the provider" test here,
//! unlike the classifier suite. Memory bodies are redacted at *write* time
//! (`app::memory::save`), so what is on disk is already clean and the corpus
//! is user-authored content going to the user's own model — the same status
//! as a chat message.
//!
//! # Blast radius
//!
//! `memory_roots` spans global, project-private, and project-shared scopes,
//! so a consolidation run in a temp repo still sees the developer's real
//! memories, and a prune naming one would delete it. Every id here is
//! uniquified per process and the scripted plans name only those, so no
//! amount of test failure can reach real memory.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mermaid_cli::effect::EffectRunner;
use mermaid_cli::providers::ProviderFactory;
use mermaid_cli::providers::model::ModelProvider;
use mermaid_cli::providers::tool::ToolRegistry;
use mermaid_domain::{Cmd, Msg};
use mermaid_runtime::git::git;

#[path = "harness/stub_model.rs"]
mod stub_model;
use stub_model::{ScriptedModel, Turn};

const STUB: &str = "stub/scripted";

/// Fixture ids are process-unique so a prune can never name a real memory.
fn id(base: &str) -> String {
    format!("stubfix-{base}-{}", std::process::id())
}

/// A git repo with `facts` written as project-shared memories, which live
/// inside the repo at `.mermaid/memory/` rather than in the real data dir.
fn project(tag: &str, facts: &[(String, &str)]) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("mermaid_mem_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    if git(&dir).args(["init", "-q"]).run().is_err() {
        return None;
    }
    let mem = dir.join(".mermaid").join("memory");
    std::fs::create_dir_all(&mem).unwrap();
    for (id, body) in facts {
        std::fs::write(
            mem.join(format!("{id}.md")),
            format!(
                "---\nname: {id}\ndescription: fixture fact {id}\nmetadata:\n  type: project\n---\n\n{body}\n"
            ),
        )
        .unwrap();
    }
    Some(dir)
}

fn memory_file(project: &Path, id: &str) -> PathBuf {
    project
        .join(".mermaid")
        .join("memory")
        .join(format!("{id}.md"))
}

/// Run one consolidation against `script` and return the reported text.
async fn consolidate(project: &Path, script: Vec<Turn>) -> (String, Arc<ScriptedModel>) {
    let model = ScriptedModel::new(script);
    let providers = Arc::new(ProviderFactory::with_seeded_providers(
        mermaid_domain::Config::default(),
        [(STUB.to_string(), model.clone() as Arc<dyn ModelProvider>)],
    ));
    let (mut runner, mut rx) = EffectRunner::pair_from(
        project.to_path_buf(),
        providers,
        Arc::new(ToolRegistry::new()),
    );

    runner.dispatch(Cmd::ConsolidateMemory {
        model_id: STUB.to_string(),
    });

    // The report is the terminal `RuntimeText`; `MemoryChanged` may precede it.
    let report = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(msg) = rx.recv().await {
            if let Msg::RuntimeText(text) = msg {
                return text;
            }
        }
        panic!("consolidation produced no report");
    })
    .await
    .expect("consolidation never reported");

    runner.shutdown().await;
    (report, model)
}

#[tokio::test]
async fn a_valid_plan_prunes_exactly_what_it_named() {
    let (keep, drop) = (id("keep"), id("drop"));
    let Some(project) = project(
        "valid",
        &[
            (keep.clone(), "The build uses just check."),
            (drop.clone(), "The build uses just check."),
        ],
    ) else {
        return;
    };

    let (report, _) = consolidate(
        &project,
        vec![Turn::say(&format!(
            r#"Looks like a duplicate. {{"prune": ["{drop}"], "reason": "exact duplicate"}}"#
        ))],
    )
    .await;

    assert!(report.contains("pruned 1 fact"), "{report}");
    assert!(
        !memory_file(&project, &drop).exists(),
        "the named fact should be gone"
    );
    assert!(
        memory_file(&project, &keep).exists(),
        "the unnamed fact must survive"
    );
    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
async fn an_unparseable_plan_deletes_nothing() {
    let fact = id("unparseable");
    let Some(project) = project(
        "unparseable",
        &[
            (fact.clone(), "A durable fact."),
            (
                id("unparseable-b"),
                "A second fact, so there is something to compare.",
            ),
        ],
    ) else {
        return;
    };

    let (report, _) = consolidate(
        &project,
        vec![Turn::say(
            "I looked at your memories and they all seem useful!",
        )],
    )
    .await;

    assert!(report.contains("couldn't parse"), "{report}");
    assert!(
        memory_file(&project, &fact).exists(),
        "a model that ignored the output contract must not cost the user a memory"
    );
    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
async fn a_hallucinated_plan_deletes_nothing_and_says_so() {
    // The model names ids that do not exist. Deleting by prefix, or treating
    // a miss as a match, would take out the wrong fact.
    let fact = id("real");
    let Some(project) = project(
        "hallucinated",
        &[
            (fact.clone(), "A durable fact."),
            (
                id("real-b"),
                "A second fact, so there is something to compare.",
            ),
        ],
    ) else {
        return;
    };

    let (report, _) = consolidate(
        &project,
        vec![Turn::say(
            r#"{"prune": ["not-a-real-id", "also-not-real"], "reason": "stale"}"#,
        )],
    )
    .await;

    assert!(
        report.contains("none matched"),
        "the user must be told nothing matched: {report}"
    );
    assert!(
        memory_file(&project, &fact).exists(),
        "a hallucinated id must not delete a real memory"
    );
    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
async fn an_empty_plan_is_reported_as_nothing_to_prune() {
    let fact = id("nothing");
    let Some(project) = project(
        "empty",
        &[
            (fact.clone(), "A durable fact."),
            (
                id("nothing-b"),
                "A second fact, so there is something to compare.",
            ),
        ],
    ) else {
        return;
    };

    let (report, _) = consolidate(
        &project,
        vec![Turn::say(r#"{"prune": [], "reason": "all still current"}"#)],
    )
    .await;

    assert!(report.contains("nothing to prune"), "{report}");
    assert!(
        report.contains("all still current"),
        "the reason carries: {report}"
    );
    assert!(memory_file(&project, &fact).exists());
    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
async fn a_provider_failure_deletes_nothing() {
    let fact = id("failure");
    let Some(project) = project(
        "failure",
        &[
            (fact.clone(), "A durable fact."),
            (
                id("failure-b"),
                "A second fact, so there is something to compare.",
            ),
        ],
    ) else {
        return;
    };

    let (report, _) = consolidate(&project, vec![Turn::fail("503 Service Unavailable")]).await;

    assert!(report.contains("failed"), "{report}");
    assert!(
        memory_file(&project, &fact).exists(),
        "a dead provider must not cost the user a memory"
    );
    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
async fn a_single_memory_short_circuits_without_a_model_call() {
    // Nothing to compare against, so there is nothing to consolidate. Spending
    // a call (and shipping the corpus) to learn that would be waste.
    let fact = id("lonely");
    let Some(project) = project("lonely", &[(fact.clone(), "The only fact.")]) else {
        return;
    };
    // Only meaningful when this process has no other memories in scope; the
    // real global corpus would push the count past the threshold.
    if mermaid_cli::app::memory::entries_with_bodies(&project).len() != 1 {
        let _ = std::fs::remove_dir_all(&project);
        return;
    }

    let (report, model) = consolidate(&project, vec![Turn::say("unused")]).await;
    assert!(report.contains("Nothing to consolidate"), "{report}");
    assert_eq!(model.calls(), 0, "no corpus should have been sent");
    assert!(memory_file(&project, &fact).exists());
    let _ = std::fs::remove_dir_all(&project);
}
