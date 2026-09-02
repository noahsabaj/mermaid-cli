# AGENTS.md

Contributor + agent guardrails for Mermaid. Terse on purpose — the code and
tests hold the detail; this is what's easy to get wrong.

## Architecture: pure MVU core, effects as data

- `crates/mermaid-domain/` is a **pure** Model-View-Update core, and it is a
  crate so the compiler enforces that rather than a reviewer. The reducer is
  `fn update(State, Msg) -> (State, Vec<Cmd>)`: synchronous, no `.await`, no
  wildcard `_ =>` arms that hide new `Msg`s (`update_step` carries
  `#[deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)]`
  — both, because the first only fires when `_` covers two or more remaining
  variants), no I/O, no wall clock (it reads
  `state.now`, injected, so `--replay` is deterministic). Effects are **data**
  (`Cmd`); the impure shell (`src/effect/`) executes them. `render(&State)` is
  pure too — a function of domain state and nothing else.

  Enforcement is split, and the split is the point. **Direction** belongs to the
  crate boundary: `use crate::app::Config` inside `mermaid-domain` is an
  `unresolved import`. So does most of **purity**, via what the manifest omits —
  it lists no `tokio`, `reqwest`, `rusqlite`, `crossterm` or `clap`, so a
  reducer that wants to `.await` cannot, because the runtime is not a
  dependency. `.github/scripts/check_layering.py` owns only what neither can
  express: `std::fs`, `std::process`, `std::net` and the wall clock, for
  `mermaid-domain` and `src/render`. Its predecessor could see none of the
  first two, which is how the "pure" core came to hold 34 upward edges, two of
  them cycles. What debt remains is in `.github/baselines/layering.txt`, which
  may only shrink.
- One `TurnId` = one model call + its tools; an agentic run spans many turns.
  Tool outcomes gate through `Vec<Option<ToolOutcome>>` plus a stale-turn drop —
  don't bypass it.

## Hard rules

- **No emojis / pictographs** in any user-facing output, ever. CI enforces it
  (`.github/scripts/check_no_emoji.py`). Box-drawing, arrows, and the middot are
  fine — they sit below the flagged ranges.
- **No back-compat shims.** The product is the `mermaid` binary. The published
  crates (`mermaid-cli`, `mermaid-domain`, `mermaid-model`, `mermaid-runtime`)
  carry **no
  API-stability promise** — they are on crates.io only because `cargo publish`
  cannot resolve an unpublished path dependency. So delete cleanly rather than
  deprecate: no renamed `_vars`, no "removed" tombstone comments, and no
  `pub use` kept alive for a hypothetical downstream. That last clause is the
  one the compiler cannot help with — a `pub` item is reachable by definition,
  so `-D warnings` never sees it — and it is why
  `.github/scripts/check_exports.py` exists. Breaking a library
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

  **A local clippy run only covers your own platform.** The integration
  suite is one binary (`tests/integration.rs`, modules under `tests/it/`);
  `tests/it/mod.rs` gates `pty_exit` and `daemon_integration` with
  `#[cfg(unix)]` and `sandbox_fs`/`sandbox_network` with Linux+macOS+Windows,
  and 99 items under `src/` and `crates/` are `#[cfg(unix)]`. On Windows those
  compile to nothing, so clippy has nothing to lint and a green local run says
  nothing about them — the two `too_many_lines` violations in `pty_exit.rs`
  were found by CI, not by any local sweep. To check a gated module before
  pushing, drop the `#[cfg(...)]` on its `mod` line in `tests/it/mod.rs` (and
  any `#![cfg(...)]` at the top of the file) and run
  `cargo clippy --test integration`; that reproduces the Linux verdict exactly.
  Restore both afterwards.
- UI surfaces get coverage; keep the render/reducer tests green.

## Ratchets

The source guards record their known violations in `.github/baselines/*.txt`
and fail CI if the set grows. They also fail if it *shrinks without the file
being updated* — so fixing something puts the new, smaller number in your diff,
which is the only reason a baseline ever moves. A baseline that could only be
appended to would be a place debt goes to be forgotten.

| baseline | guard | what it counts |
|---|---|---|
| `layering.txt` | `check_layering.py` | impurity in `mermaid-domain` and `src/render` |
| `expect_budget.txt` | `check_expect_budget.py` | every `#[expect]` / `#[allow]` of a clippy lint |
| `exports.txt` | `check_exports.py` | crate-root `pub use` names with no consumer |
| `clippy_pedantic.txt` | `check_clippy_ratchet.py` | pedantic + nursery warnings, by lint |

Run `just ratchet` and commit the result. The `N keys / M occurrences` header
line of each file is the debt counter; it should be going down.

`clippy_pedantic.txt` is the exception to that command and to the guard shape.
Its lints are *tracked*, not blocking — they are taste categories with real
false-positive rates — so the job runs off the PR critical path
(`if: github.event_name != 'pull_request'`), because enabling them changes
clippy's fingerprint and rebuilds the workspace. It gets its own recipes,
`just clippy-debt` and `just clippy-debt-record`, so `just ratchet` stays
instant. It also keys on the lint alone rather than `(lint, file)`: per-file
keys measure 1,390 entries against 85, and every one of them churns when a file
is split.

Its `clippy::unwrap_used` count measures **shipped code**, via
`allow-unwrap-in-tests` in `.clippy.toml`. Counting the suite put it at 1,353,
all but a handful of them tests — `unwrap()` in a test *is* the assertion, and
the panic is the failure being reported. A number that large and that
test-shaped tracks how many tests exist, not how much risk ships, and it had
started charging new tests against a budget. Six remain. Five are `clap`
`default_value_t` expansions under `src/cli/args.rs`, where the `unwrap()` is
the derive macro's and not ours; the sixth is somewhere under `#[cfg(unix)]`,
counted by the Linux job and not locatable from a Windows checkout.

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
