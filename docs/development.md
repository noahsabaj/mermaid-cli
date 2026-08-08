# Development

Contributor guardrails live in [`AGENTS.md`](../AGENTS.md) (MVU purity, the no-emoji
rule, no back-compat shims, the ratchet baselines). The one-command pre-PR gate is:

```
just check    # cargo fmt --check + clippy -D warnings + guards + cargo nextest run
```

Or run the pieces directly if you don't have [`just`](https://github.com/casey/just)
/ [`cargo-nextest`](https://nexte.st):

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace   # or: cargo test --workspace
```

That gate runs the same suites on every platform, including the render
snapshots. What is still `#[cfg(unix)]` is per-test and covers what Windows does
not have — file modes, signals, `sh`, seccomp, Landlock, Seatbelt — with
`#[cfg(windows)]` tests covering the `cmd`, ConPTY and clipboard paths in their
place.

CI additionally runs dependency-free source guards (`.github/scripts/`): no
emoji/pictographs in source, and `mermaid-domain` stays a pure MVU core (no I/O, no
wall clock).

A PR runs the suite on Linux, macOS and Windows against **stable**, and all
three must pass to merge. `beta` and `nightly` answer a different question — is
a future toolchain about to break us — whose answer does not change from one PR
to the next, so they run on a daily schedule and on every push to `main` rather
than on the critical path of your merge. Run them on demand from the Actions tab
(`workflow_dispatch`) if a change is toolchain-sensitive.

## Snapshot suites

The TUI has a snapshot suite (`src/render/snapshots.rs`) that pins full rendered
frames for curated scenes at 80x24 and 120x40. One set of `.snap` files serves
every platform: the suite pins the clock, host, user, version and cwd, and its
fixture clock is a fixed *local wall clock* rather than a fixed instant, so the
rendered timestamp reads the same in every timezone. A mismatch panics with a
diff and writes a gitignored `.snap.new` sibling. The test job publishes those
as a `pending-snapshots-*` artifact on failure, so a frame from a platform you
don't have is still reviewable. Review and accept deliberate visual changes with
`just snapshots` (`cargo insta review`) and commit the updated `.snap` files in
the same PR as the style change.

Terminal behavior that a `Line`/`Span` assertion cannot see — a shredded
background, a glyph past a border, a stale status band — is covered separately
by `tests/pty_frame.rs`, which drives the real binary on a pty and compares
whole terminal grids against `tests/snapshots/*.txt`. Those DO run on every
platform. Regenerate with `UPDATE_SNAPSHOTS=1 cargo test --test pty_frame` and
read the diff before accepting it.
