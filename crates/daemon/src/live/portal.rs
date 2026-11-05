//! Window and screen capture through the `xdg-desktop-portal` ScreenCast interface.
//!
//! The daemon owns the portal session, so the stream keeps running after the GUI closes.
//! The portal hands over a PipeWire remote; a `gst-launch-1.0` child reads the node and
//! writes raw RGB frames to a pipe, which a reader thread turns into [`RgbImage`]s.
//! The portal and the frame feed sit behind small seams so tests need neither.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use image::RgbImage;
use zbus::blocking::{Connection, Proxy};
use zvariant::{DeserializeDict, ObjectPath, OwnedFd as BusFd, SerializeDict, Type, Value};

use super::capture::{CaptureError, Capturer};
use super::vdesktop::{StderrTail, guarded_command, in_path, log_output, stop_group};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SESSION: &str = "org.freedesktop.portal.Session";

/// Source types of the portal's bit mask: monitors and windows.
const SOURCE_MONITOR_WINDOW: u32 = 1 | 2;
const CURSOR_HIDDEN: u32 = 1;
const CURSOR_EMBEDDED: u32 = 2;
/// The first ScreenCast version that stores restore tokens.
const PERSIST_VERSION: u32 = 4;
/// "Remember until revoked".
const PERSIST_UNTIL_REVOKED: u32 = 2;
/// How long the portal answers `CreateSession` and `SelectSources`.
const QUICK_REPLY: Duration = Duration::from_secs(10);
/// How long the user gets to choose a source. Below the IPC client's 30 s read timeout.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(25);
/// The biggest frame the daemon captures; larger sources are scaled down to fit.
const MAX_CAPTURE: (u32, u32) = (1920, 1080);
/// Size assumed when the portal does not report the stream's size.
const FALLBACK_SIZE: (u32, u32) = (1920, 1080);
/// How long `capture` waits for the very first frame.
const FIRST_FRAME_WAIT: Duration = Duration::from_secs(20);
/// How long `capture` waits for a new frame before repeating the last one.
const FRAME_WAIT: Duration = Duration::from_millis(250);
/// How long the gst child gets to exit after SIGTERM.
const GST_GRACE: Duration = Duration::from_millis(500);
/// File name of the stored restore token inside the state dir.
pub const TOKEN_FILE: &str = "portal-token";

/// The result of choosing a source in the portal.
pub struct Selection {
    /// The PipeWire remote.
    pub fd: OwnedFd,
    /// The node of the chosen stream.
    pub node: u32,
    /// The stream's size, when the portal reports it.
    pub size: Option<(u32, u32)>,
    /// The token that skips the picker next time.
    pub restore_token: Option<String>,
    /// Closes the portal session when dropped.
    pub session: Box<dyn Send>,
}

/// Asks the user (or the stored token) for a screen or window.
pub trait Portal {
    /// Opens a session and returns the chosen stream.
    ///
    /// # Errors
    ///
    /// [`CaptureError::Failed`] with a message the user can act on.
    fn open(&mut self, restore_token: Option<&str>) -> Result<Selection, CaptureError>;
}

/// Where the restore token is kept between daemon runs.
#[derive(Debug, Clone)]
pub struct TokenFile(PathBuf);

impl TokenFile {
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        Self(state_dir.join(TOKEN_FILE))
    }

    fn load(&self) -> Option<String> {
        let token = fs::read_to_string(&self.0).ok()?;
        let token = token.trim();
        (!token.is_empty()).then(|| token.to_owned())
    }

    fn store(&self, token: &str) {
        if let Err(err) = fs::write(&self.0, token) {
            tracing::warn!("cannot save the portal restore token: {err}");
        }
    }

    fn forget(&self) {
        // A missing file is the normal case.
        let _ = fs::remove_file(&self.0);
    }
}

