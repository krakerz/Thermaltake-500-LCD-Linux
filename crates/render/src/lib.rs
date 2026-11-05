//! Turns images, animations and videos into panel-ready JPEG frames: decode, crop, fit,
//! compose, encode.

mod crop;
mod decode;
mod encode;
mod fit;
mod media;
mod rotate;
mod video;

use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use image::imageops::{self, FilterType};
use image::{RgbImage, RgbaImage};
use tt500_lcd_protocol::{
    BootAnimation, BootError, Frame, FrameError, MAX_BOOT_FRAMES, Rotation, StandbyImage,
};

pub use crop::Crop;
pub use fit::Fit;
pub use media::{MediaKind, detect};
pub use rotate::{canvas_size, is_portrait};
pub use video::{DecodePath, HWDEC_ENV, VideoInfo, VideoPoll, VideoStream, probe};

/// Largest image or animation file read, in bytes (videos are streamed, not read).
pub const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;

/// Video frame-rate cap when none is given; TT RGB Plus played MP4 at about 15 fps.
pub const DEFAULT_FPS: u32 = 15;

/// Highest frame rate: one frame per [`MIN_FRAME_INTERVAL`](tt500_lcd_protocol::MIN_FRAME_INTERVAL).
pub const MAX_FPS: u32 = 30;

/// How a source is rendered for the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    /// Scaling mode.
    pub fit: Fit,
    /// Colour behind transparency and letterbox bars.
    pub background: [u8; 3],
    /// Starting JPEG quality (1..=100); lowered automatically if a frame is too big.
    pub quality: u8,
    /// Region of the source to show, applied before `fit`; `None` shows the whole source.
    pub crop: Option<Crop>,
    /// Frame-rate cap the user asked for, used clamped to 1..=[`MAX_FPS`]. `None`: animations
    /// keep their own frame timing (only limited to [`MAX_FPS`]) and videos use [`DEFAULT_FPS`].
    pub max_fps: Option<u32>,
    /// Panel rotation step. The source is fitted to [`canvas_size`] (portrait for steps 1 and
    /// 3; crop rectangles stay in source pixels) and then turned into the 480x128 frame.
    pub rotation: Rotation,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            fit: Fit::Cover,
            background: [0, 0, 0],
            quality: 90,
            crop: None,
            max_fps: None,
            rotation: Rotation::default(),
        }
    }
}

impl RenderOptions {
    /// Frame-rate cap for videos: the requested one clamped to 1..=[`MAX_FPS`], else [`DEFAULT_FPS`].
    #[must_use]
    pub fn video_fps_cap(&self) -> u32 {
        self.max_fps.unwrap_or(DEFAULT_FPS).clamp(1, MAX_FPS)
    }

    /// Shortest time an animation frame is shown: `1/max_fps`, or `1/`[`MAX_FPS`] when unset.
    #[must_use]
    pub fn animation_min_hold(&self) -> Duration {
        Duration::from_secs(1) / self.max_fps.unwrap_or(MAX_FPS).clamp(1, MAX_FPS)
    }
}

/// A frame and how long to show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedFrame {
    /// The validated JPEG.
    pub frame: Frame,
    /// Time before the next frame.
    pub hold: Duration,
}

/// What to show on the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playlist {
    /// One static frame.
    Still(Frame),
    /// At least two frames, looped.
    Animation(Vec<TimedFrame>),
}

impl Playlist {
    /// Number of frames.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        match self {
            Self::Still(_) => 1,
            Self::Animation(frames) => frames.len(),
        }
    }
}

/// A rendered image or animation and the size of its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// The frames.
    pub playlist: Playlist,
    /// Source width in pixels, after EXIF orientation and before cropping.
    pub width: u32,
    /// Source height in pixels, after EXIF orientation and before cropping.
    pub height: u32,
}

