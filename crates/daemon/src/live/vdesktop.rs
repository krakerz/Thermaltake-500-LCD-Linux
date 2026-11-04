//! A headless sway session running user commands, for the "virtual desktop" live source.
//!
//! Sway renders on a headless output of the requested size; the daemon spawns each command
//! as `sh -c` in its own process group, restarts it with a backoff when it exits, and kills
//! every group when the desktop is dropped.

use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};
use tt500_lcd_ipc::DesktopSpec;

/// Refresh rate of the headless output; the capture rate is chosen by the worker.
const OUTPUT_REFRESH_HZ: u32 = 30;
/// How long sway gets to write its ready file.
const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// First restart delay of a client that exited; doubles per quick failure.
const BACKOFF_START: Duration = Duration::from_secs(1);
/// The longest restart delay.
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A client that ran this long counts as healthy: its backoff resets.
const HEALTHY_UPTIME: Duration = Duration::from_secs(60);
/// A client that exits sooner than this after its start counts as a quick failure.
const QUICK_EXIT: Duration = Duration::from_secs(10);
/// Consecutive quick failures after which the client is reported as a problem.
const REPORT_AFTER_QUICK_FAILURES: u32 = 5;
/// Shell exit status of a command that was found but isn't executable.
const EXIT_NOT_EXECUTABLE: i32 = 126;
/// Shell exit status of a command that wasn't found.
const EXIT_NOT_FOUND: i32 = 127;
/// Grace period between SIGTERM and SIGKILL for a client's process group.
const CLIENT_GRACE: Duration = Duration::from_secs(1);
/// Grace period for sway, which removes its sockets on SIGTERM.
const SWAY_GRACE: Duration = Duration::from_secs(2);

/// Environment variables that would make sway nest into the host session.
const HOST_SESSION_VARS: [&str; 3] = ["WAYLAND_DISPLAY", "DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE"];

/// Environment changes for a spawned process: variables to set and to remove.
#[derive(Debug, PartialEq, Eq)]
struct EnvPlan {
    set: Vec<(&'static str, OsString)>,
    remove: Vec<&'static str>,
}

impl EnvPlan {
    fn apply(&self, command: &mut Command) {
        for var in &self.remove {
            command.env_remove(var);
        }
        for (var, value) in &self.set {
            command.env(var, value);
        }
    }
}

fn sway_env(sway_sock: &Path) -> EnvPlan {
    EnvPlan {
        set: vec![
            ("WLR_BACKENDS", "headless".into()),
            ("WLR_LIBINPUT_NO_DEVICES", "1".into()),
            ("LIBSEAT_BACKEND", "noop".into()),
            ("SWAYSOCK", sway_sock.into()),
        ],
        remove: HOST_SESSION_VARS.to_vec(),
    }
}

fn client_env(wayland_display: &str, sway_sock: &Path) -> EnvPlan {
    EnvPlan {
        set: vec![
            ("WAYLAND_DISPLAY", wayland_display.into()),
            ("XDG_CURRENT_DESKTOP", "sway".into()),
            ("SWAYSOCK", sway_sock.into()),
        ],
        remove: vec!["DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE"],
    }
}

/// Quotes a path as a double-quoted shell word that sits inside an outer single-quoted
/// `sh -c '...'` string (`'` becomes `'\''`).
fn quote_in_single(path: &str) -> String {
    let mut inner = String::new();
    for c in path.chars() {
        match c {
            '\\' | '"' | '$' | '`' => {
                inner.push('\\');
                inner.push(c);
            }
            '\'' => inner.push_str("'\\''"),
            _ => inner.push(c),
        }
    }
    format!("\"{inner}\"")
}

/// The sway config: one headless output, no chrome, and an `exec` that reports the display.
fn sway_config(spec: &DesktopSpec, ready_file: &Path) -> String {
    format!(
        "output * mode {w}x{h}@{OUTPUT_REFRESH_HZ}Hz scale {scale}\n\
         output * bg #000000 solid_color\n\
         default_border none\n\
         default_floating_border none\n\
         gaps inner 0\n\
         input * events disabled\n\
         xwayland disable\n\
         seat * hide_cursor 1\n\
         exec sh -c 'printf %s \"$WAYLAND_DISPLAY\" > {ready}'\n",
        w = spec.size.width,
        h = spec.size.height,
        scale = spec.scale,
        ready = quote_in_single(&ready_file.to_string_lossy()),
    )
}

/// Restart delay after the `failures`-th consecutive quick exit (1-based): 1 s, 2 s, 4 s ... 30 s.
fn backoff_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(5);
    BACKOFF_START
        .saturating_mul(1 << doublings)
        .min(BACKOFF_MAX)
}

