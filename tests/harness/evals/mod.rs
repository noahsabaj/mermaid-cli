//! The behavioural eval suite: a handful of fixed tasks, scored by outcome.
//!
//! Each task under `evals/tasks/<id>/` is data: a fixture project to start
//! from, a prompt, and checks that look only at what happened — does
//! `cargo test` pass now, did the tests stay untouched, does the answer name
//! the right port. Nothing asserts on wording, in the prompt or in the reply.
//! That is the point of the suite: a paragraph of coaching can be deleted and
//! the evals say whether anything got worse, where a phrase test would only
//! say the paragraph is gone.
//!
//! Every run goes through the real binary (`mermaid run --format ndjson`) in a
//! throwaway copy of the fixture, and is scored from what the run left behind
//! plus its event stream. Two tiers share that path:
//!
//!   * **Offline** (`tests/it/evals.rs`, every CI run): the model is
//!     [`mock_provider::MockProvider`] replaying the task's `reference.toml`, a
//!     known-good solution. This cannot measure a model. It proves each task's
//!     checks can pass, that a do-nothing run fails them (so no check is
//!     vacuous), and that the path from model output to changed files works.
//!   * **Live** (`just eval <model>`, on demand): the same tasks against real
//!     models, with no code change for a new one. Run it when a model
//!     generation lands; if scores rise, the harness is scaling with the
//!     model, and if deleting a coaching paragraph leaves them flat, it can go.
//!
//! The report also counts edits that only applied through the fuzzy patch
//! matcher (`fuzzy` on `tool_finished` lines). When that rate sits near zero
//! for current models, the matcher is safe to drop.

pub mod mock_provider;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use mock_provider::{MockProvider, MockTurn};

/// The model id the offline tier runs under. The provider half names the
/// `[providers.evalmock]` entry [`isolate`] writes.
pub const MOCK_MODEL: &str = "evalmock/reference";

/// A task, loaded from `evals/tasks/<id>/task.toml`.
#[derive(Debug)]
pub struct Task {
    pub id: String,
    pub dir: PathBuf,
    pub spec: TaskSpec,
}

/// The on-disk shape of `task.toml`. Unknown keys are an error, so a typo in
/// a check cannot quietly turn it into no check at all.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    /// Directory under `evals/fixtures/` the run starts from.
    pub fixture: String,
    pub prompt: String,
    /// Skipped by the live tier: the task is about the mock provider's
    /// behaviour, not the model's.
    #[serde(default)]
    pub offline_only: bool,
    /// Extra top-level `mermaid` flags, e.g. `["--reasoning", "high"]`.
    #[serde(default)]
    pub args: Vec<String>,
    /// JSON Schema file, relative to the task directory, passed as
    /// `--output-schema`.
    #[serde(default)]
    pub output_schema: Option<String>,
    #[serde(rename = "check")]
    pub checks: Vec<Check>,
}

/// One outcome a run must produce.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Check {
    /// Run a command in the project afterwards. It must exit 0 and, when
    /// `stdout_contains` is set, print that text.
    Command {
        run: Vec<String>,
        #[serde(default)]
        stdout_contains: Option<String>,
    },
    /// These paths (files or directories, `.` for the whole project) must be
    /// byte-identical to the fixture.
    Unchanged { paths: Vec<String> },
    /// The final answer must contain at least one of these, ignoring case.
    AnswerContains { any: Vec<String> },
    /// Facts from the run's `result` line.
    Result {
        /// Whether `structured_output` must be present (`true`) or absent.
        #[serde(default)]
        structured_output: Option<bool>,
        /// Some reported error must contain this text.
        #[serde(default)]
        error_contains: Option<String>,
    },
    /// Offline only: some request to the provider carried each of these
    /// top-level fields.
    RequestSent { fields: Vec<String> },
}

