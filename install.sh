#!/bin/sh
# Mermaid installer (macOS / Linux). Downloads the latest (or a pinned) release
# binary for this platform from GitHub Releases, verifies it against
# SHA256SUMS, and installs `mermaid` + `mermaidd` onto your PATH. No Rust or
# cargo required.
#
#   curl -fsSL https://noahsabaj.github.io/mermaid-cli/install.sh | sh
#
# Environment overrides:
#   MERMAID_VERSION=v0.11.0        install a specific tag (default: latest)
#   MERMAID_INSTALL_DIR=DIR        install location (default: $HOME/.local/bin)
#   MERMAID_TARGET=linux-x86_64    force the platform asset (default: detected)
#   MERMAID_NO_MODIFY_PATH=1       skip the PATH guidance message
set -eu

REPO="noahsabaj/mermaid-cli"

say() { printf '%s\n' "$*"; }
err() { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- required tools --------------------------------------------------------
if command -v curl >/dev/null 2>&1; then
  download() { curl -fsSL -o "$1" "$2"; }
elif command -v wget >/dev/null 2>&1; then
  download() { wget -qO "$1" "$2"; }
else
  err "need curl or wget to download"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  err "need sha256sum or shasum to verify the download"
fi

command -v tar >/dev/null 2>&1 || err "need tar to extract the archive"

# --- platform detection ----------------------------------------------------
target="${MERMAID_TARGET:-}"
if [ -z "$target" ]; then
  os=$(uname -s)
  arch=$(uname -m)
  case "$os" in
    Linux) plat=linux ;;
    Darwin) plat=macos ;;
    MINGW* | MSYS* | CYGWIN*)
      err "Windows detected — use install.ps1 instead:
  irm https://noahsabaj.github.io/mermaid-cli/install.ps1 | iex" ;;
    *) err "unsupported OS: $os" ;;
  esac
  case "$arch" in
    x86_64 | amd64) cpu=x86_64 ;;
    aarch64 | arm64) cpu=aarch64 ;;
    *) err "unsupported architecture: $arch" ;;
  esac
  target="${plat}-${cpu}"
fi
asset="mermaid-${target}.tar.gz"

if [ -n "${MERMAID_VERSION:-}" ]; then
  base="https://github.com/${REPO}/releases/download/${MERMAID_VERSION}"
else
  base="https://github.com/${REPO}/releases/latest/download"
fi

dir="${MERMAID_INSTALL_DIR:-$HOME/.local/bin}"

# --- download + verify -----------------------------------------------------
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t mermaid) || err "could not create temp dir"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading ${asset}…"
download "$tmp/$asset" "$base/$asset" || err "download failed: $base/$asset"
download "$tmp/SHA256SUMS" "$base/SHA256SUMS" || err "download failed: $base/SHA256SUMS"

want=$(grep " ${asset}\$" "$tmp/SHA256SUMS" | awk '{print $1}')
[ -n "$want" ] || err "no checksum listed for ${asset}"
got=$(sha256 "$tmp/$asset")
[ "$want" = "$got" ] || err "checksum mismatch for ${asset}
  expected: $want
  actual:   $got"
say "Checksum verified."

# --- extract + install -----------------------------------------------------
tar -xzf "$tmp/$asset" -C "$tmp" || err "could not extract ${asset}"
mkdir -p "$dir" || err "could not create install dir: $dir"
for bin in mermaid mermaidd; do
  [ -f "$tmp/$bin" ] || err "archive is missing $bin"
  cp "$tmp/$bin" "$dir/$bin.tmp" && chmod 0755 "$dir/$bin.tmp" && mv -f "$dir/$bin.tmp" "$dir/$bin" \
    || err "could not install $bin to $dir (try: sudo, or set MERMAID_INSTALL_DIR)"
done

say "Installed mermaid + mermaidd to $dir"
"$dir/mermaid" --version 2>/dev/null || true

# --- PATH guidance ---------------------------------------------------------
case ":$PATH:" in
  *":$dir:"*) ;;
  *)
    if [ -z "${MERMAID_NO_MODIFY_PATH:-}" ]; then
      say ""
      say "$dir is not on your PATH. Add it, e.g.:"
      say "  echo 'export PATH=\"$dir:\$PATH\"' >> ~/.profile && export PATH=\"$dir:\$PATH\""
    fi
    ;;
esac

say ""
say "Done. Run 'mermaid' to start, or 'mermaid update' to upgrade later."
