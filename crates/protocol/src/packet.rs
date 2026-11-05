//! Fixed reports from the capture; the only bytes that can reach the device.

use std::fmt;

/// Feature report length including the report ID.
pub(crate) const FEATURE_REPORT_LEN: usize = 64;
/// Output report length including the report ID.
pub(crate) const OUTPUT_REPORT_LEN: usize = 1024;
/// JPEG bytes per output report: 1024 minus the 8-byte header.
pub(crate) const CHUNK_PAYLOAD_LEN: usize = 1016;

const OUTPUT_REPORT_ID: u8 = 0x02;
const IMAGE_COMMAND: u8 = 0x09;
const STANDBY_COMMAND: u8 = 0x0d;
const BOOT_COMMAND: u8 = 0x0c;
/// Bytes 12..16 of every boot-animation packet; meaning unknown, replayed as captured.
const BOOT_TRAILER: [u8; 4] = [0x28, 0x00, 0xa2, 0x8a];
/// Header bytes of a standby packet: `02 0d S 00 last idx(2) len(2)` + 7 filler zeros.
const STANDBY_HEADER_LEN: usize = 16;
/// Payload bytes per standby packet.
pub(crate) const STANDBY_CHUNK_LEN: usize = OUTPUT_REPORT_LEN - STANDBY_HEADER_LEN;

/// A start-up SET_FEATURE report (ID 0x03), byte for byte as TT RGB Plus 3.0 sent it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FeatureCommand {
    bytes: [u8; FEATURE_REPORT_LEN],
}

impl FeatureCommand {
    const fn from_prefix(prefix: &[u8]) -> Self {
        let mut bytes = [0u8; FEATURE_REPORT_LEN];
        let mut i = 0;
        while i < prefix.len() {
            bytes[i] = prefix[i];
            i += 1;
        }
        Self { bytes }
    }

    /// The full 64-byte report, report ID first.
    pub fn as_bytes(&self) -> &[u8; FEATURE_REPORT_LEN] {
        &self.bytes
    }
}

impl fmt::Debug for FeatureCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let end = self
            .bytes
            .iter()
            .rposition(|&b| b != 0)
            .map_or(1, |i| i + 1);
        write!(f, "FeatureCommand({:02x?})", &self.bytes[..end])
    }
}

// Meaning unknown (see NOTES.md); replayed verbatim, in this order, once per session.
pub(crate) const STARTUP_18: FeatureCommand = FeatureCommand::from_prefix(&[0x03, 0x18]);
pub(crate) const STARTUP_1A: FeatureCommand = FeatureCommand::from_prefix(&[0x03, 0x1a]);
pub(crate) const STARTUP_0C: FeatureCommand = FeatureCommand::from_prefix(&[
    0x03, 0x0c, 0x64, 0x00, 0x00, 0x00, 0x00, 0xe0, 0x01, 0x80, 0x00, 0x04, 0x05,
]);
pub(crate) const STARTUP_1D: FeatureCommand =
    FeatureCommand::from_prefix(&[0x03, 0x1d, 0x00, 0xff, 0xff, 0xea, 0xff, 0xff, 0xff]);

/// Panel brightness in percent, 0 (panel off) to 100.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Brightness(u8);

impl Brightness {
    /// Full brightness, what the start-up sequence sets.
    pub const MAX: Self = Self(100);

    /// The percentage.
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for Brightness {
    fn default() -> Self {
        Self::MAX
    }
}

impl TryFrom<u8> for Brightness {
    type Error = BrightnessError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value <= 100 {
            Ok(Self(value))
        } else {
            Err(BrightnessError { value })
        }
    }
}

/// A brightness above 100.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("brightness {value} is out of range 0..=100")]
pub struct BrightnessError {
    value: u8,
}

/// `03 0c NN` + zeros: brightness, confirmed by the TT RGB Plus 2.0 capture.
pub(crate) const fn brightness_command(brightness: Brightness) -> FeatureCommand {
    FeatureCommand::from_prefix(&[0x03, 0x0c, brightness.0])
}

