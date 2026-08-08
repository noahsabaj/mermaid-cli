#!/usr/bin/env python3
"""Guard: the layered modules only reach DOWNWARD, and stay pure.

Replaces `check_domain_purity.py`, which could only see I/O tokens. Dependency
DIRECTION is the property AGENTS.md actually claims, and it is invisible to a
token scan: `use crate::app::Config` contains no forbidden word. That blind spot
let the pure MVU core accumulate edges into `app`, `session`, `providers`, and
`render` — two of them cycles — while the guard reported OK on every run.

Scope is deliberate. Only the modules AGENTS.md makes a promise about are
layered; the shell modules (`app`, `providers`, `effect`, `session`, ...) are
siblings that may freely reference each other. A guard that also ranked those
would land with a pile of extra baseline entries nobody has a reason to pay
down, and a baseline nobody pays down is wallpaper.

Findings ratchet against `.github/baselines/layering.txt` — see `ratchet.py`.
"""

import re
import sys
from pathlib import Path
from typing import NamedTuple

sys.path.insert(0, str(Path(__file__).parent))
import ratchet  # noqa: E402


class Layer(NamedTuple):
    may_use: frozenset[str]  # module names reachable from here
    pure: bool  # subject to the impure-token scan
    why: str  # printed on violation


LAYERS: dict[str, Layer] = {
    # -- rank 1: the pure MVU core -----------------------------------------
    "src/domain": Layer(
        may_use=frozenset({"models", "constants", "utils", "runtime", "prompts"}),
        pure=True,
        why=(
            "`src/domain` is the pure MVU core: `fn update(State, Msg) -> "
            "(State, Vec<Cmd>)`. Effects are DATA — if the reducer needs the "
            "shell, emit a `Cmd` and handle it in `src/effect`. If it needs a "
            "TYPE that lives above it (a config struct, a conversation "
            "record), move the type DOWN; do not reach up for it. "
            "`crate::runtime` is allowed because the names domain uses from it "
            "(`SafetyMode`, `TaskStatus`, the storage record structs) are plain "
            "value types."
        ),
    ),
    # -- rank 2: the pure view ---------------------------------------------
    "src/render": Layer(
        may_use=frozenset({"domain", "models", "constants", "utils", "runtime"}),
        pure=True,
        why=(
            "`render(&State) -> Frame` is a pure function of domain state. "
            "Everything the frame shows must arrive through `State` or "
            "`RenderCache` — resolved once at startup by the shell, not read "
            "from the environment or the clock per frame."
        ),
    ),
    # -- rank 0: leaf ------------------------------------------------------
    "src/prompts.rs": Layer(
        may_use=frozenset({"models", "constants", "utils"}),
        pure=True,
        why=(
            "`src/prompts.rs` is a leaf string table. `src/domain` imports "
            "`get_system_prompt` from it, so it must stay below domain."
        ),
    ),
}

# Module names that resolve through a re-export facade in `src/lib.rs`.
ALIASES = {"mermaid_model": "models", "mermaid_runtime": "runtime"}

