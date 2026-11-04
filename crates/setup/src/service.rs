use std::path::{Path, PathBuf};
use std::process::Output;

use tt500_lcd_ipc::Client;

use crate::SetupError;
use crate::files::{remove_if_exists, replace_line, write_atomic};
use crate::runner::Runner;

/// Name of the systemd user unit.
pub const UNIT_NAME: &str = "tt500-lcd.service";

/// The unit shipped in `packaging/`; only its `ExecStart` is replaced.
const UNIT_TEMPLATE: &str = include_str!("../../../packaging/systemd/tt500-lcd.service");

/// What systemd says about the daemon's user unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ServiceState {
    /// The unit file exists in the user's systemd directory.
    pub unit_installed: bool,
    /// `systemctl --user is-enabled` succeeds.
    pub enabled: bool,
    /// `systemctl --user is-active` succeeds.
    pub active: bool,
}

impl ServiceState {
    /// A short label: "running", "stopped" or "not installed".
    #[must_use]
    pub fn label(&self) -> &'static str {
        match (self.unit_installed, self.active) {
            (false, _) => "not installed",
            (true, true) => "running",
            (true, false) => "stopped",
        }
    }
}

fn unit_path(config_home: &Path) -> PathBuf {
    config_home.join("systemd/user").join(UNIT_NAME)
}

/// Quotes `path` for a systemd `ExecStart=` line.
fn exec_start(daemon: &Path) -> Result<String, SetupError> {
    let bad = || SetupError::UnsupportedPath(daemon.to_path_buf());
    let text = daemon.to_str().ok_or_else(bad)?;
    if text.contains(['\n', '"', '\\']) {
        return Err(bad());
    }
    let text = text.replace('%', "%%");
    Ok(if text.contains(char::is_whitespace) {
        format!("\"{text}\"")
    } else {
        text
    })
}

fn render_unit(daemon: &Path) -> Result<String, SetupError> {
    Ok(replace_line(
        UNIT_TEMPLATE,
        "ExecStart=",
        &exec_start(daemon)?,
    ))
}

/// Runs `systemctl --user ARGS`; the exit status is the answer.
fn systemctl(runner: &dyn Runner, args: &[&str]) -> Result<Output, SetupError> {
    let mut full = vec!["--user"];
    full.extend_from_slice(args);
    runner.run("systemctl", &full).map_err(SetupError::Spawn)
}

fn systemctl_ok(runner: &dyn Runner, args: &[&str]) -> Result<(), SetupError> {
    let output = systemctl(runner, args)?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = match stderr.trim() {
        "" => output.status.to_string(),
        text => text.to_owned(),
    };
    Err(SetupError::Systemctl {
        command: args.join(" "),
        detail,
    })
}

/// Reads the unit's state.
///
/// # Errors
///
/// Returns [`SetupError::Spawn`] when `systemctl` cannot be started.
pub fn service_status(runner: &dyn Runner, config_home: &Path) -> Result<ServiceState, SetupError> {
    let succeeds = |verb: &str| -> Result<bool, SetupError> {
        Ok(systemctl(runner, &[verb, UNIT_NAME])?.status.success())
    };
    Ok(ServiceState {
        unit_installed: unit_path(config_home).is_file(),
        enabled: succeeds("is-enabled")?,
        active: succeeds("is-active")?,
    })
}

/// Writes the unit for `daemon`, then reloads systemd and enables and starts it.
///
/// With `socket` set, a daemon answering there while the unit is not active is refused,
/// since the unit's own daemon could not take the socket.
///
/// # Errors
///
/// Fails when a daemon runs outside systemd, the unit cannot be written, or `systemctl` fails.
pub fn install_service(
    runner: &dyn Runner,
    config_home: &Path,
    daemon: &Path,
    socket: Option<&Path>,
) -> Result<(), SetupError> {
    let unit = render_unit(daemon)?;
    if let Some(socket) = socket
        && Client::connect(socket).is_ok()
        && !systemctl(runner, &["is-active", UNIT_NAME])?
            .status
            .success()
    {
        return Err(SetupError::DaemonRunningOutsideSystemd);
    }
    write_atomic(&unit_path(config_home), &unit)?;
    systemctl_ok(runner, &["daemon-reload"])?;
    systemctl_ok(runner, &["enable", "--now", UNIT_NAME])
}

