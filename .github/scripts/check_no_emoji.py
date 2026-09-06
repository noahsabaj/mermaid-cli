#!/usr/bin/env python3
"""Guard: no emoji / pictographs in Mermaid's Rust source.

Mermaid's user-facing output is deliberately emoji-free (a hard product rule).
This scans every workspace crate's Rust source for emoji/pictograph codepoints.
It is NOT a blanket non-ASCII ban: box-drawing (U+2500-257F), arrows
(U+2190-21FF), the middot (U+00B7), typographic quotes, and the ellipsis all sit
below the flagged ranges and are legitimate TUI/text characters.

The scan covers `crates/*/src` as well as `src`. Scanning only `src` was
silently correct while that held every line of Rust in the repo, and silently
wrong the moment a module moved into a workspace crate — the guard would have
kept passing on a shrinking share of the code without saying so.
"""

import sys
import unicodedata
from pathlib import Path


def is_emoji(cp: int) -> bool:
    return (
        0x1F300 <= cp <= 0x1FAFF  # emoji, pictographs, symbols & pictographs ext
        or 0x2600 <= cp <= 0x27BF  # miscellaneous symbols + dingbats
        or cp == 0xFE0F  # emoji variation selector
        or 0x1F1E6 <= cp <= 0x1F1FF  # regional indicators (flag letters)
        # Misc Technical sits below the dingbat block, but this run carries
        # Emoji=Yes and renders as colour glyphs on some terminals (⏩ .. ⏺, ⌚ ⌛).
        # `⎿` (U+23BF) is outside it.
        or 0x23E9 <= cp <= 0x23FA
        or cp in (0x231A, 0x231B)
    )


def rust_sources() -> list[Path]:
    """Every Rust file in the workspace: the root crate plus `crates/*`."""
    roots = [Path("src")] + sorted(p / "src" for p in Path("crates").glob("*"))
    return sorted(f for root in roots if root.is_dir() for f in root.rglob("*.rs"))


def main() -> int:
    violations = []
    files = rust_sources()
    for path in files:
        for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            for col, ch in enumerate(line, 1):
                if is_emoji(ord(ch)):
                    name = unicodedata.name(ch, f"U+{ord(ch):04X}")
                    violations.append(f"  {path}:{lineno}:{col}: {ch!r} ({name})")

    if violations:
        print("emoji/pictograph found (Mermaid output must stay emoji-free):")
        print("\n".join(violations))
        return 1
    print(f"no emoji — OK ({len(files)} files scanned)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
