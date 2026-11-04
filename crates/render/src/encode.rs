//! Baseline JPEG encoding within the panel's size caps: 4:2:2 live frames, 4:2:0 standby images.

use image::RgbImage;
use jpeg_encoder::{ColorType, Encoder, EncodingError, SamplingFactor};
use tt500_lcd_protocol::{
    BootAnimation, Frame, HEIGHT, MAX_BOOT_DATA_BYTES, MAX_BOOT_FRAME_BYTES, MAX_FRAME_BYTES,
    MAX_STANDBY_JPEG_BYTES, StandbyImage, WIDTH,
};

use crate::RenderError;

/// Qualities tried after the requested one, highest first.
const LADDER: [u8; 5] = [80, 70, 60, 50, 40];

/// Baseline, non-progressive JPEG with the given chroma subsampling: the shapes the panel
/// was seen to accept (4:2:2 for live frames, 4:2:0 for the standby image).
fn encode_jpeg(
    rgb: &RgbImage,
    quality: u8,
    sampling: SamplingFactor,
) -> Result<Vec<u8>, EncodingError> {
    let mut out = Vec::with_capacity(16 * 1024);
    let mut encoder = Encoder::new(&mut out, quality);
    encoder.set_sampling_factor(sampling);
    encoder.set_progressive(false);
    encoder.set_optimized_huffman_tables(false);
    encoder.encode(rgb.as_raw(), WIDTH, HEIGHT, ColorType::Rgb)?;
    Ok(out)
}

/// Encodes `rgb` (480x128) at `quality`, stepping the quality down until the JPEG is at most
/// `max_len` bytes.
fn encode_within(
    rgb: &RgbImage,
    quality: u8,
    sampling: SamplingFactor,
    max_len: usize,
) -> Result<Vec<u8>, RenderError> {
    let start = quality.clamp(1, 100);
    let steps = std::iter::once(start).chain(LADDER.into_iter().filter(|&q| q < start));
    for q in steps {
        let jpeg = encode_jpeg(rgb, q, sampling).map_err(|e| RenderError::Encode(e.to_string()))?;
        if jpeg.len() <= max_len {
            return Ok(jpeg);
        }
    }
    Err(RenderError::TooComplex)
}

/// Encodes an arbitrary-sized preview image at quality 80 (not panel-constrained).
pub(crate) fn encode_preview(rgb: &RgbImage) -> Result<Vec<u8>, RenderError> {
    let too_big = || RenderError::Encode("preview image too large".to_owned());
    let width = u16::try_from(rgb.width()).map_err(|_| too_big())?;
    let height = u16::try_from(rgb.height()).map_err(|_| too_big())?;
    let mut out = Vec::with_capacity(32 * 1024);
    Encoder::new(&mut out, 80)
        .encode(rgb.as_raw(), width, height, ColorType::Rgb)
        .map_err(|e| RenderError::Encode(e.to_string()))?;
    Ok(out)
}

/// Encodes a live frame: 4:2:2, within [`MAX_FRAME_BYTES`].
pub(crate) fn encode_frame(rgb: &RgbImage, quality: u8) -> Result<Frame, RenderError> {
    let jpeg = encode_within(rgb, quality, SamplingFactor::R_4_2_2, MAX_FRAME_BYTES)?;
    Ok(Frame::from_jpeg(jpeg)?)
}

/// Encodes a standby image: 4:2:0, within [`MAX_STANDBY_JPEG_BYTES`].
pub(crate) fn encode_standby(rgb: &RgbImage, quality: u8) -> Result<StandbyImage, RenderError> {
    let jpeg = encode_within(
        rgb,
        quality,
        SamplingFactor::R_4_2_0,
        MAX_STANDBY_JPEG_BYTES,
    )?;
    Ok(StandbyImage::from_jpeg(jpeg)?)
}

