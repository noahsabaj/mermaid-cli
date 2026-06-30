# Mermaid — Hardening Backlog

The first review (**116 findings**, `#1`–`#116`) is fully resolved; its history
lives in [`hardening-archive.md`](hardening-archive.md).

A **second review (2026-06-29)** then surfaced a fresh batch (`#117`–`#142`). Its
**6 root causes plus the non-atomic-mutation pattern were fixed** (RC-1…RC-6 +
atomicity — see the "Second review" section of the archive) and are closed. What
remains open below are the individual findings that were *not* root causes — they
each need their own fix.

- **Status:** root causes resolved · **26 open** (2 HIGH · 11 MED · 13 LOW/INFO)
- **Last updated:** 2026-06-29
- **Severity legend:** `HIGH` exploitable / data-loss · `MED` correctness or
  availability · `LOW`/`INFO` hardening, cosmetics, or by-design risk confirmed.

---

## Open — HIGH

- **#117** `crates/mermaid-runtime/src/checkpoint.rs` — `fresh_checkpoint_id` is
  time-only (`checkpoint-{nanos:x}`, no salt/seq unlike `fresh_id`) and the
  duplicate-PK DB insert is swallowed (`let _ = store.checkpoints().create(...)`);
  two checkpoints in one tick overwrite the prior manifest+files, so a restore
  returns the wrong content. Reuse the collision-hardened `fresh_id` and surface
  the insert error.
- **#118** `crates/mermaid-runtime/src/approval.rs` (`approve_and_replay_with`) —
  the `user_decision.is_none()` pre-check is not atomic with the effect, so two
  concurrent `approve <id>` both run `replay_pending_action` (e.g. `git push`)
  before either records the decision → the effect fires twice. Gate the replay on
  a single-shot `UPDATE … WHERE user_decision IS NULL` *before* running it.

## Open — MED

- **#119** `crates/mermaid-runtime/src/policy.rs` (`decide`) — the `Memory`
  short-circuit precedes the override block, so a `PolicyOverride{Memory, Deny/Ask}`
  is silently ignored; memory writes can only be stopped by `read_only`.
- **#120** `src/bin/mermaidd.rs` — tasks set to `Running` then spawned are never
  reconciled on startup; a daemon crash/stop mid-run leaves them `Running` forever.
- **#121** `src/domain/reducer.rs` (`handle_upstream_error`) — returns the turn to
  `Idle` without draining `ui.queued_messages` (the success/cancel paths do), so a
  message queued during a turn that then errors runs out of order later.
- **#122** `src/models/adapters/ollama.rs` (`think_for_ollama`) — `think` is sent
  for every non-gpt-oss model whenever reasoning ≠ None (default Medium); a
  non-thinking model 400s on every request. Gate on a per-model capability.
- **#123** `src/models/adapters/openai_compat.rs` — `ChatCompletionChunk.choices`
  is required with no mid-stream `error` branch; an OpenRouter `data:{"error":…}`
  or usage-only final frame dies as `ParseError("missing field choices")`, hiding
  the provider's real message (Gemini handles this; OpenAI-compat doesn't).
- **#124** `src/models/adapters/openai_compat.rs` (`build_request_body`) —
  `temperature` is always sent; OpenAI o-series / GPT-5 reject a non-default
  temperature with a 400. Add a per-model knob to omit it.
- **#125** `src/models/adapters/openai_compat.rs` / `gemini.rs` — on a stream with
  no usage frame the adapter returns `Some(TokenUsage::provider(0,0,0))` instead of
  `None`, defeating the reducer's `Some`/`None` guard → the `/context` gauge and
  run-summary reset to "0 tokens" on vLLM/llama.cpp/LMStudio.
- **#126** `src/providers/tool/exec.rs` (`read_capped`) — the on-disk tee log is
  uncapped while the in-memory buffer is capped; a command emitting GBs fills the
  temp dir (retained on Ctrl+B).
