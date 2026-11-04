//! Telling images from videos by their first bytes.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use image::ImageFormat;

use crate::RenderError;

/// Bytes read to recognise a file.
const SNIFF_BYTES: u64 = 64;

/// What kind of file a source is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    /// A still image or an animation the `image` crate decodes (GIF, APNG, WebP, ...).
    Image,
    /// A video container decoded by ffmpeg (MP4/MOV, MKV/WebM, AVI).
    Video,
}

/// Recognises `path` from its first bytes; the extension is ignored.
///
/// # Errors
/// [`RenderError::Io`] if the file cannot be read, [`RenderError::Unsupported`] if it is
/// neither a supported image nor a known video container.
pub fn detect(path: &Path) -> Result<MediaKind, RenderError> {
    let mut header = Vec::with_capacity(SNIFF_BYTES as usize);
    File::open(path)?
        .take(SNIFF_BYTES)
        .read_to_end(&mut header)?;
    sniff(&header).ok_or(RenderError::Unsupported)
}

fn sniff(header: &[u8]) -> Option<MediaKind> {
    if matches!(
        image::guess_format(header),
        Ok(ImageFormat::Gif
            | ImageFormat::Png
            | ImageFormat::Jpeg
            | ImageFormat::WebP
            | ImageFormat::Bmp)
    ) {
        return Some(MediaKind::Image);
    }
    let at = |start: usize| header.get(start..start + 4);
    let iso_bmff = match at(4) {
        // Still-image HEIF/AVIF share the container; the image crate here cannot decode them.
        Some(b"ftyp") => !matches!(
            at(8),
            Some(b"avif" | b"avis" | b"heic" | b"heix" | b"mif1" | b"msf1")
        ),
        Some(b"moov" | b"mdat" | b"wide" | b"free" | b"skip") => true,
        _ => false,
    };
    let matroska = header.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]);
    let avi = header.starts_with(b"RIFF") && at(8) == Some(b"AVI ");
    (iso_bmff || matroska || avi).then_some(MediaKind::Video)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_containers() {
        assert_eq!(
            sniff(b"\0\0\0\x18ftypisom\0\0\x02\0"),
            Some(MediaKind::Video)
        );
        assert_eq!(sniff(b"\0\0\0\x14ftypqt  \0\0\0\0"), Some(MediaKind::Video));
        assert_eq!(sniff(b"\0\0\0\x08wide\0\0\0\0mdat"), Some(MediaKind::Video));
        assert_eq!(
            sniff(&[0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42]),
            Some(MediaKind::Video)
        );
        assert_eq!(sniff(b"RIFF\x10\0\0\0AVI LIST"), Some(MediaKind::Video));
        assert_eq!(sniff(b"\0\0\0\x1cftypavif\0\0\0\0"), None);
        assert_eq!(sniff(b"definitely not media"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn images_win() {
        assert_eq!(sniff(b"GIF89a\x01\0\x01\0"), Some(MediaKind::Image));
        assert_eq!(
            sniff(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"),
            Some(MediaKind::Image)
        );
        assert_eq!(sniff(b"RIFF\x10\0\0\0WEBPVP8 "), Some(MediaKind::Image));
    }
}
