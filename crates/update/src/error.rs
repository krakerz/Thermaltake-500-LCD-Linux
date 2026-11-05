use std::io;
use std::path::PathBuf;

use tt500_lcd_setup::SetupError;

/// Why checking for or installing an update failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UpdateError {
    /// This copy was built from source, not installed from a release.
    #[error("updates are only available for installed builds (this one was built from source)")]
    FromSource,
    /// A request failed or the server answered with an error.
    #[error("cannot reach the release server: {0}")]
    Http(String),
    /// The release data is not what GitHub's API returns.
    #[error("unexpected release data: {0}")]
    BadRelease(String),
    /// The release has no download for this install method.
    #[error("release {version} has no download ending in {suffix}")]
    NoAsset {
        /// The release's version.
        version: String,
        /// The file-name suffix looked for.
        suffix: &'static str,
    },
    /// The downloaded file does not match the published checksum.
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// The published SHA-256.
        expected: String,
        /// The SHA-256 of the download.
        actual: String,
    },
    /// The release archive lacks a file the installer needs.
    #[error("the release archive does not contain {0}")]
    MissingInArchive(String),
    /// The new copy does not run, so the old one was put back.
    #[error("the new version does not start ({0}); the old version was restored")]
    NewCopyBroken(String),
    /// A file operation failed.
    #[error("{action} {}: {source}", path.display())]
    Io {
        /// What was being done, e.g. "downloading to".
        action: &'static str,
        /// The file or directory.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// Restarting the service or finding the config directory failed.
    #[error(transparent)]
    Setup(#[from] SetupError),
}

pub(crate) fn io_error(
    action: &'static str,
    path: &std::path::Path,
) -> impl FnOnce(io::Error) -> UpdateError {
    let path = path.to_path_buf();
    move |source| UpdateError::Io {
        action,
        path,
        source,
    }
}
