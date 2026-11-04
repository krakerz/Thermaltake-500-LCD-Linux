//! Standby-image dry run: the exact packets a real upload would send, written to disk.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tt500_lcd_protocol::{Rotation, StandbyImage};

/// File name of the raw packet dump.
const DUMP_FILE: &str = "standby-dryrun.bin";
/// File name of the hex summary.
const SUMMARY_FILE: &str = "standby-dryrun.txt";
/// Bytes of each packet shown in the summary.
const SUMMARY_BYTES: usize = 32;

/// Written dry-run files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunFiles {
    pub packets: PathBuf,
    pub summary: PathBuf,
}

/// Writes the packets of `image` (raw, back to back) and a text summary into `dir`.
/// Nothing is sent to the panel.
pub fn write_dry_run(
    dir: &Path,
    image: &StandbyImage,
    rotation: Rotation,
) -> io::Result<DryRunFiles> {
    fs::create_dir_all(dir)?;
    let packets = image.packets();
    let mut raw = Vec::with_capacity(packets.len() * 1024);
    let mut summary = String::new();
    let per_slot = packets.len() / 2;
    let _ = writeln!(summary, "standby dry run: nothing was sent to the panel");
    let _ = writeln!(summary, "rotation: step {}", rotation.get());
    let _ = writeln!(summary, "jpeg bytes: {}", image.byte_len());
    let _ = writeln!(
        summary,
        "packets per slot: {per_slot} (slot 1 first, then slot 0)"
    );
    let _ = writeln!(summary, "packets total: {}", packets.len());
    for (n, packet) in packets.iter().enumerate() {
        raw.extend_from_slice(packet.as_bytes());
        let _ = write!(summary, "{n:3}: ");
        for (i, byte) in packet.as_bytes()[..SUMMARY_BYTES].iter().enumerate() {
            if i > 0 {
                summary.push(' ');
            }
            let _ = write!(summary, "{byte:02x}");
        }
        summary.push('\n');
    }
    let files = DryRunFiles {
        packets: dir.join(DUMP_FILE),
        summary: dir.join(SUMMARY_FILE),
    };
    fs::write(&files.packets, raw)?;
    fs::write(&files.summary, summary)?;
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_is_the_exact_packets() {
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xfe, 0x0b, 0xb8];
        jpeg.extend(std::iter::repeat_n(0x55, 3000 - 2));
        // Minimal 4:2:0 SOF0 for 480x128, then EOI.
        jpeg.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x80, 0x01, 0xe0]);
        jpeg.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1, 0xff, 0xd9]);
        let image = StandbyImage::from_jpeg(jpeg).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let files = write_dry_run(dir.path(), &image, Rotation::default()).unwrap();
        let raw = fs::read(&files.packets).unwrap();
        let want: Vec<u8> = image
            .packets()
            .iter()
            .flat_map(|p| p.as_bytes().to_vec())
            .collect();
        assert_eq!(raw, want);
        assert_eq!(raw.len() % 1024, 0);
        let text = fs::read_to_string(&files.summary).unwrap();
        assert!(text.contains("packets per slot: 4"), "{text}");
        assert!(text.contains("packets total: 8"), "{text}");
        assert!(text.contains(&format!("jpeg bytes: {}", image.byte_len())));
        assert!(
            text.contains("  0: 02 0d 01 00 00 00 00 f0 03 00"),
            "{text}"
        );
    }
}
