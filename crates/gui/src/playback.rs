//! Decodes animations and streams video frames for the live preview, without GTK.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Cursor, Read};
use std::mem;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, ImageFormat, ImageResult, RgbaImage, guess_format};
use tt500_lcd_ipc::{MAX_FPS, Size};

use crate::media::{LoadError, MAX_IMAGE_BYTES, decode_error, premultiplied_bgra};
use crate::settings::HwDecode;

/// Widest preview frame; larger sources are downscaled for drawing only.
pub const PREVIEW_MAX_WIDTH: u32 = 1280;
/// Tallest preview frame.
pub const PREVIEW_MAX_HEIGHT: u32 = 720;

/// Memory cap for the decoded frames of one animation.
const MAX_ANIMATION_BYTES: usize = 256 * 1024 * 1024;
/// Frame cap for one animation.
const MAX_FRAMES: usize = 2000;
/// Decoded video frames buffered ahead; the bounded channel paces ffmpeg.
const VIDEO_BUFFER_FRAMES: usize = 3;
/// Bytes read to sniff the image format.
const SNIFF_BYTES: usize = 64;
/// Hold used for GIF frames that declare a delay of 10 ms or less, as browsers do.
const DEFAULT_FRAME_HOLD: Duration = Duration::from_millis(100);

/// One preview frame: premultiplied BGRA, stride `4 * width`.
#[derive(Debug)]
pub struct Frame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixels as cairo's `ARgb32` expects them.
    pub bgra: Vec<u8>,
}

/// The decoded frames of an animation plus when each is shown.
#[derive(Debug)]
pub struct Animation {
    frames: Vec<Frame>,
    ends: Vec<Duration>,
}

impl Animation {
    /// Splits into the frames and the clock that says which one is visible when.
    #[must_use]
    pub fn into_parts(self) -> (Vec<Frame>, FrameClock) {
        (self.frames, FrameClock { ends: self.ends })
    }
}

/// Maps elapsed time to the index of the frame that is visible.
#[derive(Debug, Clone)]
pub struct FrameClock {
    /// Cumulative end time of each frame.
    ends: Vec<Duration>,
}

impl FrameClock {
    /// The index of the frame visible at `elapsed`, looping over the whole animation.
    #[must_use]
    pub fn frame_at(&self, elapsed: Duration) -> usize {
        let Some(&total) = self.ends.last() else {
            return 0;
        };
        if total.is_zero() {
            return 0;
        }
        let nanos = elapsed.as_nanos() % total.as_nanos();
        let looped = Duration::from_nanos(u64::try_from(nanos).unwrap_or(0));
        self.ends
            .partition_point(|end| *end <= looped)
            .min(self.ends.len() - 1)
    }
}

/// The preview size for a source of `size`: scaled down to fit 1280x720, never up.
#[must_use]
pub fn preview_size(size: Size) -> (u32, u32) {
    let (width, height) = (f64::from(size.width), f64::from(size.height));
    let scale = (f64::from(PREVIEW_MAX_WIDTH) / width)
        .min(f64::from(PREVIEW_MAX_HEIGHT) / height)
        .min(1.0);
    if scale >= 1.0 {
        return (size.width.max(1), size.height.max(1));
    }
    let side = |value: f64| ((value * scale).round() as u32).max(1);
    (side(width), side(height))
}

/// How long a frame with the delay `numer / denom` ms is shown, as the daemon does.
fn hold_time((numer, denom): (u32, u32)) -> Duration {
    let ms = u64::from(numer) / u64::from(denom.max(1));
    let hold = if ms <= 10 {
        DEFAULT_FRAME_HOLD
    } else {
        Duration::from_millis(ms)
    };
    hold.max(Duration::from_secs(1) / MAX_FPS)
}

