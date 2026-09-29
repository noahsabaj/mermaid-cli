//! The behavioural eval suite. See `tests/harness/evals/mod.rs` for the
//! design and `evals/README.md` for how to run it against a model.
//!
//! The offline tests below cannot score a model, and do not try to. They keep
//! the suite itself honest on every CI run: each task's checks are passable
//! (the reference solution passes them), none is vacuous (doing nothing fails
//! them), and the path from model output to changed files works end to end
//! through the real binary. The `live` test is the benchmark, and runs only
//! when asked.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::harness::evals::mock_provider::{MockProvider, MockTurn};
use crate::harness::evals::report::{
    HistoryEntry, Provenance, append_history, default_history_path, guidance_comparison,
    history_entries, history_section, read_history, report, verdict,
};
use crate::harness::evals::{Check, Guidance, Run, Target, run_task, run_task_with, tasks};

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

#[test]
fn the_guidance_switch_reaches_the_prompt() {
    // Pinning the pack on or off must change what the model is sent, or an
    // on/off comparison compares nothing. Measured by size, not wording: the
    // pack is extra prompt, and which words it holds is not this test's
    // business.
    let task = tasks()
        .into_iter()
        .find(|t| t.id == "answer-repo-question")
        .expect("the answer-repo-question task");
    let system_prompt_len = |guidance| {
        let mock = MockProvider::start(task.reference().script());
        let run = run_task_with(&task, &Target::Mock(&mock), guidance);
        assert!(run.passed(), "{}", run.explain());
        assert_eq!(run.guidance, guidance);
        let _ = std::fs::remove_dir_all(&run.sandbox);
        let first = mock.requests().into_iter().next().expect("a request");
        first["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter(|m| m["role"] == "system")
            .map(|m| m["content"].to_string().len())
            .sum::<usize>()
    };
    let on = system_prompt_len(Guidance::On);
    let off = system_prompt_len(Guidance::Off);
    assert!(
        on > off,
        "the pack pinned on sent {on} bytes of system prompt, off sent {off}"
    );
}

/// A scored run with nothing but the fields the report reads.
fn fake_run(model: &str, task: &str, guidance: Guidance, passed: bool) -> Run {
    Run {
        task: task.to_string(),
        model: model.to_string(),
        guidance,
        pack: match guidance {
            Guidance::On => Some(true),
            Guidance::Off => Some(false),
            Guidance::Default => Some(false),
        },
        default_pack: Some(false),
        checks: vec![crate::harness::evals::CheckResult {
            check: "outcome".to_string(),
            failure: (!passed).then(|| "wrong".to_string()),
        }],
        harness_error: None,
        response: String::new(),
        errors: Vec::new(),
        turns: 4,
        tokens: if guidance == Guidance::On { 2000 } else { 1000 },
        tool_calls: 3,
        edits: 1,
        fuzzy_edits: 0,
        seconds: 10.0,
        sandbox: PathBuf::new(),
    }
}

#[test]
fn the_verdict_says_whether_the_default_picked_right() {
    // on (passed, scored), off (passed, scored), what the default picks.
    assert_eq!(
        verdict((3, 3), (1, 3), Some(true)),
        "the pack helps; the default is right"
    );
    assert_eq!(
        verdict((3, 3), (1, 3), Some(false)),
        "the pack helps; the default is wrong for this model"
    );
    assert_eq!(
        verdict((1, 3), (3, 3), Some(true)),
        "the pack hurts; the default is wrong for this model"
    );
    assert_eq!(
        verdict((1, 3), (3, 3), Some(false)),
        "the pack hurts; the default is right"
    );
    assert_eq!(
        verdict((2, 3), (2, 3), Some(true)),
        "no measured difference; the default turns it on, so it only costs tokens here"
    );
    assert_eq!(verdict((2, 3), (0, 0), Some(true)), "not scored");
    assert_eq!(verdict((2, 3), (1, 3), None), "the pack helps");
}

#[test]
fn the_report_compares_the_pack_on_and_off() {
    let runs = vec![
        fake_run("hosted/strong", "a", Guidance::On, true),
        fake_run("hosted/strong", "b", Guidance::On, true),
        fake_run("hosted/strong", "a", Guidance::Off, true),
        fake_run("hosted/strong", "b", Guidance::Off, false),
        // Only one setting: no comparison row for it.
        fake_run("hosted/other", "a", Guidance::Default, true),
    ];
    let comparison = guidance_comparison(&runs).expect("a comparison");
    assert!(
        comparison.contains(
            "| hosted/strong | off | 2/2 | 1/2 | 2000 / 1000 | the pack helps; the default is wrong for this model |"
        ),
        "{comparison}"
    );
    assert!(
        comparison.contains("| hosted/strong | b | 1/1 | 0/1 | +100 pts |"),
        "{comparison}"
    );
    assert!(!comparison.contains("hosted/other"), "{comparison}");
    // One run per side is not enough to act on, and the report says so.
    assert!(comparison.contains("MERMAID_EVAL_REPEAT=3"), "{comparison}");

    let markdown = report(&runs);
    assert!(
        markdown.contains("## hosted/strong · guidance pack on"),
        "{markdown}"
    );
    assert!(
        markdown.contains("## hosted/strong · guidance pack off"),
        "{markdown}"
    );
    assert!(
        markdown.contains("## hosted/other · default config (pack off)"),
        "{markdown}"
    );
    assert!(
        markdown.contains("## Guidance pack: on versus off"),
        "{markdown}"
    );

    // Nothing to compare: no comparison section.
    assert!(guidance_comparison(&runs[4..]).is_none());
}

#[test]
fn history_round_trips_and_trends_on_common_tasks() {
    let dir = crate::harness::test_sandbox("mermaid-eval-history");
    let path = dir.join("results").join("history.jsonl");
    let provenance = |date: &str, version: &str| Provenance {
        date: date.to_string(),
        mermaid: version.to_string(),
        commit: Some("abc123".to_string()),
        dirty: false,
        label: None,
    };

    // An older release, which ran a task this run does not.
    let older = vec![
        fake_run("hosted/model", "a", Guidance::Off, false),
        fake_run("hosted/model", "gone", Guidance::Off, true),
    ];
    let older = history_entries(&older, &provenance("2026-01-01", "0.27.0"));
    append_history(&path, &older).expect("append");

    let mut unscored = fake_run("hosted/broken", "a", Guidance::Off, false);
    unscored.harness_error = Some("the model never answered".to_string());
    let now = vec![
        fake_run("hosted/model", "a", Guidance::Off, true),
        fake_run("hosted/model", "b", Guidance::Off, true),
        unscored,
    ];
    let current = history_entries(&now, &provenance("2026-09-29", "0.28.0"));
    // A model that never answered records nothing, rather than a zero.
    assert_eq!(current.len(), 1, "{current:?}");
    let past = read_history(&path);
    assert_eq!(past, older);

    let section = history_section(&past, &current);
    assert!(
        section.contains("| hosted/model | off | 2026-09-29 | 0.28.0 `abc123` | 2/2 (100%) |"),
        "{section}"
    );
    // The older entry is scored on task `a` only, and says so.
    assert!(
        section.contains("| 2026-01-01 | 0.27.0 `abc123` | 0/1 (0%), 1/2 tasks |"),
        "{section}"
    );

    append_history(&path, &current).expect("append");
    let all: Vec<HistoryEntry> = read_history(&path);
    assert_eq!(all.len(), 2);
    assert_eq!(all[1].tasks["a"].passed, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The benchmark: every task against real models. Needs credentials, costs
/// money, and is stochastic, so it only runs when asked:
///
/// ```text
/// just eval anthropic/<model>,ollama/<model>
/// just eval-guidance anthropic/<model>
/// ```
///
/// Scores are reported, not asserted: a model failing a task is a result, not
/// a test failure. `evals/README.md` lists the variables.
#[test]
#[ignore = "calls real models; run with `just eval <model>`"]
fn live() {
    let opts = LiveOptions::from_env();
    std::fs::create_dir_all(&opts.out).expect("create the report directory");
    let tasks: Vec<_> = tasks()
        .into_iter()
        .filter(|t| !t.spec.offline_only)
        .filter(|t| opts.only.as_ref().is_none_or(|ids| ids.contains(&t.id)))
        .collect();
    assert!(!tasks.is_empty(), "MERMAID_EVAL_TASKS matched no live task");

    let runs = run_live(&opts, &tasks);
    let mut markdown = report(&runs);
    let entries = history_entries(&runs, &Provenance::current(opts.label.clone()));
    if let Some(path) = &opts.history {
        let past = read_history(path);
        markdown.push('\n');
        markdown.push_str(&history_section(&past, &entries));
        match append_history(path, &entries) {
            Ok(()) => {
                let _ = write!(
                    markdown,
                    "\nAppended {} entr{} to {}. Commit it to keep the trend.\n",
                    entries.len(),
                    if entries.len() == 1 { "y" } else { "ies" },
                    path.display()
                );
            },
            Err(e) => eprintln!("eval: could not append to {}: {e}", path.display()),
        }
    }

    std::fs::write(opts.out.join("report.md"), &markdown).expect("write report.md");
    std::fs::write(
        opts.out.join("runs.json"),
        serde_json::to_string_pretty(&runs_json(&runs)).expect("serialize runs"),
    )
    .expect("write runs.json");
    println!("{markdown}\nReport written to {}", opts.out.display());
}

/// The live run's settings, from `MERMAID_EVAL_*` (see `evals/README.md`).
struct LiveOptions {
    models: Vec<String>,
    only: Option<Vec<String>>,
    repeat: usize,
    jobs: usize,
    settings: Vec<Guidance>,
    history: Option<PathBuf>,
    label: Option<String>,
    out: PathBuf,
}

impl LiveOptions {
    fn from_env() -> Self {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let list = |text: String| -> Vec<String> {
            text.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let count = |name: &str| {
            env(name)
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(1)
                .max(1)
        };
        let settings = env("MERMAID_EVAL_GUIDANCE").map_or_else(
            || vec![Guidance::Default],
            |text| {
                list(text)
                    .iter()
                    .map(|s| {
                        Guidance::parse(s).unwrap_or_else(|| {
                            panic!("MERMAID_EVAL_GUIDANCE: {s:?} is not default, on or off")
                        })
                    })
                    .collect()
            },
        );
        Self {
            models: list(
                env("MERMAID_EVAL_MODELS")
                    .expect("set MERMAID_EVAL_MODELS to a comma-separated list of model ids"),
            ),
            only: env("MERMAID_EVAL_TASKS").map(list),
            repeat: count("MERMAID_EVAL_REPEAT"),
            jobs: count("MERMAID_EVAL_JOBS"),
            settings,
            history: match env("MERMAID_EVAL_HISTORY") {
                Some(off) if off.eq_ignore_ascii_case("off") => None,
                Some(path) => Some(PathBuf::from(path)),
                None => Some(default_history_path()),
            },
            label: env("MERMAID_EVAL_LABEL"),
            out: env("MERMAID_EVAL_OUT").map_or_else(
                || crate::harness::test_sandbox("mermaid-evals"),
                PathBuf::from,
            ),
        }
    }
}

/// Every model, guidance setting, task and repeat, `opts.jobs` at a time,
/// sorted so the report reads the same whatever order they finished in.
fn run_live(opts: &LiveOptions, tasks: &[crate::harness::evals::Task]) -> Vec<Run> {
    let mut queue = VecDeque::new();
    for model in &opts.models {
        for &guidance in &opts.settings {
            for task in tasks {
                for attempt in 1..=opts.repeat {
                    queue.push_back((model.as_str(), guidance, task, attempt));
                }
            }
        }
    }
    let total = queue.len();
    let queue = Mutex::new(queue);
    let done = Mutex::new(Vec::with_capacity(total));
    std::thread::scope(|scope| {
        for _ in 0..opts.jobs.min(total) {
            scope.spawn(|| {
                loop {
                    let Some((model, guidance, task, attempt)) =
                        queue.lock().expect("the job queue").pop_front()
                    else {
                        break;
                    };
                    let run = run_task_with(task, &Target::Live(model), guidance);
                    eprintln!(
                        "eval: {model} · pack {} · {} · run {attempt}/{}: {} in {:.0}s ({})",
                        guidance.as_str(),
                        task.id,
                        opts.repeat,
                        if run.passed() { "pass" } else { "FAIL" },
                        run.seconds,
                        run.sandbox.display()
                    );
                    done.lock().expect("the results").push(run);
                }
            });
        }
    });
    let mut runs = done.into_inner().expect("the results");
    runs.sort_by(|a, b| (&a.model, a.guidance, &a.task).cmp(&(&b.model, b.guidance, &b.task)));
    runs
}

fn runs_json(runs: &[Run]) -> Vec<serde_json::Value> {
    runs.iter()
        .map(|run| {
            serde_json::json!({
                "model": run.model,
                "guidance": run.guidance.as_str(),
                "pack": run.pack,
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
        .collect()
}