/// The failure count after a client exited having run for `uptime`.
fn failures_after_exit(failures: u32, uptime: Duration) -> u32 {
    if uptime >= HEALTHY_UPTIME {
        1
    } else {
        failures.saturating_add(1)
    }
}

/// Forwards a child's output to the log, one debug event per line.
fn log_output(stream: impl Read + Send + 'static, label: String) {
    thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            tracing::debug!("{label}: {line}");
        }
    });
}

fn spawn_logged(command: &mut Command, label: &str) -> std::io::Result<Child> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn()?;
    if let Some(out) = child.stdout.take() {
        log_output(out, label.to_owned());
    }
    if let Some(err) = child.stderr.take() {
        log_output(err, label.to_owned());
    }
    Ok(child)
}

fn signal_group(child: &Child, signal: Signal) {
    let pid = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
    if let Some(pid) = pid {
        // ESRCH (the group is already gone) is expected and harmless.
        let _ = kill_process_group(pid, signal);
    }
}

/// SIGTERM the child's process group, give it `grace` to exit, then SIGKILL the group and reap.
fn stop_group(child: &mut Child, grace: Duration) {
    signal_group(child, Signal::TERM);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    // Members of the group may outlive the leader.
    signal_group(child, Signal::KILL);
    let _ = child.wait();
}

/// The first word of a shell command line, for messages.
fn first_word(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or(command)
}

#[derive(Debug)]
struct Client {
    command: String,
    child: Option<Child>,
    started: Instant,
    failures: u32,
    /// Consecutive exits sooner than [`QUICK_EXIT`]; reset by a longer run.
    quick_failures: u32,
    restart_at: Instant,
    /// The command can't work (not found, not executable): it is never restarted.
    gave_up: bool,
    /// What the user should fix, while there is something.
    problem: Option<String>,
}

impl Client {
    fn new(command: String, now: Instant) -> Self {
        Self {
            command,
            child: None,
            started: now,
            failures: 0,
            quick_failures: 0,
            restart_at: now,
            gave_up: false,
            problem: None,
        }
    }

    fn spawn(&mut self, display: &str, sway_sock: &Path) -> std::io::Result<()> {
        let mut command = Command::new("sh");
        command.arg("-c").arg(&self.command);
        client_env(display, sway_sock).apply(&mut command);
        self.child = Some(spawn_logged(&mut command, &self.command)?);
        self.started = Instant::now();
        Ok(())
    }

    /// Records an exit with `code` after `uptime`: schedules the restart or gives up.
    fn on_exit(&mut self, code: Option<i32>, uptime: Duration, now: Instant) {
        self.child = None;
        let word = first_word(&self.command);
        match code {
            Some(EXIT_NOT_FOUND) => {
                self.problem = Some(format!("command not found: {word}"));
                self.gave_up = true;
                return;
            }
            Some(EXIT_NOT_EXECUTABLE) => {
                self.problem = Some(format!("command is not executable: {word}"));
                self.gave_up = true;
                return;
            }
            _ => {}
        }
        self.failures = failures_after_exit(self.failures, uptime);
        self.restart_at = now + backoff_delay(self.failures);
        self.quick_failures = if uptime < QUICK_EXIT {
            self.quick_failures.saturating_add(1)
        } else {
            0
        };
        if self.quick_failures >= REPORT_AFTER_QUICK_FAILURES {
            let status = code.map_or_else(|| "killed by a signal".to_owned(), |c| c.to_string());
            self.problem = Some(format!("`{word}` keeps exiting (status {status})"));
        }
    }

