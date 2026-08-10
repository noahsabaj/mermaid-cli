# Design: one session event log

Status: accepted. Tracks redesign item #8 — the last of the three structural items
(after the streaming driver #6 and the Engine extraction #7 in the same ledger).
This document is the contract for the PR sequence at the bottom; each PR cites the
section it implements.

## Problem

Mermaid persists "what happened in this session" five different ways, each with its
own schema, its own security hardening, and its own failure modes:

1. **Conversation snapshot** — `.mermaid/conversations/<id>.json` (+ `<id>.meta`
   sidecar). A full `ConversationHistory` rewritten atomically on nearly every
   message: ~28 reducer sites emit `Cmd::SaveConversation(session.snapshot_conversation())`,
   and the effect layer serializes the whole transcript again each time. Carries its
   own redaction pass, screenshot strip, 0600 mode, 64 MiB read cap, id-traversal
   validation, and optimistic-concurrency `.conflict` siblings (F73).
2. **Recorder trace** — `--record <path>` JSONL of every reducer *input* (`Msg`),
   with a config-snapshot header and a fingerprint trailer. Opt-in, interactive-only,
   deliberately unstable schema. Its own redaction pass and 0600 handling (#17, #132).
3. **RunEvent stream** — `mermaid run --format ndjson` stdout and the daemon's
   per-task broadcast. Frozen v1 wire contract, golden-tested. Zero persistence: the
   broadcast channel is dropped once the terminal status lands.
4. **Runtime SQLite** — `runtime.sqlite3` (schema v6). The *runtime* tables (tasks,
   task_events, tool_runs, approvals, processes, checkpoints, compactions, outcomes)
   are live and correct. The *session content* tables are not — see the findings.
5. **Compaction archives** — `.mermaid/compactions/<conversation>/<archive>.json`,
   the only durable copy of compacted-out messages, plus a bookkeeping row in the
   SQLite `compactions` table stitched to it by a path string.

Five schemas describe overlapping content (a committed `ChatMessage` exists in
representations 1, 4, and 5; session identity in all five), so every cross-cutting
fix — secret redaction, screenshot stripping, owner-only file modes, size caps —
had to be discovered and landed per store, and several were.

### Findings the design must answer for (surveyed 2026-08-10)

- **The `sessions` and `messages` tables have no production writers.** The only
  callers of `SessionsRepo::upsert` and `MessagesRepo::add` are storage tests. Yet
  the daemon snapshot and dashboard list `sessions` (always empty), the
  `session_messages` endpoint reads `messages` (always "not found"), and the task
  detail view joins both. Three shipped read paths serve permanently-empty data.
- **`tasks.conversation_id` is never backfilled.** It is set only at task creation,
  where the conversation does not exist yet, so it is `NULL` for every daemon task.
  `run_non_interactive_with` returns the real `session_id` in its `RunResult` and
  the daemon drops it on the floor.
- **A late `subscribe_task` attach loses the session.** For an already-terminal
  task the daemon synthesizes `RunEvent::Result { response: final_report,
  total_tokens: 0, session_id: "" }` — the one field a follow-up
  `mermaid run --resume` needs is the one that is blank.
- **Nothing ever reads a compaction archive.** The only durable copy of
  compacted-out messages is write-only cold storage with no load path in the tree.
- **Headless and daemon runs have no history at all beyond the snapshot.** The
  recorder is wired only into the interactive driver, and the RunEvent stream
  evaporates, so a daemon task's mid-run life is unreconstructable after the fact.

## Invariants (unchanged by this design)

- Existing `conversations/*.json` files keep loading forever; the resume/list/fork
  paths keep their legacy-field tolerance.
- `--record` / `--replay` byte-exactness: recording format v1 and the replay fold
  are untouched.
- The `RunEvent` v1 wire contract stays frozen (golden tests).
- Reducer purity: `mermaid-domain` stays serde-only; anything the log needs from
  the reducer rides out on a `Cmd` payload as data.
- Security properties hold at a single chokepoint per store: credential redaction,
  screenshot stripping, 0600 modes, size caps, id-traversal validation.
- The flaky-drive posture: storage errors surface as warnings and self-heal on the
  next operation; they never crash the session.

## Design

### The log

One append-only JSONL file per session, co-located with the snapshot it explains:

```
.mermaid/conversations/<session-id>.jsonl
```

Co-location is deliberate: every listing path filters on the `json` extension, so
`.jsonl` files are invisible to the pickers with zero code change, and
`delete_conversation`'s cascade extends by one line.

Each line is an envelope around a typed event:

```json
{"v":1,"seq":17,"ts":"2026-08-10T12:00:00.000-04:00","event":{"type":"message","message":{...}}}
```

- `v` — `SESSION_EVENT_FORMAT_VERSION`, bumped only on an incompatible envelope
  change. Readers refuse newer versions (same posture as the recorder and the DB).
- `seq` — per-file monotonic counter, owned by the appender. Truncation detection
  and a resume cursor for late attach.
- `ts` — appender wall clock. Transport metadata, not content: content timestamps
  live inside the events (a `ChatMessage` already carries its own).

`SessionEvent` lives in `mermaid-domain` next to `run_event.rs`, under the same
purity rule (serde-only, no I/O) and the same golden-test discipline: internally
tagged `type`, `snake_case`, one pinned wire sample per variant.

### Granularity: the committed transcript, not reducer inputs

The recorder proves a `Msg`-granularity log can reconstruct everything — and also
why it must stay opt-in: it captures every keystroke and stream chunk, costs
megabytes per hour, and its schema must stay free to grow with `Msg`. An always-on
session store wants the opposite properties: small, stable, and content-shaped.
So the log records what the session *committed*, and the recorder remains the
opt-in diagnostics instrument for reducer inputs. Four of the five representations
unify onto the log; the recorder is explicitly out of scope, kept for what it is.

Variants (initial set):

- `started` — session identity: id, project path, model, created_at, and lineage
  (`forked_from` / `parent_session`) when known at birth. First line of every log.
- `message { message: ChatMessage }` — one committed transcript entry. Subsumes the
  snapshot's `messages`, the dead `messages` table, and the archives (see below).
- `compaction { record: CompactionEvent, replacement: Vec<ChatMessage> }` — a
  compaction replaced the model-visible transcript with `replacement`. The
  *archived* originals are not copied anywhere: they are the earlier `message`
  lines in this same file. Append-only storage makes the archive a boundary
  marker instead of a second copy — this is what deletes representation 5.
- `reset { messages: Vec<ChatMessage> }` — a wholesale transcript replacement that
  is not a compaction (safety valve; enumerated callers of `replace_messages` /
  `set_messages` decide between `compaction` and `reset` in PR B).
- `state { ... }` — the scalar session state as one small struct: title, model,
  safety mode, plan state, advertised context, token meters, context usage, and
  provenance (`git_branch`, `git_sha`, `cli_version`). Emitted only when it
  differs from the last emitted value. One coarse variant instead of ten fine
  ones: the struct is tiny next to a message, and a fold assigns rather than
  patches.
- `input { text }` — one prompt-history entry.
- `tasks { store: ChecklistStore }` — checklist snapshot (already snapshot-shaped).

A pure `fold_session(events) -> ConversationHistory` lives beside the type and
replays events through the same mutators the live session uses (`add_messages`,
`replace_messages`, `add_compaction`, `add_to_input_history`), so title
derivation and `updated_at` stamping cannot drift from the live behavior.

**The completeness guard is a test, not a checklist**: drive a session through the
reducer, collect the emitted events, and assert `fold_session(events)` equals
`snapshot_conversation()` (revision excluded — it is `serde(skip)`). Any mutation
that forgets to emit turns that test red. The test must be written to fail first
(delete one emission, watch it go red) before it counts as a guard.

### How events leave the pure core

`Session` gains a `#[serde(skip)] pending_events: Vec<SessionEvent>` buffer, filled
by the same mutation chokepoints that already guard the render memo
(`messages_mut` bumps `revision`; the event buffer rides the same narrow waist).
`Cmd::SaveConversation` changes payload from `ConversationHistory` to
`{ snapshot: ConversationHistory, events: Vec<SessionEvent> }` — the reducer's ~28
emission sites drain the buffer into the command they already emit. `Cmd` is never
serialized (recordings hold `Msg` only), so the payload change has no wire or
replay impact; `Cmd::tag()`/`summary()` strings stay byte-identical for traces.

### The writer

The effect layer's existing persistence chain (the FIFO that already orders
archive-before-snapshot) gains one step: **append the events, then rewrite the
snapshot**. The append chokepoint owns, in one place, exactly what the snapshot
writer owns today: session-id validation, credential redaction (`redact_json` per
line), screenshot stripping on `message`/`compaction`/`reset` events, 0600 create
mode, and a read cap analogous to the 64 MiB conversation cap. Append failures
surface like snapshot failures do (warn, retry on the next save) and re-open the
file handle on error, mirroring `with_shared_store`'s self-healing for the
flaky-drive case.

**Backfill on first append**: when the log file does not exist but the snapshot
carries messages (a session created before this feature, resumed after upgrade),
the writer seeds the log with `started` + one `message` per existing snapshot
message + one `state`, then appends the new events. One O(transcript) write, once
per pre-upgrade session, and every log is total from its first line.

Both drivers (interactive `run.rs` and headless `run_non_interactive`) go through
the same `EffectRunner`, so daemon tasks get a durable content log with no extra
wiring — this is what fixes the "daemon runs have no history" finding.

### Authority: snapshot stays authoritative; the log is history + recovery

Resume, `--continue`, and the pickers keep reading the snapshot, unchanged. The
log adds:

- **Corruption recovery**: a session whose `.json` is missing or unparseable but
  whose `.jsonl` folds cleanly is recoverable instead of skipped.
- **History**: the full pre-compaction transcript, queryable after the fact.
- **The daemon views** (next section).

Flipping authority — fold-first resume with the snapshot demoted to a throttled
cache (killing the O(transcript) rewrite per message) — is the natural follow-up,
deliberately out of scope until fold-equals-snapshot has soaked in CI and a
release. The design makes the flip a policy change, not a rewrite.

### The runtime index: feed `sessions`, drop `messages`

The `sessions` table becomes what it was meant to be — the cross-project index —
by feeding it at the one place that knows the facts: after a successful snapshot
write, the persistence chain upserts `(id, project_path, model_id, title,
conversation_path, total_tokens)` through `with_shared_store`. One indexed upsert
per save, the same pattern as process upserts.

The `messages` table is deleted (schema v7 — the F76 migration loop's first real
non-additive step). Its two readers repoint at the truth:

- `session_messages` resolves the session row → `conversation_path` → reads the
  snapshot (or folds the log) and serves those messages.
- The task detail view does the same through the task's `conversation_id`.

And the dead key comes alive: after `run_non_interactive_with` returns, the daemon
stamps `RunResult.session_id` onto the task row (`tasks.conversation_id`) alongside
the terminal status. `mermaid task <id>` then joins to a real session.

### The daemon: attach stops lying, late or mid-run

`subscribe_task` on an already-terminal task currently synthesizes an empty-handed
`Result`. With the backlink and the log in place it reads the task row →
`conversation_id` → session content, and emits a real terminal event: the actual
final response (the continuation-join logic in `build_result` extracts to a
helper over `&[ChatMessage]`), the real `session_id`, and the persisted token
total.

**The mid-run attach (shipped after the sequence, #378).** The follow-up this
section deferred: every attach now replays what it missed before it joins the
broadcast. `RunEvent::catch_up` projects the log's committed events onto the
same frozen wire — one `text` line per committed assistant message instead of
the deltas that produced it, tool pairs off the same run metadata the live
projection reads, the newest checklist last. Three things the deferral had not
worked out, now settled:

- **The backlink has to exist mid-run.** `tasks.conversation_id` was stamped at
  terminal status, so during the run there was no key from the task to its log.
  It is now stamped when the run *announces* its session — a small watcher on
  the task's own broadcast, waiting for `session_started`. The end-of-run write
  stays as the authority; this is the same value, earlier. `mermaid task <id>`
  shows the session mid-run as a side effect.
- **Assigning events clear the replay.** `compaction` / `reset` drop everything
  projected before them, exactly as `fold_session` does, so the catch-up
  describes the transcript as it stands. That rule is also what keeps a
  replacement message's inline actions from replaying tool calls a second time.
- **Subscribe first, read second.** The receiver attaches before the log is
  read, so a message committed during the read is replayed *and* delivered
  live. The overlap is bounded to that one in-flight message, and it is the
  right way round: repetition is recoverable by a consumer, a hole is not. The
  ack's `replayed` count says how many of the following lines are replay.

Identity is stated from the task row rather than the log's `started` event: the
log describes the SESSION (which for a resumed one predates this run), and only
the row knows the task the stream belongs to.

### Compaction: the archive is a boundary, not a copy

`Cmd::SaveCompactionArchive` stops producing `.mermaid/compactions/**` files. The
`compaction` event carries the record and the replacement; the archived originals
are already in the log. The SQLite `compactions` bookkeeping row stays (it is
runtime index, and the dashboard reads it) with `archive_path` now recording the
log path. Existing archive files are left untouched on disk — nothing reads them
today, and deleting user data is not this design's job. The ordering barrier the
effect chain enforces (archive-before-snapshot) becomes append-before-snapshot,
preserving the same crash property: the only durable copy of dropped messages
lands before the stripped snapshot can overwrite the old one.

## Compatibility and rollback

- **Old → new**: no log file means backfill-on-first-append; everything else is
  additive. Pre-upgrade sessions resume exactly as before.
- **New → old** (rollback): an older mermaid ignores `.jsonl` files entirely and
  keeps operating on snapshots, which remained authoritative. The v7 `messages`
  drop is safe in both directions because the table was empty in production.
- **Skew**: a log written by a newer format version is refused by readers the same
  way the recorder and DB refuse newer versions; the snapshot still loads.

## PR sequence

Each PR is one commit, shipped serially under the standing flow (branch-dispatched
CI, verdict read from the output file, then merge).

- **PR A — this document.**
- **PR B — the schema, pure.** `SessionEvent` + envelope + golden wire tests +
  `fold_session` + `Session::pending_events` + the `Cmd::SaveConversation` payload
  change + emissions at every mutation chokepoint (enumerating `replace_messages`
  / `set_messages` callers into `compaction` vs `reset`). The fold-equals-snapshot
  test lands here, proven red first. No I/O.
- **PR C — the writer.** Append chokepoint in `src/session` (validation,
  redaction, stripping, 0600, cap, self-healing reopen), persistence-chain wiring
  (append before snapshot), backfill-on-first-append, and the `sessions` upsert.
  Tests: append+fold equals loaded snapshot; corrupt-snapshot recovery via fold;
  backfill idempotence.
- **PR D — compaction on the log.** `SaveCompactionArchive` appends instead of
  writing archive files; `compactions.archive_path` points at the log;
  `qa_compact_smoke` and `reducer_flows` updated to the new shape.
- **PR E — the daemon reads the truth.** Task→conversation backlink at terminal
  status; `subscribe_task` terminal synthesis from session content;
  `session_messages` and task detail served from `conversation_path`; dashboard
  `sessions` now real (fed since PR C).
- **PR F — schema v7.** Drop the `messages` table and `MessagesRepo`; remove the
  dead reads. The v7 arm in `migrate_within_txn` is the first genuinely
  non-additive step, exercising the F76 structure as designed.

## What the sequence learned (all six shipped: #367-372)

Three things this plan did not anticipate, recorded because they are now
load-bearing:

- **Creating a log needs the batch's *assigning* events.** PR C's rule — a
  backfill subsumes the batch, so drop it — is right only for ADDITIVE events
  (`message`, `action`, …), whose effects the snapshot already contains.
  Assigning events (`compaction`, `reset`, `state`, `tasks`) restate what the
  backfill says and carry a fact the transcript cannot; dropping those lost the
  boundary whenever a compaction happened to create the log. The split is an
  exhaustive match, so a new variant has to choose a side.
- **The compaction append is not best-effort.** Once PR D removed the archive
  file, the append became the only record that the dropped messages existed, so
  it runs with `?` ahead of the snapshot rewrite — inheriting exactly the
  ordering guarantee the archive-first rule used to provide.
- **F24/RC-F had to move with the read it guarded.** The daemon's transcript cap
  lived in the SQL query PR F deleted; it now bounds `transcript_rows`
  tail-first. Dropping the table without carrying the cap forward would have
  quietly removed an OOM guard.

## Rejected alternatives

- **`Msg`-granularity always-on log** (make the recorder the store): captures
  keystrokes and stream deltas by default — a privacy and volume regression the
  recorder's opt-in design exists to avoid; and it would freeze `Msg`'s schema,
  which record/replay deliberately keeps loose.
- **Snapshot diffing in the effect layer** (derive events by comparing consecutive
  snapshots): avoids touching the reducer but guesses at intent — a compaction and
  a hand-edit are indistinguishable from the outside, and `replace_messages` makes
  index-based diffs wrong. The reducer knows what happened; it should say so.
- **SQLite as the one store** (write events as rows instead of JSONL): couples
  every session save to the shared DB and the flaky-drive failure domain, breaks
  the project-scoped portability of `.mermaid/` (a repo's sessions travel with the
  checkout), and makes the security chokepoint two-headed. The DB stays what it
  is: the cross-project runtime index.
- **Fold-first resume in this sequence**: flipping authority before the
  fold-equality guard has soaked trades a working resume path for an unproven one.
  The flip is cheap later; a bad resume is expensive now.
