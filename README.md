# Thermaltake 500 LCD for Linux

Show images, animations and videos on the Thermaltake Tower 500 LCD panel kit (RC Pro) from Linux.

Product page: [LCD Panel Kit for The Tower 500](https://thermaltake.com/products/lcd-panel-kit-for-the-tower-500) (USB `264a:232a`).

## Description

The panel kit has no Linux software. A small user daemon (`tt500-lcdd`) owns the panel's USB HID device (`264a:232a`) and streams the chosen image, animation or video to it continuously. A command-line tool (`tt500-lcd`) controls the daemon over a Unix socket. The protocol was worked out from our own USB captures.

## Features

- Show a still image or animation (PNG, JPEG, GIF, WebP, BMP; animated GIF, APNG and WebP), fitted to the 480x128 panel
- Play a video file (MP4, MKV, WebM, MOV, AVI) in a loop at 15 fps by default, up to 30 with `--fps` (needs ffmpeg)
- Show only a chosen region of the source (`--crop`)
- Brightness 0-100 (0 turns the panel off)
- A GTK4 / libadwaita app (`tt500-lcd-gui`) with a sidebar: Display > Static picks a file, crops it, sets the frame rate and shows it on the panel; Display > Area streams a virtual desktop (your own commands) or a monitor with the same crop editor; Boot has a Standby image tab and a Boot animation tab, each with its own crop editor, that write to the panel's flash after a dry run and a confirmation; brightness and the panel status are always at the bottom of the sidebar
- Profiles: save what the panel shows (with brightness and rotation) under a name, switch between them by hand, on a schedule or while a program runs
- Reconnects automatically after unplugging the panel or passing it to a VM
- Content, brightness and rotation survive restarts
- Runs as a systemd user service

## Installation

Pick one of the three ways below. Releases are on the [releases page](https://github.com/krakerz/Thermaltake-500-LCD-Linux/releases).

### Release archive (recommended)

Download `tt500-lcd-<version>-linux-x86_64.tar.gz`, then:

```sh
tar -xzf tt500-lcd-<version>-linux-x86_64.tar.gz
cd tt500-lcd-<version>-linux-x86_64
./install.sh
```

`install.sh` installs the daemon, CLI and GUI to `~/.local/bin`, the desktop entry and icon under `~/.local/share`, and then runs `tt500-lcd service install`. It is safe to re-run; `--prefix DIR` installs elsewhere, `--no-service` skips the service. It never runs `sudo`: it prints the two commands that install the udev rule, which you run yourself, then replug the panel. `uninstall.sh` (also kept in `~/.local/share/tt500-lcd/`) removes everything it installed and keeps your settings.

### AppImage

Download `TT500-LCD-<version>-x86_64.AppImage`, `chmod +x` it and run it. It opens the GUI and carries the daemon and CLI too:

```sh
./TT500-LCD-<version>-x86_64.AppImage              # the GUI
./TT500-LCD-<version>-x86_64.AppImage --cli status # the tt500-lcd command
```

The systemd service needs a daemon path that stays valid, but an AppImage mounts itself somewhere new on every run. So each start copies its `tt500-lcdd` to `~/.local/lib/tt500-lcd/` (only when it differs), and "Run the panel service at login" (or `--cli service install`) points the unit there; "Start this app at login" points at the AppImage file, so keep it where it is. The udev rule is not installed by the AppImage: copy `70-thermaltake-lcd.rules` from the repository (or the release archive) with the `sudo` commands in step 1 below. AppImages need FUSE 2 (`libfuse2`) unless you run them with `--appimage-extract-and-run`.

### From source

Build it (see below), then:

1. Let your user open the panel (needs root once), then replug the panel:

   ```sh
   sudo install -Dm644 packaging/udev/70-thermaltake-lcd.rules /etc/udev/rules.d/70-thermaltake-lcd.rules
   sudo udevadm control --reload && sudo udevadm trigger
   ```

2. Install the daemon and CLI to `~/.local/bin`:

   ```sh
   cargo install --locked --root ~/.local --path crates/daemon
   cargo install --locked --root ~/.local --path crates/cli
   ```

3. Install and start the user service (the unit runs the `tt500-lcdd` found next to the CLI, else in `PATH`):

   ```sh
   tt500-lcd service install
   ```

   `tt500-lcd service status` shows whether it is installed, enabled and running; `tt500-lcd service uninstall` disables and removes it. If a daemon is already running outside systemd, stop it first. The GUI does the same: Main menu > Preferences > "Run the panel service at login".

   To start the GUI at login (XDG autostart), run `tt500-lcd autostart on` (`off` / `status` also work), or use Preferences > "Start this app at login". The GUI must be installed next to the CLI or in `PATH`; the GUI switch uses the running binary.

   The GUI shows a system-tray icon (StatusNotifierItem; needs a bar or desktop with tray support) with a menu for the window, profile, brightness and Clear Panel. Autostart starts it hidden in the tray (`tt500-lcd-gui --hidden`); closing the window keeps it running in the tray unless you turn that off in Preferences.

   Doing it by hand instead: `install -Dm644 packaging/systemd/tt500-lcd.service ~/.config/systemd/user/tt500-lcd.service && systemctl --user daemon-reload && systemctl --user enable --now tt500-lcd.service` (this unit expects the daemon at `~/.local/bin/tt500-lcdd`).

Make sure `~/.local/bin` is in your `PATH`.

### Updating

Installed builds (the release archive or the AppImage) update themselves from the newest published release: Preferences > Updates > "Check for updates" (then "Update" and "Restart"), or `tt500-lcd update` (`--check` only looks). The GUI checks at start-up, at most once a day, unless you turn "Check for updates automatically" off. An update downloads the release, verifies its SHA-256, replaces the daemon, CLI and GUI together (all or nothing; the old version is restored if the new one does not start) and restarts the panel service. Builds from source are not updated this way; rebuild them. To update by hand, run the new archive's `install.sh` or replace the AppImage file.

Video playback needs `ffmpeg` (with `ffprobe`) at runtime; it is optional for images and GIFs. Arch: `sudo pacman -S ffmpeg`; Debian/Ubuntu: `sudo apt install ffmpeg`.

## Building from source

Prerequisites: Rust 1.87 or newer, `pkg-config` and the libudev headers (Arch: `systemd-libs`; Debian/Ubuntu: `libudev-dev`).

```sh
git clone https://github.com/krakerz/Thermaltake-500-LCD-Linux
cd Thermaltake500-LCD-Linux
cargo build --release
```

The binaries are `target/release/tt500-lcdd` and `target/release/tt500-lcd`.

## Usage

```sh
tt500-lcd show ~/Pictures/cat.gif
tt500-lcd show ~/Videos/clip.mp4
```

- `tt500-lcd show <file> --fit contain` fits the whole image inside the panel instead of filling it
- `tt500-lcd show <file> --crop X,Y,W,H` shows only that region (source pixels) before fitting
- `tt500-lcd show <file> --fps N` caps the frame rate (1-30). Videos default to 15 fps; GIFs and other animations keep their own timing unless `--fps` is given, and are resampled to the cap at their original speed
- `tt500-lcd show-screen [--output DP-1]` mirrors a screen (wlroots compositors such as Hyprland and sway); it takes `--fit`, `--crop` and `--fps` like `show`
- `tt500-lcd show-desktop -- alacritty -e ~/Scripts/sysmon.sh` runs the command on a headless virtual desktop and shows it. `--run CMD` (repeatable) adds more commands, `--size WxH` (default 960x256) and `--scale N` (default 2) set the virtual output
- `tt500-lcd preview -o frame.jpg` saves the latest full frame of the running live source
- `tt500-lcd show-portal [--reselect]` mirrors a single window or screen chosen in the desktop portal's picker (any compositor with xdg-desktop-portal ScreenCast, including Hyprland); it takes `--fit`, `--crop`, `--fps` and `--steady` like `show-screen`. It needs GStreamer with the PipeWire plugin (`gst-plugin-pipewire`). The first start asks which window or screen to share (answer within 25 s); the daemon remembers the choice (a restore token in its state dir), `--reselect` or the GUI's "Choose again…" asks again, and the stream stops when the shared window is closed
- `--steady` / `--no-steady` (`show-desktop`, `show-screen`) skip or keep half-drawn frames: with steady frames a changed capture is shown only once the next capture matches it, or after 250 ms, which stops a terminal that redraws once a second from flickering, at the cost of a little delay. On by default for the virtual desktop, off for a screen
- Live sources are streamed by the daemon and restored after a restart (retried for about a minute while the screen comes up). `show-screen` reads `WAYLAND_DISPLAY` from the daemon's environment; if the systemd user service lacks it, run `systemctl --user import-environment WAYLAND_DISPLAY` once per login and restart the service
- Videos are played from where they are, not copied. If you move or delete one, `tt500-lcd status` shows a problem after the next daemon restart; show it again
- `tt500-lcd status` prints what the daemon is doing
- `tt500-lcd watch` prints the status again every time it changes (`--json` for one JSON line each; Ctrl+C to stop)
- `tt500-lcd brightness 30` sets the brightness (0-100); it also applies while nothing is shown (the daemon opens the panel briefly)
- `tt500-lcd rotation 1` rotates the picture (0 = normal, 1 = portrait turned 90° clockwise, 2 = 180°, 3 = portrait turned 90° counter-clockwise; portrait modes use a 128×480 layout, for a panel mounted vertically); the daemon renders everything it shows or uploads rotated, re-renders what is showing when you change it, stores the step and re-applies it after every start-up
- `tt500-lcd clear` stops streaming and releases the panel; it keeps the brightness (the app's Clear Panel button also resets it to 100)
- `tt500-lcd standby image.png` uploads a still image as the panel's standby image, the picture the panel shows when nothing is streaming. **This writes to the panel's flash memory**, so it asks "This writes the image to the panel's flash memory. Continue? [y/N]" first (`--yes` skips the question, and is required without a terminal). It takes `--fit` and `--crop` like `show`, accepts stills only (no GIF animations or videos), and re-encodes the image to a small 480x128 JPEG. The upload format comes from a USB capture of the vendor software; the panel's reaction to anything else is unknown, so use it sparingly.
- `tt500-lcd standby image.png --dry-run` renders the image and builds the exact packets but sends nothing to the panel (it works without a panel). It writes `standby-dryrun.bin` (the raw 1024-byte packets) and `standby-dryrun.txt` (a hex summary) to the daemon's state directory (`$XDG_STATE_HOME/tt500-lcd/`, usually `~/.local/state/tt500-lcd/`) and prints their paths.
- `tt500-lcd boot animation.gif` uploads an animation as the panel's boot animation, the one it plays at power-on. **This writes to the panel's flash memory**, so it asks "This writes the boot animation to the panel's flash memory. Continue? [y/N]" first (`--yes` skips the question, and is required without a terminal). It takes a GIF, animated PNG or animated WebP (no stills or videos) plus `--fit` and `--crop`, keeps its timing when all its frames last the same 40 to 1000 ms (a 100 ms GIF stays 100 ms, the vendor's 40 ms factory animation stays 40 ms) and otherwise resamples it at its original speed to its most common frame delay (clamped to 40 to 1000 ms), then keeps the first 75 frames (the vendor's factory animation has 75; the daemon log notes a cut). The frames are re-encoded as small 480x128 JPEGs (at most 16 KiB each, 640 KiB in total). While it uploads the daemon pauses streaming, waits for the panel to confirm every 64 KiB block (progress goes to the daemon log) and gives up with an error after 5 seconds without an answer; the panel may then hold a partial boot animation. The format comes from a USB capture of the vendor software; the panel's reaction to anything else is unknown, so use it sparingly.
- `tt500-lcd boot animation.gif --dry-run` renders the animation and builds the exact packets but sends nothing to the panel (it works without a panel). It writes `boot-dryrun.bin` (the raw 1024-byte packets) and `boot-dryrun.txt` (frame delay, frame sizes, block count and the first bytes of the key packets) to the daemon's state directory and prints their paths.

## Profiles

A profile is a named snapshot of the display: the source (file or live source), fit, crop, frame rate, steady mode, brightness and rotation. The daemon keeps them in `profiles.toml` in its state directory and restores the active one's content after a restart. Boot animation and standby image are flash writes and are never part of a profile.

```sh
tt500-lcd profile save Day                       # snapshot what the panel shows now
tt500-lcd profile activate Day
tt500-lcd profile list [--json]
tt500-lcd profile move Day 1                     # position, or up / down
tt500-lcd profile trigger add-time Night 22:00 --days weekdays
tt500-lcd profile trigger add-range Night 22:00 06:00 --days fri,sat
tt500-lcd profile trigger add-process Game game.exe
tt500-lcd profile reset Default                  # back to factory settings
```

- `profile save NAME` on an existing name replaces its settings and keeps its triggers. Images and videos are referenced by path, not copied: a moved file makes activation fail (the previous content stays, `status` shows the problem, and the daemon retries every 30 s, up to 10 times)
- Other `profile` commands: `rename`, `delete`, `trigger remove-time|remove-range|remove-process|clear`. `--days` takes `mon,tue,...`, `weekdays`, `weekend` or `daily` (default every day)
- Triggers: a time range (`add-range START END`, may wrap midnight; `--days` names the day the range starts, so Fri 22:00-06:00 runs until Saturday 06:00) is active from start to end and applies its profile when it begins; a time of day switches the profile on when it passes; "while a process runs" switches it on when the process starts and off when it exits. A process matches by its name (`comm`, up to 15 characters, or the basename of its executable)
- Default profile: a built-in profile named Default always exists and is listed first. It is the fallback when no trigger applies, so there is no "no profile" state. It cannot be renamed or deleted; you can save over it, reorder it and give it triggers. A new install starts it from the stored content, and `profile reset Default` returns it to factory settings (clear the panel, brightness 100, rotation 0, no triggers), applying them at once if it is active
- Priority, applied whenever a trigger edge happens (a range or schedule time passes, a process starts or exits): a running process beats an active time range, which beats the latest schedule entry, which beats the profile you picked by hand, which beats Default. Between edges your manual choice holds. When a process or range ends the daemon falls back down this ladder. Among several matches the list order decides (`profile move` or drag in the GUI)
- A direct `show`, `show-screen`, `show-desktop` or `clear` keeps the active profile's name but marks it "(changed)", and the daemon will fall back to Default rather than your last pick; if Default is active under fallback, the change simply holds until the next trigger edge. Brightness and rotation changes do not mark it
- GUI: the Profiles page lists them (save current, activate, rename, delete, drag the handle or use the row menu to reorder, edit time ranges, times and processes; Default offers Reset to Defaults); the sidebar footer has a profile switcher (no "none" entry). Activating a profile reloads the Static and Area pages with its settings

## Graphical app

`tt500-lcd-gui` needs GTK4 and libadwaita. Build dependencies: Arch `gtk4 libadwaita`; Debian/Ubuntu `libgtk-4-dev libadwaita-1-dev`. It talks to the same daemon, so the service above must be running.

```sh
cargo install --locked --root ~/.local --path crates/gui
install -Dm644 crates/gui/data/io.github.krakerz.Tt500Lcd.desktop ~/.local/share/applications/io.github.krakerz.Tt500Lcd.desktop
install -Dm644 crates/gui/data/io.github.krakerz.Tt500Lcd.svg ~/.local/share/icons/hicolor/scalable/apps/io.github.krakerz.Tt500Lcd.svg
```

The app icons are from Lucide (ISC licence, `crates/gui/data/icons/LICENSE`). Regenerate them with `scripts/update-icons.sh` (needs `picosvg`).

## FAQ

**The panel shows its old picture when the daemon stops.**
Expected: the panel falls back to the image stored in its own flash.

**I pass the panel to a VM. What happens?**
The daemon notices it is gone and reconnects when it returns.

**How do I turn the panel off?**
`tt500-lcd brightness 0`.

**Can I change the rotation or the boot animation?**
Rotation: yes, `tt500-lcd rotation 0|1|2|3` (or the GUI sidebar); the picture is rotated on the PC. Boot animation: use `tt500-lcd boot`.

## License

Licensed under the GNU General Public License, version 3 or (at your option) any later version (GPL-3.0-or-later). See the [LICENSE](LICENSE) file.

JPEG encoding uses jpeg-encoder, based in part on the work of the Independent JPEG Group.

---

### Notes

Built and maintained with the help of AI.
