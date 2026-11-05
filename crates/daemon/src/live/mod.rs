//! Live sources: screen capture and the headless virtual desktop.
//!
//! [`start`] builds a capturer and a worker thread; the [`LiveStream`] feeds the streamer
//! and the [`LiveControl`] stays with the IPC server (crop changes, previews).

pub mod capture;
#[cfg(test)]
pub mod fake;
pub mod portal;
pub mod preview;
pub mod screencopy;
pub mod steady;
pub mod vdesktop;

use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use image::RgbImage;
use tt500_lcd_ipc::{ErrorCode, IpcError, LiveSource, Size};
use tt500_lcd_protocol::Frame;
use tt500_lcd_render::{RenderError, RenderOptions, VideoPoll, render_rgb_frame};

use self::capture::{CaptureError, Capturer};
use self::preview::{Preview, PreviewCache};
use self::screencopy::Screencopy;
use self::steady::SteadyGate;
use self::vdesktop::VirtualDesktop;
use crate::streamer::FrameStream;

/// How long `start` waits for the first frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(15);
/// The same for the portal, which first asks the user and must stay below the IPC read timeout.
const PORTAL_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(28);
/// Pause before reconnecting after the source vanished.
const RECONNECT_INTERVAL: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(2)
};
/// Pause before capturing again after a failed capture.
const FAILED_RETRY: Duration = Duration::from_secs(1);
/// How often the virtual desktop's clients are checked.
const SUPERVISE_INTERVAL: Duration = Duration::from_secs(1);
/// Shaped frames waiting for the streamer; a full queue drops the newest.
const QUEUE_DEPTH: usize = 2;
/// How long dropping a stream waits for its worker before leaving it to finish alone.
const STOP_GRACE: Duration = Duration::from_secs(3);
/// Granularity of interruptible sleeps.
const SLEEP_STEP: Duration = Duration::from_millis(50);

/// The source as it is stored and compared: a one-time `reselect` is not part of it.
#[must_use]
pub fn persistent(source: &LiveSource) -> LiveSource {
    match source {
        LiveSource::Portal { .. } => LiveSource::Portal { reselect: false },
        other => other.clone(),
    }
}

/// A short description of the source for the status line.
#[must_use]
pub fn label(source: &LiveSource) -> String {
    match source {
        LiveSource::Desktop(spec) => {
            format!(
                "Virtual desktop {}\u{d7}{}",
                spec.size.width, spec.size.height
            )
        }
        LiveSource::Screen { output: Some(name) } => format!("Screen {name}"),
        LiveSource::Screen { output: None } => "Screen".to_owned(),
        LiveSource::Portal { .. } => "Screen or window".to_owned(),
        _ => "Live source".to_owned(),
    }
}

/// How to reach a fresh capturer; called again after the source vanished.
type Connect = Box<dyn FnMut() -> Result<Box<dyn Capturer>, CaptureError> + Send>;

/// How the worker shapes frames; changed in place by [`LiveControl::set_options`].
#[derive(Debug, Clone, Copy)]
struct Shaping {
    options: RenderOptions,
    fps: u32,
    /// Hold back half-drawn frames (see [`SteadyGate`]).
    steady: bool,
}

#[derive(Debug)]
struct Shared {
    spec: LiveSource,
    shaping: Mutex<Shaping>,
    latest: Mutex<Option<(Arc<RgbImage>, u64)>>,
    /// The last encoded preview (see [`PreviewCache`]).
    preview: Mutex<PreviewCache>,
    stop: AtomicBool,
    alive: AtomicBool,
    /// What the source reports as wrong while it keeps running (see [`Capturer::problem`]).
    problem: Mutex<Option<String>>,
}

/// The server's handle on a running live source.
#[derive(Debug, Clone)]
pub struct LiveControl(Arc<Shared>);

impl LiveControl {
    /// The source being captured.
    #[must_use]
    pub fn spec(&self) -> &LiveSource {
        &self.0.spec
    }

    /// Changes how frames are shaped and how often they are captured; nothing restarts.
    pub fn set_options(&self, options: RenderOptions, fps: u32, steady: bool) {
        *self.0.shaping.lock().expect("lock poisoned") = Shaping {
            options,
            fps,
            steady,
        };
    }

    /// The latest full source frame and its capture counter.
    #[must_use]
    pub fn latest(&self) -> Option<(Arc<RgbImage>, u64)> {
        self.0.latest.lock().expect("lock poisoned").clone()
    }

