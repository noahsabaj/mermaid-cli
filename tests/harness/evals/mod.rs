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
pub mod report;
#[cfg(target_os = "linux")]
pub mod screen;

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
    /// Later user messages in the same conversation, each sent as its own
    /// `mermaid --continue run` after the previous run ends. How a task
    /// expresses "yes, go ahead".
    #[serde(default)]
    pub followups: Vec<String>,
    /// The safety mode the runs use. Defaults to `full_access`: nobody is
    /// there to approve a write. `auto` puts borderline actions in front of
    /// the safety classifier, which then decides the outcome.
    #[serde(default = "default_safety")]
    pub safety: String,
    /// A window to open on a private Xvfb display before the run, with the
    /// `computer` tool turned on (only `settings` today). Such a task runs
    /// only on Linux with Xvfb installed; elsewhere it is not scored.
    #[serde(default)]
    pub screen_app: Option<String>,
    #[serde(rename = "check")]
    pub checks: Vec<Check>,
}

fn default_safety() -> String {
    "full_access".to_string()
}

/// One outcome a run must produce.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Check {
    /// Run a command in the project afterwards. It must exit 0 and, when
    /// `stdout_contains` is set, print that text.
    ///
    /// `overlay` names a directory under the task whose files are copied
    /// into the project first: hidden inputs or tests the model never saw,
    /// so a solution tuned to the visible samples does not pass.
    ///
    /// `restore` names paths (as in `unchanged`) put back to the fixture's
    /// version first, dropping any file the run added under them. Running the
    /// original tests this way lets a model add regression tests without
    /// failing the task, where `unchanged` would fail it for the addition,
    /// while a weakened or deleted test still does not pass.
    Command {
        run: Vec<String>,
        #[serde(default)]
        stdout_contains: Option<String>,
        #[serde(default)]
        overlay: Option<String>,
        #[serde(default)]
        restore: Vec<String>,
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
    /// The task's screen app saved exactly this, ignoring surrounding
    /// whitespace. It saves outside the project, so only the app can.
    AppSaved { equals: String },
}

#[cfg(target_os = "linux")]
use screen::Screen;

/// No screen exists off Linux; this type has no values.
#[cfg(not(target_os = "linux"))]
enum Screen {}

#[cfg(not(target_os = "linux"))]
impl Screen {
    fn display(&self) -> &str {
        match *self {}
    }
    fn saved(&self) -> Option<String> {
        match *self {}
    }
}

/// Whether tasks with a `screen_app` can run here.
#[must_use]
pub fn screen_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        screen::available()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Open `task`'s screen app, if it has one.
fn open_screen(task: &Task, sandbox: &Path) -> Result<Option<Screen>, String> {
    let Some(app) = &task.spec.screen_app else {
        return Ok(None);
    };
    if !screen_available() {
        return Err("needs a screen: Linux with Xvfb installed".to_string());
    }
    #[cfg(target_os = "linux")]
    {
        Screen::start(app, &sandbox.join("screen")).map(Some)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (app, sandbox);
        unreachable!("screen_available is false off Linux")
    }
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
///
/// # Panics
///
/// When `evals/tasks` cannot be read or a `task.toml` does not parse: a
/// broken task is a broken suite, and should say which file.
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
    ///
    /// # Panics
    ///
    /// When the file is missing or does not parse.
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

/// Which guidance pack setting a run is pinned to. The pack is the coaching
/// layered onto the core prompt (`[output] guidance`); running the same model
/// with it on and then off is how the suite says whether the coaching still
/// earns its tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Guidance {
    /// Whatever the config in effect decides. For the live tier that is the
    /// user's own config, which is `auto` unless they pinned it.
    Default,
    On,
    Off,
}

impl Guidance {
    /// Parse one entry of `MERMAID_EVAL_GUIDANCE`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "default" => Some(Self::Default),
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::On => "on",
            Self::Off => "off",
        }
    }

    /// The `-c` override that pins it, if any.
    fn override_arg(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::On => Some("output.guidance=on"),
            Self::Off => Some("output.guidance=off"),
        }
    }
}

/// Whether the config in effect turns the guidance pack on for a live
/// `model`: the user's own config, as the binary under test reads it. `None`
/// when it cannot be read.
#[must_use]
pub fn default_guidance(model: &str) -> Option<bool> {
    mermaid_cli::app::load_config()
        .ok()
        .map(|config| config.guidance_pack_enabled(model))
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
    pub guidance: Guidance,
    /// Whether the guidance pack was on, when known: always for a pinned
    /// setting, and for `Default` when the config could be read.
    pub pack: Option<bool>,
    /// Whether the config in effect would turn the pack on for this model,
    /// whatever this run pinned. Live runs only.
    pub default_pack: Option<bool>,
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
        if self.guidance != Guidance::Default {
            let _ = write!(out, " (guidance pack {})", self.guidance.as_str());
        }
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
        Target::Live(_) => Duration::from_mins(15),
    }
}

