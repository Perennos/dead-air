#!/usr/bin/env bash
# Type-check the Windows build from Linux without a MinGW toolchain.
#
# `cargo check --target x86_64-pc-windows-gnu` still runs the build scripts of the bundled
# SQLite and of ring, which want a MinGW C compiler. For a check (no linking), this script
# points them at stand-ins that write empty objects, so only the Rust code is type-checked.
# Real Windows builds and tests run on the windows-latest CI runner with MSVC.
set -eu
STUBS=$(mktemp -d)
trap 'rm -rf "$STUBS"' EXIT
cat > "$STUBS/cc" <<'STUB'
#!/bin/sh
out=
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out=$2; shift 2; continue ;;
    -o*) out=${1#-o} ;;
    --version|-v|-dumpversion) echo "stub 0.0"; exit 0 ;;
  esac
  shift
done
[ -n "$out" ] && : > "$out"
exit 0
STUB
cat > "$STUBS/ar" <<'STUB'
#!/bin/sh
for a in "$@"; do case "$a" in *.a|*.lib) printf '!<arch>\n' > "$a";; esac; done
exit 0
STUB
chmod +x "$STUBS/cc" "$STUBS/ar"
cd "$(dirname "$0")/.."
CC_x86_64_pc_windows_gnu="$STUBS/cc" AR_x86_64_pc_windows_gnu="$STUBS/ar" \
    cargo check --target x86_64-pc-windows-gnu --all-targets "$@"
