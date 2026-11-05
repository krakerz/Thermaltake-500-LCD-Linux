#!/usr/bin/env bash
# Removes what install.sh installed (binaries, desktop entry, icon, the service and the
# GUI autostart entry). Settings and profiles are kept. It never runs sudo.
set -euo pipefail

usage() {
    cat <<USAGE
Usage: uninstall.sh [--prefix DIR]

  --prefix DIR   the prefix install.sh used (default ~/.local)
  -h, --help     show this help
USAGE
}

prefix="${HOME:?HOME is not set}/.local"
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) prefix="${2:?--prefix needs a directory}"; shift 2 ;;
        --prefix=*) prefix="${1#--prefix=}"; shift ;;
        -h | --help) usage; exit 0 ;;
        *) echo "uninstall.sh: unknown option '$1'" >&2; usage >&2; exit 2 ;;
    esac
done
case "$prefix" in
    /*) ;;
    *) echo "uninstall.sh: --prefix must be an absolute path" >&2; exit 2 ;;
esac

app_id=io.github.krakerz.Tt500Lcd
share="$prefix/share"
cli="$prefix/bin/tt500-lcd"

if [ -x "$cli" ]; then
    "$cli" service uninstall || echo "Could not remove the service; check: systemctl --user status tt500-lcd" >&2
    "$cli" autostart off || true
fi
rm -f "$prefix/bin/tt500-lcdd" "$prefix/bin/tt500-lcd" "$prefix/bin/tt500-lcd-gui" \
    "$share/applications/$app_id.desktop" \
    "$share/icons/hicolor/scalable/apps/$app_id.svg"
rm -rf "$share/tt500-lcd"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$share/applications" 2>/dev/null || true

echo "Removed tt500-lcd from $prefix. Settings and profiles in ~/.config/tt500-lcd and the daemon's state were kept."
if [ -f /etc/udev/rules.d/70-thermaltake-lcd.rules ]; then
    echo "To remove the udev rule too, run:"
    echo "  sudo rm /etc/udev/rules.d/70-thermaltake-lcd.rules && sudo udevadm control --reload-rules"
fi