/// A known-good solution, from `reference.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    /// How many of the reference's edits apply only fuzzily. The offline tier
    /// checks the event stream reports exactly this many.
    #[serde(default)]
    pub fuzzy_edits: usize,
    #[serde(rename = "turn")]
    turns: Vec<ReferenceTurn>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ReferenceTurn {
    Say(SayTurn),
    Tool(ToolCallSpec),
    Calls(CallsTurn),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SayTurn {
    say: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallSpec {
    tool: String,
    args: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CallsTurn {
    calls: Vec<ToolCallSpec>,
}

impl Reference {
    /// The reference as a mock provider script.
    #[must_use]
    pub fn script(&self) -> Vec<MockTurn> {
        self.turns
            .iter()
            .map(|turn| match turn {
                ReferenceTurn::Say(say) => MockTurn::Say(say.say.clone()),
                ReferenceTurn::Tool(call) => {
                    MockTurn::Tools(vec![(call.tool.clone(), call.args.clone())])
                },
                ReferenceTurn::Calls(turn) => MockTurn::Tools(
                    turn.calls
                        .iter()
                        .map(|call| (call.tool.clone(), call.args.clone()))
                        .collect(),
                ),
            })
            .collect()
    }
}

fn evals_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("evals")
}

/// Every task, sorted by id.
#[must_use]
pub fn tasks() -> Vec<Task> {
    let root = evals_dir().join("tasks");
    let mut tasks: Vec<Task> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("reading {}: {e}", root.display()))
        .map(|entry| entry.expect("a task directory entry").path())
        .filter(|dir| dir.join("task.toml").is_file())
        .map(|dir| {
            let path = dir.join("task.toml");
            let text = std::fs::read_to_string(&path).expect("reading task.toml");
            let spec: TaskSpec = toml::from_str(&text)
                .unwrap_or_else(|e| panic!("{} does not parse: {e}", path.display()));
            let id = dir
                .file_name()
                .and_then(|n| n.to_str())
                .expect("a task directory name")
                .to_string();
            Task { id, dir, spec }
        })
        .collect();
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    tasks
}

impl Task {
    /// The task's `reference.toml`.
    #[must_use]
    pub fn reference(&self) -> Reference {
        let path = self.dir.join("reference.toml");
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        toml::from_str(&text).unwrap_or_else(|e| panic!("{} does not parse: {e}", path.display()))
    }

    #[must_use]
    pub fn fixture(&self) -> PathBuf {
        evals_dir().join("fixtures").join(&self.spec.fixture)
    }
}

/// What a run is pointed at.
pub enum Target<'a> {
    /// The offline tier: a scripted endpoint, fully sandboxed config.
    Mock(&'a MockProvider),
    /// The live tier: a real model id, resolved through the user's own config
    /// and credentials.
    Live(&'a str),
}

impl Target<'_> {
    fn model(&self) -> &str {
        match self {
            Self::Mock(_) => MOCK_MODEL,
            Self::Live(model) => model,
        }
    }
}

/// One check's verdict.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub check: String,
    pub failure: Option<String>,
}

/// Everything one run of one task produced.
#[derive(Debug, Clone)]
pub struct Run {
    pub task: String,
    pub model: String,
    pub checks: Vec<CheckResult>,
    /// The run never produced a `result` line (crash, timeout).
    pub harness_error: Option<String>,
    pub response: String,
    pub errors: Vec<String>,
    pub turns: usize,
    pub tokens: u64,
    pub tool_calls: usize,
    /// Successful `edit_file` / `apply_patch` calls.
    pub edits: usize,
    /// Those edits that only applied through the fuzzy matcher.
    pub fuzzy_edits: usize,
    pub seconds: f64,
    /// The run's working copy, event stream and stderr, for a post-mortem.
    pub sandbox: PathBuf,
}

impl Run {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.harness_error.is_none() && self.checks.iter().all(|c| c.failure.is_none())
    }

    /// A readable account of what failed, for assertion messages.
    #[must_use]
    pub fn explain(&self) -> String {
        let mut out = format!("{} on {}", self.task, self.model);
        if let Some(error) = &self.harness_error {
            let _ = write!(out, "\n  run failed: {error}");
        }
        for check in &self.checks {
            match &check.failure {
                None => {
                    let _ = write!(out, "\n  pass  {}", check.check);
                },
                Some(why) => {
                    let _ = write!(out, "\n  FAIL  {}: {why}", check.check);
                },
            }
        }
        if !self.errors.is_empty() {
            let _ = write!(out, "\n  run errors: {:?}", self.errors);
        }
        let _ = write!(out, "\n  answer: {:?}", self.response);
        let _ = write!(out, "\n  artifacts: {}", self.sandbox.display());
        out
    }
}

/// How long one run may take before it is killed and scored as failed.
#[must_use]
pub fn run_timeout(target: &Target<'_>) -> Duration {
    match target {
        Target::Mock(_) => Duration::from_secs(120),
        Target::Live(_) => Duration::from_secs(15 * 60),
    }
}

/// How long a check command may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(300);

