#!/usr/bin/env bash
# DEAD AIR one-click install for macOS and Linux. "bash install.sh --dry-run" prints the steps and changes nothing.
# Needs Python 3.11 or newer (dead_air.py imports tomllib). It never installs Python or FFmpeg for you.
set -eu

DRY=0
if [ "${1:-}" = "--dry-run" ]; then DRY=1; fi
ROOT=$(cd "$(dirname "$0")" && pwd)
cd "$ROOT"

run() {
    if [ "$DRY" = 1 ]; then echo "[dry-run] $*"; else "$@"; fi
}

PY=
for candidate in python3 python; do
    if command -v "$candidate" >/dev/null 2>&1 &&
        "$candidate" -c 'import sys; sys.exit(sys.version_info < (3, 11))' >/dev/null 2>&1; then
        PY=$candidate
        break
    fi
done
if [ -z "$PY" ]; then
    echo "Python 3.11 or newer was not found. Install it, then run this again:"
    echo "  Debian/Ubuntu: sudo apt install python3 python3-venv"
    echo "  Fedora:        sudo dnf install python3"
    echo "  macOS:         brew install python"
    exit 1
fi
echo "DEAD AIR install, using $PY"
if [ "$DRY" = 1 ]; then echo "[dry-run] Nothing will be changed."; fi

echo "Step 1: virtual environment"
if [ ! -x .venv/bin/python ]; then run "$PY" -m venv .venv; fi
echo "Step 2: requirements.txt"
run .venv/bin/python -m pip install --disable-pip-version-check -r requirements.txt

if ! command -v ffmpeg >/dev/null 2>&1 || ! command -v ffprobe >/dev/null 2>&1; then
    echo
    echo "ffmpeg and ffprobe must both be on PATH. This script does not install them."
    echo "Official downloads: https://ffmpeg.org/download.html"
    echo "  Debian/Ubuntu: sudo apt install ffmpeg"
    echo "  Fedora:        sudo dnf install ffmpeg"
    echo "  macOS:         brew install ffmpeg"
    echo
fi

echo "Step 3: launcher"
PYV="$ROOT/.venv/bin/python"
if [ "$(uname -s)" = "Darwin" ]; then
    DIR=${INSTALL_SHORTCUT_DIR:-$HOME/Desktop}
    FILE="$DIR/Dead Air.command"
    if [ "$DRY" = 1 ]; then
        echo "[dry-run] would write $FILE"
    else
        mkdir -p "$DIR"
        printf '#!/bin/bash\ncd "%s" || exit 1\nexec "%s" dead_air.py gui --config dead-air.toml\n' "$ROOT" "$PYV" > "$FILE"
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
        printf '[Desktop Entry]\nType=Application\nName=Dead Air\nComment=DEAD AIR stream watchdog\nExec="%s" "%s/dead_air.py" gui --config "%s/dead-air.toml"\nPath=%s\nIcon=%s/brand/favicon.svg\nTerminal=false\nCategories=AudioVideo;Monitor;\n' \
            "$PYV" "$ROOT" "$ROOT" "$ROOT" "$ROOT" > "$FILE"
        chmod +x "$FILE"
        echo "  $FILE"
    fi
fi

echo
if [ "$DRY" = 1 ]; then echo "[dry-run] Done. Nothing was changed."; exit 0; fi
echo "Done. Open Dead Air from the launcher above."
echo "The page shows what the watcher has recorded. To run the checks, start the watcher:"
echo "  .venv/bin/python dead_air.py watch --config dead-air.toml --continuous"
