# Thermaltake 500 LCD for Linux

Show images, animations and videos on the Thermaltake Tower 500 LCD panel kit (RC Pro) from Linux.

Product page: [LCD Panel Kit for The Tower 500](https://thermaltake.com/products/lcd-panel-kit-for-the-tower-500) (USB `264a:232a`).

![TT500 LCD: a virtual desktop running a GPU monitor, cropped onto the panel](https://gist.githubusercontent.com/krakerz/26f00ce4bb280b20fe8407ac08510726/raw/area.png)

<p>
  <img src="https://gist.githubusercontent.com/krakerz/26f00ce4bb280b20fe8407ac08510726/raw/boot.png" alt="Writing a standby image to the panel" width="49%">
  <img src="https://gist.githubusercontent.com/krakerz/26f00ce4bb280b20fe8407ac08510726/raw/profiles.png" alt="Profiles with a time-range trigger" width="49%">
</p>

## Description

The panel kit has no Linux software. A small user daemon (`tt500-lcdd`) owns the panel's USB HID device and streams the chosen content to it. A command-line tool (`tt500-lcd`) and a GTK4 app (`tt500-lcd-gui`) control the daemon over a Unix socket. The protocol was worked out from our own USB captures.

## Features

- Still images and animations (PNG, JPEG, GIF, WebP, BMP, APNG), fitted to the 480x128 panel
- Video loops (MP4, MKV, WebM, MOV, AVI) at 15 fps by default, up to 30 (needs ffmpeg)
- Crop, fit mode, rotation and brightness 0-100 (0 turns the panel off)
- Mirror a monitor, a window (desktop portal) or a headless virtual desktop running your own commands
- Upload a standby image and a boot animation to the panel's flash
- Profiles that switch by hand, on a schedule or while a program runs
- GTK4 / libadwaita app with a tray icon
- Reconnects after unplugging the panel or passing it to a VM; content, brightness and rotation survive restarts
- Runs as a systemd user service

## Installation

Releases are on the [releases page](https://github.com/krakerz/Thermaltake-500-LCD-Linux/releases).

### Release archive (recommended)

```sh
tar -xzf tt500-lcd-<version>-linux-x86_64.tar.gz
cd tt500-lcd-<version>-linux-x86_64
./install.sh
```

`install.sh` puts the daemon, CLI and GUI in `~/.local/bin`, the desktop entry and icon under `~/.local/share`, and runs `tt500-lcd service install`. It is safe to re-run (`--prefix DIR` installs elsewhere, `--no-service` skips the service) and never runs `sudo`: it prints the two commands that install the udev rule, which you run yourself before replugging the panel. `uninstall.sh` removes what it installed and keeps your settings.

### udev rule

Needed once, so your user may open the panel (run from the repository or the unpacked archive), then replug the panel:

```sh
sudo install -Dm644 packaging/udev/70-thermaltake-lcd.rules /etc/udev/rules.d/70-thermaltake-lcd.rules
sudo udevadm control --reload && sudo udevadm trigger
```

### AppImage

Download `TT500-LCD-<version>-x86_64.AppImage`, `chmod +x` it and run it. It opens the GUI and carries the daemon and CLI:

```sh
./TT500-LCD-<version>-x86_64.AppImage              # the GUI
./TT500-LCD-<version>-x86_64.AppImage --cli status # the tt500-lcd command
```

An AppImage mounts somewhere new on every run, so each start copies its `tt500-lcdd` to `~/.local/lib/tt500-lcd/` and the service points there. "Start this app at login" points at the AppImage file, so keep it where it is. The AppImage does not install the udev rule (see above). It needs FUSE 2 (`libfuse2`) unless run with `--appimage-extract-and-run`.

### Updating

Release archive and AppImage installs update themselves: Preferences > Updates, or `tt500-lcd update` (`--check` only looks). The GUI checks at start-up at most once a day (can be turned off). The download is SHA-256 verified and replaced all or nothing. Source builds are not updated this way; rebuild them.

Video needs `ffmpeg` (with `ffprobe`) at runtime. Arch: `sudo pacman -S ffmpeg`; Debian/Ubuntu: `sudo apt install ffmpeg`.

## Building from source

Prerequisites: Rust 1.87 or newer, `pkg-config`, the libudev headers (Arch: `systemd-libs`; Debian/Ubuntu: `libudev-dev`), and for the GUI GTK4 and libadwaita (Arch: `gtk4 libadwaita`; Debian/Ubuntu: `libgtk-4-dev libadwaita-1-dev`).

```sh
git clone https://github.com/krakerz/Thermaltake-500-LCD-Linux
cd Thermaltake-500-LCD-Linux
cargo install --locked --root ~/.local --path crates/daemon
cargo install --locked --root ~/.local --path crates/cli
cargo install --locked --root ~/.local --path crates/gui
install -Dm644 crates/gui/data/io.github.krakerz.Tt500Lcd.desktop ~/.local/share/applications/io.github.krakerz.Tt500Lcd.desktop
install -Dm644 crates/gui/data/io.github.krakerz.Tt500Lcd.svg ~/.local/share/icons/hicolor/scalable/apps/io.github.krakerz.Tt500Lcd.svg
tt500-lcd service install
```

Install the [udev rule](#udev-rule) too, and make sure `~/.local/bin` is in your `PATH`. `cargo build --release` alone leaves the binaries in `target/release/`. The GUI is optional; skip its two steps for a CLI-only setup.

## Usage

```sh
tt500-lcd show ~/Pictures/cat.gif
tt500-lcd show ~/Videos/clip.mp4
```

Common options for `show` (most also work on the live sources below):

- `--fit contain` fits the whole image inside the panel instead of filling it
- `--crop X,Y,W,H` shows only that region (source pixels)
- `--fps N` caps the frame rate (1-30); GIFs keep their own timing unless given

Other commands:

- `tt500-lcd status` prints what the daemon is doing; `watch` prints it on every change (`--json` for JSON lines)
- `tt500-lcd brightness 30` sets brightness (0-100)
- `tt500-lcd rotation 1` rotates the picture: 0 normal, 1 90° clockwise, 2 180°, 3 90° counter-clockwise (1 and 3 use a 128x480 layout)
- `tt500-lcd clear` stops streaming; the panel falls back to its stored image and keeps its brightness

Videos are played from where they are, not copied. If you move one, show it again.

### Live sources

- `tt500-lcd show-screen [--output DP-1]` mirrors a screen (wlroots compositors such as Hyprland and sway)
- `tt500-lcd show-portal [--reselect]` mirrors a window or screen chosen in the desktop portal picker (needs `gst-plugin-pipewire`); the choice is remembered
- `tt500-lcd show-desktop -- alacritty -e ~/Scripts/sysmon.sh` runs the command on a headless virtual desktop (`--run CMD` adds more, `--size WxH`, `--scale N`)
- `--steady` / `--no-steady` skips or keeps half-drawn frames (on by default for the virtual desktop)
- `tt500-lcd preview -o frame.jpg` saves the latest frame of a live source

Live sources are restored after a restart. `show-screen` needs `WAYLAND_DISPLAY` in the daemon's environment; if the service lacks it, run `systemctl --user import-environment WAYLAND_DISPLAY` and restart the service.

### Standby image and boot animation

```sh
tt500-lcd standby image.png
tt500-lcd boot animation.gif
```

**Both write to the panel's flash memory** and ask for confirmation (`--yes` skips it and is required without a terminal). `standby` takes stills; `boot` takes a GIF, APNG or animated WebP and keeps at most 75 frames. Both take `--fit` and `--crop`. `--dry-run` builds the packets without sending anything and writes them to `~/.local/state/tt500-lcd/`. The upload format comes from a USB capture of the vendor software; use it sparingly.

### Profiles

A profile is a named snapshot of the display: source, fit, crop, fps, steady mode, brightness and rotation. Flash writes are never part of one.

```sh
tt500-lcd profile save Day
tt500-lcd profile activate Day
tt500-lcd profile list [--json]
tt500-lcd profile trigger add-time Night 22:00 --days weekdays
tt500-lcd profile trigger add-range Night 22:00 06:00 --days fri,sat
tt500-lcd profile trigger add-process Game game.exe
```

- Also: `rename`, `delete`, `move`, `reset Default`, `trigger remove-time|remove-range|remove-process|clear`
- `--days` takes `mon,tue,...`, `weekdays`, `weekend` or `daily`
- The built-in `Default` profile always exists and is the fallback; it cannot be renamed or deleted
- Priority: running process > active time range > latest schedule entry > your manual pick > Default; a manual pick is dropped when its own time range ends
- A direct `show` or `clear` marks the active profile "(changed)"

### Graphical app

`tt500-lcd-gui` (a running service is required) has Display (Static, Area), Boot and Profiles pages, a profile switcher, brightness and the panel status. The tray icon needs a bar with StatusNotifierItem support.

### Service and autostart

- `tt500-lcd service install | status | uninstall` manages the systemd user service (also Preferences > "Run the panel service at login"); stop any daemon already running outside systemd first
- `tt500-lcd autostart on | off | status` starts the GUI hidden in the tray at login (XDG autostart)
- By hand: `install -Dm644 packaging/systemd/tt500-lcd.service ~/.config/systemd/user/tt500-lcd.service && systemctl --user daemon-reload && systemctl --user enable --now tt500-lcd.service` (expects the daemon at `~/.local/bin/tt500-lcdd`)

## FAQ

**The panel shows its old picture when the daemon stops.**
Expected: the panel falls back to the image stored in its own flash.

**I pass the panel to a VM. What happens?**
The daemon notices it is gone and reconnects when it returns.

**How do I turn the panel off?**
`tt500-lcd brightness 0`.

**Can I change the rotation or the boot animation?**
Yes: `tt500-lcd rotation 0|1|2|3` and `tt500-lcd boot`.

## License

GPL-3.0-or-later, see [LICENSE](LICENSE). JPEG encoding uses jpeg-encoder, based in part on the work of the Independent JPEG Group. The app's interface icons are from [Lucide](https://lucide.dev) (ISC, see `crates/gui/data/icons/LICENSE`).

---

### Notes

Built and maintained with the help of AI.
