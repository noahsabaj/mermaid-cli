# Mermaid — Hardening Archive (Completed)

Documented history of the architectural + security review that produced **116
findings** (`#1`–`#116`) grouped under 7 root causes, and how every one was
resolved. **All 116 are closed.** A **second review (2026-06-29)** then added
`#117`–`#142`; its **6 root causes plus the non-atomic-mutation pattern are
resolved** and recorded in the next section, while the remaining non-root-cause
items stay open in [`BACKLOG.md`](BACKLOG.md).

The root causes and two sweep-ins were fixed across the Axis hardening PRs
(GitHub PRs #64–#68 — not to be confused with findings #64/#68 below); the four
partial residuals plus the fail-open verdict parse were closed afterward, then
six subsystem groups were completed in turn: **Group 2 (MCP client)**, **Group 4
(daemon, persistence & storage)**, **Group 3 (computer-use)**, **Group 1
(provider adapters, retry & auth)**, **Group 5 (MVU core)**, and **Group 6 (app
shell)**.

- **Findings:** 116 · **Resolved:** 116 · **Open:** 0
- **Severity legend:** `HIGH` exploitable / data-loss · `MED` correctness or
  availability · `LOW`/`INFO` hardening, cosmetics, or by-design risk confirmed.

---

## Second review (2026-06-29) — root-cause hardening (`#117`–`#142`)

A follow-up architecture + security review (multi-agent, adversarially verified)
surfaced 26 findings. The **6 root causes plus the non-atomic-mutation pattern**
were fixed in this batch and are recorded here; the individual non-root-cause
findings remain in [`BACKLOG.md`](BACKLOG.md). Each fix shipped with regression
tests; `cargo test`, `clippy -D warnings`, and `rustfmt` are clean.

**RC-1 — Unix process termination was decentralized; only the Esc path killed the
process group.** A single `terminate_tree(pid, Grace)` primitive
(`src/utils/proc.rs`; Unix signals the group `-pid` *and* the bare pid, SIGTERM →
400 ms → SIGKILL, or immediate SIGKILL; Windows `taskkill /T /F`) now backs every
termination site. The foreground **timeout no longer leaks the process tree**: it
moved *inside* `run_command`'s `select!` as a `TimedOut` arm that tree-kills then
aborts the driver (it previously only built an error string and dropped a detached
task that kept the child alive — `exec.rs`). `/stop` and `/restart` group-kill via
`terminate_tree_blocking` and now spawn managed processes as group leaders
(`process_group(0)`), and Ctrl+B background launches use `setsid`, so the group
kill reaches grandchildren (`client.rs`, `exec.rs`).

**RC-2 — shell risk was classified by argv0 alone, ignoring write/exec-bearing
arguments** (`policy.rs`). `find` left the read-only set and is now
argument-aware (`-exec`/`-execdir`/`-ok`/`-okdir` ⇒ Process; `-delete`/`-fprint*`/
`-fls` ⇒ ShellMutation); `sort -o`/`--output` ⇒ ShellMutation; `config`/`branch`/
`tag` were removed from `GIT_READ_ONLY` (they mutate). These no longer auto-run in
`read_only`/`auto` — closing the worst finding, where `find . -exec …` /
`find / -delete` ran in read_only and auto-ran with no classifier or checkpoint.

**RC-3 — the destructive-root hard-deny was exact-string matching over a
pre-lowercased command** (`policy.rs` `is_dangerous_root`). It now normalizes a
trailing `/*`, `/.`, `/` and `${VAR}`→`$VAR` before matching and drops the dead
uppercase `$HOME`/`${HOME}` arms, so `rm -rf ${HOME}`, `/etc/`, `/usr/*` are
caught. Deliberately exact-match-after-normalization (not prefix-match) so a
legitimate `rm -rf /home/<user>/…/node_modules` isn't pulled into the
un-overridable deny. The fork-bomb check was generalized from the literal `:`
name to any `name(){ … name|name& … }` (`is_fork_bomb`).

**RC-4 — the gate/classifier only vetted the arguments `action_detail` formatted**
(`policy_gate.rs`, `auto_classifier.rs`). `action_detail` now surfaces the real
`web_search` queries and the subagent `prompt`, so the classifier and the human
approval modal see the actual content (was just a label → silent
exfiltration-via-query). The classifier's no-command/path fallback is now fenced
in `BEGIN/END UNTRUSTED ACTION` markers, and the injection pre-filter scans the
`summary` too — closing the unfenced/unfiltered subagent-description path.

**RC-5 — the schema was re-applied inline on every open with a dead version
guard** (`storage.rs`). `init_schema` now reads `user_version` *first* and bails
when it exceeds `SCHEMA_VERSION` (the guard was dead — the version was clobbered
before the check, so an older binary silently operated a newer DB); creation +
migration run inside `BEGIN IMMEDIATE`/`COMMIT` (serializing the daemon/CLI
`ensure_column` race), `ensure_column` is duplicate-column tolerant, and the
version is stamped only after a successful migration. The paired `tasks().create`
(task + event) and `update_status` (update + event) writes are now atomic
transactions.

**RC-6 — effect-output messages without a `TurnId` bypassed the stale filter**
(`reducer.rs`, `msg.rs`, `effect/mod.rs`). `ProviderContextResolved` now carries
and is guarded by `model_id` (mirroring `OllamaPlacementResolved`), so a context
probe that lands after a `/model` switch no longer overwrites the new model's
window.

**Atomicity — the agent wrote the user's files non-atomically while protecting its
own state.** A confined atomic writer `write_atomic_beneath` (`pathguard.rs`:
`openat2(RESOLVE_BENEATH)` temp → fsync → `renameat` within the same parent fd,
preserving the destination's mode) now backs `write_file` and `edit_file`
(`filesystem.rs`) — a crash/kill/disk-full mid-write leaves the prior file intact
instead of truncated, and the `#77` symlink confinement is preserved.
`restore_checkpoint` (`checkpoint.rs`) stages writes-before-deletes with per-file
atomic writes and best-effort rollback. (DB write atomicity is RC-5.)

### Second-review individual findings (`#117`–`#142`)

Beyond the root causes above, the 26 individual findings were resolved (each with
regression tests; build / `clippy -D warnings` / `rustfmt` clean; validated
end-to-end headless against `ollama/minimax-m3:cloud`):

- **Providers** — #122 Ollama `think` is gated on a cached `/api/show` thinking
  capability probe (omit when unsupported; preserve + retry on probe failure);
  #123 openai-compat tolerates a usage-only final chunk (`choices` defaulted) and
  surfaces a mid-stream `{"error"}` frame as a typed error; #124 temperature is
  omitted for o-series/gpt-5; #125 a stream with no usage frame returns `None`
  (preserving the char-estimate) instead of zeros; #137 Gemini stops
  double-counting cached input tokens; #138 Anthropic `message_stop` breaks the
  outer stream loop.
- **Daemon / persistence** — #117 checkpoint ids use the collision-hardened
  `fresh_id` and the insert error is propagated (no silent disk/DB divergence);
  #118 approvals are claimed atomically (`UPDATE … WHERE user_decision IS NULL`)
  before the un-rollback-able effect, released on error, recovered on restart;
  #120 a startup reconcile resets tasks stranded `Running` and stale claims; #128
  query limits are clamped (no negative-LIMIT wrap); #129 conversation loads are
  size-capped; #130 a startup GC prunes archived/old-terminal rows + orphaned
  checkpoint dirs (never active data); #131 a daemon-lifetime advisory `flock`
  closes the socket-startup TOCTOU.
- **Safety / recorder** — #119 a `PolicyOverride` on the Memory category now
  applies (override block moved above the memory short-circuit, still below the
  destructive hard-deny); #132 recordings are written `0600` with a one-time
  cleartext warning; #141 the injection pre-filter normalizes text and covers more
  reviewer-directed markers.
- **Render** — #135 `build_live_messages` uses `state.now` (purity restored) and
  returns `Cow` (no idle-frame transcript clone); #136 wide tables shrink to fit a
  narrow viewport; #140 the dead `layout.rs` was removed; #134 the per-frame
  double-clone and markdown-cache thrash were eliminated, and the `ChatWidget`
  cache now holds the fully wrapped (not just parsed) assistant lines so a
  committed message is never re-parsed or re-wrapped per frame — proven
  output-identical by a cache-hit-vs-miss buffer test.
- **Exec / computer-use / MCP** — #126 the on-disk tee log is capped at 64 MiB;
  #127 computer-use backends are wall-clock bounded; #139 MCP validation runs the
  graceful shutdown on the error path.
- **Domain** — #121 a provider error drains the queued-message FIFO (no
  out-of-order replay); #133 `RuntimeState.timeline` is bounded to 200 events.
- **#142** (non-Linux fallback TOCTOU) is by design — `openat2(RESOLVE_BENEATH)`
  closes it on Linux — and now emits a one-time operator warning when the fallback
  is used.

---

## Resolved by group

**Group 6 — App shell: CLI, config, subagent, filesystem tools (11 · 10 LOW, 1 INFO):**
#75 (the `agent` tool's `MAX_DEPTH`/`SUBAGENT_DEPTH` machinery was dead — a
`tokio::task_local` that doesn't survive the tool-dispatch spawn boundary, and
`build_child_registry` already omits the `agent` tool so subagents can't nest at
all; removed the dead gate — the registry exclusion is the real, working guard),
#76 (a timed-out subagent now shuts its child `EffectRunner` down: the timeout
moved inside `drive_child` so the single unconditional `shutdown()` runs on every
exit path, instead of the runner being dropped and leaking its MCP children),
#78 (`read_file`'s `truncated` flag comes from the bounded read, not a sniff of
the output for the marker string — a file whose own content contained that text
used to be falsely flagged), #79 (the `read_file` schema no longer claims "in
parallel"; the reads are sequential by design), #108 (the instruction-file walk
checks the `$HOME` boundary *before* searching, so `~/AGENTS.md` is no longer
loaded; the walk is now injectable so the rule is unit-tested without mutating
the process env), #109 (a single `AGENTS.md`/`MERMAID.md` is wrapped in a
`# Project Instructions:` header like the multi-file and memory paths, so it
reaches the system prompt as clearly-bounded project data rather than unlabeled
trusted-system text), #110 (`mermaid update` confirms before downloading +
running the install script — fail-closed in a non-interactive session, `--force`
bypasses), #111 (a malformed config is no longer silently swallowed — startup +
user-facing reads warn via `load_config_or_warn`, and the config-mutating
`persist_*` / `mcp add` / safety-mode paths propagate the error instead of
clobbering the file with defaults), #112 (the main event loop's `tokio::select!`
dropped `biased;` so sustained streaming can't starve the input/signal/tick
arms), #113 (`mermaid restore` confirms before overwriting the working tree —
fail-closed non-interactive, `--force` bypasses), #116 (the QA harness test root
is env-overridable via `MERMAID_QA_TEST_ENV` with a generic default — no
developer-specific hardcoded path). A shared `utils::confirm` gate now backs the
#110/#113 confirmations and the MCP untrusted-package prompt (#10), replacing the
duplicated y/N + fail-closed logic.

