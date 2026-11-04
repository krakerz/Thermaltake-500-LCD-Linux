//! Socket location.

use std::env;
use std::path::PathBuf;

/// Environment variable that overrides the socket path (used by tests).
pub const SOCKET_ENV: &str = "TT500_LCD_SOCKET";

/// No socket location could be determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SocketPathError {
    /// Neither `TT500_LCD_SOCKET` nor `XDG_RUNTIME_DIR` is set.
    #[error("neither {SOCKET_ENV} nor XDG_RUNTIME_DIR is set")]
    NoRuntimeDir,
}

/// The daemon socket path: `$TT500_LCD_SOCKET`, else `$XDG_RUNTIME_DIR/tt500-lcd/daemon.sock`.
///
/// # Errors
///
/// Returns [`SocketPathError::NoRuntimeDir`] when neither variable is set.
pub fn socket_path() -> Result<PathBuf, SocketPathError> {
    resolve(env::var_os(SOCKET_ENV), env::var_os("XDG_RUNTIME_DIR"))
}

fn resolve(
    override_path: Option<std::ffi::OsString>,
    runtime_dir: Option<std::ffi::OsString>,
) -> Result<PathBuf, SocketPathError> {
    if let Some(path) = override_path.filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let dir = runtime_dir
        .filter(|p| !p.is_empty())
        .ok_or(SocketPathError::NoRuntimeDir)?;
    Ok(PathBuf::from(dir).join("tt500-lcd").join("daemon.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins() {
        let got = resolve(Some("/x/s.sock".into()), Some("/run/user/1".into()));
        assert_eq!(got, Ok(PathBuf::from("/x/s.sock")));
    }

    #[test]
    fn runtime_dir_default() {
        let got = resolve(None, Some("/run/user/1".into()));
        assert_eq!(got, Ok(PathBuf::from("/run/user/1/tt500-lcd/daemon.sock")));
    }

    #[test]
    fn neither_is_an_error() {
        assert_eq!(resolve(None, None), Err(SocketPathError::NoRuntimeDir));
        assert_eq!(
            resolve(Some("".into()), None),
            Err(SocketPathError::NoRuntimeDir)
        );
    }
}
