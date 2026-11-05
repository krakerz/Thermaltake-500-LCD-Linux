#!/usr/bin/env bash
# Installs tt500-lcd from the release archive into ~/.local (or --prefix DIR):
# binaries, desktop entry, icon, then the systemd user service. Safe to re-run.
# It never runs sudo; the udev rule needs one command that is printed for you.
set -euo pipefail

usage() {
    cat <<USAGE
Usage: install.sh [--prefix DIR] [--no-service]

  --prefix DIR   install under DIR instead of ~/.local
  --no-service   do not install and start the systemd user service
  -h, --help     show this help
USAGE
}

prefix="${HOME:?HOME is not set}/.local"
service=1
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) prefix="${2:?--prefix needs a directory}"; shift 2 ;;
        --prefix=*) prefix="${1#--prefix=}"; shift ;;
        --no-service) service=0; shift ;;
        -h | --help) usage; exit 0 ;;
        *) echo "install.sh: unknown option '$1'" >&2; usage >&2; exit 2 ;;
    esac
done
case "$prefix" in
    /*) ;;
    *) echo "install.sh: --prefix must be an absolute path" >&2; exit 2 ;;
esac
case "$prefix" in
    *[[:space:]\"\'\\\$\`%]*) echo "install.sh: the prefix may not contain spaces or shell/desktop-file special characters" >&2; exit 2 ;;
esac

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
for f in bin/tt500-lcdd bin/tt500-lcd bin/tt500-lcd-gui share/applications/io.github.krakerz.Tt500Lcd.desktop \
    share/icons/hicolor/scalable/apps/io.github.krakerz.Tt500Lcd.svg share/udev/70-thermaltake-lcd.rules; do
    [ -f "$here/$f" ] || { echo "install.sh: $f is missing; run this script from the unpacked release folder" >&2; exit 1; }
done

app_id=io.github.krakerz.Tt500Lcd
rule=70-thermaltake-lcd.rules
share="$prefix/share"
mkdir -p "$prefix/bin" "$share/applications" "$share/icons/hicolor/scalable/apps" "$share/tt500-lcd"

# Copy to a temporary name, then rename: a running binary cannot be written over
# ("text file busy"), but its directory entry can be replaced.
put() { # mode source dest
    install -m "$1" "$2" "$3.new.$$"
    mv -f "$3.new.$$" "$3"
}

for b in tt500-lcdd tt500-lcd tt500-lcd-gui; do
    put 755 "$here/bin/$b" "$prefix/bin/$b"
done
sed "s|^Exec=.*|Exec=$prefix/bin/tt500-lcd-gui|" "$here/share/applications/$app_id.desktop" >"$share/applications/$app_id.desktop.new.$$"
chmod 644 "$share/applications/$app_id.desktop.new.$$"
mv -f "$share/applications/$app_id.desktop.new.$$" "$share/applications/$app_id.desktop"
put 644 "$here/share/icons/hicolor/scalable/apps/$app_id.svg" "$share/icons/hicolor/scalable/apps/$app_id.svg"
put 644 "$here/share/udev/$rule" "$share/tt500-lcd/$rule"
[ -f "$here/uninstall.sh" ] && put 755 "$here/uninstall.sh" "$share/tt500-lcd/uninstall.sh"
# Lets the app tell an installed copy (which can update itself) from a build tree.
printf 'installed by install.sh\n' >"$share/tt500-lcd/installed-by-install-sh"

command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$share/applications" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$share/icons/hicolor" 2>/dev/null || true

echo "Installed tt500-lcd under $prefix"
case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "Note: $prefix/bin is not in your PATH; add it to run tt500-lcd from a terminal." ;;
esac

if cmp -s "$here/share/udev/$rule" "/etc/udev/rules.d/$rule" 2>/dev/null; then
    echo "The udev rule is already installed."
else
    cat <<MSG

The panel needs a udev rule so your user may open it. This script does not run sudo;
run these two commands yourself, then replug the panel's USB cable:

  sudo install -m 644 "$share/tt500-lcd/$rule" /etc/udev/rules.d/$rule
  sudo udevadm control --reload-rules && sudo udevadm trigger

MSG
fi

if [ "$service" -eq 1 ]; then
    if "$prefix/bin/tt500-lcd" service install; then
        :
    else
        echo "Could not set up the systemd user service. Retry later with: $prefix/bin/tt500-lcd service install" >&2
    fi
else
    echo "Skipped the service. Set it up with: $prefix/bin/tt500-lcd service install"
fi
