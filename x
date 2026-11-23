#!/usr/bin/env bash
# Build, install and check ograph from source on Linux, macOS and Windows (Git Bash).
# Building here means a corporate laptop needs no downloaded executables, only cargo and a
# crate registry it can reach (usually Artifactory).
#
#   ./x doctor             where cargo, the build, the crates and ogrep come from; run it first
#   ./x fetch              download every locked crate (shows the registry serves them)
#   ./x build | release    debug or release build of ograph
#   ./x install [dir]      release build, then copy ograph to dir (default ~/bin on
#                          Windows, which Git Bash has on PATH, ~/.cargo/bin elsewhere)
#   ./x install-skill [dir]  copy skills/ograph into dir (default ~/.claude/skills and
#                          ~/.agents/skills)
#   ./x test [args]        cargo test --workspace --locked
#   ./x vendor             cargo vendor into ./vendor, for offline builds
#   ./x <anything else>    passed to cargo (clippy, fmt, tree, ...)
#
# ograph needs ogrep 0.2 or newer on PATH to read the service catalog; `./x doctor` checks it.
#
# Registry: cargo uses the machine's own ~/.cargo/config.toml, which on a corporate laptop
# normally already replaces crates.io with Artifactory. To point one run somewhere without
# touching that file, set
#   OGRAPH_CARGO_REGISTRY=sparse+https://<host>/artifactory/api/cargo/<repo>/index/
# and, if it needs a token, CARGO_REGISTRIES_OGRAPH_MIRROR_TOKEN.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) HOST=windows ;;
  Darwin)               HOST=macos ;;
  *)                    HOST=linux ;;
esac

if [ "$HOST" = windows ]; then
  EXE=.exe
  MANIFEST="$(cygpath -w "$HERE/Cargo.toml")"
  # A repo on the WSL share (//wsl.localhost/...) needs target/ on a Windows disk: link.exe
  # reads the leading / of such a path as an option, and the share has no file locks for cargo.
  if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    case "$HERE" in
      //*) export CARGO_TARGET_DIR="$(cygpath -w "$HOME/.ograph-target")" ;;
    esac
  fi
  BIN_DEFAULT="$HOME/bin"
else
  EXE=
  MANIFEST="$HERE/Cargo.toml"
  BIN_DEFAULT="${CARGO_HOME:-$HOME/.cargo}/bin"
fi
# Pinned so `install` copies the binary this build wrote, whatever target-dir a cargo config sets.
if [ -z "${CARGO_TARGET_DIR:-}" ]; then
  CARGO_TARGET_DIR="${CARGO_BUILD_TARGET_DIR:-$HERE/target}"
  [ "$HOST" = windows ] && CARGO_TARGET_DIR="$(cygpath -w "$CARGO_TARGET_DIR")"
  export CARGO_TARGET_DIR
fi
TARGET_U="$(if [ "$HOST" = windows ]; then cygpath -u "$CARGO_TARGET_DIR"; else echo "$CARGO_TARGET_DIR"; fi)"
RELEASE_BIN="$TARGET_U/release/ograph$EXE"
CARGO_CONFIG="${CARGO_HOME:-$HOME/.cargo}/config.toml"

case "${1:-help}" in
  help|-h|--help) sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
esac

if ! command -v cargo >/dev/null 2>&1; then
  echo "x: no cargo on PATH. Install Rust 1.85 or newer with rustup (on Windows, the MSVC toolchain)." >&2
  exit 1
fi

REGISTRY_ARGS=()
if [ -n "${OGRAPH_CARGO_REGISTRY:-}" ]; then
  REGISTRY_ARGS=(
    --config "registries.ograph-mirror.index=\"$OGRAPH_CARGO_REGISTRY\""
    --config "source.ograph-mirror.registry=\"$OGRAPH_CARGO_REGISTRY\""
    --config 'source.crates-io.replace-with="ograph-mirror"'
  )
fi

# --manifest-path goes right after the subcommand: after a `--` it would reach the test binary.
run_cargo() {
  local sub="$1"; shift
  cargo ${REGISTRY_ARGS[@]+"${REGISTRY_ARGS[@]}"} "$sub" --manifest-path "$MANIFEST" "$@"
}

# Windows refuses to replace a running .exe, and `ograph serve` runs until stopped.
refuse_if_running() {
  [ "$HOST" = windows ] || return 0
  local live
  # No /FI filter: Git Bash would rewrite /FI into a path.
  live="$(tasklist 2>/dev/null | grep -ci '^ograph\.exe' || true)"
  [ "${live:-0}" -eq 0 ] && return 0
  echo "x: ograph.exe is still running ($live process(es)), and Windows will not replace a running .exe." >&2
  echo "   Stop 'ograph serve' with Ctrl-C in its window, or end them all with: taskkill //IM ograph.exe //F" >&2
  exit 1
}

# Prints a problem and returns 1 when ogrep is missing or older than 0.2.
check_ogrep() {
  local bin="${OGRAPH_OGREP:-ogrep}" version
  if ! version="$("$bin" --version 2>/dev/null)"; then
    echo "ogrep:         MISSING. ograph reads the service catalog with ogrep; without it every fact"
    echo "               about a service is quarantined. Install ogrep 0.2+ and put it on PATH,"
    echo "               or set OGRAPH_OGREP=/path/to/ogrep."
    return 1
  fi
  case "$version" in
    "ogrep 0.0."*|"ogrep 0.1."*) echo "ogrep:         $version is too old; ograph needs 0.2 or newer"; return 1 ;;
  esac
  echo "ogrep:         $version ($(command -v "$bin"))"
}

