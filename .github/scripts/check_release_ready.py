#!/usr/bin/env python3
"""Guard: is this tree actually ready to be tagged?

`.github/workflows/release.yml` applies its gates AFTER the tag is pushed, and
two of them run late: `verify-version` is the first job, but the CHANGELOG
extraction lives in the `release` job, downstream of five platform builds. A
mismatch there is discovered once the GitHub release exists and its binaries
have shipped — recoverable only by deleting the tag and re-cutting it (which
`docs/development.md` describes, because it has happened).

This script is the same gates, runnable before `git tag`. It is deliberately a
MIRROR, not a second opinion: every rule below is transcribed from
`release.yml`, and the sections name the lines they come from so the two can be
diffed by eye when the workflow changes.

WHAT IS CHECKED

1. Package and intra-workspace dependency versions (release.yml `verify-version`).
   Both halves matter: `cargo publish` resolves a stale `version =` on a path
   dependency to the PREVIOUS release on crates.io, so a manifest can be
   internally consistent and still publish a crate wired to last month's
   sibling. 13 strings across four manifests as of v0.23.0.
2. The CHANGELOG section extraction (release.yml, `Extract release notes`).
   The workflow fails on an empty extraction, but only after the builds; the
   failure mode it prevents is a published release with blank notes.
3. The compare links at the bottom of the CHANGELOG. Not checked by the
   workflow at all — it is checked here because it is the one part of a
   version bump with no automated consequence, so it is the part that gets
   forgotten, and a wrong link is invisible until someone clicks it.
4. `Cargo.lock`. Checked for every workspace crate rather than just
   `mermaid-cli`: a partial `cargo update` leaves the root correct and a
   sibling behind, which is precisely the shape gate 1 exists to catch in the
   manifests.

WHY IT HAS A SELF-TEST. A release gate that has never failed is not evidence.
`--self-test` builds a fixture tree that is deliberately un-bumped, asserts
every check fires on it with the expected finding count, then bumps the same
fixture and asserts every check goes quiet. It needs no arguments and no
network, so `just guards` runs it on every PR — which is what keeps this file
honest as `release.yml` evolves.
"""

import argparse
import re
import shutil
import sys
import tempfile
from pathlib import Path

# `[package] version` — the FIRST `^version = ` in a manifest, matching
# release.yml's `grep -m1`. A `[dependencies]` entry cannot match: those carry
# a crate name before the `=`.
PACKAGE_VERSION_RE = re.compile(r'^version = "(.*)"', re.MULTILINE)

# An intra-workspace dependency line. Mirrors release.yml's
# `grep -E '^mermaid-[a-z]+ = \{.*version = "'`.
WORKSPACE_DEP_RE = re.compile(r'^(mermaid-[a-z]+) = \{.*version = "', re.MULTILINE)

# The version inside such a line. release.yml uses a greedy `sed -E`, which
# takes the LAST `version = "..."` on the line; `findall()[-1]` is the same
# choice, and it matters for a line that also pins a `default-features` style
# nested table.
DEP_VERSION_RE = re.compile(r'version = "([^"]*)"')


def manifests(root: Path) -> list[Path]:
    """`Cargo.toml` plus each `crates/*/Cargo.toml`, in release.yml's order."""
    return [root / "Cargo.toml"] + sorted((root / "crates").glob("*/Cargo.toml"))


def check_versions(root: Path, want: str) -> list[str]:
    """Gate 1 — release.yml `verify-version` (lines ~34-59)."""
    findings = []
    for manifest in manifests(root):
        if not manifest.is_file():
            findings.append(f"{manifest}: missing")
            continue
        text = manifest.read_text(encoding="utf-8")
        rel = manifest.relative_to(root).as_posix()

        match = PACKAGE_VERSION_RE.search(text)
        if match is None:
            findings.append(f"{rel}: no `version = ` line")
        elif match.group(1) != want:
            findings.append(f"{rel}: package v{match.group(1)}, expected {want}")

        # Line-at-a-time, like the `grep | sed` pipeline this mirrors. Slicing
        # the whole text to the next "\n" would drop the final character of a
        # manifest whose last line is a dependency and has no trailing newline
        # (`str.find` returns -1 there, and `text[start:-1]` is silently short).
        for line in text.splitlines():
            dep_match = WORKSPACE_DEP_RE.match(line)
            if dep_match is None:
                continue
            versions = DEP_VERSION_RE.findall(line)
            got = versions[-1]
            if got != want:
                findings.append(
                    f"{rel}: dependency {dep_match.group(1)} pinned to v{got}, "
                    f"expected {want}"
                )
    return findings


def extract_changelog_section(text: str, want: str) -> str:
    """The `## [VERSION]` section, up to the next `## ` header.

    Transcribed from release.yml's awk: `index($0, ver) == 1` is a LITERAL
    prefix match (so the `[`/`]` are not regex), and the section ends at the
    next line starting `## `.
    """
    header = f"## [{want}]"
    out: list[str] = []
    flag = False
    for line in text.splitlines():
        if line.startswith(header):
            flag = True
            continue
        if flag and line.startswith("## "):
            break
        if flag:
            out.append(line)
    return "\n".join(out)