IMPURE = [
    # filesystem / network / process
    (r"\bstd::fs\b", "std::fs"),
    (r"\bstd::net\b", "std::net"),
    (r"\bstd::process\b", "std::process"),
    (r"\bstd::io\b", "std::io"),
    (r"\bstd::thread\b", "std::thread"),
    # `std::env::consts::{OS,ARCH}` are compile-time `&'static str`s, not a
    # runtime environment read — `src/prompts.rs` uses them to name the host
    # platform in the system prompt, which is deterministic. Everything else
    # under `std::env` reads the live process environment.
    (r"\bstd::env\b(?!::consts\b)", "std::env"),
    (r"\bFile::(?:open|create)\b", "File::open/create"),
    (r"\bCommand::new\b", "Command::new"),
    (r"\brusqlite\b", "rusqlite"),
    (r"\breqwest\b", "reqwest"),
    (r"\bkeyring\b", "keyring"),
    # async — the reducer is synchronous, by rule 1 of reducer.rs's header
    (r"\btokio::", "tokio::"),
    (r"\.await\b", ".await"),
    (r"\basync\s+(?:fn|move|\{)", "async"),
    # the wall clock — `--replay` determinism reads `state.now`
    (r"\bSystemTime::now\b", "SystemTime::now"),
    (r"\bInstant::now\b", "Instant::now"),
    (r"\b(?:Utc|Local)::now\b", "chrono now"),
    # other nondeterminism
    (r"\bgetrandom\b", "getrandom"),
    (r"\brand::", "rand::"),
    (r"\bunsafe\b", "unsafe"),
]
# Deliberately absent, with reasons, so nobody "helpfully" adds them:
#   PathBuf::from / Path::new — pure string->path conversion. The reducer builds
#     `Cmd::CreateRuntimeCheckpoint { paths }` this way and that is exactly
#     right: the Cmd is data, the Cmd's *execution* is I/O.
#   Duration, SystemTime (the type) — value types; only `::now()` reads a clock.
#   include_str! — compile-time, not runtime I/O.

# `#[cfg(test)]`, `#[cfg(all(test, unix))]`, `#[cfg(any(test, feature = "x"))]`.
CFG_TEST = re.compile(r"#\[cfg\((?:[^()]|\([^()]*\))*\btest\b(?:[^()]|\([^()]*\))*\)\]")
CFG_TEST_MOD = re.compile(
    r"#\[cfg\((?:[^()]|\([^()]*\))*\btest\b(?:[^()]|\([^()]*\))*\)\]\s*"
    r"(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;"
)


def blank_span(chars: list[str], start: int, end: int) -> None:
    """Replace a span with spaces, preserving newlines so line numbers survive."""
    for i in range(start, end):
        if chars[i] != "\n":
            chars[i] = " "


def strip_noncode(text: str) -> str:
    """Blank comments, strings, and char literals; keep every newline in place.

    Nesting-aware for block comments (Rust allows `/* /* */ */`) and hash-aware
    for raw strings. Without this the guard reports a trailing
    `// ... tokio:: ...` comment as a violation, and misses nothing inside
    `/* */` because it never entered one.
    """
    chars = list(text)
    n = len(chars)
    i = 0
    while i < n:
        c = chars[i]
        # line comment
        if c == "/" and i + 1 < n and chars[i + 1] == "/":
            j = text.find("\n", i)
            j = n if j == -1 else j
            blank_span(chars, i, j)
            i = j
            continue
        # block comment, nesting
        if c == "/" and i + 1 < n and chars[i + 1] == "*":
            depth = 1
            j = i + 2
            while j < n and depth:
                if chars[j] == "/" and j + 1 < n and chars[j + 1] == "*":
                    depth += 1
                    j += 2
                elif chars[j] == "*" and j + 1 < n and chars[j + 1] == "/":
                    depth -= 1
                    j += 2
                else:
                    j += 1
            blank_span(chars, i, j)
            i = j
            continue
        # raw string: r"..." / r#"..."# / br##"..."##
        if c in "rb":
            k = i
            if chars[k] == "b" and k + 1 < n and chars[k + 1] == "r":
                k += 1
            if chars[k] == "r":
                h = k + 1
                while h < n and chars[h] == "#":
                    h += 1
                if h < n and chars[h] == '"':
                    hashes = "#" * (h - k - 1)
                    close = text.find('"' + hashes, h + 1)
                    j = n if close == -1 else close + 1 + len(hashes)
                    blank_span(chars, i, j)
                    i = j
                    continue
        # string literal
        if c == '"':
            j = i + 1
            while j < n:
                if chars[j] == "\\":
                    j += 2
                    continue
                if chars[j] == '"':
                    j += 1
                    break
                j += 1
            blank_span(chars, i, j)
            i = j
            continue
        # char literal vs lifetime: `'a'` and `'\n'` are literals, `'a` is not
        if c == "'":
            m = re.match(r"'(?:\\.|[^'\\])'", text[i : i + 8])
            if m:
                blank_span(chars, i, i + m.end())
                i += m.end()
                continue
        i += 1
    return "".join(chars)


