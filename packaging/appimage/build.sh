#!/usr/bin/env bash
# Builds TT500-LCD-VERSION-x86_64.AppImage with linuxdeploy and its GTK plugin: the GUI is the
# entry point, the daemon and CLI ride along (see AppRun).
# Usage: packaging/appimage/build.sh VERSION BIN_DIR OUT_DIR   (BIN_DIR = target/release)
# Needs: curl, file, patchelf, glib-compile-schemas, and the GTK 4 / libadwaita runtime.
set -euo pipefail

version="${1:?usage: build.sh VERSION BIN_DIR OUT_DIR}"
bin_dir="$(cd "${2:?usage: build.sh VERSION BIN_DIR OUT_DIR}" && pwd)"
out_dir="${3:?usage: build.sh VERSION BIN_DIR OUT_DIR}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
app_id=io.github.krakerz.Tt500Lcd

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
appdir="$work/AppDir"
mkdir -p "$appdir/usr/bin" "$out_dir"
for b in tt500-lcdd tt500-lcd tt500-lcd-gui; do
    install -m 755 "$bin_dir/$b" "$appdir/usr/bin/$b"
done

tools="$work/tools"
mkdir -p "$tools"
curl -fsSL -o "$tools/linuxdeploy" https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
curl -fsSL -o "$tools/linuxdeploy-plugin-gtk.sh" https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh
chmod +x "$tools/linuxdeploy" "$tools/linuxdeploy-plugin-gtk.sh"

export ARCH=x86_64
export APPIMAGE_EXTRACT_AND_RUN=1 # no FUSE in CI containers
export DEPLOY_GTK_VERSION=4
export LINUXDEPLOY_OUTPUT_VERSION="$version"
export PATH="$tools:$PATH"
(
    cd "$work"
    linuxdeploy --appdir "$appdir" \
        --executable "$appdir/usr/bin/tt500-lcd-gui" \
        --executable "$appdir/usr/bin/tt500-lcdd" \
        --executable "$appdir/usr/bin/tt500-lcd" \
        --desktop-file "$root/crates/gui/data/$app_id.desktop" \
        --icon-file "$root/crates/gui/data/$app_id.svg" \
        --custom-apprun "$root/packaging/appimage/AppRun" \
        --plugin gtk \
        --output appimage
)
built="$(find "$work" -maxdepth 1 -name '*.AppImage' | head -n 1)"
[ -n "$built" ] || { echo "build.sh: linuxdeploy produced no AppImage" >&2; exit 1; }
final="$out_dir/TT500-LCD-$version-x86_64.AppImage"
mv "$built" "$final"
chmod 755 "$final"
echo "$final"
