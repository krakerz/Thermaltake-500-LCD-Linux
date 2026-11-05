//! The link to the daemon: a watcher thread and one-shot requests, without GTK.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use tt500_lcd_ipc::{Client, ClientError, PreviewReply, ProfilesReply, Request, Status};

/// Delay between reconnect attempts while the daemon is down.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// What the watcher reports.
#[derive(Debug, Clone)]
pub enum Update {
    /// The daemon's current status (first on every connection, then on each change).
    Status(Box<Status>),
    /// The daemon cannot be reached; carries the reason. Sent once per outage.
    NotRunning(String),
}

/// Subscribes on a background thread and forwards every status; reconnects while the
/// daemon is down. The thread ends when the receiver is dropped.
pub fn watch(socket: PathBuf) -> async_channel::Receiver<Update> {
    let (tx, rx) = async_channel::unbounded();
    let spawned = thread::Builder::new()
        .name("daemon-watch".into())
        .spawn(move || watch_loop(&socket, &tx));
    if let Err(error) = spawned {
        tracing::error!("cannot start the daemon watcher: {error}");
    }
    rx
}

fn watch_loop(socket: &Path, tx: &async_channel::Sender<Update>) {
    let mut reported_down = false;
    while !tx.is_closed() {
        let outage = match Client::connect(socket).and_then(Client::subscribe) {
            Ok((status, mut subscription)) => {
                reported_down = false;
                if tx.send_blocking(Update::Status(Box::new(status))).is_err() {
                    return;
                }
                loop {
                    match subscription.next_status() {
                        Ok(Some(status)) => {
                            if tx.send_blocking(Update::Status(Box::new(status))).is_err() {
                                return;
                            }
                        }
                        Ok(None) => break "the daemon closed the connection".to_owned(),
                        Err(error) => break error.to_string(),
                    }
                }
            }
            Err(error) => error.to_string(),
        };
        if !reported_down {
            reported_down = true;
            if tx.send_blocking(Update::NotRunning(outage)).is_err() {
                return;
            }
        }
        thread::sleep(RETRY_INTERVAL);
    }
}

/// One request on a fresh connection. Blocking: run it with `gio::spawn_blocking`.
///
/// # Errors
/// Returns user-facing text: a missing daemon, or the daemon's own message.
pub fn call(socket: &Path, request: Request) -> Result<Status, String> {
    let mut client = Client::connect(socket).map_err(describe)?;
    client.call(request).map_err(describe)
}

/// Asks for the latest live-source frame. Blocking: run it with `gio::spawn_blocking`.
///
/// # Errors
/// Returns user-facing text, e.g. when no live source is running.
pub fn preview(socket: &Path) -> Result<PreviewReply, String> {
    let mut client = Client::connect(socket).map_err(describe)?;
    client.preview().map_err(describe)
}

/// Fetches the saved profiles and the daemon status.
///
/// # Errors
/// Returns user-facing text, e.g. when the daemon is too old for profiles.
pub fn profiles(socket: &Path) -> Result<ProfilesReply, String> {
    let mut client = Client::connect(socket).map_err(describe)?;
    client.profiles().map_err(describe)
}

pub(crate) fn describe(error: ClientError) -> String {
    match error {
        ClientError::Connect { .. } => "tt500-lcdd is not running".to_owned(),
        ClientError::Daemon(error) => error.message,
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use tt500_lcd_ipc::{
        DeviceState, ErrorCode, Event, EventEnvelope, IpcError, Outcome, Response,
    };

    fn status(brightness: u8) -> Status {
        Status {
            daemon_version: "0".into(),
            ipc_version: 1,
            device: DeviceState::Idle,
            content: None,
            brightness,
            rotation: None,
            content_error: None,
            profile: None,
        }
    }

    #[test]
    fn watch_forwards_statuses_then_reports_the_outage_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("d.sock");
        let listener = UnixListener::bind(&socket).expect("bind");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(&stream)
                .read_line(&mut line)
                .expect("read request");
            let reply = Response {
                id: 1,
                outcome: Outcome::Result(status(10)),
            };
            let event = EventEnvelope {
                event: Event::Status(status(20)),
            };
            let mut stream = &stream;
            for text in [
                serde_json::to_string(&reply).expect("json"),
                serde_json::to_string(&event).expect("json"),
            ] {
                writeln!(stream, "{text}").expect("write");
            }
        });
        let rx = watch(socket);
        let first = rx.recv_blocking().expect("first");
        let second = rx.recv_blocking().expect("second");
        let third = rx.recv_blocking().expect("third");
        server.join().expect("server thread");
        assert!(matches!(first, Update::Status(s) if s.brightness == 10));
        assert!(matches!(second, Update::Status(s) if s.brightness == 20));
        assert!(matches!(third, Update::NotRunning(_)));
    }

    #[test]
    fn call_on_a_missing_socket_says_not_running() {
        let dir = tempfile::tempdir().expect("temp dir");
        let error = call(&dir.path().join("none.sock"), Request::Status).expect_err("no daemon");
        assert!(error.contains("not running"), "{error}");
    }

    #[test]
    fn preview_on_a_missing_socket_says_not_running() {
        let dir = tempfile::tempdir().expect("temp dir");
        let error = preview(&dir.path().join("none.sock")).expect_err("no daemon");
        assert!(error.contains("not running"), "{error}");
    }

    #[test]
    fn daemon_errors_show_their_message() {
        let error = IpcError {
            code: ErrorCode::BadRequest,
            message: "fps out of range".into(),
        };
        assert_eq!(describe(ClientError::Daemon(error)), "fps out of range");
    }
}
