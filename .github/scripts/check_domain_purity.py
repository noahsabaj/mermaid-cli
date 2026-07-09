#!/usr/bin/env python3
"""Guard: `src/domain` must stay a pure MVU core — no I/O, no wall clock.

The reducer is a pure `fn update(State, Msg) -> (State, Vec<Cmd>)`; effects are
data (`Cmd`) executed in the impure shell (`src/effect`). This guard fails CI if
production code under `src/domain` reaches for the filesystem, network, process
spawning, an async runtime, or the wall clock (the reducer injects `state.now`
instead, so `--replay` stays deterministic).

Test modules (`#[cfg(test)]`, by convention at the end of a file) are exempt.
"""

import re
import sys
from pathlib import Path

# (pattern, human name). Word-ish boundaries keep these from matching inside
# longer identifiers or comments about them.
FORBIDDEN = [
    (r"\bstd::fs\b", "std::fs"),
    (r"\bstd::net\b", "std::net"),
    (r"\bstd::process\b", "std::process"),
    (r"\btokio::", "tokio::"),
    (r"\breqwest\b", "reqwest"),
    (r"\bSystemTime::now\b", "SystemTime::now"),
    (r"\bInstant::now\b", "Instant::now"),
]

ROOT = Path("src/domain")


def production_prefix(text: str) -> str:
    """Everything before the first `#[cfg(test)]` — the production portion.

    Domain files keep their tests in a single trailing `#[cfg(test)] mod tests`,
    so truncating there exempts test setup (which legitimately stamps clocks).
    """
    idx = text.find("#[cfg(test)]")
    return text if idx == -1 else text[:idx]


def main() -> int:
    violations = []
    files = sorted(ROOT.rglob("*.rs"))
    for path in files:
        prod = production_prefix(path.read_text(encoding="utf-8"))
        for lineno, line in enumerate(prod.splitlines(), 1):
            stripped = line.strip()
            if stripped.startswith("//"):
                continue
            for pattern, name in FORBIDDEN:
                if re.search(pattern, line):
                    violations.append(f"  {path}:{lineno}: `{name}` -> {stripped}")

    if violations:
        print("domain/ purity violations (move I/O + the wall clock to the effect layer):")
        print("\n".join(violations))
        return 1
    print(f"domain/ purity OK ({len(files)} files scanned)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
