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
async fn execute_command_cancellation_aborts_promptly() {
    // Spawn a 60-second sleep under the tool and cancel ~30ms in.
    // v0.6 would wait up to 300s (the timeout cap) because the
    // tool's await loop had no idea a cancel was pending. v0.7's
    // token-based select! aborts well before then.
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

    // The 5s outer timeout is the real "didn't hang" guard — a propagation
    // regression would block until the 60s sleep, well past 5s.
    let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("test timed out — cancellation didn't propagate")
        .expect("join");
    let elapsed = cancel_at.elapsed();

    assert!(outcome.was_cancelled());
    // The intent is "aborts promptly, not after the 60s sleep / 300s cap" —
    // not a hard sub-300ms SLA. A tight bound here measured CI scheduling /
    // process-teardown jitter (flaked on loaded windows runners). 2s keeps a
    // wide margin while still catching a real "cancel didn't propagate" hang.
    assert!(
        elapsed < Duration::from_secs(2),
        "cancellation took {:?} — far slower than expected (regression?)",
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
    // The 1s timeout must fire (>=900ms, so it didn't abort instantly) and the
    // 10s sleep must be killed early (<8s). The ceiling measures process-kill +
    // task teardown overhead on top of the 1s timeout, not the timeout itself,
    // so it's deliberately generous: a 5s ceiling still flaked on loaded
    // windows runners (observed >5s). 8s stays comfortably below the 10s full
    // sleep, so it still catches a real "timeout never fired" regression.
    assert!(
        elapsed >= Duration::from_millis(900) && elapsed < Duration::from_secs(8),
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
        ollama_num_ctx: None,
        ollama_allow_ram_offload: None,
        resolved_context_window: None,
        resolved_max_output: None,
        output_schema: None,
        suppress_auto_compact: false,
        suppressed_builtin_tools: Vec::new(),
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
    for i in 0..10 {
        runner.dispatch(Cmd::SetTerminalTitle(format!("t{i}")));
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
