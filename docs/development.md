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

## Cutting a release

Branch off `main`, never commit to it directly. Then, in one commit:

1. Bump the version in **every** manifest — `Cargo.toml` plus each
   `crates/*/Cargo.toml`, both the `[package] version` and every
   intra-workspace `version =` requirement. That is 13 strings across four
   manifests as of v0.23.0.
2. Run `cargo update --workspace` so `Cargo.lock` follows.
3. Cut `## [Unreleased]` in the CHANGELOG to `## [x.y.z] - <date>`, leaving a
   fresh empty `## [Unreleased]`. Update the link block at the bottom: add a
   `[x.y.z]:` compare link and point `[Unreleased]:` at `vx.y.z...HEAD`.

Then verify, **before tagging**:

```
just preflight 0.23.0    # the target version, with or without a leading `v`
just check
```

`just preflight` mirrors every gate `.github/workflows/release.yml` applies.
This matters because the workflow applies them *after* the tag is pushed, and
the CHANGELOG check runs downstream of five platform builds — so a mismatch is
discovered once the GitHub release exists and its binaries have shipped. The
recovery is deleting the tag and re-cutting it, which is worse than a
five-second local check.

Open a PR, wait for all legs, merge. Then tag the merged commit and push:

```
git tag -a v0.23.0 -m "v0.23.0"
git push origin v0.23.0
```

The tag triggers `release.yml`: it verifies the versions again, builds five
platforms, publishes the crates in dependency order (model, runtime, domain,
cli), then the package managers.

If a build fails, nothing is published — the publish jobs `needs:` the builds.
Confirm with `gh release view` and crates.io, then delete the tag
(`git push origin :refs/tags/vX.Y.Z`), re-tag at the fix, and push again. That
is cheaper than burning a version number.

`check_release_ready.py` carries a `--self-test` that builds a deliberately
un-bumped fixture and asserts every gate fires on it, then bumps the same
fixture and asserts they all go quiet. `just guards` and CI run it on every PR,
so the gate cannot rot into one that passes everything — a release gate that
has never failed is not evidence.

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
