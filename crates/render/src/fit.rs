//! Scaling an arbitrary image to the panel's canvas: 480x128, or 128x480 for portrait.

use image::imageops::{self, FilterType};
use image::{Rgb, RgbImage};

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

/// Dimensions of a `target` canvas (width, height) after scaling a `w` x `h` source by `max`
/// (cover) or `min` (contain) of the axis ratios, rounded up and at least 1. The bounding
/// axis is pinned to the exact canvas size.
pub(crate) fn scaled_dims(w: u32, h: u32, cover: bool, target: (u32, u32)) -> (u32, u32) {
    let (tw, th) = target;
    // tw/w >= th/h  <=>  tw*h >= th*w, compared without floats.
    let width_ratio_larger = u64::from(tw) * u64::from(h) >= u64::from(th) * u64::from(w);
    let pin_width = width_ratio_larger == cover;
    let (num, den) = if pin_width { (tw, w) } else { (th, h) };
    let scale = |v: u32| {
        let s = (u64::from(v) * u64::from(num))
            .div_ceil(u64::from(den))
            .max(1);
        u32::try_from(s).unwrap_or(u32::MAX)
    };
    if pin_width {
        (tw, scale(h))
    } else {
        (scale(w), th)
    }
}

/// Produces a `target` (width, height) image from `src` according to `fit`.
pub(crate) fn fit_image(
    src: &RgbImage,
    fit: Fit,
    background: [u8; 3],
    filter: FilterType,
    target: (u32, u32),
) -> RgbImage {
    let (w, h) = src.dimensions();
    let (tw, th) = target;
    if (w, h) == target {
        return src.clone();
    }
    match fit {
        Fit::Stretch => imageops::resize(src, tw, th, filter),
        Fit::Cover => {
            let (sw, sh) = scaled_dims(w, h, true, target);
            let scaled = imageops::resize(src, sw, sh, filter);
            let (x, y) = ((sw - tw) / 2, (sh - th) / 2);
            imageops::crop_imm(&scaled, x, y, tw, th).to_image()
        }
        Fit::Contain => {
            let (sw, sh) = scaled_dims(w, h, false, target);
            let scaled = imageops::resize(src, sw, sh, filter);
            let mut canvas = RgbImage::from_pixel(tw, th, Rgb(background));
            imageops::replace(
                &mut canvas,
                &scaled,
                i64::from((tw - sw) / 2),
                i64::from((th - sh) / 2),
            );
            canvas
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 480;
    const H: u32 = 128;
    const LANDSCAPE: (u32, u32) = (W, H);
    const PORTRAIT: (u32, u32) = (H, W);

    #[test]
    fn cover_1080p_crops_vertically() {
        let (sw, sh) = scaled_dims(1920, 1080, true, LANDSCAPE);
        assert_eq!((sw, sh), (480, 270));
        assert_eq!((sh - H) / 2, 71);
    }

    #[test]
    fn contain_square_is_centred() {
        let (sw, sh) = scaled_dims(100, 100, false, LANDSCAPE);
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
                for target in [LANDSCAPE, PORTRAIT] {
                    let out = fit_image(&src, fit, [0; 3], FilterType::Triangle, target);
                    assert_eq!(out.dimensions(), target, "{w}x{h} {fit:?}");
                }
            }
        }
    }

    #[test]
    fn contain_pads_with_background() {
        let src = RgbImage::from_pixel(100, 100, Rgb([200, 0, 0]));
        let out = fit_image(
            &src,
            Fit::Contain,
            [1, 2, 3],
            FilterType::Triangle,
            LANDSCAPE,
        );
        assert_eq!(out.get_pixel(0, 0), &Rgb([1, 2, 3]));
        assert_eq!(out.get_pixel(240, 64), &Rgb([200, 0, 0]));
    }
}
