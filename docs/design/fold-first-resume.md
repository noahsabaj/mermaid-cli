# Design: fold-first resume

Status: accepted. The follow-up `docs/design/event-log.md` scoped out and named
as the natural destination — read that first; this note assumes its vocabulary
(`SessionEvent`, `fold_session`, the per-session `.jsonl`).

## Problem

The event-log sequence (#367-372) left two files describing every session and a
test asserting they agree:

- `.mermaid/conversations/<id>.json` — the snapshot, **authoritative**, rewritten
  in full by 24 reducer emission sites, i.e. after nearly every message.
- `.mermaid/conversations/<id>.jsonl` — the log, append-only, holding the same
  history plus what compaction removed.

Two consequences follow from that split, and they are the whole reason for this
change:

1. **An invariant held by a test rather than by construction.** `fold(events)`
   and the snapshot are two independent renderings of one truth. They agree
   because `fold_matches_snapshot` says so. Nothing structural prevents drift.
2. **Cost that grows with the session.** Appending one message rewrites the
   entire transcript. A 500-message session serializes 500 messages to record
   the 501st. The log already appends in constant time; the snapshot is what
   makes a save O(transcript).

Flipping authority fixes both: the log becomes the single source of truth, and
the snapshot becomes what it has actually been all along — a materialized view
kept for fast startup.

## The shape

Three files per session, with distinct and honest jobs:

| File | Role | Written |
| --- | --- | --- |
| `<id>.jsonl` | **Truth.** Append-only `SessionEvent` log. | Every save, O(1) |
| `<id>.meta` | Index for the picker: id, title, updated_at, counts. | Every save, tiny |
| `<id>.json` | **Checkpoint.** A fold materialized at a known point. | Every ~200 events, and on clean exit |

The `.meta` sidecar moves to the append path. That matters more than it looks:
with checkpoints written every ~200 events, a short session has no checkpoint at
all until it exits, so the picker cannot depend on one. Log + meta is the pair
that always exists; the checkpoint is purely a startup accelerator.

### The watermark

A checkpoint is only useful if you know *which* events it already contains. The
writer stamps that as a top-level `checkpoint_seq` key in the checkpoint file —
**not** a field on `ConversationHistory`. The domain type describes a
conversation; which log offset a cache was built at is storage's business, and
`serde` ignores the unknown key on the way back in, so the value round-trips
through the reducer untouched.

### Resume

```
load(id):
  log      = read <id>.jsonl          (the truth)
  ckpt     = read <id>.json + its checkpoint_seq

  if no log:                          -> ckpt as-is        (legacy session)
  if ckpt and seq valid for this log: -> ckpt + replay events after seq
  otherwise:                          -> fold the log from zero
```

"Valid for this log" means the watermark is present and does not exceed what the
log actually holds. Every other case — no checkpoint, no watermark, a truncated
or replaced log, an unparseable checkpoint — falls to the full fold. That is the
property worth the whole exercise: **the checkpoint is never load-bearing.**
Delete it, corrupt it, hand it a log from a different session, and resume is
still correct; it is only slower.

`--continue` picks the newest session by log mtime and then goes through the
same path, so there is exactly one resume algorithm.

## Checkpoint cadence

Write when ~200 events have accumulated since the last one, plus on clean exit.

The thing being bounded is **replay length**, not elapsed time: a checkpoint's
whole job is to cap how much log a resume has to fold. Counting events bounds
that directly. Turn boundaries do not — one long agentic turn can produce
hundreds of events with no pause — and a time debounce ties freshness to the
clock rather than to the work.

Write volume drops from "whole transcript, every message" to "whole transcript,
every ~200 events" — and the events themselves were already being written.

## Concurrent writers

A daemon run and an interactive session can hold the same session id; the
runtime store is deliberately multi-process. With the snapshot authoritative,
F73 caught that through an `(mtime, len)` baseline and diverted the loser to a
`.conflict` sibling. Append-only storage does not get that for free: two
processes appending interleave silently, and a fold would replay one session's
turns spliced into another's.

So F73 moves to the log, in the same shape and for the same reason. The appender
records the file length after each of its own writes; before the next append it
stats the file, and a length that does not match means another writer got in.
This process then stops appending to the shared log and diverts to a
`.conflict` sibling with a warning. `stat` is O(1) — counting lines would
reintroduce exactly the per-append cost this design exists to remove.

## What this raises the stakes on

Under snapshot authority, a transcript mutation that forgets to emit its event
makes `fold_matches_snapshot` fail in CI. Under fold-first, that same omission
is **latent data loss** — the checkpoint has the message, the log does not, and
whichever resume path folds from zero drops it.

The mitigations are the ones already built, now load-bearing rather than
merely tidy: mutations go through `Session`'s chokepoints, the event match is
exhaustive so a new variant cannot be quietly ignored, and `fold == snapshot` is
asserted per-mutator and across a real reducer flow. This design adds one more,
because a guard that only runs in CI does not observe real sessions:

- **A drift check in `mermaid doctor`**: fold the log from zero, compare against
  checkpoint-plus-replay, and report any difference. The same comparison the
  test makes, pointed at whatever is actually on disk.

## Migration

Nothing to migrate. A session with a log resumes by folding it; a session with
only a snapshot (written before the log existed) keeps loading from that
snapshot, and gets a log the first time it saves — the backfill from #369
already does this. Rollback is equally quiet: an older mermaid ignores
`.jsonl`, reads the checkpoint as a plain snapshot, and loses only the events
after the last checkpoint.

## PR sequence

- **PR G — this note.**
- **PR H — resume reads the log.** `replay_events` made public in the domain;
  the seq-aware log reader; `checkpoint_seq` stamped and honored; the resume
  algorithm above in `load_conversation` and `load_last_conversation`; `.meta`
  written at append time; the picker and `--continue` discover sessions by log.
  Snapshot writes stay on their current cadence, so this PR changes only where
  the truth is read from.
- **PR I — the checkpoint stops being hot.** Throttle to ~200 events plus a
  flush at shutdown. This is the PR that deletes the O(transcript) per-message
  cost, and it is deliberately separate so that if resume is wrong, it is wrong
  while the snapshot is still fresh.
- **PR J — F73 moves to the log.** Length-baseline conflict detection on append,
  `.conflict` siblings, and the retirement of the snapshot-side guard.
- **PR K — the drift check.** `mermaid doctor` folds from zero and diffs.

## Rejected alternatives

- **Pure log, no checkpoint.** One file per session, folded from zero every
  time. Conceptually cleanest — no cache, no watermark, no coherence question —
  but a long session's log holds every message ever, including everything
  compaction removed, so startup would pay for history the session no longer
  shows. The checkpoint is the standard answer to exactly that (WAL plus
  checkpoints, AOF plus RDB), and keeping it disposable preserves the property
  that motivated the flip.
- **Keeping the snapshot authoritative and merely throttling its writes.** Gets
  the same write-volume win, and resume would still read snapshot-then-tail — so
  it looks nearly identical in code. It differs in what a bad snapshot means: a
  wrong session versus a cache to discard. Same mechanics, opposite failure
  semantics, and only one of them lets you delete the file.
- **An exclusive lock per session log.** Stronger than detect-and-diverge —
  interleaving becomes impossible rather than merely caught — but it adds a
  blocking failure mode (a crashed process leaving a stale lock) and forbids a
  daemon/interactive overlap that works today.
