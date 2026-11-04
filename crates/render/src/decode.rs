//! Decoding of still images and GIF, APNG and WebP animations into RGBA frames.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::time::Duration;

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{
    AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Rgb, RgbImage, Rgba,
    RgbaImage,
};
use tt500_lcd_protocol::{MAX_BOOT_FRAME_DELAY_MS, MIN_BOOT_FRAME_DELAY_MS, MIN_FRAME_INTERVAL};

use crate::RenderError;

/// Most animation frames accepted.
pub(crate) const MAX_FRAMES: usize = 1000;

/// Delay used for frames declaring 10 ms or less (browser convention).
const DEFAULT_GIF_DELAY: Duration = Duration::from_millis(100);

/// A decoded source: one image, or frames with their hold times.
pub(crate) enum Decoded {
    Still(RgbaImage),
    Animation(Vec<(RgbaImage, Duration)>),
}

impl Decoded {
    /// Source size in pixels (an animation's frames all have the canvas size).
    pub(crate) fn dimensions(&self) -> (u32, u32) {
        match self {
            Self::Still(img) => img.dimensions(),
            Self::Animation(frames) => frames.first().map_or((0, 0), |(img, _)| img.dimensions()),
        }
    }
}

fn decode_error(err: &image::ImageError) -> RenderError {
    RenderError::Decode(err.to_string())
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Decoded, RenderError> {
    let format = image::guess_format(bytes).map_err(|_| RenderError::Unsupported)?;
    match format {
        ImageFormat::Gif => {
            decode_animation(GifDecoder::new(Cursor::new(bytes)).map_err(|e| decode_error(&e))?)
        }
        ImageFormat::Png => {
            let decoder = PngDecoder::new(Cursor::new(bytes)).map_err(|e| decode_error(&e))?;
            if decoder.is_apng().map_err(|e| decode_error(&e))? {
                decode_animation(decoder.apng().map_err(|e| decode_error(&e))?)
            } else {
                decode_still(bytes, format)
            }
        }
        ImageFormat::WebP => {
            let decoder = WebPDecoder::new(Cursor::new(bytes)).map_err(|e| decode_error(&e))?;
            if decoder.has_animation() {
                decode_animation(decoder)
            } else {
                decode_still(bytes, format)
            }
        }
        ImageFormat::Jpeg | ImageFormat::Bmp => decode_still(bytes, format),
        _ => Err(RenderError::Unsupported),
    }
}

fn decode_still(bytes: &[u8], format: ImageFormat) -> Result<Decoded, RenderError> {
    let mut decoder = ImageReader::with_format(Cursor::new(bytes), format)
        .into_decoder()
        .map_err(|e| decode_error(&e))?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| decode_error(&e))?;
    if let Some(orientation) = orientation {
        img.apply_orientation(orientation);
    }
    Ok(Decoded::Still(img.to_rgba8()))
}

/// Composited frames of a GIF, APNG or animated WebP; one frame becomes a still.
fn decode_animation<'a>(decoder: impl AnimationDecoder<'a>) -> Result<Decoded, RenderError> {
    let mut frames = Vec::new();
    for frame in decoder.into_frames() {
        let frame = frame.map_err(|e| decode_error(&e))?;
        if frames.len() >= MAX_FRAMES {
            return Err(RenderError::TooManyFrames { max: MAX_FRAMES });
        }
        let hold = hold_time(frame.delay().numer_denom_ms());
        frames.push((frame.into_buffer(), hold));
    }
    match frames.len() {
        0 => Err(RenderError::Decode("animation has no frames".into())),
        1 => {
            let (img, _) = frames.remove(0);
            Ok(Decoded::Still(img))
        }
        _ => Ok(Decoded::Animation(frames)),
    }
}

impl Decoded {
    /// Resamples an animation so no frame is shown for less than `min_hold`, keeping its
    /// real-time speed; an animation that collapses to one frame becomes a still.
    pub(crate) fn capped(self, min_hold: Duration) -> Self {
        let Self::Animation(frames) = self else {
            return self;
        };
        let holds: Vec<Duration> = frames.iter().map(|(_, hold)| *hold).collect();
        let schedule = cap_schedule(&holds, min_hold);
        let mut images: Vec<Option<RgbaImage>> =
            frames.into_iter().map(|(img, _)| Some(img)).collect();
        let mut kept: Vec<(RgbaImage, Duration)> = schedule
            .into_iter()
            .map(|(index, hold)| {
                let img = images[index]
                    .take()
                    .expect("schedule indices are increasing");
                (img, hold)
            })
            .collect();
        if kept.len() == 1 {
            let (img, _) = kept.remove(0);
            Self::Still(img)
        } else {
            Self::Animation(kept)
        }
    }
}

