//! File work for an update: verified download, archive extraction and the all-or-nothing swap.

use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{UpdateError, io_error};
use crate::http::Http;

/// Mode of an installed executable.
const EXECUTABLE: u32 = 0o755;

/// Hashes everything written through it.
struct HashingWriter<W> {
    inner: W,
    hash: Sha256,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Downloads `url` to `dest`, checking its SHA-256 against `expected` when given.
/// A failed or mismatching download leaves nothing behind.
pub(crate) fn download(
    http: &dyn Http,
    url: &str,
    dest: &Path,
    expected: Option<&str>,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), UpdateError> {
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).map_err(io_error("creating", dir))?;
    }
    let result = (|| {
        let file = File::create(dest).map_err(io_error("creating", dest))?;
        let mut writer = HashingWriter {
            inner: file,
            hash: Sha256::new(),
        };
        http.get(url, &mut writer, progress)?;
        writer.flush().map_err(io_error("writing", dest))?;
        let actual = hex(&writer.hash.finalize());
        match expected {
            Some(expected) if expected != actual => Err(UpdateError::ChecksumMismatch {
                expected: expected.to_owned(),
                actual,
            }),
            _ => Ok(()),
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(dest);
    }
    result
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Unpacks a `.tar.gz` into `into` (the `tar` crate refuses paths that escape it).
pub(crate) fn extract(archive: &Path, into: &Path) -> Result<(), UpdateError> {
    fs::create_dir_all(into).map_err(io_error("creating", into))?;
    let file = File::open(archive).map_err(io_error("opening", archive))?;
    tar::Archive::new(flate2::read::GzDecoder::new(file))
        .unpack(into)
        .map_err(io_error("unpacking into", into))
}

/// The directory under `root` that holds `bin/<marker>`: the archive's versioned folder.
pub(crate) fn find_bundle(root: &Path, marker: &str) -> Result<PathBuf, UpdateError> {
    let entries = fs::read_dir(root).map_err(io_error("reading", root))?;
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|dir| dir.join("bin").join(marker).is_file())
        .ok_or_else(|| UpdateError::MissingInArchive(format!("bin/{marker}")))
}

/// One finished replacement: `dest` now holds the new file, `aside` the original.
#[derive(Debug)]
pub(crate) struct Swap {
    dest: PathBuf,
    aside: Option<PathBuf>,
}

/// A new file waiting next to the file it replaces.
#[derive(Debug)]
pub(crate) struct Staged {
    pub new: PathBuf,
    pub dest: PathBuf,
}

fn hidden_sibling(dest: &Path, suffix: &str) -> PathBuf {
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    dest.with_file_name(format!(".{name}.{suffix}"))
}

/// Copies `source` next to `dest` as `.name.new` with the executable bit set. Copy-then-rename
/// is what lets a running binary be replaced (writing over it fails with "text file busy").
pub(crate) fn stage(source: &Path, dest: &Path) -> Result<Staged, UpdateError> {
    let new = hidden_sibling(dest, "new");
    let copy = fs::copy(source, &new)
        .and_then(|_| fs::set_permissions(&new, fs::Permissions::from_mode(EXECUTABLE)));
    match copy {
        Ok(()) => Ok(Staged {
            new,
            dest: dest.to_path_buf(),
        }),
        Err(source_error) => {
            let _ = fs::remove_file(&new);
            Err(io_error("staging", dest)(source_error))
        }
    }
}

/// Puts every staged file in place, or none: on the first failure every swap made so far is
/// undone, newest first, and the not yet used staged files are deleted.
pub(crate) fn replace_all(staged: &[Staged]) -> Result<Vec<Swap>, UpdateError> {
    let mut done: Vec<Swap> = Vec::new();
    for (index, item) in staged.iter().enumerate() {
        let aside = item
            .dest
            .exists()
            .then(|| hidden_sibling(&item.dest, "old"));
        let mut step = || -> Result<(), UpdateError> {
            if let Some(aside) = &aside {
                fs::rename(&item.dest, aside).map_err(io_error("setting aside", &item.dest))?;
            }
            done.push(Swap {
                dest: item.dest.clone(),
                aside: aside.clone(),
            });
            fs::rename(&item.new, &item.dest).map_err(io_error("replacing", &item.dest))
        };
        if let Err(error) = step() {
            undo(&done);
            discard_staged(&staged[index..]);
            return Err(error);
        }
    }
    Ok(done)
}

/// Deletes staged files that will not be used.
pub(crate) fn discard_staged(staged: &[Staged]) {
    for item in staged {
        let _ = fs::remove_file(&item.new);
    }
}

/// Restores the originals set aside by `swaps`, newest first.
pub(crate) fn undo(swaps: &[Swap]) {
    for swap in swaps.iter().rev() {
        match &swap.aside {
            Some(aside) => {
                let _ = fs::rename(aside, &swap.dest);
            }
            None => {
                let _ = fs::remove_file(&swap.dest);
            }
        }
    }
}

/// Deletes the originals once the update is final. They can go at once: on Linux a running
/// program keeps its unlinked file.
pub(crate) fn discard(swaps: &[Swap]) {
    for aside in swaps.iter().filter_map(|swap| swap.aside.as_ref()) {
        let _ = fs::remove_file(aside);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn replaces_all_files_and_keeps_none_aside_after_discard() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        write(&a, "old a");
        write(&src.join("a"), "new a");
        write(&src.join("b"), "new b");
        let staged = [
            stage(&src.join("a"), &a).unwrap(),
            stage(&src.join("b"), &b).unwrap(),
        ];
        let swaps = replace_all(&staged).unwrap();
        assert_eq!((read(&a).as_str(), read(&b).as_str()), ("new a", "new b"));
        assert_eq!(
            fs::metadata(&a).unwrap().permissions().mode() & 0o777,
            EXECUTABLE
        );
        discard(&swaps);
        let mut names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["a", "b", "src"]);
    }

    #[test]
    fn a_failed_swap_restores_every_file_already_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            dir.path().join("a"),
            dir.path().join("b"),
            dir.path().join("c"),
        );
        for (path, text) in [(&a, "old a"), (&b, "old b"), (&c, "old c")] {
            write(path, text);
        }
        let source = dir.path().join("source");
        write(&source, "new");
        let mut staged = [
            stage(&source, &a).unwrap(),
            stage(&source, &b).unwrap(),
            stage(&source, &c).unwrap(),
        ];
        // The third new file vanishes, so its rename fails after a and b were swapped.
        fs::remove_file(&staged[2].new).unwrap();
        let err = replace_all(&staged).unwrap_err();
        assert!(matches!(err, UpdateError::Io { .. }), "{err}");
        assert_eq!(
            (read(&a).as_str(), read(&b).as_str(), read(&c).as_str()),
            ("old a", "old b", "old c")
        );
        assert!(!staged[0].new.exists() && !staged[1].new.exists());
        staged[0].new.clear();
    }

    #[test]
    fn a_new_file_without_an_original_is_removed_on_undo() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        write(&source, "new");
        let dest = dir.path().join("fresh");
        let swaps = replace_all(&[stage(&source, &dest).unwrap()]).unwrap();
        assert!(dest.exists());
        undo(&swaps);
        assert!(!dest.exists());
    }

    #[test]
    fn finds_the_versioned_folder() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("x-1.0/bin")).unwrap();
        write(&dir.path().join("x-1.0/bin/tool"), "");
        assert_eq!(
            find_bundle(dir.path(), "tool").unwrap(),
            dir.path().join("x-1.0")
        );
        assert!(find_bundle(dir.path(), "other").is_err());
    }
}