/// Why rendering failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RenderError {
    /// The file could not be read.
    #[error("cannot read input: {0}")]
    Io(#[from] io::Error),
    /// The input is bigger than [`MAX_INPUT_BYTES`].
    #[error("input is {len} bytes, the limit is {max}")]
    TooLarge {
        /// Actual size.
        len: u64,
        /// Allowed size.
        max: u64,
    },
    /// Not a supported image format.
    #[error("unsupported or unrecognised image format")]
    Unsupported,
    /// The image data is corrupt.
    #[error("cannot decode image: {0}")]
    Decode(String),
    /// An animation has more frames than allowed.
    #[error("animation has more than {max} frames")]
    TooManyFrames {
        /// Frame limit.
        max: usize,
    },
    /// The crop rectangle is empty or reaches outside the source.
    #[error("crop {crop} is empty or outside the {width}x{height} source")]
    BadCrop {
        /// The rejected rectangle.
        crop: Crop,
        /// Source width.
        width: u32,
        /// Source height.
        height: u32,
    },
    /// Video needs ffmpeg and ffprobe, and one of them is not installed.
    #[error("video playback needs ffmpeg and ffprobe, which were not found on PATH")]
    MissingFfmpeg,
    /// ffprobe or ffmpeg failed on the file.
    #[error("cannot play video: {0}")]
    Video(String),
    /// The JPEG encoder failed.
    #[error("cannot encode frame: {0}")]
    Encode(String),
    /// A frame stays over the packet cap even at the lowest quality.
    #[error("image is too detailed to fit the panel's size limit")]
    TooComplex,
    /// A standby image must be one still picture; videos and animations are not accepted.
    #[error("the standby image must be a still picture, not {0}")]
    NotStill(&'static str),
    /// A boot animation must be an animation; stills and videos are not accepted.
    #[error("the boot animation must be an animation (GIF, APNG or WebP), not {0}")]
    NotAnimated(&'static str),
    /// The encoded frames do not make a valid boot animation (a bug).
    #[error("encoded boot animation rejected: {0}")]
    Boot(#[from] BootError),
    /// The encoder produced a JPEG the protocol rejects (a bug).
    #[error("encoded frame rejected: {0}")]
    Frame(#[from] FrameError),
}

/// Reads and renders the image or animation at `path`.
///
/// # Errors
/// Returns a [`RenderError`] if the file is unreadable, too large, unsupported,
/// corrupt, too detailed to encode, or the crop does not fit the source.
pub fn render_file(path: &Path, options: &RenderOptions) -> Result<Rendered, RenderError> {
    let len = fs::metadata(path)?.len();
    if len > MAX_INPUT_BYTES {
        return Err(RenderError::TooLarge {
            len,
            max: MAX_INPUT_BYTES,
        });
    }
    render_bytes(&fs::read(path)?, options)
}

/// Reads `path` and renders it as a standby image (480x128, baseline 4:2:0, size-capped).
///
/// Only still images are accepted: an animation or a video is an error, never reduced to its
/// first frame. `options.quality` is the starting quality and `max_fps` is ignored.
///
/// # Errors
/// [`RenderError::NotStill`] for an animation or video, otherwise as [`render_file`].
pub fn render_standby_file(
    path: &Path,
    options: &RenderOptions,
) -> Result<StandbyImage, RenderError> {
    if detect(path)? == MediaKind::Video {
        return Err(RenderError::NotStill("a video"));
    }
    let len = fs::metadata(path)?.len();
    if len > MAX_INPUT_BYTES {
        return Err(RenderError::TooLarge {
            len,
            max: MAX_INPUT_BYTES,
        });
    }
    render_standby_bytes(&fs::read(path)?, options)
}

/// Renders an in-memory still image as a standby image.
///
/// # Errors
/// [`RenderError::NotStill`] for an animation, otherwise as [`render_bytes`].
pub fn render_standby_bytes(
    bytes: &[u8],
    options: &RenderOptions,
) -> Result<StandbyImage, RenderError> {
    let decode::Decoded::Still(img) = decode::decode(bytes)? else {
        return Err(RenderError::NotStill("an animation"));
    };
    if let Some(crop) = options.crop {
        let (width, height) = img.dimensions();
        crop.check(width, height)?;
    }
    let flat = decode::flatten(&img, options.background);
    let panel = crop_and_fit(flat, options, FilterType::Lanczos3);
    encode::encode_standby(&panel, options.quality)
}

/// A rendered boot animation and how the source was cut down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedBoot {
    /// The validated frames, ready to upload.
    pub animation: BootAnimation,
    /// Frames in the source animation.
    pub source_frames: usize,
    /// Frames at the chosen delay (after resampling, if any), before truncation to
    /// [`MAX_BOOT_FRAMES`].
    pub resampled_frames: usize,
    /// Source width in pixels, after EXIF orientation and before cropping.
    pub width: u32,
    /// Source height in pixels, after EXIF orientation and before cropping.
    pub height: u32,
}

impl RenderedBoot {
    /// Whether frames were cut off at the end to fit [`MAX_BOOT_FRAMES`].
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.resampled_frames > self.animation.frame_count()
    }
}

/// Reads `path` and renders it as a boot animation: 480x128 baseline 4:2:0 frames with one
/// delay for the whole animation.
///
/// Only animations are accepted (GIF, APNG, animated WebP); stills and videos are an error.
/// A source whose frame delays are all equal and within 40 to 1000 ms keeps them; any other
/// animation is resampled at its real speed to its most common delay (clamped to that range).
/// It is then cut to [`MAX_BOOT_FRAMES`] frames.
/// `options.quality` is the starting quality and `max_fps` is ignored.
///
/// # Errors
/// [`RenderError::NotAnimated`] for a still or a video, otherwise as [`render_file`].
pub fn render_boot_animation_file(
    path: &Path,
    options: &RenderOptions,
) -> Result<RenderedBoot, RenderError> {
    if detect(path)? == MediaKind::Video {
        return Err(RenderError::NotAnimated("a video"));
    }
    let len = fs::metadata(path)?.len();
    if len > MAX_INPUT_BYTES {
        return Err(RenderError::TooLarge {
            len,
            max: MAX_INPUT_BYTES,
        });
    }
    render_boot_animation_bytes(&fs::read(path)?, options)
}

/// Renders an in-memory animation as a boot animation.
///
/// # Errors
/// [`RenderError::NotAnimated`] for a still, otherwise as [`render_bytes`].
pub fn render_boot_animation_bytes(
    bytes: &[u8],
    options: &RenderOptions,
) -> Result<RenderedBoot, RenderError> {
    let decode::Decoded::Animation(frames) = decode::decode(bytes)? else {
        return Err(RenderError::NotAnimated("a still image"));
    };
    let source_frames = frames.len();
    let (width, height) = frames.first().map_or((0, 0), |(img, _)| img.dimensions());
    if let Some(crop) = options.crop {
        crop.check(width, height)?;
    }
    let (frame_delay_ms, sampled) = decode::boot_sampling(frames);
    let resampled_frames = sampled.len();
    let rgb: Vec<RgbImage> = sampled
        .iter()
        .take(MAX_BOOT_FRAMES)
        .map(|img| prepare(img, options, FilterType::Triangle))
        .collect();
    Ok(RenderedBoot {
        animation: encode::encode_boot(&rgb, frame_delay_ms, options.quality)?,
        source_frames,
        resampled_frames,
        width,
        height,
    })
}

/// Renders an in-memory image or animation.
///
/// # Errors
/// Returns a [`RenderError`] if the data is unsupported, corrupt, too detailed to encode,
/// or the crop does not fit the source.
pub fn render_bytes(bytes: &[u8], options: &RenderOptions) -> Result<Rendered, RenderError> {
    let decoded = decode::decode(bytes)?.capped(options.animation_min_hold());
    let (width, height) = decoded.dimensions();
    if let Some(crop) = options.crop {
        crop.check(width, height)?;
    }
    let playlist = match decoded {
        decode::Decoded::Still(img) => {
            Playlist::Still(render_one(&img, options, FilterType::Lanczos3)?)
        }
        decode::Decoded::Animation(frames) => Playlist::Animation(
            frames
                .iter()
                .map(|(img, hold)| {
                    Ok(TimedFrame {
                        frame: render_one(img, options, FilterType::Triangle)?,
                        hold: *hold,
                    })
                })
                .collect::<Result<Vec<_>, RenderError>>()?,
        ),
    };
    Ok(Rendered {
        playlist,
        width,
        height,
    })
}

/// Renders one frame; the crop was checked against the source size by the caller.
fn render_one(
    img: &RgbaImage,
    options: &RenderOptions,
    filter: FilterType,
) -> Result<Frame, RenderError> {
    encode::encode_frame(&prepare(img, options, filter), options.quality)
}

/// Flattens, crops and fits one frame to the panel size; the crop was checked by the caller.
fn prepare(img: &RgbaImage, options: &RenderOptions, filter: FilterType) -> RgbImage {
    let flat = decode::flatten(img, options.background);
    crop_and_fit(flat, options, filter)
}

/// Crops (when requested), fits an opaque image to the rotation's canvas and turns it into a
/// panel-sized (480x128) image.
fn crop_and_fit(img: RgbImage, options: &RenderOptions, filter: FilterType) -> RgbImage {
    let source = match options.crop {
        Some(c) => imageops::crop_imm(&img, c.x, c.y, c.width, c.height).to_image(),
        None => img,
    };
    let canvas = fit::fit_image(
        &source,
        options.fit,
        options.background,
        filter,
        canvas_size(options.rotation),
    );
    compose(rotate::to_panel(canvas, options.rotation))
}

/// Turns a decoded video frame already fitted to the rotation's canvas into a panel frame.
pub(crate) fn render_canvas_rgb(
    canvas: RgbImage,
    rotation: Rotation,
    quality: u8,
) -> Result<Frame, RenderError> {
    encode::encode_frame(&compose(rotate::to_panel(canvas, rotation)), quality)
}

/// Shapes one opaque full-source frame (a live capture) into a panel frame: crop, fit, rotate,
/// JPEG.
///
/// Uses the fast triangle filter, as it runs at video rates.
///
/// # Errors
///
/// [`RenderError::BadCrop`] when `options.crop` is empty or outside the image, or an
/// encoding error.
pub fn render_rgb_frame(img: &RgbImage, options: &RenderOptions) -> Result<Frame, RenderError> {
    let (width, height) = img.dimensions();
    if let Some(crop) = options.crop {
        crop.check(width, height)?;
    }
    let shaped = crop_and_fit(img.clone(), options, FilterType::Triangle);
    encode::encode_frame(&shaped, options.quality)
}

/// Encodes a preview JPEG (quality 85) of `img`, downscaled to `max_width` when wider.
///
/// # Errors
///
/// [`RenderError::Encode`] when the image is empty or too large for JPEG.
pub fn encode_preview_jpeg(img: &RgbImage, max_width: u32) -> Result<Vec<u8>, RenderError> {
    let (width, height) = img.dimensions();
    if max_width > 0 && width > max_width {
        let scaled_height = (u64::from(height) * u64::from(max_width) / u64::from(width)).max(1);
        let scaled = imageops::resize(
            img,
            max_width,
            u32::try_from(scaled_height).unwrap_or(height),
            FilterType::Triangle,
        );
        encode::encode_preview(&scaled)
    } else {
        encode::encode_preview(img)
    }
}

/// Hook for later overlays (clock, sensors); the identity for now.
fn compose(base: RgbImage) -> RgbImage {
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame as GifFrame, ImageFormat, Rgb, Rgba};
    use std::io::Cursor;

    #[test]
    fn png_gradient_is_still() {
        let img = RgbImage::from_fn(1000, 500, |x, y| Rgb([(x / 4) as u8, (y / 2) as u8, 128]));
        let mut png = Vec::new();
        img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        let playlist = render_bytes(&png, &RenderOptions::default())
            .unwrap()
            .playlist;
        let Playlist::Still(frame) = playlist else {
            panic!("expected still");
        };
        Frame::from_jpeg(frame.as_bytes().to_vec()).unwrap();
    }

    #[test]
    fn gif_delays_become_holds() {
        let mut gif = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut gif);
            for (i, ms) in [0u32, 50, 200].into_iter().enumerate() {
                let buf = RgbaImage::from_pixel(64, 32, Rgba([i as u8 * 100, 50, 200, 255]));
                let frame = GifFrame::from_parts(buf, 0, 0, Delay::from_numer_denom_ms(ms, 1));
                enc.encode_frame(frame).unwrap();
            }
        }
        let Playlist::Animation(frames) = render_bytes(&gif, &RenderOptions::default())
            .unwrap()
            .playlist
        else {
            panic!("expected animation");
        };
        let holds: Vec<u128> = frames.iter().map(|f| f.hold.as_millis()).collect();
        assert_eq!(holds, [100, 50, 200]);
    }

    #[test]
    fn explicit_fps_resamples_animation() {
        let mut gif = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut gif);
            for (i, ms) in [0u32, 50, 200].into_iter().enumerate() {
                let buf = RgbaImage::from_pixel(64, 32, Rgba([i as u8 * 100, 50, 200, 255]));
                let frame = GifFrame::from_parts(buf, 0, 0, Delay::from_numer_denom_ms(ms, 1));
                enc.encode_frame(frame).unwrap();
            }
        }
        let options = RenderOptions {
            max_fps: Some(15),
            ..RenderOptions::default()
        };
        let Playlist::Animation(frames) = render_bytes(&gif, &options).unwrap().playlist else {
            panic!("expected animation");
        };
        let holds: Vec<u128> = frames.iter().map(|f| f.hold.as_micros()).collect();
        assert_eq!(holds, [100_000, 66_666, 183_333]);
    }

    #[test]
    fn noise_png_renders_or_is_too_complex() {
        let mut state = 88_172_645_463_325_252_u64;
        let img = RgbImage::from_fn(480, 128, |_, _| {
            let mut px = [0u8; 3];
            for b in &mut px {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *b = state.to_le_bytes()[0];
            }
            Rgb(px)
        });
        let mut png = Vec::new();
        img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        match render_bytes(&png, &RenderOptions::default()) {
            Ok(r) => assert_eq!(r.playlist.frame_count(), 1),
            Err(e) => assert!(matches!(e, RenderError::TooComplex), "{e}"),
        }
    }

    #[test]
    fn crop_selects_the_region_before_fitting() {
        let img = RgbImage::from_fn(200, 100, |x, _| {
            if x < 100 {
                Rgb([255, 0, 0])
            } else {
                Rgb([0, 0, 255])
            }
        });
        let mut png = Vec::new();
        img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        let options = RenderOptions {
            fit: Fit::Stretch,
            crop: Some(Crop {
                x: 100,
                y: 0,
                width: 100,
                height: 100,
            }),
            ..RenderOptions::default()
        };
        let rendered = render_bytes(&png, &options).unwrap();
        assert_eq!((rendered.width, rendered.height), (200, 100));
        let Playlist::Still(frame) = rendered.playlist else {
            panic!("expected still");
        };
        let shown = image::load_from_memory(frame.as_bytes()).unwrap().to_rgb8();
        let Rgb([r, _, b]) = *shown.get_pixel(10, 64);
        assert!(
            b > 200 && r < 60,
            "left edge should be blue, got r={r} b={b}"
        );

        let bad = RenderOptions {
            crop: Some(Crop {
                x: 150,
                y: 0,
                width: 100,
                height: 10,
            }),
            ..options
        };
        assert!(matches!(
            render_bytes(&png, &bad),
            Err(RenderError::BadCrop {
                width: 200,
                height: 100,
                ..
            })
        ));
    }

    #[test]
    fn apng_animates() {
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, 64, 32);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_animated(2, 0).unwrap();
            enc.set_frame_delay(1, 5).unwrap(); // 200 ms
            let mut writer = enc.write_header().unwrap();
            for shade in [10u8, 200] {
                writer.write_image_data(&[shade; 64 * 32 * 4]).unwrap();
            }
            writer.finish().unwrap();
        }
        let rendered = render_bytes(&bytes, &RenderOptions::default()).unwrap();
        assert_eq!((rendered.width, rendered.height), (64, 32));
        let Playlist::Animation(frames) = rendered.playlist else {
            panic!("expected animation");
        };
        let holds: Vec<u128> = frames.iter().map(|f| f.hold.as_millis()).collect();
        assert_eq!(holds, [200, 200]);
    }

    fn png_bytes(img: &RgbImage) -> Vec<u8> {
        let mut png = Vec::new();
        img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        png
    }

    #[test]
    fn standby_still_is_420_and_480x128() {
        let img = RgbImage::from_fn(1000, 500, |x, y| Rgb([(x / 4) as u8, (y / 2) as u8, 128]));
        let standby = render_standby_bytes(&png_bytes(&img), &RenderOptions::default()).unwrap();
        let shown = image::load_from_memory(standby.as_bytes()).unwrap();
        assert_eq!((shown.width(), shown.height()), (480, 128));
    }

    #[test]
    fn standby_rejects_animation_and_video() {
        let mut gif = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut gif);
            for ms in [50u32, 50] {
                let buf = RgbaImage::from_pixel(64, 32, Rgba([10, 50, 200, 255]));
                let frame = GifFrame::from_parts(buf, 0, 0, Delay::from_numer_denom_ms(ms, 1));
                enc.encode_frame(frame).unwrap();
            }
        }
        assert!(matches!(
            render_standby_bytes(&gif, &RenderOptions::default()),
            Err(RenderError::NotStill(_))
        ));
        let dir = tempfile::tempdir().unwrap();
        let mp4 = dir.path().join("v.mp4");
        fs::write(&mp4, b"\0\0\0\x18ftypmp42\0\0\0\0mp42isom").unwrap();
        assert!(matches!(
            render_standby_file(&mp4, &RenderOptions::default()),
            Err(RenderError::NotStill(_))
        ));
    }

    fn gif_bytes(frames: usize, delay_ms: u32) -> Vec<u8> {
        let mut gif = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut gif);
            for i in 0..frames {
                let buf = RgbaImage::from_pixel(64, 32, Rgba([(i * 5) as u8, 50, 200, 255]));
                let frame =
                    GifFrame::from_parts(buf, 0, 0, Delay::from_numer_denom_ms(delay_ms, 1));
                enc.encode_frame(frame).unwrap();
            }
        }
        gif
    }

    #[test]
    fn boot_animation_keeps_a_uniform_native_delay() {
        for ms in [40, 50, 100] {
            let boot =
                render_boot_animation_bytes(&gif_bytes(12, ms), &RenderOptions::default()).unwrap();
            assert_eq!(boot.source_frames, 12);
            assert_eq!(boot.resampled_frames, 12);
            assert_eq!(boot.animation.frame_count(), 12);
            assert_eq!(u32::from(boot.animation.frame_delay_ms()), ms);
            assert!(!boot.truncated());
        }
    }

    #[test]
    fn boot_animation_resamples_mixed_delays_to_the_most_common() {
        // 3 x 50 ms + 1 x 250 ms = 400 ms: 50 ms wins, so 8 frames.
        let mut gif = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut gif);
            for (i, ms) in [50u32, 50, 50, 250].into_iter().enumerate() {
                let buf = RgbaImage::from_pixel(64, 32, Rgba([(i * 5) as u8, 50, 200, 255]));
                let frame = GifFrame::from_parts(buf, 0, 0, Delay::from_numer_denom_ms(ms, 1));
                enc.encode_frame(frame).unwrap();
            }
        }
        let boot = render_boot_animation_bytes(&gif, &RenderOptions::default()).unwrap();
        assert_eq!(boot.animation.frame_delay_ms(), 50);
        assert_eq!(boot.resampled_frames, 8);
        assert_eq!(boot.animation.frame_count(), 8);
    }

    #[test]
    fn boot_animation_frames_are_480x128() {
        let boot =
            render_boot_animation_bytes(&gif_bytes(12, 100), &RenderOptions::default()).unwrap();
        let shown =
            image::load_from_memory(&boot.animation.blocks()[1][0].as_bytes()[16..]).unwrap();
        assert_eq!((shown.width(), shown.height()), (480, 128));
    }

    #[test]
    fn boot_animation_is_truncated_to_75_frames() {
        // 90 frames x 40 ms: 90 kept as they are, 75 uploaded.
        let boot =
            render_boot_animation_bytes(&gif_bytes(90, 40), &RenderOptions::default()).unwrap();
        assert_eq!(boot.resampled_frames, 90);
        assert_eq!(boot.animation.frame_delay_ms(), 40);
        assert_eq!(boot.animation.frame_count(), MAX_BOOT_FRAMES);
        assert!(boot.truncated());
    }

    #[test]
    fn boot_animation_rejects_stills_and_videos() {
        let img = RgbImage::from_pixel(200, 100, Rgb([1, 2, 3]));
        assert!(matches!(
            render_boot_animation_bytes(&png_bytes(&img), &RenderOptions::default()),
            Err(RenderError::NotAnimated(_))
        ));
        assert!(matches!(
            render_boot_animation_bytes(&gif_bytes(1, 100), &RenderOptions::default()),
            Err(RenderError::NotAnimated(_))
        ));
        let dir = tempfile::tempdir().unwrap();
        let mp4 = dir.path().join("v.mp4");
        fs::write(&mp4, b"\0\0\0\x18ftypmp42\0\0\0\0mp42isom").unwrap();
        assert!(matches!(
            render_boot_animation_file(&mp4, &RenderOptions::default()),
            Err(RenderError::NotAnimated(_))
        ));
    }

    #[test]
    fn rgb_frame_checks_the_crop_and_is_panel_sized() {
        let img = RgbImage::from_pixel(960, 256, Rgb([10, 20, 30]));
        let frame = render_rgb_frame(&img, &RenderOptions::default()).unwrap();
        assert_eq!(frame.as_bytes()[..2], [0xff, 0xd8]);
        let crop_at = |x| RenderOptions {
            crop: Some(Crop {
                x,
                y: 0,
                width: 480,
                height: 128,
            }),
            ..RenderOptions::default()
        };
        assert!(render_rgb_frame(&img, &crop_at(100)).is_ok());
        assert!(matches!(
            render_rgb_frame(&img, &crop_at(600)),
            Err(RenderError::BadCrop {
                width: 960,
                height: 256,
                ..
            })
        ));
    }

    #[test]
    fn preview_downscales_wide_images_keeping_the_aspect() {
        let img = RgbImage::from_pixel(1920, 1080, Rgb([200, 100, 50]));
        let decoded = image::load_from_memory(&encode_preview_jpeg(&img, 960).unwrap()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (960, 540));
        let small = RgbImage::from_pixel(100, 50, Rgb([1, 2, 3]));
        let decoded = image::load_from_memory(&encode_preview_jpeg(&small, 960).unwrap()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (100, 50));
    }

    #[test]
    fn boot_animation_checks_the_crop() {
        let bad = RenderOptions {
            crop: Some(Crop {
                x: 60,
                y: 0,
                width: 100,
                height: 10,
            }),
            ..RenderOptions::default()
        };
        assert!(matches!(
            render_boot_animation_bytes(&gif_bytes(4, 100), &bad),
            Err(RenderError::BadCrop { .. })
        ));
    }

    #[test]
    fn standby_applies_crop_and_checks_it() {
        let img = RgbImage::from_pixel(200, 100, Rgb([1, 2, 3]));
        let bad = RenderOptions {
            crop: Some(Crop {
                x: 150,
                y: 0,
                width: 100,
                height: 10,
            }),
            ..RenderOptions::default()
        };
        assert!(matches!(
            render_standby_bytes(&png_bytes(&img), &bad),
            Err(RenderError::BadCrop { .. })
        ));
    }

    #[test]
    fn garbage_is_unsupported() {
        let err = render_bytes(b"definitely not an image", &RenderOptions::default()).unwrap_err();
        assert!(matches!(err, RenderError::Unsupported));
    }

    /// Colours near the four corners (inset 8 px) of a decoded panel frame.
    fn decoded_corners(frame: &Frame) -> [[u8; 3]; 4] {
        let img = image::load_from_memory_with_format(frame.as_bytes(), ImageFormat::Jpeg)
            .unwrap()
            .to_rgb8();
        assert_eq!(img.dimensions(), (480, 128));
        let (w, h) = img.dimensions();
        [(8, 8), (w - 9, 8), (8, h - 9), (w - 9, h - 9)].map(|(x, y)| img.get_pixel(x, y).0)
    }

    fn quadrant_source(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| match (x < w / 2, y < h / 2) {
            (true, true) => Rgb([255, 0, 0]),
            (false, true) => Rgb([0, 255, 0]),
            (true, false) => Rgb([0, 0, 255]),
            (false, false) => Rgb([255, 255, 255]),
        })
    }

    fn near(got: [u8; 3], want: [u8; 3]) -> bool {
        got.iter().zip(want).all(|(g, w)| g.abs_diff(w) < 40)
    }

    fn assert_corners(frame: &Frame, want: [[u8; 3]; 4]) {
        let got = decoded_corners(frame);
        for (g, w) in got.into_iter().zip(want) {
            assert!(near(g, w), "got {got:?}, want {want:?}");
        }
    }

    #[test]
    fn rgb_frames_are_rotated_per_step() {
        const R: [u8; 3] = [255, 0, 0];
        const G: [u8; 3] = [0, 255, 0];
        const B: [u8; 3] = [0, 0, 255];
        const W: [u8; 3] = [255, 255, 255];
        let landscape = quadrant_source(480, 128);
        let portrait = quadrant_source(128, 480);
        let render = |img: &RgbImage, step: u8| {
            let options = RenderOptions {
                rotation: Rotation::try_from(step).unwrap(),
                ..RenderOptions::default()
            };
            render_rgb_frame(img, &options).unwrap()
        };
        assert_corners(&render(&landscape, 0), [R, G, B, W]);
        assert_corners(&render(&landscape, 2), [W, B, G, R]);
        assert_corners(&render(&portrait, 1), [B, R, W, G]);
        assert_corners(&render(&portrait, 3), [G, W, R, B]);
    }

    #[test]
    fn portrait_steps_fit_the_source_to_a_portrait_canvas() {
        // A wide source cropped to a portrait window: the crop rectangle is in source pixels.
        let img = quadrant_source(1000, 500);
        let options = RenderOptions {
            rotation: Rotation::try_from(1).unwrap(),
            crop: Some(Crop {
                x: 0,
                y: 0,
                width: 500,
                height: 500,
            }),
            ..RenderOptions::default()
        };
        // The 500x500 crop covers the portrait canvas, so only the red/blue left half shows
        // (red top, blue bottom); turned clockwise: blue on the left, red on the right.
        let frame = render_rgb_frame(&img, &options).unwrap();
        let got = decoded_corners(&frame);
        assert!(
            near(got[0], [0, 0, 255]) && near(got[1], [255, 0, 0]),
            "{got:?}"
        );
    }
}
