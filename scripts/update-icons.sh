#!/usr/bin/env bash
# Regenerates data/icons/*.svg from Lucide (https://lucide.dev, ISC
# licence) using crates/gui/data/icons/icons.txt. Lucide draws with strokes, but GTK's
# symbolic-icon recolouring fills every path, so strokes are converted to
# filled outlines with picosvg (`pip install picosvg`). Dev-time only: the
# converted files are committed and embedded into the binary.
set -euo pipefail

LUCIDE_VERSION="${LUCIDE_VERSION:-1.50.0}"
PICOSVG="${PICOSVG:-picosvg}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ICONS="$REPO_ROOT/crates/gui/data/icons"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

command -v "$PICOSVG" >/dev/null || { echo "picosvg not found (pip install picosvg, or set PICOSVG)" >&2; exit 1; }
rm -f "$ICONS"/*.svg

while read -r name lucide style; do
    case "$name" in ""|\#*) continue ;; esac
    source="$work/$lucide.svg"
    [ -e "$source" ] || curl -fsSL -o "$source" \
        "https://raw.githubusercontent.com/lucide-icons/lucide/$LUCIDE_VERSION/icons/$lucide.svg"
    input="$source"
    if [ "${style:-}" = filled ]; then
        # Solid variant: fill the shape as well as stroking it.
        input="$work/$lucide-filled.svg"
        sed 's/fill="none"/fill="currentColor"/' "$source" > "$input"
    fi
    "$PICOSVG" "$input" > "$ICONS/$name.svg"
done < "$ICONS/icons.txt"

curl -fsSL -o "$ICONS/LICENSE" "https://raw.githubusercontent.com/lucide-icons/lucide/$LUCIDE_VERSION/LICENSE"
echo "wrote $(ls "$ICONS"/*.svg | wc -l) icons to $ICONS"
