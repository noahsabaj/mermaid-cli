#!/usr/bin/env python3
"""Guard: the lint-suppression budget only shrinks.

`AGENTS.md` says "decompose into helpers (clippy caps functions at 100 lines)".
That was false for the whole life of the repo: `.clippy.toml` set
`too-many-lines-threshold = 100`, but `clippy::too_many_lines` is a *pedantic*
lint, no manifest carried a `[lints]` table, and CI ran stock `clippy::all`. So
the threshold configured a lint that never ran, `update_step` reached 774 lines,
and the one `#[allow(clippy::too_many_lines)]` in the tree suppressed nothing.

The lint is on now. Going over the cap means adding an `#[expect(...)]` AND
raising a number in `.github/baselines/expect_budget.txt` — a visible act
rather than a silent one, and the number only ever goes down.

`#[expect]` over `#[allow]` on purpose: `unfulfilled_lint_expectations` warns
when a suppression stops being necessary, so shortening a function past the cap
tells you to delete its attribute. The suppression cleans itself up; an
`#[allow]` would sit there forever claiming a problem that no longer exists.
That is also why `#[allow(clippy::...)]` is counted here alongside `#[expect]`:
the budget is for suppressions in general, and the cheapest way to dodge a
shrinking `#[expect]` budget would be to spell it `#[allow]` instead.

Findings key on `(kind, lint, file)` — no line numbers, so moving a function
inside a file does not churn the baseline.
"""

import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import ratchet  # noqa: E402

# `#[expect(clippy::too_many_lines, reason = "...")]`, `#[allow(clippy::foo)]`,
# and the multi-lint forms. Captures the attribute kind and each clippy lint
# named inside it.
SUPPRESSION = re.compile(r"#\[(expect|allow)\(([^\]]*)\)\]", re.S)
CLIPPY_LINT = re.compile(r"\bclippy::([a-z_]+)")


def main(argv: list[str]) -> int:
    roots = [Path("src")] + sorted(p / "src" for p in Path("crates").glob("*"))
    files = sorted(f for root in roots if root.is_dir() for f in root.rglob("*.rs"))

    findings: dict[str, int] = {}
    occurrences: dict[str, list[str]] = {}

    for path in files:
        text = path.read_text(encoding="utf-8")
        for m in SUPPRESSION.finditer(text):
            kind, body = m.group(1), m.group(2)
            for lint in CLIPPY_LINT.findall(body):
                key = f"{kind}|clippy::{lint}|{path.as_posix()}"
                findings[key] = findings.get(key, 0) + 1
                line = text.count("\n", 0, m.start()) + 1
                occurrences.setdefault(key, []).append(
                    f"{path.as_posix()}:{line}: #[{kind}(clippy::{lint}, ...)]"
                )

    return ratchet.ratchet(
        "expect_budget", "lint-suppression budget", findings, occurrences, argv
    )


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
