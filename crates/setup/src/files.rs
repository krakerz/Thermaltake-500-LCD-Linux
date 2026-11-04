use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::SetupError;

/// The user's config directory: `$XDG_CONFIG_HOME` if absolute, else `$HOME/.config`.
///
/// # Errors
///
/// Returns [`SetupError::NoConfigHome`] when neither variable gives an absolute path.
pub fn config_home() -> Result<PathBuf, SetupError> {
    resolve_config_home(env::var_os("XDG_CONFIG_HOME"), env::var_os("HOME"))
}

fn resolve_config_home(
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf, SetupError> {
    let xdg = xdg.map(PathBuf::from).filter(|p| p.is_absolute());
    let home = home
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| p.join(".config"));
    xdg.or(home).ok_or(SetupError::NoConfigHome)
}

/// Finds `name`: next to the running executable, else the first match on `PATH`.
///
/// # Errors
///
/// Returns [`SetupError::BinaryNotFound`] when it is in neither place.
pub fn find_binary(name: &'static str) -> Result<PathBuf, SetupError> {
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    find_in(name, exe_dir.as_deref(), env::var_os("PATH").as_deref())
}

pub(crate) fn find_in(
    name: &'static str,
    exe_dir: Option<&Path>,
    path_var: Option<&OsStr>,
) -> Result<PathBuf, SetupError> {
    let beside = exe_dir.map(|dir| dir.join(name));
    let on_path = path_var
        .into_iter()
        .flat_map(env::split_paths)
        .map(|dir| dir.join(name));
    beside
        .into_iter()
        .chain(on_path)
        .find(|candidate| candidate.is_file())
        .ok_or(SetupError::BinaryNotFound(name))
}

pub(crate) fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> SetupError {
    let path = path.to_path_buf();
    move |source| SetupError::Io {
        action,
        path,
        source,
    }
}

/// Writes `contents` to `path` through a temporary file in the same directory.
pub(crate) fn write_atomic(path: &Path, contents: &str) -> Result<(), SetupError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(io_error("creating", dir))?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = dir.join(format!(".{name}.tmp"));
    let write = || -> io::Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, path)
    };
    write().map_err(|source| {
        let _ = fs::remove_file(&temp);
        io_error("writing", path)(source)
    })
}

/// Removes `path`; a file that is already gone is fine.
pub(crate) fn remove_if_exists(path: &Path) -> Result<(), SetupError> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_error("removing", path)(e)),
        _ => Ok(()),
    }
}

/// Replaces the first line starting with `key` by `key` + `value`.
pub(crate) fn replace_line(text: &str, key: &str, value: &str) -> String {
    let mut done = false;
    let mut out = String::with_capacity(text.len() + value.len());
    for line in text.lines() {
        if !done && line.starts_with(key) {
            out.push_str(key);
            out.push_str(value);
            done = true;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_home_prefers_absolute_xdg() {
        let got = resolve_config_home(Some("/x".into()), Some("/home/u".into()));
        assert_eq!(got.unwrap(), PathBuf::from("/x"));
        let got = resolve_config_home(Some("rel".into()), Some("/home/u".into()));
        assert_eq!(got.unwrap(), PathBuf::from("/home/u/.config"));
        assert!(resolve_config_home(None, None).is_err());
    }

    #[test]
    fn finds_beside_exe_before_path() {
        let beside = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        fs::write(beside.path().join("tool"), "").unwrap();
        fs::write(other.path().join("tool"), "").unwrap();
        let path = env::join_paths([other.path()]).unwrap();
        let got = find_in("tool", Some(beside.path()), Some(&path)).unwrap();
        assert_eq!(got, beside.path().join("tool"));
        let got = find_in("tool", Some(Path::new("/nonexistent")), Some(&path)).unwrap();
        assert_eq!(got, other.path().join("tool"));
        assert!(matches!(
            find_in("missing", None, Some(&path)),
            Err(SetupError::BinaryNotFound("missing"))
        ));
    }

    #[test]
    fn write_atomic_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a/b.txt");
        write_atomic(&target, "one").unwrap();
        write_atomic(&target, "two").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "two");
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 1);
    }
}