def blank_cfg_test_items(text: str) -> tuple[str, list[int]]:
    """Blank every `#[cfg(test)]` item by brace matching.

    Returns the blanked text plus the start offsets of each attribute found, so
    the caller can assert the file-shape convention independently.

    The predecessor truncated at the FIRST `#[cfg(test)]`, which silently
    exempted any production code below a mid-file test module. This blanks each
    item precisely and leaves the rest of the file scanned.
    """
    chars = list(text)
    n = len(chars)
    starts: list[int] = []
    pos = 0
    while True:
        m = CFG_TEST.search("".join(chars), pos)
        if not m:
            break
        starts.append(m.start())
        j = m.end()
        # Skip whitespace and any further attributes stacked on the same item.
        while j < n:
            if chars[j].isspace():
                j += 1
                continue
            if chars[j] == "#":
                depth = 0
                while j < n:
                    if chars[j] == "[":
                        depth += 1
                    elif chars[j] == "]":
                        depth -= 1
                        if depth == 0:
                            j += 1
                            break
                    j += 1
                continue
            break
        # Consume the item: to the matching `}`, or to the first `;` at depth 0
        # (the `mod x;` / `use x;` declaration forms).
        brace = paren = 0
        seen_brace = False
        while j < n:
            ch = chars[j]
            if ch == "(":
                paren += 1
            elif ch == ")":
                paren -= 1
            elif ch == "{":
                brace += 1
                seen_brace = True
            elif ch == "}":
                brace -= 1
                if brace == 0 and seen_brace:
                    j += 1
                    break
            elif ch == ";" and brace == 0 and paren == 0:
                j += 1
                break
            j += 1
        blank_span(chars, m.start(), j)
        pos = j
    return "".join(chars), starts


def module_dir_for(path: Path) -> Path:
    """Where `mod NAME;` inside `path` looks for NAME."""
    if path.name in ("mod.rs", "lib.rs", "main.rs"):
        return path.parent
    return path.with_suffix("")


def test_only_files(all_files: list[Path]) -> set[Path]:
    """Files reachable only through a `#[cfg(test)] mod NAME;` declaration.

    Mandatory, not optional: `src/render/mod.rs` declares `bench` and
    `snapshots` this way, and `bench.rs` alone holds four `Instant::now()` calls
    that a naive scan reports as violations.
    """
    found: set[Path] = set()
    frontier = list(all_files)
    while frontier:
        path = frontier.pop()
        try:
            text = strip_noncode(path.read_text(encoding="utf-8"))
        except OSError:
            continue
        base = module_dir_for(path)
        for m in CFG_TEST_MOD.finditer(text):
            name = m.group(1)
            for cand in (base / f"{name}.rs", base / name / "mod.rs"):
                if cand.is_file() and cand not in found:
                    found.add(cand)
                    # A test-only mod.rs makes its whole subtree test-only.
                    if cand.name == "mod.rs":
                        found.update(
                            f for f in cand.parent.rglob("*.rs") if f != cand
                        )
                    frontier.append(cand)
    return found


