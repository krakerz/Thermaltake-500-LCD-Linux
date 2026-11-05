//! Names of the running processes, read from `/proc`.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

/// The kernel truncates `comm` to this many bytes.
const COMM_BYTES: usize = 15;

/// Collects the `comm` and the executable basename of every process under `proc_root`.
///
/// Kernel threads (no `exe` link) contribute nothing; unreadable entries are skipped.
#[must_use]
pub fn process_names_in(proc_root: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let Ok(entries) = fs::read_dir(proc_root) else {
        return names;
    };
    for entry in entries.flatten() {
        let is_pid = entry
            .file_name()
            .to_str()
            .is_some_and(|name| !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()));
        if !is_pid {
            continue;
        }
        let dir = entry.path();
        let exe = fs::read_link(dir.join("exe"));
        if exe
            .as_ref()
            .is_err_and(|err| err.kind() == ErrorKind::NotFound)
        {
            continue;
        }
        if let Ok(comm) = fs::read_to_string(dir.join("comm")) {
            let comm = comm.trim();
            if !comm.is_empty() {
                names.insert(comm.to_owned());
            }
        }
        if let Ok(exe) = exe
            && let Some(base) = exe.file_name().and_then(|name| name.to_str())
        {
            let base = base.strip_suffix(" (deleted)").unwrap_or(base);
            if !base.is_empty() {
                names.insert(base.to_owned());
            }
        }
    }
    names
}

/// Names of all running processes (`/proc`).
#[must_use]
pub fn running_process_names() -> BTreeSet<String> {
    process_names_in(Path::new("/proc"))
}

/// Whether `configured` matches one of `names`.
///
/// A configured name longer than the kernel's 15-byte `comm` also matches when its first
/// 15 bytes equal an entry.
#[must_use]
pub fn process_running(names: &BTreeSet<String>, configured: &str) -> bool {
    if names.contains(configured) {
        return true;
    }
    configured
        .get(..COMM_BYTES)
        .is_some_and(|prefix| configured.len() > COMM_BYTES && names.contains(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn fake_process(root: &Path, pid: &str, comm: &str, exe: Option<&str>) {
        let dir = root.join(pid);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
        if let Some(exe) = exe {
            symlink(exe, dir.join("exe")).unwrap();
        }
    }

    #[test]
    fn scans_comm_and_exe() {
        let root = tempfile::tempdir().unwrap();
        fake_process(
            root.path(),
            "100",
            "game.exe",
            Some("/opt/wine/wine64-preloader"),
        );
        fake_process(root.path(), "101", "kworker/0:1", None);
        fake_process(root.path(), "102", "old", Some("/usr/bin/tool (deleted)"));
        fs::create_dir(root.path().join("self")).unwrap();
        fs::write(root.path().join("uptime"), "1 2").unwrap();
        let names = process_names_in(root.path());
        let want: BTreeSet<String> = ["game.exe", "wine64-preloader", "old", "tool"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(names, want);
    }

    #[test]
    fn missing_root_is_empty() {
        assert!(process_names_in(Path::new("/nonexistent-proc-root")).is_empty());
    }

    #[test]
    fn matches_truncated_comm() {
        let names: BTreeSet<String> = ["a-very-long-pro".to_owned(), "short".to_owned()].into();
        assert!(process_running(&names, "short"));
        assert!(process_running(&names, "a-very-long-process-name"));
        assert!(!process_running(
            &names,
            "a-very-long-pro-x"[..15].replace('p', "q").as_str()
        ));
        assert!(!process_running(&names, "other"));
        assert!(!process_running(&names, "shortish"));
    }
}