    /// A JPEG of the latest frame, downscaled to `max_width`, or `None` before the first
    /// frame. Encoded only when asked, at most every [`preview::MIN_INTERVAL`] and not
    /// again for a frame already encoded.
    ///
    /// # Errors
    ///
    /// [`RenderError`] when the frame cannot be encoded.
    pub fn preview(&self, max_width: u32) -> Result<Option<Preview>, RenderError> {
        let Some((img, seq)) = self.latest() else {
            return Ok(None);
        };
        let now = Instant::now();
        let mut cache = self.0.preview.lock().expect("lock poisoned");
        if let Some(hit) = cache.fresh(now, seq) {
            return Ok(Some(hit));
        }
        let jpeg = tt500_lcd_render::encode_preview_jpeg(&img, max_width)?;
        let preview = Preview {
            jpeg: Arc::new(jpeg),
            size: Size {
                width: img.width(),
                height: img.height(),
            },
            seq,
        };
        cache.store(now, preview.clone());
        Ok(Some(preview))
    }

    /// Whether the capture thread is still running.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.0.alive.load(Ordering::SeqCst)
    }
}

/// Stops the worker and waits (a little) for it when dropped.
struct WorkerGuard {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl fmt::Debug for WorkerGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerGuard").finish_non_exhaustive()
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        let Some(handle) = self.handle.take() else {
            return;
        };
        let deadline = Instant::now() + STOP_GRACE;
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if handle.is_finished() {
            let _ = handle.join();
        } else {
            tracing::warn!("the capture thread is slow to stop; it cleans up on its own");
        }
    }
}

/// A live capture owned by the streamer; dropping it stops the capture (and the desktop).
#[derive(Debug)]
pub struct LiveStream {
    frames: Receiver<Frame>,
    first: Frame,
    shared: Arc<Shared>,
    _worker: WorkerGuard,
}

impl FrameStream for LiveStream {
    fn poll(&self) -> VideoPoll {
        match self.frames.try_recv() {
            Ok(frame) => VideoPoll::Frame(frame),
            Err(TryRecvError::Empty) => VideoPoll::Pending,
            Err(TryRecvError::Disconnected) => VideoPoll::Ended(
                self.problem()
                    .unwrap_or_else(|| "the capture stopped".to_owned()),
            ),
        }
    }

    fn first_frame(&self) -> &Frame {
        &self.first
    }

    fn frame_interval(&self) -> Duration {
        interval(self.shared.shaping.lock().expect("lock poisoned").fps)
    }

    fn problem(&self) -> Option<String> {
        self.shared.problem.lock().expect("lock poisoned").clone()
    }
}

impl LiveStream {
    /// Starts a stream on an existing capturer (tests); it is not replaced once gone.
    #[cfg(test)]
    pub fn from_capturer(
        spec: LiveSource,
        capturer: impl Capturer + 'static,
        options: RenderOptions,
        fps: u32,
    ) -> Result<(Self, LiveControl, Size), IpcError> {
        let steady = default_steady(&spec);
        let mut slot: Option<Box<dyn Capturer>> = Some(Box::new(capturer));
        let connect: Connect = Box::new(move || {
            slot.take()
                .ok_or_else(|| CaptureError::Failed("the test capturer is used up".to_owned()))
        });
        Self::spawn(spec, connect, options, fps, steady, FIRST_FRAME_TIMEOUT)
    }

