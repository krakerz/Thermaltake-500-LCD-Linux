//! Video files: ffprobe for metadata, ffmpeg for decoding to raw 480x128 RGB frames.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, BufRead, BufReader, Read};
use std::mem;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use image::RgbImage;
use tt500_lcd_protocol::{Frame, HEIGHT, MIN_FRAME_INTERVAL, Rotation, WIDTH};

use crate::{Fit, MAX_FPS, RenderError, RenderOptions, canvas_size, fit};

/// How long to wait for the first decoded frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// Encoded frames queued ahead of the streamer; a full queue stalls ffmpeg.
const QUEUE_DEPTH: usize = 2;
/// Last ffmpeg stderr lines kept for error messages.
const STDERR_TAIL: usize = 8;
/// Longest stderr line kept, in characters.
const STDERR_LINE_CHARS: usize = 200;
/// Bytes in one raw RGB24 panel frame.
const FRAME_BYTES: usize = WIDTH as usize * HEIGHT as usize * 3;
/// Environment variable; `0` forces software decoding.
pub const HWDEC_ENV: &str = "TT500_LCD_HWDEC";
/// Longest loop (from the probe) whose encoded frames are kept for replay.
const CACHE_MAX_DURATION: Duration = Duration::from_secs(60);
/// Most encoded bytes kept for replay.
const CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
const FFMPEG: &str = "ffmpeg";
const FFPROBE: &str = "ffprobe";

/// What ffprobe reports about a video's first video stream.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoInfo {
    /// Displayed width in pixels (rotation metadata applied).
    pub width: u32,
    /// Displayed height in pixels (rotation metadata applied).
    pub height: u32,
    /// Average source frame rate, if known.
    pub fps: Option<f64>,
    /// Length of one loop, if known.
    pub duration: Option<Duration>,
}

impl VideoInfo {
    /// Frames per second shown on the panel: the source rate rounded up, at most `max_fps`
    /// (itself clamped to 1..=[`MAX_FPS`]); `max_fps` when the source rate is unknown.
    #[must_use]
    pub fn playback_fps(&self, max_fps: u32) -> u32 {
        let cap = max_fps.clamp(1, MAX_FPS);
        self.fps
            .filter(|f| f.is_finite() && *f > 0.0)
            .map_or(cap, |f| (f.ceil().min(f64::from(cap)) as u32).max(1))
    }

    /// Time between panel frames at the playback rate.
    #[must_use]
    pub fn frame_interval(&self, max_fps: u32) -> Duration {
        (Duration::from_secs(1) / self.playback_fps(max_fps)).max(MIN_FRAME_INTERVAL)
    }

    /// Frames in one loop at the playback rate, if the duration is known.
    #[must_use]
    pub fn frames_per_loop(&self, max_fps: u32) -> Option<u32> {
        self.duration
            .map(|d| (d.as_secs_f64() * f64::from(self.playback_fps(max_fps))).round() as u32)
    }
}

/// Probes `path` with ffprobe.
///
/// # Errors
/// [`RenderError::MissingFfmpeg`] if ffprobe is not installed, [`RenderError::Video`] if it
/// fails or finds no video stream.
pub fn probe(path: &Path) -> Result<VideoInfo, RenderError> {
    probe_with(OsStr::new(FFPROBE), path)
}

fn probe_with(program: &OsStr, path: &Path) -> Result<VideoInfo, RenderError> {
    let output = Command::new(program)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,avg_frame_rate:stream_tags=rotate:stream_side_data=rotation:format=duration",
            "-of",
            "default=noprint_wrappers=1",
            "-i",
        ])
        .arg(file_url(path))
        .stdin(Stdio::null())
        .output()
        .map_err(|err| spawn_error(&err))?;
    if !output.status.success() {
        return Err(RenderError::Video(last_lines(&String::from_utf8_lossy(
            &output.stderr,
        ))));
    }
    parse_probe(&String::from_utf8_lossy(&output.stdout))
}

fn spawn_error(err: &io::Error) -> RenderError {
    if err.kind() == io::ErrorKind::NotFound {
        RenderError::MissingFfmpeg
    } else {
        RenderError::Video(format!("cannot start ffmpeg: {err}"))
    }
}

fn thread_error(err: &io::Error) -> RenderError {
    RenderError::Video(format!("cannot start a thread: {err}"))
}

/// `file:` keeps ffmpeg from reading a path as a protocol URL.
fn file_url(path: &Path) -> OsString {
    let mut url = OsString::from("file:");
    url.push(path);
    url
}

