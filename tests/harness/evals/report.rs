//! What a live eval run leaves behind: the Markdown report, the guidance pack
//! comparison, and the results history.
//!
//! The history is a committed JSON Lines file (`evals/results/history.jsonl`),
//! one line per model and guidance setting per invocation. It is what turns
//! "does the harness scale with the model" from a one-off check into a trend
//! line: every report ends with how each model scored before, on the same
//! tasks, and how every recorded model compares.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Guidance, Run};

/// A count as a float, for averages. Eval counts are small, so the saturation
/// at `u32::MAX` never happens; it only keeps the conversion exact.
fn float(n: impl TryInto<u32>) -> f64 {
    f64::from(n.try_into().unwrap_or(u32::MAX))
}

/// Round to one decimal, so the history file diffs cleanly.
fn tenth(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn pct(passed: usize, runs: usize) -> String {
    if runs == 0 {
        "-".to_string()
    } else {
        format!("{:.0}%", 100.0 * float(passed) / float(runs))
    }
}

/// How a setting reads in a heading: `pack on`, `pack off`, or the config's
/// choice with what it resolved to.
fn setting(guidance: Guidance, pack: Option<bool>) -> String {
    match (guidance, pack) {
        (Guidance::On, _) => "guidance pack on".to_string(),
        (Guidance::Off, _) => "guidance pack off".to_string(),
        (Guidance::Default, Some(true)) => "default config (pack on)".to_string(),
        (Guidance::Default, Some(false)) => "default config (pack off)".to_string(),
        (Guidance::Default, None) => "default config".to_string(),
    }
}

fn on_off(pack: Option<bool>) -> &'static str {
    match pack {
        Some(true) => "on",
        Some(false) => "off",
        None => "?",
    }
}

/// Runs grouped by model, then by guidance setting, in a stable order.
fn by_model_and_setting(runs: &[Run]) -> BTreeMap<&str, BTreeMap<Guidance, Vec<&Run>>> {
    let mut out: BTreeMap<&str, BTreeMap<Guidance, Vec<&Run>>> = BTreeMap::new();
    for run in runs {
        out.entry(run.model.as_str())
            .or_default()
            .entry(run.guidance)
            .or_default()
            .push(run);
    }
    out
}

/// Score table for a set of live runs, as Markdown: one section per model and
/// guidance setting, then the on/off comparison when both were run.
#[must_use]
pub fn report(runs: &[Run]) -> String {
    let mut out = String::from("# Mermaid behavioural evals\n");
    for (model, settings) in by_model_and_setting(runs) {
        for (guidance, runs) in settings {
            write_section(&mut out, model, guidance, &runs);
        }
    }
    if let Some(comparison) = guidance_comparison(runs) {
        out.push('\n');
        out.push_str(&comparison);
    }
    out
}

