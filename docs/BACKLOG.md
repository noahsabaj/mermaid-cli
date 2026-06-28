# Mermaid — Hardening Backlog

Derived from the architectural + security review that produced **116 findings**
(`#1`–`#116`) grouped under 7 root causes. The root causes and two sweep-ins
were fixed across the Axis hardening PRs (GitHub PRs #64–#68 — not to be
confused with findings #64/#68 below); the four partial residuals plus the
fail-open verdict parse were closed afterward, then **Group 2 (MCP client)**,
**Group 4 (daemon, persistence & storage)**, **Group 3 (computer-use)**,
**Group 1 (provider adapters, retry & auth)**, and **Group 5 (MVU core)** were
completed. This file tracks what's left.

- **Last updated:** 2026-06-28
- **Status:** 105 resolved · **11 remaining** · 0 CRITICAL left · 0 HIGH left
- **Severity legend:** `HIGH` exploitable / data-loss · `MED` correctness or
  availability · `LOW`/`INFO` hardening, cosmetics, or by-design risk to confirm.

Remaining work is one group (Group 6 — app shell): a single coherent review
surface (one PR). See the end for the cheapest high-value picks.

---

## Remaining (11)

### Group 6 — App shell: CLI, config, subagent, filesystem tools (11 · 11 LOW)
`app/*`, `commands`, `instructions`, `providers/tool/{subagent,filesystem}`

- [ ] **#75** `LOW` `providers/tool/subagent.rs:436` — depth-3 nesting unreachable (dead `MAX_DEPTH` gate).
- [ ] **#76** `LOW` `providers/tool/subagent.rs:231` — timeout drops child via `Drop`, not graceful `shutdown()`.
- [ ] **#78** `LOW` `providers/tool/filesystem.rs:120` — false `truncated` flag when file content contains the marker.
- [ ] **#79** `LOW` `providers/tool/filesystem.rs:83` — `read_file` advertised "in parallel" but reads sequentially.
- [ ] **#108** `LOW` `app/instructions.rs:115` — `~/AGENTS.md` loaded despite the "don't search home" comment.
- [ ] **#109** `LOW` `domain/reducer.rs:2605` + `config.rs` — `AGENTS.md`/`MERMAID.md` content into the system prompt (unsandboxed, by-design).
- [ ] **#110** `LOW` `commands.rs:1602` — `mermaid update` runs a fetched install script with no checksum/confirm.
- [ ] **#111** `LOW` `main.rs:25` / `commands.rs:1672` — `load_config().unwrap_or_default()` silently swallows a malformed config.
- [ ] **#112** `LOW` `app/run.rs:148` — `biased;` select can starve input/signal arms under sustained streaming.
- [ ] **#113** `LOW` `commands.rs:1286` — `mermaid restore` overwrites the working tree with no confirmation.
- [ ] **#116** `INFO` `docs/qa.md` — hardcodes a developer-specific path.

---

## Suggested order (by risk)

1. **Group 6 — App shell** — the last group: mostly cosmetic, latent, or by-design-to-confirm.

**Cheapest high-value picks:** #111 (silently-swallowed malformed config), #110
(`mermaid update` runs a fetched script with no checksum/confirm), #113 (`mermaid
restore` overwrites the working tree with no confirmation).

---

## Resolved (105)

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

**This session — partial residuals + fail-open parse (5):**
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