/// How long a check command may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(300);

/// Run `task` once against `target` and score it, with the guidance pack as
/// the config decides.
#[must_use]
pub fn run_task(task: &Task, target: &Target<'_>) -> Run {
    run_task_with(task, target, Guidance::Default)
}

/// Run `task` once against `target` with the guidance pack pinned as
/// `guidance` says, and score it.
#[must_use]
pub fn run_task_with(task: &Task, target: &Target<'_>, guidance: Guidance) -> Run {
    // The setting is in the name so concurrent runs of one task never share
    // a sandbox.
    let sandbox =
        crate::harness::test_sandbox(&format!("mermaid-eval-{}-{}", task.id, guidance.as_str()));
    let project = sandbox.join("project");
    copy_dir(&task.fixture(), &project);
    init_git(&project);
    let before = snapshot(&project);

    let screen = open_screen(task, &sandbox);
    let started = Instant::now();
    let output = match &screen {
        Ok(screen) => run_conversation(task, target, guidance, &project, &sandbox, screen.as_ref()),
        Err(_) => Output::default(),
    };
    let seconds = started.elapsed().as_secs_f64();
    let _ = std::fs::write(sandbox.join("events.ndjson"), &output.stdout);
    let _ = std::fs::write(sandbox.join("stderr.txt"), &output.stderr);

    let default_pack = match target {
        Target::Live(model) => default_guidance(model),
        Target::Mock(_) => None,
    };
    let pack = match guidance {
        Guidance::On => Some(true),
        Guidance::Off => Some(false),
        Guidance::Default => default_pack,
    };
    let mut run = Run {
        task: task.id.clone(),
        model: target.model().to_string(),
        guidance,
        pack,
        default_pack,
        checks: Vec::new(),
        harness_error: match &screen {
            Err(e) => Some(e.clone()),
            Ok(_) => output.timed_out.then(|| "timed out".to_string()),
        },
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
    let result = read_result(&output, &mut run);

    save_diff(&project, &sandbox);
    let after = snapshot(&project);
    let saved = screen.as_ref().ok().and_then(|s| s.as_ref()?.saved());
    for check in &task.spec.checks {
        if let Check::Command {
            overlay, restore, ..
        } = check
        {
            restore_paths(&project, &before, restore);
            if let Some(overlay) = overlay {
                copy_dir(&task.dir.join(overlay), &project);
            }
        }
        let (label, failure) = score(
            check,
            &project,
            (&before, &after),
            &run,
            result.as_ref(),
            target,
            saved.as_deref(),
        );
        run.checks.push(CheckResult {
            check: label,
            failure,
        });
    }
    run
}

/// Tally `output` into `run` and return its `result` line, noting a run the
/// model never answered so it is not scored as a zero.
fn read_result(output: &Output, run: &mut Run) -> Option<Value> {
    let result = read_events(&output.stdout, run);
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
    result
}

/// Run the task's prompt, then each follow-up as a continuation of the same
/// conversation, sharing one time limit. The runs' output is concatenated and
/// scored as one.
fn run_conversation(
    task: &Task,
    target: &Target<'_>,
    guidance: Guidance,
    project: &Path,
    sandbox: &Path,
    screen: Option<&Screen>,
) -> Output {
    let prompts: Vec<&String> = std::iter::once(&task.spec.prompt)
        .chain(&task.spec.followups)
        .collect();
    let started = Instant::now();
    let mut output = Output {
        status: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
        timed_out: false,
    };
    for (i, prompt) in prompts.iter().enumerate() {
        let cmd = mermaid_command(
            task,
            target,
            guidance,
            (sandbox, project),
            screen,
            prompt,
            i > 0,
        );
        let remaining = run_timeout(target).saturating_sub(started.elapsed());
        let step = run_with_timeout(cmd, remaining);
        output.stdout.extend_from_slice(&step.stdout);
        output.stderr.extend_from_slice(&step.stderr);
        output.status = step.status;
        output.timed_out = step.timed_out;
        if step.timed_out {
            break;
        }
    }
    output
}

/// The `mermaid run` invocation for one message of `task`'s conversation.
/// `resume` continues the conversation the previous message started.
fn mermaid_command(
    task: &Task,
    target: &Target<'_>,
    guidance: Guidance,
    (sandbox, project): (&Path, &Path),
    screen: Option<&Screen>,
    prompt: &str,
    resume: bool,
) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mermaid"));
    cmd.args(["--model", target.model()]).args(&task.spec.args);
    if resume {
        cmd.arg("--continue");
    }
    // Autonomous by default, like any unattended run: nobody is there to
    // approve a write. The working copy is a throwaway.
    cmd.args(["-c", &format!("safety.mode={}", task.spec.safety)])
        .args(["-c", "safety.checkpoint_on_mutation=false"]);
    if let Some(pin) = guidance.override_arg() {
        cmd.args(["-c", pin]);
    }
    if let Some(screen) = screen {
        cmd.args(["-c", "tools.computer=true"])
            .env("DISPLAY", screen.display())
            .env_remove("WAYLAND_DISPLAY");
    }
    // An ablation: config overrides for every live run, such as
    // `tools.provider_native=false`.
    if let Target::Live(_) = target {
        for pin in live_overrides() {
            cmd.args(["-c", &pin]);
        }
    }
    cmd.args(["run", "--format", "ndjson"]);
    if let Some(schema) = &task.spec.output_schema {
        cmd.arg("--output-schema").arg(task.dir.join(schema));
    }
    cmd.arg(prompt)
        .current_dir(project)
        // Inside the project, where `--confine-fs` would allow it, and out of
        // the snapshot. The model's own `cargo` runs and the checks share it.
        .env("CARGO_TARGET_DIR", project.join("target"))
        .env_remove("CARGO_BUILD_TARGET_DIR");
    git_identity(&mut cmd);
    if let Target::Mock(mock) = target {
        isolate(&mut cmd, sandbox, mock);
    }
    cmd
}

