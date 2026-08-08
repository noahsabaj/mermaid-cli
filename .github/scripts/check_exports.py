#!/usr/bin/env python3
"""Guard: a crate root may not re-export a name nothing uses.

The four crates in this workspace carry no API-stability promise (see
`AGENTS.md`), so a `pub use` in a crate root is not a contract with anyone —
it exists to give code *inside* the workspace a shorter path. A re-export with
no consumer is therefore dead surface: it survives `cargo build`, it survives
`-D warnings` (a `pub` item is reachable by definition), and it accumulates.

The measured cost of not having this check: 34 such names, including all twelve
`*Repo` types in `mermaid-runtime`'s root and six `OUTCOME_*` constants. Worse
than the count, the flat aliases had drifted *asymmetrically* — the root
forwarded three of `redact`'s four functions, so `crate::utils::redact_json`
resolved and `crate::utils::redact_json_text` did not, for no reason a reader
could discover.

`cargo-public-api` and `cargo-semver-checks` both answer a different question:
they guard an API this project explicitly does not promise, and
`semver-checks` actively conflicts with the "delete cleanly, never deprecate"
rule. This finds the defect that was actually there, with no dependency.

WHAT IS CHECKED. Only *explicit* re-exports in a crate root (`src/lib.rs`,
`crates/*/src/lib.rs`): `pub use path::Name;`, `pub use path::Name as Alias;`,
and each name inside `pub use path::{A, B as C};`. A glob (`pub use path::*;`)
names nothing, so there is nothing to check and nothing to blame.

WHAT COUNTS AS A CONSUMER. Any other mention of the name, as a whole word, in
any `.rs` file in the workspace — `src/`, `crates/*/src/`, `tests/`,
`benches/`. Deliberately generous: this guard's job is to catch a name with
*zero* reachable users, and being generous is what keeps it free of the false
positives that get a check disabled. A name used only in a doc link or a test
is a name someone can find, which is the whole point of the re-export.

Findings key on `(path, name)` — no line numbers, so reordering an import
block does not churn the baseline.
"""

import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import ratchet  # noqa: E402

# `pub use` up to its terminating `;`. `re.S` so a brace-wrapped list that
# rustfmt has split across lines is one match.
PUB_USE = re.compile(r"^[ \t]*pub(?:\([^)]*\))?\s+use\s+(.+?);", re.M | re.S)
# Inside a `{...}` list: `A`, `A as B`, and the nested `a::{B}` form.
LEAF = re.compile(r"([A-Za-z_][A-Za-z0-9_]*)(?:\s+as\s+([A-Za-z_][A-Za-z0-9_]*))?")


def exported_names(stmt: str) -> list[str]:
    """The names a single `pub use ...` statement binds into the crate root.

    An alias binds the alias, not the original: `pub use x::Foo as Bar;`
    creates `Bar`, and it is `Bar` that has to have a consumer.
    """
    stmt = " ".join(stmt.split())
    if stmt.endswith("*") or "::*" in stmt:
        return []  # a glob names nothing; nothing to attribute
    if "{" not in stmt:
        m = re.search(r"([A-Za-z_][A-Za-z0-9_]*)(?:\s+as\s+([A-Za-z_][A-Za-z0-9_]*))?$", stmt)
        if not m:
            return []
        return [m.group(2) or m.group(1)]

    # Everything from the first `{` to the last `}`, minus the path segments
    # that precede a nested `::{`.
    body = stmt[stmt.index("{") + 1 : stmt.rindex("}")]
    body = re.sub(r"[A-Za-z_][A-Za-z0-9_]*\s*::\s*\{", "{", body)
    names: list[str] = []
    for chunk in re.split(r"[,{}]", body):
        chunk = chunk.strip()
        if not chunk or chunk == "self":
            continue
        m = LEAF.fullmatch(chunk)
        if not m:
            continue
        names.append(m.group(2) or m.group(1))
    return names


def main(argv: list[str]) -> int:
    roots = [Path("src")] + sorted(p / "src" for p in Path("crates").glob("*"))
    lib_files = [r / "lib.rs" for r in roots if (r / "lib.rs").is_file()]
    if not lib_files:
        print("check_exports: no crate roots found — the guard would pass vacuously")
        return 1

    scan_dirs = [d for d in roots + [Path("tests"), Path("benches")] if d.is_dir()]
    corpus = {
        f: f.read_text(encoding="utf-8")
        for d in scan_dirs
        for f in sorted(d.rglob("*.rs"))
    }

    findings: dict[str, int] = {}
    occurrences: dict[str, list[str]] = {}

    for lib in lib_files:
        text = corpus.get(lib) or lib.read_text(encoding="utf-8")
        # The re-export statements themselves are not consumers of the names
        # they bind — a root full of `pub use` would otherwise vouch for itself.
        without_reexports = PUB_USE.sub("", text)
        for m in PUB_USE.finditer(text):
            for name in exported_names(m.group(1)):
                word = re.compile(rf"\b{re.escape(name)}\b")
                used = any(
                    word.search(without_reexports if f == lib else body)
                    for f, body in corpus.items()
                )
                if used:
                    continue
                key = f"unused-export|{lib.as_posix()}|{name}"
                findings[key] = findings.get(key, 0) + 1
                line = text.count("\n", 0, m.start()) + 1
                occurrences.setdefault(key, []).append(
                    f"{lib.as_posix()}:{line}: `{name}` is re-exported and never named"
                )

    return ratchet.ratchet(
        "exports", "crate-root re-exports", findings, occurrences, argv
    )


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
