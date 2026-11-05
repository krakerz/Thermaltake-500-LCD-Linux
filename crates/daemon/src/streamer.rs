//! The streamer: the only code that touches the device.
//!
//! [`Core`] is a deadline scheduler driven by explicit `Instant`s so it can be tested
//! without sleeping; [`run`] is the thread loop around it.

use std::fmt;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use tt500_lcd_ipc::{ContentInfo, DeviceState, ErrorCode, IPC_VERSION, IpcError, Status};
use tt500_lcd_protocol::{
    BootAnimation, Brightness, Discovery, FeatureCommand, FeatureQuery, FeatureReply, Frame,
    HidTransport, KEEP_ALIVE_INTERVAL, OpenError, OutputPacket, Panel, Rotation, STILL_REFRESH,
    SessionError, StandbyImage, StatusReply, Transport, TransportError,
};
use tt500_lcd_render::{Playlist, TimedFrame, VideoPoll, VideoStream};

use crate::hub::StatusHub;
use crate::live::LiveStream;

/// How often to look for the panel while it is absent.
pub const SCAN_INTERVAL: Duration = Duration::from_secs(2);

/// Delay before an idle brightness change opens the panel; changes inside it are coalesced.
pub const IDLE_BRIGHTNESS_DELAY: Duration = Duration::from_millis(200);

/// Status shared with the IPC threads.
pub type SharedStatus = Arc<StatusHub>;

/// Opens a transport to the panel.
pub trait Connector {
    /// The transport this connector yields.
    type Transport: Transport;

    /// Tries to open the panel once.
    fn connect(&mut self) -> Result<Self::Transport, OpenError>;
}

/// Opens the real panel; hidapi is initialised on first use and kept.
#[derive(Default)]
pub struct HidConnector {
    discovery: Option<Discovery>,
}

impl Connector for HidConnector {
    type Transport = HidTransport;

    fn connect(&mut self) -> Result<HidTransport, OpenError> {
        if self.discovery.is_none() {
            self.discovery = Some(Discovery::new()?);
        }
        match self.discovery.as_mut() {
            Some(discovery) => discovery.open(),
            None => Err(OpenError::NotFound),
        }
    }
}

// Captured replies, as in the protocol crate's mock (not available in normal builds).
const FIRMWARE: [u8; 13] = [
    0x0a, 0x0c, 0x8c, 0x17, 0x9d, b'2', b'1', b'.', b'0', b'.', b'5', b'.', b'4',
];
const GEOMETRY: [u8; 13] = [0x07, 0, 0, 0, 0, 0, 0, 0xe0, 0x01, 0x80, 0x00, 0x04, 0x05];
const STATUS: [u8; 9] = [0x0f, 0xe2, 0xff, 0xff, 0xff, 0xea, 0xff, 0xff, 0xff];
const REPORT10: [u8; 2] = [0x10, 0x05];

/// A transport that answers like the panel and discards everything (`--no-device`).
#[derive(Debug, Default)]
pub struct NullTransport {
    packets: u64,
    /// Block number of the last boot-animation packet; GET 0x0d reports it as stored.
    boot_block: u8,
}

impl Transport for NullTransport {
    fn set_feature(&mut self, _command: &FeatureCommand) -> Result<(), TransportError> {
        Ok(())
    }

    fn get_feature(&mut self, query: FeatureQuery) -> Result<FeatureReply, TransportError> {
        let reply: &[u8] = match query {
            FeatureQuery::Firmware => &FIRMWARE,
            FeatureQuery::Geometry => &GEOMETRY,
            FeatureQuery::Status => &STATUS,
            FeatureQuery::Report10 => &REPORT10,
            FeatureQuery::UploadProgress => {
                return Ok(FeatureReply::new(&[0x0d, self.boot_block]));
            }
        };
        Ok(FeatureReply::new(reply))
    }

    fn write_output(&mut self, packet: &OutputPacket) -> Result<(), TransportError> {
        self.packets += 1;
        if packet.as_bytes()[1] == 0x0c {
            self.boot_block = packet.as_bytes()[2];
        }
        tracing::trace!(total = self.packets, "null transport: output packet");
        Ok(())
    }
}

/// Connector for `--no-device`.
#[derive(Debug, Default)]
pub struct NullConnector;

impl Connector for NullConnector {
    type Transport = NullTransport;

    fn connect(&mut self) -> Result<NullTransport, OpenError> {
        Ok(NullTransport::default())
    }
}

/// Logs every report at debug level, then forwards it.
#[derive(Debug)]
pub struct Logged<T>(T);

impl<T: Transport> Transport for Logged<T> {
    fn set_feature(&mut self, command: &FeatureCommand) -> Result<(), TransportError> {
        tracing::debug!("SET {command:?}");
        self.0.set_feature(command)
    }

    fn get_feature(&mut self, query: FeatureQuery) -> Result<FeatureReply, TransportError> {
        tracing::debug!("GET 0x{:02x}", query.report_id());
        self.0.get_feature(query)
    }

    fn write_output(&mut self, packet: &OutputPacket) -> Result<(), TransportError> {
        let bytes = packet.as_bytes();
        if bytes[1] == 0x09 && bytes[6] == 0 && bytes[7] == 0 {
            tracing::debug!("frame BB=0x{:02x}", bytes[2]);
        }
        self.0.write_output(packet)
    }
}

/// What `show` loaded.
#[derive(Debug)]
pub enum Content {
    /// A still or a pre-rendered animation.
    Playlist(Playlist),
    /// A video decoded live; dropping it stops ffmpeg.
    Video(VideoStream),
    /// A live capture; dropping it stops the capture (and the virtual desktop).
    Live(LiveStream),
}

/// A source of frames produced while streaming: a video decoder or a live capture.
pub trait FrameStream: Send + fmt::Debug {
    /// The next frame if it is ready; never blocks.
    fn poll(&self) -> VideoPoll;
    /// The first frame, available as soon as the stream exists.
    fn first_frame(&self) -> &Frame;
    /// Time between frames.
    fn frame_interval(&self) -> Duration;
    /// Something the user should fix while the stream keeps running (live sources only).
    fn problem(&self) -> Option<String> {
        None
    }
    /// Whether a frame identical to the last one sent may be skipped (video files only).
    fn skips_repeats(&self) -> bool {
        false
    }
}

impl FrameStream for VideoStream {
    fn poll(&self) -> VideoPoll {
        Self::poll(self)
    }

    fn first_frame(&self) -> &Frame {
        Self::first_frame(self)
    }

    fn frame_interval(&self) -> Duration {
        Self::frame_interval(self)
    }

    fn skips_repeats(&self) -> bool {
        true
    }
}

