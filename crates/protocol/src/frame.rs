//! JPEG validation: only frames the panel was seen to accept can be sent.

use std::fmt;
use std::sync::Arc;

use crate::packet::CHUNK_PAYLOAD_LEN;
use crate::{HEIGHT, WIDTH};

/// Largest JPEG accepted: 12 packets, the largest frame seen in the capture.
pub const MAX_FRAME_BYTES: usize = 12 * CHUNK_PAYLOAD_LEN;

/// A JPEG checked to be what the panel was seen to accept.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    // Arc: animations loop over the same frames.
    jpeg: Arc<[u8]>,
}

impl Frame {
    /// Validates `jpeg`: baseline, 480x128, 3 components with 4:2:2 sampling, size cap.
    ///
    /// # Errors
    /// Returns a [`FrameError`] naming the first check that failed.
    pub fn from_jpeg(jpeg: Vec<u8>) -> Result<Self, FrameError> {
        validate(&jpeg, MAX_FRAME_BYTES, Chroma::Yuv422)?;
        Ok(Self { jpeg: jpeg.into() })
    }

    /// The JPEG bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.jpeg
    }

    /// Length of the JPEG in bytes.
    pub fn byte_len(&self) -> usize {
        self.jpeg.len()
    }

    /// Number of output packets this frame needs.
    pub fn packet_count(&self) -> usize {
        self.jpeg.len().div_ceil(CHUNK_PAYLOAD_LEN)
    }
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Frame({} bytes)", self.jpeg.len())
    }
}

/// Why a JPEG was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FrameError {
    /// More than [`MAX_FRAME_BYTES`].
    #[error("frame is {len} bytes, the limit is {max}")]
    TooLarge {
        /// Actual size.
        len: usize,
        /// Allowed size.
        max: usize,
    },
    /// Missing the start (FF D8) or end (FF D9) marker.
    #[error("not a JPEG: missing start or end marker")]
    NotJpeg,
    /// A segment runs past the end of the data.
    #[error("truncated JPEG")]
    Truncated,
    /// A non-baseline frame header (progressive, lossless, arithmetic, ...).
    #[error("not a baseline JPEG (frame marker 0x{marker:02x})")]
    NotBaseline {
        /// The frame header marker found.
        marker: u8,
    },
    /// Image data starts before any frame header.
    #[error("no frame header before the image data")]
    MissingFrameHeader,
    /// Dimensions other than 480x128.
    #[error("JPEG is {width}x{height}, the panel needs 480x128")]
    WrongSize {
        /// Width found.
        width: u16,
        /// Height found.
        height: u16,
    },
    /// Not 3 components with the chroma subsampling this image kind needs.
    #[error("JPEG must have 3 components with the expected chroma subsampling")]
    WrongComponents,
    /// Sample precision other than 8 bits.
    #[error("JPEG sample precision must be 8 bits")]
    WrongPrecision,
}

fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

/// Chroma subsampling a JPEG must use (the SOF0 H/V nibbles of Y, Cb, Cr).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Chroma {
    /// Live frames: Y 0x21, Cb 0x11, Cr 0x11.
    Yuv422,
    /// Standby images: Y 0x22, Cb 0x11, Cr 0x11.
    Yuv420,
}

impl Chroma {
    const fn sampling(self) -> [u8; 3] {
        match self {
            Self::Yuv422 => [0x21, 0x11, 0x11],
            Self::Yuv420 => [0x22, 0x11, 0x11],
        }
    }
}

pub(crate) fn validate(jpeg: &[u8], max_len: usize, chroma: Chroma) -> Result<(), FrameError> {
    if jpeg.len() > max_len {
        return Err(FrameError::TooLarge {
            len: jpeg.len(),
            max: max_len,
        });
    }
    if jpeg.len() < 4 || jpeg[..2] != [0xff, 0xd8] || jpeg[jpeg.len() - 2..] != [0xff, 0xd9] {
        return Err(FrameError::NotJpeg);
    }
    let mut i = 2;
    loop {
        if i >= jpeg.len() {
            return Err(FrameError::Truncated);
        }
        if jpeg[i] != 0xff {
            return Err(FrameError::NotJpeg);
        }
        while i < jpeg.len() && jpeg[i] == 0xff {
            i += 1;
        }
        let Some(&marker) = jpeg.get(i) else {
            return Err(FrameError::Truncated);
        };
        i += 1;
        match marker {
            0x01 | 0xd0..=0xd7 => continue,
            0xd9 | 0xda => return Err(FrameError::MissingFrameHeader),
            _ => {}
        }
        if i + 2 > jpeg.len() {
            return Err(FrameError::Truncated);
        }
        let seg_len = usize::from(be16(&jpeg[i..]));
        if seg_len < 2 || i + seg_len > jpeg.len() {
            return Err(FrameError::Truncated);
        }
        let segment = &jpeg[i + 2..i + seg_len];
        match marker {
            0xc0 => return check_sof0(segment, chroma),
            0xc1..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                return Err(FrameError::NotBaseline { marker });
            }
            _ => i += seg_len,
        }
    }
}

