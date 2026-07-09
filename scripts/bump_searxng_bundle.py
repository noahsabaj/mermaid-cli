#!/usr/bin/env python3
"""Pin a mermaid-searxng bundle release into src/searxng/bundle_manifest.rs.

Rewrites BUNDLE_VERSION and each target's sha256 from a published SHA256SUMS
file. Invoked by the mermaid-searxng release CI (the automated bump PR), the
analog of mermaid-cli's own brew/scoop/winget bumps.

Usage:
    scripts/bump_searxng_bundle.py <tag> <path-to-SHA256SUMS>
"""

import pathlib
import re
import sys

MANIFEST = pathlib.Path("src/searxng/bundle_manifest.rs")
ASSET_RE = re.compile(r"^mermaid-searxng-(?P<target>.+?)\.(?:tar\.zst|zip)$")


def parse_sha256sums(text: str) -> dict[str, str]:
    """Map target -> sha256 from `<hex>  mermaid-searxng-<target>.<ext>` lines."""
    shas: dict[str, str] = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) != 2:
            continue
        digest, name = parts[0], parts[1].lstrip("*")
        m = ASSET_RE.match(name)
        if m:
            shas[m.group("target")] = digest.lower()
    return shas


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    tag, sums_path = sys.argv[1], sys.argv[2]
    shas = parse_sha256sums(pathlib.Path(sums_path).read_text())
    if not shas:
        print(f"error: no mermaid-searxng-* checksums in {sums_path}", file=sys.stderr)
        return 1

    text = MANIFEST.read_text()
    text, n = re.subn(
        r'(BUNDLE_VERSION: &str = ")[^"]*(")', rf"\g<1>{tag}\g<2>", text
    )
    if n != 1:
        print("error: BUNDLE_VERSION line not found in the manifest", file=sys.stderr)
        return 1
    for target, digest in shas.items():
        pattern = rf'("{re.escape(target)}" => ")[0-9a-fA-F]{{64}}(")'
        text, n = re.subn(pattern, rf"\g<1>{digest}\g<2>", text)
        if n != 1:
            print(f"error: no sha slot for target {target}", file=sys.stderr)
            return 1

    MANIFEST.write_text(text)
    print(f"pinned bundle {tag}: {', '.join(sorted(shas))}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