case "${1:-help}" in
  fetch)   shift; run_cargo fetch --locked "$@"
           echo "every locked crate is available" ;;

  build)   shift; run_cargo build --locked -p ograph-cli "$@"
           echo "binary: $TARGET_U/debug/ograph$EXE" ;;

  release) shift; run_cargo build --release --locked -p ograph-cli "$@"
           echo "binary: $RELEASE_BIN" ;;

  install) shift
           dest="${1:-$BIN_DEFAULT}"
           refuse_if_running
           run_cargo build --release --locked -p ograph-cli
           mkdir -p "$dest"
           cp -f "$RELEASE_BIN" "$dest/"
           echo "installed ograph$EXE to $dest"
           "$dest/ograph$EXE" --version
           case ":$PATH:" in
             *":$dest:"*) echo "run 'hash -r', then: ograph --help" ;;
             *) echo "note: $dest is not on PATH. Add to your shell profile:"
                echo "        export PATH=\"$dest:\$PATH\"" ;;
           esac
           ogrep_msg="$(check_ogrep)" || printf '\n%s\n' "$ogrep_msg" ;;

  install-skill)
           shift
           dirs=("$@")
           [ ${#dirs[@]} -gt 0 ] || dirs=("$HOME/.claude/skills" "$HOME/.agents/skills")
           for d in "${dirs[@]}"; do
             mkdir -p "$d"
             rm -rf "$d/ograph"
             cp -r "$HERE/skills/ograph" "$d/ograph"
             echo "installed the ograph skill to $d/ograph"
           done
           echo "restart your agent to load it" ;;

  test)    shift; run_cargo test --workspace --locked "$@" ;;

  vendor)  shift
           vendor_dir="$HERE/vendor"; [ "$HOST" = windows ] && vendor_dir="$(cygpath -w "$vendor_dir")"
           run_cargo vendor --locked --versioned-dirs "$vendor_dir" "$@" >/dev/null
           echo "vendored every locked crate into ./vendor. To build from it offline, add to"
           echo "$HERE/.cargo/config.toml:"
           echo ""
           echo '  [source.crates-io]'
           echo '  replace-with = "vendored-sources"'
           echo ''
           echo '  [source.vendored-sources]'
           echo '  directory = "vendor"' ;;

  doctor)  status=0
           echo "host:          $HOST"
           echo "toolchain:     $(cargo --version 2>&1) / $(rustc -vV 2>/dev/null | sed -n 's/^host: //p')"
           echo "build output:  $TARGET_U"
           echo "install to:    $BIN_DEFAULT"
           if [ -n "${OGRAPH_CARGO_REGISTRY:-}" ]; then
             echo "registry:      $OGRAPH_CARGO_REGISTRY (from OGRAPH_CARGO_REGISTRY)"
           elif [ -f "$CARGO_CONFIG" ] && grep -q 'replace-with' "$CARGO_CONFIG"; then
             echo "registry:      crates.io replaced in $CARGO_CONFIG:"
             grep -n -A1 'replace-with\|^\[source\.' "$CARGO_CONFIG" | sed 's/^/                 /'
           else
             echo "registry:      crates.io (no replacement in $CARGO_CONFIG)"
           fi
           check_ogrep || status=1
           if command -v ograph >/dev/null 2>&1; then
             echo "ograph:        $(ograph --version) ($(command -v ograph))"
           else
             echo "ograph:        not installed yet (./x install)"
           fi
           if command -v pi >/dev/null 2>&1; then
             echo "pi:            $(command -v pi) (optional: model-assisted facts and the pi agent)"
           else
             echo "pi:            not found (optional: only model-assisted facts and the pi agent need it)"
           fi
           echo ""
           echo "checking that every locked crate resolves..."
           if fetched="$(run_cargo fetch --locked 2>&1)"; then
             echo "ok: all crates available"
           else
             echo "FAILED: cargo cannot fetch the locked crates. Output:" >&2
             printf '%s\n' "$fetched" | tail -15 | sed 's/^/  /' >&2
             echo "" >&2
             echo "On a corporate network, point cargo at the Artifactory Cargo remote, in" >&2
             echo "$CARGO_CONFIG:" >&2
             echo "" >&2
             echo '  [registries.artifactory]' >&2
             echo '  index = "sparse+https://<host>/artifactory/api/cargo/<repo>/index/"' >&2
             echo '' >&2
             echo '  [source.artifactory]' >&2
             echo '  registry = "sparse+https://<host>/artifactory/api/cargo/<repo>/index/"' >&2
             echo '' >&2
             echo '  [source.crates-io]' >&2
             echo '  replace-with = "artifactory"' >&2
             echo "" >&2
             echo "or set OGRAPH_CARGO_REGISTRY for a single run (see the top of ./x)." >&2
             status=1
           fi
           exit "$status" ;;

  *)       sub="$1"; shift; run_cargo "$sub" "$@" ;;
esac
