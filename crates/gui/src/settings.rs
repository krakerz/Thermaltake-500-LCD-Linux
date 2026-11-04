//! The GUI's own saved settings (not the daemon's), as JSON in the user's config directory.

use std::cell::Cell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use tt500_lcd_ipc::{DESKTOP_DEFAULT_SCALE, DESKTOP_DEFAULT_SIZE, Size};

use crate::area::DEFAULT_CUSTOM;

/// Lowest preview frame rate.
pub const PREVIEW_MIN_FPS: u32 = 1;
/// Highest preview frame rate.
pub const PREVIEW_MAX_FPS: u32 = 30;
/// Default preview frame rate.
pub const PREVIEW_DEFAULT_FPS: u32 = 10;

/// Whether the preview's ffmpeg may use VA-API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HwDecode {
    /// Try VA-API and fall back to software decoding.
    #[default]
    Auto,
    /// Software decoding only.
    Off,
}

/// How the crop editor plays animations and videos.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreviewSettings {
    /// Play animations and videos; off shows the still first frame and runs no ffmpeg.
    pub play: bool,
    /// Video preview frame rate, clamped to 1..=30.
    pub fps: u32,
    /// Pause while the window is not focused (hidden or minimized always pauses).
    pub pause_unfocused: bool,
    /// Hardware decoding choice.
    pub hw_decode: HwDecode,
}

impl Default for PreviewSettings {
    fn default() -> Self {
        Self {
            play: true,
            fps: PREVIEW_DEFAULT_FPS,
            pause_unfocused: true,
            hw_decode: HwDecode::Auto,
        }
    }
}

impl PreviewSettings {
    /// The frame rate within the allowed range.
    #[must_use]
    pub fn clamped_fps(&self) -> u32 {
        self.fps.clamp(PREVIEW_MIN_FPS, PREVIEW_MAX_FPS)
    }
}

thread_local! {
    /// The preview settings of this (GTK main) thread, loaded on first use.
    static PREVIEW: Cell<Option<PreviewSettings>> = const { Cell::new(None) };
}

/// The current preview settings (read from the settings file on first use).
#[must_use]
pub fn preview() -> PreviewSettings {
    PREVIEW.with(|cell| {
        cell.get().unwrap_or_else(|| {
            let loaded = path().map_or_else(PreviewSettings::default, |p| load(&p).preview);
            cell.set(Some(loaded));
            loaded
        })
    })
}

/// Makes `preview` current and saves it, keeping the file's other settings.
pub fn set_preview(preview: PreviewSettings) {
    PREVIEW.with(|cell| cell.set(Some(preview)));
    if let Some(path) = path() {
        let mut all = load(&path);
        all.preview = preview;
        if let Err(error) = save(&path, &all) {
            tracing::warn!("cannot save {}: {error}", path.display());
        }
    }
}

/// What the Area tab remembers between runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiSettings {
    /// Commands of the virtual desktop.
    pub desktop_commands: Vec<String>,
    /// Virtual desktop size in physical pixels.
    pub desktop_size: Size,
    /// Virtual desktop scale factor.
    pub desktop_scale: u8,
    /// Last custom desktop size, kept while a preset is selected.
    pub custom_size: Size,
    /// Scale factor of the last custom desktop size.
    pub custom_scale: u8,
    /// The "steady frames" choice; `None` follows the source type's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steady: Option<bool>,
    /// The editor preview's behaviour.
    pub preview: PreviewSettings,
}

impl Default for GuiSettings {
    fn default() -> Self {
        Self {
            desktop_commands: Vec::new(),
            desktop_size: DESKTOP_DEFAULT_SIZE,
            desktop_scale: DESKTOP_DEFAULT_SCALE,
            custom_size: DEFAULT_CUSTOM.size,
            custom_scale: DEFAULT_CUSTOM.scale,
            steady: None,
            preview: PreviewSettings::default(),
        }
    }
}

/// Where the settings live, `None` without a home directory.
#[must_use]
pub fn path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "tt500-lcd").map(|dirs| dirs.config_dir().join("gui.json"))
}

/// Reads `path`; defaults when the file is missing or broken.
#[must_use]
pub fn load(path: &Path) -> GuiSettings {
    let Ok(text) = fs::read_to_string(path) else {
        return GuiSettings::default();
    };
    serde_json::from_str(&text).unwrap_or_else(|error| {
        tracing::warn!("ignoring broken settings {}: {error}", path.display());
        GuiSettings::default()
    })
}

/// Writes `settings` to `path` through a temporary file, so a crash never leaves half a file.
///
/// # Errors
/// Any I/O failure creating the directory, writing or renaming.
pub fn save(path: &Path, settings: &GuiSettings) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(io::Error::other)?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, text)?;
    fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_a_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("sub").join("gui.json");
        let settings = GuiSettings {
            desktop_commands: vec!["alacritty -e top".into(), "foot".into()],
            desktop_size: Size {
                width: 1440,
                height: 384,
            },
            desktop_scale: 3,
            custom_size: Size {
                width: 800,
                height: 600,
            },
            custom_scale: 2,
            steady: Some(false),
            preview: PreviewSettings {
                play: false,
                fps: 24,
                pause_unfocused: false,
                hw_decode: HwDecode::Off,
            },
        };
        save(&file, &settings).expect("save");
        assert_eq!(load(&file), settings);
    }

    #[test]
    fn missing_or_broken_files_give_defaults() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("gui.json");
        assert_eq!(load(&file), GuiSettings::default());
        fs::write(&file, "{not json").expect("write");
        assert_eq!(load(&file), GuiSettings::default());
    }

    #[test]
    fn missing_fields_take_their_defaults() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("gui.json");
        fs::write(&file, r#"{"desktop_scale":1}"#).expect("write");
        let loaded = load(&file);
        assert_eq!(loaded.desktop_scale, 1);
        assert_eq!(loaded.desktop_size, DESKTOP_DEFAULT_SIZE);
        assert_eq!(loaded.custom_size, DEFAULT_CUSTOM.size);
    }

    #[test]
    fn preview_settings_default_when_missing_or_partial() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("gui.json");
        fs::write(&file, r#"{"desktop_scale":1}"#).expect("write");
        assert_eq!(load(&file).preview, PreviewSettings::default());
        fs::write(&file, r#"{"preview":{"fps":5}}"#).expect("write");
        let preview = load(&file).preview;
        assert_eq!(preview.fps, 5);
        assert!(preview.play && preview.pause_unfocused);
        assert_eq!(preview.hw_decode, HwDecode::Auto);
    }

    #[test]
    fn preview_fps_is_clamped() {
        let at = |fps| PreviewSettings {
            fps,
            ..PreviewSettings::default()
        };
        assert_eq!(at(0).clamped_fps(), 1);
        assert_eq!(at(10).clamped_fps(), 10);
        assert_eq!(at(500).clamped_fps(), 30);
    }
}