fn write_section(out: &mut String, model: &str, guidance: Guidance, runs: &[&Run]) {
    let passed = runs.iter().filter(|r| r.passed()).count();
    let unscored = runs.iter().filter(|r| r.harness_error.is_some()).count();
    let edits: usize = runs.iter().map(|r| r.edits).sum();
    let fuzzy: usize = runs.iter().map(|r| r.fuzzy_edits).sum();
    let pack = runs.first().and_then(|r| r.pack);
    let heading = if guidance == Guidance::Default && pack.is_none() {
        format!("## {model}")
    } else {
        format!("## {model} · {}", setting(guidance, pack))
    };
    let _ = write!(
        out,
        "\n{heading}\n\n{passed}/{} runs passed{}. Fuzzy edits: {fuzzy} of {edits}{}.\n\n",
        runs.len(),
        if unscored == 0 {
            String::new()
        } else {
            format!(" ({unscored} never finished; see Failures)")
        },
        if edits == 0 {
            String::new()
        } else {
            format!(" ({:.0}%)", 100.0 * float(fuzzy) / float(edits))
        }
    );
    out.push_str("| task | passed | turns | tokens | tool calls | fuzzy edits | seconds |\n");
    out.push_str("|---|---|---|---|---|---|---|\n");
    for (task, score) in task_scores(runs) {
        let _ = writeln!(
            out,
            "| {task} | {}/{} | {:.1} | {:.0} | {:.1} | {}/{} | {:.0} |",
            score.passed,
            score.runs,
            score.turns,
            score.tokens,
            score.tool_calls,
            score.fuzzy_edits,
            score.edits,
            score.seconds,
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

// ── Guidance pack on versus off ─────────────────────────────────────────

/// Fewer runs per task than this and a difference is reported as a hint.
const CONFIDENT_REPEAT: usize = 3;

/// Passes over scored runs, and mean tokens, for one side of the comparison.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Side {
    passed: usize,
    scored: usize,
    tokens: f64,
}

impl Side {
    fn of(runs: &[&Run]) -> Self {
        let scored: Vec<&&Run> = runs.iter().filter(|r| r.harness_error.is_none()).collect();
        let tokens = if scored.is_empty() {
            0.0
        } else {
            scored.iter().map(|r| float(r.tokens)).sum::<f64>() / float(scored.len())
        };
        Self {
            passed: scored.iter().filter(|r| r.passed()).count(),
            scored: scored.len(),
            tokens,
        }
    }

    fn rate(self) -> f64 {
        if self.scored == 0 {
            0.0
        } else {
            float(self.passed) / float(self.scored)
        }
    }
}

/// What a model's on/off scores say, and whether the default agrees.
///
/// `default` is whether the config in effect turns the pack on for the model
/// (the `auto` default decides by provider locality, which only stands in for
/// capability). The verdict is the direct reading of the pass rates; how much
/// to trust it is the repeat count's job, reported separately.
#[must_use]
pub fn verdict(on: (usize, usize), off: (usize, usize), default: Option<bool>) -> String {
    let rate = |(passed, scored): (usize, usize)| {
        if scored == 0 {
            None
        } else {
            Some(float(passed) / float(scored))
        }
    };
    let (Some(on), Some(off)) = (rate(on), rate(off)) else {
        return "not scored".to_string();
    };
    let better = if (on - off).abs() < f64::EPSILON {
        None
    } else {
        Some(on > off)
    };
    let finding = match better {
        None => "no measured difference",
        Some(true) => "the pack helps",
        Some(false) => "the pack hurts",
    };
    let agreement = match (better, default) {
        (_, None) => "",
        (None, Some(true)) => "; the default turns it on, so it only costs tokens here",
        (None, Some(false)) => "; the default leaves it off, which is right",
        (Some(better), Some(default)) if better == default => "; the default is right",
        (Some(_), Some(_)) => "; the default is wrong for this model",
    };
    format!("{finding}{agreement}")
}

/// The on/off comparison, per model and per task, when some model was run
/// with the pack both on and off.
#[must_use]
pub fn guidance_comparison(runs: &[Run]) -> Option<String> {
    let mut rows = Vec::new();
    let mut detail = Vec::new();
    let mut fewest = usize::MAX;
    for (model, settings) in by_model_and_setting(runs) {
        let (Some(on), Some(off)) = (settings.get(&Guidance::On), settings.get(&Guidance::Off))
        else {
            continue;
        };
        let default = on.first().and_then(|r| r.default_pack);
        let (on_side, off_side) = (Side::of(on), Side::of(off));
        rows.push(format!(
            "| {model} | {} | {}/{} | {}/{} | {:.0} / {:.0} | {} |",
            on_off(default),
            on_side.passed,
            on_side.scored,
            off_side.passed,
            off_side.scored,
            on_side.tokens,
            off_side.tokens,
            verdict(
                (on_side.passed, on_side.scored),
                (off_side.passed, off_side.scored),
                default
            ),
        ));
        let on_tasks = group_by_task(on);
        let off_tasks = group_by_task(off);
        for task in on_tasks
            .keys()
            .chain(off_tasks.keys())
            .collect::<BTreeSet<_>>()
        {
            let side = |tasks: &BTreeMap<&str, Vec<&Run>>| {
                tasks.get(task).map_or(
                    Side {
                        passed: 0,
                        scored: 0,
                        tokens: 0.0,
                    },
                    |runs| Side::of(runs),
                )
            };
            let (a, b) = (side(&on_tasks), side(&off_tasks));
            fewest = fewest.min(a.scored).min(b.scored);
            let delta = if a.scored == 0 || b.scored == 0 {
                "-".to_string()
            } else {
                format!("{:+.0} pts", 100.0 * (a.rate() - b.rate()))
            };
            detail.push(format!(
                "| {model} | {task} | {}/{} | {}/{} | {delta} |",
                a.passed, a.scored, b.passed, b.scored
            ));
        }
    }
    if rows.is_empty() {
        return None;
    }
    let mut out = String::from(
        "## Guidance pack: on versus off\n\n\
         Same model, same tasks, with the coaching pack pinned on and then off. \
         `default` is what the config in effect picks for the model. The shipped \
         `auto` default decides by provider (on for local, off for hosted), which \
         only stands in for how capable the model is; these rows say whether it \
         picked right.\n\n",
    );
    out.push_str("| model | default | on | off | tokens on / off | verdict |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for row in rows {
        out.push_str(&row);
        out.push('\n');
    }
    if fewest < CONFIDENT_REPEAT {
        let _ = write!(
            out,
            "\nSome tasks have fewer than {CONFIDENT_REPEAT} scored runs per side. Treat a \
             difference as a hint, and rerun with `MERMAID_EVAL_REPEAT={CONFIDENT_REPEAT}` \
             or more before acting on it.\n"
        );
    }
    out.push_str("\n| model | task | on | off | difference |\n|---|---|---|---|---|\n");
    for row in detail {
        out.push_str(&row);
        out.push('\n');
    }
    Some(out)
}

fn group_by_task<'a>(runs: &[&'a Run]) -> BTreeMap<&'a str, Vec<&'a Run>> {
    let mut tasks: BTreeMap<&str, Vec<&Run>> = BTreeMap::new();
    for run in runs {
        tasks.entry(run.task.as_str()).or_default().push(run);
    }
    tasks
}

