#!/usr/bin/env python3
"""Shared baseline machinery for the source guards.

Every guard in this directory follows the same contract:

  * The guard produces a mapping of *finding key* -> occurrence count.
  * `.github/baselines/<name>.txt` records the debt that predates the guard.
  * CI fails on a key that is not in the baseline          (new debt)
  * CI fails on a count above the baseline's               (regression)
  * CI fails on a baseline key that no longer fires        (stale entry)
  * CI fails on a count below the baseline's               (uncounted progress)

The last two are the point. A baseline that only ever gets appended to is a
place debt goes to be forgotten; a baseline that must be *edited down* when you
fix something puts the number in the diff, where a reviewer sees it move.

`python3 .github/scripts/<guard>.py --write-baseline` regenerates one file;
`just ratchet` regenerates all of them. The failure message always names the
command, so nobody has to remember it.

KEYS CARRY NO LINE NUMBERS. A key is `<rule>|<path>[|<detail>]`. Line numbers
churn on every edit and would turn these files into merge-conflict generators;
they belong in the human-readable output, not in the recorded key.
"""

from pathlib import Path

BASELINE_DIR = Path(".github/baselines")


def read_baseline(name: str) -> dict[str, int]:
    """Parse `.github/baselines/<name>.txt` into {key: count}.

    A missing file reads as "no known debt", so a brand-new guard fails on
    everything it finds until someone records a baseline deliberately.
    """
    path = BASELINE_DIR / f"{name}.txt"
    if not path.exists():
        return {}
    out: dict[str, int] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        # `rpartition` and not `split`: keys contain spaces (a rationale-bearing
        # detail segment can), the trailing count never does.
        key, _, count = line.rpartition(" ")
        out[key] = int(count)
    return out


def render_baseline(
    findings: dict[str, int], title: str, regen: str = "just ratchet"
) -> str:
    total = sum(findings.values())
    body = "".join(f"{k} {findings[k]}\n" for k in sorted(findings))
    # The `N keys / M occurrences` line is rewritten on every regeneration, so
    # the debt counter lands in every diff that touches this file. That single
    # line is what makes the mechanism social rather than merely technical.
    return (
        f"# Ratchet baseline: {title}\n"
        f"#\n"
        f"# Debt that predates the guard. This file may only SHRINK.\n"
        f"# Regenerate with: {regen}\n"
        f"#\n"
        f"# {len(findings)} keys / {total} occurrences\n"
        f"{body}"
    )


def write_baseline(
    name: str, findings: dict[str, int], title: str, regen: str = "just ratchet"
) -> None:
    path = BASELINE_DIR / f"{name}.txt"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(render_baseline(findings, title, regen), encoding="utf-8")


def ratchet(
    name: str,
    title: str,
    findings: dict[str, int],
    occurrences: dict[str, list[str]],
    argv: list[str],
    regen: str = "just ratchet",
) -> int:
    """Compare `findings` against the recorded baseline and print a verdict.

    `occurrences[key]` is a list of `path:line: text` strings for humans; it is
    never persisted, only printed.

    `regen` is the command a failure tells the reader to run. It is a parameter
    and not a constant because one guard (`check_clippy_ratchet.py`) rebuilds
    the workspace and so cannot live in `just ratchet` with the instant ones.
    A message naming the wrong command is worse than none: it sends the reader
    to a recipe that will not touch the file they were just told to update.
    """
    if "--write-baseline" in argv:
        write_baseline(name, findings, title, regen)
        print(
            f"{name}: wrote {len(findings)} keys "
            f"/ {sum(findings.values())} occurrences"
        )
        return 0

    base = read_baseline(name)
    new = {k: v for k, v in findings.items() if k not in base}
    worse = {k: v for k, v in findings.items() if k in base and v > base[k]}
    stale = {k: v for k, v in base.items() if k not in findings}
    better = {k: v for k, v in findings.items() if k in base and v < base[k]}

    if new or worse:
        print(f"{title}: NEW violations — this is the gate, not a suggestion.\n")
        for key, count in sorted({**new, **worse}.items()):
            was = f" (baseline {base[key]})" if key in base else ""
            print(f"  {key}  x{count}{was}")
            for line in occurrences.get(key, [])[:8]:
                print(f"      {line}")
        print(
            f"\nFix them, or — if this is deliberate — run `{regen}` "
            f"and explain the new line in the PR description."
        )
        return 1

    if stale or better:
        print(f"{title}: the baseline is now STALE (you fixed something).\n")
        for key, count in sorted(stale.items()):
            print(f"  fixed entirely, delete the line: {key} {count}")
        for key, count in sorted(better.items()):
            print(f"  {base[key]} -> {count}, lower the number: {key}")
        print(
            f"\nRun `{regen}` and commit "
            f"`.github/baselines/{name}.txt`."
        )
        return 1

    print(
        f"{title}: OK — {len(base)} known keys / {sum(base.values())} "
        f"occurrences remaining (.github/baselines/{name}.txt)"
    )
    return 0