/// Panel rotation step as sent by TT RGB Plus (`03 1d NN`), 0 to 3.
///
/// The panel ignores the command visually: TT RGB Plus rotates the picture itself and the
/// frame stays 480x128 whatever the step. 0 is normal, 2 is turned 180 degrees, 1 and 3 are
/// portrait layouts (128x480) turned 90 degrees clockwise and counter-clockwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Rotation(u8);

impl Rotation {
    /// The step number.
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for Rotation {
    type Error = RotationError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value <= 3 {
            Ok(Self(value))
        } else {
            Err(RotationError { value })
        }
    }
}

/// A rotation step above 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("rotation {value} is out of range 0..=3")]
pub struct RotationError {
    value: u8,
}

/// `03 1d NN` + zeros: rotation step, as sent by TT RGB Plus's rotate buttons.
pub(crate) const fn rotation_command(rotation: Rotation) -> FeatureCommand {
    FeatureCommand::from_prefix(&[0x03, 0x1d, rotation.0])
}

/// The feature reports the panel is ever asked for (GET_FEATURE).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeatureQuery {
    /// 0x07: width/height as u16 LE at bytes 7..11.
    Geometry,
    /// 0x0a: firmware version string from byte 5.
    Firmware,
    /// 0x0f: status, polled as keep-alive.
    Status,
    /// 0x10: read at start-up like TT RGB Plus; meaning unknown.
    Report10,
    /// 0x0d: boot-animation upload progress, `0d NN` with NN the last block stored.
    UploadProgress,
}

impl FeatureQuery {
    /// HID report ID of this query.
    pub const fn report_id(self) -> u8 {
        match self {
            Self::Geometry => 0x07,
            Self::Firmware => 0x0a,
            Self::Status => 0x0f,
            Self::Report10 => 0x10,
            Self::UploadProgress => 0x0d,
        }
    }
}

/// Bytes returned by a GET_FEATURE, report ID first (at most 64).
#[derive(Clone, PartialEq, Eq)]
pub struct FeatureReply {
    bytes: [u8; FEATURE_REPORT_LEN],
    len: usize,
}

impl FeatureReply {
    /// Copies up to 64 bytes of `data`.
    pub fn new(data: &[u8]) -> Self {
        let len = data.len().min(FEATURE_REPORT_LEN);
        let mut bytes = [0u8; FEATURE_REPORT_LEN];
        bytes[..len].copy_from_slice(&data[..len]);
        Self { bytes, len }
    }

    /// The reply bytes, report ID first.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl fmt::Debug for FeatureReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FeatureReply({:02x?})", self.as_bytes())
    }
}

/// One 1024-byte output report (ID 0x02) carrying a JPEG chunk.
#[derive(Clone, PartialEq, Eq)]
pub struct OutputPacket {
    bytes: [u8; OUTPUT_REPORT_LEN],
}

impl OutputPacket {
    /// The full report, report ID first.
    pub fn as_bytes(&self) -> &[u8; OUTPUT_REPORT_LEN] {
        &self.bytes
    }
}

impl fmt::Debug for OutputPacket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OutputPacket(header {:02x?})", &self.bytes[..8])
    }
}

/// Frames that carry a new brightness in byte 2 after `03 0c NN`, as TT RGB Plus does.
pub(crate) const BRIGHTNESS_ECHO_FRAMES: u8 = 8;

/// Byte 2 of every image packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameMarker {
    /// 0x00 on the first frame after the start-up sequence.
    FirstAfterStart,
    /// NN on the frames right after a brightness change.
    Brightness(Brightness),
    /// 0xff on every other frame.
    Continuing,
}

impl FrameMarker {
    const fn byte(self) -> u8 {
        match self {
            Self::FirstAfterStart => 0x00,
            Self::Brightness(brightness) => brightness.get(),
            Self::Continuing => 0xff,
        }
    }
}

