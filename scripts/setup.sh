#!/bin/sh
# keyboard-follows-mouse: macOS setup check. Read-only apart from building into ./target.
#   sh scripts/setup.sh            check the toolchain, build (cargo build --release), run `kfm-config doctor`
#   sh scripts/setup.sh --help
# It never installs anything (no Rust, no Karabiner-Elements, no Xcode tools), never touches
# Karabiner, launchd, permissions or your settings. Works from a folder whose path has spaces and
# from a ZIP download (no .git needed).
# Exit status: 0 = ready (doctor found no problem), 1 = needs attention (a prerequisite is missing
# or doctor reported problems; read its list), 2 = the build failed.
set -u
case "${1:-}" in
  "") ;;
  -h|--help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 0;;
  *) echo "usage: sh scripts/setup.sh [--help]" >&2; exit 2;;
esac
ROOT="$(cd "$(dirname "$0")/.." && pwd)" || { echo "cannot find the repository folder" >&2; exit 1; }
[ -f "$ROOT/Cargo.toml" ] || { echo "not the repository folder: $ROOT (Cargo.toml missing)" >&2; exit 1; }

echo "keyboard-follows-mouse setup check"
echo
echo "Status and limits: this is an experimental hobby project, provided as is. Only Mac -> Windows has run"
echo "on real hardware, and only in one session; Mac -> Mac real key injection and the Windows -> Mac"
echo "source mode have never run on hardware, and Windows -> Windows is not a target. See the status table"
echo "in README.md ('Status - read this first') before relying on it. Unofficial: not affiliated with Logitech."
echo
echo "This script only checks and builds into: $ROOT/target"
echo "(the build downloads Rust crates from crates.io; nothing else on this Mac is changed)"
echo

OS="$(uname -s)"; ARCH="$(uname -m)"
echo "system: $OS $ARCH"
if [ "$OS" != Darwin ]; then
  echo "This script is for macOS. On Windows use: powershell -ExecutionPolicy Bypass -File scripts\\setup.ps1" >&2
  echo "Linux is not supported." >&2
  exit 1
fi

# cargo may be installed but not on PATH in this shell (rustup puts it in ~/.cargo/bin).
if ! command -v cargo >/dev/null 2>&1 && [ -x "$HOME/.cargo/bin/cargo" ]; then
  PATH="$HOME/.cargo/bin:$PATH"; export PATH
  echo "note: using cargo from $HOME/.cargo/bin (not on your PATH in this shell)"
fi
if ! command -v cargo >/dev/null 2>&1 || ! command -v rustc >/dev/null 2>&1; then
  echo "MISSING: Rust (cargo / rustc) is not installed. Nothing was installed by this script."
  echo "  next: install it yourself with rustup, then run this script again:"
  echo "        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh     (see https://rustup.rs first)"
  echo "        or, with Homebrew: brew install rustup && rustup default stable"
  echo "  undo: rustup self uninstall"
  exit 1
fi
RV="$(rustc --version 2>/dev/null)"
echo "rust: $RV"
# Edition 2024 needs Rust 1.85 or newer.
MINOR="$(printf '%s\n' "$RV" | sed -n 's/^rustc 1\.\([0-9][0-9]*\)\..*/\1/p')"
if [ -n "$MINOR" ] && [ "$MINOR" -lt 85 ]; then
  echo "TOO OLD: this project needs Rust 1.85 or newer (edition 2024)."
  echo "  next: rustup update stable      undo: rustup toolchain install <old version>"
  exit 1
fi

if xcode-select -p >/dev/null 2>&1; then
  echo "xcode command line tools: found"
else
  echo "MISSING: Xcode command line tools (the linker cargo needs). Nothing was installed by this script."
  echo "  next: xcode-select --install   (a system dialog asks for your confirmation), then run this again"
  echo "  undo: sudo rm -rf /Library/Developer/CommandLineTools   (only if you want them gone)"
  exit 1
fi

if [ -d "/Applications/Karabiner-Elements.app" ]; then
  echo "Karabiner-Elements: found (path check only)"
else
  echo "Karabiner-Elements: NOT found at /Applications/Karabiner-Elements.app"
  echo "  a Mac that SENDS keys needs it (https://karabiner-elements.pqrs.org/); a receiver does not. Install it yourself if needed."
fi
echo

echo "building: cargo build --release"
if ! ( cd "$ROOT" && cargo build --release ); then
  echo "BUILD FAILED. Read the first error above; kfm-config doctor was not run." >&2
  echo "  undo: nothing to undo (only $ROOT/target was written; delete it to start clean)" >&2
  exit 2
fi
echo

KFM_CONFIG="$ROOT/target/release/kfm-config"
[ -x "$KFM_CONFIG" ] || { echo "build finished but $KFM_CONFIG is missing" >&2; exit 2; }
DOCTOR_OUT="$("$KFM_CONFIG" doctor)"
STATUS=$?
printf '%s\n' "$DOCTOR_OUT"
echo
FOREIGN="$(printf '%s\n' "$DOCTOR_OUT" | grep 'an existing installation was found' | head -n 1)"
if [ -n "$FOREIGN" ]; then
  echo "*** STOP ***  $FOREIGN"
  echo "*** Another kfm/r75 installation is loaded on this Mac. Tell the user and ask what to do BEFORE asking setup questions,"
  echo "*** creating settings or running any install step. Do not overwrite it; this script changed nothing but ./target."
  echo
fi
if [ -n "$FOREIGN" ]; then
  echo "setup check: STOPPED. The build succeeded, but the existing installation above needs the user's decision first."
elif [ $STATUS -eq 0 ]; then
  echo "setup check: ready. Next: \"$KFM_CONFIG\" show   (or ask your assistant to continue, see AGENTS.md)."
else
  echo "setup check: the build succeeded; doctor listed what still needs attention above (each item says what to do next and how to undo)."
  echo "On a fresh download the first item is normally 'settings do not exist yet': create them with"
  echo "  \"$KFM_CONFIG\" init --role mac-source      (or receiver)"
fi
exit $STATUS
