//! The boot animation: JPEG frames the panel stores in its own memory and plays at power-on.
//!
//! Uploading writes to the panel's flash, so only an animation within what the capture
//! showed or what the vendor's own files hold (up to 75 baseline 480x128 4:2:0 JPEGs of at
//! most 16 KiB, at most ten 64 KiB data blocks, 40 to 1000 ms per frame) can be turned into
//! packets.

use crate::FrameError;
use crate::frame::{Chroma, validate};
use crate::packet::{OutputPacket, boot_packets};

/// Most frames accepted: the 75 frames of the vendor's factory boot animation.
pub const MAX_BOOT_FRAMES: usize = 75;
/// Shortest frame delay accepted, in milliseconds: the factory animation's 40 ms (25 fps).
pub const MIN_BOOT_FRAME_DELAY_MS: u16 = 40;
/// Longest frame delay accepted, in milliseconds.
pub const MAX_BOOT_FRAME_DELAY_MS: u16 = 1000;
/// Largest JPEG accepted per frame; the capture's frames were 12.3 to 15.5 KB.
pub const MAX_BOOT_FRAME_BYTES: usize = 16 * 1024;
/// Size of every data block except the last: 65 packets of 1008 bytes plus 16.
pub const BOOT_BLOCK_BYTES: usize = 65536;
/// Most data blocks accepted: the ten of the capture.
pub const MAX_BOOT_BLOCKS: usize = 10;
/// Most JPEG data accepted in total.
pub const MAX_BOOT_DATA_BYTES: usize = MAX_BOOT_BLOCKS * BOOT_BLOCK_BYTES;

/// Why a set of JPEGs cannot be a boot animation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BootError {
    /// No frames were given.
    #[error("a boot animation needs at least one frame")]
    NoFrames,
    /// More than [`MAX_BOOT_FRAMES`] frames.
    #[error("boot animation has {count} frames, the limit is {max}")]
    TooManyFrames {
        /// Frames given.
        count: usize,
        /// Frames allowed.
        max: usize,
    },
    /// The frame delay is outside [`MIN_BOOT_FRAME_DELAY_MS`]..=[`MAX_BOOT_FRAME_DELAY_MS`].
    #[error("boot animation frame delay is {ms} ms, it must be {min} to {max} ms")]
    BadFrameDelay {
        /// Delay given.
        ms: u16,
        /// Shortest delay allowed.
        min: u16,
        /// Longest delay allowed.
        max: u16,
    },
    /// A frame failed the JPEG checks (the index counts from 0).
    #[error("boot animation frame {index}: {source}")]
    Frame {
        /// Index of the frame.
        index: usize,
        /// What was wrong.
        source: FrameError,
    },
    /// The JPEGs together exceed [`MAX_BOOT_DATA_BYTES`].
    #[error("boot animation data is {len} bytes, the limit is {max}")]
    TooMuchData {
        /// Total size.
        len: usize,
        /// Allowed size.
        max: usize,
    },
}

/// JPEG frames checked to be what the panel was seen to accept as a boot animation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootAnimation {
    frame_delay_ms: u16,
    sizes: Vec<u32>,
    data: Vec<u8>,
}

