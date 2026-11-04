//! Holds back half-drawn frames of a live source.

use std::sync::Arc;
use std::time::{Duration, Instant};

use image::RgbImage;

/// How long a changed frame may wait for a matching capture before it is shown anyway,
/// so constantly changing content still updates about four times a second.
pub const MAX_HOLD: Duration = Duration::from_millis(250);

/// Decides which captured frames are published.
///
/// A frame that differs from the previous capture is held back until the next capture is
/// identical to it, or until it has waited [`MAX_HOLD`]. Identical frames pass at once.
#[derive(Debug, Default)]
pub struct SteadyGate {
    last: Option<Arc<RgbImage>>,
    /// When the current run of changing frames began; `None` while the picture is steady.
    changing_since: Option<Instant>,
}

impl SteadyGate {
    /// Takes the next capture; returns it when it should be published.
    ///
    /// With `steady` off every frame passes. `now` is the capture time.
    pub fn offer(
        &mut self,
        img: Arc<RgbImage>,
        steady: bool,
        now: Instant,
    ) -> Option<Arc<RgbImage>> {
        let previous = self.last.replace(img);
        let current = self.last.as_ref()?;
        if !steady {
            self.changing_since = None;
            return Some(Arc::clone(current));
        }
        let Some(previous) = previous else {
            return Some(Arc::clone(current));
        };
        if previous.dimensions() == current.dimensions() && previous.as_raw() == current.as_raw() {
            self.changing_since = None;
            return Some(Arc::clone(current));
        }
        let since = *self.changing_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= MAX_HOLD {
            self.changing_since = None;
            return Some(Arc::clone(current));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use image::Rgb;

    use super::*;

    fn solid(value: u8) -> Arc<RgbImage> {
        Arc::new(RgbImage::from_pixel(4, 2, Rgb([value, 0, 0])))
    }

    fn red(img: &Option<Arc<RgbImage>>) -> Option<u8> {
        img.as_ref().map(|img| img.get_pixel(0, 0).0[0])
    }

    #[test]
    fn publishes_only_stable_frames() {
        let mut gate = SteadyGate::default();
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        // A, B (half drawn), C, C.
        assert_eq!(red(&gate.offer(solid(1), true, ms(0))), Some(1));
        assert_eq!(red(&gate.offer(solid(2), true, ms(66))), None);
        assert_eq!(red(&gate.offer(solid(3), true, ms(133))), None);
        assert_eq!(red(&gate.offer(solid(3), true, ms(200))), Some(3));
        // Identical frames keep passing.
        assert_eq!(red(&gate.offer(solid(3), true, ms(266))), Some(3));
    }

    #[test]
    fn constantly_changing_content_updates_after_the_hold() {
        let mut gate = SteadyGate::default();
        let t = Instant::now();
        assert!(gate.offer(solid(0), true, t).is_some());
        let mut published = Vec::new();
        for n in 1..=10u8 {
            let at = t + Duration::from_millis(u64::from(n) * 100);
            if let Some(img) = gate.offer(solid(n), true, at) {
                published.push((n, img.get_pixel(0, 0).0[0]));
            }
        }
        // The change starts at 100 ms, so the frame at 400 ms is the first past the hold.
        assert_eq!(published, [(4, 4), (8, 8)]);
    }

    #[test]
    fn off_passes_every_frame() {
        let mut gate = SteadyGate::default();
        let t = Instant::now();
        for n in 0..5u8 {
            assert_eq!(red(&gate.offer(solid(n), false, t)), Some(n));
        }
    }

    #[test]
    fn a_size_change_counts_as_a_change() {
        let mut gate = SteadyGate::default();
        let t = Instant::now();
        assert!(gate.offer(solid(1), true, t).is_some());
        let bigger = Arc::new(RgbImage::from_pixel(8, 2, Rgb([1, 0, 0])));
        assert!(gate.offer(bigger, true, t).is_none());
    }
}
