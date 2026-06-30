# Mermaid — Hardening Backlog

Both review batches are resolved; the full history lives in
[`hardening-archive.md`](hardening-archive.md).

- First review (`#1`–`#116`): resolved.
- Second review (`#117`–`#142`): resolved — the 6 root causes + the
  non-atomic-mutation pattern, **and** every individual finding. See the archive's
  "Second review" section. Fixes shipped with regression tests; `cargo test`,
  `clippy -D warnings`, and `rustfmt` are clean, and the build was validated
  end-to-end headless against `ollama/minimax-m3:cloud` (read-only, and an
  edit-then-run-tests agentic task).

- **Status:** 0 open defects · 2 documented residuals (both by design, below)
- **Last updated:** 2026-06-29

---

## Resolved since

- **#134 — render-cache optimization (done).** The `ChatWidget` cache now stores
  the fully **wrapped**, role-prefixed assistant lines (keyed on
  `(content, theme, width)`), not just the markdown parse — so a committed message
  is cloned from cache, never re-parsed or re-wrapped, each frame. This was the
  remaining per-frame "re-wrap the whole scrollback" cost. The load-bearing
  `last_rendered_rows` / click-map assembly was deliberately left untouched (the
  risk the residual flagged); the wrapped block is a pure function of its key, and
  a regression test asserts the cache-hit frame is byte-for-byte identical to a
  cache-miss / cold-cache frame.

## Documented residuals (by design — not open defects)

Both were explicitly reviewed and **accepted as-is** (2026-06-29). The
higher-value alternatives were weighed and declined: for `#141`, strengthening
the real classifier boundary (fenced prompt / fail-safe parse) rather than the
denylist; for `#142`, a portable per-component `O_NOFOLLOW` descent. In each case
the primary boundary/platform is already solid and the residual is either
inherent (a denylist can't catch paraphrase) or narrow and non-primary (a local
symlink race on non-Linux hosts), so the cost/benefit didn't favor investing.

- **#141 — by design.** The Auto-classifier injection pre-filter was hardened
  (whitespace/zero-width normalization + more reviewer-directed markers) but is
  inherently incomplete; the fenced untrusted-action prompt and the fail-safe
  verdict parse remain the real defense. Defense-in-depth, not a boundary.
- **#142 — by design.** On non-Linux / pre-5.6 kernels the path-confinement
  fallback keeps a check-then-use symlink TOCTOU window; Linux
  `openat2(RESOLVE_BENEATH)` closes it. Closing the fallback would mean the
  per-component symlink inspection `openat2` exists to replace. It now emits a
  one-time operator warning when the fallback path is used.

No open defects remain.