fn cut(line: &str) -> String {
    line.chars().take(STDERR_LINE_CHARS).collect()
}

/// The last three non-empty lines of ffprobe's stderr.
fn last_lines(text: &str) -> String {
    let lines: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(cut)
        .collect();
    if lines.is_empty() {
        return "ffprobe failed".to_owned();
    }
    lines[lines.len().saturating_sub(3)..].join("; ")
}

fn parse_rate(text: &str) -> Option<f64> {
    let rate = match text.split_once('/') {
        Some((num, den)) => {
            let den: f64 = den.trim().parse().ok()?;
            if den == 0.0 {
                return None;
            }
            num.trim().parse::<f64>().ok()? / den
        }
        None => text.trim().parse().ok()?,
    };
    (rate.is_finite() && rate > 0.0).then_some(rate)
}

fn parse_probe(text: &str) -> Result<VideoInfo, RenderError> {
    let (mut width, mut height, mut fps, mut duration, mut rotation) = (None, None, None, None, 0);
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        match key {
            "width" => width = value.parse::<u32>().ok().filter(|v| *v > 0),
            "height" => height = value.parse::<u32>().ok().filter(|v| *v > 0),
            "avg_frame_rate" => fps = parse_rate(value),
            "duration" => {
                duration = value
                    .parse::<f64>()
                    .ok()
                    .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
                    .filter(|d| !d.is_zero());
            }
            "rotation" | "TAG:rotate" => {
                if let Ok(degrees) = value.parse::<f64>() {
                    rotation = degrees.round() as i64;
                }
            }
            _ => {}
        }
    }
    let (Some(width), Some(height)) = (width, height) else {
        return Err(RenderError::Video("no video stream found".into()));
    };
    let (width, height) = if rotation.rem_euclid(180) == 90 {
        (height, width)
    } else {
        (width, height)
    };
    Ok(VideoInfo {
        width,
        height,
        fps,
        duration,
    })
}

/// The scale and the optional crop or pad that fit a `w`x`h` picture on the canvas.
fn fit_steps(w: u32, h: u32, options: &RenderOptions) -> ((u32, u32), Option<String>) {
    let target = canvas_size(options.rotation);
    let (pw, ph) = target;
    match options.fit {
        Fit::Stretch => (target, None),
        Fit::Cover => {
            let (sw, sh) = fit::scaled_dims(w, h, true, target);
            let post = format!("crop={pw}:{ph}:{}:{}", (sw - pw) / 2, (sh - ph) / 2);
            ((sw, sh), Some(post))
        }
        Fit::Contain => {
            let (sw, sh) = fit::scaled_dims(w, h, false, target);
            let [r, g, b] = options.background;
            let post = format!(
                "pad={pw}:{ph}:{}:{}:color=0x{r:02x}{g:02x}{b:02x}",
                (pw - sw) / 2,
                (ph - sh) / 2
            );
            ((sw, sh), Some(post))
        }
    }
}

/// The ffmpeg `-vf` chain: rate cap, optional crop, fit, then raw RGB24.
///
/// Portrait steps decode at 128x480; `pump` turns each frame into the panel's 480x128. With
/// `hardware` the frames are VA-API surfaces: without a crop `scale_vaapi` does the scaling on
/// the GPU and only the small scaled picture is downloaded; with a crop the full picture is
/// downloaded first, as the crop is exact only in software.
fn filter_graph(info: &VideoInfo, options: &RenderOptions, hardware: bool) -> String {
    let mut parts = vec![format!(
        "fps={}",
        info.playback_fps(options.video_fps_cap())
    )];
    let (w, h) = match options.crop {
        Some(c) => (c.width, c.height),
        None => (info.width, info.height),
    };
    let ((sw, sh), post) = fit_steps(w, h, options);
    if hardware && options.crop.is_none() {
        parts.push(format!("scale_vaapi=w={sw}:h={sh}:format=nv12"));
        // Crop and pad run on RGB, where their offsets and colour are exact.
        parts.extend(["hwdownload", "format=nv12", "format=rgb24"].map(String::from));
    } else {
        if hardware {
            parts.extend(["hwdownload", "format=nv12"].map(String::from));
        }
        if let Some(c) = options.crop {
            parts.push(format!(
                "crop={}:{}:{}:{}:exact=1",
                c.width, c.height, c.x, c.y
            ));
        }
        parts.push(format!("scale={sw}:{sh}"));
    }
    parts.extend(post);
    parts.push("setsar=1".into());
    parts.push("format=rgb24".into());
    parts.join(",")
}

