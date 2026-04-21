# Mermaid Architecture (v0.7+)

This is the short tour of how Mermaid is put together after the v0.7 rewrite. For the why — the bugs the old design made possible and the ones this one rules out — see the commit messages in the `feat(domain)` / `feat(effect)` / `feat(providers)` series.

## One sentence

```
crossterm events + effect results → update(State, Msg) → (State, Vec<Cmd>) → EffectRunner → …
```

Everything else is detail.

## The four laws

1. **One event loop.** A single `tokio::select!` in `src/app/run.rs` drives every turn. Keyboard input, paste events, model streaming chunks, tool completion, tick timers — all of them arrive as `Msg` values, and the select picks whichever is ready. There is no second event loop hiding inside an observer.

2. **Pure reducer.** `fn update(State, Msg) -> (State, Vec<Cmd>)` lives in `src/domain/reducer.rs`. It is synchronous. It does no I/O. It owns no tokio handles. A unit test that exercises the full conversation path only needs `State`, a sequence of `Msg` values, and an `assert_eq!` on the result — no runtime, no terminal.

3. **Effects as data.** The reducer returns `Cmd` values. It never calls a provider, writes a file, or spawns a task. `src/effect/EffectRunner::dispatch` turns each `Cmd` into real work on the tokio runtime. Retry, tracing, and rate-limiting wrap the dispatcher uniformly instead of being re-implemented inside each adapter.

4. **Structured concurrency per turn.** Every in-flight turn owns a `TurnScope` (`src/effect/turn_scope.rs`): one `CancellationToken`, one `JoinSet`. Spawning a task into the scope ties its lifetime to the turn. Cancelling the token signals every child at its next `.await`. Cancelling the scope drops every handle. There is no `task.abort()` anywhere in the codebase; the only way to stop in-flight work is `Cmd::CancelScope(TurnId)`.

## The modules

```
src/
├── domain/      — pure, no async, no I/O.
│   ├── state.rs        State, TurnState, UiState, Session
│   ├── msg.rs          Msg (~25 variants)
│   ├── cmd.rs          Cmd
│   ├── reducer.rs      update(State, Msg)
│   ├── transition.rs   invariant-enforcing helpers
│   └── ids.rs          TurnId, ToolCallId, SubagentId nonces
│
├── effect/     — the only place tokio tasks spawn.
│   ├── mod.rs          EffectRunner + dispatch
│   ├── turn_scope.rs   Per-turn CancellationToken + JoinSet
│   └── middleware.rs   retry_transient_http
│
├── providers/  — the adapter surface.
│   ├── model/          ModelProvider + {Ollama, Anthropic, Gemini, OpenAI-compat}
│   ├── tool/           ToolExecutor + {filesystem, exec, web, mcp}
│   ├── ctx.rs          StreamContext, ExecContext
│   ├── capabilities.rs Capabilities
│   └── factory.rs      ProviderFactory (cache by model_id)
│
├── app/        — the thin outer shell.
│   ├── run.rs          ~40-line main loop
│   ├── event_source.rs crossterm Event → Msg
│   ├── terminal.rs     TerminalGuard (panic-safe teardown)
│   └── recorder.rs     --record / --replay
│
└── render/     — pure view, fn(&State, &mut Frame)
    ├── mod.rs          render() entrypoint
    ├── layout.rs       pure layout math
    ├── chat.rs, input.rs, status.rs, palette.rs, attachments.rs
```

## Type-enforced invariants

The architecture upgrades several conventions into type-level guarantees.

### Stale events are dropped automatically.

Every `Msg` variant carrying effect output embeds a `TurnId`. The reducer's first line on any such message is `if !state.turn.accepts(msg.turn_id()) return;` — mismatches drop silently.

In v0.6, the equivalent was `app.check_interrupt()`, a polling function every long-running op was supposed to call periodically. Forgetting was silent. That's why `web_search` (30-second timeout) and `execute_command` (300-second timeout) both shipped with the same "20-press Ctrl+C" bug. Here, the reducer can't even look at a stale event — filter happens before any state transition.

### Tool results can't go missing.

`TurnState::ExecutingTools::outcomes` is `Vec<Option<ToolOutcome>>`. To transition out of `ExecutingTools` and into the follow-up model call, the reducer must call `try_complete_outcomes`, which flattens `Vec<Option<_>>` → `Vec<_>` only when every slot is `Some(_)`. No code path advances with missing outcomes.

In v0.6, the equivalent was 30 lines of placeholder bookkeeping added after a customer report. Here, the shape of the type makes the bug impossible.

### Cancellation has exactly one entry point.

`Cmd::CancelScope(TurnId)` is the only way to abort work. Grep for `handle.abort()` or `JoinSet::abort_all` in the post-v0.7 codebase; outside of the scope-drop implementation, there are no hits. Every cancellation flows through the reducer → Cmd → runner → token.cancel() path.

### Cancellation is signalled, not polled.

`ExecContext::token` and `StreamContext::token` are `CancellationToken`s threaded into every provider and tool. Adapters race `token.cancelled()` against their main future inside `tokio::select!`. Abort latency is bounded by how long `SIGKILL` takes to arrive, not by whatever polling interval the code happened to choose.

## The message loop

```rust
// src/app/run.rs, condensed.
loop {
    terminal.draw(|f| render(&state, f))?;

    let msg = tokio::select! {
        m = msg_rx.recv()   => m,                    // effect results
        e = events.next()   => event_to_msg(e),      // crossterm
        _ = tick.tick()     => Some(Msg::Tick),      // 60 Hz
    };

    let (new_state, cmds) = update(state, msg);
    state = new_state;
    for cmd in cmds { runner.dispatch(cmd); }

    if state.should_exit { break; }
}
```

That's the whole thing. There is no second runtime. There are no observer callbacks. The render is pure — it takes `&State`, paints into ratatui, mutates nothing.

## Adding a provider

See [`docs/adding_providers.md`](adding_providers.md). tl;dr: implement `ModelProvider` for a struct, register it in `ProviderFactory::build_provider`. One file; no dispatch plumbing to update.

## Adding a tool

See [`docs/adding_tools.md`](adding_tools.md). tl;dr: implement `ToolExecutor` for a unit struct, register it in `ToolRegistry::default` (or a custom registry). One file; cancellation and progress come automatically from `ExecContext`.

## Debugging with --record / --replay

See [`docs/replay_debugging.md`](replay_debugging.md). Event sourcing is nearly free in an MVU architecture; we capture every `Msg` the reducer sees, and a replay is a straight fold.

## What's still old (v0.7.0)

The new path is opt-in via `MERMAID_V7=1`. The v0.6 runtime still runs by default. Every piece of the new architecture is live — domain, effect, providers, render, app::run — but the default binary path hasn't flipped yet. The plan is to flip once v7 has been exercised against real sessions and any parity gaps close.

Gaps the default path still has to fill:
- Subagent dispatch (the `agent` tool) runs through v0.6's code.
- MCP server init happens in v0.6's bootstrap; the `OnceLock` singleton is shared.
- Conversation load modal / /cloud-setup interactive flow / model list modal — reducer has the Msg vocabulary, render layer doesn't paint the full UIs yet.

Those land in follow-up commits before the default flip.
