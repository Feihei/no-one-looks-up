#!/usr/bin/env bash
# Package a distributable build of No One.
#
# Output layout (this is exactly what Bevy's AssetServer expects):
#
#   dist/
#     no_one_looks_up[.exe]
#     assets/shaders/*.wesl    runtime shaders (trees, facades, cirrus)
#     assets/ui/title.png      title art
#     assets/fonts/*.ttf       ink UI face (every on-screen Chinese string)
#     README.md
#
# Why the assets have to travel with the binary: bevy_asset resolves its asset
# root as `BEVY_ASSET_ROOT` -> `CARGO_MANIFEST_DIR` -> the executable's own
# directory. `cargo run` injects `CARGO_MANIFEST_DIR`, so a dev build always
# finds the repo's `assets/`; a bare binary has neither variable and silently
# falls back to its own folder. Missing `assets/shaders/*.wesl` does not error
# out -- the ExtendedMaterial stays pending forever, so trees and building
# facades (including their lit windows) simply never draw. Lamps still glow
# because their emissive is a StandardMaterial constant.
#
# `assets/models/*.glb` is deliberately not copied: trees are baked
# procedurally since R3.9 and nothing loads those files any more (~47 MB).
#
# Usage:
#   scripts/package.sh                build --release, then package into dist/
#   scripts/package.sh --no-build     reuse target/release as-is
#   scripts/package.sh --out DIR      package into DIR instead of dist/
#   scripts/package.sh --force        allow overwriting a non-package DIR

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/dist"
MARKER=".no-one-package"
BUILD=1
FORCE=0

usage() {
    sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while (($# > 0)); do
    case "$1" in
        --no-build) BUILD=0; shift ;;
        --out)
            [[ $# -ge 2 ]] || { echo "--out needs a directory" >&2; exit 2; }
            OUT="$2"
            shift 2
            ;;
        --force) FORCE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1 (try --help)" >&2; exit 2 ;;
    esac
done

# --- binary name -------------------------------------------------------------
BIN="$(sed -n 's/^name[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)"
[[ -n "$BIN" ]] || { echo "cannot read [package] name from Cargo.toml" >&2; exit 1; }
if [[ "${OS:-}" == "Windows_NT" ]]; then
    BIN_FILE="$BIN.exe"
else
    BIN_FILE="$BIN"
fi

# --- build -------------------------------------------------------------------
if ((BUILD)); then
    command -v cargo >/dev/null 2>&1 || {
        echo "cargo not found; export RUSTUP_HOME/CARGO_HOME or use --no-build" >&2
        exit 1
    }
    echo "==> cargo build --release"
    (cd -- "$ROOT" && cargo build --release)
fi

SRC_BIN="$ROOT/target/release/$BIN_FILE"
[[ -f "$SRC_BIN" ]] || {
    echo "missing $SRC_BIN; run without --no-build first" >&2
    exit 1
}

# --- output directory --------------------------------------------------------
# Only wipe a directory this script created. Anything else needs --force, so a
# mistyped --out cannot eat a real folder.
if [[ -d "$OUT" && -n "$(ls -A -- "$OUT" 2>/dev/null)" && ! -f "$OUT/$MARKER" ]] && ((!FORCE)); then
    echo "refusing to overwrite non-package directory: $OUT" >&2
    echo "  (pass --force if that is really what you want)" >&2
    exit 2
fi
rm -rf -- "$OUT"
mkdir -p -- "$OUT/assets"
: > "$OUT/$MARKER"

# --- payload -----------------------------------------------------------------
echo "==> copying binary"
cp -- "$SRC_BIN" "$OUT/$BIN_FILE"

echo "==> copying runtime assets"
shopt -s nullglob
for dir in "$ROOT"/assets/*/; do
    name="$(basename -- "$dir")"
    [[ "$name" == "models" ]] && continue
    cp -R -- "$dir" "$OUT/assets/$name"
done
for file in "$ROOT"/assets/*; do
    [[ -f "$file" ]] && cp -- "$file" "$OUT/assets/"
done
shopt -u nullglob

[[ -f "$ROOT/README.md" ]] && cp -- "$ROOT/README.md" "$OUT/"

# --- verify ------------------------------------------------------------------
# verify list: the font path mirrors `core::ui_font::UI_FONT_PATH`. Without it
# the Chinese menus fall back to Bevy's Latin-only face and render as empty
# boxes -- again no error, just unreadable text.
missing=0
for required in \
    "assets/shaders/tree_sway.wesl" \
    "assets/shaders/facade_uv.wesl" \
    "assets/shaders/cirrus.wesl" \
    "assets/ui/title.png" \
    "assets/fonts/ZhaohuaTypeWriterLight.ttf"
do
    if [[ ! -f "$OUT/$required" ]]; then
        echo "missing after packaging: $required" >&2
        missing=1
    fi
done
((missing == 0)) || exit 1

# --- report ------------------------------------------------------------------
echo
echo "==> package ready: $OUT"
(cd -- "$OUT" && find . -type f | sort | sed 's|^\./|  |')
echo
du -sh -- "$OUT" 2>/dev/null || true
echo
echo "run it from anywhere: \"$OUT/$BIN_FILE\" (assets sit next to it)"