/// Picks the capture size: the source size scaled down to fit [`MAX_CAPTURE`] (never up),
/// keeping its aspect, width a multiple of 4 (RGB rows are padded to 4 bytes) and height even.
#[must_use]
pub fn fit_size(source: (u32, u32)) -> (u32, u32) {
    let (w, h) = (source.0.max(1), source.1.max(1));
    let (max_w, max_h) = MAX_CAPTURE;
    let (cw, ch) = if w <= max_w && h <= max_h {
        (w, h)
    } else if u64::from(w) * u64::from(max_h) >= u64::from(h) * u64::from(max_w) {
        (
            max_w,
            (u64::from(h) * u64::from(max_w) / u64::from(w)) as u32,
        )
    } else {
        (
            (u64::from(w) * u64::from(max_h) / u64::from(h)) as u32,
            max_h,
        )
    };
    ((cw & !3).max(4), (ch & !1).max(2))
}

/// The `gst-launch-1.0` arguments that write `width`x`height` RGB frames to stdout from the
/// PipeWire remote on stdin.
#[must_use]
pub fn gst_args(node: u32, size: (u32, u32), fps: u32) -> Vec<String> {
    let (width, height) = size;
    [
        "-q".to_owned(),
        "pipewiresrc".to_owned(),
        "fd=0".to_owned(),
        format!("path={node}"),
        "always-copy=true".to_owned(),
        "!".to_owned(),
        "videoconvert".to_owned(),
        "!".to_owned(),
        "videoscale".to_owned(),
        "add-borders=true".to_owned(),
        "!".to_owned(),
        format!("video/x-raw,format=RGB,width={width},height={height}"),
        "!".to_owned(),
        "videorate".to_owned(),
        "drop-only=true".to_owned(),
        "!".to_owned(),
        format!("video/x-raw,framerate={}/1", fps.max(1)),
        "!".to_owned(),
        "fdsink".to_owned(),
        "fd=1".to_owned(),
    ]
    .into()
}

/// `CreateSession`'s results.
#[derive(Debug, DeserializeDict, SerializeDict, Type)]
#[zvariant(signature = "a{sv}")]
struct SessionResults {
    session_handle: Option<String>,
}

/// One stream's properties in `Start`'s results.
#[derive(Debug, DeserializeDict, SerializeDict, Type)]
#[zvariant(signature = "a{sv}")]
struct StreamProps {
    size: Option<(i32, i32)>,
}

/// `Start`'s results.
#[derive(Debug, DeserializeDict, SerializeDict, Type)]
#[zvariant(signature = "a{sv}")]
struct StartResults {
    streams: Option<Vec<(u32, StreamProps)>>,
    restore_token: Option<String>,
}

/// The stream a `Start` response chose.
#[derive(Debug)]
struct Started {
    node: u32,
    size: Option<(u32, u32)>,
    restore_token: Option<String>,
}

/// The first stream's node, size and the new restore token from a `Start` response.
fn parse_start(code: u32, results: StartResults) -> Result<Started, CaptureError> {
    match code {
        0 => {}
        1 => {
            return Err(CaptureError::Failed(
                "the selection was cancelled".to_owned(),
            ));
        }
        other => {
            return Err(CaptureError::Failed(format!(
                "the portal refused the capture (code {other})"
            )));
        }
    }
    let (node, props) = results
        .streams
        .and_then(|streams| streams.into_iter().next())
        .ok_or_else(|| CaptureError::Failed("the portal returned no stream".to_owned()))?;
    let size = props
        .size
        .and_then(|(w, h)| Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?)))
        .filter(|&(w, h)| w > 0 && h > 0);
    Ok(Started {
        node,
        size,
        restore_token: results.restore_token,
    })
}

/// The cursor mode to ask for: embedded when offered, else hidden.
fn cursor_mode(available: u32) -> u32 {
    if available & CURSOR_EMBEDDED != 0 {
        CURSOR_EMBEDDED
    } else {
        CURSOR_HIDDEN
    }
}

fn failed(what: &str, err: impl std::fmt::Display) -> CaptureError {
    CaptureError::Failed(format!("{what}: {err}"))
}

/// Closes a portal session when dropped.
struct BusSession {
    connection: Connection,
    path: String,
}

impl Drop for BusSession {
    fn drop(&mut self) {
        let closed = Proxy::new(&self.connection, DESTINATION, self.path.as_str(), SESSION)
            .and_then(|proxy| proxy.call_method("Close", &()));
        if let Err(err) = closed {
            tracing::debug!("closing the portal session: {err}");
        }
    }
}

