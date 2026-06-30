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

- **Status:** 0 open defects · 3 documented residuals (below)
- **Last updated:** 2026-06-29

---

## Documented residuals (by design / deferred — not open defects)

- **#134 — deferred optimization.** The per-frame transcript double-clone and the
  markdown-cache thrash are fixed (`Cow` borrow on idle frames + bounded cache
  eviction). What remains is a pure optimization: caching the wrapped/stringified
  lines per committed message so the whole scrollback isn't re-wrapped each frame.
  Deferred because it reworks `last_rendered_rows` (load-bearing for scroll/click
  math) and must not alter rendered output — a focused change for later, not a
  correctness bug.
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
