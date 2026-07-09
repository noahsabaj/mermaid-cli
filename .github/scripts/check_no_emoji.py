#!/usr/bin/env python3
"""Guard: no emoji / pictographs in Mermaid's Rust source.

Mermaid's user-facing output is deliberately emoji-free (a hard product rule).
This scans `src/**/*.rs` for emoji/pictograph codepoints. It is NOT a blanket
non-ASCII ban: box-drawing (U+2500-257F), arrows (U+2190-21FF), the middot
(U+00B7), typographic quotes, and the ellipsis all sit below the flagged ranges
and are legitimate TUI/text characters.
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
    )


def main() -> int:
    violations = []
    files = sorted(Path("src").rglob("*.rs"))
    for path in files:
        for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            for col, ch in enumerate(line, 1):
                if is_emoji(ord(ch)):
                    name = unicodedata.name(ch, f"U+{ord(ch):04X}")
                    violations.append(f"  {path}:{lineno}:{col}: {ch!r} ({name})")

    if violations:
        print("emoji/pictograph found in src/ (Mermaid output must stay emoji-free):")
        print("\n".join(violations))
        return 1
    print(f"no emoji in src/ — OK ({len(files)} files scanned)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
