#!/bin/sh
# Install the latest `orchestrator` release on Linux or macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/Mattbusel/tokio-prompt-orchestrator/main/install.sh | sh
#
# Downloads the archive for this OS and CPU from GitHub Releases, checks it
# against SHA256SUMS.txt, and copies the binaries into ~/.local/bin (override
# with INSTALL_DIR=/some/dir). Pin a version with VERSION=v1.4.2.
set -eu

REPO="Mattbusel/tokio-prompt-orchestrator"
NAME="orchestrator"
BINS="orchestrator replay coordinator validate"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "needs '$1'; install it and run again"; }

need curl
need tar
need uname

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)  case "$arch" in
            x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
            *) die "no prebuilt Linux binary for $arch yet; use: cargo install tokio-prompt-orchestrator --features web-api" ;;
          esac ;;
  Darwin) case "$arch" in
            arm64|aarch64) target="aarch64-apple-darwin" ;;
            x86_64)        target="x86_64-apple-darwin" ;;
            *) die "unknown Mac CPU $arch" ;;
          esac ;;
  MINGW*|MSYS*|CYGWIN*) die "on Windows use PowerShell: irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex" ;;
  *) die "unsupported OS $os; use: cargo install tokio-prompt-orchestrator --features web-api" ;;
esac

if [ -n "${VERSION:-}" ]; then
  tag="$VERSION"
else
  # releases/latest redirects to .../tag/<tag>; read the tag from the final URL
  tag="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" | sed 's#.*/tag/##')"
  [ -n "$tag" ] || die "could not find the latest release"
fi

asset="$NAME-$tag-$target.tar.gz"
base="https://github.com/$REPO/releases/download/$tag"
tmp="$(mktemp -d 2>/dev/null || mktemp -d -t orchestrator)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading $asset"
curl -fsSL "$base/$asset" -o "$tmp/$asset" || die "download failed: $base/$asset"
curl -fsSL "$base/SHA256SUMS.txt" -o "$tmp/SHA256SUMS.txt" || die "download failed: $base/SHA256SUMS.txt"

expected="$(grep " $asset\$" "$tmp/SHA256SUMS.txt" | awk '{print $1}')"
[ -n "$expected" ] || die "$asset is not listed in SHA256SUMS.txt"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
else
  die "needs sha256sum or shasum to verify the download"
fi
[ "$expected" = "$actual" ] || die "checksum mismatch for $asset (expected $expected, got $actual)"
say "Checksum OK"

tar xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$INSTALL_DIR"
for b in $BINS; do
  src="$tmp/$NAME-$tag-$target/$b"
  [ -f "$src" ] || continue
  cp "$src" "$INSTALL_DIR/$b"
  chmod +x "$INSTALL_DIR/$b"
done
say "Installed $("$INSTALL_DIR/orchestrator" --version) to $INSTALL_DIR"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) say ""
     say "$INSTALL_DIR is not on your PATH. Add this line to ~/.bashrc or ~/.zshrc:"
     say "  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
say ""
say "Try it with no API key:  orchestrator --provider echo"
