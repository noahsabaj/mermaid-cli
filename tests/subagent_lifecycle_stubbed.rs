//! Subagent lifecycle under a scripted model: what a child is advertised,
//! what a continuation reuses, and what happens when one is backgrounded.
//!
//! `build_child_registry` has a unit test for which tools it *builds*. That is
//! not the same question as which tools the child's model was *told about* —
//! the request is assembled well downstream of the registry, and an `explore`
//! child that gets advertised `write_file` would happily call it. Only a stub
//! sees the request that actually went out.
//!
//! Continuations and detach are likewise invisible without a model: both are
//! defined by what happens across two drives of the same child.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mermaid_cli::providers::ProviderFactory;
use mermaid_cli::providers::ctx::test_exec_context_with_config;
use mermaid_cli::providers::model::ModelProvider;
use mermaid_cli::providers::tool::ToolExecutor;
use mermaid_cli::providers::tool::subagent::{SubagentSpawner, SubagentTool};
use mermaid_cli::providers::tool::web::WebCapabilities;
use mermaid_domain::{Msg, ToolCallId, ToolStatus, TurnId};
use mermaid_runtime::SafetyMode;

#[path = "harness/stub_model.rs"]
mod stub_model;
use stub_model::{ScriptedModel, Turn};

const STUB: &str = "stub/scripted";

fn config() -> mermaid_domain::Config {
    let mut config = mermaid_domain::Config::default();
    config.safety.mode = SafetyMode::FullAccess;
    config.safety.checkpoint_on_mutation = false;
    config
}

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

fn workdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mermaid_sublife_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the fixture needs its own temp directory");
    dir
}