/// Encodes the frames of a boot animation: 4:2:0, every frame within
/// [`MAX_BOOT_FRAME_BYTES`] and all together within [`MAX_BOOT_DATA_BYTES`]. The whole
/// animation steps down the quality ladder together so the frames stay alike.
pub(crate) fn encode_boot(
    frames: &[RgbImage],
    frame_delay_ms: u16,
    quality: u8,
) -> Result<BootAnimation, RenderError> {
    let start = quality.clamp(1, 100);
    let steps = std::iter::once(start).chain(LADDER.into_iter().filter(|&q| q < start));
    'ladder: for q in steps {
        let mut jpegs = Vec::with_capacity(frames.len());
        let mut total = 0;
        for rgb in frames {
            let jpeg = encode_jpeg(rgb, q, SamplingFactor::R_4_2_0)
                .map_err(|e| RenderError::Encode(e.to_string()))?;
            total += jpeg.len();
            if jpeg.len() > MAX_BOOT_FRAME_BYTES || total > MAX_BOOT_DATA_BYTES {
                continue 'ladder;
            }
            jpegs.push(jpeg);
        }
        return Ok(BootAnimation::from_jpegs(&jpegs, frame_delay_ms)?);
    }
    Err(RenderError::TooComplex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    /// Returns the sampling factors (Y, Cb, Cr) from SOF0 and whether an SOF2 exists.
    fn sof_info(jpeg: &[u8]) -> (Option<[u8; 3]>, bool) {
        let (mut i, mut sof0, mut sof2) = (2, None, false);
        while i + 4 <= jpeg.len() && jpeg[i] == 0xFF {
            let marker = jpeg[i + 1];
            let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
            match marker {
                0xC0 => {
                    // Skip precision(1) height(2) width(2) count(1); then id/sampling/tq per component.
                    let c = &jpeg[i + 4 + 6..];
                    sof0 = Some([c[1], c[4], c[7]]);
                }
                0xC2 => sof2 = true,
                0xDA => break,
                _ => {}
            }
            i += 2 + len;
        }
        (sof0, sof2)
    }

    /// Deterministic white noise (xorshift), `amp` bits of amplitude around mid-grey.
    fn noise(amp_shift: u32) -> RgbImage {
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        RgbImage::from_fn(WIDTH.into(), HEIGHT.into(), |_, _| {
            let mut px = [0u8; 3];
            for b in &mut px {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *b = (128 + u16::from(state.to_le_bytes()[0] >> amp_shift) - (128 >> amp_shift))
                    as u8;
            }
            Rgb(px)
        })
    }

    #[test]
    fn output_is_baseline_422() {
        let img = RgbImage::from_fn(WIDTH.into(), HEIGHT.into(), |x, y| {
            Rgb([(x % 256) as u8, (y * 2) as u8, 90])
        });
        let jpeg = encode_jpeg(&img, 90, SamplingFactor::R_4_2_2).unwrap();
        let (sof0, sof2) = sof_info(&jpeg);
        assert_eq!(sof0, Some([0x21, 0x11, 0x11]));
        assert!(!sof2);
        Frame::from_jpeg(jpeg).unwrap();
    }

    #[test]
    fn standby_output_is_baseline_420() {
        let img = RgbImage::from_fn(WIDTH.into(), HEIGHT.into(), |x, y| {
            Rgb([(x % 256) as u8, (y * 2) as u8, 90])
        });
        let standby = encode_standby(&img, 90).unwrap();
        let (sof0, sof2) = sof_info(standby.as_bytes());
        assert_eq!(sof0, Some([0x22, 0x11, 0x11]));
        assert!(!sof2);
        assert!(standby.byte_len() <= MAX_STANDBY_JPEG_BYTES);
    }

    #[test]
    fn standby_noise_steps_down_or_is_too_complex() {
        assert!(matches!(
            encode_standby(&noise(0), 90),
            Err(RenderError::TooComplex)
        ));
        let busy = noise(3);
        let first = encode_jpeg(&busy, 90, SamplingFactor::R_4_2_0)
            .unwrap()
            .len();
        match encode_standby(&busy, 90) {
            Ok(s) => assert!(s.byte_len() <= MAX_STANDBY_JPEG_BYTES),
            Err(e) => assert!(matches!(e, RenderError::TooComplex), "{e} (q90 {first})"),
        }
    }

    #[test]
    fn boot_frames_are_420_and_within_the_caps() {
        let gradient = RgbImage::from_fn(WIDTH.into(), HEIGHT.into(), |x, y| {
            Rgb([(x % 256) as u8, (y * 2) as u8, 90])
        });
        let boot = encode_boot(&[gradient.clone(), gradient], 100, 90).unwrap();
        assert_eq!(boot.frame_count(), 2);
        assert!(
            boot.frame_sizes()
                .iter()
                .all(|&n| n as usize <= MAX_BOOT_FRAME_BYTES)
        );
        assert!(matches!(
            encode_boot(&[noise(0)], 100, 90),
            Err(RenderError::TooComplex)
        ));
    }

    #[test]
    fn full_noise_is_too_complex() {
        assert!(matches!(
            encode_frame(&noise(0), 90),
            Err(RenderError::TooComplex)
        ));
    }

    #[test]
    fn busy_image_steps_down_to_fit() {
        // Moderate noise: over the cap at q90, fits after the ladder lowers the quality.
        let img = noise(3);
        let first = encode_jpeg(&img, 90, SamplingFactor::R_4_2_2)
            .unwrap()
            .len();
        let frame = encode_frame(&img, 90).unwrap();
        assert!(
            first > MAX_FRAME_BYTES,
            "q90 was {first} bytes; test image not busy enough"
        );
        assert!(frame.byte_len() <= MAX_FRAME_BYTES);
    }
}
