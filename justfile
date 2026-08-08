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

# Re-record every guard's baseline. Review the diff: the `N keys / M
# occurrences` header line is the debt counter, and it should be going down.
ratchet:
    {{python}} .github/scripts/check_layering.py --write-baseline
    {{python}} .github/scripts/check_expect_budget.py --write-baseline

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
