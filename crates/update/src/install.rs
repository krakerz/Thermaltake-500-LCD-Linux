use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

/// Environment variable that replaces the release API URL (for tests against a fake server).
pub const UPDATE_URL_ENV: &str = "TT500_UPDATE_URL";

/// Written by `install.sh` under `<prefix>/share/`; tells an installed copy from a build tree.
pub(crate) const MARKER: &str = "share/tt500-lcd/installed-by-install-sh";

/// File-name suffix of the release archive.
pub(crate) const ARCHIVE_SUFFIX: &str = "-linux-x86_64.tar.gz";
/// File-name suffix of the AppImage.
pub(crate) const APPIMAGE_SUFFIX: &str = "-x86_64.AppImage";

/// How this copy of the program got onto the machine; decides what an update replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstallKind {
    /// Unpacked from the release archive by `install.sh` under `prefix`.
    Archive {
        /// The install prefix, e.g. `~/.local`; the binaries are in `<prefix>/bin`.
        prefix: PathBuf,
    },
    /// Running from an AppImage file.
    AppImage {
        /// The `.AppImage` file.
        path: PathBuf,
    },
    /// A cargo or development build: nothing to update.
    Source,
}

impl InstallKind {
    /// Detects the kind from the running executable and the `APPIMAGE` variable.
    #[must_use]
    pub fn current() -> Self {
        let exe = env::current_exe()
            .ok()
            .and_then(|exe| exe.canonicalize().ok());
        Self::detect(exe.as_deref(), env::var_os("APPIMAGE").as_deref())
    }

    pub(crate) fn detect(exe: Option<&Path>, appimage: Option<&OsStr>) -> Self {
        let image = appimage
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_file());
        if let Some(path) = image {
            return Self::AppImage { path };
        }
        let prefix = exe
            .and_then(Path::parent)
            .filter(|bin| bin.file_name() == Some(OsStr::new("bin")))
            .and_then(Path::parent)
            .filter(|prefix| fs::metadata(prefix.join(MARKER)).is_ok());
        prefix.map_or(Self::Source, |prefix| Self::Archive {
            prefix: prefix.to_path_buf(),
        })
    }

    /// A phrase for the UI: how this copy was installed.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Archive { .. } => "installed with install.sh",
            Self::AppImage { .. } => "AppImage",
            Self::Source => "built from source",
        }
    }

    /// Whether updates can be installed.
    #[must_use]
    pub fn can_update(&self) -> bool {
        !matches!(self, Self::Source)
    }

    /// The GUI to start again after an update.
    #[must_use]
    pub fn gui_path(&self) -> Option<PathBuf> {
        match self {
            Self::Archive { prefix } => Some(prefix.join("bin").join(tt500_lcd_setup::GUI_BINARY)),
            Self::AppImage { path } => Some(path.clone()),
            Self::Source => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_needs_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("bin/tt500-lcd");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, "").unwrap();
        assert_eq!(InstallKind::detect(Some(&exe), None), InstallKind::Source);
        let marker = dir.path().join(MARKER);
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(&marker, "").unwrap();
        let kind = InstallKind::detect(Some(&exe), None);
        assert_eq!(
            kind,
            InstallKind::Archive {
                prefix: dir.path().to_path_buf()
            }
        );
        assert_eq!(
            kind.gui_path().unwrap(),
            dir.path().join("bin/tt500-lcd-gui")
        );
    }

    #[test]
    fn appimage_wins_when_the_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("TT500.AppImage");
        fs::write(&image, "").unwrap();
        let kind = InstallKind::detect(
            Some(Path::new("/tmp/mnt/usr/bin/x")),
            Some(image.as_os_str()),
        );
        assert_eq!(kind, InstallKind::AppImage { path: image });
        let missing = dir.path().join("gone.AppImage");
        assert_eq!(
            InstallKind::detect(None, Some(missing.as_os_str())),
            InstallKind::Source
        );
    }

    #[test]
    fn source_builds_cannot_update() {
        assert!(!InstallKind::Source.can_update());
        assert!(InstallKind::Source.gui_path().is_none());
    }
}