**Group 5 — MVU core: reducer/render purity, compaction, turn lifecycle (12 · 2 MED, 10 LOW):**
#45 (the reducer no longer does the synchronous `MERMAID.md`/memory refresh — a
background **config watcher** in the effect layer polls and emits
`Msg::InstructionsChanged`/`MemoryChanged`, so `update()` is genuinely I/O-free
and a `--record` log replays without re-statting the live filesystem; edits also
land faster, before the next submit — the "both" design chosen over the
sync-exception and async-lag alternatives), #18 (the clipboard copy routes
through a new `Msg::CopySelection` → `Cmd::CopyToClipboard`, so the side effect is
recorded/replayable instead of dispatched out-of-band; the text selection stays
render-layer state), #54 (`temp_dir` injected once into `State` at startup, like
`cwd` — the reducer reads it instead of calling `std::env::temp_dir()`), #55
(`StatusWidget` takes host/user as props read once into `RenderCache`, not from
the environment every frame). Message-sequence + turn lifecycle: #71 (compaction
strips a pre-existing orphan `tool_use` from the preserved tail so it can't 400
the next request), #72 (the `⚠ truncated`/content-filter note is suppressed when
tool calls are pending, so it never lands between `tool_calls` and their
results), #73 (manual `/compact` completion drains `queued_messages`,
auto-submitting a message typed during compaction), #74 (`ApprovalRequested` is
dropped when the turn is already `Cancelling`, so a modal can't outlive its
turn). Unicode/display: #101 (diff-row background fills by display cells via a
`pad_to_cells` helper, not char count), #104 (user-message timestamp alignment
measures display width, not bytes), #102 (`conversation_list` truncates on a
`floor_char_boundary`, never mid-`char`), #103 (`slash_palette` clamps
`selected_index` before slicing).

