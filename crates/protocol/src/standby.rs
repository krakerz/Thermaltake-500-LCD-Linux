//! The standby image: a JPEG the panel stores in its own memory and shows when idle.
//!
//! Uploading writes to the panel's flash, so only a JPEG validated against what the
//! capture showed (baseline, 480x128, 4:2:0, small) can be turned into packets.

use std::fmt;
use std::sync::Arc;

use crate::frame::{Chroma, validate};
use crate::packet::{OutputPacket, standby_packets};
use crate::{FrameError, HEIGHT, WIDTH};

/// Largest standby JPEG accepted: 21 packets per slot (21 × 1008 bytes minus
/// the 8-byte size header), the most TT RGB Plus 2.0 was seen to send.
pub const MAX_STANDBY_JPEG_BYTES: usize = 21 * 1008 - 8;

/// Slots in the order the capture sent them: slot 1 first, then slot 0.
const SLOTS: [u8; 2] = [1, 0];

/// A JPEG checked to be what the panel was seen to accept as a standby image.
#[derive(Clone, PartialEq, Eq)]
pub struct StandbyImage {
    jpeg: Arc<[u8]>,
}

impl StandbyImage {
    /// Validates `jpeg`: baseline, 480x128, 3 components with 4:2:0 sampling, at most
    /// [`MAX_STANDBY_JPEG_BYTES`].
    ///
    /// # Errors
    /// Returns a [`FrameError`] naming the first check that failed.
    pub fn from_jpeg(jpeg: Vec<u8>) -> Result<Self, FrameError> {
        validate(&jpeg, MAX_STANDBY_JPEG_BYTES, Chroma::Yuv420)?;
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

    /// Every packet of the upload, in send order: all packets of slot 1, then the identical
    /// ones for slot 0. The payload stream is `00 00 00 00 e0 01 80 00` + the JPEG.
    pub fn packets(&self) -> Vec<OutputPacket> {
        let mut stream = Vec::with_capacity(8 + self.jpeg.len());
        stream.extend_from_slice(&[0, 0, 0, 0]);
        stream.extend_from_slice(&WIDTH.to_le_bytes());
        stream.extend_from_slice(&HEIGHT.to_le_bytes());
        stream.extend_from_slice(&self.jpeg);
        SLOTS
            .into_iter()
            .flat_map(|slot| standby_packets(&stream, slot))
            .collect()
    }
}

impl fmt::Debug for StandbyImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StandbyImage({} bytes)", self.jpeg.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{build_jpeg, sample_jpeg};

    fn image(padding: usize) -> StandbyImage {
        StandbyImage::from_jpeg(build_jpeg(0xc0, WIDTH, HEIGHT, 0x22, padding)).unwrap()
    }

    #[test]
    fn rejects_422_progressive_wrong_size_and_too_big() {
        assert_eq!(
            StandbyImage::from_jpeg(sample_jpeg(0)),
            Err(FrameError::WrongComponents)
        );
        assert_eq!(
            StandbyImage::from_jpeg(build_jpeg(0xc2, WIDTH, HEIGHT, 0x22, 0)),
            Err(FrameError::NotBaseline { marker: 0xc2 })
        );
        assert_eq!(
            StandbyImage::from_jpeg(build_jpeg(0xc0, 480, 127, 0x22, 0)),
            Err(FrameError::WrongSize {
                width: 480,
                height: 127
            })
        );
        let big = build_jpeg(0xc0, WIDTH, HEIGHT, 0x22, MAX_STANDBY_JPEG_BYTES);
        assert!(matches!(
            StandbyImage::from_jpeg(big),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn rejects_420_for_live_frames_only() {
        assert!(crate::Frame::from_jpeg(build_jpeg(0xc0, WIDTH, HEIGHT, 0x22, 0)).is_err());
    }

    #[test]
    fn packet_layout_is_byte_exact() {
        // JPEG of 2500 + ~40 bytes: stream = 8 + len, 3 packets per slot.
        let img = image(2500);
        let stream_len = 8 + img.byte_len();
        let per_slot = stream_len.div_ceil(1008);
        assert_eq!(per_slot, 3);
        let packets = img.packets();
        assert_eq!(packets.len(), 2 * per_slot);

        let last_len = stream_len - 2 * 1008;
        for (n, p) in packets.iter().enumerate() {
            let b = p.as_bytes();
            let slot = if n < per_slot { 1 } else { 0 };
            let idx = n % per_slot;
            let (last, len) = if idx == per_slot - 1 {
                (1, last_len)
            } else {
                (0, 1008)
            };
            let want = [
                0x02,
                0x0d,
                slot,
                0x00,
                last,
                u8::try_from(idx).unwrap(),
                0x00,
                u8::try_from(len & 0xff).unwrap(),
                u8::try_from(len >> 8).unwrap(),
            ];
            assert_eq!(b[..9], want, "packet {n}");
            assert!(b[9..16].iter().all(|&x| x == 0), "filler of packet {n}");
            assert!(b[16 + len..].iter().all(|&x| x == 0), "padding of {n}");
        }
        // Payload header and JPEG start.
        let first = packets[0].as_bytes();
        assert_eq!(first[16..24], [0, 0, 0, 0, 0xe0, 0x01, 0x80, 0x00]);
        assert_eq!(first[24..28], [0xff, 0xd8, 0xff, 0xe0]);
        // Both slots carry an identical payload.
        for i in 0..per_slot {
            assert_eq!(
                packets[i].as_bytes()[16..],
                packets[per_slot + i].as_bytes()[16..]
            );
        }
    }

    #[test]
    fn single_packet_image_is_last_in_both_slots() {
        let packets = image(0).packets();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].as_bytes()[2..5], [1, 0, 1]);
        assert_eq!(packets[1].as_bytes()[2..5], [0, 0, 1]);
    }

    #[test]
    fn exact_multiple_flags_only_the_final_packet() {
        // Choose padding so that 8 + jpeg length is exactly 2 * 1008.
        let base = image(0).byte_len() + 4; // COM segment adds 4 bytes of framing
        let img = image(2 * 1008 - 8 - base);
        assert_eq!(8 + img.byte_len(), 2 * 1008);
        let packets = img.packets();
        assert_eq!(packets.len(), 4);
        let flags: Vec<u8> = packets.iter().map(|p| p.as_bytes()[4]).collect();
        assert_eq!(flags, [0, 1, 0, 1]);
        assert_eq!(packets[1].as_bytes()[7..9], [0xf0, 0x03]);
    }
}