/// Decodes every frame of the GIF, APNG or WebP at `path`, downscaled for preview. Blocking.
///
/// Stops at 2000 frames or 256 MiB of pixels and keeps what is decoded.
///
/// # Errors
/// Returns a [`LoadError`] when the file cannot be read or decoded.
pub fn decode_animation(path: &Path) -> Result<Animation, LoadError> {
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    File::open(path)?
        .take(SNIFF_BYTES as u64)
        .read_to_end(&mut head)?;
    if path.metadata()?.len() > MAX_IMAGE_BYTES {
        return Err(LoadError::Decode("file is larger than 64 MiB".into()));
    }
    let format = guess_format(&head).map_err(|_| LoadError::Unsupported)?;
    let bytes = std::fs::read(path)?;
    let cursor = Cursor::new(bytes);
    match format {
        ImageFormat::Gif => collect_frames(
            GifDecoder::new(cursor)
                .map_err(|e| decode_error(&e))?
                .into_frames(),
        ),
        ImageFormat::Png => {
            let apng = PngDecoder::new(cursor)
                .map_err(|e| decode_error(&e))?
                .apng()
                .map_err(|e| decode_error(&e))?;
            collect_frames(apng.into_frames())
        }
        ImageFormat::WebP => collect_frames(
            WebPDecoder::new(cursor)
                .map_err(|e| decode_error(&e))?
                .into_frames(),
        ),
        _ => Err(LoadError::Unsupported),
    }
}

fn collect_frames(
    frames: impl Iterator<Item = ImageResult<image::Frame>>,
) -> Result<Animation, LoadError> {
    let mut out = Vec::new();
    let mut ends = Vec::new();
    let mut total = Duration::ZERO;
    let mut bytes = 0_usize;
    for frame in frames {
        let frame = frame.map_err(|e| decode_error(&e))?;
        if out.len() >= MAX_FRAMES || bytes >= MAX_ANIMATION_BYTES {
            tracing::warn!("animation preview stops after {} frames", out.len());
            break;
        }
        total += hold_time(frame.delay().numer_denom_ms());
        let image = shrink(frame.into_buffer());
        let bgra = premultiplied_bgra(&image);
        bytes += bgra.len();
        ends.push(total);
        out.push(Frame {
            width: image.width(),
            height: image.height(),
            bgra,
        });
    }
    if out.is_empty() {
        return Err(LoadError::Decode("the file has no frames".into()));
    }
    Ok(Animation { frames: out, ends })
}

/// Downscales `image` to the preview size when it is larger.
fn shrink(image: RgbaImage) -> RgbaImage {
    let (width, height) = preview_size(Size {
        width: image.width(),
        height: image.height(),
    });
    if (width, height) == image.dimensions() {
        image
    } else {
        image::imageops::resize(&image, width, height, FilterType::Triangle)
    }
}

/// The ffmpeg arguments that stream `path` as endless raw RGBA frames on stdout.
///
/// With a VA-API render node in `vaapi` the video is decoded and scaled on the GPU.
#[must_use]
pub fn video_args(
    path: &Path,
    width: u32,
    height: u32,
    fps: u32,
    vaapi: Option<&Path>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-hide_banner", "-nostdin", "-loglevel", "error"]
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
    let mut url = OsString::from("file:");
    url.push(path);
    args.push(url);
    let filter = if vaapi.is_some() {
        format!("fps={fps},scale_vaapi=w={width}:h={height}:format=nv12,hwdownload,format=nv12")
    } else {
        format!("fps={fps},scale={width}:{height}:flags=bilinear")
    };
    args.extend(
        [
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-vf",
            filter.as_str(),
            "-pix_fmt",
            "rgba",
            "-f",
            "rawvideo",
            "pipe:1",
        ]
        .into_iter()
        .map(OsString::from),
    );
    args
}

/// The render node to decode on, or `None` for software decoding: hardware decoding is off in
/// the settings, the environment (`TT500_LCD_HWDEC=0`) forces software, or there is no node.
#[must_use]
pub fn vaapi_device(
    choice: HwDecode,
    env: Option<&OsStr>,
    nodes: impl IntoIterator<Item = PathBuf>,
) -> Option<PathBuf> {
    let forced_off = env.is_some_and(|v| {
        ["0", "false", "off", "no"]
            .iter()
            .any(|off| v.eq_ignore_ascii_case(off))
    });
    if choice == HwDecode::Off || forced_off {
        return None;
    }
    let mut nodes: Vec<PathBuf> = nodes.into_iter().collect();
    nodes.sort();
    nodes.into_iter().next()
}