fn ffmpeg_args(
    path: &Path,
    info: &VideoInfo,
    options: &RenderOptions,
    vaapi: Option<&Path>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-hide_banner", "-nostdin", "-nostats", "-loglevel", "error"]
        .into_iter()
        .map(OsString::from)
        .collect();
    if let Some(device) = vaapi {
        args.extend(
            ["-hwaccel", "vaapi", "-hwaccel_output_format", "vaapi"]
                .into_iter()
                .map(OsString::from),
        );
        args.push("-hwaccel_device".into());
        args.push(device.into());
    }
    args.extend(["-stream_loop", "-1", "-i"].into_iter().map(OsString::from));
    args.push(file_url(path));
    let filter = filter_graph(info, options, vaapi.is_some());
    args.extend(
        [
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-vf",
            filter.as_str(),
            "-pix_fmt",
            "rgb24",
            "-f",
            "rawvideo",
            "pipe:1",
        ]
        .into_iter()
        .map(OsString::from),
    );
    args
}

/// Whether the environment value (`TT500_LCD_HWDEC`) allows hardware decoding.
fn hwdec_enabled(value: Option<&OsStr>) -> bool {
    value.is_none_or(|v| {
        !["0", "false", "off", "no"]
            .iter()
            .any(|off| v.eq_ignore_ascii_case(off))
    })
}

/// The first DRM render node (`/dev/dri/renderD*`), if any.
fn vaapi_device() -> Option<PathBuf> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
        .map(|entry| entry.path())
        .collect();
    nodes.sort();
    nodes.into_iter().next()
}

/// How a video is decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodePath {
    /// VA-API hardware decoding and scaling.
    Vaapi,
    /// Software decoding, by choice or because there is no render node.
    Software,
    /// Software decoding after the VA-API attempt failed, with the reason.
    SoftwareFallback(String),
}

impl fmt::Display for DecodePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vaapi => f.write_str("VA-API hardware decoding"),
            Self::Software => f.write_str("software decoding"),
            Self::SoftwareFallback(reason) => {
                write!(f, "software decoding (VA-API failed: {reason})")
            }
        }
    }
}

/// Kills and reaps ffmpeg when dropped, so neither the process nor a zombie outlives the stream.
#[derive(Debug, Default)]
struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            // Errors mean it already exited; wait() still reaps it. SIGKILL makes wait() immediate.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Debug, Default)]
struct Process {
    child: ChildGuard,
    stderr: Option<JoinHandle<()>>,
    tail: Arc<Mutex<VecDeque<String>>>,
}

/// What [`VideoStream::poll`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoPoll {
    /// The next frame.
    Frame(Frame),
    /// The decoder has not produced the next frame yet.
    Pending,
    /// The decoder stopped (ffmpeg failed or the raw stream ended), with the reason.
    Ended(String),
}

/// A running decoder producing panel-ready frames; dropping it kills and reaps ffmpeg.
#[derive(Debug)]
pub struct VideoStream {
    first: Frame,
    interval: Duration,
    decode: DecodePath,
    state: Box<RefCell<State>>,
}

/// Where polled frames come from.
#[derive(Debug)]
enum State {
    /// ffmpeg is running; `cache` collects the first loop while it still fits.
    Live {
        frames: Receiver<Result<Frame, String>>,
        process: Process,
        cache: Option<LoopCache>,
    },
    /// The first loop is cached and ffmpeg is gone; `next` indexes the frame to play next.
    Replay { frames: Vec<Frame>, next: usize },
}

/// The encoded frames of the first loop, starting with the first frame.
#[derive(Debug)]
struct LoopCache {
    /// Frames in one loop.
    target: usize,
    max_bytes: usize,
    bytes: usize,
    frames: Vec<Frame>,
}

impl LoopCache {
    /// A cache seeded with `first`, or `None` when it alone exceeds `max_bytes`.
    fn new(target: usize, max_bytes: usize, first: &Frame) -> Option<Self> {
        let bytes = first.as_bytes().len();
        (target > 0 && bytes <= max_bytes).then(|| Self {
            target,
            max_bytes,
            bytes,
            frames: vec![first.clone()],
        })
    }

    /// Adds `frame`; `false` when the loop no longer fits and the cache must be dropped.
    fn push(&mut self, frame: &Frame) -> bool {
        self.bytes += frame.as_bytes().len();
        self.frames.push(frame.clone());
        self.bytes <= self.max_bytes
    }