/// The real portal on the session bus.
pub struct BusPortal {
    connection: Connection,
}

impl BusPortal {
    /// Connects to the session bus.
    ///
    /// # Errors
    ///
    /// [`CaptureError::Failed`] when there is no session bus.
    pub fn connect() -> Result<Self, CaptureError> {
        let connection = Connection::session().map_err(|err| failed("no session D-Bus", err))?;
        Ok(Self { connection })
    }

    fn screencast(&self) -> Result<Proxy<'static>, CaptureError> {
        Proxy::new(&self.connection, DESTINATION, PORTAL_PATH, SCREENCAST)
            .map_err(|err| failed("the ScreenCast portal is unavailable", err))
    }

    /// Calls a request-style method and waits for its `Response` signal, subscribed to
    /// before the call so it cannot be missed.
    fn request<B, R>(
        &self,
        method: &str,
        token: &str,
        body: &B,
        timeout: Duration,
    ) -> Result<(u32, R), CaptureError>
    where
        B: serde::Serialize + zvariant::DynamicType,
        R: serde::de::DeserializeOwned + Type,
    {
        let sender = self
            .connection
            .unique_name()
            .ok_or_else(|| CaptureError::Failed("no D-Bus name".to_owned()))?
            .trim_start_matches(':')
            .replace('.', "_");
        let path = format!("{PORTAL_PATH}/request/{sender}/{token}");
        let request = Proxy::new(&self.connection, DESTINATION, path, REQUEST)
            .map_err(|err| failed("portal request", err))?;
        let mut signals = request
            .receive_signal("Response")
            .map_err(|err| failed("portal request", err))?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            if let Some(message) = signals.next() {
                let _ = tx.send(message);
            }
        });
        self.screencast()?
            .call_method(method, body)
            .map_err(|err| failed(&format!("portal {method}"), err))?;
        let message = rx.recv_timeout(timeout).map_err(|_| {
            CaptureError::Failed(format!("nothing was chosen within {} s", timeout.as_secs()))
        })?;
        message
            .body()
            .deserialize::<(u32, R)>()
            .map_err(|err| failed("portal response", err))
    }
}

fn handle_token(purpose: &str) -> String {
    format!("tt500_{purpose}_{}", std::process::id())
}