/// The DRM render nodes of this machine.
fn render_nodes() -> Vec<PathBuf> {
    fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        .map(|e| e.path())
        .collect()
}

/// How long the window may stay unfocused before the preview pauses.
pub const UNFOCUSED_PAUSE_AFTER: Duration = Duration::from_secs(2);

/// Decides when the preview's ffmpeg should run.
#[derive(Debug, Default)]
pub struct PauseGate {
    unfocused_since: Option<Instant>,
}

impl PauseGate {
    /// Whether the preview should play at `now`. A hidden window always pauses; an unfocused
    /// one only after [`UNFOCUSED_PAUSE_AFTER`], and only if `pause_unfocused`.
    pub fn should_play(
        &mut self,
        now: Instant,
        visible: bool,
        focused: bool,
        pause_unfocused: bool,
    ) -> bool {
        if !visible {
            self.unfocused_since = None;
            return false;
        }
        if focused || !pause_unfocused {
            self.unfocused_since = None;
            return true;
        }
        let since = *self.unfocused_since.get_or_insert(now);
        now.duration_since(since) <= UNFOCUSED_PAUSE_AFTER
    }
}

/// The running ffmpeg, shared so [`VideoPreview`]'s drop can kill whichever attempt is live.
#[derive(Debug, Default)]
struct Running {
    child: Mutex<Option<Child>>,
    stopped: AtomicBool,
}

impl Running {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Child>> {
        self.child.lock().expect("lock poisoned")
    }
}

/// A running ffmpeg that streams a looping video as preview frames.
///
/// Dropping it kills and reaps the ffmpeg child. If the GUI is killed instead, ffmpeg dies of
/// SIGPIPE at its next write to the closed pipe (the code forbids `unsafe`, so no `pdeathsig`).
pub struct VideoPreview {
    running: Arc<Running>,
    frames: mpsc::Receiver<Frame>,
    hardware: bool,
}

impl fmt::Debug for VideoPreview {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VideoPreview")
            .field("hardware", &self.hardware)
            .finish_non_exhaustive()
    }
}

fn spawn_child(
    path: &Path,
    (width, height): (u32, u32),
    fps: u32,
    vaapi: Option<&Path>,
) -> Result<(Child, ChildStdout), LoadError> {
    let mut child = Command::new("ffmpeg")
        .args(video_args(path, width, height, fps, vaapi))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => LoadError::MissingFfmpeg,
            _ => LoadError::Io(e),
        })?;
    let Some(stdout) = child.stdout.take() else {
        // Not reachable with Stdio::piped(); reap the child anyway.
        let _ = child.kill();
        let _ = child.wait();
        return Err(LoadError::Video("no output pipe".into()));
    };
    Ok((child, stdout))
}

/// Reads frames until the pipe closes or the receiver is gone; whether any frame arrived.
fn read_frames(
    mut stdout: ChildStdout,
    (width, height): (u32, u32),
    sender: &mpsc::SyncSender<Frame>,
) -> bool {
    let frame_len = width as usize * height as usize * 4;
    let mut buf = vec![0_u8; frame_len];
    let mut any = false;
    while stdout.read_exact(&mut buf).is_ok() {
        let Some(image) =
            RgbaImage::from_raw(width, height, mem::replace(&mut buf, vec![0; frame_len]))
        else {
            return any;
        };
        any = true;
        let frame = Frame {
            width,
            height,
            bgra: premultiplied_bgra(&image),
        };
        if sender.send(frame).is_err() {
            break;
        }
    }
    any
}