**Group 1 — Provider adapters, retry & auth (13 · 3 MED, 10 LOW):**
#12 (the OpenAI-compat streaming path now carries cached-input + reasoning tokens
through the same `token_usage_from_wire` converter the non-stream path uses, instead
of dropping them), #13 (Ollama maps the response `done_reason` to a `FinishReason`
on both the streaming and non-streaming paths instead of hardcoding `None`), #49
(Ollama's streaming token total uses `saturating_add`, matching the non-stream path —
no overflow on untrusted counts), #51 (Gemini treats an empty
`FINISH_REASON_UNSPECIFIED` response identically on both paths via one shared
predicate), #53 (the Anthropic legacy thinking budget returns `None` when
`max_tokens <= 1024` rather than emitting `budget >= max_tokens`, a guaranteed 400),
#52 (the SSE reader skips an empty `data:` keep-alive frame instead of emitting `""`
and tearing the stream down), #83 (the provider cache key is normalized — provider
segment lowercased — so `Anthropic/x` and `anthropic/x` share one cached instance),
#84 (the cache uses a per-key `OnceCell` so concurrent first-callers build the
provider exactly once, without holding the lock across the build), #85
(`retry_async_if` skips terminal errors — the Ollama web search/fetch POSTs no longer
retry 4xx, only network failures / 5xx / 429), #87 (retry jitter draws real entropy
from `getrandom` instead of `subsec_nanos()`, via one impl shared by the retry and
middleware layers), #86 (the Ollama adapter picks the URL scheme by host class —
loopback/LAN stay http, public hosts default to https — instead of forcing cleartext),
#88 (the Ollama cloud key is read from `OLLAMA_API_KEY` only and never written to
`config.toml`; the field and its on-disk read-back cache were removed entirely). #42
was already satisfied by the existing invariant: the model wrappers `select!` the
whole `chat` future against `ctx.token.cancelled()`, so a cancel drops the in-flight
retry-backoff sleep — documented rather than re-plumbed through the `Model` trait.