/// A message from the IPC threads to the streamer.
#[derive(Debug)]
pub enum Command {
    /// Show new content.
    Show {
        content: Content,
        info: ContentInfo,
        reply: Sender<Status>,
    },
    /// Stop streaming and close the device.
    Clear { reply: Sender<Status> },
    /// Change the brightness (already persisted by the caller).
    SetBrightness {
        value: Brightness,
        reply: Sender<Status>,
    },
    /// Change the rotation step (already persisted by the caller).
    SetRotation {
        value: Rotation,
        reply: Sender<Status>,
    },
    /// Upload a standby image to the panel's flash (already rendered and validated).
    UploadStandby {
        image: StandbyImage,
        reply: Sender<Result<Status, IpcError>>,
    },
    /// Upload a boot animation to the panel's flash (already rendered and validated).
    UploadBootAnimation {
        animation: BootAnimation,
        reply: Sender<Result<Status, IpcError>>,
    },
    /// Replace the loaded content's description; the source itself is untouched.
    UpdateInfo {
        info: ContentInfo,
        reply: Sender<Status>,
    },
    /// Restoring the stored content failed at start-up; reported until the next show or clear.
    RestoreFailed { message: String },
}

/// Where frames come from: a playlist, or a stream (video decoder or live capture).
#[derive(Debug)]
enum Source {
    Playlist {
        playlist: Playlist,
        index: usize,
    },
    /// `last` is re-sent while the decoder lags, and as a still once it has ended.
    Video {
        stream: Box<dyn FrameStream>,
        last: Frame,
        ended: bool,
    },
}

impl From<Content> for Source {
    fn from(content: Content) -> Self {
        match content {
            Content::Playlist(playlist) => Self::Playlist { playlist, index: 0 },
            Content::Video(stream) => Self::stream(Box::new(stream)),
            Content::Live(stream) => Self::stream(Box::new(stream)),
        }
    }
}

impl Source {
    fn stream(stream: Box<dyn FrameStream>) -> Self {
        Self::Video {
            last: stream.first_frame().clone(),
            stream,
            ended: false,
        }
    }

    /// Whether an unchanged frame may be skipped: video files, not stills or live captures.
    fn skips_repeats(&self) -> bool {
        matches!(self, Self::Video { stream, .. } if stream.skips_repeats())
    }

    /// The running stream's problem, if it reports one.
    fn problem(&self) -> Option<String> {
        match self {
            Self::Video { stream, .. } => stream.problem(),
            Self::Playlist { .. } => None,
        }
    }

    /// The next frame and how long to hold it before asking again.
    fn next_frame(&mut self) -> TimedFrame {
        match self {
            Self::Playlist {
                playlist: Playlist::Still(frame),
                ..
            } => TimedFrame {
                frame: frame.clone(),
                hold: STILL_REFRESH,
            },
            Self::Playlist {
                playlist: Playlist::Animation(frames),
                index,
            } => {
                let timed = frames[*index % frames.len()].clone();
                *index = (*index + 1) % frames.len();
                timed
            }
            Self::Video {
                stream,
                last,
                ended,
            } => {
                if !*ended {
                    match stream.poll() {
                        VideoPoll::Frame(frame) => *last = frame,
                        VideoPoll::Pending => {}
                        VideoPoll::Ended(reason) => {
                            tracing::warn!("video stopped, keeping its last frame: {reason}");
                            *ended = true;
                        }
                    }
                }
                TimedFrame {
                    frame: last.clone(),
                    hold: if *ended {
                        STILL_REFRESH
                    } else {
                        stream.frame_interval()
                    },
                }
            }
        }
    }
}

#[derive(Debug)]
struct Loaded {
    source: Source,
    info: ContentInfo,
    /// The last frame sent and when; a stream's identical repeats are skipped until
    /// `STILL_REFRESH` has passed.
    last_sent: Option<(Frame, Instant)>,
}

/// Why opening the panel failed.
enum ConnectError {
    /// Wrong geometry: never retry at this device.
    Blocked(String),
    /// Anything else: scan again later.
    Retry(String),
}

/// Scheduler state: the open panel, the content and all deadlines.
pub struct Core<C: Connector> {
    connector: C,
    panel: Option<Panel<Logged<C::Transport>>>,
    content: Option<Loaded>,
    brightness: Brightness,
    rotation: Rotation,
    /// Idle only: when to open the panel briefly to apply the brightness and rotation.
    idle_brightness_due: Option<Instant>,
    device: DeviceState,
    next_frame: Instant,
    next_keep_alive: Instant,
    next_scan: Instant,
    last_keep_alive: Option<StatusReply>,
    blocked: Option<String>,
    last_error: Option<String>,
    content_error: Option<String>,
    /// The live source's own problem (a failing desktop command); shown unless
    /// `content_error` is set.
    live_problem: Option<String>,
    status: SharedStatus,
}

impl<C: Connector> Core<C> {
    /// Creates an idle core and publishes its status.
    pub fn new(
        connector: C,
        brightness: Brightness,
        rotation: Rotation,
        status: SharedStatus,
    ) -> Self {
        let now = Instant::now();
        let core = Self {
            connector,
            panel: None,
            content: None,
            brightness,
            rotation,
            idle_brightness_due: None,
            device: DeviceState::Idle,
            next_frame: now,
            next_keep_alive: now,
            next_scan: now,
            last_keep_alive: None,
            blocked: None,
            last_error: None,
            content_error: None,
            live_problem: None,
            status,
        };
        core.publish();
        core
    }

    fn publish(&self) {
        let status = Status {
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            ipc_version: IPC_VERSION,
            device: self.device.clone(),
            content: self.content.as_ref().map(|c| c.info.clone()),
            brightness: self.brightness.get(),
            rotation: Some(self.rotation.get()),
            content_error: self
                .content_error
                .clone()
                .or_else(|| self.live_problem.clone()),
            profile: None,
        };
        self.status.set(status);
    }

    fn snapshot(&self) -> Status {
        self.status.get()
    }