impl Portal for BusPortal {
    fn open(&mut self, restore_token: Option<&str>) -> Result<Selection, CaptureError> {
        let screencast = self.screencast()?;
        let version: u32 = screencast.get_property("version").unwrap_or(1);
        let cursors: u32 = screencast.get_property("AvailableCursorModes").unwrap_or(0);

        let token = handle_token("create");
        let session_token = handle_token("session");
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("handle_token", Value::from(token.as_str()));
        options.insert("session_handle_token", Value::from(session_token.as_str()));
        let (code, created): (u32, SessionResults) =
            self.request("CreateSession", &token, &(options,), QUICK_REPLY)?;
        let session_path = match (code, created.session_handle) {
            (0, Some(path)) => path,
            (code, _) => {
                return Err(CaptureError::Failed(format!(
                    "the portal refused a session (code {code})"
                )));
            }
        };
        let session = BusSession {
            connection: self.connection.clone(),
            path: session_path.clone(),
        };
        let session_object = ObjectPath::try_from(session_path.as_str())
            .map_err(|err| failed("session path", err))?;

        let token = handle_token("select");
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("handle_token", Value::from(token.as_str()));
        options.insert("types", Value::from(SOURCE_MONITOR_WINDOW));
        options.insert("multiple", Value::from(false));
        options.insert("cursor_mode", Value::from(cursor_mode(cursors)));
        if version >= PERSIST_VERSION {
            options.insert("persist_mode", Value::from(PERSIST_UNTIL_REVOKED));
            if let Some(stored) = restore_token {
                options.insert("restore_token", Value::from(stored));
            }
        }
        let (code, _): (u32, HashMap<String, zvariant::OwnedValue>) = self.request(
            "SelectSources",
            &token,
            &(&session_object, options),
            QUICK_REPLY,
        )?;
        if code != 0 {
            return Err(CaptureError::Failed(format!(
                "the portal refused the source types (code {code})"
            )));
        }

        let token = handle_token("start");
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("handle_token", Value::from(token.as_str()));
        let (code, results): (u32, StartResults) = self.request(
            "Start",
            &token,
            &(&session_object, "", options),
            PROMPT_TIMEOUT,
        )?;
        let Started {
            node,
            size,
            restore_token,
        } = parse_start(code, results)?;

        let reply = screencast
            .call_method(
                "OpenPipeWireRemote",
                &(&session_object, HashMap::<&str, Value<'_>>::new()),
            )
            .map_err(|err| failed("OpenPipeWireRemote", err))?;
        let fd: BusFd = reply
            .body()
            .deserialize()
            .map_err(|err| failed("OpenPipeWireRemote reply", err))?;
        Ok(Selection {
            fd: fd.into(),
            node,
            size,
            restore_token,
            session: Box::new(session),
        })
    }
}

/// A running frame source: the pipe of raw frames and what to stop with it.
pub struct Feed {
    pub reader: Box<dyn Read + Send>,
    /// Dropped before the portal session: stops the gst child.
    pub guard: Box<dyn Send>,
    /// The child's last stderr line, for the message when it ends.
    pub stderr: Option<Arc<StderrTail>>,
}

/// The gst child; stopped (group SIGTERM, then SIGKILL) when dropped.
struct GstChild(Child);

impl Drop for GstChild {
    fn drop(&mut self) {
        stop_group(&mut self.0, GST_GRACE);
    }
}

/// Whether `gst-launch-1.0` and its `pipewiresrc` plugin are installed.
#[must_use]
pub fn gstreamer_available() -> bool {
    in_path("gst-launch-1.0")
        && Command::new("gst-inspect-1.0")
            .args(["--exists", "pipewiresrc"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

/// Starts the gst child on the portal's PipeWire remote.
///
/// # Errors
///
/// [`CaptureError::Failed`] when the child cannot be spawned.
pub fn spawn_gst(fd: OwnedFd, node: u32, size: (u32, u32), fps: u32) -> Result<Feed, CaptureError> {
    let mut command = guarded_command("gst-launch-1.0");
    command
        .args(gst_args(node, size, fps))
        .stdin(Stdio::from(fd))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|err| failed("cannot start gst-launch-1.0", err))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let guard = GstChild(child);
    let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
        return Err(CaptureError::Failed(
            "gst-launch-1.0 has no pipes".to_owned(),
        ));
    };
    let tail = Arc::new(StderrTail::default());
    log_output(stderr, "gst".to_owned(), Some(Arc::clone(&tail)));
    Ok(Feed {
        reader: Box::new(stdout),
        guard: Box::new(guard),
        stderr: Some(tail),
    })
}

/// Opens the portal (the stored token first unless `reselect`), starts the feed with
/// `start_feed` and saves the new token.
///
/// # Errors
///
/// [`CaptureError::Failed`] when the portal or the feed fails.
pub fn connect(
    portal: &mut dyn Portal,
    tokens: &TokenFile,
    reselect: bool,
    start_feed: impl FnOnce(OwnedFd, u32, (u32, u32)) -> Result<Feed, CaptureError>,
) -> Result<PortalCapturer, CaptureError> {
    if reselect {
        tokens.forget();
    }
    let stored = if reselect { None } else { tokens.load() };
    let selection = portal.open(stored.as_deref())?;
    if let Some(token) = &selection.restore_token {
        tokens.store(token);
    }
    let size = fit_size(selection.size.unwrap_or(FALLBACK_SIZE));
    let feed = start_feed(selection.fd, selection.node, size)?;
    Ok(PortalCapturer::new(feed, size, selection.session))
}

/// The newest frame and whether the feed ended.
#[derive(Default)]
struct Slot {
    frame: Option<Arc<RgbImage>>,
    seq: u64,
    ended: Option<String>,
}

type Shared = (Mutex<Slot>, Condvar);

/// Frames from a portal stream.
pub struct PortalCapturer {
    shared: Arc<Shared>,
    seen: u64,
    reader: Option<JoinHandle<()>>,
    /// Stops the child first, then closes the session (field order).
    _guard: Box<dyn Send>,
    _session: Box<dyn Send>,
}

impl PortalCapturer {
    /// Reads `size` RGB frames from the feed on a thread.
    #[must_use]
    pub fn new(feed: Feed, size: (u32, u32), session: Box<dyn Send>) -> Self {
        let shared = Arc::new(Shared::default());
        let Feed {
            mut reader,
            guard,
            stderr,
        } = feed;
        let slot = Arc::clone(&shared);
        let len = size.0 as usize * size.1 as usize * 3;
        let handle = thread::spawn(move || {
            let reason = loop {
                let mut buf = vec![0_u8; len];
                if let Err(err) = reader.read_exact(&mut buf) {
                    let detail = stderr.as_ref().and_then(|tail| tail.get());
                    break detail.unwrap_or_else(|| format!("the stream ended ({err})"));
                }
                let Some(img) = RgbImage::from_raw(size.0, size.1, buf) else {
                    break "a frame had the wrong size".to_owned();
                };
                let (lock, wake) = &*slot;
                let mut state = lock.lock().expect("lock poisoned");
                state.frame = Some(Arc::new(img));
                state.seq += 1;
                wake.notify_all();
            };
            let (lock, wake) = &*slot;
            lock.lock().expect("lock poisoned").ended = Some(reason);
            wake.notify_all();
        });
        Self {
            shared,
            seen: 0,
            reader: Some(handle),
            _guard: guard,
            _session: session,
        }
    }
}

impl Capturer for PortalCapturer {
    fn capture(&mut self) -> Result<RgbImage, CaptureError> {
        let (lock, wake) = &*self.shared;
        let started = Instant::now();
        let mut state = lock.lock().expect("lock poisoned");
        loop {
            if state.seq > self.seen
                && let Some(frame) = &state.frame
            {
                self.seen = state.seq;
                return Ok(RgbImage::clone(frame));
            }
            if let Some(reason) = &state.ended {
                return Err(CaptureError::Gone(reason.clone()));
            }
            let waited = started.elapsed();
            // A still window sends no frames: repeat the last one.
            if let Some(frame) = &state.frame
                && waited >= FRAME_WAIT
            {
                return Ok(RgbImage::clone(frame));
            }
            if state.frame.is_none() && waited >= FIRST_FRAME_WAIT {
                return Err(CaptureError::Failed(
                    "the stream sent no frame yet".to_owned(),
                ));
            }
            state = wake
                .wait_timeout(state, FRAME_WAIT)
                .expect("lock poisoned")
                .0;
        }
    }
}

impl Drop for PortalCapturer {
    fn drop(&mut self) {
        // The child is stopped by `_guard` after this; its end of the pipe closing ends the
        // reader, so only join when it already finished.
        if let Some(handle) = self.reader.take() {
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Cursor;

    use super::*;

    struct FakePortal {
        tokens_seen: Vec<Option<String>>,
        reply: Option<Result<(), String>>,
    }

    impl Portal for FakePortal {
        fn open(&mut self, restore_token: Option<&str>) -> Result<Selection, CaptureError> {
            self.tokens_seen.push(restore_token.map(str::to_owned));
            if let Some(Err(message)) = self.reply.take() {
                return Err(CaptureError::Failed(message));
            }
            Ok(Selection {
                fd: File::open("/dev/null").expect("dev null").into(),
                node: 42,
                size: Some((3840, 2160)),
                restore_token: Some("fresh".to_owned()),
                session: Box::new(()),
            })
        }
    }

    fn fake_feed(frames: usize, size: (u32, u32)) -> Feed {
        let bytes = vec![7_u8; frames * size.0 as usize * size.1 as usize * 3];
        Feed {
            reader: Box::new(Cursor::new(bytes)),
            guard: Box::new(()),
            stderr: None,
        }
    }

    #[test]
    fn fits_sources_into_the_capture_box() {
        assert_eq!(fit_size((3840, 2160)), (1920, 1080));
        assert_eq!(fit_size((1280, 720)), (1280, 720));
        assert_eq!(fit_size((1001, 601)), (1000, 600));
        // Tall windows are limited by height; width stays a multiple of 4.
        assert_eq!(fit_size((1000, 4000)), (268, 1080));
        assert_eq!(fit_size((0, 0)), (4, 2));
    }

    #[test]
    fn gst_args_name_the_node_and_size() {
        let args = gst_args(77, (1920, 1080), 15);
        assert!(args.contains(&"path=77".to_owned()));
        assert!(args.contains(&"fd=0".to_owned()));
        assert!(args.contains(&"video/x-raw,format=RGB,width=1920,height=1080".to_owned()));
        assert!(args.contains(&"video/x-raw,framerate=15/1".to_owned()));
    }

    #[test]
    fn start_results_round_trip_through_dbus_encoding() {
        let sent = StartResults {
            streams: Some(vec![(
                9,
                StreamProps {
                    size: Some((800, 600)),
                },
            )]),
            restore_token: Some("tok".to_owned()),
        };
        let ctx = zvariant::serialized::Context::new_dbus(zvariant::LE, 0);
        let bytes = zvariant::to_bytes(ctx, &sent).expect("encodes");
        let (got, _): (StartResults, _) = bytes.deserialize().expect("decodes");
        let started = parse_start(0, got).expect("parses");
        assert_eq!(started.node, 9);
        assert_eq!(started.size, Some((800, 600)));
        assert_eq!(started.restore_token.as_deref(), Some("tok"));
    }

    #[test]
    fn start_response_codes_map_to_errors() {
        let empty = || StartResults {
            streams: None,
            restore_token: None,
        };
        let cancelled = parse_start(1, empty()).expect_err("cancelled");
        assert!(cancelled.to_string().contains("cancelled"));
        assert!(parse_start(2, empty()).is_err());
        assert!(parse_start(0, empty()).is_err());
    }

    #[test]
    fn cursor_mode_prefers_embedded() {
        assert_eq!(cursor_mode(3), CURSOR_EMBEDDED);
        assert_eq!(cursor_mode(1), CURSOR_HIDDEN);
        assert_eq!(cursor_mode(0), CURSOR_HIDDEN);
    }

    #[test]
    fn connect_reuses_and_saves_the_token_and_reselect_drops_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tokens = TokenFile::new(tmp.path());
        let mut portal = FakePortal {
            tokens_seen: Vec::new(),
            reply: None,
        };
        let start = |_fd, node, size| {
            assert_eq!(node, 42);
            assert_eq!(size, (1920, 1080));
            Ok(fake_feed(1, size))
        };
        drop(connect(&mut portal, &tokens, false, start).expect("first"));
        drop(connect(&mut portal, &tokens, false, start).expect("second"));
        drop(connect(&mut portal, &tokens, true, start).expect("third"));
        assert_eq!(
            portal.tokens_seen,
            vec![None, Some("fresh".to_owned()), None]
        );
        assert_eq!(tokens.load().as_deref(), Some("fresh"));
    }

    #[test]
    fn a_cancelled_selection_is_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut portal = FakePortal {
            tokens_seen: Vec::new(),
            reply: Some(Err("the selection was cancelled".to_owned())),
        };
        let result = connect(
            &mut portal,
            &TokenFile::new(tmp.path()),
            false,
            |_, _, size| Ok(fake_feed(0, size)),
        );
        assert!(result.is_err());
    }

    #[test]
    fn frames_arrive_and_the_end_is_gone() {
        let size = (8, 4);
        let mut capturer = PortalCapturer::new(fake_feed(3, size), size, Box::new(()));
        let img = capturer.capture().expect("a frame");
        assert_eq!(img.dimensions(), size);
        assert_eq!(img.get_pixel(0, 0).0, [7, 7, 7]);
        let mut result = capturer.capture();
        // The last frame may be repeated once before the end is noticed.
        for _ in 0..3 {
            if result.is_err() {
                break;
            }
            result = capturer.capture();
        }
        assert!(matches!(result, Err(CaptureError::Gone(_))));
    }
}