    /// Reaps an exited child, clears the problem once it stays up, and restarts when due.
    fn supervise(&mut self, now: Instant, display: &str, sway_sock: &Path) {
        if let Some(child) = &mut self.child
            && let Ok(Some(status)) = child.try_wait()
        {
            self.on_exit(status.code(), self.started.elapsed(), now);
            tracing::warn!(
                command = %self.command,
                %status,
                restart_in = ?(!self.gave_up).then(|| backoff_delay(self.failures)),
                problem = ?self.problem,
                "desktop command exited"
            );
        } else if self.child.is_some() && self.started.elapsed() >= HEALTHY_UPTIME {
            self.problem = None;
            self.quick_failures = 0;
        }
        if self.child.is_none()
            && !self.gave_up
            && now >= self.restart_at
            && let Err(e) = self.spawn(display, sway_sock)
        {
            self.failures = self.failures.saturating_add(1);
            self.restart_at = now + backoff_delay(self.failures);
            tracing::warn!(command = %self.command, error = %e, "restarting command failed");
        }
    }
}

/// A running headless sway with its supervised client commands.
#[derive(Debug)]
pub struct VirtualDesktop {
    sway: Child,
    run_dir: PathBuf,
    sway_sock: PathBuf,
    display: String,
    socket: PathBuf,
    clients: Vec<Client>,
}

impl VirtualDesktop {
    /// Starts sway headless under `run_dir` and launches the spec's commands on it.
    ///
    /// # Errors
    ///
    /// Describes why the spec is invalid, sway is missing or didn't come up, or a command
    /// couldn't be spawned. Nothing is left running on error.
    pub fn start(spec: &DesktopSpec, run_dir: &Path) -> Result<Self, String> {
        spec.validate()?;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or("XDG_RUNTIME_DIR is not set")?;
        let _ = fs::remove_dir_all(run_dir);
        fs::create_dir_all(run_dir).map_err(|e| format!("creating {}: {e}", run_dir.display()))?;

        let ready_file = run_dir.join("ready");
        let config_path = run_dir.join("sway.conf");
        let sway_sock = run_dir.join("sway.sock");
        if let Err(e) = fs::write(&config_path, sway_config(spec, &ready_file)) {
            let _ = fs::remove_dir_all(run_dir);
            return Err(format!("writing {}: {e}", config_path.display()));
        }

        let mut command = Command::new("sway");
        command.arg("--config").arg(&config_path);
        sway_env(&sway_sock).apply(&mut command);
        let sway = match spawn_logged(&mut command, "sway") {
            Ok(child) => child,
            Err(e) => {
                let _ = fs::remove_dir_all(run_dir);
                return Err(format!(
                    "starting headless sway (is `sway` installed?): {e}"
                ));
            }
        };
        // From here on, dropping `desktop` stops sway and removes the run dir.
        let mut desktop = Self {
            sway,
            run_dir: run_dir.to_owned(),
            sway_sock,
            display: String::new(),
            socket: PathBuf::new(),
            clients: Vec::new(),
        };
        desktop.display = desktop.wait_ready(&ready_file)?;
        desktop.socket = runtime.join(&desktop.display);

        let now = Instant::now();
        for command in &spec.commands {
            let mut client = Client::new(command.clone(), now);
            client
                .spawn(&desktop.display, &desktop.sway_sock)
                .map_err(|e| format!("spawning `{command}`: {e}"))?;
            desktop.clients.push(client);
        }
        Ok(desktop)
    }