    /// Applies a command and answers it with the new status.
    pub fn handle(&mut self, cmd: Command, now: Instant) {
        let reply = match cmd {
            Command::Show {
                content,
                info,
                reply,
            } => {
                self.content = Some(Loaded {
                    source: Source::from(content),
                    info,
                    last_sent: None,
                });
                self.blocked = None;
                self.content_error = None;
                self.live_problem = None;
                self.next_scan = now;
                self.next_frame = now;
                self.idle_brightness_due = None;
                if self.panel.is_none() {
                    self.device = DeviceState::Searching { last_error: None };
                }
                Some(reply)
            }
            Command::Clear { reply } => {
                self.content = None;
                self.blocked = None;
                self.content_error = None;
                self.live_problem = None;
                self.panel = None;
                self.device = DeviceState::Idle;
                Some(reply)
            }
            Command::SetBrightness { value, reply } => {
                self.brightness = value;
                if let Some(panel) = self.panel.as_mut() {
                    if let Err(err) = panel.set_brightness(value) {
                        self.fail_session(&err, now);
                    }
                } else if self.content.is_none() && self.idle_brightness_due.is_none() {
                    // Idle: `tick` opens the panel briefly; later changes ride along.
                    self.idle_brightness_due = Some(now + IDLE_BRIGHTNESS_DELAY);
                }
                Some(reply)
            }
            Command::SetRotation { value, reply } => {
                self.rotation = value;
                if let Some(panel) = self.panel.as_mut() {
                    if let Err(err) = panel.set_rotation(value) {
                        self.fail_session(&err, now);
                    }
                } else if self.content.is_none() && self.idle_brightness_due.is_none() {
                    // Idle: same brief open as for a brightness change.
                    self.idle_brightness_due = Some(now + IDLE_BRIGHTNESS_DELAY);
                }
                Some(reply)
            }
            Command::UploadStandby { image, reply } => {
                let result = self.upload_standby(&image, now);
                self.publish();
                // The requester may have gone away; nothing to do then.
                let _ = reply.send(result.map(|()| self.snapshot()));
                return;
            }
            Command::UploadBootAnimation { animation, reply } => {
                let result = self.upload_boot_animation(&animation, now);
                self.publish();
                // The requester may have gone away; nothing to do then.
                let _ = reply.send(result.map(|()| self.snapshot()));
                return;
            }
            Command::UpdateInfo { info, reply } => {
                if let Some(loaded) = self.content.as_mut()
                    && loaded.info.kind == tt500_lcd_ipc::ContentKind::Live
                {
                    loaded.info = info;
                }
                Some(reply)
            }
            Command::RestoreFailed { message } => {
                self.content_error = Some(message);
                None
            }
        };
        self.publish();
        if let Some(reply) = reply {
            // The requester may have gone away; nothing to do then.
            let _ = reply.send(self.snapshot());
        }
    }

    /// Writes `image` to the panel's flash; see [`Core::upload`].
    fn upload_standby(&mut self, image: &StandbyImage, now: Instant) -> Result<(), IpcError> {
        tracing::info!(
            "uploading standby image: {} bytes JPEG, {} packets (writes to the panel's flash)",
            image.byte_len(),
            image.packets().len()
        );
        self.upload(UploadKind::Standby, now, |panel| {
            panel.upload_standby(image)
        })
    }

    /// Writes `animation` to the panel's flash; see [`Core::upload`]. Progress goes to the log.
    fn upload_boot_animation(
        &mut self,
        animation: &BootAnimation,
        now: Instant,
    ) -> Result<(), IpcError> {
        tracing::info!(
            "uploading boot animation: {} frames at {} ms ({:.1} fps), {} bytes JPEG, \
             {} data blocks (writes to the panel's flash)",
            animation.frame_count(),
            animation.frame_delay_ms(),
            1000.0 / f64::from(animation.frame_delay_ms()),
            animation.data_len(),
            animation.block_count()
        );
        self.upload(UploadKind::BootAnimation, now, |panel| {
            panel.upload_boot_animation(animation, |percent| {
                tracing::info!("boot animation upload: {percent}%");
            })
        })
    }

    /// Runs `send` on the panel. Streaming: on the open session, between frames. Otherwise
    /// opens the panel, runs the start-up, sets the stored brightness, sends and closes it
    /// again. An absent panel is an error; nothing is queued.
    fn upload(
        &mut self,
        kind: UploadKind,
        now: Instant,
        send: impl FnOnce(&mut Panel<Logged<C::Transport>>) -> Result<(), SessionError>,
    ) -> Result<(), IpcError> {
        let unavailable = |message: String| IpcError {
            code: ErrorCode::Unavailable,
            message,
        };
        let what = kind.name();
        if let Some(panel) = self.panel.as_mut() {
            return match send(panel) {
                Ok(()) => {
                    tracing::info!("{what} uploaded on the open session");
                    Ok(())
                }
                Err(err) => {
                    self.fail_session(&err, now);
                    Err(upload_failed(kind, &err))
                }
            };
        }
        if let Some(reason) = &self.blocked {
            return Err(unavailable(format!(
                "the panel is blocked ({reason}); nothing was uploaded"
            )));
        }
        let transport = self.connector.connect().map_err(|err| {
            unavailable(format!(
                "cannot open the panel: {err}; nothing was uploaded"
            ))
        })?;
        let mut panel = Panel::start(Logged(transport)).map_err(|err| {
            unavailable(format!(
                "panel start-up failed: {err}; nothing was uploaded"
            ))
        })?;
        // Same as every connect: re-apply the stored settings after the start-up.
        apply_stored_settings(&mut panel, self.brightness, self.rotation)
            .and_then(|()| send(&mut panel))
            .map_err(|err| upload_failed(kind, &err))?;
        tracing::info!("{what} uploaded; panel closed again");
        Ok(())
    }

    /// Does whatever is due and returns the next deadline (`None`: wait for a command).
    pub fn tick(&mut self, now: Instant) -> Option<Instant> {
        if self.content.is_none() {
            self.panel = None;
            return self.apply_idle_brightness(now);
        }
        if self.blocked.is_some() {
            return None;
        }
        if self.panel.is_none() {
            if now < self.next_scan {
                return Some(self.next_scan);
            }
            if let Err(retry) = self.connect(now) {
                return retry;
            }
        }
        let (Some(panel), Some(loaded)) = (self.panel.as_mut(), self.content.as_mut()) else {
            return Some(self.next_scan);
        };
        if now >= self.next_keep_alive {
            match panel.keep_alive() {
                Ok(reply) => {
                    if self.last_keep_alive != Some(reply) {
                        tracing::debug!("status {:02x?}", reply.0);
                        self.last_keep_alive = Some(reply);
                    }
                }
                Err(err) => {
                    self.fail_session(&err, now);
                    return Some(self.next_scan);
                }
            }
            self.next_keep_alive += KEEP_ALIVE_INTERVAL;
            if self.next_keep_alive <= now {
                self.next_keep_alive = now + KEEP_ALIVE_INTERVAL;
            }
        }
        if now >= self.next_frame {
            let TimedFrame { frame, hold } = loaded.source.next_frame();
            let repeat = loaded.source.skips_repeats()
                && loaded
                    .last_sent
                    .as_ref()
                    .is_some_and(|(sent, at)| *sent == frame && now < *at + STILL_REFRESH);
            if !repeat {
                if let Err(err) = panel.send_frame(&frame) {
                    self.fail_session(&err, now);
                    return Some(self.next_scan);
                }
                loaded.last_sent = Some((frame, now));
            }
            self.next_frame += hold;
            if self.next_frame <= now {
                self.next_frame = now + hold;
            }
        }
        let problem = loaded.source.problem();
        if problem != self.live_problem {
            self.live_problem = problem;
            self.publish();
        }
        Some(self.next_frame.min(self.next_keep_alive))
    }