/// Resamples frames to a fixed `interval` at the animation's real-time speed: sample `k` shows
/// the frame visible at `k * interval`, for as many samples as fit the loop (at least one).
pub(crate) fn resample_fixed(
    frames: Vec<(RgbaImage, Duration)>,
    interval: Duration,
) -> Vec<RgbaImage> {
    let total: Duration = frames.iter().map(|(_, hold)| *hold).sum();
    let samples = ((total.as_micros() + interval.as_micros() / 2) / interval.as_micros()).max(1);
    let mut images = frames.into_iter();
    let Some((mut current, mut ends)) = images.next() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for k in 0..samples {
        let at = interval * u32::try_from(k).expect("at most 1000 frames of animation");
        while at >= ends {
            let Some((img, hold)) = images.next() else {
                break;
            };
            current = img;
            ends += hold;
        }
        out.push(current.clone());
    }
    out
}

/// Picks the frame delay (in ms) of a boot animation and the frames to show at it.
///
/// When every hold is the same and within [`MIN_BOOT_FRAME_DELAY_MS`]..=[`MAX_BOOT_FRAME_DELAY_MS`]
/// the frames are used as they are. Otherwise the most common hold (the longer one on a tie),
/// clamped to that range, becomes the delay and the animation is resampled to it at its real
/// speed.
pub(crate) fn boot_sampling(frames: Vec<(RgbaImage, Duration)>) -> (u16, Vec<RgbaImage>) {
    let range = u64::from(MIN_BOOT_FRAME_DELAY_MS)..=u64::from(MAX_BOOT_FRAME_DELAY_MS);
    let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
    for (_, hold) in &frames {
        *counts.entry(hold_ms(*hold)).or_default() += 1;
    }
    let common = counts
        .iter()
        .max_by_key(|&(_, count)| *count)
        .map_or(*range.start(), |(&ms, _)| ms);
    let delay = common.clamp(*range.start(), *range.end());
    let delay_ms = u16::try_from(delay).expect("the range fits in u16");
    if counts.len() == 1 && range.contains(&common) {
        return (delay_ms, frames.into_iter().map(|(img, _)| img).collect());
    }
    (
        delay_ms,
        resample_fixed(frames, Duration::from_millis(delay)),
    )
}

fn hold_ms(hold: Duration) -> u64 {
    u64::try_from(hold.as_millis()).unwrap_or(u64::MAX)
}

/// Which source frames to show, and for how long, so every hold is at least `min_hold`.
///
/// The output switches frames at the earliest time that is both `min_hold` after the previous
/// switch and at or after the next source frame's start, and shows the source frame visible at
/// that time. The loop length is kept; only the last hold is padded up to `min_hold`.
fn cap_schedule(holds: &[Duration], min_hold: Duration) -> Vec<(usize, Duration)> {
    let starts: Vec<Duration> = holds
        .iter()
        .scan(Duration::ZERO, |t, hold| {
            let start = *t;
            *t += *hold;
            Some(start)
        })
        .collect();
    let total: Duration = holds.iter().sum();
    let mut schedule = Vec::new();
    let (mut index, mut at) = (0, Duration::ZERO);
    while let Some(&next_start) = starts.get(index + 1) {
        let switch = (at + min_hold).max(next_start);
        if switch >= total {
            break;
        }
        // The frame visible at `switch`: the last one starting at or before it.
        let next = starts.partition_point(|start| *start <= switch) - 1;
        schedule.push((index, switch - at));
        index = next;
        at = switch;
    }
    schedule.push((index, (total - at).max(min_hold)));
    schedule
}

fn hold_time((numer, denom): (u32, u32)) -> Duration {
    let ms = u64::from(numer) / u64::from(denom.max(1));
    let hold = if ms <= 10 {
        DEFAULT_GIF_DELAY
    } else {
        Duration::from_millis(ms)
    };
    hold.max(MIN_FRAME_INTERVAL)
}

