# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- `tt500-lcd service install|uninstall|status` and `tt500-lcd autostart on|off|status`, and a GUI Preferences dialog (Main menu), to run the daemon as a systemd user service and start the GUI at login
- GUI: Area tab streams a monitor or a virtual desktop running your own commands to the panel, with the same crop editor
- Steady frames for live sources: a frame that changed since the last capture is shown only once the next capture matches it (or after 250 ms), so a terminal caught mid-redraw no longer flickers on the panel; `--steady` / `--no-steady` on `show-desktop` and `show-screen`, a switch on the GUI Area tab; on by default for the virtual desktop, off for a screen (`steady` in the daemon's `show_live` request)
- `tt500-lcd show-screen`, `show-desktop -- CMD` and `preview -o FILE` for live sources (screen mirroring, headless virtual desktop, preview frame)
- `tt500-lcd boot <animation>` uploads a GIF, animated PNG or WebP as the panel's boot animation (up to 75 frames at one delay of 40 to 1000 ms: a GIF whose frames all last 40 to 1000 ms keeps its timing, any other is resampled to its most common delay; the daemon's `upload_boot_animation` request); it writes to the panel's flash memory and asks for confirmation (`--yes` skips it); `--dry-run` sends nothing and writes the exact packets to the state directory
- `tt500-lcd standby <image>` uploads a still image as the panel's standby image (the daemon's `upload_standby` request); it writes to the panel's flash memory and asks for confirmation (`--yes` skips it); `--dry-run` sends nothing and writes the exact packets to the state directory
- GUI: GIFs, animated PNG/WebP and videos play in the preview while you crop
- GUI: write the standby image (stills) or the boot animation (GIF, APNG, WebP) to the panel; a dry run shows what will be written, then a confirmation asks before writing to the panel's flash
- GUI: close the loaded file (header button)
- GTK4 / libadwaita app `tt500-lcd-gui` with a desktop file and icon
- Daemon event stream (`subscribe`) and `tt500-lcd watch` (`--json` for one JSON line per change)
- `show --fps` (1-30) sets the highest frame rate for videos and animations
- Show a still image or GIF on the panel
- The daemon keeps streaming and reconnects after the panel is unplugged
- CLI commands `show`, `clear`, `status` and `brightness`
- Brightness 0-100 (0 turns the panel off)
- Content and brightness survive restarts
- Play video files (MP4, MKV, WebM, MOV, AVI) in a loop at up to 15 fps (needs ffmpeg)
- Animated PNG and WebP play as animations
- `show --crop X,Y,W,H` shows only a region of the source
- systemd user unit and udev rule

### Changed

- GUI: sidebar with Display and Boot pages; the standby image and the boot animation (Boot page, one tab each) have their own file picker and crop editor, and brightness and panel status sit at the bottom of the sidebar; a placeholder Area tab is next to Static
- Brightness also applies while nothing is shown (the daemon opens the panel briefly)
- GUI: Clear Panel also resets the file, framing, frame rate and brightness (100); `tt500-lcd clear` keeps the brightness
- GUI: a new message replaces the current one instead of queueing
- Videos are played from their original path instead of being copied; there is no video size limit any more
- Animations are resampled to the frame-rate cap at their original speed
- The frame interval floor dropped from 50 ms to 33 ms (up to 30 fps)
