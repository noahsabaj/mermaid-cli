# Behavioural evals

A handful of fixed coding tasks, each scored by what the run actually did: does
`cargo test` pass now, were the tests left alone, does the answer name the
right port. Nothing here checks wording, in the prompt or in the reply.

That is what makes the suite useful for deleting things. A phrase test can only
say a paragraph of the system prompt is still there. An outcome test says
whether removing it made anything worse. Run the suite against each new model
generation with no code change: if scores rise, the harness is scaling with the
model, and if a piece of coaching stops moving the score, it can go.

## Running it against a model

```
just eval anthropic/<model>
just eval anthropic/<model>,ollama/qwen3-coder:30b
```

Each task runs through the real binary (`mermaid run --format ndjson`) in a
throwaway copy of its fixture, in `full_access` mode, using your own config and
credentials. It costs whatever those model calls cost. The report (`report.md`
and `runs.json`) is printed and written under your temp directory; every run's
working copy, event stream, stderr and `git diff` stay next to it for a
post-mortem.

| variable | meaning |
|---|---|
| `MERMAID_EVAL_MODELS` | comma-separated model ids (what `just eval` sets) |
| `MERMAID_EVAL_TASKS` | comma-separated task ids to run; default all |
| `MERMAID_EVAL_REPEAT` | runs per task; default 1. Models are stochastic, so use 3 or more before drawing conclusions |
| `MERMAID_EVAL_GUIDANCE` | comma-separated `default`, `on`, `off`: run every task once per setting of the guidance pack. Default `default`, which is whatever your config says |
| `MERMAID_EVAL_JOBS` | runs in flight at once; default 1. Mind the provider's rate limits |
| `MERMAID_EVAL_LABEL` | a tag for this run's history entries, such as the change being measured |
| `MERMAID_EVAL_HISTORY` | history file to append to; `off` to append nothing. Default `evals/results/history.jsonl` |
| `MERMAID_EVAL_OUT` | report directory |

Scores are reported, not asserted: a model failing a task is a result, not a
test failure. A run where the model never answered at all (a bad key, an
unknown id) is flagged separately, so a misconfiguration does not read as a
score of zero.

## Guidance pack on versus off

The guidance pack is the coaching layered onto the core system prompt: how to
plan, how to read a codebase, checklist discipline. The question the suite
exists to answer is whether that coaching still helps the models people use,
and the only way to answer it is to run the same model with the pack on and
then off:

```
just eval-guidance anthropic/<model>           # 3 runs per task per setting
just eval-guidance ollama/qwen3-coder:30b 5
```

The report then has a section per setting and an on-versus-off table: pass
rates, mean tokens, a per-task difference, and a verdict (the pack helps,
hurts, or makes no measured difference). With fewer than 3 runs per task on
either side, it says the difference is only a hint.

**The default is a stand-in.** `[output] guidance = "auto"`, the shipped
default, turns the pack on for local providers (Ollama, or a `base_url` on a
loopback or LAN host) and off for hosted APIs. Where a model is served says
little about how capable it is: a strong local model gets coached anyway, and
a weak hosted one does not. The table's `default` column is what your config
picks for each model, and the verdict says whether that pick matches the
measurement ("the default is right" or "the default is wrong for this model").
Once enough models have been measured, the default should come from these
results rather than from locality.

## History

Every live run appends one line per model and guidance setting to
`evals/results/history.jsonl`: the date, the Mermaid version and commit (with
`dirty` when the checkout had uncommitted changes), the `MERMAID_EVAL_LABEL`
tag, and each task's passes, runs, turns, tokens and seconds. Commit it. The
report ends with a history section scored on the tasks the current run
covered, so rows stay comparable as tasks are added:

- every model ever recorded, latest run first by pass rate, which is the
  "does the harness scale with the model" view across releases;
- each current model's own trend over its earlier runs, which is the "did this
  harness change help" view.

For a throwaway run (debugging a task, a half-configured model) set
`MERMAID_EVAL_HISTORY=off`. A setting where no run was scored at all (a bad
key, an unknown id) records nothing, so a misconfiguration never enters the
history as a zero.

## What CI runs

The offline tier (`tests/it/evals.rs`) runs every task on every CI run, with a
scripted OpenAI-compatible endpoint in place of the model replaying the task's
`reference.toml`. It does not measure any model. It keeps the suite honest:

