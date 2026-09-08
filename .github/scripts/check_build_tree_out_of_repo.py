#!/usr/bin/env python3
"""Guard: the build tree stays outside the working tree, in every file that names it.

A target dir inside the checkout is gigabytes of hashable content sitting in the
repository. `git stash -a` sweeps ignored files by design, so .gitignore is not
protection: one such sweep hashed 12,433 .rlib and binary blobs into the object
store and left 2.5 GiB of unreachable objects behind. `build.target-dir` in
.cargo/config.toml moves the tree out, which is what makes that unreachable.

The path has to be repeated in files cargo config does not reach:

  * release.yml       — packaging steps cd into the build tree, and
                        cargo-generate-rpm needs --target-dir spelled out
                        (cargo-deb reads cargo's config and does not).
  * justfile          — CLIPPY_RATCHET_TARGET_DIR is an explicit override, so it
                        does NOT inherit build.target-dir.

Every repetition is a chance for one to drift back to ./target and quietly put
the artifacts back inside the repo. This asserts they all agree, and that none
of them names an in-tree path.

It also asserts every Swatinem/rust-cache step passes `workspaces`. That action
does NOT read build.target-dir -- it derives the cache path from the workspace
root and caches `<workspace>/target`. Moving the build tree without telling it
made it cache a directory that no longer exists: every Rust job still passed,
having silently rebuilt from scratch. Roughly 90% of a CI leg is compilation
(see the env comment in rust.yml), so the failure is invisible in the check
marks and expensive in wall clock -- exactly the kind that survives review.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

CONFIG = ROOT / ".cargo" / "config.toml"
CONSUMERS = [
    ROOT / ".github" / "workflows" / "release.yml",
    ROOT / ".github" / "workflows" / "rust.yml",
    ROOT / "justfile",
]

# Every workflow that caches Rust build output.
CACHE_ACTION = "Swatinem/rust-cache"
CACHE_WORKFLOWS = [
    ROOT / ".github" / "workflows" / "release.yml",
    ROOT / ".github" / "workflows" / "rust.yml",
]


def declared_target_dir() -> str:
    """The single source of truth: build.target-dir in .cargo/config.toml."""
    text = CONFIG.read_text(encoding="utf-8")
    m = re.search(r"^\s*target-dir\s*=\s*\"([^\"]+)\"", text, re.MULTILINE)
    if not m:
        sys.exit(
            f"{CONFIG.relative_to(ROOT)}: no build.target-dir found.\n"
            "The build tree belongs outside the checkout; see this guard's docstring."
        )
    return m.group(1)


def cache_steps_declare_workspaces(target_dir: str) -> list[str]:
    """Every rust-cache step must point the action at the moved build tree.

    Parsed by indentation rather than with a YAML library, to keep these guards
    dependency-free (nothing here imports outside the stdlib).
    """
    want = f'workspaces: ". -> {target_dir}"'
    failures = []
    for path in CACHE_WORKFLOWS:
        lines = path.read_text(encoding="utf-8").split("\n")
        rel = path.relative_to(ROOT)
        for i, line in enumerate(lines):
            if CACHE_ACTION not in line or "uses:" not in line:
                continue
            col = line.index("uses:")
            # The step's own keys sit at `col`; a sibling step opens with "- "
            # at col - 2, and anything shallower has left the step entirely.
            block, j = [], i + 1
            while j < len(lines):
                nxt = lines[j]
                if not nxt.strip():
                    j += 1
                    continue
                indent = len(nxt) - len(nxt.lstrip())
                if indent < col or (indent == col - 2 and nxt.lstrip().startswith("- ")):
                    break
                block.append(nxt)
                j += 1
            if not any(want in b for b in block):
                failures.append(
                    f"  {rel}:{i + 1}: {CACHE_ACTION} step is missing `{want}`\n"
                    f"      without it the action caches <workspace>/target, "
                    f"which no longer exists -- the job silently rebuilds from scratch"
                )
    return failures


def main() -> int:
    target_dir = declared_target_dir()

    # An out-of-tree path must escape the workspace root. A bare or ./-rooted
    # path puts the artifacts straight back where the incident started.
    if not target_dir.startswith("../"):
        sys.exit(
            f"build.target-dir is {target_dir!r}, which resolves inside the checkout.\n"
            "Cargo resolves it against the workspace root, so an out-of-tree "
            "value has to start with '../'."
        )

    failures = []
    for path in CONSUMERS:
        text = path.read_text(encoding="utf-8")
        rel = path.relative_to(ROOT)

        # Any in-tree build path that is not a comment line.
        for n, line in enumerate(text.splitlines(), 1):
            if line.lstrip().startswith("#"):
                continue
            for bad in re.finditer(r"(?<![\w./-])\.?/?target/(?:[\w.-]+/)*", line):
                # `../mermaid-target/...` is the good path, not a hit.
                if bad.group(0).startswith("target/"):
                    failures.append(
                        f"  {rel}:{n}: names in-tree {bad.group(0)!r} "
                        f"(should live under {target_dir}/)"
                    )

        if target_dir not in text:
            failures.append(f"  {rel}: never mentions {target_dir!r}")

    failures += cache_steps_declare_workspaces(target_dir)

    if failures:
        print("build tree escaped its declared location:", file=sys.stderr)
        print("\n".join(failures), file=sys.stderr)
        return 1

    print(f"build tree out of repo: OK — {target_dir} agreed by "
          f"{len(CONSUMERS)} consumer(s) + .cargo/config.toml, "
          f"and every {CACHE_ACTION} step points at it")
    return 0


if __name__ == "__main__":
    sys.exit(main())
