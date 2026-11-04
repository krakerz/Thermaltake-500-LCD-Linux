//! Host-side panel rotation: the panel ignores `03 1d NN` visually, so the picture is turned
//! before encoding, the way TT RGB Plus does (steps 1 and 3 lay out a portrait canvas).

use image::RgbImage;
use image::imageops;
use tt500_lcd_protocol::{HEIGHT, Rotation, WIDTH};

/// The (width, height) the source is fitted to before the final turn: 480x128 for steps 0
/// and 2, the portrait 128x480 for steps 1 and 3.
#[must_use]
pub fn canvas_size(rotation: Rotation) -> (u32, u32) {
    let (w, h) = (u32::from(WIDTH), u32::from(HEIGHT));
    if is_portrait(rotation) {
        (h, w)
    } else {
        (w, h)
    }
}

/// Whether the step lays content out on a portrait canvas (steps 1 and 3).
#[must_use]
pub fn is_portrait(rotation: Rotation) -> bool {
    rotation.get() % 2 == 1
}

/// Turns a canvas-sized image into the 480x128 frame the panel gets: step 1 rotates 90
/// degrees clockwise, 2 by 180, 3 counter-clockwise; 0 is the identity.
pub(crate) fn to_panel(canvas: RgbImage, rotation: Rotation) -> RgbImage {
    match rotation.get() {
        1 => imageops::rotate90(&canvas),
        2 => imageops::rotate180(&canvas),
        3 => imageops::rotate270(&canvas),
        _ => canvas,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    const RED: Rgb<u8> = Rgb([255, 0, 0]);
    const GREEN: Rgb<u8> = Rgb([0, 255, 0]);
    const BLUE: Rgb<u8> = Rgb([0, 0, 255]);
    const WHITE: Rgb<u8> = Rgb([255, 255, 255]);

    fn rotation(step: u8) -> Rotation {
        Rotation::try_from(step).unwrap()
    }

    /// A canvas of four coloured quadrants: top-left red, top-right green, bottom-left blue,
    /// bottom-right white.
    fn quadrants((w, h): (u32, u32)) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| match (x < w / 2, y < h / 2) {
            (true, true) => RED,
            (false, true) => GREEN,
            (true, false) => BLUE,
            (false, false) => WHITE,
        })
    }

    /// The colours at the panel's four corners: top-left, top-right, bottom-left, bottom-right.
    fn corners(img: &RgbImage) -> [Rgb<u8>; 4] {
        let (w, h) = img.dimensions();
        [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)].map(|(x, y)| *img.get_pixel(x, y))
    }

    #[test]
    fn canvas_sizes() {
        assert_eq!(canvas_size(rotation(0)), (480, 128));
        assert_eq!(canvas_size(rotation(1)), (128, 480));
        assert_eq!(canvas_size(rotation(2)), (480, 128));
        assert_eq!(canvas_size(rotation(3)), (128, 480));
    }

    #[test]
    fn steps_0_and_2_stay_landscape() {
        let normal = to_panel(quadrants(canvas_size(rotation(0))), rotation(0));
        assert_eq!(normal.dimensions(), (480, 128));
        assert_eq!(corners(&normal), [RED, GREEN, BLUE, WHITE]);
        let flipped = to_panel(quadrants(canvas_size(rotation(2))), rotation(2));
        assert_eq!(flipped.dimensions(), (480, 128));
        assert_eq!(corners(&flipped), [WHITE, BLUE, GREEN, RED]);
    }

    #[test]
    fn step_1_turns_portrait_clockwise_top_to_the_right() {
        let panel = to_panel(quadrants(canvas_size(rotation(1))), rotation(1));
        assert_eq!(panel.dimensions(), (480, 128));
        // Portrait top (red, green) ends up on the right; portrait left side goes to the top.
        assert_eq!(corners(&panel), [BLUE, RED, WHITE, GREEN]);
    }

    #[test]
    fn step_3_turns_portrait_counter_clockwise_top_to_the_left() {
        let panel = to_panel(quadrants(canvas_size(rotation(3))), rotation(3));
        assert_eq!(panel.dimensions(), (480, 128));
        assert_eq!(corners(&panel), [GREEN, WHITE, RED, BLUE]);
    }
}
