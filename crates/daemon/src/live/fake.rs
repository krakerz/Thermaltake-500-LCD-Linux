//! A scripted [`Capturer`] for tests.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use image::{Rgb, RgbImage};

use super::capture::{CaptureError, Capturer};

/// Yields solid-colour frames numbered by the capture counter, or scripted errors first.
#[derive(Debug)]
pub struct FakeCapturer {
    width: u32,
    height: u32,
    script: VecDeque<CaptureError>,
    count: Arc<AtomicU64>,
    problem: Option<String>,
}

impl FakeCapturer {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            script: VecDeque::new(),
            count: Arc::new(AtomicU64::new(0)),
            problem: None,
        }
    }

    /// Makes the next captures fail with these errors, in order.
    pub fn with_errors(mut self, errors: impl IntoIterator<Item = CaptureError>) -> Self {
        self.script.extend(errors);
        self
    }

    /// Makes the capturer report this problem alongside every frame.
    pub fn with_problem(mut self, problem: &str) -> Self {
        self.problem = Some(problem.to_owned());
        self
    }

    /// The number of successful captures, shared with the capturer.
    pub fn counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.count)
    }
}

impl Capturer for FakeCapturer {
    fn capture(&mut self) -> Result<RgbImage, CaptureError> {
        if let Some(error) = self.script.pop_front() {
            return Err(error);
        }
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        // The low byte of the frame number is the red channel.
        Ok(RgbImage::from_pixel(
            self.width,
            self.height,
            Rgb([(n % 256) as u8, 0, 0]),
        ))
    }

    fn problem(&self) -> Option<String> {
        self.problem.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_frames_and_plays_the_script() {
        let mut fake =
            FakeCapturer::new(4, 2).with_errors([CaptureError::Gone("sway died".to_owned())]);
        assert!(matches!(fake.capture(), Err(CaptureError::Gone(_))));
        let first = fake.capture().expect("first frame");
        let second = fake.capture().expect("second frame");
        assert_eq!(first.dimensions(), (4, 2));
        assert_eq!(first.get_pixel(0, 0), &Rgb([1, 0, 0]));
        assert_eq!(second.get_pixel(0, 0), &Rgb([2, 0, 0]));
        assert_eq!(fake.counter().load(Ordering::SeqCst), 2);
    }
}