/// `segment` is the SOF0 body after the length: P, Y, X, Nf, then (id, HV, Tq) per component.
fn check_sof0(segment: &[u8], chroma: Chroma) -> Result<(), FrameError> {
    if segment.len() < 6 {
        return Err(FrameError::Truncated);
    }
    if segment[0] != 8 {
        return Err(FrameError::WrongPrecision);
    }
    let height = be16(&segment[1..]);
    let width = be16(&segment[3..]);
    if (width, height) != (WIDTH, HEIGHT) {
        return Err(FrameError::WrongSize { width, height });
    }
    if segment[5] != 3 {
        return Err(FrameError::WrongComponents);
    }
    if segment.len() < 6 + 3 * 3 {
        return Err(FrameError::Truncated);
    }
    let sampling = [segment[7], segment[10], segment[13]];
    if sampling != chroma.sampling() {
        return Err(FrameError::WrongComponents);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn sample_jpeg(padding: usize) -> Vec<u8> {
    build_jpeg(0xc0, WIDTH, HEIGHT, 0x21, padding)
}

#[cfg(test)]
pub(crate) fn build_jpeg(sof: u8, width: u16, height: u16, luma_hv: u8, padding: usize) -> Vec<u8> {
    let mut out = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00];
    if padding > 0 {
        let len = u16::try_from(padding + 2).unwrap();
        out.extend_from_slice(&[0xff, 0xfe]);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend(std::iter::repeat_n(0x55u8, padding));
    }
    out.extend_from_slice(&[0xff, sof, 0x00, 0x11, 0x08]);
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&[
        0x03, 0x01, luma_hv, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01,
    ]);
    out.extend_from_slice(&[0xff, 0xd9]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_minimal_baseline_jpeg() {
        let frame = Frame::from_jpeg(sample_jpeg(0)).unwrap();
        assert_eq!(frame.packet_count(), 1);
        assert_eq!(frame.byte_len(), frame.as_bytes().len());
        assert_eq!(
            format!("{frame:?}"),
            format!("Frame({} bytes)", frame.byte_len())
        );
    }

    #[test]
    fn accepts_padded_multi_packet_jpeg() {
        assert_eq!(
            Frame::from_jpeg(sample_jpeg(2500)).unwrap().packet_count(),
            3
        );
    }

    #[test]
    fn rejects_progressive() {
        let jpeg = build_jpeg(0xc2, WIDTH, HEIGHT, 0x21, 0);
        assert_eq!(
            Frame::from_jpeg(jpeg),
            Err(FrameError::NotBaseline { marker: 0xc2 })
        );
    }

    #[test]
    fn rejects_420_sampling() {
        let jpeg = build_jpeg(0xc0, WIDTH, HEIGHT, 0x22, 0);
        assert_eq!(Frame::from_jpeg(jpeg), Err(FrameError::WrongComponents));
    }

    #[test]
    fn rejects_wrong_size() {
        let jpeg = build_jpeg(0xc0, 481, HEIGHT, 0x21, 0);
        assert_eq!(
            Frame::from_jpeg(jpeg),
            Err(FrameError::WrongSize {
                width: 481,
                height: 128
            })
        );
    }

    #[test]
    fn rejects_missing_eoi_and_soi() {
        let mut jpeg = sample_jpeg(0);
        jpeg.truncate(jpeg.len() - 2);
        assert_eq!(Frame::from_jpeg(jpeg), Err(FrameError::NotJpeg));
        assert_eq!(
            Frame::from_jpeg(vec![0, 1, 2, 3, 4]),
            Err(FrameError::NotJpeg)
        );
    }

    #[test]
    fn rejects_too_large() {
        let jpeg = vec![0u8; 13 * CHUNK_PAYLOAD_LEN];
        assert_eq!(
            Frame::from_jpeg(jpeg),
            Err(FrameError::TooLarge {
                len: 13 * CHUNK_PAYLOAD_LEN,
                max: MAX_FRAME_BYTES
            })
        );
    }

    #[test]
    fn rejects_truncated_segment() {
        let jpeg = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0xff, 0xff, 0xd9];
        assert_eq!(Frame::from_jpeg(jpeg), Err(FrameError::Truncated));
    }

    #[test]
    fn rejects_scan_before_frame_header() {
        let jpeg = vec![0xff, 0xd8, 0xff, 0xda, 0xff, 0xd9];
        assert_eq!(Frame::from_jpeg(jpeg), Err(FrameError::MissingFrameHeader));
    }

    #[test]
    fn rejects_wrong_precision() {
        let mut jpeg = sample_jpeg(0);
        let sof = jpeg.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
        jpeg[sof + 4] = 12;
        assert_eq!(Frame::from_jpeg(jpeg), Err(FrameError::WrongPrecision));
    }
}
