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

# Run the test suite via nextest.
test *ARGS:
    cargo nextest run --workspace {{ARGS}}

# Dependency-free source guards. CI runs exactly these.
#
# `just check` used to skip them while claiming to be "what CI runs" — CI has
# always run guards this recipe did not.
guards:
    {{python}} .github/scripts/check_no_emoji.py
    {{python}} .github/scripts/check_layering.py
    {{python}} .github/scripts/check_expect_budget.py
    {{python}} .github/scripts/check_exports.py
    {{python}} .github/scripts/check_build_tree_out_of_repo.py
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
# path. `CLIPPY_RATCHET_TARGET_DIR` keeps that rebuild out of the
# main build tree, so the next `cargo test` does not pay for this one. It is
# an explicit override, so it does NOT inherit build.target-dir from
# .cargo/config.toml — it has to name an out-of-tree path itself, or it
# recreates a ./target inside the checkout. Both recipes run the clippy
# named in .github/baselines/clippy_toolchain.txt (the script adds the
# `+<toolchain>`), because each release moves these counts; to move to a
# newer clippy, edit that file and re-record.
clippy-debt:
    CLIPPY_RATCHET_TARGET_DIR=../mermaid-target/clippy-debt {{python}} .github/scripts/check_clippy_ratchet.py

# Re-record it after paying some down.
clippy-debt-record:
    CLIPPY_RATCHET_TARGET_DIR=../mermaid-target/clippy-debt {{python}} .github/scripts/check_clippy_ratchet.py --write-baseline

# Create an isolated worktree off fresh origin/main, with its own build tree.
#
# Each worktree gets its OWN parent directory, which is what keeps the build
# trees apart: build.target-dir in .cargo/config.toml is relative, and cargo
# resolves it against each worktree's root, so `<base>/<name>/repo` builds into
# `<base>/<name>/mermaid-target`. Worktrees sharing one parent would resolve to
# one shared dir and serialise on cargo's exclusive target-dir lock -- which is
# the whole reason the old setup used per-worktree CARGO_TARGET_DIR values, and
# how target-hardening/, target-tui/ and target-mermaidd/ ended up inside the
# checkout in the first place.
#
# Always off fresh origin/main, never off the primary checkout's HEAD.
worktree NAME BASE="../mermaid-worktrees":
    git fetch origin --quiet
    git worktree add -b {{NAME}} {{BASE}}/{{NAME}}/repo origin/main
    @echo "worktree:   {{BASE}}/{{NAME}}/repo"
    @echo "build tree: {{BASE}}/{{NAME}}/mermaid-target (outside the worktree)"

# Remove a worktree created by `just worktree`, and its build tree with it.
# Dropping the worktree alone leaves gigabytes of build output orphaned on disk.
worktree-rm NAME BASE="../mermaid-worktrees":
    git worktree remove {{BASE}}/{{NAME}}/repo
    rm -rf {{BASE}}/{{NAME}}
    @echo "removed {{BASE}}/{{NAME}} (worktree + build tree)"

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