#[tokio::test]
async fn an_explore_child_is_advertised_only_read_only_tools() {
    // A read-only ceiling that still advertises mutating tools invites the
    // child to call one and get denied — wasted turns at best, and a
    // misleading capability claim at worst.
    let model = ScriptedModel::new([Turn::say("Found it in src/main.rs.")]);
    let tool = SubagentTool::new(spawner(vec![model.clone()]));
    let (ctx, _rx) = test_exec_context_with_config(TurnId(1), ToolCallId(1), workdir(), config());

    let outcome = tool
        .execute(
            serde_json::json!({
                "prompt": "find the entry point",
                "type": "explore",
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

    let advertised: Vec<String> = model.requests()[0]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert!(
        advertised.iter().any(|t| t == "read_file"),
        "explore still needs to read: {advertised:?}"
    );
    for forbidden in ["write_file", "delete_file", "apply_patch", "agent"] {
        assert!(
            !advertised.iter().any(|t| t == forbidden),
            "an explore child must not be offered {forbidden}: {advertised:?}"
        );
    }
}

#[tokio::test]
async fn a_child_emitting_several_tool_calls_runs_them_all_and_sees_every_result() {
    // `TurnState::ExecutingTools` parallelizes a turn's tool calls, and the
    // reducer gates the follow-up model call on every outcome landing. Losing
    // one would hang the child until its timeout; feeding back the wrong
    // count would have it reason from a partial picture. The agent-loop suite
    // checks the calls reach the reducer against an empty registry; this runs
    // them for real.
    let dir = std::env::temp_dir().join(format!("mermaid_multitool_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let model = ScriptedModel::new([
        Turn::tools([
            (
                "write_file".to_string(),
                serde_json::json!({"path": "a.txt", "content": "alpha\n"}),
            ),
            (
                "write_file".to_string(),
                serde_json::json!({"path": "b.txt", "content": "beta\n"}),
            ),
            (
                "write_file".to_string(),
                serde_json::json!({"path": "c.txt", "content": "gamma\n"}),
            ),
        ]),
        Turn::say("Wrote all three."),
    ]);
    let tool = SubagentTool::new(spawner(vec![model.clone()]));
    let (ctx, _rx) = test_exec_context_with_config(TurnId(1), ToolCallId(1), dir.clone(), config());

    let outcome = tool
        .execute(
            serde_json::json!({
                "prompt": "write the three files",
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

    for (name, want) in [
        ("a.txt", "alpha\n"),
        ("b.txt", "beta\n"),
        ("c.txt", "gamma\n"),
    ] {
        let got = std::fs::read_to_string(dir.join(name))
            .unwrap_or_else(|e| panic!("{name} was not written: {e}"))
            .replace("\r\n", "\n");
        assert_eq!(got, want, "{name}");
    }

    // The second call must carry all three results, or the child reasoned
    // about work it could not see the outcome of.
    let requests = model.requests();
    assert_eq!(requests.len(), 2, "one turn of tools, then the report");
    let results = requests[1]
        .messages
        .iter()
        .filter(|m| m.role == mermaid_model::models::MessageRole::Tool)
        .count();
    assert_eq!(
        results, 3,
        "every tool result must feed back before the next turn"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_continuation_reuses_the_context_the_child_already_built() {
    // The whole point of `agent_id`: the follow-up must not re-explore. If
    // the second drive started from an empty history it would still answer,
    // just wastefully and without what it learned — a silent regression.
    let model = ScriptedModel::new([
        Turn::say("The entry point is src/main.rs."),
        Turn::say("It calls run() on line 12."),
    ]);
    let spawner = spawner(vec![model.clone()]);

    let first = SubagentTool::new(spawner.clone())
        .execute(
            serde_json::json!({
                "prompt": "find the entry point",
                "model": format!("{STUB}-0"),
            }),
            test_exec_context_with_config(TurnId(1), ToolCallId(1), workdir(), config()).0,
        )
        .await;
    assert_eq!(first.status, ToolStatus::Success);
    let agent_id = first
        .model_content
        .split("[agent_id: ")
        .nth(1)
        .and_then(|s| s.split(' ').next())
        .expect("the report carries a continuation handle")
        .to_string();

    let second = SubagentTool::new(spawner)
        .execute(
            serde_json::json!({
                "prompt": "what does it call?",
                "agent_id": agent_id,
            }),
            test_exec_context_with_config(TurnId(2), ToolCallId(2), workdir(), config()).0,
        )
        .await;
    assert_eq!(
        second.status,
        ToolStatus::Success,
        "{}",
        second.model_content
    );

    let requests = model.requests();
    assert_eq!(requests.len(), 2, "one call per drive");
    let second_history: Vec<String> = requests[1]
        .messages
        .iter()
        .map(|m| m.content.clone())
        .collect();
    assert!(
        second_history
            .iter()
            .any(|c| c.contains("find the entry point")),
        "the continuation must carry the first drive's prompt: {second_history:?}"
    );
    assert!(
        second_history
            .iter()
            .any(|c| c.contains("The entry point is src/main.rs.")),
        "the continuation must carry what the child already answered: {second_history:?}"
    );
    assert!(
        second_history
            .iter()
            .any(|c| c.contains("what does it call?")),
        "and the new prompt: {second_history:?}"
    );
}

#[tokio::test]
async fn backgrounding_a_child_returns_at_once_and_reports_later() {
    // Ctrl+B. The turn must be released immediately while the child keeps
    // running, and its report must arrive out of band rather than being lost.
    let model = ScriptedModel::new([Turn::stall(1), Turn::say("Finished in the background.")]);
    let tool = SubagentTool::new(spawner(vec![model]));

    let (mut ctx, _rx) =
        test_exec_context_with_config(TurnId(1), ToolCallId(1), workdir(), config());
    let (notify_tx, mut notify_rx) = tokio::sync::mpsc::channel::<Msg>(64);
    ctx.notify = Some(notify_tx);
    let background = ctx.background.clone();

    let handle = tokio::spawn(async move {
        tool.execute(
            serde_json::json!({
                "prompt": "take your time",
                "description": "slow child",
                "model": format!("{STUB}-0"),
            }),
            ctx,
        )
        .await
    });

    // Fire Ctrl+B while the child is still stalling.
    tokio::time::sleep(Duration::from_millis(100)).await;
    background.cancel();

    let outcome = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("detach must release the turn promptly")
        .unwrap();
    assert_eq!(outcome.status, ToolStatus::Success);
    assert!(
        outcome.model_content.contains("moved to background"),
        "the model needs to know the child was released: {}",
        outcome.model_content
    );

    // The child keeps going and posts its report through the notify channel.
    let report = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(msg) = notify_rx.recv().await {
            if let Msg::BackgroundAgentFinished { report, .. } = msg {
                return report;
            }
        }
        panic!("the detached child never reported");
    })
    .await
    .expect("timed out waiting for the background report");
    assert!(
        report.contains("Finished in the background."),
        "the detached child's work must still reach the conversation: {report}"
    );
}
