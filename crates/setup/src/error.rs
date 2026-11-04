use std::io;
use std::path::PathBuf;

/// Why a setup action failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SetupError {
    /// A binary was found neither next to this executable nor on `PATH`.
    #[error("{0} not found (looked next to this program and in PATH)")]
    BinaryNotFound(&'static str),
    /// Neither `XDG_CONFIG_HOME` nor `HOME` gives a usable config directory.
    #[error("cannot determine the config directory: set XDG_CONFIG_HOME or HOME")]
    NoConfigHome,
    /// A path cannot be written into a unit or desktop file.
    #[error("the path {} cannot be used in a unit or desktop file", .0.display())]
    UnsupportedPath(PathBuf),
    /// A daemon answers on the socket but is not the systemd unit.
    #[error(
        "tt500-lcdd is already running outside systemd; stop it first (e.g. `pkill tt500-lcdd`), then install the service"
    )]
    DaemonRunningOutsideSystemd,
    /// `systemctl` could not be started.
    #[error("cannot run systemctl: {0}")]
    Spawn(#[source] io::Error),
    /// `systemctl` exited with an error.
    #[error("`systemctl --user {command}` failed: {detail}")]
    Systemctl {
        /// The subcommand and arguments.
        command: String,
        /// Its error output, or the exit status when empty.
        detail: String,
    },
    /// A file operation failed.
    #[error("{action} {}: {source}", path.display())]
    Io {
        /// What was being done, e.g. "writing".
        action: &'static str,
        /// The file or directory.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
}