/// Disables and stops the unit, removes its file and reloads systemd.
///
/// # Errors
///
/// Fails when the unit file cannot be removed or `systemctl` fails.
pub fn uninstall_service(runner: &dyn Runner, config_home: &Path) -> Result<(), SetupError> {
    let path = unit_path(config_home);
    if path.is_file() {
        systemctl_ok(runner, &["disable", "--now", UNIT_NAME])?;
    }
    remove_if_exists(&path)?;
    systemctl_ok(runner, &["daemon-reload"])
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::io;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    use super::*;

    /// Records every call and answers success, except for the verbs listed in `fail`.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<String>>,
        fail: Vec<&'static str>,
    }

    impl Runner for Fake {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
            let line = format!("{program} {}", args.join(" "));
            let failing = args.iter().any(|a| self.fail.contains(a));
            self.calls.borrow_mut().push(line);
            Ok(Output {
                status: ExitStatus::from_raw(if failing { 1 << 8 } else { 0 }),
                stdout: Vec::new(),
                stderr: if failing {
                    b"boom\n".to_vec()
                } else {
                    Vec::new()
                },
            })
        }
    }

    #[test]
    fn install_writes_unit_with_absolute_path_and_runs_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        install_service(&fake, dir.path(), Path::new("/opt/my bin/tt500-lcdd"), None).unwrap();
        let unit = fs::read_to_string(unit_path(dir.path())).unwrap();
        assert!(
            unit.contains("ExecStart=\"/opt/my bin/tt500-lcdd\"\n"),
            "{unit}"
        );
        assert!(unit.contains("WantedBy=default.target"));
        assert!(!unit.contains("%h"));
        assert_eq!(
            *fake.calls.borrow(),
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable --now tt500-lcd.service"
            ]
        );
    }

    #[test]
    fn install_reports_systemctl_failure() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake {
            fail: vec!["enable"],
            ..Fake::default()
        };
        let err = install_service(&fake, dir.path(), Path::new("/b/tt500-lcdd"), None).unwrap_err();
        assert!(err.to_string().contains("enable --now"), "{err}");
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn install_refuses_a_daemon_running_outside_systemd() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let fake = Fake {
            fail: vec!["is-active"],
            ..Fake::default()
        };
        let err = install_service(&fake, dir.path(), Path::new("/b/tt500-lcdd"), Some(&socket))
            .unwrap_err();
        assert!(
            matches!(err, SetupError::DaemonRunningOutsideSystemd),
            "{err}"
        );
        assert!(!unit_path(dir.path()).exists());
    }

    #[test]
    fn uninstall_disables_removes_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        install_service(&fake, dir.path(), Path::new("/b/tt500-lcdd"), None).unwrap();
        fake.calls.borrow_mut().clear();
        uninstall_service(&fake, dir.path()).unwrap();
        assert!(!unit_path(dir.path()).exists());
        assert_eq!(
            *fake.calls.borrow(),
            [
                "systemctl --user disable --now tt500-lcd.service",
                "systemctl --user daemon-reload"
            ]
        );
    }

    #[test]
    fn uninstall_without_a_unit_only_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        uninstall_service(&fake, dir.path()).unwrap();
        assert_eq!(*fake.calls.borrow(), ["systemctl --user daemon-reload"]);
    }

    #[test]
    fn status_uses_exit_codes() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake {
            fail: vec!["is-active"],
            ..Fake::default()
        };
        let state = service_status(&fake, dir.path()).unwrap();
        assert_eq!(
            state,
            ServiceState {
                unit_installed: false,
                enabled: true,
                active: false
            }
        );
        assert_eq!(state.label(), "not installed");
        let installed = ServiceState {
            unit_installed: true,
            active: true,
            ..state
        };
        assert_eq!(installed.label(), "running");
    }

    #[test]
    fn rejects_paths_that_cannot_be_quoted() {
        assert!(exec_start(Path::new("/a\"b/d")).is_err());
        assert_eq!(exec_start(Path::new("/a%b/d")).unwrap(), "/a%%b/d");
    }
}
