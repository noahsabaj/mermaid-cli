#!/usr/bin/env python3
"""Guard: the pedantic/nursery lint debt only shrinks.

Two tiers of clippy, split by what a merge should wait on.

TIER 1 is in every manifest's `[lints.clippy]` table and runs on the `Clippy`
job under `-D warnings`. Six lints, chosen because a violation is a defect:
`too_many_lines`, `excessive_nesting`, `dbg_macro`, `todo`, `unimplemented`,
`mem_forget`. Blocking, and adds no CI seconds.

TIER 2 is this script. `pedantic` and `nursery` plus a handful of named lints
are worth *tracking* and not worth blocking on: they are style-and-taste
categories with real false-positive rates, and "how much pedantic debt is
there" does not change PR-to-PR in a way a merge should wait on. Turning them
on also changes clippy's fingerprint, which forces a full recompile — so
running it per-PR would cost ~200s on the critical path to re-answer a
question whose answer moves once a month. Same reasoning the repo already
applies to the beta and nightly test legs.

`--force-warn` and not `-W`: the manifests deny `warnings`, and a `deny` in
`mermaid-runtime` aborts the build before `mermaid-model` and `mermaid-cli` are
ever linted. That is not hypothetical — the first `too_many_lines` survey of
this workspace reported 3 violations because the bottom crate failed and masked
the other two. The real number was 59.

Findings key on the LINT ALONE — not `(lint, file)` like the other guards.
That is a deliberate departure. Keying per file produces 1,390 entries against
85, and every one of them churns when a file is split; the program that
introduced this guard split seven files in a single branch, and a baseline that
cannot survive that is a baseline nobody will regenerate. The per-file detail
is not lost, it is just not *persisted*: the failure output prints the offending
paths, exactly as the other guards print theirs.

Counts are deduped by source position first: cargo lints the same file once per
target (lib, test binary, bin), so an undeduped count reports the same line two
or three times.

NOT WIRED INTO `just ratchet`. Every other guard reads files and finishes in
milliseconds; this one changes clippy's lint fingerprint and rebuilds the
workspace, so folding it in would take `just ratchet` from instant to minutes.
It has its own recipes, `just clippy-debt` and `just clippy-debt-record`.
"""

import json
import os
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import ratchet  # noqa: E402

LINTS = [
    "clippy::pedantic",
    "clippy::nursery",
    "clippy::unwrap_used",
    "clippy::panic",
    "clippy::wildcard_enum_match_arm",
    "clippy::string_slice",
    "clippy::trivially_copy_pass_by_ref",
    "clippy::many_single_char_names",
]


def collect() -> tuple[dict[str, int], dict[str, list[str]]]:
    cmd = [
        "cargo",
        "clippy",
        "--workspace",
        "--all-targets",
        "--message-format=json",
        "--",
    ]
    for lint in LINTS:
        cmd += ["--force-warn", lint]

    proc = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8")
    if proc.returncode != 0 and not proc.stdout.strip():
        print("cargo clippy failed to run:\n" + proc.stderr[-4000:])
        raise SystemExit(2)

    seen: set[tuple[str, str, int, int]] = set()
    findings: dict[str, int] = {}
    occurrences: dict[str, list[str]] = {}

    for line in proc.stdout.splitlines():
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if msg.get("reason") != "compiler-message":
            continue
        body = msg.get("message") or {}
        code = (body.get("code") or {}).get("code") or ""
        if not code.startswith("clippy::"):
            continue
        spans = [s for s in body.get("spans", []) if s.get("is_primary")]
        if not spans:
            continue
        span = spans[0]
        path = Path(span["file_name"]).as_posix()
        # Registry sources reached through a path dependency are not ours.
        if path.startswith("/") or ":" in path.split("/")[0]:
            continue
        position = (code, path, span["line_start"], span["column_start"])
        if position in seen:
            continue
        seen.add(position)

        findings[code] = findings.get(code, 0) + 1
        occurrences.setdefault(code, []).append(
            f"{path}:{span['line_start']}: {body.get('message', '')}"
        )

    return findings, occurrences


def main(argv: list[str]) -> int:
    if not Path("Cargo.toml").is_file():
        print("check_clippy_ratchet: run from the workspace root")
        return 2
    # Keep the fingerprint-flipping build out of the shared target dir, so a
    # local run of this script does not force the next `cargo test` to rebuild
    # the world. CI has no such dir to protect and leaves this unset.
    if "CLIPPY_RATCHET_TARGET_DIR" in os.environ:
        os.environ["CARGO_TARGET_DIR"] = os.environ["CLIPPY_RATCHET_TARGET_DIR"]
    findings, occurrences = collect()
    return ratchet.ratchet(
        "clippy_pedantic",
        "pedantic + nursery debt",
        findings,
        occurrences,
        argv,
        regen="just clippy-debt-record",
    )


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