/// Splits a JPEG into `02 09 BB last lenLE16 idxLE16 payload` packets, zero padded.
pub(crate) fn packets(jpeg: &[u8], marker: FrameMarker) -> impl Iterator<Item = OutputPacket> + '_ {
    let count = jpeg.len().div_ceil(CHUNK_PAYLOAD_LEN);
    jpeg.chunks(CHUNK_PAYLOAD_LEN)
        .enumerate()
        .map(move |(idx, chunk)| {
            let len = u16::try_from(chunk.len()).expect("chunks are at most 1016 bytes");
            let index = u16::try_from(idx).expect("frames are capped far below u16::MAX packets");
            let mut bytes = [0u8; OUTPUT_REPORT_LEN];
            bytes[0] = OUTPUT_REPORT_ID;
            bytes[1] = IMAGE_COMMAND;
            bytes[2] = marker.byte();
            bytes[3] = u8::from(idx + 1 == count);
            bytes[4..6].copy_from_slice(&len.to_le_bytes());
            bytes[6..8].copy_from_slice(&index.to_le_bytes());
            bytes[8..8 + chunk.len()].copy_from_slice(chunk);
            OutputPacket { bytes }
        })
}

/// Splits a standby payload stream into `02 0d S 00 last idxLE16 lenLE16 <7 zeros> payload`
/// packets for one slot, zero padded.
pub(crate) fn standby_packets(stream: &[u8], slot: u8) -> Vec<OutputPacket> {
    let count = stream.len().div_ceil(STANDBY_CHUNK_LEN);
    stream
        .chunks(STANDBY_CHUNK_LEN)
        .enumerate()
        .map(|(idx, chunk)| {
            let len = u16::try_from(chunk.len()).expect("chunks are at most 1008 bytes");
            let index =
                u16::try_from(idx).expect("standby images are capped far below u16::MAX packets");
            let mut bytes = [0u8; OUTPUT_REPORT_LEN];
            bytes[0] = OUTPUT_REPORT_ID;
            bytes[1] = STANDBY_COMMAND;
            bytes[2] = slot;
            bytes[3] = 0x00;
            bytes[4] = u8::from(idx + 1 == count);
            bytes[5..7].copy_from_slice(&index.to_le_bytes());
            bytes[7..9].copy_from_slice(&len.to_le_bytes());
            bytes[STANDBY_HEADER_LEN..STANDBY_HEADER_LEN + chunk.len()].copy_from_slice(chunk);
            OutputPacket { bytes }
        })
        .collect()
}