// ── History ─────────────────────────────────────────────────────────────

/// One task's aggregate within a [`HistoryEntry`]. Means are over every run,
/// scored or not, as the report table shows them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskScore {
    pub passed: usize,
    pub runs: usize,
    /// Runs that never produced a verdict (bad key, crash, timeout). They
    /// count in `runs` but say nothing about the model.
    #[serde(default)]
    pub unscored: usize,
    pub turns: f64,
    pub tokens: f64,
    pub tool_calls: f64,
    pub seconds: f64,
    pub edits: usize,
    pub fuzzy_edits: usize,
}

fn task_scores(runs: &[&Run]) -> BTreeMap<String, TaskScore> {
    group_by_task(runs)
        .into_iter()
        .map(|(task, runs)| {
            let n = float(runs.len());
            let mean = |f: &dyn Fn(&Run) -> f64| tenth(runs.iter().map(|r| f(r)).sum::<f64>() / n);
            let score = TaskScore {
                passed: runs.iter().filter(|r| r.passed()).count(),
                runs: runs.len(),
                unscored: runs.iter().filter(|r| r.harness_error.is_some()).count(),
                turns: mean(&|r| float(r.turns)),
                tokens: mean(&|r| float(r.tokens)).round(),
                tool_calls: mean(&|r| float(r.tool_calls)),
                seconds: mean(&|r| r.seconds).round(),
                edits: runs.iter().map(|r| r.edits).sum(),
                fuzzy_edits: runs.iter().map(|r| r.fuzzy_edits).sum(),
            };
            (task.to_string(), score)
        })
        .collect()
}

/// One model and guidance setting from one invocation of the live suite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// UTC date of the run, `YYYY-MM-DD`.
    pub date: String,
    /// Mermaid version under test.
    pub mermaid: String,
    /// Short commit of the checkout, when git could say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The checkout had uncommitted changes: the commit alone does not say
    /// what was measured.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dirty: bool,
    /// Free-form tag from `MERMAID_EVAL_LABEL`, e.g. the experiment a run was
    /// for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub model: String,
    /// `default`, `on` or `off`: what the run pinned.
    pub guidance: String,
    /// Whether the pack was actually on, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack: Option<bool>,
    pub tasks: BTreeMap<String, TaskScore>,
}

/// Where the checkout stands, stamped on every entry.
#[derive(Debug, Clone, Default)]
pub struct Provenance {
    pub date: String,
    pub mermaid: String,
    pub commit: Option<String>,
    pub dirty: bool,
    pub label: Option<String>,
}

impl Provenance {
    /// Today's date, this build's version, and the checkout's commit.
    #[must_use]
    pub fn current(label: Option<String>) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        Self {
            date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
            mermaid: env!("CARGO_PKG_VERSION").to_string(),
            commit: git(&["rev-parse", "--short=12", "HEAD"]),
            // The history file is left out: the first run of a session appends
            // to it, and every run after that would be stamped dirty by the
            // suite's own output rather than by a change to the code.
            dirty: git(&[
                "status",
                "--porcelain",
                "--untracked-files=no",
                "--",
                ".",
                ":(exclude)evals/results",
            ])
            .is_some_and(|status| !status.is_empty()),
            label: label.filter(|l| !l.trim().is_empty()),
        }
    }
}

/// One entry per model and guidance setting in `runs`. A setting where no run
/// was scored at all (every run a misconfiguration) records nothing: it would
/// read as a model scoring zero.
#[must_use]
pub fn history_entries(runs: &[Run], provenance: &Provenance) -> Vec<HistoryEntry> {
    let mut entries = Vec::new();
    for (model, settings) in by_model_and_setting(runs) {
        for (guidance, runs) in settings {
            if runs.iter().all(|r| r.harness_error.is_some()) {
                continue;
            }
            entries.push(HistoryEntry {
                date: provenance.date.clone(),
                mermaid: provenance.mermaid.clone(),
                commit: provenance.commit.clone(),
                dirty: provenance.dirty,
                label: provenance.label.clone(),
                model: model.to_string(),
                guidance: guidance.as_str().to_string(),
                pack: runs.first().and_then(|r| r.pack),
                tasks: task_scores(&runs),
            });
        }
    }
    entries
}