    fn spawn(
        spec: LiveSource,
        connect: Connect,
        options: RenderOptions,
        fps: u32,
        steady: bool,
        first_timeout: Duration,
    ) -> Result<(Self, LiveControl, Size), IpcError> {
        let shared = Arc::new(Shared {
            spec,
            shaping: Mutex::new(Shaping {
                options,
                fps,
                steady,
            }),
            latest: Mutex::new(None),
            preview: Mutex::new(PreviewCache::default()),
            stop: AtomicBool::new(false),
            alive: AtomicBool::new(true),
            problem: Mutex::new(None),
        });
        let (frames_tx, frames) = mpsc::sync_channel(QUEUE_DEPTH);
        let (first_tx, first_rx) = mpsc::channel();
        let worker = Worker {
            shared: Arc::clone(&shared),
            connect,
            frames: frames_tx,
            first: Some(first_tx),
            seq: 0,
            warned_crop: false,
            gate: SteadyGate::default(),
        };
        let handle = thread::Builder::new()
            .name("live-capture".into())
            .spawn(move || worker.run())
            .map_err(|err| IpcError {
                code: ErrorCode::Internal,
                message: format!("cannot spawn the capture thread: {err}"),
            })?;
        // From here on, dropping the guard stops the worker.
        let guard = WorkerGuard {
            shared: Arc::clone(&shared),
            handle: Some(handle),
        };
        let (first, size) = match first_rx.recv_timeout(first_timeout) {
            Ok(result) => result?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(IpcError {
                    code: ErrorCode::Unavailable,
                    message: format!(
                        "the source gave no frame within {} s",
                        first_timeout.as_secs()
                    ),
                });
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(IpcError {
                    code: ErrorCode::Internal,
                    message: "the capture thread stopped".to_owned(),
                });
            }
        };
        let stream = Self {
            frames,
            first,
            shared: Arc::clone(&shared),
            _worker: guard,
        };
        Ok((stream, LiveControl(shared), size))
    }
}

/// Whether a source holds back half-drawn frames when the request doesn't say: a virtual
/// desktop redraws terminals in place, a screen is shown as it is.
#[must_use]
pub fn default_steady(source: &LiveSource) -> bool {
    matches!(source, LiveSource::Desktop(_))
}

fn interval(fps: u32) -> Duration {
    Duration::from_secs(1) / fps.max(1)
}

/// The capture thread's state.
struct Worker {
    shared: Arc<Shared>,
    connect: Connect,
    frames: SyncSender<Frame>,
    /// Answers `start` with the first frame or the reason there is none.
    first: Option<Sender<Result<(Frame, Size), IpcError>>>,
    seq: u64,
    warned_crop: bool,
    gate: SteadyGate,
}

impl Worker {
    fn set_problem(&self, problem: Option<String>) {
        *self.shared.problem.lock().expect("lock poisoned") = problem;
    }

    fn stopped(&self) -> bool {
        self.shared.stop.load(Ordering::SeqCst)
    }

    fn run(mut self) {
        let mut capturer: Option<Box<dyn Capturer>> = None;
        let mut next_tick = Instant::now();
        while !self.stopped() {
            if capturer.is_none() {
                match (self.connect)() {
                    Ok(connected) => capturer = Some(connected),
                    Err(err) => {
                        if self.retry_or_give_up(&err) {
                            continue;
                        }
                        break;
                    }
                }
            }
            let Some(active) = capturer.as_mut() else {
                continue;
            };
            match active.capture() {
                Ok(img) => {
                    self.set_problem(active.problem());
                    if !self.deliver(img) {
                        break;
                    }
                }
                Err(err) => {
                    if matches!(err, CaptureError::Gone(_)) {
                        // Drops the desktop too; the next round starts a fresh one.
                        capturer = None;
                    }
                    if self.retry_or_give_up(&err) {
                        continue;
                    }
                    break;
                }
            }
            let hold = interval(self.shared.shaping.lock().expect("lock poisoned").fps);
            next_tick += hold;
            let now = Instant::now();
            if next_tick <= now {
                next_tick = now + hold;
            }
            self.sleep_until(next_tick);
        }
        drop(capturer);
        self.shared.alive.store(false, Ordering::SeqCst);
    }

    /// Reports a failure: fatal while `start` still waits, otherwise logged and retried
    /// after a pause. `false` means the worker must stop.
    fn retry_or_give_up(&mut self, err: &CaptureError) -> bool {
        let pause = match err {
            CaptureError::Gone(_) => RECONNECT_INTERVAL,
            CaptureError::Failed(_) | CaptureError::Closed(_) => FAILED_RETRY,
        };
        if self.fail_first(IpcError {
            code: ErrorCode::Unavailable,
            message: err.to_string(),
        }) {
            return false;
        }
        if matches!(err, CaptureError::Closed(_)) {
            // Reconnecting would only ask the user again: stop and say why.
            self.set_problem(Some(err.to_string()));
            return false;
        }
        tracing::warn!("live capture: {err}; retrying in {pause:?}");
        self.sleep_until(Instant::now() + pause);
        true
    }

    /// Answers a waiting `start` with `err`; `true` when it was still waiting.
    fn fail_first(&mut self, err: IpcError) -> bool {
        match self.first.take() {
            Some(first) => {
                let _ = first.send(Err(err));
                true
            }
            None => false,
        }
    }