/// Splits one boot-animation block into `02 0c B 00 00 00 last idxLE16 lenLE16 F 28 00 a2 8a`
/// packets, zero padded. `final_data_block` sets byte 11 (F) on every packet.
pub(crate) fn boot_packets(block: u8, payload: &[u8], final_data_block: bool) -> Vec<OutputPacket> {
    let count = payload.len().div_ceil(STANDBY_CHUNK_LEN);
    payload
        .chunks(STANDBY_CHUNK_LEN)
        .enumerate()
        .map(|(idx, chunk)| {
            let len = u16::try_from(chunk.len()).expect("chunks are at most 1008 bytes");
            let index = u16::try_from(idx).expect("blocks are at most 65536 bytes");
            let mut bytes = [0u8; OUTPUT_REPORT_LEN];
            bytes[0] = OUTPUT_REPORT_ID;
            bytes[1] = BOOT_COMMAND;
            bytes[2] = block;
            bytes[6] = u8::from(idx + 1 == count);
            bytes[7..9].copy_from_slice(&index.to_le_bytes());
            bytes[9..11].copy_from_slice(&len.to_le_bytes());
            bytes[11] = u8::from(final_data_block);
            bytes[12..16].copy_from_slice(&BOOT_TRAILER);
            bytes[STANDBY_HEADER_LEN..STANDBY_HEADER_LEN + chunk.len()].copy_from_slice(chunk);
            OutputPacket { bytes }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(prefix: &[u8]) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[..prefix.len()].copy_from_slice(prefix);
        out
    }

    fn buffer(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| u8::try_from(i % 251).unwrap() | 1)
            .collect()
    }

    #[test]
    fn startup_commands_match_the_capture() {
        assert_eq!(STARTUP_18.as_bytes(), &literal(&[0x03, 0x18]));
        assert_eq!(STARTUP_1A.as_bytes(), &literal(&[0x03, 0x1a]));
        assert_eq!(
            STARTUP_0C.as_bytes(),
            &literal(&[
                0x03, 0x0c, 0x64, 0x00, 0x00, 0x00, 0x00, 0xe0, 0x01, 0x80, 0x00, 0x04, 0x05
            ])
        );
        assert_eq!(
            STARTUP_1D.as_bytes(),
            &literal(&[0x03, 0x1d, 0x00, 0xff, 0xff, 0xea, 0xff, 0xff, 0xff])
        );
    }

    #[test]
    fn packets_split_2050_bytes_into_three() {
        let jpeg = buffer(2050);
        let out: Vec<OutputPacket> = packets(&jpeg, FrameMarker::FirstAfterStart).collect();
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[0].as_bytes()[..8],
            [0x02, 0x09, 0x00, 0x00, 0xf8, 0x03, 0x00, 0x00]
        );
        assert_eq!(
            out[1].as_bytes()[..8],
            [0x02, 0x09, 0x00, 0x00, 0xf8, 0x03, 0x01, 0x00]
        );
        assert_eq!(
            out[2].as_bytes()[..8],
            [0x02, 0x09, 0x00, 0x01, 0x12, 0x00, 0x02, 0x00]
        );
        assert_eq!(out[0].as_bytes()[8..1024], jpeg[..1016]);
        assert_eq!(out[1].as_bytes()[8..1024], jpeg[1016..2032]);
        assert_eq!(out[2].as_bytes()[8..26], jpeg[2032..]);
        assert!(out[2].as_bytes()[26..].iter().all(|&b| b == 0));
    }

    #[test]
    fn continuing_marker_is_0xff() {
        let out: Vec<OutputPacket> = packets(&buffer(10), FrameMarker::Continuing).collect();
        assert_eq!(out[0].as_bytes()[2], 0xff);
    }

    #[test]
    fn exact_multiple_flags_last_full_packet() {
        let out: Vec<OutputPacket> =
            packets(&buffer(2 * CHUNK_PAYLOAD_LEN), FrameMarker::Continuing).collect();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].as_bytes()[3], 0);
        assert_eq!(out[1].as_bytes()[3], 1);
        assert_eq!(out[1].as_bytes()[4..6], [0xf8, 0x03]);
    }

    #[test]
    fn query_report_ids() {
        assert_eq!(FeatureQuery::Geometry.report_id(), 0x07);
        assert_eq!(FeatureQuery::Firmware.report_id(), 0x0a);
        assert_eq!(FeatureQuery::Status.report_id(), 0x0f);
        assert_eq!(FeatureQuery::Report10.report_id(), 0x10);
    }

    #[test]
    fn brightness_range() {
        assert_eq!(Brightness::try_from(100).unwrap().get(), 100);
        assert_eq!(Brightness::try_from(0).unwrap().get(), 0);
        assert!(Brightness::try_from(101).is_err());
        assert!(Brightness::try_from(255).is_err());
        assert_eq!(Brightness::default(), Brightness::MAX);
    }

    #[test]
    fn brightness_command_is_03_0c_nn_and_zeros() {
        let command = brightness_command(Brightness::try_from(40).unwrap());
        assert_eq!(command.as_bytes(), &literal(&[0x03, 0x0c, 0x28]));
    }

    #[test]
    fn rotation_range() {
        for step in 0..=3 {
            assert_eq!(Rotation::try_from(step).unwrap().get(), step);
        }
        assert!(Rotation::try_from(4).is_err());
        assert!(Rotation::try_from(255).is_err());
        assert_eq!(Rotation::default().get(), 0);
    }

    #[test]
    fn rotation_command_is_03_1d_nn_and_zeros() {
        for step in 0..=3 {
            let command = rotation_command(Rotation::try_from(step).unwrap());
            assert_eq!(command.as_bytes(), &literal(&[0x03, 0x1d, step]));
        }
    }

    #[test]
    fn brightness_marker_byte() {
        let marker = FrameMarker::Brightness(Brightness::try_from(40).unwrap());
        assert_eq!(marker.byte(), 0x28);
    }

    #[test]
    fn reply_truncates_to_64_bytes() {
        let reply = FeatureReply::new(&[7u8; 100]);
        assert_eq!(reply.as_bytes().len(), 64);
        assert_eq!(FeatureReply::new(&[1, 2]).as_bytes(), &[1, 2]);
    }
}
