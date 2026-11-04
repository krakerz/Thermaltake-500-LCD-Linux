//! Embedded Lucide icons, installed into the GTK icon search path at startup.

use std::fs;
use std::path::Path;

use directories::ProjectDirs;

include!(concat!(env!("OUT_DIR"), "/icons.rs"));

/// Open a file.
pub const OPEN: &str = "tt5-open-symbolic";
/// Clear the panel.
pub const CLEAR: &str = "tt5-clear-symbolic";
/// Apply to the panel.
pub const APPLY: &str = "tt5-apply-symbolic";
/// Brightness.
pub const BRIGHTNESS: &str = "tt5-brightness-symbolic";
/// Frame rate.
pub const FPS: &str = "tt5-fps-symbolic";
/// Crop.
pub const CROP: &str = "tt5-crop-symbolic";
/// Zoom in.
pub const ZOOM_IN: &str = "tt5-zoom-in-symbolic";
/// Zoom out.
pub const ZOOM_OUT: &str = "tt5-zoom-out-symbolic";
/// Reset the view.
pub const RESET: &str = "tt5-reset-symbolic";
/// Close the loaded file.
pub const REMOVE: &str = "tt5-remove-symbolic";
/// Panel connected.
pub const PANEL: &str = "tt5-panel-symbolic";
/// Panel off or daemon down.
pub const PANEL_OFF: &str = "tt5-panel-off-symbolic";
/// Searching for the panel.
pub const SEARCHING: &str = "tt5-searching-symbolic";
/// The Display page.
pub const DISPLAY: &str = "tt5-display-symbolic";
/// The Area page.
pub const AREA: &str = "tt5-area-symbolic";
/// The Boot page.
pub const POWER: &str = "tt5-power-symbolic";
/// Warning.
pub const WARNING: &str = "tt5-warning-symbolic";
/// The Profiles page.
pub const PROFILES: &str = "tt5-profiles-symbolic";
/// Drag handle of a reorderable row.
pub const HANDLE: &str = "tt5-handle-symbolic";

/// Each icon with the Adwaita icon used while its SVG is not embedded.
const FALLBACKS: &[(&str, &str)] = &[
    (OPEN, "document-open-symbolic"),
    (CLEAR, "edit-clear-all-symbolic"),
    (APPLY, "object-select-symbolic"),
    (BRIGHTNESS, "display-brightness-symbolic"),
    (FPS, "media-playback-start-symbolic"),
    (CROP, "edit-cut-symbolic"),
    (ZOOM_IN, "zoom-in-symbolic"),
    (ZOOM_OUT, "zoom-out-symbolic"),
    (RESET, "edit-undo-symbolic"),
    (REMOVE, "window-close-symbolic"),
    (PANEL, "video-display-symbolic"),
    (PANEL_OFF, "video-display-symbolic"),
    (SEARCHING, "video-display-symbolic"),
    (DISPLAY, "video-display-symbolic"),
    (AREA, "video-display-symbolic"),
    (POWER, "system-shutdown-symbolic"),
    (WARNING, "dialog-warning-symbolic"),
    (PROFILES, "view-list-symbolic"),
    (HANDLE, "list-drag-handle-symbolic"),
];

const APP_ICON_NAME: &str = "io.github.krakerz.Tt500Lcd";
const APP_ICON: &[u8] = include_bytes!("../../data/io.github.krakerz.Tt500Lcd.svg");

/// The icon name to use: `name` when its SVG is embedded, else the Adwaita fallback.
#[must_use]
pub fn icon(name: &'static str) -> &'static str {
    if ICONS.iter().any(|(stem, _)| *stem == name) {
        return name;
    }
    FALLBACKS
        .iter()
        .find(|(icon, _)| *icon == name)
        .map_or(name, |(_, fallback)| fallback)
}

/// Writes the embedded icons to the cache directory and adds it to the icon search path.
pub fn install() {
    let Some(dirs) = ProjectDirs::from("", "", "tt500-lcd") else {
        tracing::warn!("no home directory, icons are not installed");
        return;
    };
    let dir = dirs.cache_dir().join("icons");
    if let Err(error) = fs::create_dir_all(&dir) {
        tracing::warn!("cannot create {}: {error}", dir.display());
        return;
    }
    for (name, bytes) in ICONS {
        write_if_changed(&dir.join(format!("{name}.svg")), bytes);
    }
    write_if_changed(&dir.join(format!("{APP_ICON_NAME}.svg")), APP_ICON);
    match gtk::gdk::Display::default() {
        Some(display) => gtk::IconTheme::for_display(&display).add_search_path(&dir),
        None => tracing::warn!("no display, icons are not installed"),
    }
}

fn write_if_changed(path: &Path, bytes: &[u8]) {
    if fs::read(path).is_ok_and(|existing| existing == bytes) {
        return;
    }
    if let Err(error) = fs::write(path, bytes) {
        tracing::warn!("cannot write {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: &[&str] = &[
        OPEN, CLEAR, APPLY, BRIGHTNESS, FPS, CROP, ZOOM_IN, ZOOM_OUT, RESET, REMOVE, PANEL,
        PANEL_OFF, SEARCHING, DISPLAY, AREA, POWER, WARNING, PROFILES, HANDLE,
    ];

    #[test]
    fn every_listed_icon_has_a_constant_and_a_fallback() {
        let listed: Vec<&str> = include_str!("../../data/icons/icons.txt")
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        assert_eq!(listed.len(), ALL.len(), "icons.txt vs constants");
        for name in listed {
            assert!(ALL.contains(&name), "no constant for {name}");
            assert!(
                FALLBACKS.iter().any(|(icon, _)| *icon == name),
                "no fallback for {name}"
            );
        }
    }
}