    /// The Wayland socket of the virtual desktop.
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Restarts clients that exited (with backoff; not when the command can't run). Call
    /// about once a second.
    ///
    /// # Errors
    ///
    /// Sway itself exited: the desktop is dead and must be restarted.
    pub fn supervise(&mut self) -> Result<(), String> {
        match self.sway.try_wait() {
            Ok(Some(status)) => return Err(format!("headless sway exited ({status})")),
            Ok(None) => {}
            Err(e) => return Err(format!("waiting for sway: {e}")),
        }
        let now = Instant::now();
        for client in &mut self.clients {
            client.supervise(now, &self.display, &self.sway_sock);
        }
        Ok(())
    }

    /// What is wrong with the commands, if anything (for example a typo'd program name).
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        let problems: Vec<&str> = self
            .clients
            .iter()
            .filter_map(|c| c.problem.as_deref())
            .collect();
        (!problems.is_empty()).then(|| problems.join("; "))
    }

    fn wait_ready(&mut self, ready_file: &Path) -> Result<String, String> {
        let deadline = Instant::now() + READY_TIMEOUT;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.sway.try_wait() {
                return Err(format!("headless sway exited during startup ({status})"));
            }
            if let Ok(text) = fs::read_to_string(ready_file) {
                let name = text.trim();
                if !name.is_empty() {
                    return Ok(name.to_owned());
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err(format!(
            "headless sway didn't come up within {}s",
            READY_TIMEOUT.as_secs()
        ))
    }
}

impl Drop for VirtualDesktop {
    fn drop(&mut self) {
        // SIGTERM every client group first so the grace periods overlap.
        for client in &self.clients {
            if let Some(child) = &client.child {
                signal_group(child, Signal::TERM);
            }
        }
        for client in &mut self.clients {
            if let Some(child) = &mut client.child {
                stop_group(child, CLIENT_GRACE);
            }
        }
        stop_group(&mut self.sway, SWAY_GRACE);
        let _ = fs::remove_dir_all(&self.run_dir);
    }
}

#[cfg(test)]
mod tests {
    use tt500_lcd_ipc::Size;

    use super::*;

    fn spec(commands: &[&str]) -> DesktopSpec {
        DesktopSpec {
            commands: commands.iter().map(|c| (*c).to_owned()).collect(),
            size: Size {
                width: 960,
                height: 256,
            },
            scale: 2,
        }
    }

    #[test]
    fn config_has_output_mode_and_ready_exec() {
        let conf = sway_config(&spec(&["x"]), Path::new("/run/tt/ready"));
        assert!(conf.contains("output * mode 960x256@30Hz scale 2\n"));
        for line in [
            "output * bg #000000 solid_color",
            "default_border none",
            "default_floating_border none",
            "gaps inner 0",
            "input * events disabled",
            "xwayland disable",
            "seat * hide_cursor 1",
            "exec sh -c 'printf %s \"$WAYLAND_DISPLAY\" > \"/run/tt/ready\"'",
        ] {
            assert!(
                conf.lines().any(|l| l == line),
                "missing {line:?} in {conf}"
            );
        }
        assert!(!conf.contains("bar"));
    }

    #[test]
    fn ready_path_is_quoted_safely() {
        assert_eq!(quote_in_single("/a b/it's$x"), "\"/a b/it'\\''s\\$x\"");
    }

    #[test]
    fn sway_env_is_headless_and_detached_from_the_host() {
        let env = sway_env(Path::new("/run/tt/sway.sock"));
        assert!(env.set.contains(&("WLR_BACKENDS", "headless".into())));
        assert!(env.set.contains(&("WLR_LIBINPUT_NO_DEVICES", "1".into())));
        assert!(env.set.contains(&("SWAYSOCK", "/run/tt/sway.sock".into())));
        for var in ["WAYLAND_DISPLAY", "DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE"] {
            assert!(env.remove.contains(&var), "{var} must be removed");
        }
    }

    #[test]
    fn client_env_points_at_our_display_only() {
        let env = client_env("wayland-7", Path::new("/s"));
        assert!(env.set.contains(&("WAYLAND_DISPLAY", "wayland-7".into())));
        assert!(env.set.contains(&("XDG_CURRENT_DESKTOP", "sway".into())));
        assert_eq!(env.remove, ["DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE"]);
    }

    #[test]
    fn backoff_doubles_to_the_cap() {
        let secs: Vec<u64> = (1..=8).map(|n| backoff_delay(n).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn healthy_uptime_resets_the_backoff() {
        assert_eq!(failures_after_exit(4, Duration::from_secs(2)), 5);
        assert_eq!(failures_after_exit(4, HEALTHY_UPTIME), 1);
    }

    #[test]
    fn missing_or_unrunnable_commands_are_not_restarted() {
        let now = Instant::now();
        for (code, want) in [
            (127, "command not found: alacrtitty"),
            (126, "command is not executable: alacrtitty"),
        ] {
            let mut client = Client::new("alacrtitty -e ~/x.sh".to_owned(), now);
            client.on_exit(Some(code), Duration::from_millis(5), now);
            assert!(client.gave_up);
            assert_eq!(client.problem.as_deref(), Some(want));
        }
    }

    #[test]
    fn repeated_quick_exits_are_reported_but_keep_retrying() {
        let now = Instant::now();
        let mut client = Client::new("foo --bar".to_owned(), now);
        for _ in 1..REPORT_AFTER_QUICK_FAILURES {
            client.on_exit(Some(3), Duration::from_secs(1), now);
            assert_eq!(client.problem, None);
        }
        client.on_exit(Some(3), Duration::from_secs(1), now);
        assert_eq!(
            client.problem.as_deref(),
            Some("`foo` keeps exiting (status 3)")
        );
        assert!(!client.gave_up);
        assert!(client.restart_at > now);
        // A longer run resets the quick-failure streak.
        client.on_exit(Some(3), QUICK_EXIT, now);
        assert_eq!(client.quick_failures, 0);
    }

    #[test]
    fn supervisor_stops_restarting_a_command_that_is_not_found() {
        let mut client = Client::new("exit 127".to_owned(), Instant::now());
        let sock = Path::new("/nonexistent/sway.sock");
        client.spawn("wayland-none", sock).expect("spawn sh");
        let deadline = Instant::now() + Duration::from_secs(10);
        while client.problem.is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
            client.supervise(Instant::now(), "wayland-none", sock);
        }
        assert_eq!(client.problem.as_deref(), Some("command not found: exit"));
        for _ in 0..3 {
            client.supervise(
                Instant::now() + Duration::from_secs(60),
                "wayland-none",
                sock,
            );
        }
        assert!(client.child.is_none(), "must not be restarted");
    }

    fn sway_available() -> bool {
        Command::new("sway")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
            && std::env::var_os("XDG_RUNTIME_DIR").is_some()
    }

    /// Starts a real headless sway, captures frames from it, supervises a dying client and
    /// checks that dropping leaves no process behind. Skipped when sway is missing.
    #[test]
    fn real_headless_sway_captures_and_cleans_up() {
        use crate::live::capture::Capturer;
        use crate::live::screencopy::Screencopy;

        if !sway_available() {
            eprintln!("skipping: sway not installed");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let run_dir = dir.path().join("desktop");
        let marker = format!("tt500-test-{}", std::process::id());
        let mut desktop = VirtualDesktop::start(
            &spec(&[&format!("sleep 300 # {marker}"), "exit 1"]),
            &run_dir,
        )
        .expect("start desktop");

        let mut capturer = Screencopy::connect(desktop.socket_path(), None).expect("connect");
        let frame = capturer.capture().expect("capture");
        assert_eq!(frame.dimensions(), (960, 256));
        let again = capturer
            .capture()
            .expect("second capture reuses the buffer");
        assert_eq!(again.dimensions(), (960, 256));

        thread::sleep(Duration::from_millis(1200));
        desktop.supervise().expect("sway is alive");
        assert_eq!(
            desktop.problem(),
            None,
            "exit 1 is not a startup problem yet"
        );
        let socket = desktop.socket_path().to_owned();
        drop(capturer);
        drop(desktop);
        assert!(!run_dir.exists(), "run dir removed");
        assert!(!socket.exists(), "wayland socket removed");
        let left = Command::new("pgrep")
            .args(["-f", &marker])
            .output()
            .expect("pgrep");
        assert!(
            String::from_utf8_lossy(&left.stdout).trim().is_empty(),
            "client processes left behind"
        );
    }
}
