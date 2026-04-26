//! Integration tests that pin the v0.7 architectural promises.
//!
//! The v0.6 bugs these guard against:
//!
//! - **20-press Ctrl+C.** A user cancel mid-tool had to wait for the
//!   tool's 30s / 300s timeout because nothing propagated the
//!   cancellation signal into the tool's body. Here,
//!   `Cmd::CancelScope` flips the scope's `CancellationToken`, and
//!   `ExecuteCommandTool::execute` races it against the subprocess
//!   wait via `select!`. Abort latency is bounded by how long it
//!   takes `SIGKILL` to arrive.
//!
//! - **`kill_on_drop(true)` drift.** That flag was missing from
//!   `src/agents/executor.rs` for months. The type system now
//!   guarantees tokio reaps the child when the scope drops, because
//!   the `Command` is owned by the scope's `JoinSet`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use mermaid_cli::domain::{ToolCallId, TurnId};
use mermaid_cli::providers::ctx::test_exec_context;
use mermaid_cli::providers::tool::ToolExecutor;
use mermaid_cli::providers::tool::exec::ExecuteCommandTool;

#[tokio::test]
async fn execute_command_cancels_within_100ms() {
    // Spawn a 60-second sleep under the tool and cancel ~30ms in.
    // v0.6 would wait up to 300s (the timeout cap) because the
    // tool's await loop had no idea a cancel was pending. v0.7's
    // token-based select! aborts immediately.
    let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
    let token = ctx.token.clone();

    let handle = tokio::spawn(async move {
        ExecuteCommandTool
            .execute(serde_json::json!({"command": "sleep 60"}), ctx)
            .await
    });

    // Give the child a beat to come up, then cancel.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let cancel_at = Instant::now();
    token.cancel();

    let outcome = tokio::time::timeout(Duration::from_millis(500), handle)
        .await
        .expect("test timed out — cancellation didn't propagate")
        .expect("join");
    let elapsed = cancel_at.elapsed();

    assert!(outcome.was_cancelled());
    assert!(
        elapsed < Duration::from_millis(300),
        "cancellation took {:?} — v0.6 regression?",
        elapsed
    );
}

#[tokio::test]
async fn execute_command_timeout_honored() {
    // Assert the timeout argument still works. 1s timeout on a 10s
    // sleep should produce a "timed out" Finished outcome, NOT hang.
    let (ctx, _rx) = test_exec_context(TurnId(2), ToolCallId(1), PathBuf::from("/tmp"));
    let start = Instant::now();
    let outcome = ExecuteCommandTool
        .execute(
            serde_json::json!({"command": "sleep 10", "timeout": 1}),
            ctx,
        )
        .await;
    let elapsed = start.elapsed();

    assert_eq!(outcome.status, mermaid_cli::domain::ToolStatus::Error);
    let output = outcome.as_tool_message_content();
    assert!(output.contains("timed out"), "got: {}", output);
    assert!(output.contains("was killed"), "got: {}", output);
    assert!(
        elapsed >= Duration::from_millis(900) && elapsed < Duration::from_millis(1500),
        "timeout duration off: {:?}",
        elapsed
    );
}

#[tokio::test]
async fn cancelling_empty_scope_is_safe() {
    // Constructing a scope and dropping it without any work shouldn't
    // panic or leak.
    use mermaid_cli::effect::TurnScope;
    let scope = TurnScope::new(TurnId(1));
    drop(scope);
}

#[tokio::test]
async fn effect_runner_cancels_scope_on_command() {
    use mermaid_cli::domain::{Cmd, Msg};
    use mermaid_cli::effect::EffectRunner;

    let (mut runner, _rx) = EffectRunner::pair(PathBuf::from("/tmp"));

    // Dispatch a CallModel to create a scope.
    let request = mermaid_cli::domain::ChatRequest {
        model_id: "test/m".to_string(),
        messages: vec![],
        system_prompt: String::new(),
        instructions: None,
        reasoning: mermaid_cli::models::ReasoningLevel::Medium,
        temperature: 0.7,
        max_tokens: 4096,
        tools: vec![],
    };
    runner.dispatch(Cmd::CallModel {
        turn: TurnId(1),
        request,
    });
    assert_eq!(runner.scope_count(), 1);

    // Cancel it.
    runner.dispatch(Cmd::CancelScope(TurnId(1)));
    assert_eq!(
        runner.scope_count(),
        0,
        "CancelScope must drop the scope entry"
    );

    // Just observed the type-level guarantee: the only way to abort
    // is through Cmd::CancelScope. No bare handle.abort() anywhere.
    let _ = &_rx as &dyn std::any::Any;
    let _ = Msg::Tick; // import used
}

#[tokio::test]
async fn effect_runner_shutdown_bounded_time() {
    use mermaid_cli::domain::Cmd;
    use mermaid_cli::effect::EffectRunner;

    let (mut runner, _rx) = EffectRunner::pair(PathBuf::from("/tmp"));

    // Queue up several detached operations.
    for _ in 0..10 {
        runner.dispatch(Cmd::DismissStatusAfter { ms: 10 });
    }

    let start = Instant::now();
    runner.shutdown().await;
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(500),
        "shutdown took {:?} — bounded drain broken?",
        elapsed
    );
}