def check_changelog(root: Path, want: str) -> list[str]:
    """Gate 2 — release.yml `Extract release notes` (lines ~212-228)."""
    changelog = root / "CHANGELOG.md"
    if not changelog.is_file():
        return ["CHANGELOG.md: missing"]
    section = extract_changelog_section(changelog.read_text(encoding="utf-8"), want)
    if not section.strip():
        return [
            f"CHANGELOG.md: no `## [{want}]` section, or it is empty "
            f"(the release would ship blank notes)"
        ]
    return []


def check_compare_links(root: Path, want: str) -> list[str]:
    """Gate 3 — the link block at the bottom of the CHANGELOG."""
    changelog = root / "CHANGELOG.md"
    if not changelog.is_file():
        return ["CHANGELOG.md: missing"]
    text = changelog.read_text(encoding="utf-8")
    findings = []
    if not re.search(rf"^\[{re.escape(want)}\]: ", text, re.MULTILINE):
        findings.append(f"CHANGELOG.md: no `[{want}]:` compare link")
    unreleased = rf"^\[Unreleased\]: .*compare/v{re.escape(want)}\.\.\.HEAD\s*$"
    if not re.search(unreleased, text, re.MULTILINE):
        findings.append(
            f"CHANGELOG.md: `[Unreleased]:` does not point at v{want}...HEAD"
        )
    return findings


def workspace_crate_names(root: Path) -> list[str]:
    """Every workspace crate, read from the manifests rather than hardcoded."""
    names = []
    for manifest in manifests(root):
        if not manifest.is_file():
            continue
        match = re.search(
            r'^name = "([^"]+)"', manifest.read_text(encoding="utf-8"), re.MULTILINE
        )
        if match:
            names.append(match.group(1))
    return names


def check_lockfile(root: Path, want: str) -> list[str]:
    """Gate 4 — `Cargo.lock` agrees, for every workspace crate."""
    lock = root / "Cargo.lock"
    if not lock.is_file():
        return ["Cargo.lock: missing"]
    text = lock.read_text(encoding="utf-8")
    findings = []
    for name in workspace_crate_names(root):
        match = re.search(
            rf'^name = "{re.escape(name)}"\nversion = "([^"]*)"', text, re.MULTILINE
        )
        if match is None:
            findings.append(f"Cargo.lock: no entry for {name}")
        elif match.group(1) != want:
            findings.append(
                f"Cargo.lock: {name} v{match.group(1)}, expected {want} "
                f"(run `cargo update --workspace`)"
            )
    return findings


GATES = [
    ("versions (release.yml verify-version)", check_versions),
    ("CHANGELOG section extracts non-empty", check_changelog),
    ("CHANGELOG compare links", check_compare_links),
    ("Cargo.lock agrees", check_lockfile),
]


def run_gates(root: Path, want: str, quiet: bool = False) -> list[str]:
    all_findings = []
    for label, gate in GATES:
        findings = gate(root, want)
        all_findings.extend(findings)
        if not quiet:
            mark = "FAIL" if findings else "ok  "
            print(f"  {mark}  {label}")
            for finding in findings:
                print(f"          {finding}")
    return all_findings


# ── self-test ────────────────────────────────────────────────────────────────

FIXTURE_MANIFESTS = {
    "Cargo.toml": (
        '[package]\nname = "mermaid-cli"\nversion = "{v}"\n\n'
        "[dependencies]\n"
        'mermaid-domain = {{ path = "crates/mermaid-domain", version = "{v}" }}\n'
        'mermaid-runtime = {{ path = "crates/mermaid-runtime", version = "{v}" }}\n'
    ),
    "crates/mermaid-domain/Cargo.toml": (
        '[package]\nname = "mermaid-domain"\nversion = "{v}"\n\n'
        "[dependencies]\n"
        'mermaid-runtime = {{ path = "../mermaid-runtime", version = "{v}" }}\n'
    ),
    "crates/mermaid-runtime/Cargo.toml": (
        '[package]\nname = "mermaid-runtime"\nversion = "{v}"\n'
    ),
}

FIXTURE_LOCK = (
    "[[package]]\n"
    'name = "mermaid-cli"\nversion = "{v}"\n\n'
    "[[package]]\n"
    'name = "mermaid-domain"\nversion = "{v}"\n\n'
    "[[package]]\n"
    'name = "mermaid-runtime"\nversion = "{v}"\n'
)