/// Alpha-blends `img` over a solid background.
pub(crate) fn flatten(img: &RgbaImage, background: [u8; 3]) -> RgbImage {
    RgbImage::from_fn(img.width(), img.height(), |x, y| {
        let Rgba([r, g, b, a]) = *img.get_pixel(x, y);
        let a = u32::from(a);
        let mix = |fg: u8, bg: u8| {
            let v = (u32::from(fg) * a + u32::from(bg) * (255 - a) + 127) / 255;
            u8::try_from(v).unwrap_or(u8::MAX)
        };
        Rgb([
            mix(r, background[0]),
            mix(g, background[1]),
            mix(b, background[2]),
        ])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_time_rules() {
        assert_eq!(hold_time((0, 1)), Duration::from_millis(100));
        assert_eq!(hold_time((30, 1)), MIN_FRAME_INTERVAL);
        assert_eq!(hold_time((200, 1)), Duration::from_millis(200));
    }

    #[test]
    fn flatten_blends_alpha() {
        let clear = RgbaImage::from_pixel(1, 1, Rgba([255, 255, 255, 0]));
        assert_eq!(
            flatten(&clear, [10, 20, 30]).get_pixel(0, 0).0,
            [10, 20, 30]
        );
        let solid = RgbaImage::from_pixel(1, 1, Rgba([255, 255, 255, 255]));
        assert_eq!(
            flatten(&solid, [10, 20, 30]).get_pixel(0, 0).0,
            [255, 255, 255]
        );
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn resample_fixed_keeps_the_loop_speed() {
        let shade = |v: u8| RgbaImage::from_pixel(1, 1, Rgba([v, 0, 0, 255]));
        let reds = |frames: Vec<RgbaImage>| -> Vec<u8> {
            frames.iter().map(|f| f.get_pixel(0, 0).0[0]).collect()
        };
        // 40 ms + 250 ms + 110 ms = 400 ms = 4 samples at t = 0, 100, 200, 300.
        let frames = vec![(shade(1), ms(40)), (shade(2), ms(250)), (shade(3), ms(110))];
        assert_eq!(reds(resample_fixed(frames, ms(100))), [1, 2, 2, 3]);
        // Slow frames repeat; fast ones are dropped.
        let slow = vec![(shade(1), ms(300)), (shade(2), ms(100))];
        assert_eq!(reds(resample_fixed(slow, ms(100))), [1, 1, 1, 2]);
        let fast = vec![(shade(1), ms(30)), (shade(2), ms(30)), (shade(3), ms(240))];
        assert_eq!(reds(resample_fixed(fast, ms(100))), [1, 3, 3]);
    }

    #[test]
    fn boot_sampling_keeps_uniform_delays_and_resamples_the_rest() {
        let shade = |v: u8| RgbaImage::from_pixel(1, 1, Rgba([v, 0, 0, 255]));
        let uniform = |n: u8, hold: u64| -> Vec<(RgbaImage, Duration)> {
            (0..n).map(|i| (shade(i), ms(hold))).collect()
        };
        for hold in [40, 100, 1000] {
            let (delay, out) = boot_sampling(uniform(5, hold));
            assert_eq!((u64::from(delay), out.len()), (hold, 5));
        }
        // Too fast (a 20 ms hold survives hold_time): 40 ms, loop speed kept.
        let (delay, out) = boot_sampling(uniform(8, 20));
        assert_eq!((delay, out.len()), (40, 4));
        // Too slow: clamped to 1000 ms.
        let (delay, out) = boot_sampling(uniform(2, 2000));
        assert_eq!((delay, out.len()), (1000, 4));
        // Mixed: the most common hold wins (50 ms x3 over 100 ms x1), 400 ms = 8 samples.
        let mixed = vec![
            (shade(1), ms(50)),
            (shade(2), ms(50)),
            (shade(3), ms(50)),
            (shade(4), ms(250)),
        ];
        let (delay, out) = boot_sampling(mixed);
        assert_eq!((delay, out.len()), (50, 8));
    }

    #[test]
    fn cap_schedule_cases() {
        let m = Duration::from_secs(1) / 15;
        let holds = [ms(100), ms(50), ms(200)];
        assert_eq!(
            cap_schedule(&holds, ms(50)),
            [(0, ms(100)), (1, ms(50)), (2, ms(200))]
        );
        assert_eq!(
            cap_schedule(&holds, m),
            [(0, ms(100)), (1, m), (2, ms(350) - ms(100) - m)]
        );
        assert_eq!(cap_schedule(&[ms(40); 4], m), [(0, m), (1, m), (3, m)]);
        assert_eq!(cap_schedule(&[ms(40); 2], ms(100)), [(0, ms(100))]);
    }

    #[test]
    fn capped_collapses_to_still() {
        let frame = || RgbaImage::new(2, 2);
        let two = Decoded::Animation(vec![(frame(), ms(40)), (frame(), ms(40))]);
        assert!(matches!(two.capped(ms(100)), Decoded::Still(_)));
    }
}