    /// Stores the frame for previews and sends the shaped one on, unless the steady gate
    /// holds it back. `false`: stop.
    fn deliver(&mut self, img: RgbImage) -> bool {
        let Shaping {
            mut options,
            steady,
            ..
        } = *self.shared.shaping.lock().expect("lock poisoned");
        let Some(img) = self.gate.offer(Arc::new(img), steady, Instant::now()) else {
            return true;
        };
        self.seq += 1;
        *self.shared.latest.lock().expect("lock poisoned") = Some((Arc::clone(&img), self.seq));
        let mut shaped = render_rgb_frame(&img, &options);
        if matches!(shaped, Err(RenderError::BadCrop { .. })) && options.crop.is_some() {
            if !self.warned_crop {
                tracing::warn!("the crop no longer fits the live source; showing all of it");
                self.warned_crop = true;
            }
            options.crop = None;
            shaped = render_rgb_frame(&img, &options);
        } else {
            self.warned_crop = false;
        }
        let frame = match shaped {
            Ok(frame) => frame,
            Err(err) => {
                tracing::warn!("cannot shape a live frame: {err}");
                return !self.fail_first(IpcError {
                    code: ErrorCode::Render,
                    message: err.to_string(),
                });
            }
        };
        if let Some(first) = self.first.take() {
            let size = Size {
                width: img.width(),
                height: img.height(),
            };
            let _ = first.send(Ok((frame, size)));
            return true;
        }
        !matches!(
            self.frames.try_send(frame),
            Err(TrySendError::Disconnected(_))
        )
    }

    fn sleep_until(&self, deadline: Instant) {
        while !self.stopped() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            thread::sleep(left.min(SLEEP_STEP));
        }
    }
}

/// A virtual desktop and the capture client on it; the client goes first when dropped.
struct DesktopCapturer {
    capture: Screencopy,
    desktop: VirtualDesktop,
    last_check: Instant,
}

impl DesktopCapturer {
    fn start(spec: &tt500_lcd_ipc::DesktopSpec, run_dir: &Path) -> Result<Self, CaptureError> {
        let desktop = VirtualDesktop::start(spec, run_dir).map_err(CaptureError::Failed)?;
        let capture = Screencopy::connect(desktop.socket_path(), None)?;
        Ok(Self {
            capture,
            desktop,
            last_check: Instant::now(),
        })
    }
}

impl Capturer for DesktopCapturer {
    fn capture(&mut self) -> Result<RgbImage, CaptureError> {
        if self.last_check.elapsed() >= SUPERVISE_INTERVAL {
            self.last_check = Instant::now();
            self.desktop.supervise().map_err(CaptureError::Gone)?;
        }
        self.capture.capture()
    }

    fn problem(&self) -> Option<String> {
        self.desktop.problem()
    }
}

/// The host compositor's socket, from the daemon's own environment.
fn host_wayland_socket() -> Result<PathBuf, IpcError> {
    let unavailable = |message: &str| IpcError {
        code: ErrorCode::Unavailable,
        message: message.to_owned(),
    };
    let display = env::var_os("WAYLAND_DISPLAY").ok_or_else(|| {
        unavailable(
            "WAYLAND_DISPLAY is not set in the daemon's environment; run \
             `systemctl --user import-environment WAYLAND_DISPLAY` and restart tt500-lcdd",
        )
    })?;
    let display = PathBuf::from(display);
    if display.is_absolute() {
        return Ok(display);
    }
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| unavailable("XDG_RUNTIME_DIR is not set in the daemon's environment"))?;
    Ok(PathBuf::from(runtime).join(display))
}

/// Alternates between two run directories so a new desktop can come up while the old one
/// is still being torn down.
static DESKTOP_GENERATION: AtomicUsize = AtomicUsize::new(0);

