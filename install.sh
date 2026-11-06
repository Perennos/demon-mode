#!/usr/bin/env bash
# Perennos DEMON MODE: one-click install for macOS and Linux. Never needs admin rights.
#   bash install.sh            install the demon binary into ~/.local/bin and add a launcher
#   bash install.sh --dry-run  print the steps and change nothing (no downloads either)
# 1. A prebuilt release binary for this OS and CPU, checked against the release's SHA256SUMS.
# 2. Otherwise, if cargo is installed, a local build from this checkout.
# 3. Otherwise, the one-line Rust install command, and a stop.
# Env INSTALL_BIN_DIR: install the binary there instead of ~/.local/bin.
# Env INSTALL_SHORTCUT_DIR: write the launcher into that folder (used by the tests).
# Env DEMON_INSTALL_FROM_SOURCE=1: skip the download and build with cargo.
set -eu

VERSION="0.2.0"
REPO="Perennos/demon-mode"
ROOT="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="${INSTALL_BIN_DIR:-$HOME/.local/bin}"
DEMON="$BIN_DIR/demon"
DRY=0
if [ "${1:-}" = "--dry-run" ]; then DRY=1; fi

run() {
  echo "+ $*"
  if [ "$DRY" = 0 ]; then "$@"; fi
}

release_target() {
  case "$(uname -s)/$(uname -m)" in
    Linux/x86_64) echo x86_64-unknown-linux-musl ;;
    Linux/aarch64 | Linux/arm64) echo aarch64-unknown-linux-musl ;;
    Darwin/arm64) echo aarch64-apple-darwin ;;
    Darwin/x86_64) echo x86_64-apple-darwin ;;
    *) echo "" ;;
  esac
}

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    return 1
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  else
    return 1
  fi
}

# Returns 1 (fall back to a local build) when no prebuilt binary applies; exits on a bad checksum.
install_prebuilt() {
  target="$(release_target)"
  if [ -z "$target" ] || [ "${DEMON_INSTALL_FROM_SOURCE:-}" = 1 ]; then return 1; fi
  name="demon-v$VERSION-$target"
  base="https://github.com/$REPO/releases/download/v$VERSION"
  if [ "$DRY" = 1 ]; then
    echo "+ download $base/$name.tar.gz and $base/SHA256SUMS"
    echo "+ verify the SHA-256, then install demon into $BIN_DIR (a missing release falls back to cargo)"
    return 0
  fi
  if ! command -v tar >/dev/null 2>&1; then return 1; fi
  work="$(mktemp -d)"
  if ! fetch "$base/SHA256SUMS" "$work/SHA256SUMS" || ! fetch "$base/$name.tar.gz" "$work/$name.tar.gz"; then
    echo "No prebuilt v$VERSION binary for $target was found; building from source instead."
    rm -rf "$work"
    return 1
  fi
  expected="$(awk -v file="$name.tar.gz" '$2 == file || $2 == "*" file { print $1 }' "$work/SHA256SUMS")"
  if ! actual="$(sha256_of "$work/$name.tar.gz")"; then
    echo "No SHA-256 tool (sha256sum or shasum) was found; building from source instead."
    rm -rf "$work"
    return 1
  fi
  if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
    rm -rf "$work"
    echo "The download of $name.tar.gz does not match SHA256SUMS. Nothing was installed." >&2
    exit 1
  fi
  # set -e does not apply inside a function used as a condition, so every step is checked here.
  if ! tar -xzf "$work/$name.tar.gz" -C "$work"; then
    rm -rf "$work"
    echo "Could not unpack $name.tar.gz. Nothing was installed." >&2
    exit 1
  fi
  if ! { mkdir -p "$BIN_DIR" && cp "$work/$name/demon" "$DEMON.new" && chmod 755 "$DEMON.new" && mv -f "$DEMON.new" "$DEMON"; }; then
    rm -rf "$work"
    echo "Could not write $DEMON. Nothing was installed." >&2
    exit 1
  fi
  rm -rf "$work"
  echo "Installed the verified v$VERSION release binary: $DEMON"
}

install_from_source() {
  if ! command -v cargo >/dev/null 2>&1; then
    echo "No prebuilt binary applies here and Rust is not installed. Install Rust (no admin needed), then run this again:" >&2
    echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
    exit 1
  fi
  echo "Building Demon Mode from source with cargo (a few minutes the first time)."
  run cargo build --release --locked --manifest-path "$ROOT/Cargo.toml"
  run mkdir -p "$BIN_DIR"
  run cp "$ROOT/target/release/demon" "$DEMON"
}

if ! install_prebuilt; then install_from_source; fi

if [ "$(uname -s)" = "Darwin" ]; then
  LAUNCHER="${INSTALL_SHORTCUT_DIR:-$HOME/Desktop}/Demon Mode.command"
  CONTENT="#!/bin/sh
exec \"$DEMON\" gui"
else
  LAUNCHER="${INSTALL_SHORTCUT_DIR:-$HOME/.local/share/applications}/demon-mode.desktop"
  CONTENT="[Desktop Entry]
Type=Application
Name=Demon Mode
Comment=Local page for Perennos DEMON MODE on 127.0.0.1
Exec=\"$DEMON\" gui
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
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "Note: $BIN_DIR is not on your PATH. Add it, or run $DEMON by its full path." ;;
esac
echo "Upgrading from the Python release? Your ledger stays where it was. Run: $DEMON hook-config --client claude"
echo "(or --client codex) and replace only the old 'python -m demon_mode' hook command; keep every other hook."
echo "Next step: open the Demon Mode launcher ($LAUNCHER), or run: $DEMON gui"