impl VideoPreview {
    /// Starts ffmpeg on the video at `path`, whose first frame has `size`, at `fps` (1..=30).
    ///
    /// With hardware decoding allowed and a render node present it tries VA-API first; if that
    /// ffmpeg ends before producing a frame, a software ffmpeg takes over.
    ///
    /// # Errors
    /// Returns [`LoadError::MissingFfmpeg`] when ffmpeg is not installed, or
    /// [`LoadError::Io`] when it cannot be started.
    pub fn spawn(path: &Path, size: Size, fps: u32, hw: HwDecode) -> Result<Self, LoadError> {
        let dims = preview_size(size);
        let fps = fps.clamp(1, 30);
        let device = vaapi_device(
            hw,
            env::var_os("TT500_LCD_HWDEC").as_deref(),
            render_nodes(),
        );
        let (child, stdout) = spawn_child(path, dims, fps, device.as_deref())?;
        let running = Arc::new(Running {
            child: Mutex::new(Some(child)),
            ..Running::default()
        });
        let (sender, frames) = mpsc::sync_channel(VIDEO_BUFFER_FRAMES);
        let hardware = device.is_some();
        let supervisor = Arc::clone(&running);
        let path = path.to_path_buf();
        // The reader ends on its own once the pipe closes; it is never joined.
        thread::spawn(move || {
            let produced = read_frames(stdout, dims, &sender);
            if produced || !hardware {
                return;
            }
            tracing::info!("VA-API preview decoding failed; using software decoding");
            let mut slot = supervisor.lock();
            if supervisor.stopped.load(Ordering::SeqCst) {
                return;
            }
            if let Some(mut dead) = slot.take() {
                let _ = dead.wait();
            }
            let Ok((child, stdout)) = spawn_child(&path, dims, fps, None) else {
                return;
            };
            *slot = Some(child);
            drop(slot);
            read_frames(stdout, dims, &sender);
        });
        Ok(Self {
            running,
            frames,
            hardware,
        })
    }

    /// The next decoded frame, if one is ready.
    #[must_use]
    pub fn try_frame(&self) -> Option<Frame> {
        self.frames.try_recv().ok()
    }
}

impl Drop for VideoPreview {
    fn drop(&mut self) {
        let mut slot = self.running.lock();
        self.running.stopped.store(true, Ordering::SeqCst);
        if let Some(mut child) = slot.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Rgba};
    use std::time::Instant;

    fn size(width: u32, height: u32) -> Size {
        Size { width, height }
    }

    #[test]
    fn preview_size_scales_down_only() {
        assert_eq!(preview_size(size(1920, 1080)), (1280, 720));
        assert_eq!(preview_size(size(480, 128)), (480, 128));
        assert_eq!(preview_size(size(4000, 100)), (1280, 32));
        assert_eq!(preview_size(size(100, 5000)), (14, 720));
    }

    #[test]
    fn frame_clock_loops_over_the_holds() {
        let clock = FrameClock {
            ends: vec![Duration::from_millis(100), Duration::from_millis(300)],
        };
        let at = |ms| clock.frame_at(Duration::from_millis(ms));
        assert_eq!(at(0), 0);
        assert_eq!(at(99), 0);
        assert_eq!(at(100), 1);
        assert_eq!(at(299), 1);
        assert_eq!(at(300), 0);
    }

    #[test]
    fn hold_time_rules() {
        assert_eq!(hold_time((0, 1)), Duration::from_millis(100));
        assert_eq!(hold_time((20, 1)), Duration::from_secs(1) / 30);
        assert_eq!(hold_time((200, 1)), Duration::from_millis(200));
    }