fn connector(
    source: &LiveSource,
    run_dir: &Path,
    state_dir: &Path,
    fps: u32,
) -> Result<Connect, IpcError> {
    match source {
        LiveSource::Desktop(spec) => {
            let spec = spec.clone();
            let dir = run_dir.join(format!(
                "desktop-{}",
                DESKTOP_GENERATION.fetch_add(1, Ordering::SeqCst) % 2
            ));
            Ok(Box::new(move || {
                let capturer = DesktopCapturer::start(&spec, &dir)?;
                Ok(Box::new(capturer) as Box<dyn Capturer>)
            }))
        }
        LiveSource::Screen { output } => {
            let socket = host_wayland_socket()?;
            let output = output.clone();
            Ok(Box::new(move || {
                let capturer = Screencopy::connect(&socket, output.as_deref())?;
                Ok(Box::new(capturer) as Box<dyn Capturer>)
            }))
        }
        LiveSource::Portal { reselect } => {
            if !portal::gstreamer_available() {
                return Err(IpcError {
                    code: ErrorCode::Unavailable,
                    message: "window capture needs GStreamer with the PipeWire plugin \
                              (gst-launch-1.0 and pipewiresrc, package gst-plugin-pipewire)"
                        .to_owned(),
                });
            }
            let tokens = portal::TokenFile::new(state_dir);
            let mut reselect = *reselect;
            let mut used = false;
            Ok(Box::new(move || {
                if std::mem::replace(&mut used, true) {
                    // Opening again would show the picker again.
                    return Err(CaptureError::Closed(
                        "the shared window or screen was closed; choose it again".to_owned(),
                    ));
                }
                let mut bus = portal::BusPortal::connect()?;
                let capturer = portal::connect(
                    &mut bus,
                    &tokens,
                    std::mem::take(&mut reselect),
                    |fd, node, size| portal::spawn_gst(fd, node, size, fps),
                )?;
                Ok(Box::new(capturer) as Box<dyn Capturer>)
            }))
        }
        _ => Err(IpcError {
            code: ErrorCode::Unsupported,
            message: "unsupported live source".to_owned(),
        }),
    }
}

