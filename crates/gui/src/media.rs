//! Loads the first frame of a file for the crop editor, without GTK.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::Path;
use std::process::{Command, Stdio};

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::{
    AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbaImage, guess_format,
};
use tt500_lcd_ipc::Size;

/// Longest side of the display copy; larger sources are downscaled for drawing only.
pub const DISPLAY_MAX_SIDE: u32 = 4096;

/// Largest image file the daemon accepts, and so the GUI.
pub(crate) const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

/// Bytes read to sniff the image format.
const SNIFF_BYTES: usize = 64;

/// Lines of ffmpeg's stderr kept in an error message.
const STDERR_TAIL_LINES: usize = 3;

/// What kind of file is being edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// One image.
    Still,
    /// A multi-frame GIF, APNG or WebP.
    Animation,
    /// A video file.
    Video,
}

/// A decoded first frame plus the facts the editor needs.
#[derive(Debug, Clone)]
pub struct Source {
    /// The first frame, downscaled to at most [`DISPLAY_MAX_SIDE`] on its longest side.
    pub image: RgbaImage,
    /// The decoded (EXIF-oriented) size of the source, the daemon's crop space.
    pub size: Size,
    /// Still, animation or video.
    pub kind: SourceKind,
}

/// A file could not be loaded for preview.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LoadError {
    /// The file could not be read.
    #[error("cannot read the file: {0}")]
    Io(#[from] io::Error),
    /// Not an image the app understands.
    #[error("unsupported file")]
    Unsupported,
    /// The image is damaged or too large.
    #[error("cannot decode the image: {0}")]
    Decode(String),
    /// A video needs ffmpeg, which is not installed.
    #[error("ffmpeg is not installed, which videos need")]
    MissingFfmpeg,
    /// ffmpeg could not read the video.
    #[error("cannot read the video: {0}")]
    Video(String),
}

pub(crate) fn decode_error(err: &image::ImageError) -> LoadError {
    LoadError::Decode(err.to_string())
}

/// Loads the first frame of the image or video at `path`. Blocking.
///
/// # Errors
/// Returns a [`LoadError`] when the file cannot be read or decoded.
pub fn load(path: &Path) -> Result<Source, LoadError> {
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    File::open(path)?
        .take(SNIFF_BYTES as u64)
        .read_to_end(&mut head)?;
    match guess_format(&head) {
        Ok(format) => load_image(path, format),
        Err(_) => load_video(path),
    }
}

fn load_image(path: &Path, format: ImageFormat) -> Result<Source, LoadError> {
    if path.metadata()?.len() > MAX_IMAGE_BYTES {
        return Err(LoadError::Decode("file is larger than 64 MiB".into()));
    }
    let bytes = std::fs::read(path)?;
    let (image, kind) = match format {
        ImageFormat::Gif | ImageFormat::Png | ImageFormat::WebP => (
            decode_first(&bytes, format)?,
            animation_kind(&bytes, format)?,
        ),
        ImageFormat::Jpeg | ImageFormat::Bmp => (decode_first(&bytes, format)?, SourceKind::Still),
        _ => return Err(LoadError::Unsupported),
    };
    Ok(build(image, kind))
}

/// Decodes the first frame like the daemon: format-pinned, EXIF orientation applied.
fn decode_first(bytes: &[u8], format: ImageFormat) -> Result<RgbaImage, LoadError> {
    let mut decoder = ImageReader::with_format(Cursor::new(bytes), format)
        .into_decoder()
        .map_err(|e| decode_error(&e))?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| decode_error(&e))?;
    if let Some(orientation) = orientation {
        img.apply_orientation(orientation);
    }
    Ok(img.to_rgba8())
}

fn animation_kind(bytes: &[u8], format: ImageFormat) -> Result<SourceKind, LoadError> {
    let animated = match format {
        ImageFormat::Gif => {
            let decoder = GifDecoder::new(Cursor::new(bytes)).map_err(|e| decode_error(&e))?;
            decoder.into_frames().take(2).count() >= 2
        }
        ImageFormat::Png => PngDecoder::new(Cursor::new(bytes))
            .map_err(|e| decode_error(&e))?
            .is_apng()
            .map_err(|e| decode_error(&e))?,
        ImageFormat::WebP => WebPDecoder::new(Cursor::new(bytes))
            .map_err(|e| decode_error(&e))?
            .has_animation(),
        _ => false,
    };
    Ok(if animated {
        SourceKind::Animation
    } else {
        SourceKind::Still
    })
}

fn load_video(path: &Path) -> Result<Source, LoadError> {
    let mut png = run_ffmpeg(&frame_args(path, 1))?;
    if png.is_empty() {
        png = run_ffmpeg(&frame_args(path, 0))?;
    }
    if png.is_empty() {
        return Err(LoadError::Video("no frame could be read".into()));
    }
    let image = image::load_from_memory_with_format(&png, ImageFormat::Png)
        .map_err(|e| LoadError::Video(e.to_string()))?
        .to_rgba8();
    Ok(build(image, SourceKind::Video))
}

/// Runs ffmpeg and returns its stdout; a failed exit is an error with the stderr tail.
fn run_ffmpeg(args: &[OsString]) -> Result<Vec<u8>, LoadError> {
    let output = Command::new("ffmpeg")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => LoadError::MissingFfmpeg,
            _ => LoadError::Io(e),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lines: Vec<&str> = stderr.lines().rev().take(STDERR_TAIL_LINES).collect();
        let tail: Vec<&str> = lines.into_iter().rev().collect();
        return Err(LoadError::Video(tail.join(" ")));
    }
    Ok(output.stdout)
}

