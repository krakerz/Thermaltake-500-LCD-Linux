//! A source rectangle chosen by the user, applied before fitting.

use std::fmt;

use crate::RenderError;

/// A rectangle in source pixels, after EXIF orientation or video rotation metadata.
///
/// It replaces the whole source as the input of [`Fit`](crate::Fit), so its aspect ratio
/// need not match the panel's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Crop {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width, at least 1.
    pub width: u32,
    /// Height, at least 1.
    pub height: u32,
}

impl Crop {
    /// Checks that the rectangle is non-empty and inside a `width` x `height` source.
    ///
    /// # Errors
    /// Returns [`RenderError::BadCrop`] otherwise.
    pub fn check(&self, width: u32, height: u32) -> Result<(), RenderError> {
        let fits = |start: u32, len: u32, max: u32| {
            len > 0 && u64::from(start) + u64::from(len) <= u64::from(max)
        };
        if fits(self.x, self.width, width) && fits(self.y, self.height, height) {
            Ok(())
        } else {
            Err(RenderError::BadCrop {
                crop: *self,
                width,
                height,
            })
        }
    }
}

impl fmt::Display for Crop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{},{},{}", self.x, self.y, self.width, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crop(x: u32, y: u32, width: u32, height: u32) -> Crop {
        Crop {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn inside_and_full_frame_pass() {
        assert!(crop(0, 0, 200, 100).check(200, 100).is_ok());
        assert!(crop(10, 20, 190, 80).check(200, 100).is_ok());
    }

    #[test]
    fn empty_outside_and_overflow_fail() {
        for bad in [
            crop(0, 0, 0, 10),
            crop(0, 0, 10, 0),
            crop(1, 0, 200, 100),
            crop(0, 1, 200, 100),
            crop(u32::MAX, 0, 1, 1),
            crop(1, 1, u32::MAX, 1),
        ] {
            assert!(
                matches!(bad.check(200, 100), Err(RenderError::BadCrop { .. })),
                "{bad}"
            );
        }
    }
}