/// Starts capturing `source` and waits for its first frame.
///
/// `run_dir` holds the virtual desktop's files, `state_dir` the portal's restore token. Returns the stream for the streamer, the
/// control for the server and the source size (the crop's coordinate space).
///
/// # Errors
///
/// [`ErrorCode::Unavailable`] when the source can't be reached (or the portal needs
/// GStreamer and has none) or gives no frame in time (15 s, 28 s for the portal).
pub fn start(
    source: LiveSource,
    options: RenderOptions,
    fps: u32,
    steady: bool,
    run_dir: &Path,
    state_dir: &Path,
) -> Result<(LiveStream, LiveControl, Size), IpcError> {
    let connect = connector(&source, run_dir, state_dir, fps)?;
    let timeout = if matches!(source, LiveSource::Portal { .. }) {
        PORTAL_FIRST_FRAME_TIMEOUT
    } else {
        FIRST_FRAME_TIMEOUT
    };
    LiveStream::spawn(persistent(&source), connect, options, fps, steady, timeout)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use tt500_lcd_render::Crop;

    use super::fake::FakeCapturer;
    use super::*;

    fn spec() -> LiveSource {
        LiveSource::Screen { output: None }
    }

    fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn count_frames(stream: &LiveStream, window: Duration) -> usize {
        let end = Instant::now() + window;
        let mut frames = 0;
        while Instant::now() < end {
            if matches!(stream.poll(), VideoPoll::Frame(_)) {
                frames += 1;
            }
            thread::sleep(Duration::from_millis(2));
        }
        frames
    }

    #[test]
    fn streams_at_its_fps_and_reports_the_source_size() {
        let fake = FakeCapturer::new(960, 256);
        let (stream, control, size) =
            LiveStream::from_capturer(spec(), fake, RenderOptions::default(), 20).unwrap();
        assert_eq!(
            size,
            Size {
                width: 960,
                height: 256
            }
        );
        assert_eq!(stream.frame_interval(), Duration::from_millis(50));
        let frames = count_frames(&stream, Duration::from_millis(600));
        // 20 fps for 0.6 s is at most about 12; debug-build shaping under load may be slower.
        assert!((2..=14).contains(&frames), "got {frames} frames");
        assert!(control.is_alive());
        assert!(control.latest().is_some());
    }

    #[test]
    fn set_options_keeps_the_worker_and_changes_shaping() {
        let fake = FakeCapturer::new(960, 256);
        let counter = fake.counter();
        let (stream, control, _) =
            LiveStream::from_capturer(spec(), fake, RenderOptions::default(), 30).unwrap();
        wait_for("a few captures", || counter.load(Ordering::SeqCst) >= 3);
        let (_, seq_before) = control.latest().unwrap();
        let options = RenderOptions {
            crop: Some(Crop {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            }),
            ..RenderOptions::default()
        };
        control.set_options(options, 10, false);
        assert_eq!(stream.frame_interval(), Duration::from_millis(100));
        wait_for("captures after the change", || {
            control
                .latest()
                .is_some_and(|(_, seq)| seq > seq_before + 1)
        });
        // The counter was not reset: the same capturer kept running.
        assert!(counter.load(Ordering::SeqCst) > 3);
        // A crop outside the source falls back to the whole frame instead of stalling.
        control.set_options(
            RenderOptions {
                crop: Some(Crop {
                    x: 5000,
                    y: 0,
                    width: 10,
                    height: 10,
                }),
                ..RenderOptions::default()
            },
            30,
            false,
        );
        let (_, seq) = control.latest().unwrap();
        wait_for("frames with a bad crop", || {
            control.latest().is_some_and(|(_, s)| s > seq + 1)
        });
        assert!(count_frames(&stream, Duration::from_millis(300)) > 0);
    }

    #[test]
    fn dropping_the_stream_joins_the_worker() {
        let fake = FakeCapturer::new(64, 32);
        let counter = fake.counter();
        let (stream, control, _) =
            LiveStream::from_capturer(spec(), fake, RenderOptions::default(), 30).unwrap();
        assert!(control.is_alive());
        drop(stream);
        assert!(!control.is_alive());
        let after = counter.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(150));
        assert_eq!(counter.load(Ordering::SeqCst), after);
    }

    #[test]
    fn a_vanished_source_is_reconnected() {
        /// Works for three frames, then is gone.
        struct Flaky(FakeCapturer, u32);
        impl Capturer for Flaky {
            fn capture(&mut self) -> Result<RgbImage, CaptureError> {
                if self.1 == 3 {
                    return Err(CaptureError::Gone("sway died".to_owned()));
                }
                self.1 += 1;
                self.0.capture()
            }
        }
        let connects = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&connects);
        let connect: Connect = Box::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Flaky(FakeCapturer::new(64, 32), 0)))
        });
        let (stream, control, _) = LiveStream::spawn(
            spec(),
            connect,
            RenderOptions::default(),
            30,
            false,
            FIRST_FRAME_TIMEOUT,
        )
        .unwrap();
        wait_for("a reconnect", || connects.load(Ordering::SeqCst) >= 2);
        assert!(control.is_alive());
        assert!(count_frames(&stream, Duration::from_millis(300)) > 0);
    }

    #[test]
    fn a_source_that_never_connects_fails_the_start() {
        let connect: Connect = Box::new(|| Err(CaptureError::Failed("no compositor".to_owned())));
        let err = LiveStream::spawn(
            spec(),
            connect,
            RenderOptions::default(),
            30,
            false,
            FIRST_FRAME_TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Unavailable);
        assert!(err.message.contains("no compositor"), "{}", err.message);
    }

    #[test]
    fn a_closed_source_ends_the_stream_with_its_reason_and_is_not_reopened() {
        let connects = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&connects);
        /// One frame, then gone.
        struct OneShot(FakeCapturer, bool);
        impl Capturer for OneShot {
            fn capture(&mut self) -> Result<RgbImage, CaptureError> {
                if std::mem::replace(&mut self.1, true) {
                    return Err(CaptureError::Gone("stream ended".to_owned()));
                }
                self.0.capture()
            }
        }
        let mut first = Some(OneShot(FakeCapturer::new(64, 32), false));
        let connect: Connect = Box::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            match first.take() {
                Some(capturer) => Ok(Box::new(capturer) as Box<dyn Capturer>),
                None => Err(CaptureError::Closed("closed by the user".to_owned())),
            }
        });
        let (stream, control, _) = LiveStream::spawn(
            spec(),
            connect,
            RenderOptions::default(),
            30,
            false,
            FIRST_FRAME_TIMEOUT,
        )
        .unwrap();
        wait_for("the worker to stop", || !control.is_alive());
        assert_eq!(connects.load(Ordering::SeqCst), 2);
        let mut ended = None;
        for _ in 0..50 {
            if let VideoPoll::Ended(reason) = stream.poll() {
                ended = Some(reason);
                break;
            }
        }
        assert_eq!(ended.as_deref(), Some("closed by the user"));
    }

    #[test]
    fn labels_name_the_source() {
        assert_eq!(
            label(&LiveSource::Screen {
                output: Some("DP-1".to_owned())
            }),
            "Screen DP-1"
        );
        assert_eq!(label(&spec()), "Screen");
    }
}
