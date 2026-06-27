# Mermaid — Hardening Backlog

Derived from the architectural + security review that produced **116 findings**
(`#1`–`#116`) grouped under 7 root causes. The root causes and two sweep-ins
were fixed across the Axis hardening PRs (GitHub PRs #64–#68 — not to be
confused with findings #64/#68 below); the four partial residuals plus the
fail-open verdict parse were closed afterward. This file tracks what's left.

- **Last updated:** 2026-06-27
- **Status:** 58 resolved · **58 remaining** · 0 CRITICAL left · 1 HIGH left
- **Severity legend:** `HIGH` exploitable / data-loss · `MED` correctness or
  availability · `LOW`/`INFO` hardening, cosmetics, or by-design risk to confirm.

Remaining work is split into six groups by subsystem, so each is a single
coherent review surface (one PR). Suggested order is by risk — see the end.

---

## Remaining (58)

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

### Group 2 — MCP client robustness & safety (8 · 1 HIGH, 2 MED, 5 LOW)
`mcp/transport`, `mcp/registry`, `mcp/add`, `tool/mcp`

- [ ] **#10** `HIGH` `mcp/registry.rs:270-434` — `mermaid add <unknown>` speculatively runs guessed packages via `npx -y` (typosquat RCE). *Previously deferred.*
- [ ] **#36** `MED` `mcp/transport.rs:97` — one invalid-UTF-8 byte permanently kills the reader task.
- [ ] **#37** `MED` `mcp/transport.rs:168-231` — no timeout on stdin write/flush → whole-server hang.
- [ ] **#89** `LOW` `mcp/transport.rs:110` — response dispatch matches `id` only (server-initiated request can collide).
- [ ] **#91** `LOW` `tool/mcp.rs:106` — `isError:true` reported to the model as `Success`.
- [ ] **#92** `LOW` `mcp/transport.rs:263` — `start_kill()` is SIGKILL, not the documented SIGTERM.
- [ ] **#93** `LOW` `mcp/add.rs` + `server_manager.rs:31` — MCP secret env & command args echoed/logged. *Deferred from Cause 7; redaction chokepoint doesn't cover MCP setup.*
- [ ] **#94** `LOW` `mcp/transport.rs` — in-flight requests not failed on stdout EOF (full 30 s wait).

### Group 3 — Computer-use (6 · 2 MED, 4 LOW)
`providers/tool/computer_use/*`

- [ ] **#32** `MED` `computer_use/driver.rs:184` — `scale_coords` can overflow `i32` (wrapped-negative click / debug panic).
- [ ] **#35** `MED` `computer_use/mod.rs:72` — macOS registers 7 tools but 6 always `bail!` (no `cliclick`).
- [ ] **#96** `LOW` `computer_use/click.rs`, `mouse_move.rs` — coordinates truncated, never clamped to display size.
- [ ] **#97** `LOW` `computer_use/driver.rs` — geometry/probe subprocess has no `kill_on_drop`/timeout.
- [ ] **#98** `LOW` `computer_use/{click,type_text,press_key}.rs` — implicit post-action auto-screenshot is ungated.
- [ ] **#100** `LOW` `computer_use/driver.rs:598` — macOS focused-capture offset (0,0) latent mis-click.

### Group 4 — Daemon, persistence & storage (8 · 8 LOW)
`bin/mermaidd`, `runtime/storage`, `runtime/approval`, `runtime/client`

- [ ] **#61** `LOW` `runtime/storage.rs:1882` — wall-nanos IDs can collide → `ON CONFLICT` overwrites unrelated rows.
- [ ] **#62** `LOW` `runtime/approval.rs:17` — `approve_and_replay` not transactional (crash mid-replay → stuck "approved").
- [ ] **#63** `LOW` `runtime/client.rs:929` — `restart_process`/`open_process` exec command + URL from DB rows.
- [ ] **#64** `LOW` `runtime/storage.rs:1550` — token expiry compared as an RFC3339 string.
- [ ] **#65** `LOW` `bin/mermaidd.rs:496` — `pair ttl_days<=0` mints a never-expiring token (defeats 30-day TTL).
- [ ] **#66** `LOW` `bin/mermaidd.rs:26` — socket/dir perms best-effort (`let _ =`), no `SO_PEERCRED`.
- [ ] **#67** `LOW` `bin/mermaidd.rs:468` — ungated `plugin_preview` can trigger a git fetch (env-gated).
- [ ] **#68** `LOW` `session/conversation.rs:230` — `--continue` hard-fails on a corrupt newest file (list tolerates it).

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

1. **Group 2 — MCP** — the lone HIGH (#10) plus two whole-server-hang bugs (#36/#37).
2. **Group 4 — Daemon/persistence** — token & exec-from-DB surface (#65, #63, #62).
3. **Group 3 — Computer-use** — the `i32` overflow (#32) and ungated capture (#98).
4. **Group 1 — Providers** — large but low-risk correctness; mostly mechanical.
5. **Group 5 — MVU core** — the deferred purity residuals; some carry a real behavioral tradeoff (#45, #18).
6. **Group 6 — App shell** — mostly cosmetic, latent, or by-design-to-confirm.

**Cheapest high-value picks across groups:** #65 (never-expiring token), #72
(message-ordering 400), #36/#37 (MCP reader robustness), #93 (extend redaction
to MCP logging).

## Deliberate deferrals (conscious, not misses)

- **#18, #45, #54, #55** — reducer/render *I/O-and-env* residuals left after the
  time-injection core landed. Determinism for `(State, Msg) → State` holds;
  these concern render purity / turn-freshness tradeoffs.
- **#93** — the redaction chokepoint scrubs the recorder, memory, and compaction
  summaries; it does **not** yet cover MCP server-setup logging or `add.rs`
  config storage.
- **#10** — deferred once already; highest single residual risk.

---

## Resolved (58)

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
