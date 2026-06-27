# Record and replay sessions

Because the v0.7 architecture is event-sourced — every reducer input is a single `Msg` — you can snapshot a session as a flat sequence of messages and replay it later. Deterministic regression tests become small JSONL files, and reproducing a user-reported bug often means reading their replay log rather than their conversation.

## On-disk format

One JSON object per line (JSONL). Each object shape:

```json
{
  "ts": "2026-04-21T12:34:56.789-04:00",
  "kind": "SubmitPrompt",
  "turn": null,
  "body": {"text": "explain main.rs", "attachment_ids": []}
}
```

- `ts` — RFC3339 timestamp. Not just informational: it is the wall clock that
  was injected as `state.now` for this step (Cause 3). A replay driver stamps
  `state.now = ts` before folding the entry through `update`, so the reducer —
  which reads `state.now` instead of `Local::now()`/`SystemTime::now()` —
  reproduces every turn timestamp (`started`, `since`, message `timestamp`)
  exactly. That is what makes the fold deterministic rather than approximate.
- `kind` — `MsgKind` variant (see `src/domain/msg.rs`).
- `turn` — embedded `TurnId` for effect-result messages; `null` for user-intent and housekeeping.
- `body` — best-effort structured payload from `record_msg_body`. Variants that carry raw binary or runtime-only data are marked with `"recordable": false` and include compact metadata instead of the full payload.

Runtime lifecycle events are recorded as normal reducer inputs. For example, an external SIGTERM records `{"kind":"RuntimeSignal","body":{"signal":"terminate"}}`, which lets debugging distinguish a clean user quit from a process-manager shutdown.

## Recording

Pass `--record <file>` at startup to write a JSONL log of every reducer input:

```bash
mermaid --record /tmp/session.jsonl
```

Library callers can also construct a `Recorder` directly:

```rust
use mermaid_cli::app::{Recorder, record_msg_body};
let mut recorder = Recorder::open("session.jsonl")?;
// Inside the main loop, before each update():
recorder.record_kind(msg.kind(), msg.turn_id(), record_msg_body(&msg))?;
```

## Replaying

```rust
use mermaid_cli::app::Replay;
let replay = Replay::open("session.jsonl")?;
for entry in replay {
    let entry = entry?;
    // Inject the recorded clock so timestamps reproduce exactly (Cause 3):
    state.now = entry.ts.parse()?;
    // Reconstruct a Msg from entry.kind + entry.body, feed to update().
}
```

The reconstruction step is explicit (and hand-coded by variant today) because `Msg` doesn't currently round-trip through serde. Replay logs are structured and useful for debugging, but variants marked `"recordable": false` are observability records rather than deterministic replay inputs.

## Use cases that land for free

- **Regression tests.** Save a JSONL log of any interesting session. A future commit that changes reducer behavior can be tested against the log: fold over the Msg stream (injecting each entry's `ts` as `state.now`), assert the final `State` equals the known-good snapshot. Because the reducer reads only its inputs and `state.now`, the same log always folds to the same `State`.
- **Bug reports.** When a user reports weird behavior, ask them to run `mermaid --record /tmp/session.jsonl` and send you the log. You can replay it locally against your build.
- **Fuzz-style property testing.** Generate random `Msg` sequences, fold over them, assert invariants (every committed assistant message has a matching user, every `ExecutingTools` eventually resolves or cancels, etc.).

## Why this is nearly free architecturally

In a traditional TUI architecture, state mutations happen at dozens of call sites, and capturing "what happened this turn" means wiring up logging everywhere. In an MVU architecture the reducer is the only thing that mutates state, so logging its inputs is enough to reconstruct everything.

One chokepoint. One log. One replay.
