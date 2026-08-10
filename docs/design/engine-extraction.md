# Design: one Engine

Status: accepted. Tracks redesign item #7, the last structural item in the ledger
(the streaming driver #6 and the event log #8 are the other two; #8 shipped as
PRs #367-378). This document is the contract for the PR sequence at the bottom;
each PR cites the section it implements.

## Problem

`update(State, Msg) -> (State, Vec<Cmd>)` is the whole product. Driving it is
five lines:

```rust
state.now = clock;                       // inject the wall clock as data
let (new_state, cmds) = update(state, msg);
state = new_state;
for cmd in cmds { runner.dispatch(cmd); }
if state.should_exit { break }
```

Those five lines are written out longhand in **six** places, each with its own
spelling of the surrounding loop:

| # | site | inbox | stops when | observer |
|---|---|---|---|---|
| 1 | `app/run.rs:233` | msgs + crossterm + signals + 16ms tick | `should_exit` | recorder JSONL |
| 2 | `app/run_non_interactive.rs:334` (`drive_to_idle`) | msgs + signals | idle & queue drained, or cancel grace | `RunEvent` stdout + broadcast |
| 3 | `providers/tool/subagent.rs:1169` (`drive_child`) | msgs | idle & queue drained, or deadline | `ChildProgress` to parent |
| 4 | `app/replay.rs:146` (`fold`) | a `Vec` read off disk | end of log | none |
| 5 | `subagent.rs:793` | child progress vs. the drive future | drive completes | forwards progress |
| 6 | `subagent.rs:928` (detached) | child progress vs. the drive future | drive completes | `Msg::BackgroundAgent*` |

Sites 5 and 6 do not call `update` themselves — they are the *second* loop each
child needs, because `drive_child` owns the reducer and cannot also relay
progress. Two loops per subagent is the tell: the driving is not a thing you can
hold, so anything that wants to watch it has to wrap it.

What that costs, concretely:

- **Fixes land once per copy.** #76 (a timed-out child dropped its runner
  mid-flight and leaked MCP children) was fixed in `drive_child` by moving the
  deadline into the `select!`. Site 2 still wraps its drive in
  `timeout(deadline, ...)` and returns `Err` through `?` **before**
  `runner.shutdown().await` — the identical leak, unfixed, because it is a
  different function.
- **Cancellation means two different things.** Site 2 injects `Msg::CancelTurn`
  and gives the turn a 15s grace to unwind; site 3 breaks out immediately. Both
  are defensible, neither is written down, and no caller can choose.
- **Nothing can attach.** A driving loop that is a `loop {}` inside an `async fn`
  has no handle. `subscribe_task` (#378) can replay the event log to a late
  attach precisely because the log is a value; the *live* reducer is not. Daemon
  attach, a second view of one session, and any SDK surface all want the same
  thing: `send(msg)` and `subscribe()`.

## Shape

One type owns the reducer state and the effect sink, and exposes the loop
around them as data rather than as control flow:

```rust
pub struct Engine<S: EffectSink, O: StepObserver = ()> { state, sink, observer }

impl Engine {
    fn  reduce(&mut self, now, msg) -> StepOutcome;   // the pure kernel; sync
    async fn step(&mut self, msg) -> StepOutcome;     // observe, then reduce
    async fn drive(&mut self, inbox, policy) -> DriveExit;
}
```

Three seams, one per axis the six sites actually differ on:

- **`EffectSink`** — where a `Cmd` goes. `EffectRunner` for a live run;
  `DropEffects` for `--replay`, whose log already holds each effect's result as
  a later `Msg`. Also the interception point: the interactive loop owns
  `Cmd::ComposeInEditor` (it suspends the terminal and the event stream, which
  only that loop holds), so its sink peels that variant off and forwards the
  rest.
- **`StepObserver`** — what watches each message *before* `update` consumes it.
  The recorder, the `RunEvent` projection, and `ChildProgress` all need the
  pre-update state, and one of them needs to `.await` a channel send, so the
  hook is `async`.
- **`DrivePolicy`** — `StopWhen` (`Exit` for interactive, `Settled` for
  headless), an optional `CancellationToken` with an explicit `OnCancel`
  (`Abort` or `Unwind { grace }` — the two behaviours sites 2 and 3 already
  have, now named), and an optional wall-clock `deadline` as a `select!` arm so
  the caller keeps its state and its shutdown path on timeout.

`reduce` is deliberately sync and observer-free: `--replay` folds a log with no
tokio runtime in sight, and keeping the kernel callable from a plain `for` loop
is what proves the abstraction did not smuggle in a runtime.

### Why the state is owned, not borrowed

Every site today threads `State` through by value and hands it back at the end
(`drive_child` returns `(Result<String, DriveError>, State)` for exactly this
reason — the caller needs the final usage totals on *every* exit path, including
timeout). Ownership moves into `Engine`, so `drive` returns a verdict and the
caller reads `engine.state()` after any outcome. The `(result, state)` tuple and
its "returned on EVERY exit path" comment stop being a rule to remember.

Internally the field is `Option<State>` — only because `update` consumes `State`
by value, which is the pure reducer's signature and not up for negotiation.
`reduce` takes it out and puts the new one back; it is `Some` at every
observable point.

### The actor

`Engine` as written above is *owned* — the caller holds it and calls `step`. The
interactive TUI must stay that way: it renders `&State` at 60fps in the same
task, and an actor would either clone `State` per frame or per message.

The actor is the second surface, over the same core:

```rust
let handle = EngineActor::spawn(engine, policy);   // owns the Engine in a task
handle.send(msg).await;                            // -> the engine's inbox
let mut rx = handle.subscribe();                   // -> broadcast<EngineEvent>
```

`EngineEvent` carries *projections* (the `RunEvent` vocabulary, already frozen
at v1), not `State`: a subscriber that is a daemon socket, a second TUI, or an
SDK client cannot use a `State` it cannot render, and broadcasting one would put
a deep clone on the hot streaming path. Subscribers that need history get it the
way `subscribe_task` already does — catch up from the session event log, then
join the broadcast.

This is what daemon attach and multi-session need, and it is deliberately *not*
what the TUI uses.

## Invariants

- `update` keeps its signature and its purity. Nothing in this design gives the
  reducer a clock, an inbox, or an `.await`.
- `--replay` stays deterministic and runtime-free: same fold, same double-fold
  determinism check, same fingerprint comparison.
- The `RunEvent` wire stays v1. The projection moves behind an observer; the
  bytes do not change.
- No behaviour change to cancellation semantics at either headless site: `Abort`
  and `Unwind { grace: 15s }` reproduce what sites 3 and 2 do today,
  respectively.

## PR sequence

1. **The engine and every drive loop.** `src/engine/`: `EffectSink`,
   `StepObserver`, `Engine::{reduce, step, drive}`, `DrivePolicy`, `Inbox`.
   Converts sites 1-4: `--replay` (via `DropEffects`), `drive_to_idle` (closing
   #76 there by moving its deadline into the policy), `drive_child`, and the
   interactive loop — which keeps its `select!` but calls `engine.step`, with
   the recorder as an observer and `ComposeInEditor` peeled off by a sink.
   Sites 5 and 6 keep their relay loops: they are about the *child handle*, not
   the reducer, and collapse in PR 2. *(this PR)*
2. **The actor.** `EngineActor::spawn` + `EngineHandle::{send, subscribe}`,
   `EngineEvent`. The subagent's two relay loops become one `subscribe()`, and
   daemon attach / multi-session / an SDK surface get the handle they need.
