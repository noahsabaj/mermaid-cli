#!/usr/bin/env python3
"""Run real Mermaid dogfood scenarios against a safe local fixture.

This harness is intentionally opt-in. It drives `mermaid run` with a real
model and real tools, verifies filesystem artifacts, and restores the fixture
between scenarios so agents can QA Mermaid without a human operating the TUI.
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import difflib
import hashlib
import json
import os
import shutil
import subprocess
import sys
import textwrap
from pathlib import Path
from typing import Callable, Iterable


REPO_ROOT = Path(__file__).resolve().parents[1]
# The built-in QA root — the ONE path the harness trusts to reset without a
# marker, because it owns it. This is deliberately NOT env-overridable: the
# `guard_test_env` marker check anchors on it, so folding the env var in here
# would let `MERMAID_QA_TEST_ENV=~/anything` masquerade as the trusted default
# and bypass the guard entirely.
BUILTIN_TEST_ENV = (Path.home() / ".cache" / "mermaid-qa" / "test-env").resolve()
# The effective default: the env override if set, else the built-in root. Any
# path other than BUILTIN_TEST_ENV must carry a QA_MARKER file (see
# `guard_test_env`), including an env-supplied one.
DEFAULT_TEST_ENV = Path(
    os.environ.get("MERMAID_QA_TEST_ENV") or BUILTIN_TEST_ENV
).resolve()
DEFAULT_MODEL = "minimax-m2.7:cloud"
QA_MARKER = ".mermaid-qa-root"
IGNORED_PARTS = {
    ".git",
    ".mermaid",
    ".pytest_cache",
    "__pycache__",
    ".qa-runs",
    "node_modules",
    "dist",
}


class QaFailure(RuntimeError):
    pass


@dataclasses.dataclass
class CommandResult:
    args: list[str]
    cwd: str
    returncode: int
    stdout: str
    stderr: str

    def to_json(self) -> dict:
        return dataclasses.asdict(self)


@dataclasses.dataclass
class ScenarioResult:
    name: str
    ok: bool
    duration_seconds: float
    workspace: str | None = None
    mermaid_returncode: int | None = None
    mermaid_errors: list[str] = dataclasses.field(default_factory=list)
    checks: list[str] = dataclasses.field(default_factory=list)
    failure: str | None = None

    def to_json(self) -> dict:
        return dataclasses.asdict(self)


@dataclasses.dataclass(frozen=True)
class Scenario:
    name: str
    tier: str
    fixture: str | None
    prompt: str | None
    checker: Callable[["QaContext", Path, dict, CommandResult | None], list[str]]
    allow_mermaid_errors: bool = False


@dataclasses.dataclass
class QaContext:
    args: argparse.Namespace
    run_dir: Path
    workspaces_dir: Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run Mermaid self-QA scenarios")
    parser.add_argument(
        "--test-env",
        default=os.environ.get("MERMAID_QA_TEST_ENV", str(DEFAULT_TEST_ENV)),
        help="Fixture root to wipe and recreate",
    )
    parser.add_argument(
        "--model",
        default=os.environ.get("MERMAID_QA_MODEL", DEFAULT_MODEL),
        help="Model passed to `mermaid --model`",
    )
    parser.add_argument(
        "--mermaid-bin",
        default=str(REPO_ROOT / "target" / "debug" / "mermaid"),
        help="Mermaid binary to test",
    )
    parser.add_argument(
        "--scenario",
        action="append",
        choices=sorted(SCENARIOS.keys()) if "SCENARIOS" in globals() else None,
        help="Scenario to run; may be repeated",
    )
    parser.add_argument(
        "--tier",
        choices=["fast", "real", "all"],
        default="fast",
        help="Scenario tier to run when --scenario is omitted",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=25 * 60,
        help="Per Mermaid run timeout in seconds",
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help="Do not run `cargo build --bin mermaid` before QA",
    )
    parser.add_argument(
        "--keep-workspace",
        action="store_true",
        help="Do not restore scenario workspaces after checks",
    )
    parser.add_argument(
        "--wipe-runs",
        action="store_true",
        help="Also delete prior .qa-runs logs when resetting the QA root",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Print only the final JSON report path and status",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    test_env = Path(args.test_env).expanduser().resolve()
    mermaid_bin = Path(args.mermaid_bin).expanduser().resolve()
    args.test_env = str(test_env)
    args.mermaid_bin = str(mermaid_bin)

    started = dt.datetime.now(dt.UTC)
    run_id = started.strftime("%Y%m%d_%H%M%S")
    try:
        guard_test_env(test_env)
        reset_test_env(test_env, wipe_runs=args.wipe_runs)
    except Exception as exc:  # noqa: BLE001 - CLI entry must render guard failures cleanly.
        failure = {
            "ok": False,
            "started_at": started.isoformat(),
            "finished_at": dt.datetime.now(dt.UTC).isoformat(),
            "model": args.model,
            "mermaid_bin": str(mermaid_bin),
            "test_env": str(test_env),
            "fatal_error": str(exc),
            "results": [],
        }
        if args.json:
            print(json.dumps(failure, sort_keys=True))
        else:
            print("Mermaid self-QA")
            print(f"Fatal: {exc}")
            print()
            print("FAIL")
        return 1

    run_dir = test_env / ".qa-runs" / run_id
    run_dir.mkdir(parents=True, exist_ok=True)
    workspaces_dir = test_env / "workspaces"
    workspaces_dir.mkdir(parents=True, exist_ok=True)
    ctx = QaContext(args=args, run_dir=run_dir, workspaces_dir=workspaces_dir)

    report = {
        "ok": False,
        "started_at": started.isoformat(),
        "model": args.model,
        "mermaid_bin": str(mermaid_bin),
        "test_env": str(test_env),
        "run_dir": str(run_dir),
        "results": [],
    }

    try:
        if not args.skip_build:
            build_result = run_command(
                ["cargo", "build", "--bin", "mermaid"],
                cwd=REPO_ROOT,
                timeout=10 * 60,
            )
            write_command(run_dir / "cargo_build.json", build_result)
            if build_result.returncode != 0:
                raise QaFailure("cargo build --bin mermaid failed")

        selected = args.scenario or scenarios_for_tier(args.tier)
        results = [run_scenario(ctx, SCENARIOS[name]) for name in selected]
        report["results"] = [result.to_json() for result in results]
        report["ok"] = all(result.ok for result in results)
    except Exception as exc:  # noqa: BLE001 - top-level report must capture all failures.
        report["fatal_error"] = str(exc)

    report["finished_at"] = dt.datetime.now(dt.UTC).isoformat()
    report_path = run_dir / "report.json"
    report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")

    if args.json:
        print(json.dumps({"ok": report["ok"], "report": str(report_path)}, sort_keys=True))
    else:
        print_summary(report, report_path)
    return 0 if report["ok"] else 1


def guard_test_env(test_env: Path) -> None:
    """Refuse dangerous reset targets.

    Only the built-in cache root (`BUILTIN_TEST_ENV`) may be reset without a
    marker — the harness created it. EVERY other target, whether it arrived via
    `--test-env` or `MERMAID_QA_TEST_ENV`, must carry a `QA_MARKER` file. The
    anchor is the built-in constant, NOT `DEFAULT_TEST_ENV`, because the latter
    folds in the env var and would compare equal to an env-supplied path —
    silently disarming the guard.
    """
    home = Path.home().resolve()
    forbidden = {
        Path("/").resolve(),
        home,
        REPO_ROOT.resolve(),
        REPO_ROOT.parent.resolve(),
    }
    if not str(test_env):
        raise QaFailure("test-env path is empty")
    if test_env in forbidden:
        raise QaFailure(f"refusing to use dangerous test-env path: {test_env}")
    if test_env != BUILTIN_TEST_ENV and not (test_env / QA_MARKER).exists():
        raise QaFailure(
            f"refusing to reset unmarked test-env {test_env}; "
            f"create {test_env / QA_MARKER} first if this path is intentional"
        )


def reset_test_env(test_env: Path, wipe_runs: bool) -> None:
    test_env.mkdir(parents=True, exist_ok=True)
    for child in list(test_env.iterdir()):
        if child.name == ".qa-runs" and not wipe_runs:
            continue
        remove_path(child)
    (test_env / QA_MARKER).write_text("Mermaid QA root. Safe to reset by scripts/qa_mermaid.py.\n")
    (test_env / ".qa-runs").mkdir(exist_ok=True)


def remove_path(path: Path) -> None:
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    else:
        path.unlink(missing_ok=True)


def run_scenario(ctx: QaContext, scenario: Scenario) -> ScenarioResult:
    start = dt.datetime.now()
    scenario_dir = ctx.run_dir / scenario.name
    scenario_dir.mkdir(parents=True, exist_ok=True)
    workspace = ctx.workspaces_dir / scenario.name
    snapshot = ctx.run_dir / "snapshots" / scenario.name

    try:
        if scenario.fixture is not None:
            create_fixture(workspace, scenario.fixture)
            copy_tree(workspace, snapshot)
        before = manifest(workspace) if workspace.exists() else {}

        mermaid_result = None
        mermaid_json: dict = {}
        if scenario.prompt is not None:
            prompt = scenario.prompt.strip()
            (scenario_dir / "prompt.txt").write_text(prompt + "\n")
            mermaid_result = run_mermaid(ctx, workspace, prompt)
            write_command(scenario_dir / "mermaid_command.json", mermaid_result)
            (scenario_dir / "stdout.txt").write_text(mermaid_result.stdout)
            (scenario_dir / "stderr.txt").write_text(mermaid_result.stderr)
            mermaid_json = parse_mermaid_json(mermaid_result.stdout)
            (scenario_dir / "mermaid.json").write_text(
                json.dumps(mermaid_json, indent=2, sort_keys=True) + "\n"
            )
            errors = list(mermaid_json.get("errors") or [])
            if mermaid_result.returncode != 0 and not scenario.allow_mermaid_errors:
                raise QaFailure(f"mermaid exited {mermaid_result.returncode}: {errors}")
            if errors and not scenario.allow_mermaid_errors:
                raise QaFailure(f"mermaid reported tool errors: {errors}")
            if not str(mermaid_json.get("response") or "").strip():
                raise QaFailure("mermaid returned an empty response")

        checks = scenario.checker(ctx, workspace, before, mermaid_result)
        after = manifest(workspace) if workspace.exists() else {}
        write_manifest(scenario_dir / "before_manifest.json", before)
        write_manifest(scenario_dir / "after_manifest.json", after)
        write_diff(scenario_dir / "diff.patch", snapshot, workspace)

        result = ScenarioResult(
            name=scenario.name,
            ok=True,
            duration_seconds=elapsed_seconds(start),
            workspace=str(workspace) if workspace.exists() else None,
            mermaid_returncode=mermaid_result.returncode if mermaid_result else None,
            mermaid_errors=list(mermaid_json.get("errors") or []),
            checks=checks,
        )
    except Exception as exc:  # noqa: BLE001 - scenario report should preserve all failures.
        result = ScenarioResult(
            name=scenario.name,
            ok=False,
            duration_seconds=elapsed_seconds(start),
            workspace=str(workspace) if workspace.exists() else None,
            failure=str(exc),
        )
    finally:
        if workspace.exists() and snapshot.exists() and not ctx.args.keep_workspace:
            restore_tree(snapshot, workspace)

    return result


def run_mermaid(ctx: QaContext, workspace: Path, prompt: str) -> CommandResult:
    return run_command(
        [
            ctx.args.mermaid_bin,
            "--model",
            ctx.args.model,
            "--path",
            str(workspace),
            "run",
            "-f",
            "json",
            prompt,
        ],
        cwd=REPO_ROOT,
        timeout=ctx.args.timeout,
    )


def create_fixture(workspace: Path, fixture: str) -> None:
    remove_path(workspace) if workspace.exists() else None
    workspace.mkdir(parents=True, exist_ok=True)
    files = fixture_files(fixture)
    for rel, content in files.items():
        path = workspace / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(content).lstrip())


def fixture_files(fixture: str) -> dict[str, str]:
    common = {
        "README.md": """
            # Mermaid QA Arithmetic Fixture

            This is a tiny Python project used to test Mermaid.

            Run checks with:

            ```bash
            python3 -m unittest discover -s tests
            ```
        """,
        "MERMAID.md": """
            # Mermaid QA Fixture

            - This is a disposable QA project.
            - Use `python3 -m unittest discover -s tests` for validation.
            - Do not edit tests unless the user explicitly asks.
            - Keep implementation changes small and explain the command result.
        """,
        "src/__init__.py": "",
        "tests/__init__.py": "",
    }

    if fixture == "clean":
        common.update(
            {
                "src/calculator.py": """
                    def add(a, b):
                        return a + b


                    def subtract(a, b):
                        return a - b
                """,
                "tests/test_calculator.py": """
                    import unittest

                    from src.calculator import add, subtract


                    class CalculatorTests(unittest.TestCase):
                        def test_add(self):
                            self.assertEqual(add(2, 3), 5)

                        def test_subtract(self):
                            self.assertEqual(subtract(7, 4), 3)


                    if __name__ == "__main__":
                        unittest.main()
                """,
            }
        )
    elif fixture == "buggy_add":
        common.update(
            {
                "src/calculator.py": """
                    def add(a, b):
                        return a - b


                    def subtract(a, b):
                        return a - b
                """,
                "tests/test_calculator.py": """
                    import unittest

                    from src.calculator import add, subtract


                    class CalculatorTests(unittest.TestCase):
                        def test_add(self):
                            self.assertEqual(add(2, 3), 5)
                            self.assertEqual(add(-1, 1), 0)

                        def test_subtract(self):
                            self.assertEqual(subtract(7, 4), 3)


                    if __name__ == "__main__":
                        unittest.main()
                """,
            }
        )
    elif fixture == "missing_multiply":
        common.update(
            {
                "src/calculator.py": """
                    def add(a, b):
                        return a + b


                    def subtract(a, b):
                        return a - b
                """,
                "tests/test_calculator.py": """
                    import unittest

                    from src.calculator import add, multiply, subtract


                    class CalculatorTests(unittest.TestCase):
                        def test_add(self):
                            self.assertEqual(add(2, 3), 5)

                        def test_subtract(self):
                            self.assertEqual(subtract(7, 4), 3)

                        def test_multiply(self):
                            self.assertEqual(multiply(6, 7), 42)
                            self.assertEqual(multiply(-3, 4), -12)


                    if __name__ == "__main__":
                        unittest.main()
                """,
            }
        )
    else:
        raise QaFailure(f"unknown fixture: {fixture}")
    return common


def check_read_only(
    _ctx: QaContext,
    workspace: Path,
    before: dict,
    _mermaid: CommandResult | None,
) -> list[str]:
    after = manifest(workspace)
    if after != before:
        raise QaFailure("read-only project scan modified files")
    return ["workspace unchanged"]


def check_fix_failing_test(
    _ctx: QaContext,
    workspace: Path,
    before: dict,
    _mermaid: CommandResult | None,
) -> list[str]:
    result = run_command(["python3", "-m", "unittest", "discover", "-s", "tests"], cwd=workspace)
    if result.returncode != 0:
        raise QaFailure(f"fixture tests still fail:\n{result.stdout}\n{result.stderr}")
    after = manifest(workspace)
    changed = changed_files(before, after)
    if changed != ["src/calculator.py"]:
        raise QaFailure(f"expected only src/calculator.py to change, got {changed}")
    source = (workspace / "src" / "calculator.py").read_text()
    if "return a + b" not in source:
        raise QaFailure("calculator.add was not repaired to use addition")
    return ["tests pass", "only implementation changed", "add uses addition"]


def check_add_small_feature(
    _ctx: QaContext,
    workspace: Path,
    before: dict,
    _mermaid: CommandResult | None,
) -> list[str]:
    result = run_command(["python3", "-m", "unittest", "discover", "-s", "tests"], cwd=workspace)
    if result.returncode != 0:
        raise QaFailure(f"fixture tests still fail:\n{result.stdout}\n{result.stderr}")
    after = manifest(workspace)
    changed = changed_files(before, after)
    if changed != ["src/calculator.py"]:
        raise QaFailure(f"expected only src/calculator.py to change, got {changed}")
    source = (workspace / "src" / "calculator.py").read_text()
    if "def multiply" not in source:
        raise QaFailure("multiply function was not implemented")
    return ["tests pass", "only implementation changed", "multiply implemented"]


def check_runtime_smoke(
    ctx: QaContext,
    _workspace: Path,
    _before: dict,
    _mermaid: CommandResult | None,
) -> list[str]:
    result = run_command([ctx.args.mermaid_bin, "tasks", "--limit", "5"], cwd=REPO_ROOT)
    write_command(ctx.run_dir / "runtime_tasks.json", result)
    if result.returncode != 0:
        raise QaFailure(f"`mermaid tasks` failed: {result.stderr}")
    if "Mermaid runtime tasks" not in result.stdout:
        raise QaFailure("runtime task output did not include the expected heading")
    return ["runtime tasks command exits cleanly"]


def check_compact_smoke(
    ctx: QaContext,
    workspace: Path,
    _before: dict,
    _mermaid: CommandResult | None,
) -> list[str]:
    workspace.mkdir(parents=True, exist_ok=True)
    result = run_command(
        [
            ctx.args.mermaid_bin,
            "--path",
            workspace,
            "qa",
            "compact-smoke",
            "--turns",
            "6",
            "--format",
            "json",
        ],
        cwd=REPO_ROOT,
    )
    write_command(ctx.run_dir / "compact_smoke_command.json", result)
    if result.returncode != 0:
        raise QaFailure(f"`mermaid qa compact-smoke` failed: {result.stderr}")
    payload = parse_mermaid_json(result.stdout)
    if payload.get("ok") is not True:
        raise QaFailure(f"compact-smoke reported failure: {payload}")
    if int(payload.get("archived_messages") or 0) <= 0:
        raise QaFailure("compact-smoke archived no messages")
    if int(payload.get("preserved_messages") or 0) <= 0:
        raise QaFailure("compact-smoke preserved no messages")
    if int(payload.get("replacement_messages") or 0) < 3:
        raise QaFailure("compact-smoke produced too few replacement messages")
    for key in ("conversation_path", "archive_path"):
        path = payload.get(key)
        if not path or not Path(path).exists():
            raise QaFailure(f"compact-smoke missing {key}: {path}")
    return [
        "compact smoke command exits cleanly",
        "archives and preserves messages",
        "conversation and archive files exist",
    ]


SCENARIOS: dict[str, Scenario] = {
    "compact_smoke": Scenario(
        name="compact_smoke",
        tier="fast",
        fixture=None,
        prompt=None,
        checker=check_compact_smoke,
    ),
    "runtime_smoke": Scenario(
        name="runtime_smoke",
        tier="fast",
        fixture=None,
        prompt=None,
        checker=check_runtime_smoke,
    ),
    "read_only_project_scan": Scenario(
        name="read_only_project_scan",
        tier="real",
        fixture="clean",
        prompt="""
            Inspect this small Python project. Do not modify any files.
            Report the project purpose, important files, the validation command,
            and one possible next improvement.
        """,
        checker=check_read_only,
    ),
    "fix_failing_test": Scenario(
        name="fix_failing_test",
        tier="real",
        fixture="buggy_add",
        prompt="""
            Run `python3 -m unittest discover -s tests`, fix the implementation
            bug that causes the failure, rerun the same command, and report the
            result. Do not edit tests.
        """,
        checker=check_fix_failing_test,
        allow_mermaid_errors=True,
    ),
    "add_small_feature": Scenario(
        name="add_small_feature",
        tier="real",
        fixture="missing_multiply",
        prompt="""
            Run `python3 -m unittest discover -s tests`, implement the missing
            calculator feature needed by the tests, rerun the same command, and
            report the result. Do not edit tests.
        """,
        checker=check_add_small_feature,
        allow_mermaid_errors=True,
    ),
}


def scenarios_for_tier(tier: str) -> list[str]:
    if tier == "all":
        return list(SCENARIOS.keys())
    return [name for name, scenario in SCENARIOS.items() if scenario.tier == tier]


def run_command(
    args: Iterable[str | Path],
    cwd: Path,
    timeout: int = 120,
) -> CommandResult:
    argv = [str(arg) for arg in args]
    completed = subprocess.run(
        argv,
        cwd=str(cwd),
        text=True,
        capture_output=True,
        timeout=timeout,
        check=False,
    )
    return CommandResult(
        args=argv,
        cwd=str(cwd),
        returncode=completed.returncode,
        stdout=completed.stdout,
        stderr=completed.stderr,
    )


def parse_mermaid_json(stdout: str) -> dict:
    try:
        value = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise QaFailure(f"Mermaid stdout was not valid JSON: {exc}\n{stdout[:1000]}") from exc
    if not isinstance(value, dict):
        raise QaFailure("Mermaid JSON output was not an object")
    return value


def manifest(root: Path) -> dict[str, str]:
    out: dict[str, str] = {}
    if not root.exists():
        return out
    for path in sorted(p for p in root.rglob("*") if p.is_file()):
        rel = path.relative_to(root).as_posix()
        if should_ignore(rel):
            continue
        out[rel] = sha256_file(path)
    return out


def should_ignore(rel: str) -> bool:
    parts = set(Path(rel).parts)
    return bool(parts & IGNORED_PARTS)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def changed_files(before: dict[str, str], after: dict[str, str]) -> list[str]:
    keys = sorted(set(before) | set(after))
    return [key for key in keys if before.get(key) != after.get(key)]


def write_manifest(path: Path, data: dict[str, str]) -> None:
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")


def write_command(path: Path, result: CommandResult) -> None:
    path.write_text(json.dumps(result.to_json(), indent=2, sort_keys=True) + "\n")


def write_diff(path: Path, before: Path, after: Path) -> None:
    lines: list[str] = []
    before_files = text_files(before)
    after_files = text_files(after)
    for rel in sorted(set(before_files) | set(after_files)):
        old = before_files.get(rel, [])
        new = after_files.get(rel, [])
        if old == new:
            continue
        lines.extend(
            difflib.unified_diff(
                old,
                new,
                fromfile=f"before/{rel}",
                tofile=f"after/{rel}",
                lineterm="",
            )
        )
        lines.append("")
    path.write_text("\n".join(lines).rstrip() + ("\n" if lines else ""))


def text_files(root: Path) -> dict[str, list[str]]:
    out: dict[str, list[str]] = {}
    if not root.exists():
        return out
    for path in sorted(p for p in root.rglob("*") if p.is_file()):
        rel = path.relative_to(root).as_posix()
        if should_ignore(rel):
            continue
        try:
            out[rel] = path.read_text().splitlines()
        except UnicodeDecodeError:
            out[rel] = ["<binary>"]
    return out


def copy_tree(src: Path, dst: Path) -> None:
    remove_path(dst) if dst.exists() else None
    shutil.copytree(src, dst)


def restore_tree(snapshot: Path, workspace: Path) -> None:
    remove_path(workspace) if workspace.exists() else None
    shutil.copytree(snapshot, workspace)


def elapsed_seconds(start: dt.datetime) -> float:
    return (dt.datetime.now() - start).total_seconds()


def print_summary(report: dict, report_path: Path) -> None:
    print("Mermaid self-QA")
    print(f"Report: {report_path}")
    print(f"Model: {report['model']}")
    print(f"Test env: {report['test_env']}")
    print()
    for item in report.get("results", []):
        status = "PASS" if item.get("ok") else "FAIL"
        print(f"{status} {item['name']} ({item['duration_seconds']:.1f}s)")
        if item.get("checks"):
            for check in item["checks"]:
                print(f"  - {check}")
        if item.get("mermaid_errors"):
            print(f"  - mermaid tool errors observed: {len(item['mermaid_errors'])}")
        if item.get("failure"):
            print(f"  - {item['failure']}")
    if report.get("fatal_error"):
        print()
        print(f"Fatal: {report['fatal_error']}")
    print()
    print("PASS" if report.get("ok") else "FAIL")


if __name__ == "__main__":
    sys.exit(main())
