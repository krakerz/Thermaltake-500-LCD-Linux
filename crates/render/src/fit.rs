//! Scaling an arbitrary image to the panel's 480x128 canvas.

use image::imageops::{self, FilterType};
use image::{Rgb, RgbImage};
use tt500_lcd_protocol::{HEIGHT, WIDTH};

/// How a source image is fitted to the panel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Fit {
    /// Scale to cover the panel and centre-crop the overflow.
    #[default]
    Cover,
    /// Scale to fit inside the panel and pad with the background colour.
    Contain,
    /// Resize to exactly the panel size, ignoring the aspect ratio.
    Stretch,
}

const W: u32 = WIDTH as u32;
const H: u32 = HEIGHT as u32;

/// Dimensions after scaling by `max` (cover) or `min` (contain) of the axis ratios,
/// rounded up and at least 1. The bounding axis is pinned to the exact panel size.
pub(crate) fn scaled_dims(w: u32, h: u32, cover: bool) -> (u32, u32) {
    // W/w >= H/h  <=>  W*h >= H*w, compared without floats.
    let width_ratio_larger = u64::from(W) * u64::from(h) >= u64::from(H) * u64::from(w);
    let pin_width = width_ratio_larger == cover;
    let (num, den) = if pin_width { (W, w) } else { (H, h) };
    let scale = |v: u32| {
        let s = (u64::from(v) * u64::from(num))
            .div_ceil(u64::from(den))
            .max(1);
        u32::try_from(s).unwrap_or(u32::MAX)
    };
    if pin_width {
        (W, scale(h))
    } else {
        (scale(w), H)
    }
}

/// Produces a 480x128 image from `src` according to `fit`.
pub(crate) fn fit_image(
    src: &RgbImage,
    fit: Fit,
    background: [u8; 3],
    filter: FilterType,
) -> RgbImage {
    let (w, h) = src.dimensions();
    if (w, h) == (W, H) {
        return src.clone();
    }
    match fit {
        Fit::Stretch => imageops::resize(src, W, H, filter),
        Fit::Cover => {
            let (sw, sh) = scaled_dims(w, h, true);
            let scaled = imageops::resize(src, sw, sh, filter);
            let (x, y) = ((sw - W) / 2, (sh - H) / 2);
            imageops::crop_imm(&scaled, x, y, W, H).to_image()
        }
        Fit::Contain => {
            let (sw, sh) = scaled_dims(w, h, false);
            let scaled = imageops::resize(src, sw, sh, filter);
            let mut canvas = RgbImage::from_pixel(W, H, Rgb(background));
            imageops::replace(
                &mut canvas,
                &scaled,
                i64::from((W - sw) / 2),
                i64::from((H - sh) / 2),
            );
            canvas
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_1080p_crops_vertically() {
        let (sw, sh) = scaled_dims(1920, 1080, true);
        assert_eq!((sw, sh), (480, 270));
        assert_eq!((sh - H) / 2, 71);
    }

    #[test]
    fn contain_square_is_centred() {
        let (sw, sh) = scaled_dims(100, 100, false);
        assert_eq!((sw, sh), (128, 128));
        assert_eq!((W - sw) / 2, 176);
    }

    #[test]
    fn output_is_always_panel_sized() {
        for (w, h) in [
            (1, 1),
            (100, 100),
            (1920, 1080),
            (480, 128),
            (4000, 10),
            (10, 4000),
        ] {
            let src = RgbImage::from_pixel(w, h, Rgb([9, 9, 9]));
            for fit in [Fit::Cover, Fit::Contain, Fit::Stretch] {
                let out = fit_image(&src, fit, [0; 3], FilterType::Triangle);
                assert_eq!(out.dimensions(), (W, H), "{w}x{h} {fit:?}");
            }
        }
    }

    #[test]
    fn contain_pads_with_background() {
        let src = RgbImage::from_pixel(100, 100, Rgb([200, 0, 0]));
        let out = fit_image(&src, Fit::Contain, [1, 2, 3], FilterType::Triangle);
        assert_eq!(out.get_pixel(0, 0), &Rgb([1, 2, 3]));
        assert_eq!(out.get_pixel(240, 64), &Rgb([200, 0, 0]));
    }
}
