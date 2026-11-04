//! Blocking client for the daemon socket.

use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::framing::{FramingError, read_message, write_message};
use crate::message::{
    DryRunReply, Event, EventEnvelope, IpcError, Outcome, PreviewReply, Request, RequestEnvelope,
    ServerMessage, Status, UploadBootAnimationParams, UploadStandbyParams,
};

/// Large GIFs are rendered before the daemon replies.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// A client call failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    /// The socket could not be reached (daemon not running?).
    #[error("cannot connect to {}: {source}", path.display())]
    Connect {
        /// Socket path tried.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
    /// Socket I/O failed.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    /// A message could not be framed or parsed.
    #[error("framing error: {0}")]
    Framing(#[from] FramingError),
    /// The daemon broke the protocol (id mismatch, early EOF).
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The daemon answered with an error.
    #[error("daemon error: {0}")]
    Daemon(#[from] IpcError),
}

fn unexpected_preview() -> ClientError {
    ClientError::Protocol("unexpected preview reply".into())
}

/// A connection to the daemon.
#[derive(Debug)]
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    buf: String,
}

impl Client {
    /// Connects to the daemon socket at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Connect`] if the socket cannot be opened.
    pub fn connect(path: &Path) -> Result<Self, ClientError> {
        let connect_err = |source| ClientError::Connect {
            path: path.to_path_buf(),
            source,
        };
        let writer = UnixStream::connect(path).map_err(connect_err)?;
        writer.set_read_timeout(Some(READ_TIMEOUT))?;
        let reader = BufReader::new(writer.try_clone()?);
        Ok(Self {
            reader,
            writer,
            next_id: 1,
            buf: String::new(),
        })
    }

    /// Sends `request` and waits for its reply, skipping pushed events.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Daemon`] for an error reply, or another variant on
    /// transport or protocol failure.
    pub fn call(&mut self, request: Request) -> Result<Status, ClientError> {
        match self.exchange(request)? {
            Outcome::Result(status) => Ok(status),
            Outcome::DryRun(reply) => Ok(reply.status),
            Outcome::Preview(_) => Err(unexpected_preview()),
            Outcome::Error(error) => Err(ClientError::Daemon(error)),
        }
    }

    /// Asks the daemon to upload a standby image. `Ok(None)` is a real upload (status only);
    /// `Ok(Some(_))` is a dry run with the files written.
    ///
    /// # Errors
    ///
    /// As [`Client::call`].
    pub fn upload_standby(
        &mut self,
        params: UploadStandbyParams,
    ) -> Result<(Status, Option<DryRunReply>), ClientError> {
        match self.exchange(Request::UploadStandby(params))? {
            Outcome::Result(status) => Ok((status, None)),
            Outcome::DryRun(reply) => Ok((reply.status.clone(), Some(reply))),
            Outcome::Preview(_) => Err(unexpected_preview()),
            Outcome::Error(error) => Err(ClientError::Daemon(error)),
        }
    }

    /// Asks the daemon to upload a boot animation. `Ok(None)` is a real upload (status only);
    /// `Ok(Some(_))` is a dry run with the files written.
    ///
    /// # Errors
    ///
    /// As [`Client::call`].
    pub fn upload_boot_animation(
        &mut self,
        params: UploadBootAnimationParams,
    ) -> Result<(Status, Option<DryRunReply>), ClientError> {
        match self.exchange(Request::UploadBootAnimation(params))? {
            Outcome::Result(status) => Ok((status, None)),
            Outcome::DryRun(reply) => Ok((reply.status.clone(), Some(reply))),
            Outcome::Preview(_) => Err(unexpected_preview()),
            Outcome::Error(error) => Err(ClientError::Daemon(error)),
        }
    }

    /// Fetches the latest live-source frame: the daemon writes a JPEG and replies with its
    /// path, the source size and a capture counter.
    ///
    /// # Errors
    ///
    /// As [`Client::call`]; `not_found` when no live source is running.
    pub fn preview(&mut self) -> Result<PreviewReply, ClientError> {
        match self.exchange(Request::Preview)? {
            Outcome::Preview(reply) => Ok(reply),
            Outcome::Error(error) => Err(ClientError::Daemon(error)),
            Outcome::Result(_) | Outcome::DryRun(_) => Err(ClientError::Protocol(
                "daemon answered a preview request with another reply".into(),
            )),
        }
    }

    fn exchange(&mut self, request: Request) -> Result<Outcome, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        write_message(&mut self.writer, &RequestEnvelope { id, request })?;
        loop {
            let message: Option<ServerMessage> = read_message(&mut self.reader, &mut self.buf)?;
            match message {
                None => return Err(ClientError::Protocol("daemon closed the connection".into())),
                Some(ServerMessage::Event(_)) => {}
                Some(ServerMessage::Response(response)) if response.id == id => {
                    return Ok(response.outcome);
                }
                Some(ServerMessage::Response(response)) => {
                    return Err(ClientError::Protocol(format!(
                        "reply id {} does not match request id {id}",
                        response.id
                    )));
                }
            }
        }
    }

    /// Turns this connection into a subscription: returns the current status and a
    /// [`Subscription`] that yields every later change.
    ///
    /// # Errors
    ///
    /// As [`Client::call`]; an old daemon answers with a `bad_request` error.
    pub fn subscribe(mut self) -> Result<(Status, Subscription), ClientError> {
        let status = self.call(Request::Subscribe)?;
        // Changes can be hours apart; the subscription blocks until one arrives.
        self.writer.set_read_timeout(None)?;
        Ok((
            status,
            Subscription {
                reader: self.reader,
                _writer: self.writer,
                buf: self.buf,
            },
        ))
    }
}