fn frame_args(path: &Path, seek_secs: u32) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-ss",
        &seek_secs.to_string(),
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
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-c:v",
            "png",
            "pipe:1",
        ]
        .into_iter()
        .map(OsString::from),
    );
    args
}

/// Wraps a full-size frame: records its size, downscales the display copy.
fn build(image: RgbaImage, kind: SourceKind) -> Source {
    let size = Size {
        width: image.width(),
        height: image.height(),
    };
    let longest = image.width().max(image.height());
    let image = if longest > DISPLAY_MAX_SIDE {
        let scale = f64::from(DISPLAY_MAX_SIDE) / f64::from(longest);
        let width = ((f64::from(image.width()) * scale).round() as u32).max(1);
        let height = ((f64::from(image.height()) * scale).round() as u32).max(1);
        image::imageops::resize(&image, width, height, FilterType::Triangle)
    } else {
        image
    };
    Source { image, size, kind }
}

/// Pixels as cairo's `ARgb32` expects: premultiplied, little-endian BGRA, stride `4 * width`.
#[must_use]
pub fn premultiplied_bgra(image: &RgbaImage) -> Vec<u8> {
    let premultiply = |c: u8, a: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
    let mut out = Vec::with_capacity(image.as_raw().len());
    for pixel in image.pixels() {
        let [r, g, b, a] = pixel.0;
        out.extend_from_slice(&[premultiply(b, a), premultiply(g, a), premultiply(r, a), a]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, Rgba};

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn png_is_a_still() {
        let dir = tempdir();
        let path = dir.path().join("a.png");
        RgbaImage::from_pixel(200, 100, Rgba([1, 2, 3, 255]))
            .save(&path)
            .expect("write png");
        let source = load(&path).expect("loads");
        assert_eq!(source.kind, SourceKind::Still);
        assert_eq!(
            source.size,
            Size {
                width: 200,
                height: 100
            }
        );
        assert_eq!(source.image.dimensions(), (200, 100));
    }

    #[test]
    fn two_frame_gif_is_an_animation() {
        let dir = tempdir();
        let path = dir.path().join("a.gif");
        let mut encoder = GifEncoder::new(File::create(&path).expect("create"));
        for shade in [0, 255] {
            let img = RgbaImage::from_pixel(16, 8, Rgba([shade, 0, 0, 255]));
            encoder
                .encode_frame(Frame::from_parts(
                    img,
                    0,
                    0,
                    Delay::from_numer_denom_ms(100, 1),
                ))
                .expect("frame");
        }
        drop(encoder);
        let source = load(&path).expect("loads");
        assert_eq!(source.kind, SourceKind::Animation);
        assert_eq!(
            source.size,
            Size {
                width: 16,
                height: 8
            }
        );
    }

    #[test]
    fn single_frame_gif_is_a_still() {
        let dir = tempdir();
        let path = dir.path().join("a.gif");
        let mut encoder = GifEncoder::new(File::create(&path).expect("create"));
        let img = RgbaImage::from_pixel(16, 8, Rgba([9, 9, 9, 255]));
        encoder.encode_frame(Frame::new(img)).expect("frame");
        drop(encoder);
        assert_eq!(load(&path).expect("loads").kind, SourceKind::Still);
    }

    #[test]
    fn random_bytes_fail() {
        let dir = tempdir();
        let path = dir.path().join("noise.bin");
        std::fs::write(&path, [0x13u8, 0x37, 0x42, 0x99, 0x01, 0x02, 0x03]).expect("write");
        assert!(load(&path).is_err());
    }

    #[test]
    fn missing_file_is_an_io_error() {
        assert!(matches!(
            load(Path::new("/nonexistent/tt500-lcd-test.png")),
            Err(LoadError::Io(_))
        ));
    }

    #[test]
    fn frame_args_are_exact() {
        let args = frame_args(Path::new("/v/a b.mp4"), 1);
        let args: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
        assert_eq!(
            args,
            [
                "-hide_banner",
                "-nostdin",
                "-loglevel",
                "error",
                "-ss",
                "1",
                "-i",
                "file:/v/a b.mp4",
                "-map",
                "0:v:0",
                "-frames:v",
                "1",
                "-f",
                "image2pipe",
                "-c:v",
                "png",
                "pipe:1",
            ]
        );
    }

    #[test]
    fn premultiplies_half_transparent_red() {
        let image = RgbaImage::from_pixel(1, 1, Rgba([255, 0, 0, 128]));
        assert_eq!(premultiplied_bgra(&image), [0, 0, 128, 128]);
    }

    #[test]
    fn huge_sources_are_downscaled_for_display_only() {
        let source = build(
            RgbaImage::from_pixel(8192, 64, Rgba([0, 0, 0, 255])),
            SourceKind::Still,
        );
        assert_eq!(
            source.size,
            Size {
                width: 8192,
                height: 64
            }
        );
        assert_eq!(source.image.dimensions(), (4096, 32));
    }

    #[test]
    fn video_first_frame_with_ffmpeg() {
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
        let dir = tempdir();
        let path = dir.path().join("clip.mkv");
        let status = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=size=640x360:rate=10:duration=2")
            .arg(&path)
            .status()
            .expect("run ffmpeg");
        assert!(status.success());
        let source = load(&path).expect("loads");
        assert_eq!(source.kind, SourceKind::Video);
        assert_eq!(
            source.size,
            Size {
                width: 640,
                height: 360
            }
        );
    }
}