    fn is_full(&self) -> bool {
        self.frames.len() >= self.target
    }
}

/// How many frames of the first loop to keep, from the probe.
#[derive(Debug, Clone, Copy)]
struct CachePlan {
    frames: usize,
    max_bytes: usize,
}

impl CachePlan {
    fn for_video(info: &VideoInfo, max_fps: u32) -> Option<Self> {
        info.duration.filter(|d| *d <= CACHE_MAX_DURATION)?;
        let frames = usize::try_from(info.frames_per_loop(max_fps)?).ok()?;
        (frames > 0).then_some(Self {
            frames,
            max_bytes: CACHE_MAX_BYTES,
        })
    }
}

const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<VideoStream>();
};

impl VideoStream {
    /// Starts ffmpeg on `path` and waits for the first frame.
    ///
    /// Tries VA-API first (unless `TT500_LCD_HWDEC=0` or there is no render node) and falls back
    /// to software decoding for this file if that fails to produce a frame. A video of at most
    /// 60 s whose encoded loop fits 64 MiB is cached during its first loop and then replayed
    /// without ffmpeg.
    ///
    /// # Errors
    /// [`RenderError::BadCrop`] (checked before spawning), [`RenderError::MissingFfmpeg`], or
    /// [`RenderError::Video`] with the tail of ffmpeg's stderr.
    pub fn spawn(
        path: &Path,
        info: &VideoInfo,
        options: &RenderOptions,
    ) -> Result<Self, RenderError> {
        if let Some(crop) = options.crop {
            crop.check(info.width, info.height)?;
        }
        let mut decode = DecodePath::Software;
        if hwdec_enabled(std::env::var_os(HWDEC_ENV).as_deref())
            && let Some(device) = vaapi_device()
        {
            match Self::spawn_ffmpeg(path, info, options, Some(&device), DecodePath::Vaapi) {
                Ok(stream) => return Ok(stream),
                Err(err @ RenderError::MissingFfmpeg) => return Err(err),
                Err(err) => decode = DecodePath::SoftwareFallback(err.to_string()),
            }
        }
        Self::spawn_ffmpeg(path, info, options, None, decode)
    }

