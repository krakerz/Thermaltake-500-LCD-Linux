//! Boot-animation dry run: the exact packets a real upload would send, written to disk.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use tt500_lcd_protocol::OutputPacket;
use tt500_lcd_render::RenderedBoot;

use crate::standby::DryRunFiles;

/// File name of the raw packet dump.
const DUMP_FILE: &str = "boot-dryrun.bin";
/// File name of the hex summary.
const SUMMARY_FILE: &str = "boot-dryrun.txt";
/// Bytes of each packet shown in the summary.
const SUMMARY_BYTES: usize = 32;

fn hex(packet: &OutputPacket) -> String {
    let mut out = String::new();
    for (i, byte) in packet.as_bytes()[..SUMMARY_BYTES].iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Writes the packets of `rendered` (raw, in send order) and a text summary into `dir`.
/// Nothing is sent to the panel.
pub fn write_boot_dry_run(dir: &Path, rendered: &RenderedBoot) -> io::Result<DryRunFiles> {
    fs::create_dir_all(dir)?;
    let animation = &rendered.animation;
    let blocks = animation.blocks();
    let mut raw = Vec::new();
    let mut summary = String::new();
    let _ = writeln!(
        summary,
        "boot animation dry run: nothing was sent to the panel"
    );
    let _ = writeln!(summary, "frames: {}", animation.frame_count());
    let delay_ms = animation.frame_delay_ms();
    let _ = writeln!(
        summary,
        "frame delay: {delay_ms} ms ({:.1} fps)",
        1000.0 / f64::from(delay_ms)
    );
    let sizes: Vec<String> = animation.frame_sizes().iter().map(u32::to_string).collect();
    let _ = writeln!(summary, "frame sizes (bytes): {}", sizes.join(" "));
    let _ = writeln!(summary, "jpeg bytes total: {}", animation.data_len());
    let _ = writeln!(
        summary,
        "data blocks: {} (block 0 is the table)",
        animation.block_count()
    );
    if rendered.truncated() {
        let _ = writeln!(
            summary,
            "truncated: yes, kept {} of {} frames at {delay_ms} ms (source had {})",
            animation.frame_count(),
            rendered.resampled_frames,
            rendered.source_frames
        );
    } else {
        let _ = writeln!(
            summary,
            "truncated: no ({} source frames, {} at {delay_ms} ms)",
            rendered.source_frames, rendered.resampled_frames
        );
    }
    let _ = writeln!(
        summary,
        "packets total: {}",
        blocks.iter().map(Vec::len).sum::<usize>()
    );
    for (n, block) in blocks.iter().enumerate() {
        for packet in block {
            raw.extend_from_slice(packet.as_bytes());
        }
        let _ = writeln!(summary, "block {n}: {} packet(s)", block.len());
        if let (Some(first), Some(last)) = (block.first(), block.last()) {
            let _ = writeln!(summary, "  first: {}", hex(first));
            if block.len() > 1 {
                let _ = writeln!(summary, "  last:  {}", hex(last));
            }
        }
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
    use image::{Delay, Frame, Rgba, RgbaImage};
    use tt500_lcd_render::{RenderOptions, render_boot_animation_bytes};

    use super::*;

    fn rendered() -> RenderedBoot {
        let mut gif = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut gif);
            for i in 0..6u8 {
                let buf = RgbaImage::from_pixel(64, 32, Rgba([i * 40, 50, 9, 255]));
                let delay = Delay::from_numer_denom_ms(100, 1);
                enc.encode_frame(Frame::from_parts(buf, 0, 0, delay))
                    .unwrap();
            }
        }
        render_boot_animation_bytes(&gif, &RenderOptions::default()).unwrap()
    }

    #[test]
    fn dump_is_the_exact_packets() {
        let boot = rendered();
        let dir = tempfile::tempdir().unwrap();
        let files = write_boot_dry_run(dir.path(), &boot).unwrap();
        let raw = fs::read(&files.packets).unwrap();
        let want: Vec<u8> = boot
            .animation
            .blocks()
            .iter()
            .flatten()
            .flat_map(|p| p.as_bytes().to_vec())
            .collect();
        assert_eq!(raw, want);
        assert_eq!(raw.len() % 1024, 0);
        let text = fs::read_to_string(&files.summary).unwrap();
        assert!(text.contains("frames: 6"), "{text}");
        assert!(
            text.contains("data blocks: 1 (block 0 is the table)"),
            "{text}"
        );
        assert!(text.contains("frame delay: 100 ms (10.0 fps)"), "{text}");
        assert!(text.contains("truncated: no"), "{text}");
        assert!(
            text.contains("first: 02 0c 00 00 00 00 01 00 00 1c 00 00 28 00 a2 8a"),
            "{text}"
        );
        assert!(text.contains("block 1:"), "{text}");
    }
}
