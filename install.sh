#!/bin/sh
# Installs a prebuilt ograph from its GitHub releases, and ogrep too when it is missing:
#
#   curl -fsSL https://raw.githubusercontent.com/eusthace811/ograph/main/install.sh | sh
#
# Linux and macOS (x86_64, arm64) and Windows in Git Bash (x86_64). Settings:
#   OGRAPH_VERSION=v0.3.1       a release tag (default: the latest release)
#   OGRAPH_INSTALL_DIR=DIR      where to put it (default ~/.local/bin; ~/bin in Git Bash)
#   OGREP_VERSION=v0.4.0        the ogrep release to install when ogrep is missing or older than 0.4
# Where downloaded executables are not allowed, build from source instead: ./x install.
set -eu

REPO=eusthace811/ograph
BIN=ograph
VERSION="${OGRAPH_VERSION:-latest}"

say() { printf '%s\n' "$*" >&2; }
die() { say "install.sh: $*"; exit 1; }

case "$(uname -s)" in
  Linux)                OS=unknown-linux-musl ;;
  Darwin)               OS=apple-darwin ;;
  MINGW*|MSYS*|CYGWIN*) OS=pc-windows-msvc ;;
  *) die "no prebuilt release for $(uname -s); build from source with ./x install" ;;
esac
case "$(uname -m)" in
  x86_64|amd64)  ARCH=x86_64 ;;
  arm64|aarch64) ARCH=aarch64 ;;
  *) die "no prebuilt release for $(uname -m); build from source with ./x install" ;;
esac
EXE=
EXT=tar.gz
DEFAULT_DIR="$HOME/.local/bin"
if [ "$OS" = pc-windows-msvc ]; then
  ARCH=x86_64 # Windows on Arm runs x86_64 executables.
  EXE=.exe
  EXT=zip
  DEFAULT_DIR="$HOME/bin" # Git Bash puts ~/bin on PATH.
fi
TARGET="$ARCH-$OS"
DIR="${OGRAPH_INSTALL_DIR:-$DEFAULT_DIR}"

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 3 -o "$2" "$1" || die "could not download $1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1" || die "could not download $1"; }
else
  die "needs curl or wget"
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
else
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# install_release OWNER/REPO NAME TAG: downloads NAME-TARGET from that release, checks it
# against the release's SHA256SUMS and copies NAME into DIR.
install_release() {
  case "$3" in
    latest) url="https://github.com/$1/releases/latest/download" ;;
    *)      url="https://github.com/$1/releases/download/$3" ;;
  esac
  pkg="$2-$TARGET"
  say "downloading $pkg.$EXT from $1 ($3)"
  fetch "$url/$pkg.$EXT" "$TMP/$pkg.$EXT"
  fetch "$url/SHA256SUMS" "$TMP/SHA256SUMS"
  want="$(awk -v f="$pkg.$EXT" '$2 == f || $2 == "*" f { print $1 }' "$TMP/SHA256SUMS")"
  [ -n "$want" ] || die "$pkg.$EXT is not in the release's SHA256SUMS"
  [ "$(sha256 "$TMP/$pkg.$EXT")" = "$want" ] || die "$pkg.$EXT does not match its SHA256SUMS entry"
  if [ "$EXT" = zip ]; then
    powershell -NoProfile -Command "Expand-Archive -Force '$(cygpath -w "$TMP/$pkg.$EXT")' '$(cygpath -w "$TMP")'" ||
      die "could not unpack $pkg.zip"
  else
    tar -xzf "$TMP/$pkg.$EXT" -C "$TMP"
  fi
  mkdir -p "$DIR"
  cp -f "$TMP/$pkg/$2$EXE" "$DIR/$2$EXE" || die "could not write $DIR/$2$EXE (on Windows: is it running?)"
  chmod 755 "$DIR/$2$EXE"
  say "installed $("$DIR/$2$EXE" --version) to $DIR"
}

install_release "$REPO" "$BIN" "$VERSION"

# ograph reads the service catalog with ogrep: it needs 0.3 or newer, and 0.4 or newer to date dependencies.
if [ -z "${OGRAPH_OGREP:-}" ]; then
  have="$(PATH="$DIR:$PATH" ogrep --version 2>/dev/null || true)"
  case "$have" in
    ""|"ogrep 0.0."*|"ogrep 0.1."*|"ogrep 0.2."*|"ogrep 0.3."*) install_release eusthace811/ogrep ogrep "${OGREP_VERSION:-latest}" ;;
    *) say "found $have" ;;
  esac
fi

case ":$PATH:" in
  *":$DIR:"*) ;;
  *) say "note: $DIR is not on PATH. Add to your shell profile: export PATH=\"$DIR:\$PATH\"" ;;
esac