def line_of(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def layer_for(path: Path) -> tuple[str, Layer] | None:
    posix = path.as_posix()
    for prefix, layer in LAYERS.items():
        if posix == prefix or posix.startswith(prefix + "/"):
            return prefix, layer
    return None


def own_module(prefix: str) -> str:
    return Path(prefix).stem if prefix.endswith(".rs") else Path(prefix).name


def main(argv: list[str]) -> int:
    roots = [Path("src")] + sorted(p / "src" for p in Path("crates").glob("*"))
    all_files = sorted(
        f for root in roots if root.is_dir() for f in root.rglob("*.rs")
    )

    # A guard whose scope quietly evaporates is worse than no guard.
    for prefix in LAYERS:
        if not any(layer_for(f) and layer_for(f)[0] == prefix for f in all_files):
            print(
                f"layering: the layer table names `{prefix}`, which resolves to "
                f"zero files. The tree moved and the guard stopped watching it. "
                f"Update LAYERS in {Path(__file__).name}."
            )
            return 1

    skip = test_only_files(all_files)

    findings: dict[str, int] = {}
    occurrences: dict[str, list[str]] = {}
    rationale: dict[str, str] = {}

    def record(key: str, line: int, path: Path, text_line: str, why: str) -> None:
        findings[key] = findings.get(key, 0) + 1
        occurrences.setdefault(key, []).append(
            f"{path.as_posix()}:{line}: {text_line.strip()}"
        )
        rationale[key] = why

    for path in all_files:
        hit = layer_for(path)
        if not hit or path in skip:
            continue
        prefix, layer = hit
        posix = path.as_posix()
        raw = path.read_text(encoding="utf-8")
        raw_lines = raw.splitlines()
        code, _ = blank_cfg_test_items(strip_noncode(raw))

        # No file-shape rule here, deliberately. An earlier draft required at
        # most one `#[cfg(test)]` per layered file, as the last item — the
        # convention the predecessor's truncate-at-first-occurrence silently
        # depended on. Two legitimate patterns violate it (`src/render/mod.rs`
        # declares `#[cfg(test)] mod bench;` and `mod snapshots;`;
        # `src/render/widgets/chat.rs:536` gates a single test helper fn), so
        # the rule would have landed as two baseline entries that can never
        # reach zero. `blank_cfg_test_items` blanks each item precisely instead
        # of truncating, so production code below a mid-file test module is
        # still scanned and the rule has nothing left to insure against.

        # -- import edges ---------------------------------------------------
        me = own_module(prefix)
        seen: set[tuple[str, int]] = set()

        def note_edge(target: str, offset: int) -> None:
            target = ALIASES.get(target, target)
            if target == me or target in layer.may_use:
                return
            line = line_of(code, offset)
            if (target, line) in seen:
                return
            seen.add((target, line))
            record(
                f"layer|{posix}|{target}",
                line,
                path,
                raw_lines[line - 1] if line <= len(raw_lines) else "",
                layer.why,
            )

        for m in re.finditer(r"crate::\{([^}]*)\}", code, re.S):
            for part in m.group(1).split(","):
                head = re.match(r"\s*([a-z_][a-z0-9_]*)", part)
                if head:
                    note_edge(head.group(1), m.start())
        for m in re.finditer(r"crate::([a-z_][a-z0-9_]*)", code):
            note_edge(m.group(1), m.start())
        for m in re.finditer(r"\b(mermaid_model|mermaid_runtime)::", code):
            note_edge(m.group(1), m.start())

        # -- impure tokens --------------------------------------------------
        if layer.pure:
            for pattern, name in IMPURE:
                lines_hit = {
                    line_of(code, m.start()) for m in re.finditer(pattern, code)
                }
                for line in sorted(lines_hit):
                    record(
                        f"impure|{posix}|{name}",
                        line,
                        path,
                        raw_lines[line - 1] if line <= len(raw_lines) else "",
                        layer.why,
                    )

    rc = ratchet.ratchet("layering", "layering + purity", findings, occurrences, argv)
    # Print the rationale for whatever the ratchet just rejected. The `why` is
    # the whole reason the layer table is a Python dict and not a TOML file.
    if rc:
        base = ratchet.read_baseline("layering")
        offenders = {
            k for k, v in findings.items() if k not in base or v > base.get(k, 0)
        }
        for key in sorted(offenders):
            print(f"\n{key}:\n  {rationale[key]}")
    return rc


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