def write_fixture(root: Path, version: str, released: str | None) -> None:
    """A miniature workspace at `version`.

    `released` is the version whose CHANGELOG section exists — pass the OLD
    version to model an un-bumped tree, or the target to model a cut one.
    """
    for rel, template in FIXTURE_MANIFESTS.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(template.format(v=version), encoding="utf-8")
    (root / "Cargo.lock").write_text(FIXTURE_LOCK.format(v=version), encoding="utf-8")

    sections = "## [Unreleased]\n\n"
    if released:
        sections += f"## [{released}] - 2026-01-01\n\n### Fixed\n\n- a real entry.\n\n"
    # A PRIOR release section, always. Not decoration: it is what bounds the
    # target section. Without it the extraction walks past an empty section
    # into the link block and reads as non-empty — which made an earlier
    # version of this fixture unable to model a blank release note at all.
    # Every real CHANGELOG in Keep a Changelog format has one.
    sections += "## [0.0.9] - 2025-01-01\n\n### Fixed\n\n- a prior entry.\n\n"
    links = f"[Unreleased]: https://example.invalid/compare/v{version}...HEAD\n"
    if released:
        links += f"[{released}]: https://example.invalid/compare/v0.0.9...v{released}\n"
    links += "[0.0.9]: https://example.invalid/compare/v0.0.8...v0.0.9\n"
    (root / "CHANGELOG.md").write_text(
        f"# Changelog\n\n{sections}{links}", encoding="utf-8"
    )


def self_test() -> int:
    """Prove the gates fire on a known-bad tree and stay quiet on a good one.

    The matched pair is the point: passing on the good tree alone would also
    be true of a script that checked nothing.
    """
    want = "0.23.0"
    old = "0.22.0"
    failures = []
    tmp = Path(tempfile.mkdtemp(prefix="mermaid-preflight-selftest-"))
    try:
        # 1. Un-bumped tree: every gate must fire.
        bad = tmp / "bad"
        bad.mkdir()
        write_fixture(bad, version=old, released=old)
        expected = {
            check_versions: 6,  # 3 package + 3 intra-workspace deps
            check_changelog: 1,
            check_compare_links: 2,  # missing [0.23.0], stale [Unreleased]
            check_lockfile: 3,  # every crate in the fixture lock
        }
        for gate, count in expected.items():
            findings = gate(bad, want)
            if len(findings) != count:
                failures.append(
                    f"{gate.__name__} on the un-bumped tree: expected {count} "
                    f"findings, got {len(findings)}: {findings}"
                )

        # 2. Correctly cut tree: every gate must go quiet.
        good = tmp / "good"
        good.mkdir()
        write_fixture(good, version=want, released=want)
        for gate, _ in expected.items():
            findings = gate(good, want)
            if findings:
                failures.append(
                    f"{gate.__name__} on the cut tree: expected no findings, "
                    f"got {findings}"
                )

        # 3. The section that EXISTS but is empty. This is the case gate 2 is
        # really for, and the dangerous one: a header is present, so anything
        # that only asked "is there a `## [VERSION]` header?" would pass while
        # the release shipped blank notes.
        blank = tmp / "blank"
        blank.mkdir()
        write_fixture(blank, version=want, released=want)
        changelog = blank / "CHANGELOG.md"
        changelog.write_text(
            changelog.read_text(encoding="utf-8").replace(
                "### Fixed\n\n- a real entry.\n", "   \n\t\n"
            ),
            encoding="utf-8",
        )
        empty = check_changelog(blank, want)
        if len(empty) != 1:
            failures.append(
                f"an empty `## [{want}]` section must be caught, got: {empty}"
            )

        # 4. The partial bump that gate 1's second half exists for: manifests
        # bumped, one intra-workspace `version =` left behind.
        partial = tmp / "partial"
        partial.mkdir()
        write_fixture(partial, version=want, released=want)
        manifest = partial / "crates/mermaid-domain/Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                f'version = "{want}" }}', f'version = "{old}" }}'
            ),
            encoding="utf-8",
        )
        stale = check_versions(partial, want)
        if len(stale) != 1 or "mermaid-runtime" not in stale[0]:
            failures.append(
                f"a stale intra-workspace pin must be caught, got: {stale}"
            )
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    if failures:
        print("release-readiness SELF-TEST FAILED:")
        for failure in failures:
            print(f"  {failure}")
        return 1
    print("release-readiness self-test OK (gates fire on a bad tree, quiet on a cut one)")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Verify this tree is ready to tag, before `git tag`."
    )
    parser.add_argument(
        "version", nargs="?", help="target version, `0.23.0` or `v0.23.0`"
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="prove the gates fail on an un-bumped fixture; needs no version",
    )
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if not args.version:
        parser.error("a version is required (or use --self-test)")

    # release.yml strips a leading `v` from the tag; accept both spellings.
    want = args.version.lstrip("v")
    root = Path(__file__).resolve().parents[2]

    print(f"release readiness for v{want} (in {root}):")
    findings = run_gates(root, want)
    if findings:
        print(f"\nNOT READY to tag v{want}: {len(findings)} problem(s) above.")
        print("Fix them, then re-run. Tagging now would fail release.yml after")
        print("the GitHub release and its binaries had already shipped.")
        return 1
    print(f"\nready to tag v{want}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