impl BootAnimation {
    /// Validates the delay shown after every frame (in milliseconds, within
    /// [`MIN_BOOT_FRAME_DELAY_MS`]..=[`MAX_BOOT_FRAME_DELAY_MS`]), every JPEG (baseline,
    /// 480x128, 4:2:0, at most [`MAX_BOOT_FRAME_BYTES`]) and the totals:
    /// 1..=[`MAX_BOOT_FRAMES`] frames, at most [`MAX_BOOT_DATA_BYTES`] bytes.
    ///
    /// # Errors
    /// Returns a [`BootError`] naming the first check that failed.
    pub fn from_jpegs(jpegs: &[Vec<u8>], frame_delay_ms: u16) -> Result<Self, BootError> {
        if !(MIN_BOOT_FRAME_DELAY_MS..=MAX_BOOT_FRAME_DELAY_MS).contains(&frame_delay_ms) {
            return Err(BootError::BadFrameDelay {
                ms: frame_delay_ms,
                min: MIN_BOOT_FRAME_DELAY_MS,
                max: MAX_BOOT_FRAME_DELAY_MS,
            });
        }
        if jpegs.is_empty() {
            return Err(BootError::NoFrames);
        }
        if jpegs.len() > MAX_BOOT_FRAMES {
            return Err(BootError::TooManyFrames {
                count: jpegs.len(),
                max: MAX_BOOT_FRAMES,
            });
        }
        let mut sizes = Vec::with_capacity(jpegs.len());
        let mut total = 0usize;
        for (index, jpeg) in jpegs.iter().enumerate() {
            validate(jpeg, MAX_BOOT_FRAME_BYTES, Chroma::Yuv420)
                .map_err(|source| BootError::Frame { index, source })?;
            sizes.push(u32::try_from(jpeg.len()).expect("frames are at most 16 KiB"));
            total += jpeg.len();
        }
        if total > MAX_BOOT_DATA_BYTES {
            return Err(BootError::TooMuchData {
                len: total,
                max: MAX_BOOT_DATA_BYTES,
            });
        }
        let mut data = Vec::with_capacity(total);
        for jpeg in jpegs {
            data.extend_from_slice(jpeg);
        }
        Ok(Self {
            frame_delay_ms,
            sizes,
            data,
        })
    }

    /// Number of frames.
    pub fn frame_count(&self) -> usize {
        self.sizes.len()
    }

    /// Delay of every frame in milliseconds.
    pub fn frame_delay_ms(&self) -> u16 {
        self.frame_delay_ms
    }

    /// JPEG size of every frame, in order.
    pub fn frame_sizes(&self) -> &[u32] {
        &self.sizes
    }

    /// Total JPEG bytes.
    pub fn data_len(&self) -> usize {
        self.data.len()
    }

    /// Number of data blocks (blocks 1..=N; the table is block 0).
    pub fn block_count(&self) -> usize {
        self.data.len().div_ceil(BOOT_BLOCK_BYTES)
    }