**Group 3 — Computer-use (6 · 2 MED, 4 LOW):**
#32 (`scale_coords` translation is now saturating — the `+ offset` i32 add no
longer panics in debug or wraps to a negative click in release), #35 (the registry
advertises only the tools a backend can drive — macOS gets just `screenshot`,
Wayland drops `list_windows` — instead of offering input verbs that `bail!` at
call time), #96 (model-supplied click/move coords are clamped to the screenshot's
own pixel bounds at the `scale_coords` chokepoint), #97 (the geometry/probe helpers
and the downscale encoders run under `kill_on_drop` + a `tokio::time::timeout`, so
a wedged xdotool/xrandr/convert can't hang the agent loop), #98 (the implicit
post-action auto-screenshot is gated behind a new `computer_use.auto_screenshot`
config flag, default on, and de-duplicated into one helper), #100 (macOS focused
capture now grabs the full display so the reported `(0,0)` offset is genuinely
correct — avoiding an AppleScript points-vs-device-pixels Retina hazard).

**Group 4 — Daemon, persistence & storage (8 · 8 LOW):**
#61 (collision-free `fresh_id` — a per-process random salt plus a monotonic
counter make same-nanosecond ID collisions impossible, so the `ON CONFLICT`
upserts can't silently overwrite an unrelated row), #62 (`approve_and_replay`
runs the un-rollback-able replay effect *before* the "approved" mark, so a crash
leaves the approval re-runnable instead of stuck "approved but never applied"),
#63 (`restart_process` refuses a DB command flagged destructive; `open_process`
and the TUI open path validate the DB target — only `http(s)` URLs or existing
files reach the OS opener), #64 (token expiry compared as a parsed instant,
failing closed — not an RFC3339 string compare), #65 (the daemon `pair` command
clamps a client-supplied `ttl_days <= 0` to the 30-day default so a socket caller
can't mint a never-expiring token; the local CLI's owner-only opt-out is
unchanged), #66 (socket/data-dir chmod failures are fatal at the daemon boundary,
and the Unix accept loop rejects any peer whose uid isn't the socket owner or
root — TCP still relies on token auth), #67 (`plugin_preview` now requires the
pairing token, like `plugin_install`, so an unauthenticated caller can't trigger
a git fetch), #68 (`--continue` tolerates a corrupt newest conversation file,
falling back to the newest valid one like the session picker).