/// `MERMAID_EVAL_CONFIG`: `;`-separated `-c` overrides for live runs.
fn live_overrides() -> Vec<String> {
    std::env::var("MERMAID_EVAL_CONFIG")
        .unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|pin| !pin.is_empty())
        .map(str::to_string)
        .collect()
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
            // With follow-ups there is one per run; the last one is the
            // conversation's outcome.
            "result" => result = Some(event),
            _ => {},
        }
    }
    result
}

fn score(
    check: &Check,
    project: &Path,
    (before, after): (&Snapshot, &Snapshot),
    run: &Run,
    result: Option<&Value>,
    target: &Target<'_>,
    saved: Option<&str>,
) -> (String, Option<String>) {
    match check {
        Check::Command {
            run: argv,
            stdout_contains,
            overlay: _,
            restore,
        } => {
            let (mut label, failure) = score_command(argv, stdout_contains.as_deref(), project);
            if !restore.is_empty() {
                let _ = write!(label, " with the original {} restored", restore.join(", "));
            }
            (label, failure)
        },
        Check::Unchanged { paths } => {
            let label = format!("{} unchanged", paths.join(", "));
            let changed: Vec<&String> = before
                .keys()
                .chain(after.keys().filter(|k| !before.contains_key(*k)))
                .filter(|file| covers(paths, file) && before.get(*file) != after.get(*file))
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
        Check::AppSaved { equals } => {
            let label = format!("the screen app saved {equals:?}");
            let failure = match saved {
                Some(text) if text.trim() == equals.trim() => None,
                Some(text) => Some(format!("it saved {:?}", text.trim())),
                None => Some("nothing was saved".to_string()),
            };
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

#[derive(Default)]
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

/// Whether `file`, a project-relative path, falls under one of `paths`.
fn covers(paths: &[String], file: &str) -> bool {
    paths.iter().any(|p| {
        let p = p.trim_end_matches('/');
        p == "." || file == p || file.starts_with(&format!("{p}/"))
    })
}

/// Put `paths` back to the fixture's version: files the run added under them
/// are removed, and the fixture's own are rewritten.
fn restore_paths(project: &Path, fixture: &BTreeMap<String, Vec<u8>>, paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    for (file, _) in snapshot(project) {
        if covers(paths, &file) && !fixture.contains_key(&file) {
            std::fs::remove_file(project.join(&file)).expect("remove an added file");
        }
    }
    for (file, bytes) in fixture.iter().filter(|(file, _)| covers(paths, file)) {
        let target = project.join(file);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("recreate a restored directory");
        }
        std::fs::write(target, bytes).expect("restore a fixture file");
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
/// A git identity for commits the model makes, and no signing: the machine
/// running the evals may have neither set up, and a task that asks for a
/// commit should not fail on that.
fn git_identity(cmd: &mut Command) {
    cmd.env("GIT_AUTHOR_NAME", "mermaid-eval")
        .env("GIT_AUTHOR_EMAIL", "eval@example.invalid")
        .env("GIT_COMMITTER_NAME", "mermaid-eval")
        .env("GIT_COMMITTER_EMAIL", "eval@example.invalid")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_0", "false");
}

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
/// Every file in a project, by relative path.
type Snapshot = BTreeMap<String, Vec<u8>>;

fn snapshot(project: &Path) -> Snapshot {
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
