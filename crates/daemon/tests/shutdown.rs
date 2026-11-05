//! The daemon leaves no sway or desktop command behind: on SIGINT (clean shutdown) and on
//! SIGKILL (parent-death signal). Needs `sway`; the SIGKILL case also needs `setpriv`.
//! Only processes whose command line carries this test's own temp dir or marker are looked at.

#![allow(clippy::unwrap_used)] // helpers in a test crate

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};
use tt500_lcd_ipc::{Client, DesktopSpec, Fit, LiveSource, Request, ShowLiveParams, Size};

fn have(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Pids whose command line contains `needle`.
fn pgrep(needle: &str) -> Vec<String> {
    let out = Command::new("pgrep")
        .args(["-f", needle])
        .output()
        .expect("pgrep");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn wait_until(what: &str, timeout: Duration, done: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// Starts a daemon with a virtual desktop running a harmless loop; returns it, with the
/// temp dir (whose path is in sway's command line) and the loop's marker.
fn daemon_with_desktop(root: &Path, marker: &str) -> Child {
    let socket = root.join("run/tt500-lcd/daemon.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_tt500-lcdd"))
        .arg("--no-device")
        .env("TT500_LCD_SOCKET", &socket)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut client = loop {
        match Client::connect(&socket) {
            Ok(client) => break client,
            Err(err) if Instant::now() > deadline => panic!("daemon not reachable: {err}"),
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    };
    client
        .call(Request::ShowLive(ShowLiveParams {
            source: LiveSource::Desktop(DesktopSpec {
                commands: vec![format!("while sleep 1; do :; done # {marker}")],
                size: Size {
                    width: 960,
                    height: 256,
                },
                scale: 1,
            }),
            fit: Fit::default(),
            crop: None,
            fps: None,
            steady: None,
        }))
        .expect("show the desktop");
    child
}

fn signal(child: &Child, signal: Signal) {
    let pid = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
    kill_process(pid.expect("pid"), signal).expect("signal the daemon");
}

#[test]
fn sigint_stops_the_desktop_and_exits_cleanly() {
    if !have("sway") {
        eprintln!("skipping: sway not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let root_str = root.to_str().unwrap();
    let marker = format!("tt500-int-{}", std::process::id());
    let mut daemon = daemon_with_desktop(root, &marker);
    assert!(!pgrep(root_str).is_empty(), "sway is running");
    assert!(!pgrep(&marker).is_empty(), "the desktop command is running");

    signal(&daemon, Signal::INT);
    let status = daemon.wait().unwrap();
    assert!(status.success(), "daemon exit status {status}");
    assert!(
        !root.join("run/tt500-lcd/daemon.sock").exists(),
        "socket removed"
    );
    wait_until("sway to be gone", Duration::from_secs(3), || {
        pgrep(root_str).is_empty() && pgrep(&marker).is_empty()
    });
}

#[test]
fn sigkill_takes_the_desktop_down_through_pdeathsig() {
    if !have("sway") || !have("setpriv") {
        eprintln!("skipping: sway or setpriv not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let root_str = root.to_str().unwrap();
    let marker = format!("tt500-kill-{}", std::process::id());
    let mut daemon = daemon_with_desktop(root, &marker);
    assert!(!pgrep(root_str).is_empty(), "sway is running");

    signal(&daemon, Signal::KILL);
    let _ = daemon.wait();
    wait_until("sway to be gone", Duration::from_secs(5), || {
        pgrep(root_str).is_empty() && pgrep(&marker).is_empty()
    });
}