**Group 2 — MCP client robustness & safety (8 · 1 HIGH, 2 MED, 5 LOW):**
#10 (typosquat RCE — convention/search now probe the npm registry for package
*existence* instead of executing `npx -y <guess>`; a default-NO confirmation
gate, fail-closed when non-interactive, with a `--yes` opt-in, guards any
non-registry package), #36 (reader skips a non-UTF-8 frame instead of dying),
#37 (stdin write/flush — and `send_notification` — are timeout-bounded), #89
(only true JSON-RPC responses complete a pending request; server-initiated
requests no longer collide), #91 (`isError:true` maps to a tool `Error`, not
`Success`), #92 (real SIGTERM before SIGKILL on Unix; docstring corrected), #93
(MCP server args + stderr + spawn-error context run through the redaction
chokepoint), #94 (pending requests fail fast on stdout EOF instead of waiting
the 30 s timeout).

**Earlier — partial residuals + fail-open parse (5):**
#7 (Auto-classifier prompt-injection hardening), #23 (`parse_verdict`
fail-open), #77 (fd-based confined file ops), #99 (screenshots no longer
persisted), #114 (tokenized destructive pre-check).

**Resolved by correcting the docs/invariant (3):**
#56 (`driver.abort()` retained — safe because the process tree is killed first;
the "no `abort()` anywhere" claim was corrected), #57, #115.

**Behaviorally fixed across the Axis hardening PRs (GitHub PRs #64–#68) (50):**
#1, #2, #3, #4, #5, #6, #8, #9, #11, #14, #15, #16, #17, #19, #20, #21, #22, #24,
#25, #26, #27, #28, #29, #30, #31, #33, #34, #38, #39, #40, #41, #43, #44, #46,
#47, #48, #50, #58, #59, #60, #69, #70, #80, #81, #82, #90, #95, #105, #106, #107.

The one CRITICAL (#1, shell-chaining classifier bypass) and 8 of the 10 HIGH
findings were fixed in that pass.
