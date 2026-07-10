# Mermaid dev tasks. `just check` is the exact pre-PR gate that CI runs.

# Show the recipes.
default:
    @just --list

# One-command gate: format check, lint (deny warnings), full test suite.
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo nextest run --workspace

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
