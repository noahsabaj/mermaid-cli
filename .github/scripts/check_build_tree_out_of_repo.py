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
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

CONFIG = ROOT / ".cargo" / "config.toml"
CONSUMERS = [
    ROOT / ".github" / "workflows" / "release.yml",
    ROOT / "justfile",
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

    if failures:
        print("build tree escaped its declared location:", file=sys.stderr)
        print("\n".join(failures), file=sys.stderr)
        return 1

    print(f"build tree out of repo: OK — {target_dir} agreed by "
          f"{len(CONSUMERS)} consumer(s) + .cargo/config.toml")
    return 0


if __name__ == "__main__":
    sys.exit(main())