/// Run `task` once against `target` and score it.
#[must_use]
pub fn run_task(task: &Task, target: &Target<'_>) -> Run {
    let sandbox = crate::harness::test_sandbox(&format!("mermaid-eval-{}", task.id));
    let project = sandbox.join("project");
    copy_dir(&task.fixture(), &project);
    init_git(&project);
    let before = snapshot(&project);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mermaid"));
    cmd.args(["--model", target.model()])
        .args(&task.spec.args)
        // Autonomous, like any unattended run: nobody is there to approve a
        // write. The working copy is a throwaway.
        .args(["-c", "safety.mode=full_access"])
        .args(["-c", "safety.checkpoint_on_mutation=false"])
        .args(["run", "--format", "ndjson"]);
    if let Some(schema) = &task.spec.output_schema {
        cmd.arg("--output-schema").arg(task.dir.join(schema));
    }
    cmd.arg(&task.spec.prompt)
        .current_dir(&project)
        // Inside the project, where `--confine-fs` would allow it, and out of
        // the snapshot. The model's own `cargo` runs and the checks share it.
        .env("CARGO_TARGET_DIR", project.join("target"))
        .env_remove("CARGO_BUILD_TARGET_DIR");
    if let Target::Mock(mock) = target {
        isolate(&mut cmd, &sandbox, mock);
    }

    let started = Instant::now();
    let output = run_with_timeout(cmd, run_timeout(target));
    let seconds = started.elapsed().as_secs_f64();
    let _ = std::fs::write(sandbox.join("events.ndjson"), &output.stdout);
    let _ = std::fs::write(sandbox.join("stderr.txt"), &output.stderr);

    let mut run = Run {
        task: task.id.clone(),
        model: target.model().to_string(),
        checks: Vec::new(),
        harness_error: output.timed_out.then(|| "timed out".to_string()),
        response: String::new(),
        errors: Vec::new(),
        turns: 0,
        tokens: 0,
        tool_calls: 0,
        edits: 0,
        fuzzy_edits: 0,
        seconds,
        sandbox: sandbox.clone(),
    };
    let result = read_events(&output.stdout, &mut run);
    match &result {
        Some(result) => {
            run.response = result["response"].as_str().unwrap_or("").to_string();
            run.tokens = result["total_tokens"].as_u64().unwrap_or(0);
            run.errors = result["errors"]
                .as_array()
                .map(|errors| {
                    errors
                        .iter()
                        .filter_map(|e| e.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            // A model that never answered once (bad key, unknown id, server
            // down) has not been scored, and must not read as a model scoring
            // zero.
            if run.turns == 0 && !run.errors.is_empty() {
                run.harness_error = Some(format!("the model never answered: {}", run.errors[0]));
            }
        },
        None if run.harness_error.is_none() => {
            run.harness_error = Some(format!(
                "no result line (exit {:?}); stderr tail:\n{}",
                output.status,
                tail(&String::from_utf8_lossy(&output.stderr), 15)
            ));
        },
        None => {},
    }

    save_diff(&project, &sandbox);
    let after = snapshot(&project);
    for check in &task.spec.checks {
        let (label, failure) = score(
            check,
            &project,
            &before,
            &after,
            &run,
            result.as_ref(),
            target,
        );
        run.checks.push(CheckResult {
            check: label,
            failure,
        });
    }
    run
}

/// Tally the event stream into `run`, and return its `result` line.
fn read_events(stdout: &[u8], run: &mut Run) -> Option<Value> {
    let mut result = None;
    for line in String::from_utf8_lossy(stdout).lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event["type"].as_str().unwrap_or("") {
            "turn_done" => run.turns += 1,
            "tool_finished" => {
                run.tool_calls += 1;
                // `edit_file` reports as `apply_patch` too: both are edits.
                if event["name"] == "apply_patch" && event["status"] == "success" {
                    run.edits += 1;
                    if event["fuzzy"] == true {
                        run.fuzzy_edits += 1;
                    }
                }
            },
            "result" => result = Some(event),
            _ => {},
        }
    }
    result
}

fn score(
    check: &Check,
    project: &Path,
    before: &BTreeMap<String, Vec<u8>>,
    after: &BTreeMap<String, Vec<u8>>,
    run: &Run,
    result: Option<&Value>,
    target: &Target<'_>,
) -> (String, Option<String>) {
    match check {
        Check::Command {
            run: argv,
            stdout_contains,
        } => score_command(argv, stdout_contains.as_deref(), project),
        Check::Unchanged { paths } => {
            let label = format!("{} unchanged", paths.join(", "));
            let covered = |file: &str| {
                paths.iter().any(|p| {
                    let p = p.trim_end_matches('/');
                    p == "." || file == p || file.starts_with(&format!("{p}/"))
                })
            };
            let changed: Vec<&String> = before
                .keys()
                .chain(after.keys().filter(|k| !before.contains_key(*k)))
                .filter(|file| covered(file) && before.get(*file) != after.get(*file))
                .collect();
            let failure = (!changed.is_empty()).then(|| format!("changed: {changed:?}"));
            (label, failure)
        },
        Check::AnswerContains { any } => {
            let label = format!("answer mentions one of {any:?}");
            let answer = run.response.to_lowercase();
            let failure = (!any.iter().any(|want| answer.contains(&want.to_lowercase())))
                .then(|| "not in the answer".to_string());
            (label, failure)
        },
        Check::Result {
            structured_output,
            error_contains,
        } => {
            let mut label = Vec::new();
            let mut failures = Vec::new();
            if let Some(want) = structured_output {
                label.push(if *want {
                    "structured output present"
                } else {
                    "structured output absent"
                });
                let present = result.is_some_and(|r| r.get("structured_output").is_some());
                if present != *want {
                    failures.push(format!("structured_output present = {present}"));
                }
            }
            if let Some(want) = error_contains {
                label.push("the expected error is reported");
                if !run.errors.iter().any(|e| e.contains(want.as_str())) {
                    failures.push(format!("no error mentions {want:?}"));
                }
            }
            (
                label.join(", "),
                (!failures.is_empty()).then(|| failures.join("; ")),
            )
        },
        Check::RequestSent { fields } => {
            let label = format!("request carried {fields:?}");
            let Target::Mock(mock) = target else {
                return (label, Some("only meaningful offline".to_string()));
            };
            let requests = mock.requests();
            let missing: Vec<&String> = fields
                .iter()
                .filter(|field| !requests.iter().any(|r| r.get(field.as_str()).is_some()))
                .collect();
            let failure = (!missing.is_empty()).then(|| format!("never sent: {missing:?}"));
            (label, failure)
        },
    }
}

fn score_command(
    argv: &[String],
    stdout_contains: Option<&str>,
    project: &Path,
) -> (String, Option<String>) {
    let mut label = format!("`{}` succeeds", argv.join(" "));
    if let Some(want) = stdout_contains {
        let _ = write!(label, " and prints {want:?}");
    }
    let Some((program, args)) = argv.split_first() else {
        return (label, Some("empty command".to_string()));
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(project)
        .env("CARGO_TARGET_DIR", project.join("target"))
        .env_remove("CARGO_BUILD_TARGET_DIR")
        // A failing check's output lands in the report; a backtrace there
        // buries the name of the test that failed.
        .env("RUST_BACKTRACE", "0");
    let output = run_with_timeout(cmd, CHECK_TIMEOUT);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let failure = if output.timed_out {
        Some("timed out".to_string())
    } else if output.status != Some(0) {
        Some(format!(
            "exit {:?}\n{}\n{}",
            output.status,
            tail(&stdout, 10),
            tail(&String::from_utf8_lossy(&output.stderr), 15)
        ))
    } else {
        stdout_contains
            .filter(|want| !stdout.contains(want))
            .map(|want| format!("stdout lacks {want:?}; got {:?}", stdout.trim()))
    };
    (label, failure)
}

/// Point the binary at `mock` with a config of its own, so nothing reads or
/// writes the developer's real config, data dir or credentials.
fn isolate(cmd: &mut Command, sandbox: &Path, mock: &MockProvider) {
    let home = sandbox.join("home");
    let config_dir = sandbox.join("config").join("mermaid");
    let data_dir = sandbox.join("data").join("mermaid");
    for dir in [&home, &config_dir, &data_dir] {
        std::fs::create_dir_all(dir).expect("create sandbox dir");
    }
    // `openai-effort`: the endpoint is sent `reasoning_effort`, which the mock
    // accepts and ignores.
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[providers.evalmock]\nbase_url = \"{}\"\napi_key_env = \"MERMAID_EVAL_MOCK_KEY\"\ncompat = \"openai-effort\"\n",
            mock.base_url()
        ),
    )
    .expect("write the sandbox config");
    cmd.env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("MERMAID_CONFIG_DIR", &config_dir)
        .env("MERMAID_DATA_DIR", &data_dir)
        .env("MERMAID_EVAL_MOCK_KEY", "unused")
        .env("RUST_BACKTRACE", "0");
}

struct Output {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

/// Run `cmd` to completion, killing it after `limit`. Both pipes are drained
/// on their own threads, so a chatty child cannot block on a full pipe.
fn run_with_timeout(mut cmd: Command, limit: Duration) -> Output {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawning {cmd:?}: {e}"));
    let drain = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    };
    let stdout = drain(Box::new(child.stdout.take().expect("stdout pipe")));
    let stderr = drain(Box::new(child.stderr.take().expect("stderr pipe")));
    let deadline = Instant::now() + limit;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            },
            _ => {
                timed_out = true;
                let _ = child.kill();
                break child.wait().ok().and_then(|s| s.code());
            },
        }
    };
    Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
        timed_out,
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create fixture copy");
    for entry in std::fs::read_dir(from).unwrap_or_else(|e| panic!("{}: {e}", from.display())) {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("fixture entry type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

/// Commit the fixture, so the model sees an ordinary repository and the run
/// leaves a `git diff` behind. Best-effort: without git the run still works.
fn init_git(project: &Path) {
    let git = |args: &[&str]| {
        Command::new("git")
            .args([
                "-c",
                "user.name=mermaid-eval",
                "-c",
                "user.email=eval@example.invalid",
            ])
            .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
            .args(args)
            .current_dir(project)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    let _ = git(&["init", "-q"]) && git(&["add", "-A"]) && git(&["commit", "-qm", "fixture"]);
}

fn save_diff(project: &Path, sandbox: &Path) {
    if let Ok(out) = Command::new("git")
        .args(["-c", "core.autocrlf=false", "diff"])
        .current_dir(project)
        .output()
    {
        let _ = std::fs::write(sandbox.join("diff.patch"), out.stdout);
    }
}

/// Every file in the project that a run could meaningfully change, keyed by
/// `/`-separated relative path. Git's store, Mermaid's own session store and
/// the build tree are not the project's content.
fn snapshot(project: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .expect("under the project")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            if matches!(rel.as_str(), ".git" | ".mermaid" | "target") {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.insert(rel, bytes);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(project, project, &mut out);
    out
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

// ── The live report ─────────────────────────────────────────────────────

/// Score table for a set of live runs, as Markdown.
#[must_use]
pub fn report(runs: &[Run]) -> String {
    let mut models: BTreeMap<&str, Vec<&Run>> = BTreeMap::new();
    for run in runs {
        models.entry(run.model.as_str()).or_default().push(run);
    }
    let mut out = String::from("# Mermaid behavioural evals\n");
    for (model, runs) in models {
        let passed = runs.iter().filter(|r| r.passed()).count();
        let unscored = runs.iter().filter(|r| r.harness_error.is_some()).count();
        let edits: usize = runs.iter().map(|r| r.edits).sum();
        let fuzzy: usize = runs.iter().map(|r| r.fuzzy_edits).sum();
        let _ = write!(
            out,
            "\n## {model}\n\n{passed}/{} runs passed{}. Fuzzy edits: {fuzzy} of {edits}{}.\n\n",
            runs.len(),
            if unscored == 0 {
                String::new()
            } else {
                format!(" ({unscored} never finished; see Failures)")
            },
            if edits == 0 {
                String::new()
            } else {
                format!(" ({:.0}%)", 100.0 * fuzzy as f64 / edits as f64)
            }
        );
        out.push_str("| task | passed | turns | tokens | tool calls | fuzzy edits | seconds |\n");
        out.push_str("|---|---|---|---|---|---|---|\n");
        let mut tasks: BTreeMap<&str, Vec<&Run>> = BTreeMap::new();
        for run in &runs {
            tasks.entry(run.task.as_str()).or_default().push(run);
        }
        for (task, runs) in tasks {
            let n = runs.len() as f64;
            let mean = |f: &dyn Fn(&Run) -> f64| runs.iter().map(|r| f(r)).sum::<f64>() / n;
            let _ = writeln!(
                out,
                "| {task} | {}/{} | {:.1} | {:.0} | {:.1} | {}/{} | {:.0} |",
                runs.iter().filter(|r| r.passed()).count(),
                runs.len(),
                mean(&|r| r.turns as f64),
                mean(&|r| r.tokens as f64),
                mean(&|r| r.tool_calls as f64),
                runs.iter().map(|r| r.fuzzy_edits).sum::<usize>(),
                runs.iter().map(|r| r.edits).sum::<usize>(),
                mean(&|r| r.seconds),
            );
        }
        let failures: Vec<&&Run> = runs.iter().filter(|r| !r.passed()).collect();
        if !failures.is_empty() {
            out.push_str("\nFailures:\n\n");
            for run in failures {
                let _ = writeln!(out, "```\n{}\n```", run.explain());
            }
        }
    }
    out
}