    #[test]
    fn decodes_a_two_frame_gif() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("a.gif");
        let mut encoder = GifEncoder::new(File::create(&path).expect("create"));
        for shade in [0, 255] {
            let img = RgbaImage::from_pixel(16, 8, Rgba([shade, 0, 0, 255]));
            encoder
                .encode_frame(image::Frame::from_parts(
                    img,
                    0,
                    0,
                    Delay::from_numer_denom_ms(100, 1),
                ))
                .expect("frame");
        }
        drop(encoder);
        let (frames, clock) = decode_animation(&path).expect("decodes").into_parts();
        assert_eq!(frames.len(), 2);
        for frame in &frames {
            assert_eq!((frame.width, frame.height), (16, 8));
            assert_eq!(frame.bgra.len(), 16 * 8 * 4);
        }
        assert_eq!(clock.frame_at(Duration::from_millis(150)), 1);
    }

    #[test]
    fn video_args_are_exact() {
        let args = video_args(Path::new("/tmp/a b.mp4"), 640, 360, 10, None);
        let args: Vec<&str> = args.iter().map(|a| a.to_str().expect("utf-8")).collect();
        assert_eq!(
            args,
            [
                "-hide_banner",
                "-nostdin",
                "-loglevel",
                "error",
                "-stream_loop",
                "-1",
                "-i",
                "file:/tmp/a b.mp4",
                "-map",
                "0:v:0",
                "-an",
                "-sn",
                "-vf",
                "fps=10,scale=640:360:flags=bilinear",
                "-pix_fmt",
                "rgba",
                "-f",
                "rawvideo",
                "pipe:1",
            ]
        );
    }

    #[test]
    fn hardware_args_decode_and_scale_on_the_gpu() {
        let args = video_args(
            Path::new("/v.mp4"),
            640,
            360,
            10,
            Some(Path::new("/dev/dri/renderD128")),
        );
        let args: Vec<&str> = args.iter().map(|a| a.to_str().expect("utf-8")).collect();
        let hw = args
            .iter()
            .position(|a| *a == "-hwaccel")
            .expect("-hwaccel");
        let input = args.iter().position(|a| *a == "-i").expect("-i");
        assert!(hw < input);
        assert!(args.contains(&"/dev/dri/renderD128"));
        assert!(
            args.contains(&"fps=10,scale_vaapi=w=640:h=360:format=nv12,hwdownload,format=nv12")
        );
    }

    #[test]
    fn vaapi_needs_a_node_and_no_opt_out() {
        let nodes = || {
            [
                PathBuf::from("/dev/dri/renderD129"),
                PathBuf::from("/dev/dri/renderD128"),
            ]
        };
        assert_eq!(
            vaapi_device(HwDecode::Auto, None, nodes()),
            Some(PathBuf::from("/dev/dri/renderD128"))
        );
        assert_eq!(vaapi_device(HwDecode::Off, None, nodes()), None);
        assert_eq!(
            vaapi_device(HwDecode::Auto, Some(OsStr::new("0")), nodes()),
            None
        );
        assert!(vaapi_device(HwDecode::Auto, Some(OsStr::new("1")), nodes()).is_some());
        assert_eq!(vaapi_device(HwDecode::Auto, None, []), None);
    }

    #[test]
    fn unfocused_windows_pause_after_the_grace_period() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut gate = PauseGate::default();
        assert!(gate.should_play(at(0), true, true, true));
        // Focus lost: still playing within 2 s, paused after.
        assert!(gate.should_play(at(100), true, false, true));
        assert!(gate.should_play(at(2_100), true, false, true));
        assert!(!gate.should_play(at(2_200), true, false, true));
        // Regaining focus resumes at once and restarts the grace period.
        assert!(gate.should_play(at(3_000), true, true, true));
        assert!(gate.should_play(at(3_100), true, false, true));
        // The setting off: only hidden pauses.
        assert!(gate.should_play(at(9_000), true, false, false));
        assert!(!gate.should_play(at(9_001), false, true, false));
    }

    #[test]
    fn video_preview_streams_frames_with_ffmpeg() {
        let have_ffmpeg = Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !have_ffmpeg {
            eprintln!("skipped: ffmpeg is not installed");
            return;
        }
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("clip.mkv");
        let status = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=size=640x360:rate=10:duration=1")
            .arg(&path)
            .status()
            .expect("run ffmpeg");
        assert!(status.success());
        let preview =
            VideoPreview::spawn(&path, size(640, 360), 10, HwDecode::Auto).expect("spawns");
        let start = Instant::now();
        let frame = loop {
            if let Some(frame) = preview.try_frame() {
                break frame;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "no frame in 5 s");
            thread::sleep(Duration::from_millis(20));
        };
        assert_eq!((frame.width, frame.height), (640, 360));
        assert_eq!(frame.bgra.len(), 640 * 360 * 4);
        drop(preview);
    }
}
