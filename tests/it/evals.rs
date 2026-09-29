//! The behavioural eval suite. See `tests/harness/evals/mod.rs` for the
//! design and `evals/README.md` for how to run it against a model.
//!
//! The offline tests below cannot score a model, and do not try to. They keep
//! the suite itself honest on every CI run: each task's checks are passable
//! (the reference solution passes them), none is vacuous (doing nothing fails
//! them), and the path from model output to changed files works end to end
//! through the real binary. The `live` test is the benchmark, and runs only
//! when asked.

use std::path::PathBuf;

use crate::harness::evals::mock_provider::{MockProvider, MockTurn};
use crate::harness::evals::{Check, Run, Target, report, run_task, tasks};

/// Run every task at once, one thread each: most of a run is waiting on a
/// child process, and serially the suite would take several times as long.
fn run_all(
    script_for: impl Fn(&crate::harness::evals::Task) -> Vec<MockTurn> + Sync,
) -> Vec<(Run, MockProvider)> {
    let tasks = tasks();
    std::thread::scope(|scope| {
        let handles: Vec<_> = tasks
            .iter()
            .map(|task| {
                let script = script_for(task);
                scope.spawn(move || {
                    let mock = MockProvider::start(script);
                    let run = run_task(task, &Target::Mock(&mock));
                    (run, mock)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("an eval run panicked"))
            .collect()
    })
}

#[test]
fn every_task_is_well_formed() {
    let tasks = tasks();
    assert!(
        tasks.iter().filter(|t| !t.spec.offline_only).count() >= 3,
        "the live tier needs real tasks to score"
    );
    for task in &tasks {
        assert!(
            task.fixture().is_dir(),
            "{}: no fixture at {}",
            task.id,
            task.fixture().display()
        );
        assert!(!task.spec.checks.is_empty(), "{}: no checks", task.id);
        // Parses, and is not empty.
        assert!(
            !task.reference().script().is_empty(),
            "{}: empty reference",
            task.id
        );
        if let Some(schema) = &task.spec.output_schema {
            assert!(task.dir.join(schema).is_file(), "{}: no {schema}", task.id);
        }
        // A request check can only be scored against the mock, so the live
        // tier must never see it.
        if task
            .spec
            .checks
            .iter()
            .any(|c| matches!(c, Check::RequestSent { .. }))
        {
            assert!(
                task.spec.offline_only,
                "{}: a request_sent check needs offline_only = true",
                task.id
            );
        }
    }
}

#[test]
fn each_reference_solution_passes_its_checks() {
    let runs = run_all(|task| task.reference().script());
    let mut failures = Vec::new();
    for (run, mock) in &runs {
        let expected_fuzzy = tasks()
            .into_iter()
            .find(|t| t.id == run.task)
            .map(|t| t.reference().fuzzy_edits)
            .unwrap_or_default();
        let mut problems = Vec::new();
        if !run.passed() {
            problems.push("checks failed".to_string());
        }
        if mock.remaining() != 0 || mock.overruns() != 0 {
            problems.push(format!(
                "the run did not follow the reference: {} turns unused, {} calls past the end",
                mock.remaining(),
                mock.overruns()
            ));
        }
        if run.fuzzy_edits != expected_fuzzy {
            problems.push(format!(
                "{} fuzzy edits reported, the reference has {expected_fuzzy}",
                run.fuzzy_edits
            ));
        }
        if problems.is_empty() {
            let _ = std::fs::remove_dir_all(&run.sandbox);
        } else {
            failures.push(format!(
                "{}: {}\n{}",
                run.task,
                problems.join("; "),
                run.explain()
            ));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

#[test]
fn a_run_that_does_nothing_fails_every_task() {
    // A check a do-nothing run passes is not measuring anything.
    let runs = run_all(|_| vec![MockTurn::Say("I could not complete this task.".to_string())]);
    let vacuous: Vec<String> = runs
        .iter()
        .filter(|(run, _)| run.passed())
        .map(|(run, _)| run.explain())
        .collect();
    for (run, _) in &runs {
        if !run.passed() {
            let _ = std::fs::remove_dir_all(&run.sandbox);
        }
    }
    assert!(
        vacuous.is_empty(),
        "a do-nothing run passed:\n{}",
        vacuous.join("\n\n")
    );
}

/// The benchmark: every task against real models. Needs credentials, costs
/// money, and is stochastic, so it only runs when asked:
///
/// ```text
/// just eval anthropic/<model>,ollama/<model>
/// ```
///
/// `MERMAID_EVAL_MODELS` (comma-separated, required), `MERMAID_EVAL_TASKS`
/// (comma-separated ids; default all), `MERMAID_EVAL_REPEAT` (runs per task;
/// default 1), `MERMAID_EVAL_OUT` (report directory; default under the
/// system temp dir). Scores are reported, not asserted: a model failing a task
/// is a result, not a test failure.
#[test]
#[ignore = "calls real models; run with `just eval <model>`"]
fn live() {
    let models = std::env::var("MERMAID_EVAL_MODELS")
        .expect("set MERMAID_EVAL_MODELS to a comma-separated list of model ids");
    let only: Option<Vec<String>> = std::env::var("MERMAID_EVAL_TASKS")
        .ok()
        .map(|ids| ids.split(',').map(|s| s.trim().to_string()).collect());
    let repeat: usize = std::env::var("MERMAID_EVAL_REPEAT")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1)
        .max(1);
    let out = std::env::var_os("MERMAID_EVAL_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::harness::test_sandbox("mermaid-evals"));
    std::fs::create_dir_all(&out).expect("create the report directory");

    let tasks: Vec<_> = tasks()
        .into_iter()
        .filter(|t| !t.spec.offline_only)
        .filter(|t| only.as_ref().is_none_or(|ids| ids.contains(&t.id)))
        .collect();
    let mut runs = Vec::new();
    for model in models.split(',').map(str::trim).filter(|m| !m.is_empty()) {
        for task in &tasks {
            for attempt in 1..=repeat {
                eprintln!("eval: {model} · {} · run {attempt}/{repeat}", task.id);
                let run = run_task(task, &Target::Live(model));
                eprintln!(
                    "eval:   {} in {:.0}s ({})",
                    if run.passed() { "pass" } else { "FAIL" },
                    run.seconds,
                    run.sandbox.display()
                );
                runs.push(run);
            }
        }
    }

    let markdown = report(&runs);
    let json: Vec<serde_json::Value> = runs
        .iter()
        .map(|run| {
            serde_json::json!({
                "model": run.model,
                "task": run.task,
                "passed": run.passed(),
                "failed_checks": run.checks.iter().filter(|c| c.failure.is_some()).map(|c| &c.check).collect::<Vec<_>>(),
                "harness_error": run.harness_error,
                "turns": run.turns,
                "tokens": run.tokens,
                "tool_calls": run.tool_calls,
                "edits": run.edits,
                "fuzzy_edits": run.fuzzy_edits,
                "seconds": run.seconds,
                "artifacts": run.sandbox,
            })
        })
        .collect();
    std::fs::write(out.join("report.md"), &markdown).expect("write report.md");
    std::fs::write(
        out.join("runs.json"),
        serde_json::to_string_pretty(&json).expect("serialize runs"),
    )
    .expect("write runs.json");
    println!("{markdown}\nReport written to {}", out.display());
}

#[test]
fn a_go_ahead_reaches_the_classifier_with_the_goal_before_it() {
    // `commit-after-go-ahead` through the real binary: the goal is stated in
    // one run and the commit happens in the next, after a bare "Yes, go
    // ahead." The classifier's request must still carry the original ask and
    // the plan the user approved, or the task only passes by luck.
    let task = tasks()
        .into_iter()
        .find(|t| t.id == "commit-after-go-ahead")
        .expect("the commit-after-go-ahead task");
    let mock = MockProvider::start(task.reference().script());
    let run = run_task(&task, &Target::Mock(&mock));
    assert!(run.passed(), "{}", run.explain());

    // The classifier's call is the one that offers no tools.
    let vets: Vec<String> = mock
        .requests()
        .iter()
        .filter(|r| r["tools"].as_array().is_none_or(Vec::is_empty))
        .map(|r| r["messages"].to_string())
        .collect();
    assert_eq!(vets.len(), 1, "one borderline action, one vet");
    let vet = &vets[0];
    for expected in [
        "commit the fix with git",
        "Yes, go ahead.",
        "Shall I go ahead?",
    ] {
        assert!(
            vet.contains(expected),
            "the classifier never saw {expected:?}:\n{vet}"
        );
    }
    let _ = std::fs::remove_dir_all(&run.sandbox);
}