/// A connection that receives a [`Status`] each time the daemon's status changes.
#[derive(Debug)]
pub struct Subscription {
    reader: BufReader<UnixStream>,
    /// Kept open: closing it ends the subscription on the daemon side.
    _writer: UnixStream,
    buf: String,
}

impl Subscription {
    /// Blocks until the next status change; `Ok(None)` once the daemon closed the connection.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] on transport, framing or protocol failure.
    pub fn next_status(&mut self) -> Result<Option<Status>, ClientError> {
        let message: Option<ServerMessage> = read_message(&mut self.reader, &mut self.buf)?;
        match message {
            None => Ok(None),
            Some(ServerMessage::Event(EventEnvelope { event })) => match event {
                Event::Status(status) => Ok(Some(status)),
            },
            Some(ServerMessage::Response(response)) => Err(ClientError::Protocol(format!(
                "unexpected reply {} on a subscription",
                response.id
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{DeviceState, ErrorCode};
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixListener;
    use std::thread::JoinHandle;

    type Served = (tempfile::TempDir, PathBuf, JoinHandle<String>);

    /// Answers one connection with `reply`; the thread returns the request line.
    fn serve_once(reply: &'static str) -> Served {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            stream.write_all(reply.as_bytes()).unwrap();
            line
        });
        (dir, path, handle)
    }

    #[test]
    fn call_returns_status_and_skips_events() {
        let (_dir, path, handle) = serve_once(concat!(
            r#"{"event":{"type":"status","daemon_version":"0.1.0","ipc_version":1,"device":{"state":"idle"},"content":null,"brightness":1}}"#,
            "\n",
            r#"{"id":1,"result":{"daemon_version":"0.1.0","ipc_version":1,"device":{"state":"idle"},"content":null,"brightness":100}}"#,
            "\n"
        ));
        let mut client = Client::connect(&path).unwrap();
        let status = client.call(Request::Status).unwrap();
        assert_eq!(status.device, DeviceState::Idle);
        assert_eq!(status.brightness, 100);
        assert_eq!(handle.join().unwrap(), "{\"id\":1,\"method\":\"status\"}\n");
    }

    #[test]
    fn subscribe_yields_status_then_events() {
        let (_dir, path, handle) = serve_once(concat!(
            r#"{"id":1,"result":{"daemon_version":"0.1.0","ipc_version":1,"device":{"state":"idle"},"content":null,"brightness":100}}"#,
            "\n",
            r#"{"event":{"type":"status","daemon_version":"0.1.0","ipc_version":1,"device":{"state":"idle"},"content":null,"brightness":40}}"#,
            "\n"
        ));
        let (first, mut subscription) = Client::connect(&path).unwrap().subscribe().unwrap();
        assert_eq!(first.brightness, 100);
        let second = subscription.next_status().unwrap().unwrap();
        assert_eq!(second.brightness, 40);
        assert!(subscription.next_status().unwrap().is_none());
        assert_eq!(
            handle.join().unwrap(),
            "{\"id\":1,\"method\":\"subscribe\"}\n"
        );
    }

    #[test]
    fn daemon_error_is_surfaced() {
        let (_dir, path, handle) =
            serve_once("{\"id\":1,\"error\":{\"code\":\"bad_request\",\"message\":\"nope\"}}\n");
        let mut client = Client::connect(&path).unwrap();
        let err = client.call(Request::Clear).unwrap_err();
        assert!(matches!(&err, ClientError::Daemon(e) if e.code == ErrorCode::BadRequest));
        handle.join().unwrap();
    }

    #[test]
    fn id_mismatch_is_a_protocol_error() {
        let (_dir, path, handle) =
            serve_once("{\"id\":9,\"error\":{\"code\":\"internal\",\"message\":\"x\"}}\n");
        let mut client = Client::connect(&path).unwrap();
        let err = client.call(Request::Status).unwrap_err();
        assert!(matches!(err, ClientError::Protocol(_)));
        handle.join().unwrap();
    }

    #[test]
    fn connect_failure_names_the_path() {
        let err = Client::connect(Path::new("/nonexistent/dir/s.sock")).unwrap_err();
        assert!(matches!(err, ClientError::Connect { .. }));
    }
}