- **#127** `src/providers/tool/computer_use/driver.rs` (`run_cmd_cancellable`) — no
  wall-clock timeout on any capture/input backend (scrot/grim/screencapture,
  xdotool/ydotool/wtype); a wedged backend hangs the tool until the user hits Esc,
  even though the geometry/probe helpers nearby do time-box.
- **#128** `src/bin/mermaidd.rs` + `storage.rs` `LIMIT ?1` — a huge attacker-supplied
  `limit` casts to a negative i64; SQLite reads that as unbounded → returns every
  row (auth-gated).
- **#129** `src/session/conversation.rs` — `load_*conversation` does
  `read_to_string` + `from_str` with no size cap; a giant/hostile
  `.mermaid/conversations/*.json` OOMs the process (`--continue` walks every file).

## Open — LOW / INFO

- **#130** `crates/mermaid-runtime/src/storage.rs` + checkpoint dirs — no GC
  anywhere (no `DELETE`/`VACUUM`); tasks, events, tool_runs, messages, sessions and
  checkpoint directories grow forever; `verify_token` is an O(n) scan per auth.
- **#131** `src/bin/mermaidd.rs` — daemon socket startup is TOCTOU (connect-probe →
  `remove_file` → `bind`) with no lockfile; a racing start can hijack the path.
- **#132** `src/app/recorder.rs` + `src/utils/redact.rs` — `--record` persists
  `StreamText` chunks, tool outcomes (e.g. a `read_file` of a private doc) and full
  prompts in cleartext; only credential-*shaped* substrings are scrubbed.
- **#133** `src/domain/runtime.rs` — `RuntimeState.timeline` is an unbounded `Vec`
  the reducer only ever pushes to (and it's `#[serde(default)]`, so it bloats
  snapshots).
- **#134** `src/render/mod.rs` + `widgets/chat.rs` — the full transcript is
  deep-cloned twice per frame and the whole scrollback re-wrapped/re-stringified
  every frame; the markdown cache is `clear()`-ed wholesale past 200 entries and
  thrashes. O(transcript) work per 60 Hz frame.
- **#135** `src/render/mod.rs` (`build_live_messages`) — a `chrono::Local::now()`
  read in the render path the module's own contract says is a pure function of
  `State`; the adjacent elapsed-time code deliberately uses `state.now`.
- **#136** `src/render/markdown.rs` — table column-shrink floors the budget at
  `num_cols*3`, so a many-column table on a narrow terminal still overflows and
  clips, contradicting the "no row overflows the viewport" doc.
- **#137** `src/models/adapters/gemini.rs` — `cachedContentTokenCount` (a subset of
  `promptTokenCount`) is added on top via `with_cached_input`, double-counting the
  input breakdown; `openai_compat.rs` correctly subtracts it. Display-only.
- **#138** `src/models/adapters/anthropic.rs` — `message_stop` `break` exits only
  the inner `drain_sse_events` loop, not the outer stream loop; finalization can
  wait for the body to close on a kept-alive connection.
- **#139** `src/mcp/registry.rs` (`validate_server`) — on an `initialize`/`list_tools`
  error the graceful `client.shutdown()` is skipped (the `?` short-circuits to
  `kill_on_drop`); the documented escalation is bypassed on the error path.
- **#140** `src/render/layout.rs` — dead module (`Zones::for_state`/
  `estimate_input_lines` unreferenced) whose input-height math and `state.status`
  banner branch diverge from the live inline logic — a stale second source of truth.
- **#141** `src/providers/auto_classifier.rs` (`looks_like_injection`) — INFO /
  by-design: the injection denylist is a small fixed substring set; near-synonyms
  pass, leaving the fenced prompt + fail-safe parse as the real backstop. The
  small default classifier model is a realistic injection target. Defense-in-depth,
  not a boundary.
- **#142** `crates/mermaid-runtime/src/pathguard.rs` — INFO / by-design: on
  non-Linux or pre-5.6 kernels the confined open/write fall back to a
  canonicalize-then-by-path operation with a check→use TOCTOU window; Linux
  `openat2(RESOLVE_BENEATH)` closes it. Documented residual.
