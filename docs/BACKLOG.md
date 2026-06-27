# Mermaid — Hardening Backlog

Derived from the architectural + security review that produced **116 findings**
(`#1`–`#116`) grouped under 7 root causes. The root causes and two sweep-ins
were fixed across the Axis hardening PRs (GitHub PRs #64–#68 — not to be
confused with findings #64/#68 below); the four partial residuals plus the
fail-open verdict parse were closed afterward, then **Group 2 (MCP client)** and
**Group 4 (daemon, persistence & storage)** were completed. This file tracks
what's left.

- **Last updated:** 2026-06-27
- **Status:** 74 resolved · **42 remaining** · 0 CRITICAL left · 0 HIGH left
- **Severity legend:** `HIGH` exploitable / data-loss · `MED` correctness or
  availability · `LOW`/`INFO` hardening, cosmetics, or by-design risk to confirm.

Remaining work is split into four groups by subsystem, so each is a single
coherent review surface (one PR). Suggested order is by risk — see the end.

---

## Remaining (42)

### Group 1 — Provider adapters, retry & auth (13 · 3 MED, 10 LOW)
`models/adapters/*`, `effect/middleware`, `providers/factory`

- [ ] **#12** `MED` `models/adapters/openai_compat.rs:464` — streaming path drops cached-input & reasoning tokens.
- [ ] **#13** `MED` `models/adapters/ollama.rs:258` — Ollama never surfaces a stop reason (`done_reason` unparsed).
- [ ] **#42** `MED` `effect/middleware.rs:103` — retry backoff doesn't race the cancel token (seconds of cancel latency).
- [ ] **#49** `LOW` `models/adapters/ollama.rs:241` — non-saturating token `+` (overflow on untrusted totals).
- [ ] **#51** `LOW` `models/adapters/gemini.rs` — `FINISH_REASON_UNSPECIFIED` errors only on the non-stream path.
- [ ] **#52** `LOW` `utils/sse.rs:50` — empty `data:` keep-alive frame tears down the stream.
- [ ] **#53** `LOW` `models/adapters/anthropic.rs:166` — legacy thinking budget ≥ `max_tokens` → guaranteed 400.
- [ ] **#83** `LOW` `providers/factory.rs:70` — un-normalized cache key; key/config frozen until restart.
- [ ] **#84** `LOW` `providers/factory.rs` — concurrent first-build double-builds the provider.
- [ ] **#85** `LOW` `…/retry.rs:36` — retries every error class, including non-idempotent web POSTs.
- [ ] **#86** `LOW` `models/adapters/ollama.rs:610` — forces cleartext `http://`; host classifier exists but isn't wired here.
- [ ] **#87** `LOW` retry jitter entropy from `subsec_nanos()`.
- [ ] **#88** `LOW` `ollama/cloud_setup.rs` — Ollama cloud key stored plaintext in `config.toml`.

### Group 3 — Computer-use (6 · 2 MED, 4 LOW)
`providers/tool/computer_use/*`

- [ ] **#32** `MED` `computer_use/driver.rs:184` — `scale_coords` can overflow `i32` (wrapped-negative click / debug panic).
- [ ] **#35** `MED` `computer_use/mod.rs:72` — macOS registers 7 tools but 6 always `bail!` (no `cliclick`).
- [ ] **#96** `LOW` `computer_use/click.rs`, `mouse_move.rs` — coordinates truncated, never clamped to display size.
- [ ] **#97** `LOW` `computer_use/driver.rs` — geometry/probe subprocess has no `kill_on_drop`/timeout.
- [ ] **#98** `LOW` `computer_use/{click,type_text,press_key}.rs` — implicit post-action auto-screenshot is ungated.
- [ ] **#100** `LOW` `computer_use/driver.rs:598` — macOS focused-capture offset (0,0) latent mis-click.

### Group 5 — MVU core: reducer/render purity, compaction, turn lifecycle (12 · 2 MED, 10 LOW)
`domain/reducer`, `domain/compaction`, `render/widgets/*`

- [ ] **#18** `MED` `app/run.rs:186-241` — clipboard `Cmd` + selection mutation in the event-loop arm, outside `update()` (unrecorded; render not pure). *Cause-3 deferral.*
- [ ] **#45** `MED` `domain/reducer.rs` — synchronous `instructions::refresh`/`memory::refresh` fs I/O in the reducer on the main-loop thread. *Cause-3 deferral (async `Cmd` changes turn-freshness semantics).*
- [ ] **#54** `LOW` `domain/reducer.rs:1016/2454` — `std::env::temp_dir()` read inside the reducer.
- [ ] **#55** `LOW` `render/widgets/status.rs:39` — `StatusWidget::render` reads `HOSTNAME`/`USER` every frame.
- [ ] **#71** `LOW` `domain/compaction.rs:459` — forwards a pre-existing orphan `tool_use` unpaired → provider 400.
- [ ] **#72** `LOW` `domain/reducer.rs:2297` — `⚠ truncated` note inserted between `tool_calls` and results → possible 400.
- [ ] **#73** `LOW` `domain/reducer.rs:2187` — `/compact` completion doesn't drain `queued_messages`.
- [ ] **#74** `LOW` `domain/reducer.rs:236` — `ApprovalRequested` during `Cancelling` can outlive its turn.
- [ ] **#101** `LOW` `render/widgets/chat.rs:952` — diff-row background fill uses char count, not display cells (CJK).
- [ ] **#104** `LOW` `render/widgets/chat.rs:535` — user-msg timestamp alignment from byte lengths (CJK/emoji).
- [ ] **#102** `LOW` `render/widgets/conversation_list.rs:96` — `[..16]` not char-boundary safe (latent).
- [ ] **#103** `LOW` `render/widgets/slash_palette.rs:84` — slice trusts an unclamped `selected_index` (latent).

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

1. **Group 3 — Computer-use** — the `i32` overflow (#32) and ungated capture (#98).
2. **Group 1 — Providers** — large but low-risk correctness; mostly mechanical.
3. **Group 5 — MVU core** — the deferred purity residuals; some carry a real behavioral tradeoff (#45, #18).
4. **Group 6 — App shell** — mostly cosmetic, latent, or by-design-to-confirm.

**Cheapest high-value picks across groups:** #32 (`i32` overflow → bad click /
debug panic), #72 (message-ordering 400), #111 (silently-swallowed malformed
config).

## Deliberate deferrals (conscious, not misses)

- **#18, #45, #54, #55** — reducer/render *I/O-and-env* residuals left after the
  time-injection core landed. Determinism for `(State, Msg) → State` holds;
  these concern render purity / turn-freshness tradeoffs.

---

## Resolved (74)

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
