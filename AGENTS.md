# AGENTS.md

Contributor + agent guardrails for Mermaid. Terse on purpose — the code and
tests hold the detail; this is what's easy to get wrong.

## Architecture: pure MVU core, effects as data

- `src/domain/` is a **pure** Model-View-Update core. The reducer is
  `fn update(State, Msg) -> (State, Vec<Cmd>)`: synchronous, no `.await`, no
  wildcard `_ =>` arms that hide new `Msg`s (`update_step` carries
  `#[deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)]`
  — both, because the first only fires when `_` covers two or more remaining
  variants), no I/O, no wall clock (it reads
  `state.now`, injected, so `--replay` is deterministic). Effects are **data**
  (`Cmd`); the impure shell (`src/effect/`) executes them. `render(&State)` is
  pure too — a function of domain state and nothing else.
  `.github/scripts/check_layering.py` enforces both properties for
  `src/domain`, `src/render`, and `src/prompts.rs`: they may only reach
  *downward*, and none of them may touch the filesystem, the network, a
  process, an async runtime, or the wall clock. Dependency **direction** is the
  half the old guard could not see — `use crate::app::Config` contains no
  forbidden token — and it is how the "pure" core came to hold 34 upward edges,
  two of them cycles. That debt is recorded in
  `.github/baselines/layering.txt`, which may only shrink.
- One `TurnId` = one model call + its tools; an agentic run spans many turns.
  Tool outcomes gate through `Vec<Option<ToolOutcome>>` plus a stale-turn drop —
  don't bypass it.

## Hard rules

- **No emojis / pictographs** in any user-facing output, ever. CI enforces it
  (`.github/scripts/check_no_emoji.py`). Box-drawing, arrows, and the middot are
  fine — they sit below the flagged ranges.
- **No back-compat shims.** The product is the `mermaid` binary. The published
  crates (`mermaid-cli`, `mermaid-model`, `mermaid-runtime`) carry **no
  API-stability promise** — they are on crates.io only because `cargo publish`
  cannot resolve an unpublished path dependency. So delete cleanly rather than
  deprecate: no renamed `_vars`, no "removed" tombstone comments, and no
  `pub use` kept alive for a hypothetical downstream. Breaking a library
  signature is free; breaking a CLI flag or an on-disk format needs a CHANGELOG
  entry under `### Changed`.
- **Keep the CHANGELOG current.** Add an entry under `## [Unreleased]` in the
  same PR as the change.
- **Never leak secrets.** Redact via `redact_secrets` / `redact_json` before any
  persisted or logged output; conversation and config files are `0600`.
- Don't let `reducer.rs` grow unbounded — decompose into helpers. Clippy denies
  `too_many_lines` at 100 (`.clippy.toml` sets the threshold; every manifest's
  `[lints.clippy]` enables the lint — for years only the first half was true).
  Going over means adding `#[expect(clippy::too_many_lines, reason = "...")]`
  **and** raising a number in `.github/baselines/expect_budget.txt` — a visible
  act, against a budget that only shrinks. `#[expect]` over `#[allow]`
  throughout: it warns once the suppression stops being necessary, so
  shortening a function tells you to delete its attribute. Converting the
  existing `#[allow]`s found four that were suppressing nothing.
- UI surfaces get coverage; keep the render/reducer tests green.

## Ratchets

The source guards record their known violations in `.github/baselines/*.txt`
and fail CI if the set grows. They also fail if it *shrinks without the file
being updated* — so fixing something puts the new, smaller number in your diff,
which is the only reason a baseline ever moves. A baseline that could only be
appended to would be a place debt goes to be forgotten.

| baseline | guard | what it counts |
|---|---|---|
| `layering.txt` | `check_layering.py` | upward imports + impurity in `domain`/`render`/`prompts` |
| `expect_budget.txt` | `check_expect_budget.py` | every `#[expect]` / `#[allow]` of a clippy lint |

Run `just ratchet` and commit the result. The `N keys / M occurrences` header
line of each file is the debt counter; it should be going down.

## Commands

`just check` is the exact pre-PR gate (also what CI's blocking jobs run):

```
just check    # fmt --check + clippy -D warnings + guards + nextest run
just guards   # the dependency-free source guards on their own
just ratchet  # re-record the guard baselines after you fix something
just fmt      # format the workspace
just fix      # clippy --fix, then format
```

Or run the pieces directly:

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```
