//! Video files: ffprobe for metadata, ffmpeg for decoding to raw 480x128 RGB frames.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use image::RgbImage;
use tt500_lcd_protocol::{Frame, HEIGHT, MIN_FRAME_INTERVAL, WIDTH};

use crate::{Fit, MAX_FPS, RenderError, RenderOptions, fit};

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

/// The ffmpeg `-vf` chain: rate cap, optional crop, fit, then raw RGB24.
fn filter_graph(info: &VideoInfo, options: &RenderOptions) -> String {
    let (pw, ph) = (u32::from(WIDTH), u32::from(HEIGHT));
    let mut parts = vec![format!(
        "fps={}",
        info.playback_fps(options.video_fps_cap())
    )];
    let (w, h) = match options.crop {
        Some(c) => {
            parts.push(format!(
                "crop={}:{}:{}:{}:exact=1",
                c.width, c.height, c.x, c.y
            ));
            (c.width, c.height)
        }
        None => (info.width, info.height),
    };
    match options.fit {
        Fit::Stretch => parts.push(format!("scale={pw}:{ph}")),
        Fit::Cover => {
            let (sw, sh) = fit::scaled_dims(w, h, true);
            parts.push(format!(
                "scale={sw}:{sh},crop={pw}:{ph}:{}:{}",
                (sw - pw) / 2,
                (sh - ph) / 2
            ));
        }
        Fit::Contain => {
            let (sw, sh) = fit::scaled_dims(w, h, false);
            let [r, g, b] = options.background;
            parts.push(format!(
                "scale={sw}:{sh},pad={pw}:{ph}:{}:{}:color=0x{r:02x}{g:02x}{b:02x}",
                (pw - sw) / 2,
                (ph - sh) / 2
            ));
        }
    }
    parts.push("setsar=1".into());
    parts.push("format=rgb24".into());
    parts.join(",")
}

fn ffmpeg_args(path: &Path, info: &VideoInfo, options: &RenderOptions) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-hide_banner",
        "-nostdin",
        "-nostats",
        "-loglevel",
        "error",
        "-stream_loop",
        "-1",
        "-i",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(file_url(path));
    let filter = filter_graph(info, options);
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
    frames: Receiver<Result<Frame, String>>,
    first: Frame,
    interval: Duration,
    process: Process,
}

const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<VideoStream>();
};

impl VideoStream {
    /// Starts ffmpeg on `path` and waits for the first frame.
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
        let mut child = Command::new(FFMPEG)
            .args(ffmpeg_args(path, info, options))
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
            process,
        )
    }

    /// Plays raw 480x128 RGB24 frames from `reader` (the decoder seam; tests use a `Cursor`).
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
            Process::default(),
        )
    }

    fn start(
        reader: impl Read + Send + 'static,
        interval: Duration,
        quality: u8,
        process: Process,
    ) -> Result<Self, RenderError> {
        let (sender, frames) = mpsc::sync_channel(QUEUE_DEPTH);
        thread::Builder::new()
            .name("video-decode".into())
            .spawn(move || pump(reader, &sender, quality))
            .map_err(|err| thread_error(&err))?;
        match frames.recv_timeout(FIRST_FRAME_TIMEOUT) {
            Ok(Ok(first)) => Ok(Self {
                frames,
                first,
                interval,
                process,
            }),
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
        match self.frames.try_recv() {
            Ok(Ok(frame)) => VideoPoll::Frame(frame),
            Ok(Err(reason)) => VideoPoll::Ended(with_tail(&reason, &self.process.tail)),
            Err(TryRecvError::Empty) => VideoPoll::Pending,
            Err(TryRecvError::Disconnected) => {
                VideoPoll::Ended(with_tail("the decoder stopped", &self.process.tail))
            }
        }
    }

    /// ffmpeg's pid, `None` for [`VideoStream::from_raw_rgb`] (logging, tests).
    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        self.process.child.0.as_ref().map(Child::id)
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
fn pump(mut reader: impl Read, frames: &SyncSender<Result<Frame, String>>, quality: u8) {
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
        let rgb = RgbImage::from_raw(u32::from(WIDTH), u32::from(HEIGHT), raw)
            .expect("buffer holds exactly one frame");
        let frame = match crate::render_panel_rgb(rgb, quality) {
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
            filter_graph(&info, &RenderOptions::default()),
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
            filter_graph(&info, &contain),
            "fps=15,crop=400:400:100:50:exact=1,scale=128:128,pad=480:128:176:0:color=0x010203,setsar=1,format=rgb24"
        );
        let stretch = RenderOptions {
            fit: Fit::Stretch,
            ..RenderOptions::default()
        };
        assert_eq!(
            filter_graph(&info, &stretch),
            "fps=15,scale=480:128,setsar=1,format=rgb24"
        );
    }

    #[test]
    fn ffmpeg_args_loop_and_use_file_urls() {
        let args = ffmpeg_args(
            Path::new("/v/a b.mp4"),
            &info(Some(30.0)),
            &RenderOptions::default(),
        );
        let args: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
        assert!(args.windows(2).any(|w| w == ["-stream_loop", "-1"]));
        assert!(args.contains(&"file:/v/a b.mp4"));
        assert_eq!(args.last(), Some(&"pipe:1"));
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
