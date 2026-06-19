# Mermaid Self-QA Harness

`scripts/qa_mermaid.py` lets an agent dogfood Mermaid without manual TUI driving.
It runs the real `mermaid run` path with a real model, lets Mermaid edit a safe
fixture, and verifies artifacts afterward.

## Quick Start

```bash
# Public deterministic self-test, no model call
mermaid self-test

# Full agent dogfood harness
python3 scripts/qa_mermaid.py
```

Defaults:

- Test root: `/home/nsabaj/Code/test-env`
- Tier: `fast` (deterministic checks, no model call)
- Real-model tier: `minimax-m2.7:cloud`
- Binary: `target/debug/mermaid`

The harness builds the debug binary first with `cargo build --bin mermaid`.
`mermaid self-test` is the smaller public smoke path: it runs deterministic
checks in a temporary project directory and does not require a model call or
manual TUI interaction.

## What It Wipes

The harness owns `/home/nsabaj/Code/test-env`. On each run it removes existing
contents there, except prior `.qa-runs` logs unless `--wipe-runs` is passed.
It refuses dangerous reset targets such as `/`, `$HOME`, the Mermaid repo, and
unmarked custom paths.

After reset, the directory contains harness-managed content only:

- `.mermaid-qa-root`
- `.qa-runs/<timestamp>/`
- `workspaces/<scenario>/`

## Useful Commands

```bash
# Run the default fast deterministic suite
python3 scripts/qa_mermaid.py

# Same intent through the public CLI surface
mermaid self-test --format json

# Run all real-model dogfood scenarios
python3 scripts/qa_mermaid.py --tier real

# Run fast + real scenarios
python3 scripts/qa_mermaid.py --tier all

# Run one scenario
python3 scripts/qa_mermaid.py --scenario compact_smoke

# Override the model
MERMAID_QA_MODEL=minimax-m2.7:cloud python3 scripts/qa_mermaid.py --tier real

# Keep the mutated scenario workspace for inspection
python3 scripts/qa_mermaid.py --scenario add_small_feature --keep-workspace

# Emit machine-readable top-level status
python3 scripts/qa_mermaid.py --json
```

## Scenarios

- `compact_smoke`: runs the hidden `mermaid qa compact-smoke` command to
  exercise context compaction deterministically without a model call.
- `runtime_smoke`: Mermaid's runtime task listing command exits cleanly.
- `read_only_project_scan`: Mermaid inspects a tiny Python project and must not
  change files.
- `fix_failing_test`: Mermaid fixes a bug in implementation code, leaves tests
  alone, and makes `python3 -m unittest discover -s tests` pass.
- `add_small_feature`: Mermaid implements a missing function against existing
  tests, leaves tests alone, and makes the same unittest command pass.

Each scenario writes stdout, stderr, parsed Mermaid JSON, before/after manifests,
and a unified diff under `.qa-runs/<timestamp>/<scenario>/`.

## Current Limit

This is headless QA. It exercises the same reducer, effect runner, providers,
and tools as the interactive app, but it does not yet automate the full-screen
TUI. PTY-driven TUI scenarios should be added after the headless loop is stable.
