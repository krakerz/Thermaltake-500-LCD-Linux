//! Decodes animations and streams video frames for the live preview, without GTK.

use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, ImageFormat, ImageResult, RgbaImage, guess_format};
use tt500_lcd_ipc::{MAX_FPS, Size};

use crate::media::{LoadError, MAX_IMAGE_BYTES, decode_error, premultiplied_bgra};

/// Widest preview frame; larger sources are downscaled for drawing only.
pub const PREVIEW_MAX_WIDTH: u32 = 1280;
/// Tallest preview frame.
pub const PREVIEW_MAX_HEIGHT: u32 = 720;
/// Frame rate of the video preview (changing it would restart ffmpeg).
pub const VIDEO_PREVIEW_FPS: u32 = 15;

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
#[must_use]
pub fn video_args(path: &Path, width: u32, height: u32, fps: u32) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-stream_loop",
        "-1",
        "-i",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    let mut url = OsString::from("file:");
    url.push(path);
    args.push(url);
    args.extend(
        [
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-vf",
            &format!("fps={fps},scale={width}:{height}:flags=bilinear"),
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

/// A running ffmpeg that streams a looping video as preview frames.
///
/// Dropping it kills and reaps the ffmpeg child.
pub struct VideoPreview {
    child: Child,
    frames: mpsc::Receiver<Frame>,
}

impl fmt::Debug for VideoPreview {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VideoPreview")
            .field("pid", &self.child.id())
            .finish_non_exhaustive()
    }
}

impl VideoPreview {
    /// Starts ffmpeg on the video at `path`, whose first frame has `size`.
    ///
    /// # Errors
    /// Returns [`LoadError::MissingFfmpeg`] when ffmpeg is not installed, or
    /// [`LoadError::Io`] when it cannot be started.
    pub fn spawn(path: &Path, size: Size) -> Result<Self, LoadError> {
        let (width, height) = preview_size(size);
        let mut child = Command::new("ffmpeg")
            .args(video_args(path, width, height, VIDEO_PREVIEW_FPS))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => LoadError::MissingFfmpeg,
                _ => LoadError::Io(e),
            })?;
        let Some(mut stdout) = child.stdout.take() else {
            // Not reachable with Stdio::piped(); reap the child anyway.
            let _ = child.kill();
            let _ = child.wait();
            return Err(LoadError::Video("no output pipe".into()));
        };
        let (sender, frames) = mpsc::sync_channel(VIDEO_BUFFER_FRAMES);
        let frame_len = width as usize * height as usize * 4;
        thread::spawn(move || {
            let mut buf = vec![0_u8; frame_len];
            while stdout.read_exact(&mut buf).is_ok() {
                let Some(image) = RgbaImage::from_raw(width, height, buf.clone()) else {
                    return;
                };
                let frame = Frame {
                    width,
                    height,
                    bgra: premultiplied_bgra(&image),
                };
                if sender.send(frame).is_err() {
                    return;
                }
            }
        });
        Ok(Self { child, frames })
    }

    /// The next decoded frame, if one is ready.
    #[must_use]
    pub fn try_frame(&self) -> Option<Frame> {
        self.frames.try_recv().ok()
    }
}

impl Drop for VideoPreview {
    fn drop(&mut self) {
        // The reader thread ends on its own once the pipe closes; never join it.
        let _ = self.child.kill();
        let _ = self.child.wait();
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
        let args = video_args(Path::new("/tmp/a b.mp4"), 640, 360, 15);
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
                "fps=15,scale=640:360:flags=bilinear",
                "-pix_fmt",
                "rgba",
                "-f",
                "rawvideo",
                "pipe:1",
            ]
        );
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
        let preview = VideoPreview::spawn(&path, size(640, 360)).expect("spawns");
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
