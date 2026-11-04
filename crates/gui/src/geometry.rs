//! Pan/zoom math of the crop editor, in source pixels.

use tt500_lcd_ipc::{Crop, Fit, Size};

/// Panel aspect ratio (480:128).
pub const PANEL_ASPECT: f64 = 480.0 / 128.0;

/// Narrowest crop the editor allows, in source pixels (8x magnification on the panel).
pub const MIN_CROP_WIDTH: f64 = 60.0;

/// A rectangle in floating-point pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub width: f64,
    /// Height.
    pub height: f64,
}

/// Pan/zoom state of the crop editor over a source of known size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropView {
    src_w: f64,
    src_h: f64,
    zoom: f64,
    cx: f64,
    cy: f64,
}

impl CropView {
    /// Zoom 1, centred. Source dimensions below 1 are raised to 1.
    #[must_use]
    pub fn new(size: Size) -> Self {
        let src_w = f64::from(size.width.max(1));
        let src_h = f64::from(size.height.max(1));
        let mut view = Self {
            src_w,
            src_h,
            zoom: 1.0,
            cx: src_w / 2.0,
            cy: src_h / 2.0,
        };
        view.clamp();
        view
    }

    /// The view showing `crop` (the daemon's content), clamped to the source.
    #[must_use]
    pub fn from_crop(size: Size, crop: Crop) -> Self {
        let mut view = Self::new(size);
        view.zoom = view.base().0 / f64::from(crop.width.max(1));
        view.cx = f64::from(crop.x) + f64::from(crop.width) / 2.0;
        view.cy = f64::from(crop.y) + f64::from(crop.height) / 2.0;
        view.clamp();
        view
    }

    /// The largest `PANEL_ASPECT` rectangle inside the source.
    fn base(&self) -> (f64, f64) {
        if self.src_w / self.src_h >= PANEL_ASPECT {
            (self.src_h * PANEL_ASPECT, self.src_h)
        } else {
            (self.src_w, self.src_w / PANEL_ASPECT)
        }
    }

    fn visible_size(&self) -> (f64, f64) {
        let (w, h) = self.base();
        (w / self.zoom, h / self.zoom)
    }

    fn clamp(&mut self) {
        self.zoom = self.zoom.clamp(1.0, self.max_zoom());
        let (w, h) = self.visible_size();
        self.cx = self.cx.clamp(w / 2.0, (self.src_w - w / 2.0).max(w / 2.0));
        self.cy = self.cy.clamp(h / 2.0, (self.src_h - h / 2.0).max(h / 2.0));
    }

    /// The largest zoom factor: the crop never gets narrower than [`MIN_CROP_WIDTH`].
    #[must_use]
    pub fn max_zoom(&self) -> f64 {
        (self.base().0 / MIN_CROP_WIDTH).max(1.0)
    }

    /// The current zoom factor (1 = the largest panel-shaped rectangle).
    #[must_use]
    pub fn zoom(&self) -> f64 {
        self.zoom
    }

    /// Zooms about the centre.
    pub fn set_zoom(&mut self, zoom: f64) {
        self.zoom = zoom;
        self.clamp();
    }

    /// Multiplies the zoom by `factor` keeping the source point under `anchor` fixed.
    ///
    /// `anchor` is the pointer in view-normalised coordinates, `0..=1` on both axes.
    pub fn zoom_at(&mut self, factor: f64, anchor: (f64, f64)) {
        let (old_w, old_h) = self.visible_size();
        let px = self.cx - old_w / 2.0 + anchor.0 * old_w;
        let py = self.cy - old_h / 2.0 + anchor.1 * old_h;
        self.zoom = (self.zoom * factor).clamp(1.0, self.max_zoom());
        let (new_w, new_h) = self.visible_size();
        self.cx = px - anchor.0 * new_w + new_w / 2.0;
        self.cy = py - anchor.1 * new_h + new_h / 2.0;
        self.clamp();
    }

    /// Pans by a pointer drag of `dx`, `dy` widget pixels; the image follows the pointer.
    pub fn pan_by(&mut self, dx: f64, dy: f64, view_width: f64) {
        if view_width <= 0.0 {
            return;
        }
        let scale = self.visible_size().0 / view_width;
        self.cx -= dx * scale;
        self.cy -= dy * scale;
        self.clamp();
    }

    /// The visible window in source pixels, for drawing.
    #[must_use]
    pub fn visible(&self) -> Rect {
        let (width, height) = self.visible_size();
        Rect {
            x: self.cx - width / 2.0,
            y: self.cy - height / 2.0,
            width,
            height,
        }
    }

    /// The integer crop sent to the daemon.
    #[must_use]
    pub fn crop(&self) -> Crop {
        let max_w = self.src_w as u32;
        let max_h = self.src_h as u32;
        let (vw, _) = self.visible_size();
        let width = (vw.round() as u32).clamp(1, max_w);
        let height = ((f64::from(width) / PANEL_ASPECT).round() as u32).clamp(1, max_h);
        let x = ((self.cx - f64::from(width) / 2.0).round().max(0.0) as u32).min(max_w - width);
        let y = ((self.cy - f64::from(height) / 2.0).round().max(0.0) as u32).min(max_h - height);
        Crop {
            x,
            y,
            width,
            height,
        }
    }
}