- each reference solution passes its task's checks, so every task is passable;
- a run that does nothing fails every task, so no check is vacuous;
- the path from model output to changed files works end to end.

It builds on the same determinism `--replay` relies on, one layer further out.
`--replay` folds a recorded reducer log and deliberately runs no tools, so it
cannot tell whether a file changed. The evals need tools to run for real, so
the recording is of the model's side instead: the scripted endpoint replays the
model's turns, and everything below it (adapter, reducer, effect runner,
tools) runs as it would live.

## Fuzzy edits

`edit_file` and `apply_patch` fall back to fuzzy matching (whitespace and
Unicode drift) when a model's context is not exact. Every `tool_finished` line
for such an edit carries `"fuzzy": true`, and the report counts them per model.
When that rate sits near zero for the models people actually use, the fuzzy
matcher is safe to delete.

## The tasks

| task | what it asks | scored by |
|---|---|---|
| `fix-failing-test` | make a failing `cargo test` pass | tests pass, `tests/` untouched |
| `add-flag` | add `--shout` to a small CLI | new flag works, old behaviour unchanged, tests pass |
| `answer-repo-question` | which port does the server use? (the README is stale) | answer names 7431, nothing modified |
| `ledger-refunds` | add a refund entry kind through parsing, totals and the report | a hidden ledger totals right, old output unchanged, tests pass |
| `ledger-exact-money` | move money from `f64` to integer cents in every module | the `-0.00` symptom is gone, a hidden ledger where `f64` loses a cent totals right, tests pass |
| `stock-wrong-totals` | wrong per-category totals, reported by symptom only; the cause is the CSV splitter in another module | the visible export and a hidden one with quoted commas total right, tests and data untouched |
| `ignored-parameter` | offline only: a provider that silently ignores parameters | see below |

The first three are short and single-file. The last three are closer to real
work: several files, a symptom rather than a failing test, and a hidden input
the model never sees (see `overlay` below), so fitting the visible samples is
not enough.

`ignored-parameter` covers providers that accept a parameter they do not
support and silently drop it instead of returning a 400, as many
OpenAI-compatible and local servers do. Learning capabilities from errors never
sees these, because nothing errors. The run asks for high reasoning effort and
a JSON answer; the endpoint takes both and honours neither. The task passes
when the question still gets answered and Mermaid reports the schema miss,
rather than passing prose off as structured output.

## Adding a task

1. Put the starting project under `fixtures/<name>/` (or reuse one). Keep it
   small and dependency-free: checks run on every CI platform. A Rust fixture
   needs its own `Cargo.lock` and a `.gitignore` for `/target/` and
   `/.mermaid/`.
2. Write `tasks/<id>/task.toml`:

   ```toml
   fixture = "median"
   prompt = "..."
   # optional
   args = ["--reasoning", "high"]   # extra top-level mermaid flags
   output_schema = "schema.json"    # relative to the task directory
   offline_only = true              # skip in the live tier

   [[check]]
   kind = "command"                 # exits 0 in the project afterwards
   run = ["cargo", "test", "--offline", "--quiet"]
   stdout_contains = "..."          # optional
   overlay = "hidden"               # optional: copy tasks/<id>/hidden/ into the
                                    # project first (inputs the model never saw)

   [[check]]
   kind = "unchanged"               # byte-identical to the fixture; "." for everything
   paths = ["tests"]

   [[check]]
   kind = "answer_contains"         # final answer contains one of these, ignoring case
   any = ["7431"]

   [[check]]
   kind = "result"                  # facts from the run's result line
   structured_output = false
   error_contains = "output_schema"

   [[check]]
   kind = "request_sent"            # offline only: some request carried these fields
   fields = ["reasoning_effort"]
   ```

3. Write `tasks/<id>/reference.toml`, a known-good solution as model turns:

   ```toml
   fuzzy_edits = 0                  # edits in this reference that only apply fuzzily

   [[turn]]
   tool = "read_file"
   args = { path = "src/lib.rs" }

   [[turn]]
   calls = [                        # several tool calls in one turn
       { tool = "read_file", args = { path = "a.rs" } },
       { tool = "read_file", args = { path = "b.rs" } },
   ]

   [[turn]]
   say = "Final answer."
   ```

4. `cargo test --test integration it::evals` must pass.

Check an outcome, never a wording. If a task can only be scored by matching the
model's phrasing, it is the wrong task.