/// The committed history file.
#[must_use]
pub fn default_history_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("evals")
        .join("results")
        .join("history.jsonl")
}

/// Every entry in the history file, oldest first. A missing file is an empty
/// history; a line that does not parse is skipped rather than failing a paid
/// run at the very end.
#[must_use]
pub fn read_history(path: &Path) -> Vec<HistoryEntry> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Append `entries` to the history file, one JSON object per line.
///
/// # Errors
///
/// When the file cannot be created or written.
pub fn append_history(path: &Path, entries: &[HistoryEntry]) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for entry in entries {
        let line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
        writeln!(file, "{line}")?;
    }
    Ok(())
}

/// Passes and scored runs over just `tasks`, and how many of them the entry
/// covered.
fn score_on(entry: &HistoryEntry, tasks: &BTreeSet<&str>) -> (usize, usize, usize) {
    let mut passed = 0;
    let mut scored = 0;
    let mut covered = 0;
    for task in tasks {
        if let Some(score) = entry.tasks.get(*task) {
            covered += 1;
            passed += score.passed;
            scored += score.runs - score.unscored.min(score.runs);
        }
    }
    (passed, scored, covered)
}

fn describe(entry: &HistoryEntry) -> String {
    let mut out = entry.mermaid.clone();
    if let Some(commit) = &entry.commit {
        let _ = write!(out, " `{commit}`{}", if entry.dirty { "+" } else { "" });
    }
    if let Some(label) = &entry.label {
        let _ = write!(out, " ({label})");
    }
    out
}

fn pack_label(entry: &HistoryEntry) -> String {
    match entry.guidance.as_str() {
        "default" => format!("default ({})", on_off(entry.pack)),
        other => other.to_string(),
    }
}

/// How this run compares with the recorded history: the latest entry for
/// every model ever recorded, and each current model's own trend, all scored
/// on the tasks this run covered so the rows are comparable.
///
/// `past` is the history as it was before this run; `current` is this run's
/// entries.
#[must_use]
pub fn history_section(past: &[HistoryEntry], current: &[HistoryEntry]) -> String {
    let tasks: BTreeSet<&str> = current
        .iter()
        .flat_map(|e| e.tasks.keys().map(String::as_str))
        .collect();
    let mut out = format!(
        "## History\n\nScored on the {} task(s) this run covered. A row marked `k/n tasks` \
         recorded only some of them. A `+` after a commit means the checkout had \
         uncommitted changes.\n",
        tasks.len()
    );

    // Latest entry per (model, guidance), current run included.
    let mut latest: BTreeMap<(&str, &str), &HistoryEntry> = BTreeMap::new();
    for entry in past.iter().chain(current) {
        latest.insert((entry.model.as_str(), entry.guidance.as_str()), entry);
    }
    let mut rows: Vec<(f64, String)> = latest
        .values()
        .filter_map(|entry| {
            let (passed, scored, covered) = score_on(entry, &tasks);
            (covered > 0).then(|| {
                let rate = if scored == 0 {
                    0.0
                } else {
                    float(passed) / float(scored)
                };
                (
                    rate,
                    format!(
                        "| {} | {} | {} | {} | {passed}/{scored} ({}){} |",
                        entry.model,
                        pack_label(entry),
                        entry.date,
                        describe(entry),
                        pct(passed, scored),
                        coverage_note(covered, tasks.len()),
                    ),
                )
            })
        })
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    out.push_str("\n### Every recorded model, latest run\n\n");
    out.push_str("| model | guidance | date | mermaid | passed |\n|---|---|---|---|---|\n");
    for (_, row) in rows {
        out.push_str(&row);
        out.push('\n');
    }

    for entry in current {
        let earlier: Vec<&HistoryEntry> = past
            .iter()
            .filter(|p| p.model == entry.model && p.guidance == entry.guidance)
            .collect();
        let _ = write!(
            out,
            "\n### {} · guidance {}\n\n",
            entry.model,
            pack_label(entry)
        );
        if earlier.is_empty() {
            out.push_str("First recorded run.\n");
            continue;
        }
        out.push_str("| date | mermaid | passed |\n|---|---|---|\n");
        // The ten most recent, then this run.
        let skip = earlier.len().saturating_sub(10);
        for past in earlier
            .iter()
            .skip(skip)
            .copied()
            .chain(std::iter::once(entry))
        {
            let (passed, scored, covered) = score_on(past, &tasks);
            let _ = writeln!(
                out,
                "| {} | {} | {passed}/{scored} ({}){} |",
                past.date,
                describe(past),
                pct(passed, scored),
                coverage_note(covered, tasks.len()),
            );
        }
    }
    out
}

fn coverage_note(covered: usize, total: usize) -> String {
    if covered == total {
        String::new()
    } else {
        format!(", {covered}/{total} tasks")
    }
}
