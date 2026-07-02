# Record and replay sessions

Because the architecture is event-sourced — every reducer input is a single `Msg`, and `update(State, Msg)` is a pure function whose clock is injected as `state.now` — a session snapshots as a flat JSONL file and replays as a straight fold. Reproducing a user-reported bug means folding their log, not guessing at their conversation.

## Quick start

```bash
mermaid --record /tmp/session.jsonl     # live session, every reducer input captured
mermaid --replay /tmp/session.jsonl    # later: reconstruct it, headless
```

`--replay` rebuilds the initial `State` from the recording's header, folds every entry through the pure reducer under the recorded clock, and prints:

```
replay: /tmp/session.jsonl
  recorded: 2026-07-02T08:00:00.500-04:00 · model: ollama/qwen3 · cwd: /home/you/proj
  seed: fresh session
  entries: 41 total · 41 applied · 0 skipped
  note: session quit at line 42; 0 later entries not applied
  determinism: PASS — folding the log twice produced identical state

transcript: fix the login bug — 7 messages
  [user] fix the login bug
  [assistant] Let me look at the auth module.
    (1 tool calls: read_file)
  [tool] ...
```

No model calls, no tool execution, no terminal UI, and no reads of your live config — the log is self-contained. Every replay also folds the log **twice** and compares the resulting states: `determinism: FAIL` (exit 1) means the reducer has grown a purity bug (a wall-clock read, ambient randomness), which is exactly the regression this feature exists to catch.

## On-disk format (version 1)

One JSON object per line. **Line 1 is the session header** — everything replay needs to reconstruct the initial `State`:

```json
{"format":1,"ts":"2026-07-02T08:00:00.500-04:00","model_id":"ollama/qwen3","cwd":"/home/you/proj","config":{...},"seed_conversation":null}
```

- `ts` — the startup clock. Seeds `State::new`'s injected `now`, which derives the initial conversation id and title.
- `config` — full (redacted) `Config` snapshot; MCP seeding and per-model reasoning derive from it.
- `seed_conversation` — the `--continue` / `--sessions` history, when one was loaded.

**Every later line is one reducer input:**

```json
{"ts":"2026-07-02T08:00:01.200-04:00","kind":"SubmitPrompt","turn":null,"msg":{"SubmitPrompt":{"text":"explain main.rs","attachment_ids":[]}}}
```

- `ts` — not informational: it is the exact `state.now` the reducer saw for this input. The driver stamps one clock per tick and shares it between the recording and the reducer; replay stamps `state.now = ts` before folding the entry, so every derived timestamp (turn `started`, message commit times, run summaries) reproduces exactly.
- `kind` / `turn` — denormalized copies of `Msg::kind()` / `Msg::turn_id()` for grepping a log by hand. Replay reads `msg`.
- `msg` — the full `Msg`, serde-serialized (externally tagged). Binary payloads (pasted images, tool artifacts) ride as base64 and replay bit-exactly. New `Msg` variants round-trip automatically — there is no per-variant recording code to keep in sync, and a parity test (`every_msg_kind_has_a_round_trip_sample`) breaks the build if a variant stops round-tripping.

`Recorder::open` appends, so a reused `--record` path holds several sessions back to back (each starting with its own header). `--replay` folds the first and notes where the next begins.

## What replay guarantees — and the two deliberate divergences

Folding the same log always produces the same `State`: the reducer reads time only from `state.now`, mints turn/tool-call ids from in-state counters, and touches no other ambient input. Two things are intentionally *not* preserved, both security-driven:

- **Redaction (#17).** Credential-shaped strings are scrubbed before hitting disk, so a session where a secret crossed the reducer replays the redacted transcript. Replay is deterministic with respect to the log, and identical to the live session whenever no redaction fired.
- **Copied selections.** `Msg::CopySelection` records a placeholder — the text is already in the transcript, can be huge, and the reducer ignores the payload (it only feeds a clipboard `Cmd`).

Entries a build can't reconstruct (a log written by a **newer** mermaid with unknown `Msg` variants) are skipped and reported with their line numbers; the rest of the fold proceeds.

## Programmatic API

```rust
use mermaid_cli::app::{Replay, RecordLine, replay_recording};

// High level: everything --replay does, as a struct.
let report = replay_recording(std::path::Path::new("session.jsonl"))?;
assert!(report.deterministic);
let final_state = report.state;

// Low level: classified line-by-line access.
let (header, lines) = Replay::open("session.jsonl")?;
for line in lines {
    match line? {
        RecordLine::Entry(entry) => {
            let msg = entry.to_msg()?;      // full Msg reconstruction
            // fold: state.now = entry.ts; update(state, msg)
        },
        RecordLine::Header(_) => break,     // next appended session
        RecordLine::Malformed { .. } => {}, // corrupt/truncated line
    }
}
```

## Use cases

- **Regression tests.** Save a JSONL log of any interesting session; fold it in a test and assert on the final `State`. The same log always folds to the same state — see `tests/replay_determinism.rs` for the property pinned as CI.
- **Bug reports.** Ask a user to re-run with `--record` and send the log (remind them it contains their prompts and tool output in cleartext, secrets redacted). `mermaid --replay` shows you the exact state trajectory their session took.
- **Purity canary.** `--replay` exits 1 when the double fold diverges, so replaying any stored log in CI guards the reducer's no-wall-clock/no-randomness invariant.

## Why this is nearly free architecturally

In a traditional TUI architecture, state mutations happen at dozens of call sites, and capturing "what happened this turn" means wiring up logging everywhere. In an MVU architecture the reducer is the only thing that mutates state, so logging its inputs — plus the one injected clock — is enough to reconstruct everything.

One chokepoint. One log. One replay.
