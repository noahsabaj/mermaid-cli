# AGENTS.md

Contributor + agent guardrails for Mermaid. Terse on purpose — the code and
tests hold the detail; this is what's easy to get wrong.

## Architecture: pure MVU core, effects as data

- `src/domain/` is a **pure** Model-View-Update core. The reducer is
  `fn update(State, Msg) -> (State, Vec<Cmd>)`: synchronous, no `.await`, no
  wildcard `_ =>` arms that hide new `Msg`s, no I/O, no wall clock (it reads
  `state.now`, injected, so `--replay` is deterministic). Effects are **data**
  (`Cmd`); the impure shell (`src/effect/`) executes them. `render(&State)` is
  pure. CI enforces this via `.github/scripts/check_domain_purity.py`.
- One `TurnId` = one model call + its tools; an agentic run spans many turns.
  Tool outcomes gate through `Vec<Option<ToolOutcome>>` plus a stale-turn drop —
  don't bypass it.

## Hard rules

- **No emojis / pictographs** in any user-facing output, ever. CI enforces it
  (`.github/scripts/check_no_emoji.py`). Box-drawing, arrows, and the middot are
  fine — they sit below the flagged ranges.
- **No back-compat shims.** Mermaid has no released users yet: delete cleanly
  rather than deprecate — no renamed `_vars`, no "removed" tombstone comments.
- **Keep the CHANGELOG current.** Add an entry under `## [Unreleased]` in the
  same PR as the change.
- **Never leak secrets.** Redact via `redact_secrets` / `redact_json` before any
  persisted or logged output; conversation and config files are `0600`.
- Don't let `reducer.rs` grow unbounded — decompose into helpers (clippy caps
  functions at 100 lines).
- UI surfaces get coverage; keep the render/reducer tests green.

## Commands

`just check` is the exact pre-PR gate (also what CI runs):

```
just check    # fmt --check + clippy -D warnings + nextest run
just fmt      # format the workspace
just fix      # clippy --fix, then format
```

Or run the pieces directly:

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```