    fn spawn_ffmpeg(
        path: &Path,
        info: &VideoInfo,
        options: &RenderOptions,
        vaapi: Option<&Path>,
        decode: DecodePath,
    ) -> Result<Self, RenderError> {
        // Never inherit a stdin; ffmpeg gets killed when the stream drops. If this process is
        // killed instead, ffmpeg dies of SIGPIPE at its next write to the closed pipe.
        let mut child = Command::new(FFMPEG)
            .args(ffmpeg_args(path, info, options, vaapi))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| spawn_error(&err))?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let mut process = Process {
            child: ChildGuard(Some(child)),
            ..Process::default()
        };
        process.stderr = Some(spawn_stderr_reader(stderr, Arc::clone(&process.tail))?);
        Self::start(
            stdout,
            info.frame_interval(options.video_fps_cap()),
            options.quality,
            options.rotation,
            process,
            CachePlan::for_video(info, options.video_fps_cap()),
            decode,
        )
    }

    /// Plays raw 480x128 RGB24 frames (rotation step 0) from `reader` (the decoder seam; tests use a `Cursor`).
    ///
    /// `frame_interval` is raised to `MIN_FRAME_INTERVAL`. Waits for the first frame.
    ///
    /// # Errors
    /// [`RenderError::Video`] if no frame arrives.
    pub fn from_raw_rgb(
        reader: impl Read + Send + 'static,
        frame_interval: Duration,
        quality: u8,
    ) -> Result<Self, RenderError> {
        Self::start(
            reader,
            frame_interval.max(MIN_FRAME_INTERVAL),
            quality,
            Rotation::default(),
            Process::default(),
            None,
            DecodePath::Software,
        )
    }

    fn start(
        reader: impl Read + Send + 'static,
        interval: Duration,
        quality: u8,
        rotation: Rotation,
        process: Process,
        plan: Option<CachePlan>,
        decode: DecodePath,
    ) -> Result<Self, RenderError> {
        let (sender, frames) = mpsc::sync_channel(QUEUE_DEPTH);
        thread::Builder::new()
            .name("video-decode".into())
            .spawn(move || pump(reader, &sender, quality, rotation))
            .map_err(|err| thread_error(&err))?;
        match frames.recv_timeout(FIRST_FRAME_TIMEOUT) {
            Ok(Ok(first)) => {
                let cache = plan.and_then(|p| LoopCache::new(p.frames, p.max_bytes, &first));
                Ok(Self {
                    first,
                    interval,
                    decode,
                    state: Box::new(RefCell::new(State::Live {
                        frames,
                        process,
                        cache,
                    })),
                })
            }
            other => {
                let reason = match other {
                    Ok(Err(reason)) => reason,
                    Err(RecvTimeoutError::Timeout) => format!(
                        "no frame decoded within {} s",
                        FIRST_FRAME_TIMEOUT.as_secs()
                    ),
                    _ => "the decoder stopped".to_owned(),
                };
                let Process {
                    child,
                    stderr,
                    tail,
                } = process;
                // Kill and reap ffmpeg first so its stderr reaches EOF and the tail is complete.
                drop(child);
                if let Some(handle) = stderr {
                    let _ = handle.join();
                }
                Err(RenderError::Video(with_tail(&reason, &tail)))
            }
        }
    }

    /// The first decoded frame, available as soon as the stream exists.
    #[must_use]
    pub fn first_frame(&self) -> &Frame {
        &self.first
    }

    /// Time between frames.
    #[must_use]
    pub fn frame_interval(&self) -> Duration {
        self.interval
    }

    /// Returns the next frame if it is ready; never blocks.
    pub fn poll(&self) -> VideoPoll {
        let mut state = self.state.borrow_mut();
        match &mut *state {
            State::Replay { frames, next } => {
                let frame = frames[*next].clone();
                *next = (*next + 1) % frames.len();
                VideoPoll::Frame(frame)
            }
            State::Live {
                frames,
                process,
                cache,
            } => match frames.try_recv() {
                Ok(Ok(frame)) => {
                    if let Some(kept) = cache
                        && !kept.push(&frame)
                    {
                        *cache = None;
                    }
                    if let Some(kept) = cache
                        && kept.is_full()
                    {
                        // The first loop is complete: dropping the state stops ffmpeg.
                        let frames = mem::take(&mut kept.frames);
                        *state = State::Replay { frames, next: 0 };
                    }
                    VideoPoll::Frame(frame)
                }
                Ok(Err(reason)) => VideoPoll::Ended(with_tail(&reason, &process.tail)),
                Err(TryRecvError::Empty) => VideoPoll::Pending,
                Err(TryRecvError::Disconnected) => {
                    VideoPoll::Ended(with_tail("the decoder stopped", &process.tail))
                }
            },
        }
    }

    /// ffmpeg's pid, `None` for [`VideoStream::from_raw_rgb`] and once a cached loop replays.
    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        match &*self.state.borrow() {
            State::Live { process, .. } => process.child.0.as_ref().map(Child::id),
            State::Replay { .. } => None,
        }
    }

    /// How this stream is decoded (for logging).
    #[must_use]
    pub fn decode_path(&self) -> &DecodePath {
        &self.decode
    }
}

fn spawn_stderr_reader(
    stderr: ChildStderr,
    tail: Arc<Mutex<VecDeque<String>>>,
) -> Result<JoinHandle<()>, RenderError> {
    thread::Builder::new()
        .name("ffmpeg-stderr".into())
        .spawn(move || {
            for line in BufReader::new(stderr).split(b'\n') {
                let Ok(line) = line else { break };
                let line = String::from_utf8_lossy(&line);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let mut tail = tail.lock().expect("lock poisoned");
                tail.push_back(cut(line));
                if tail.len() > STDERR_TAIL {
                    tail.pop_front();
                }
            }
        })
        .map_err(|err| thread_error(&err))
}

fn with_tail(reason: &str, tail: &Mutex<VecDeque<String>>) -> String {
    let tail = tail.lock().expect("lock poisoned");
    if tail.is_empty() {
        reason.to_owned()
    } else {
        let lines: Vec<&str> = tail.iter().map(String::as_str).collect();
        format!("{reason}: {}", lines.join("; "))
    }
}

