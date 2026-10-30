#!/usr/bin/env bash
# Perennos DEMON MODE: one-click install for macOS and Linux.
#   bash install.sh            create .venv, install the package, add a launcher
#   bash install.sh --dry-run  print the steps and change nothing
# Env INSTALL_SHORTCUT_DIR: write the launcher into that folder (used by the tests).
set -eu

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"
DRY=0
if [ "${1:-}" = "--dry-run" ]; then DRY=1; fi

run() {
  echo "+ $*"
  if [ "$DRY" = 0 ]; then "$@"; fi
}

PY=""
for candidate in python3 python; do
  if command -v "$candidate" >/dev/null 2>&1 \
     && "$candidate" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)' >/dev/null 2>&1; then
    PY="$candidate"
    break
  fi
done
if [ -z "$PY" ]; then
  echo "Python 3.10 or newer was not found. Install it, then run this again:" >&2
  echo "  Debian/Ubuntu: sudo apt install python3 python3-venv" >&2
  echo "  Fedora:        sudo dnf install python3" >&2
  echo "  Arch:          sudo pacman -S python" >&2
  echo "  macOS:         brew install python" >&2
  exit 1
fi

VPY="$ROOT/.venv/bin/python"
if [ ! -d "$ROOT/.venv" ]; then run "$PY" -m venv "$ROOT/.venv"; fi
run "$VPY" -m pip install --disable-pip-version-check "$ROOT"

if [ "$(uname -s)" = "Darwin" ]; then
  LAUNCHER="${INSTALL_SHORTCUT_DIR:-$HOME/Desktop}/Demon Mode.command"
  CONTENT="#!/bin/sh
exec \"$VPY\" -m demon_mode gui"
else
  LAUNCHER="${INSTALL_SHORTCUT_DIR:-$HOME/.local/share/applications}/demon-mode.desktop"
  CONTENT="[Desktop Entry]
Type=Application
Name=Demon Mode
Comment=Local page for Perennos DEMON MODE on 127.0.0.1
Exec=\"$VPY\" -m demon_mode gui
Icon=$ROOT/brand/icon.ico
Terminal=false
Categories=Utility;"
fi

echo "+ write $LAUNCHER"
if [ "$DRY" = 0 ]; then
  mkdir -p "$(dirname "$LAUNCHER")"
  printf '%s\n' "$CONTENT" > "$LAUNCHER"
  if [ "$(uname -s)" = "Darwin" ]; then chmod +x "$LAUNCHER"; fi
fi

echo
if [ "$DRY" = 1 ]; then echo "Dry run: nothing was changed."; fi
echo "Next step: open the Demon Mode launcher ($LAUNCHER), or run: $VPY -m demon_mode gui"