    /// The packets of every block in send order: index 0 is the table (one packet), index
    /// `n` is data block `n`. Data blocks are 65536 bytes except the last, and every packet
    /// of the last one carries the final-block flag.
    pub fn blocks(&self) -> Vec<Vec<OutputPacket>> {
        let mut table = Vec::with_capacity(4 + 4 * self.sizes.len());
        table.extend_from_slice(
            &u16::try_from(self.sizes.len())
                .expect("at most 75 frames")
                .to_le_bytes(),
        );
        table.extend_from_slice(&self.frame_delay_ms.to_le_bytes());
        for size in &self.sizes {
            table.extend_from_slice(&size.to_le_bytes());
        }
        let count = self.block_count();
        let mut blocks = vec![boot_packets(0, &table, false)];
        for (n, chunk) in self.data.chunks(BOOT_BLOCK_BYTES).enumerate() {
            let number = u8::try_from(n + 1).expect("at most 10 data blocks");
            blocks.push(boot_packets(number, chunk, n + 1 == count));
        }
        blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{build_jpeg, sample_jpeg};
    use crate::{HEIGHT, WIDTH};

    fn jpeg(padding: usize) -> Vec<u8> {
        build_jpeg(0xc0, WIDTH, HEIGHT, 0x22, padding)
    }

    /// A 4:2:0 JPEG of exactly `len` bytes.
    fn jpeg_of(len: usize) -> Vec<u8> {
        let base = jpeg(0).len() + 4;
        let out = jpeg(len - base);
        assert_eq!(out.len(), len);
        out
    }

    fn anim(sizes: &[usize]) -> BootAnimation {
        let jpegs: Vec<Vec<u8>> = sizes.iter().map(|&n| jpeg_of(n)).collect();
        BootAnimation::from_jpegs(&jpegs, 100).unwrap()
    }

    #[test]
    fn rejects_empty_too_many_and_bad_frames() {
        assert_eq!(
            BootAnimation::from_jpegs(&[], 100),
            Err(BootError::NoFrames)
        );
        let many = vec![jpeg(0); MAX_BOOT_FRAMES + 1];
        assert_eq!(
            BootAnimation::from_jpegs(&many, 100),
            Err(BootError::TooManyFrames {
                count: 76,
                max: MAX_BOOT_FRAMES
            })
        );
        assert_eq!(
            BootAnimation::from_jpegs(&[jpeg(0), sample_jpeg(0)], 100),
            Err(BootError::Frame {
                index: 1,
                source: FrameError::WrongComponents
            })
        );
        assert!(matches!(
            BootAnimation::from_jpegs(&[jpeg_of(MAX_BOOT_FRAME_BYTES + 1)], 100),
            Err(BootError::Frame {
                index: 0,
                source: FrameError::TooLarge { .. }
            })
        ));
        assert!(BootAnimation::from_jpegs(&[jpeg_of(MAX_BOOT_FRAME_BYTES)], 100).is_ok());
    }

    #[test]
    fn rejects_too_much_data() {
        // 75 frames of 16 KiB = 1 228 800 bytes > 655 360.
        let jpegs = vec![jpeg_of(MAX_BOOT_FRAME_BYTES); MAX_BOOT_FRAMES];
        assert_eq!(
            BootAnimation::from_jpegs(&jpegs, 100),
            Err(BootError::TooMuchData {
                len: MAX_BOOT_FRAMES * MAX_BOOT_FRAME_BYTES,
                max: MAX_BOOT_DATA_BYTES
            })
        );
    }

    #[test]
    fn rejects_delays_outside_40_to_1000_ms() {
        let one = [jpeg(0)];
        for ms in [0, 39, 1001, u16::MAX] {
            assert_eq!(
                BootAnimation::from_jpegs(&one, ms),
                Err(BootError::BadFrameDelay {
                    ms,
                    min: 40,
                    max: 1000
                })
            );
        }
        for ms in [40, 100, 1000] {
            let a = BootAnimation::from_jpegs(&one, ms).unwrap();
            assert_eq!(a.frame_delay_ms(), ms);
        }
    }

    #[test]
    fn accepts_75_frames_and_writes_the_delay_into_the_table() {
        let a = BootAnimation::from_jpegs(&vec![jpeg(0); MAX_BOOT_FRAMES], 40).unwrap();
        assert_eq!(a.frame_count(), 75);
        let table = a.blocks()[0][0].as_bytes().to_vec();
        assert_eq!(table[16..20], [75, 0, 40, 0]);
    }

    #[test]
    fn table_delay_is_little_endian() {
        let jpegs = [jpeg(0), jpeg(0)];
        let a = BootAnimation::from_jpegs(&jpegs, 1000).unwrap();
        assert_eq!(a.blocks()[0][0].as_bytes()[16..20], [2, 0, 0xe8, 0x03]);
        let a = BootAnimation::from_jpegs(&jpegs, 40).unwrap();
        assert_eq!(a.blocks()[0][0].as_bytes()[16..20], [2, 0, 0x28, 0x00]);
    }

    #[test]
    fn table_packet_is_byte_exact() {
        let a = anim(&[300, 500, 700]);
        let blocks = a.blocks();
        assert_eq!(blocks[0].len(), 1);
        let b = blocks[0][0].as_bytes();
        // 02 0c B=0 00 00 00 last=1 idx=0 len=16 F=0 28 00 a2 8a
        assert_eq!(
            b[..16],
            [
                0x02, 0x0c, 0, 0, 0, 0, 1, 0, 0, 16, 0, 0, 0x28, 0x00, 0xa2, 0x8a
            ]
        );
        assert_eq!(
            b[16..32],
            [3, 0, 100, 0, 0x2c, 1, 0, 0, 0xf4, 1, 0, 0, 0xbc, 2, 0, 0]
        );
        assert!(b[32..].iter().all(|&x| x == 0));
    }

    #[test]
    fn full_block_splits_at_65536_and_final_block_has_f_flag() {
        // 5 x 16000 = 80000: block 1 = 65536, block 2 = 14464.
        let a = anim(&[16000; 5]);
        assert_eq!((a.data_len(), a.block_count()), (80000, 2));
        let blocks = a.blocks();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[1].len(), 66);
        let first = blocks[1][0].as_bytes();
        assert_eq!(
            first[..16],
            [
                0x02, 0x0c, 1, 0, 0, 0, 0, 0, 0, 0xf0, 0x03, 0, 0x28, 0, 0xa2, 0x8a
            ]
        );
        assert_eq!(first[16..20], [0xff, 0xd8, 0xff, 0xe0]);
        let last = blocks[1][65].as_bytes();
        assert_eq!(
            last[..16],
            [
                0x02, 0x0c, 1, 0, 0, 0, 1, 65, 0, 16, 0, 0, 0x28, 0, 0xa2, 0x8a
            ]
        );
        assert!(last[32..].iter().all(|&x| x == 0));
        // Final block: 14464 = 14 x 1008 + 352; F = 1 on every packet.
        assert_eq!(blocks[2].len(), 15);
        let final_last = blocks[2][14].as_bytes();
        assert_eq!(
            final_last[..16],
            [
                0x02, 0x0c, 2, 0, 0, 0, 1, 14, 0, 0x60, 0x01, 1, 0x28, 0, 0xa2, 0x8a
            ]
        );
        let f: Vec<u8> = blocks[2].iter().map(|p| p.as_bytes()[11]).collect();
        assert!(f.iter().all(|&x| x == 1));
        let others = blocks[..2].iter().flatten().all(|p| p.as_bytes()[11] == 0);
        assert!(others);
        // The stream continues across the boundary: block 2 starts with JPEG 4's tail.
        let joined: Vec<u8> = blocks[1..]
            .iter()
            .flatten()
            .flat_map(|p| {
                let b = p.as_bytes();
                let len = usize::from(u16::from_le_bytes([b[9], b[10]]));
                b[16..16 + len].to_vec()
            })
            .collect();
        let want: Vec<u8> = (0..5).flat_map(|_| jpeg_of(16000)).collect();
        assert_eq!(joined, want);
    }

    #[test]
    fn exactly_one_full_block_is_one_final_block() {
        let a = anim(&[16384, 16384, 16384, 16384]);
        assert_eq!((a.data_len(), a.block_count()), (65536, 1));
        let blocks = a.blocks();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1].len(), 66);
        assert_eq!(blocks[1][65].as_bytes()[6..12], [1, 65, 0, 16, 0, 1]);
    }

    #[test]
    fn exact_multiple_of_a_block_adds_no_empty_block() {
        let a = anim(&[16384; 8]);
        assert_eq!(a.block_count(), 2);
        let blocks = a.blocks();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[2].len(), 66);
        assert_eq!(blocks[2][65].as_bytes()[11], 1);
    }

    #[test]
    fn every_packet_carries_the_trailer_and_block_number() {
        let a = anim(&[16000; 5]);
        for (n, block) in a.blocks().iter().enumerate() {
            for p in block {
                let b = p.as_bytes();
                assert_eq!(b[..2], [0x02, 0x0c]);
                assert_eq!(usize::from(b[2]), n);
                assert_eq!(b[3..6], [0, 0, 0]);
                assert_eq!(b[12..16], [0x28, 0x00, 0xa2, 0x8a]);
            }
        }
    }
}
