# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.5.1] - 2026-11-07

### Fixed

- Profiles: a hand-activated profile with a time range no longer stays active after its range ends (or ended while the daemon was stopped or the machine suspended); the daemon drops that manual pick and falls back to the next trigger or Default. A manual pick still holds between trigger edges and is still restored when a process profile exits

## [0.5.0] - 2026-11-05

### Added

- Window or screen capture through the desktop portal (xdg-desktop-portal ScreenCast): `tt500-lcd show-portal [--reselect]`, and a "Window or screen (portal)" source with a "Choose again…" button on the GUI Area tab. The first start shows the portal's picker; the daemon stores the portal's restore token (`portal-token` in its state dir) so later starts do not ask again, and `--reselect` forgets it. The daemon owns the session, so the stream keeps running after the GUI closes; when the shared window or screen is closed the stream stops with that message instead of asking again. Needs GStreamer with the PipeWire plugin (`gst-plugin-pipewire`); the daemon reports a clear error when it is missing
- Release archive with `install.sh` / `uninstall.sh` and an AppImage, built by GitHub Actions
- Self-update: Preferences > Updates and `tt500-lcd update [--check]` install the newest published release; the GUI checks once a day unless turned off
- New app icon: a bold "TT" with an orange "500" badge
- System-tray icon (StatusNotifierItem) owned by the GUI: left click toggles the window, the menu has Open, a Profile submenu, Brightness (Off/25/50/75/100 %), Clear Panel and Quit, and the tooltip shows the panel status and active profile. Closing the window hides it to the tray when a tray is present and the new preference "Keep running in the tray when the window is closed" (on by default) is set. "Start this app at login" now starts the GUI hidden in the tray (`--hidden`; the window is shown when there is no tray)

- Profiles: named display setups (source, framing, fps, steady, brightness, rotation) kept by the daemon in `profiles.toml`, switched by hand or by triggers: times of day (optionally on chosen weekdays) and "while a process runs". `tt500-lcd profile list|save|activate|rename|delete|move|trigger ...`, a GUI Profiles page (drag to reorder, trigger editor) and a profile switcher in the sidebar footer; the daemon requests are `profile_list`, `profile_save`, `profile_rename`, `profile_delete`, `profile_activate`, `profile_set_triggers` and `profile_reorder`, and `status` reports the active profile. A profile never changes the boot animation or the standby image
- Profiles: time-range triggers ("active from 22:00 to 06:00", optional weekdays by start day, may wrap midnight; `trigger add-range|remove-range`) that outrank schedule times; a built-in, undeletable Default profile (the fallback when no trigger applies) with `profile reset` / `profile_reset` and a Reset to Defaults item in the GUI; status marks a hand-changed profile as modified ("(changed)"). Clients older than this change cannot parse the new reason kinds
- Panel rotation: `tt500-lcd rotation <0|1|2|3>`, a Rotation control in the GUI sidebar and the daemon's `set_rotation` request (the step is stored and re-applied after every start-up; `status` reports it). The panel ignores `03 1d NN` (still sent, as TT RGB Plus does), so the daemon rotates the picture itself before encoding, like TT RGB Plus: 2 turns it 180°, 1 and 3 are portrait modes that lay the content out on a 128×480 canvas and turn it 90° clockwise / counter-clockwise (for a panel mounted vertically). It applies to images, animations, videos and live sources, to the standby image and boot animation, and re-renders what is showing when changed; the GUI crop editors switch to the portrait shape and the sidebar buttons read 0° / 90° / 180° / 270°
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

- Profiles: the switcher has no "none" entry, and manual `show`/`clear` keep the active profile marked as changed and fall back to Default instead of clearing the marker
- GUI: sidebar with Display and Boot pages; the standby image and the boot animation (Boot page, one tab each) have their own file picker and crop editor, and brightness and panel status sit at the bottom of the sidebar; a placeholder Area tab is next to Static
- Brightness also applies while nothing is shown (the daemon opens the panel briefly)
- GUI: Clear Panel also resets the file, framing, frame rate and brightness (100); `tt500-lcd clear` keeps the brightness
- GUI: a new message replaces the current one instead of queueing
- Videos are played from their original path instead of being copied; there is no video size limit any more
- Animations are resampled to the frame-rate cap at their original speed
- The frame interval floor dropped from 50 ms to 33 ms (up to 30 fps)
