#!/usr/bin/env bash
# Renders the tray icon PNGs (crates/gui/data/tray/tt500-<size>.png) from the app logo
# with rsvg-convert. The PNGs are committed and embedded into the GUI binary.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SVG="$REPO_ROOT/crates/gui/data/io.github.krakerz.Tt500Lcd.svg"
OUT="$REPO_ROOT/crates/gui/data/tray"

command -v rsvg-convert >/dev/null || { echo "rsvg-convert not found (librsvg)" >&2; exit 1; }
mkdir -p "$OUT"
for size in 22 32 48; do
    rsvg-convert -w "$size" -h "$size" -o "$OUT/tt500-$size.png" "$SVG"
done
echo "wrote tray icons to $OUT"