    /// Opens and starts the panel. `Err` carries the deadline `tick` should return.
    fn connect(&mut self, now: Instant) -> Result<(), Option<Instant>> {
        let started = self
            .connector
            .connect()
            .map_err(|err| ConnectError::Retry(err.to_string()))
            .and_then(|transport| {
                Panel::start(Logged(transport)).map_err(|err| match err {
                    SessionError::UnexpectedGeometry { .. } => {
                        ConnectError::Blocked(err.to_string())
                    }
                    _ => ConnectError::Retry(err.to_string()),
                })
            });
        let mut panel = match started {
            Ok(panel) => panel,
            Err(err) => return Err(self.connect_failed(err, now)),
        };
        // Re-apply the stored brightness and rotation after every start-up.
        if let Err(err) = apply_stored_settings(&mut panel, self.brightness, self.rotation) {
            return Err(self.connect_failed(ConnectError::Retry(err.to_string()), now));
        }
        tracing::info!("panel connected (firmware {})", panel.info().firmware);
        self.device = DeviceState::Connected {
            firmware: panel.info().firmware.clone(),
        };
        self.panel = Some(panel);
        self.last_error = None;
        self.last_keep_alive = None;
        if let Some(loaded) = self.content.as_mut() {
            loaded.last_sent = None;
        }
        self.next_frame = now;
        self.next_keep_alive = now + KEEP_ALIVE_INTERVAL;
        self.publish();
        Ok(())
    }

    fn connect_failed(&mut self, err: ConnectError, now: Instant) -> Option<Instant> {
        let message = match err {
            ConnectError::Blocked(reason) => {
                tracing::error!("{reason}; not retrying until the next show");
                self.blocked = Some(reason.clone());
                self.device = DeviceState::Blocked { reason };
                self.publish();
                return None;
            }
            ConnectError::Retry(message) => message,
        };
        if self.last_error.as_deref() != Some(message.as_str()) {
            tracing::warn!("cannot open the panel: {message}");
            self.last_error = Some(message.clone());
        }
        self.device = DeviceState::Searching {
            last_error: Some(message),
        };
        self.next_scan = now + SCAN_INTERVAL;
        self.publish();
        Some(self.next_scan)
    }

    /// Drops the session after a device error and goes back to scanning.
    fn fail_session(&mut self, err: &SessionError, now: Instant) {
        tracing::warn!("panel session lost: {err}");
        self.panel = None;
        self.last_error = Some(err.to_string());
        self.device = DeviceState::Searching {
            last_error: Some(err.to_string()),
        };
        self.next_scan = now + SCAN_INTERVAL;
        self.publish();
    }

    /// Idle: once due, opens the panel, runs the start-up, sends the brightness and rotation
    /// and closes it again (no frames).
    fn apply_idle_brightness(&mut self, now: Instant) -> Option<Instant> {
        let due = self.idle_brightness_due?;
        if now < due {
            return Some(due);
        }
        self.idle_brightness_due = None;
        let (brightness, rotation) = (self.brightness, self.rotation);
        let result: Result<(), String> = self
            .connector
            .connect()
            .map_err(|err| err.to_string())
            .and_then(|transport| Panel::start(Logged(transport)).map_err(|err| err.to_string()))
            .and_then(|mut panel| {
                apply_stored_settings(&mut panel, brightness, rotation)
                    .map_err(|err| err.to_string())
            });
        match result {
            Ok(()) => tracing::info!(
                "brightness {} and rotation {} applied while idle",
                brightness.get(),
                rotation.get()
            ),
            Err(err) => tracing::warn!(
                "cannot apply the brightness while idle: {err}; it applies at the next show"
            ),
        }
        None
    }
}

/// Re-applies the stored settings after a start-up: the brightness always, the rotation only
/// when it is not the start-up's own step 0.
fn apply_stored_settings<T: Transport>(
    panel: &mut Panel<T>,
    brightness: Brightness,
    rotation: Rotation,
) -> Result<(), SessionError> {
    panel.set_brightness(brightness)?;
    if rotation != Rotation::default() {
        panel.set_rotation(rotation)?;
    }
    Ok(())
}

/// What an upload writes to the panel's flash.
#[derive(Debug, Clone, Copy)]
enum UploadKind {
    Standby,
    BootAnimation,
}

impl UploadKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Standby => "standby image",
            Self::BootAnimation => "boot animation",
        }
    }
}

fn upload_failed(kind: UploadKind, err: &SessionError) -> IpcError {
    let what = kind.name();
    tracing::error!("{what} upload failed: {err}");
    // The timeout message already says the panel may hold a partial upload.
    let message = if matches!(err, SessionError::BootUploadTimeout { .. }) {
        format!("{what} upload failed: {err}")
    } else {
        format!("{what} upload failed: {err}; the panel may hold a partial upload")
    };
    IpcError {
        code: ErrorCode::Internal,
        message,
    }
}

