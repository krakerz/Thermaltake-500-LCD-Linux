use std::path::{Path, PathBuf};

use crate::SetupError;
use crate::files::{remove_if_exists, replace_line, write_atomic};

/// The app's desktop file; only its `Exec` is replaced.
const DESKTOP_TEMPLATE: &str = include_str!("../../gui/data/io.github.krakerz.Tt500Lcd.desktop");
const AUTOSTART_FILE: &str = "io.github.krakerz.Tt500Lcd.desktop";

fn entry_path(config_home: &Path) -> PathBuf {
    config_home.join("autostart").join(AUTOSTART_FILE)
}

/// Quotes `path` for a desktop-file `Exec=` key.
fn exec_key(gui: &Path) -> Result<String, SetupError> {
    let bad = || SetupError::UnsupportedPath(gui.to_path_buf());
    let text = gui.to_str().ok_or_else(bad)?;
    if text.contains(['\n', '"', '`', '$', '\\', '%']) {
        return Err(bad());
    }
    Ok(
        if text.contains([
            ' ', '\t', '\'', '<', '>', '~', '|', '&', ';', '*', '?', '#', '(', ')',
        ]) {
            format!("\"{text}\"")
        } else {
            text.to_owned()
        },
    )
}

/// Whether the autostart entry exists.
#[must_use]
pub fn autostart_enabled(config_home: &Path) -> bool {
    entry_path(config_home).is_file()
}

/// Writes the autostart entry that starts the GUI at `gui` at login, hidden in the tray.
///
/// # Errors
///
/// Fails when the path cannot be quoted or the file cannot be written.
pub fn enable_autostart(config_home: &Path, gui: &Path) -> Result<(), SetupError> {
    let exec = format!("{} --hidden", exec_key(gui)?);
    let mut entry = replace_line(DESKTOP_TEMPLATE, "Exec=", &exec);
    entry.push_str("X-GNOME-Autostart-enabled=true\n");
    write_atomic(&entry_path(config_home), &entry)
}

/// Removes the autostart entry.
///
/// # Errors
///
/// Fails when the file exists but cannot be removed.
pub fn disable_autostart(config_home: &Path) -> Result<(), SetupError> {
    remove_if_exists(&entry_path(config_home))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn on_writes_entry_off_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!autostart_enabled(dir.path()));
        enable_autostart(dir.path(), Path::new("/opt/g ui/tt500-lcd-gui")).unwrap();
        assert!(autostart_enabled(dir.path()));
        let entry = fs::read_to_string(entry_path(dir.path())).unwrap();
        assert!(
            entry.contains("\nExec=\"/opt/g ui/tt500-lcd-gui\" --hidden\n"),
            "{entry}"
        );
        assert!(entry.contains("Name=TT500 LCD"));
        assert!(entry.ends_with("X-GNOME-Autostart-enabled=true\n"));
        disable_autostart(dir.path()).unwrap();
        assert!(!autostart_enabled(dir.path()));
        disable_autostart(dir.path()).unwrap();
    }

    #[test]
    fn rejects_unquotable_paths() {
        assert!(exec_key(Path::new("/a$b/gui")).is_err());
        assert_eq!(exec_key(Path::new("/a/gui")).unwrap(), "/a/gui");
    }
}
