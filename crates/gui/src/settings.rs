//! The GUI's own saved settings (not the daemon's), as JSON in the user's config directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use tt500_lcd_ipc::{DESKTOP_DEFAULT_SCALE, DESKTOP_DEFAULT_SIZE, Size};

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
    /// The "steady frames" choice; `None` follows the source type's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steady: Option<bool>,
}

impl Default for GuiSettings {
    fn default() -> Self {
        Self {
            desktop_commands: Vec::new(),
            desktop_size: DESKTOP_DEFAULT_SIZE,
            desktop_scale: DESKTOP_DEFAULT_SCALE,
            steady: None,
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
            steady: Some(false),
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
    }
}