/// Where the whole source lands in a `panel_w` x `panel_h` panel for `fit`.
///
/// Cover may overflow the panel (drawing clips); Contain leaves bars; Stretch fills it.
#[must_use]
pub fn fit_rect(size: Size, fit: Fit, panel_w: f64, panel_h: f64) -> Rect {
    let sw = f64::from(size.width.max(1));
    let sh = f64::from(size.height.max(1));
    let scale = match fit {
        Fit::Stretch => {
            return Rect {
                x: 0.0,
                y: 0.0,
                width: panel_w,
                height: panel_h,
            };
        }
        Fit::Contain => (panel_w / sw).min(panel_h / sh),
        _ => (panel_w / sw).max(panel_h / sh),
    };
    let width = sw * scale;
    let height = sh * scale;
    Rect {
        x: (panel_w - width) / 2.0,
        y: (panel_h - height) / 2.0,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width: u32, height: u32) -> Size {
        Size { width, height }
    }

    fn crop(x: u32, y: u32, width: u32, height: u32) -> Crop {
        Crop {
            x,
            y,
            width,
            height,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn initial_crops() {
        assert_eq!(
            CropView::new(size(1920, 1080)).crop(),
            crop(0, 284, 1920, 512)
        );
        assert_eq!(CropView::new(size(480, 128)).crop(), crop(0, 0, 480, 128));
        assert_eq!(CropView::new(size(100, 10)).crop(), crop(31, 0, 38, 10));
    }

    #[test]
    fn portrait_uses_full_width() {
        let c = CropView::new(size(1080, 1920)).crop();
        assert_eq!((c.width, c.height), (1080, 288));
        assert_eq!(c.x, 0);
        assert_eq!(c.y, 816);
    }

    #[test]
    fn zoom_at_keeps_the_anchored_point() {
        let mut view = CropView::new(size(1920, 1080));
        let anchor = (0.25, 0.4);
        let before = view.visible();
        let point = (
            before.x + anchor.0 * before.width,
            before.y + anchor.1 * before.height,
        );
        view.zoom_at(2.0, anchor);
        let after = view.visible();
        assert!(close(view.zoom(), 2.0));
        assert!(close(after.x + anchor.0 * after.width, point.0), "x");
        assert!(close(after.y + anchor.1 * after.height, point.1), "y");
    }

    #[test]
    fn panning_clamps_at_the_edges() {
        let mut view = CropView::new(size(1920, 1080));
        view.set_zoom(4.0);
        view.pan_by(1.0e6, 1.0e6, 480.0);
        let v = view.visible();
        assert!(close(v.x, 0.0) && close(v.y, 0.0));
        view.pan_by(-1.0e6, -1.0e6, 480.0);
        let v = view.visible();
        assert!(close(v.x + v.width, 1920.0));
        assert!(close(v.y + v.height, 1080.0));
    }

    #[test]
    fn drag_moves_the_image_with_the_pointer() {
        let mut view = CropView::new(size(1920, 1080));
        view.set_zoom(2.0);
        let before = view.visible();
        view.pan_by(48.0, 0.0, 480.0);
        let after = view.visible();
        assert!(close(before.x - after.x, 96.0));
    }

    #[test]
    fn zoom_is_clamped() {
        let mut view = CropView::new(size(1920, 1080));
        view.set_zoom(0.1);
        assert!(close(view.zoom(), 1.0));
        view.set_zoom(1.0e6);
        assert!(close(view.zoom(), 1920.0 / MIN_CROP_WIDTH));
        assert_eq!(view.crop().width, 60);
        let mut small = CropView::new(size(60, 16));
        small.set_zoom(5.0);
        assert!(close(small.zoom(), 1.0));
    }

    #[test]
    fn from_crop_round_trips() {
        let src = size(1920, 1080);
        let mut view = CropView::new(src);
        view.zoom_at(3.0, (0.7, 0.2));
        view.pan_by(-30.0, 12.0, 480.0);
        let crop = view.crop();
        assert_eq!(CropView::from_crop(src, crop).crop(), crop);
    }

    #[test]
    fn fit_rects() {
        let wide = size(960, 128);
        let r = fit_rect(wide, Fit::Cover, 480.0, 128.0);
        assert!(close(r.width, 960.0) && close(r.x, -240.0) && close(r.y, 0.0));
        let r = fit_rect(wide, Fit::Contain, 480.0, 128.0);
        assert!(close(r.width, 480.0) && close(r.height, 64.0) && close(r.y, 32.0));
        let r = fit_rect(wide, Fit::Stretch, 480.0, 128.0);
        assert_eq!(
            r,
            Rect {
                x: 0.0,
                y: 0.0,
                width: 480.0,
                height: 128.0
            }
        );

        let square = size(480, 480);
        let r = fit_rect(square, Fit::Cover, 480.0, 128.0);
        assert!(close(r.width, 480.0) && close(r.height, 480.0) && close(r.y, -176.0));
        let r = fit_rect(square, Fit::Contain, 480.0, 128.0);
        assert!(close(r.width, 128.0) && close(r.x, 176.0) && close(r.y, 0.0));
        let r = fit_rect(square, Fit::Stretch, 480.0, 128.0);
        assert!(close(r.width, 480.0) && close(r.height, 128.0));
    }
}
