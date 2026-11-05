#!/usr/bin/env bash
# Packs the release archive: tt500-lcd-VERSION-linux-x86_64.tar.gz.
# Usage: packaging/build-archive.sh VERSION BIN_DIR OUT_DIR   (BIN_DIR = target/release)
set -euo pipefail

version="${1:?usage: build-archive.sh VERSION BIN_DIR OUT_DIR}"
bin_dir="${2:?usage: build-archive.sh VERSION BIN_DIR OUT_DIR}"
out_dir="${3:?usage: build-archive.sh VERSION BIN_DIR OUT_DIR}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
app_id=io.github.krakerz.Tt500Lcd
name="tt500-lcd-$version-linux-x86_64"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
dir="$stage/$name"
mkdir -p "$dir/bin" "$dir/share/applications" "$dir/share/icons/hicolor/scalable/apps" "$dir/share/udev" "$dir/share/systemd"
for b in tt500-lcdd tt500-lcd tt500-lcd-gui; do
    install -m 755 "$bin_dir/$b" "$dir/bin/$b"
done
install -m 755 "$root/packaging/install.sh" "$root/packaging/uninstall.sh" "$dir/"
install -m 644 "$root/crates/gui/data/$app_id.desktop" "$dir/share/applications/"
install -m 644 "$root/crates/gui/data/$app_id.svg" "$dir/share/icons/hicolor/scalable/apps/"
install -m 644 "$root/packaging/udev/70-thermaltake-lcd.rules" "$dir/share/udev/"
install -m 644 "$root/packaging/systemd/tt500-lcd.service" "$dir/share/systemd/"
install -m 644 "$root/LICENSE" "$root/README.md" "$dir/"

mkdir -p "$out_dir"
tar -C "$stage" --owner=0 --group=0 -czf "$out_dir/$name.tar.gz" "$name"
echo "$out_dir/$name.tar.gz"