/// The streamer thread loop; returns when the command channel closes.
pub fn run<C: Connector>(mut core: Core<C>, commands: &Receiver<Command>) {
    loop {
        let deadline = core.tick(Instant::now());
        let received = match deadline {
            Some(at) => commands.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => commands.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(cmd) => core.handle(cmd, Instant::now()),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::sync::mpsc;
    use std::thread;

    use tt500_lcd_protocol::mock::{MockLog, MockTransport, Sent};
    use tt500_lcd_render::RenderOptions;

    use super::*;

    struct MockConnector {
        queue: VecDeque<Result<MockTransport, OpenError>>,
    }

    impl Connector for MockConnector {
        type Transport = MockTransport;

        fn connect(&mut self) -> Result<MockTransport, OpenError> {
            self.queue.pop_front().unwrap_or(Err(OpenError::NotFound))
        }
    }

    fn connector(transports: Vec<MockTransport>) -> MockConnector {
        MockConnector {
            queue: transports.into_iter().map(Ok).collect(),
        }
    }

    fn still() -> Playlist {
        let img = image::RgbImage::from_fn(100, 50, |x, y| image::Rgb([x as u8, y as u8, 9]));
        let mut bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        tt500_lcd_render::render_bytes(&bytes, &RenderOptions::default())
            .unwrap()
            .playlist
    }

    fn animation(holds_ms: &[u64]) -> Playlist {
        let Playlist::Still(frame) = still() else {
            unreachable!("png renders to a still")
        };
        Playlist::Animation(
            holds_ms
                .iter()
                .map(|ms| TimedFrame {
                    frame: frame.clone(),
                    hold: Duration::from_millis(*ms),
                })
                .collect(),
        )
    }

    fn info() -> ContentInfo {
        ContentInfo {
            source: "t.png".into(),
            kind: tt500_lcd_ipc::ContentKind::Still,
            frames: 1,
            fit: tt500_lcd_ipc::Fit::Cover,
            source_size: None,
            crop: None,
            fps: None,
            duration_ms: None,
            path: None,
            max_fps: None,
            live: None,
            steady: None,
        }
    }

    fn core(connector: MockConnector, brightness: u8) -> Core<MockConnector> {
        core_with_rotation(connector, brightness, 0)
    }

    fn core_with_rotation(
        connector: MockConnector,
        brightness: u8,
        rotation: u8,
    ) -> Core<MockConnector> {
        let status = Arc::new(StatusHub::new(Status {
            daemon_version: String::new(),
            ipc_version: 0,
            device: DeviceState::Idle,
            content: None,
            brightness: 0,
            rotation: None,
            content_error: None,
            profile: None,
        }));
        Core::new(
            connector,
            Brightness::try_from(brightness).unwrap(),
            Rotation::try_from(rotation).unwrap(),
            status,
        )
    }

    fn show(core: &mut Core<MockConnector>, playlist: Playlist, now: Instant) {
        let (reply, _rx) = mpsc::channel();
        core.handle(
            Command::Show {
                content: Content::Playlist(playlist),
                info: info(),
                reply,
            },
            now,
        );
    }

    fn video(frames: usize) -> VideoStream {
        let raw: Vec<u8> = (0..frames)
            .flat_map(|i| vec![(i * 60) as u8; 480 * 128 * 3])
            .collect();
        VideoStream::from_raw_rgb(Cursor::new(raw), ms(70), 90).unwrap()
    }

    #[test]
    fn video_streams_at_its_frame_interval_until_cleared() {
        let (transport, log) = MockTransport::new();
        let mut core = core(connector(vec![transport]), 100);
        let t0 = Instant::now();
        let (reply, _rx) = mpsc::channel();
        core.handle(
            Command::Show {
                content: Content::Video(video(3)),
                info: info(),
                reply,
            },
            t0,
        );
        // Let the decoder queue its frames so a lagging poll cannot repeat the first one.
        thread::sleep(ms(100));
        // Three frames: the end of the stream cannot be observed within two polls.
        assert_eq!(core.tick(t0), Some(t0 + ms(70)));
        assert_eq!(core.tick(t0 + ms(70)), Some(t0 + ms(140)));
        assert_eq!(frame_markers(&log).len(), 2);
        clear(&mut core, t0 + ms(100));
        assert_eq!(core.tick(t0 + ms(140)), None);
        assert_eq!(frame_markers(&log).len(), 2);
    }

    #[test]
    fn identical_video_frames_are_sent_once_until_the_refresh() {
        let (transport, log) = MockTransport::new();
        let mut core = core(connector(vec![transport]), 100);
        let t0 = Instant::now();
        let (reply, _rx) = mpsc::channel();
        // Every raw frame is the same grey, so every encoded frame is identical.
        let raw = vec![90u8; 480 * 128 * 3 * 40];
        let stream = VideoStream::from_raw_rgb(Cursor::new(raw), ms(70), 90).unwrap();
        core.handle(
            Command::Show {
                content: Content::Video(stream),
                info: info(),
                reply,
            },
            t0,
        );
        for k in 0..5 {
            core.tick(t0 + ms(70) * k);
        }
        assert_eq!(frame_markers(&log).len(), 1);
        core.tick(t0 + STILL_REFRESH + ms(70));
        assert_eq!(frame_markers(&log).len(), 2);
    }

    fn set_brightness(core: &mut Core<MockConnector>, value: u8, now: Instant) {
        let (reply, _rx) = mpsc::channel();
        let value = Brightness::try_from(value).unwrap();
        core.handle(Command::SetBrightness { value, reply }, now);
    }

    fn clear(core: &mut Core<MockConnector>, now: Instant) {
        let (reply, _rx) = mpsc::channel();
        core.handle(Command::Clear { reply }, now);
    }

    fn device(core: &Core<MockConnector>) -> DeviceState {
        core.snapshot().device
    }

    /// Byte 2 of every frame's first packet.
    fn frame_markers(log: &MockLog) -> Vec<u8> {
        log.snapshot()
            .iter()
            .filter_map(|sent| match sent {
                Sent::Output(bytes) if bytes[1] == 0x09 && bytes[6] == 0 && bytes[7] == 0 => {
                    Some(bytes[2])
                }
                _ => None,
            })
            .collect()
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn set_rotation(core: &mut Core<MockConnector>, value: u8, now: Instant) {
        let (reply, _rx) = std::sync::mpsc::channel();
        let value = Rotation::try_from(value).unwrap();
        core.handle(Command::SetRotation { value, reply }, now);
    }

    fn set_1d(rotation: u8) -> Sent {
        let mut bytes = vec![0u8; 64];
        bytes[0] = 0x03;
        bytes[1] = 0x1d;
        bytes[2] = rotation;
        Sent::SetFeature(bytes)
    }

    fn set_0c(brightness: u8) -> Sent {
        let mut bytes = vec![0u8; 64];
        bytes[0] = 0x03;
        bytes[1] = 0x0c;
        bytes[2] = brightness;
        Sent::SetFeature(bytes)
    }

    #[test]
    fn show_runs_startup_then_streams_and_keeps_alive() {
        let (transport, log) = MockTransport::new();
        let mut core = core(connector(vec![transport]), 100);
        let t0 = Instant::now();
        assert_eq!(device(&core), DeviceState::Idle);
        show(&mut core, still(), t0);
        assert_eq!(core.tick(t0), Some(t0 + STILL_REFRESH));
        assert_eq!(
            device(&core),
            DeviceState::Connected {
                firmware: "21.0.5.4".into()
            }
        );

        let sent = log.snapshot();
        let ids: Vec<String> = sent
            .iter()
            .take(9)
            .map(|s| match s {
                Sent::GetFeature(id) => format!("GET {id:02x}"),
                Sent::SetFeature(b) => format!("SET {:02x}", b[1]),
                Sent::Output(_) => "OUT".into(),
            })
            .collect();
        assert_eq!(
            ids,
            [
                "GET 0a", "GET 10", "SET 18", "SET 1a", "GET 07", "SET 0c", "GET 0f", "SET 1d",
                "SET 0c"
            ]
        );
        assert_eq!(sent[8], set_0c(100));
        assert_eq!(frame_markers(&log), [0x00]);

        assert_eq!(core.tick(t0 + STILL_REFRESH), Some(t0 + ms(2000)));
        // The re-applied start-up brightness (100) is echoed in byte 2 after the first frame.
        assert_eq!(frame_markers(&log), [0x00, 0x64]);

        core.tick(t0 + ms(2000));
        assert_eq!(log.snapshot().last(), Some(&Sent::GetFeature(0x0f)));
    }

    #[test]
    fn animation_holds_are_respected() {
        let (transport, log) = MockTransport::new();
        let mut core = core(connector(vec![transport]), 100);
        let t0 = Instant::now();
        show(&mut core, animation(&[100, 200]), t0);
        assert_eq!(core.tick(t0), Some(t0 + ms(100)));
        assert_eq!(core.tick(t0 + ms(100)), Some(t0 + ms(300)));
        assert_eq!(core.tick(t0 + ms(200)), Some(t0 + ms(300)));
        assert_eq!(frame_markers(&log).len(), 2);
        assert_eq!(core.tick(t0 + ms(300)), Some(t0 + ms(400)));
        assert_eq!(frame_markers(&log).len(), 3);
    }

    #[test]
    fn transport_failure_rescans_and_restarts_with_stored_brightness() {
        let (broken, _) = MockTransport::new();
        // Start-up plus brightness is 9 calls; the first frame write fails.
        let broken = broken.failing_from(9);
        let (fresh, log) = MockTransport::new();
        let mut core = core(connector(vec![broken, fresh]), 30);
        let t0 = Instant::now();
        show(&mut core, still(), t0);
        assert_eq!(core.tick(t0), Some(t0 + SCAN_INTERVAL));
        assert!(matches!(device(&core), DeviceState::Searching { .. }));

        assert_eq!(core.tick(t0 + ms(1000)), Some(t0 + SCAN_INTERVAL));
        assert!(log.snapshot().is_empty());

        core.tick(t0 + SCAN_INTERVAL);
        assert!(matches!(device(&core), DeviceState::Connected { .. }));
        let sent = log.snapshot();
        assert_eq!(sent[0], Sent::GetFeature(0x0a));
        assert_eq!(sent[8], set_0c(30));
        assert_eq!(frame_markers(&log), [0x00]);
    }

    #[test]
    fn geometry_mismatch_blocks_without_retries() {
        let (wrong, log) = MockTransport::new();
        let wrong = wrong.with_reply(0x07, &[0x07, 0, 0, 0, 0, 0, 0, 0x20, 0x03, 0xe0, 0x01]);
        let (spare, _) = MockTransport::new();
        let mut core = core(connector(vec![wrong, spare]), 100);
        let t0 = Instant::now();
        show(&mut core, still(), t0);
        assert_eq!(core.tick(t0), None);
        assert!(matches!(device(&core), DeviceState::Blocked { .. }));
        assert_eq!(core.tick(t0 + ms(5000)), None);
        assert_eq!(core.connector.queue.len(), 1);
        assert!(frame_markers(&log).is_empty());
        // Nothing past GET 0x07 was sent.
        assert_eq!(log.snapshot().last(), Some(&Sent::GetFeature(0x07)));

        // A new show lifts the block.
        show(&mut core, still(), t0 + ms(6000));
        core.tick(t0 + ms(6000));
        assert!(matches!(device(&core), DeviceState::Connected { .. }));
    }

    #[test]
    fn live_stream_ticks_at_its_fps_until_cleared() {
        use crate::live::LiveStream;
        use crate::live::fake::FakeCapturer;

        let (transport, log) = MockTransport::new();
        let mut core = core(connector(vec![transport]), 100);
        let t0 = Instant::now();
        let (stream, control, _) = LiveStream::from_capturer(
            tt500_lcd_ipc::LiveSource::Screen { output: None },
            FakeCapturer::new(480, 128),
            RenderOptions::default(),
            20,
        )
        .unwrap();
        let (reply, _rx) = mpsc::channel();
        core.handle(
            Command::Show {
                content: Content::Live(stream),
                info: info(),
                reply,
            },
            t0,
        );
        assert_eq!(core.tick(t0), Some(t0 + ms(50)));
        assert_eq!(core.tick(t0 + ms(50)), Some(t0 + ms(100)));
        assert_eq!(frame_markers(&log).len(), 2);
        clear(&mut core, t0 + ms(100));
        assert!(
            !control.is_alive(),
            "clear drops the stream and joins its worker"
        );
    }

    #[test]
    fn live_problem_shows_as_content_error_until_cleared() {
        use crate::live::LiveStream;
        use crate::live::fake::FakeCapturer;

        let (transport, _log) = MockTransport::new();
        let mut core = core(connector(vec![transport]), 100);
        let t0 = Instant::now();
        let (stream, _control, _) = LiveStream::from_capturer(
            tt500_lcd_ipc::LiveSource::Screen { output: None },
            FakeCapturer::new(480, 128).with_problem("command not found: alacrtitty"),
            RenderOptions::default(),
            20,
        )
        .unwrap();
        let (reply, _rx) = mpsc::channel();
        core.handle(
            Command::Show {
                content: Content::Live(stream),
                info: info(),
                reply,
            },
            t0,
        );
        assert_eq!(core.snapshot().content_error, None);
        core.tick(t0);
        assert_eq!(
            core.snapshot().content_error.as_deref(),
            Some("command not found: alacrtitty")
        );
        clear(&mut core, t0 + ms(100));
        assert_eq!(core.snapshot().content_error, None);
    }

    #[test]
    fn clear_drops_the_panel() {
        let (transport, log) = MockTransport::new();
        let mut core = core(connector(vec![transport]), 100);
        let t0 = Instant::now();
        show(&mut core, still(), t0);
        core.tick(t0);
        let sent = log.snapshot().len();
        clear(&mut core, t0 + ms(10));
        assert_eq!(device(&core), DeviceState::Idle);
        assert_eq!(core.tick(t0 + ms(5000)), None);
        assert!(core.panel.is_none());
        assert_eq!(log.snapshot().len(), sent);
        assert!(core.snapshot().content.is_none());
    }

    #[test]
    fn brightness_change_while_connected_is_immediate_and_echoed() {
        let (transport, log) = MockTransport::new();
        let mut core = core(connector(vec![transport]), 100);
        let t0 = Instant::now();
        show(&mut core, still(), t0);
        core.tick(t0);
        set_brightness(&mut core, 30, t0 + ms(10));
        assert_eq!(log.snapshot().last(), Some(&set_0c(30)));
        assert_eq!(core.snapshot().brightness, 30);
        for k in 1..=9 {
            core.tick(t0 + STILL_REFRESH * k);
        }
        let mut want = vec![0x00];
        want.extend([0x1e; 8]);
        want.push(0xff);
        assert_eq!(frame_markers(&log), want);
    }

    #[test]
    fn rotation_change_while_connected_is_immediate() {
        let (transport, log) = MockTransport::new();
        let mut core = core(connector(vec![transport]), 100);
        let t0 = Instant::now();
        show(&mut core, still(), t0);
        core.tick(t0);
        assert_eq!(core.snapshot().rotation, Some(0));
        set_rotation(&mut core, 2, t0 + ms(10));
        assert_eq!(log.snapshot().last(), Some(&set_1d(2)));
        assert_eq!(core.snapshot().rotation, Some(2));
    }

    #[test]
    fn stored_rotation_is_reapplied_after_startup_and_brightness() {
        let (transport, log) = MockTransport::new();
        let mut core = core_with_rotation(connector(vec![transport]), 70, 3);
        let t0 = Instant::now();
        show(&mut core, still(), t0);
        core.tick(t0);
        let sent = log.snapshot();
        assert_eq!(sent[7], Sent::SetFeature(startup_1d()));
        assert_eq!(sent[8], set_0c(70));
        assert_eq!(sent[9], set_1d(3));
    }

    #[test]
    fn idle_rotation_runs_startup_then_closes() {
        let (transport, log) = MockTransport::new();
        let mut core = core(connector(vec![transport]), 100);
        let t0 = Instant::now();
        set_rotation(&mut core, 1, t0);
        assert_eq!(core.tick(t0), Some(t0 + IDLE_BRIGHTNESS_DELAY));
        assert!(log.snapshot().is_empty());
        assert_eq!(core.tick(t0 + IDLE_BRIGHTNESS_DELAY), None);
        let sent = log.snapshot();
        assert_eq!(sent.len(), 10);
        assert_eq!(sent[8], set_0c(100));
        assert_eq!(sent[9], set_1d(1));
    }

    fn startup_1d() -> Vec<u8> {
        let mut bytes = vec![0u8; 64];
        bytes[..9].copy_from_slice(&[0x03, 0x1d, 0x00, 0xff, 0xff, 0xea, 0xff, 0xff, 0xff]);
        bytes
    }

    #[test]
    fn show_supersedes_pending_idle_brightness() {
        let (transport, log) = MockTransport::new();
        let mut core = core(connector(vec![transport]), 100);
        let t0 = Instant::now();
        set_brightness(&mut core, 30, t0);
        assert_eq!(core.tick(t0), Some(t0 + IDLE_BRIGHTNESS_DELAY));
        assert!(log.snapshot().is_empty());
        show(&mut core, still(), t0 + ms(1));
        core.tick(t0 + ms(1));
        assert!(matches!(device(&core), DeviceState::Connected { .. }));
        assert_eq!(log.snapshot()[8], set_0c(30));
        core.tick(t0 + IDLE_BRIGHTNESS_DELAY);
        let startups = log
            .snapshot()
            .iter()
            .filter(|s| **s == Sent::GetFeature(0x0a))
            .count();
        assert_eq!(startups, 1);
    }

    #[test]
    fn idle_brightness_runs_startup_then_closes() {
        let (transport, log) = MockTransport::new();
        let mut core = core(connector(vec![transport]), 100);
        let t0 = Instant::now();
        set_brightness(&mut core, 30, t0);
        assert_eq!(core.tick(t0), Some(t0 + IDLE_BRIGHTNESS_DELAY));
        assert!(log.snapshot().is_empty());
        assert_eq!(core.tick(t0 + IDLE_BRIGHTNESS_DELAY), None);
        let sent = log.snapshot();
        let ids: Vec<String> = sent
            .iter()
            .map(|s| match s {
                Sent::GetFeature(id) => format!("GET {id:02x}"),
                Sent::SetFeature(b) => format!("SET {:02x}", b[1]),
                Sent::Output(_) => "OUT".into(),
            })
            .collect();
        assert_eq!(
            ids,
            [
                "GET 0a", "GET 10", "SET 18", "SET 1a", "GET 07", "SET 0c", "GET 0f", "SET 1d",
                "SET 0c"
            ]
        );
        assert_eq!(sent[8], set_0c(30));
        assert!(frame_markers(&log).is_empty());
        assert!(core.panel.is_none());
        assert_eq!(device(&core), DeviceState::Idle);
        assert!(core.connector.queue.is_empty());
    }

    fn standby_image() -> StandbyImage {
        let img = image::RgbImage::from_fn(100, 50, |x, y| image::Rgb([x as u8, y as u8, 9]));
        let mut bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        tt500_lcd_render::render_standby_bytes(&bytes, &RenderOptions::default()).unwrap()
    }

    fn upload(core: &mut Core<MockConnector>, now: Instant) -> Result<Status, IpcError> {
        let (reply, rx) = mpsc::channel();
        core.handle(
            Command::UploadStandby {
                image: standby_image(),
                reply,
            },
            now,
        );
        rx.recv().unwrap()
    }

    fn standby_slots(log: &MockLog) -> Vec<u8> {
        log.snapshot()
            .iter()
            .filter_map(|s| match s {
                Sent::Output(b) if b[1] == 0x0d => Some(b[2]),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn standby_upload_while_streaming_uses_the_open_session() {
        let (transport, log) = MockTransport::new();
        let mut core = core(connector(vec![transport]), 100);
        let t0 = Instant::now();
        show(&mut core, still(), t0);
        core.tick(t0);
        upload(&mut core, t0 + ms(10)).unwrap();
        assert!(core.panel.is_some());
        core.tick(t0 + STILL_REFRESH);
        let startups = log
            .snapshot()
            .iter()
            .filter(|s| **s == Sent::GetFeature(0x0a))
            .count();
        assert_eq!(startups, 1);
        let expected_packets = standby_image().packets().len();
        assert_eq!(standby_slots(&log).len(), expected_packets);
        // Live frame, both standby slots, then the first-frame marker again.
        assert_eq!(frame_markers(&log), [0x00, 0x00]);
    }

    #[test]
    fn standby_upload_while_idle_runs_startup_and_closes() {
        let (transport, log) = MockTransport::new();
        let mut core = core(connector(vec![transport]), 40);
        let t0 = Instant::now();
        upload(&mut core, t0).unwrap();
        let sent = log.snapshot();
        assert_eq!(sent[0], Sent::GetFeature(0x0a));
        assert_eq!(sent[8], set_0c(40));
        assert!(matches!(sent[9], Sent::Output(_)));
        assert_eq!(standby_slots(&log).len(), standby_image().packets().len());
        assert!(core.panel.is_none());
        assert_eq!(device(&core), DeviceState::Idle);
    }

    fn boot_animation() -> BootAnimation {
        let gif = {
            let mut gif = Vec::new();
            let mut enc = image::codecs::gif::GifEncoder::new(&mut gif);
            for i in 0..8u8 {
                let buf = image::RgbaImage::from_pixel(64, 32, image::Rgba([i * 30, 50, 9, 255]));
                let delay = image::Delay::from_numer_denom_ms(100, 1);
                enc.encode_frame(image::Frame::from_parts(buf, 0, 0, delay))
                    .unwrap();
            }
            drop(enc);
            gif
        };
        tt500_lcd_render::render_boot_animation_bytes(&gif, &RenderOptions::default())
            .unwrap()
            .animation
    }

    fn upload_boot(core: &mut Core<MockConnector>, now: Instant) -> Result<Status, IpcError> {
        let (reply, rx) = mpsc::channel();
        core.handle(
            Command::UploadBootAnimation {
                animation: boot_animation(),
                reply,
            },
            now,
        );
        rx.recv().unwrap()
    }

    /// A mock panel that confirms the single data block of the test animation.
    fn boot_transport() -> (MockTransport, MockLog) {
        let (transport, log) = MockTransport::new();
        (
            transport.with_reply_sequence(0x0d, &[&[0x0d, 0x00], &[0x0d, 0x01]]),
            log,
        )
    }

    fn boot_packet_blocks(log: &MockLog) -> Vec<u8> {
        log.snapshot()
            .iter()
            .filter_map(|s| match s {
                Sent::Output(b) if b[1] == 0x0c => Some(b[2]),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn boot_upload_while_idle_runs_startup_and_closes() {
        let (transport, log) = boot_transport();
        let mut core = core(connector(vec![transport]), 40);
        let t0 = Instant::now();
        upload_boot(&mut core, t0).unwrap();
        let sent = log.snapshot();
        assert_eq!(sent[0], Sent::GetFeature(0x0a));
        assert_eq!(sent[8], set_0c(40));
        assert!(matches!(&sent[9], Sent::Output(b) if b[1] == 0x0c && b[2] == 0));
        let blocks = boot_packet_blocks(&log);
        assert_eq!(blocks.iter().filter(|&&b| b == 0).count(), 1);
        assert!(blocks.contains(&1));
        assert!(core.panel.is_none());
        assert_eq!(device(&core), DeviceState::Idle);
    }

    #[test]
    fn boot_upload_while_streaming_uses_the_open_session() {
        let (transport, log) = boot_transport();
        let mut core = core(connector(vec![transport]), 100);
        let t0 = Instant::now();
        show(&mut core, still(), t0);
        core.tick(t0);
        upload_boot(&mut core, t0 + ms(10)).unwrap();
        assert!(core.panel.is_some());
        core.tick(t0 + STILL_REFRESH);
        let startups = log
            .snapshot()
            .iter()
            .filter(|s| **s == Sent::GetFeature(0x0a))
            .count();
        assert_eq!(startups, 1);
        assert!(!boot_packet_blocks(&log).is_empty());
        // Live frame, then the first-frame marker again after the upload.
        assert_eq!(frame_markers(&log), [0x00, 0x00]);
    }

    #[test]
    fn boot_upload_without_panel_errors_and_queues_nothing() {
        let mut core = core(connector(vec![]), 100);
        let t0 = Instant::now();
        let err = upload_boot(&mut core, t0).unwrap_err();
        assert_eq!(err.code, ErrorCode::Unavailable);
        assert!(err.message.contains("nothing was uploaded"), "{err}");
        let (transport, log) = boot_transport();
        core.connector.queue.push_back(Ok(transport));
        core.tick(t0 + ms(5));
        assert!(boot_packet_blocks(&log).is_empty());
    }

    #[test]
    fn standby_upload_without_panel_errors_and_queues_nothing() {
        let mut core = core(connector(vec![]), 100);
        let t0 = Instant::now();
        let err = upload(&mut core, t0).unwrap_err();
        assert_eq!(err.code, ErrorCode::Unavailable);
        assert!(err.message.contains("nothing was uploaded"), "{err}");
        // A panel that shows up later gets no upload.
        let (transport, log) = MockTransport::new();
        core.connector.queue.push_back(Ok(transport));
        core.tick(t0 + ms(5));
        assert!(standby_slots(&log).is_empty());
    }

    #[test]
    fn idle_brightness_changes_are_coalesced() {
        let (transport, log) = MockTransport::new();
        let mut core = core(connector(vec![transport]), 100);
        let t0 = Instant::now();
        set_brightness(&mut core, 30, t0);
        set_brightness(&mut core, 40, t0 + ms(50));
        set_brightness(&mut core, 50, t0 + ms(100));
        core.tick(t0 + IDLE_BRIGHTNESS_DELAY);
        let sent = log.snapshot();
        assert_eq!(sent.len(), 9);
        assert_eq!(sent[8], set_0c(50));
    }

    #[test]
    fn idle_brightness_failure_stays_idle() {
        let (transport, log) = MockTransport::new();
        let mut core = core(
            MockConnector {
                queue: VecDeque::from([Err(OpenError::NotFound), Ok(transport)]),
            },
            100,
        );
        let t0 = Instant::now();
        set_brightness(&mut core, 30, t0);
        assert_eq!(core.tick(t0 + IDLE_BRIGHTNESS_DELAY), None);
        assert_eq!(device(&core), DeviceState::Idle);
        assert_eq!(core.snapshot().brightness, 30);
        assert!(log.snapshot().is_empty());
        show(&mut core, still(), t0 + ms(300));
        core.tick(t0 + ms(300));
        assert_eq!(log.snapshot()[8], set_0c(30));
    }

    #[test]
    fn absent_panel_reports_searching() {
        let mut core = core(connector(vec![]), 100);
        let t0 = Instant::now();
        show(&mut core, still(), t0);
        assert_eq!(core.tick(t0), Some(t0 + SCAN_INTERVAL));
        assert!(matches!(
            device(&core),
            DeviceState::Searching {
                last_error: Some(_)
            }
        ));
    }

    #[test]
    fn restore_failure_is_reported_until_show() {
        let mut core = core(connector(vec![]), 100);
        let t0 = Instant::now();
        core.handle(
            Command::RestoreFailed {
                message: "clip.mkv not found".into(),
            },
            t0,
        );
        assert_eq!(
            core.snapshot().content_error.as_deref(),
            Some("clip.mkv not found")
        );
        show(&mut core, still(), t0);
        assert_eq!(core.snapshot().content_error, None);
    }

    #[test]
    fn unchanged_rescans_push_nothing() {
        let mut core = core(connector(vec![]), 100);
        let (_, changes) = core.status.subscribe();
        let t0 = Instant::now();
        show(&mut core, still(), t0);
        core.tick(t0);
        core.tick(t0 + SCAN_INTERVAL);
        let pushed: Vec<Status> = changes.try_iter().collect();
        let searching = pushed
            .iter()
            .filter(|s| {
                matches!(
                    s.device,
                    DeviceState::Searching {
                        last_error: Some(_)
                    }
                )
            })
            .count();
        assert_eq!(searching, 1, "pushed: {pushed:?}");
    }
}
