# Mermaid dev tasks. `just check` is the exact pre-PR gate that CI runs.

# Show the recipes.
default:
    @just --list

# `python3` is a Microsoft Store stub on Windows; `python` is the real one.
python := if os() == "windows" { "python" } else { "python3" }

# One-command gate: format check, lint (deny warnings), source guards, tests.
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    just guards
    cargo nextest run --workspace

# Dependency-free source guards. CI runs exactly these.
#
# `just check` used to skip them while claiming to be "what CI runs" — CI has
# always run guards this recipe did not.
guards:
    {{python}} .github/scripts/check_no_emoji.py
    {{python}} .github/scripts/check_layering.py
    {{python}} .github/scripts/check_expect_budget.py
    {{python}} .github/scripts/check_exports.py
    {{python}} .github/scripts/check_release_ready.py --self-test

# Mirrors every gate `release.yml` applies, so a version mismatch or an empty
# CHANGELOG section is found BEFORE `git tag` rather than after the GitHub
# release and its binaries have shipped. Example: `just preflight 0.23.0`.
#
# Is this tree ready to tag? (`just preflight 0.23.0`)
preflight VERSION:
    {{python}} .github/scripts/check_release_ready.py {{VERSION}}

# Re-record every guard's baseline. Review the diff: the `N keys / M
# occurrences` header line is the debt counter, and it should be going down.
ratchet:
    {{python}} .github/scripts/check_layering.py --write-baseline
    {{python}} .github/scripts/check_expect_budget.py --write-baseline
    {{python}} .github/scripts/check_exports.py --write-baseline

# Tier-2 lint debt: pedantic + nursery, tracked but not blocking. Kept out of
# `just guards` and `just ratchet` because turning these lints on changes
# clippy's fingerprint and rebuilds the workspace — minutes, against
# milliseconds for the file-reading guards. CI runs it off the PR critical
# path. `CLIPPY_RATCHET_TARGET_DIR` keeps that rebuild out of ./target, so the
# next `cargo test` does not pay for this one.
clippy-debt:
    CLIPPY_RATCHET_TARGET_DIR=target/clippy-debt {{python}} .github/scripts/check_clippy_ratchet.py

# Re-record it after paying some down.
clippy-debt-record:
    CLIPPY_RATCHET_TARGET_DIR=target/clippy-debt {{python}} .github/scripts/check_clippy_ratchet.py --write-baseline

# Format the whole workspace.
fmt:
    cargo fmt --all

# Apply clippy's machine-applicable fixes, then format.
fix:
    cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged
    cargo fmt --all

# Review render-snapshot drift interactively (accept/reject .snap.new files).
snapshots:
    cargo insta review
