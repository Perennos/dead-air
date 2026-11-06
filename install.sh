#!/usr/bin/env bash
# DEAD AIR install for macOS and Linux. "bash install.sh --dry-run" prints the steps and changes nothing.
# It installs one dead-air binary into ~/.local/bin: a release download whose SHA-256 is checked
# against the release's SHA256SUMS, or, when no release exists for this machine, a local
# `cargo install`. It never needs administrator rights and never installs FFmpeg for you.
set -eu

VERSION=0.3.0
REPO=${DEAD_AIR_REPO:-Perennos/dead-air}
BASE=${DEAD_AIR_RELEASE_BASE:-https://github.com/$REPO/releases/download/v$VERSION}
BIN_DIR=${DEAD_AIR_BIN_DIR:-$HOME/.local/bin}
DRY=0
if [ "${1:-}" = "--dry-run" ]; then DRY=1; fi
ROOT=$(cd "$(dirname "$0")" && pwd)
cd "$ROOT"

run() {
    if [ "$DRY" = 1 ]; then echo "[dry-run] $*"; else "$@"; fi
}

case "$(uname -s)/$(uname -m)" in
    Linux/x86_64 | Linux/amd64) TARGET=x86_64-unknown-linux-musl ;;
    Linux/aarch64 | Linux/arm64) TARGET=aarch64-unknown-linux-musl ;;
    Darwin/arm64 | Darwin/aarch64) TARGET=aarch64-apple-darwin ;;
    Darwin/x86_64) TARGET=x86_64-apple-darwin ;;
    *) TARGET= ;;
esac

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
    else return 1
    fi
}

fetch() { # fetch URL OUT; fails (without output) on HTTP errors such as 404
    if command -v curl >/dev/null 2>&1; then curl -fsSL --proto '=https,file' -o "$2" "$1" 2>/dev/null
    elif command -v wget >/dev/null 2>&1; then wget -q -O "$2" "$1"
    else return 1
    fi
}

echo "DEAD AIR $VERSION install"
if [ "$DRY" = 1 ]; then echo "[dry-run] Nothing will be changed."; fi
EXE="$BIN_DIR/dead-air"

echo "Step 1: the dead-air binary"
INSTALLED=0
ASSET=
if [ -n "$TARGET" ]; then ASSET="dead-air-v$VERSION-$TARGET.tar.gz"; fi
if [ "$DRY" = 1 ]; then
    if [ -n "$ASSET" ]; then
        echo "[dry-run] would download $BASE/$ASSET and $BASE/SHA256SUMS,"
        echo "[dry-run] check the SHA-256, and install dead-air to $EXE"
    fi
    if command -v cargo >/dev/null 2>&1; then
        echo "[dry-run] if no release exists: cargo install --locked --path $ROOT, then copy dead-air to $EXE"
    else
        echo "[dry-run] if no release exists and cargo is missing: print the rustup instruction and stop"
    fi
elif [ -n "$ASSET" ]; then
    WORK=$(mktemp -d)
    trap 'rm -rf "$WORK"' EXIT
    if fetch "$BASE/SHA256SUMS" "$WORK/SHA256SUMS" && fetch "$BASE/$ASSET" "$WORK/$ASSET"; then
        EXPECTED=$(awk -v name="$ASSET" '$2 == name || $2 == "*" name { print $1 }' "$WORK/SHA256SUMS")
        ACTUAL=$(sha256 "$WORK/$ASSET" || true)
        if [ -z "$EXPECTED" ] || [ -z "$ACTUAL" ] || [ "$EXPECTED" != "$ACTUAL" ]; then
            echo "The downloaded $ASSET does not match the release's SHA256SUMS. Nothing was installed."
            exit 1
        fi
        mkdir -p "$WORK/unpacked" "$BIN_DIR"
        tar -xzf "$WORK/$ASSET" -C "$WORK/unpacked"
        FOUND=$(find "$WORK/unpacked" -type f -name dead-air | head -n 1)
        if [ -z "$FOUND" ]; then echo "The release archive has no dead-air binary. Nothing was installed."; exit 1; fi
        install -m 0755 "$FOUND" "$EXE"
        echo "  verified release $VERSION ($TARGET, sha256 $ACTUAL)"
        INSTALLED=1
    else
        echo "  no release download for $TARGET; building from this checkout instead"
    fi
fi
if [ "$DRY" = 0 ] && [ "$INSTALLED" = 0 ]; then
    if command -v cargo >/dev/null 2>&1; then
        CARGO_ROOT=$(mktemp -d)
        cargo install --locked --path "$ROOT" --root "$CARGO_ROOT"
        mkdir -p "$BIN_DIR"
        install -m 0755 "$CARGO_ROOT/bin/dead-air" "$EXE"
        rm -rf "$CARGO_ROOT"
        INSTALLED=1
    else
        echo "Rust is needed to build DEAD AIR on this machine. Install it, then run this again:"
        echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
        exit 1
    fi
fi
if [ "$DRY" = 0 ]; then echo "  $EXE"; fi
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "  $BIN_DIR is not on PATH; add it, or use the full path above." ;;
esac

if ! command -v ffmpeg >/dev/null 2>&1 || ! command -v ffprobe >/dev/null 2>&1; then
    echo
    echo "ffmpeg and ffprobe must both be on PATH. This script does not install them."
    echo "Official downloads: https://ffmpeg.org/download.html"
    echo "  Debian/Ubuntu: sudo apt install ffmpeg"
    echo "  Fedora:        sudo dnf install ffmpeg"
    echo "  macOS:         brew install ffmpeg"
    echo
fi

echo "Step 2: launcher"
FLAG=
if grep -q '^\[\[streams\]\]' dead-air.toml 2>/dev/null; then FLAG=" --watch"; echo "  (dead-air.toml has a stream, so the launcher starts watching)"; fi
if [ "$(uname -s)" = "Darwin" ]; then
    DIR=${INSTALL_SHORTCUT_DIR:-$HOME/Desktop}
    FILE="$DIR/Dead Air.command"
    if [ "$DRY" = 1 ]; then
        echo "[dry-run] would write $FILE"
    else
        mkdir -p "$DIR"
        printf '#!/bin/bash\ncd "%s" || exit 1\nexec "%s" gui --config dead-air.toml%s\n' "$ROOT" "$EXE" "$FLAG" > "$FILE"
        chmod +x "$FILE"
        echo "  $FILE"
    fi
else
    DIR=${INSTALL_SHORTCUT_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/applications}
    FILE="$DIR/dead-air.desktop"
    if [ "$DRY" = 1 ]; then
        echo "[dry-run] would write $FILE"
    else
        mkdir -p "$DIR"
        printf '[Desktop Entry]\nType=Application\nName=Dead Air\nComment=DEAD AIR stream watchdog\nExec="%s" gui --config "%s/dead-air.toml"%s\nPath=%s\nIcon=%s/brand/favicon.svg\nTerminal=false\nCategories=AudioVideo;Monitor;\n' \
            "$EXE" "$ROOT" "$FLAG" "$ROOT" "$ROOT" > "$FILE"
        chmod +x "$FILE"
        echo "  $FILE"
    fi
fi

echo
if [ "$DRY" = 1 ]; then echo "[dry-run] Done. Nothing was changed."; exit 0; fi
echo "Done. Open Dead Air from the launcher above."
echo "Add your stream in the page, then press Start watching. The page runs the watcher for you."
echo "Without the page: $EXE watch --config dead-air.toml --continuous"
