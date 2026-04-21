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

- `ts` — RFC3339 timestamp; informational only.
- `kind` — `MsgKind` variant (see `src/domain/msg.rs`).
- `turn` — embedded `TurnId` for effect-result messages; `null` for user-intent and housekeeping.
- `body` — serde-serialized payload. Variants that carry raw binary data (paste images) are skipped on record.

## Recording

Today this ships as an opt-in during development. The public CLI flag (`--record <file>`) lands in a follow-up. To capture while iterating:

```rust
use mermaid_cli::app::Recorder;
let mut recorder = Recorder::open("session.jsonl")?;
// Inside the main loop, after each update():
recorder.record_kind(msg.kind(), msg.turn_id(), serde_json::to_value(&msg).unwrap_or_default())?;
```

## Replaying

```rust
use mermaid_cli::app::Replay;
let replay = Replay::open("session.jsonl")?;
for entry in replay {
    let entry = entry?;
    // Reconstruct a Msg from entry.kind + entry.body, feed to update().
}
```

The reconstruction step is explicit (and hand-coded by variant today) because `Msg` doesn't currently round-trip through serde — a few variants carry non-serializable fields. A full `impl Deserialize for Msg` lands when every in-flight variant is covered.

## Use cases that land for free

- **Regression tests.** Save a JSONL log of any interesting session. A future commit that changes reducer behavior can be tested against the log: fold over the Msg stream, assert the final `State` equals the known-good snapshot.
- **Bug reports.** When a user reports weird behavior, ask them to `MERMAID_V7_RECORD=/tmp/session.jsonl mermaid` next time and send you the log. You can replay it locally against your build.
- **Fuzz-style property testing.** Generate random `Msg` sequences, fold over them, assert invariants (every committed assistant message has a matching user, every `ExecutingTools` eventually resolves or cancels, etc.).

## Why this is nearly free architecturally

In a traditional TUI architecture, state mutations happen at dozens of call sites, and capturing "what happened this turn" means wiring up logging everywhere. In an MVU architecture the reducer is the only thing that mutates state, so logging its inputs is enough to reconstruct everything.

One chokepoint. One log. One replay.