/// Reads raw frames, encodes them and queues them; blocks when the queue is full (backpressure).
fn pump(
    mut reader: impl Read,
    frames: &SyncSender<Result<Frame, String>>,
    quality: u8,
    rotation: Rotation,
) {
    let (width, height) = canvas_size(rotation);
    let mut previous: Option<Frame> = None;
    loop {
        let mut raw = vec![0u8; FRAME_BYTES];
        if let Err(err) = reader.read_exact(&mut raw) {
            let reason = if err.kind() == io::ErrorKind::UnexpectedEof {
                "end of stream".to_owned()
            } else {
                format!("cannot read frames: {err}")
            };
            let _ = frames.send(Err(reason));
            return;
        }
        let rgb = RgbImage::from_raw(width, height, raw).expect("buffer holds exactly one frame");
        let frame = match crate::render_canvas_rgb(rgb, rotation, quality) {
            Ok(frame) => frame,
            // Too detailed even at the lowest quality: repeat the previous frame to keep pacing.
            Err(RenderError::TooComplex) => match &previous {
                Some(frame) => frame.clone(),
                None => continue,
            },
            Err(err) => {
                let _ = frames.send(Err(err.to_string()));
                return;
            }
        };
        previous = Some(frame.clone());
        if frames.send(Ok(frame)).is_err() {
            return; // the stream was dropped
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Instant;

    use super::*;
    use crate::Crop;

    fn info(fps: Option<f64>) -> VideoInfo {
        VideoInfo {
            width: 1920,
            height: 1080,
            fps,
            duration: None,
        }
    }

    #[test]
    fn probe_output_is_parsed_with_rotation() {
        let parsed = parse_probe(
            "width=1920\nheight=1080\navg_frame_rate=30000/1001\nrotation=-90\nduration=12.500000\n",
        )
        .unwrap();
        assert_eq!((parsed.width, parsed.height), (1080, 1920));
        assert!((parsed.fps.unwrap() - 29.97).abs() < 0.01);
        assert_eq!(parsed.duration, Some(Duration::from_millis(12_500)));
    }

    #[test]
    fn probe_output_handles_unknowns() {
        let parsed =
            parse_probe("width=640\nheight=360\navg_frame_rate=0/0\nduration=N/A\n").unwrap();
        assert_eq!((parsed.width, parsed.height), (640, 360));
        assert_eq!((parsed.fps, parsed.duration), (None, None));
        let rotated = parse_probe("width=640\nheight=360\nTAG:rotate=90\n").unwrap();
        assert_eq!((rotated.width, rotated.height), (360, 640));
        assert!(matches!(
            parse_probe("height=360\n"),
            Err(RenderError::Video(_))
        ));
    }

    #[test]
    fn playback_rate_is_capped() {
        assert_eq!(info(None).playback_fps(15), 15);
        assert_eq!(info(Some(29.97)).playback_fps(15), 15);
        assert_eq!(info(Some(29.97)).playback_fps(30), 30);
        assert_eq!(info(Some(60.0)).playback_fps(99), 30);
        assert_eq!(info(Some(10.0)).playback_fps(15), 10);
        assert_eq!(info(Some(10.0)).playback_fps(30), 10);
        assert_eq!(info(Some(0.5)).playback_fps(15), 1);
        assert_eq!(info(None).playback_fps(0), 1);
        assert_eq!(
            info(None).frame_interval(15),
            Duration::from_nanos(66_666_666)
        );
        assert_eq!(info(None).frame_interval(30), MIN_FRAME_INTERVAL);
        let clip = VideoInfo {
            duration: Some(Duration::from_millis(12_500)),
            ..info(None)
        };
        assert_eq!(clip.frames_per_loop(15), Some(188));
    }

    #[test]
    fn filter_graphs() {
        let info = info(Some(30.0));
        assert_eq!(
            filter_graph(&info, &RenderOptions::default(), false),
            "fps=15,scale=480:270,crop=480:128:0:71,setsar=1,format=rgb24"
        );
        let contain = RenderOptions {
            fit: Fit::Contain,
            background: [1, 2, 3],
            crop: Some(Crop {
                x: 100,
                y: 50,
                width: 400,
                height: 400,
            }),
            ..RenderOptions::default()
        };
        assert_eq!(
            filter_graph(&info, &contain, false),
            "fps=15,crop=400:400:100:50:exact=1,scale=128:128,pad=480:128:176:0:color=0x010203,setsar=1,format=rgb24"
        );
        let portrait = RenderOptions {
            rotation: Rotation::try_from(1).unwrap(),
            ..RenderOptions::default()
        };
        assert_eq!(
            filter_graph(&info, &portrait, false),
            "fps=15,scale=854:480,crop=128:480:363:0,setsar=1,format=rgb24"
        );
        let stretch = RenderOptions {
            fit: Fit::Stretch,
            ..RenderOptions::default()
        };
        assert_eq!(
            filter_graph(&info, &stretch, false),
            "fps=15,scale=480:128,setsar=1,format=rgb24"
        );
    }

    #[test]
    fn ffmpeg_args_loop_and_use_file_urls() {
        let args = ffmpeg_args(
            Path::new("/v/a b.mp4"),
            &info(Some(30.0)),
            &RenderOptions::default(),
            None,
        );
        let args: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
        assert!(args.windows(2).any(|w| w == ["-stream_loop", "-1"]));
        assert!(args.contains(&"file:/v/a b.mp4"));
        assert!(!args.contains(&"-hwaccel"));
        assert_eq!(args.last(), Some(&"pipe:1"));
    }

    #[test]
    fn hardware_graphs_scale_on_the_gpu_and_match_the_fit() {
        let info = info(Some(30.0));
        assert_eq!(
            filter_graph(&info, &RenderOptions::default(), true),
            "fps=15,scale_vaapi=w=480:h=270:format=nv12,hwdownload,format=nv12,format=rgb24,crop=480:128:0:71,setsar=1,format=rgb24"
        );
        let stretch = RenderOptions {
            fit: Fit::Stretch,
            ..RenderOptions::default()
        };
        assert_eq!(
            filter_graph(&info, &stretch, true),
            "fps=15,scale_vaapi=w=480:h=128:format=nv12,hwdownload,format=nv12,format=rgb24,setsar=1,format=rgb24"
        );
        let portrait = RenderOptions {
            rotation: Rotation::try_from(1).unwrap(),
            ..RenderOptions::default()
        };
        assert_eq!(
            filter_graph(&info, &portrait, true),
            "fps=15,scale_vaapi=w=854:h=480:format=nv12,hwdownload,format=nv12,format=rgb24,crop=128:480:363:0,setsar=1,format=rgb24"
        );
        // A crop is exact only in software: download first, then the software chain.
        let contain = RenderOptions {
            fit: Fit::Contain,
            background: [1, 2, 3],
            crop: Some(Crop {
                x: 100,
                y: 50,
                width: 400,
                height: 400,
            }),
            ..RenderOptions::default()
        };
        assert_eq!(
            filter_graph(&info, &contain, true),
            "fps=15,hwdownload,format=nv12,crop=400:400:100:50:exact=1,scale=128:128,pad=480:128:176:0:color=0x010203,setsar=1,format=rgb24"
        );
    }

    #[test]
    fn hardware_args_select_vaapi_before_the_input() {
        let args = ffmpeg_args(
            Path::new("/v/a.mp4"),
            &info(Some(30.0)),
            &RenderOptions::default(),
            Some(Path::new("/dev/dri/renderD128")),
        );
        let args: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
        let input = args.iter().position(|a| *a == "-i").unwrap();
        let hw = args.iter().position(|a| *a == "-hwaccel").unwrap();
        assert!(hw < input);
        assert_eq!(
            &args[hw..hw + 6],
            [
                "-hwaccel",
                "vaapi",
                "-hwaccel_output_format",
                "vaapi",
                "-hwaccel_device",
                "/dev/dri/renderD128"
            ]
        );
        assert!(args.iter().any(|a| a.contains("scale_vaapi")));
    }

    #[test]
    fn hwdec_env_can_force_software() {
        assert!(hwdec_enabled(None));
        assert!(hwdec_enabled(Some(OsStr::new("1"))));
        assert!(hwdec_enabled(Some(OsStr::new("auto"))));
        assert!(!hwdec_enabled(Some(OsStr::new("0"))));
        assert!(!hwdec_enabled(Some(OsStr::new("off"))));
    }

    /// An endless source of distinct grey frames that records when it is dropped.
    struct Endless {
        n: u8,
        dropped: Arc<AtomicBool>,
    }

    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            buf.fill(self.n.wrapping_mul(37));
            self.n = self.n.wrapping_add(1);
            Ok(buf.len())
        }
    }

    impl Drop for Endless {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    fn endless_stream(plan: CachePlan) -> (VideoStream, Arc<AtomicBool>) {
        let dropped = Arc::new(AtomicBool::new(false));
        let reader = Endless {
            n: 0,
            dropped: Arc::clone(&dropped),
        };
        let stream = VideoStream::start(
            reader,
            Duration::from_millis(70),
            90,
            Rotation::default(),
            Process::default(),
            Some(plan),
            DecodePath::Software,
        )
        .unwrap();
        (stream, dropped)
    }

    fn next_frame(stream: &VideoStream) -> Frame {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match stream.poll() {
                VideoPoll::Frame(frame) => return frame,
                VideoPoll::Pending => {
                    assert!(Instant::now() < deadline, "no frame in 5 s");
                    thread::sleep(Duration::from_millis(2));
                }
                VideoPoll::Ended(reason) => panic!("ended: {reason}"),
            }
        }
    }

    #[test]
    fn first_loop_is_cached_then_replayed_without_the_decoder() {
        const LOOP: usize = 4;
        let (stream, dropped) = endless_stream(CachePlan {
            frames: LOOP,
            max_bytes: 1 << 20,
        });
        let interval = stream.frame_interval();
        let mut pass = vec![stream.first_frame().clone()];
        pass.extend((1..LOOP).map(|_| next_frame(&stream)));
        // The loop is complete: the decoder (and its reader) is dropped.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !dropped.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "decoder was not dropped");
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(stream.process_id(), None);
        for round in 0..3 {
            for (i, expected) in pass.iter().enumerate() {
                assert_eq!(next_frame(&stream), *expected, "round {round} frame {i}");
            }
        }
        // Replay keeps the first pass's pacing.
        assert_eq!(stream.frame_interval(), interval);
    }

    #[test]
    fn oversize_loops_are_not_cached() {
        let (stream, dropped) = endless_stream(CachePlan {
            frames: 4,
            max_bytes: 64,
        });
        let frames: Vec<Frame> = (0..8).map(|_| next_frame(&stream)).collect();
        assert!(frames.windows(2).all(|w| w[0] != w[1]), "keeps decoding");
        assert!(!dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn cache_plan_needs_a_short_known_loop() {
        let mut clip = VideoInfo {
            duration: Some(Duration::from_secs(10)),
            ..info(Some(30.0))
        };
        let plan = CachePlan::for_video(&clip, 15).unwrap();
        assert_eq!((plan.frames, plan.max_bytes), (150, 64 * 1024 * 1024));
        clip.duration = Some(Duration::from_secs(61));
        assert!(CachePlan::for_video(&clip, 15).is_none());
        clip.duration = None;
        assert!(CachePlan::for_video(&clip, 15).is_none());
    }

    #[test]
    fn missing_ffprobe_is_reported() {
        let err =
            probe_with(OsStr::new("tt500-lcd-no-such-ffprobe"), Path::new("/x.mp4")).unwrap_err();
        assert!(matches!(err, RenderError::MissingFfmpeg));
    }

    #[test]
    fn bad_crop_is_rejected_before_spawning() {
        let source = VideoInfo {
            width: 100,
            height: 100,
            fps: None,
            duration: None,
        };
        let options = RenderOptions {
            crop: Some(Crop {
                x: 50,
                y: 0,
                width: 60,
                height: 10,
            }),
            ..RenderOptions::default()
        };
        let err = VideoStream::spawn(Path::new("/nonexistent.mp4"), &source, &options).unwrap_err();
        assert!(matches!(err, RenderError::BadCrop { .. }));
    }

    #[test]
    fn raw_frames_are_encoded_and_end_cleanly() {
        let raw: Vec<u8> = [40u8, 120, 200]
            .into_iter()
            .flat_map(|grey| vec![grey; FRAME_BYTES])
            .collect();
        let stream =
            VideoStream::from_raw_rgb(Cursor::new(raw), Duration::from_millis(70), 90).unwrap();
        Frame::from_jpeg(stream.first_frame().as_bytes().to_vec()).unwrap();
        assert_eq!(stream.frame_interval(), Duration::from_millis(70));
        assert_eq!(stream.process_id(), None);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = 0;
        loop {
            match stream.poll() {
                VideoPoll::Frame(_) => got += 1,
                VideoPoll::Pending => {
                    assert!(Instant::now() < deadline, "timed out after {got} frames");
                    thread::sleep(Duration::from_millis(5));
                }
                VideoPoll::Ended(reason) => {
                    assert_eq!(reason, "end of stream");
                    break;
                }
            }
        }
        assert_eq!(got, 2);
    }

    #[test]
    fn empty_stream_fails_and_interval_has_a_floor() {
        let err =
            VideoStream::from_raw_rgb(Cursor::new(Vec::new()), Duration::ZERO, 90).unwrap_err();
        assert!(matches!(err, RenderError::Video(m) if m == "end of stream"));
        let raw = vec![0u8; FRAME_BYTES];
        let stream =
            VideoStream::from_raw_rgb(Cursor::new(raw), Duration::from_millis(1), 90).unwrap();
        assert_eq!(stream.frame_interval(), MIN_FRAME_INTERVAL);
    }
}
