//! Newline-delimited JSON framing.

use std::io::{self, BufRead, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Longest accepted line, excluding the newline.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// A framing failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FramingError {
    /// Reading or writing the stream failed.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    /// A line exceeded [`MAX_LINE_BYTES`].
    #[error("message longer than {MAX_LINE_BYTES} bytes")]
    TooLong,
    /// The line was not valid JSON for the expected type.
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Writes `msg` as one JSON line and flushes.
///
/// # Errors
///
/// Returns an error if serialization or the write fails.
pub fn write_message<T: Serialize>(w: &mut impl Write, msg: &T) -> Result<(), FramingError> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()?;
    Ok(())
}

/// Reads one JSON line into `T`; `Ok(None)` at end of stream. `buf` is reused as scratch.
///
/// # Errors
///
/// Returns an error on I/O failure, a line over [`MAX_LINE_BYTES`], or invalid JSON.
pub fn read_message<T: DeserializeOwned>(
    r: &mut impl BufRead,
    buf: &mut String,
) -> Result<Option<T>, FramingError> {
    buf.clear();
    let limit = u64::try_from(MAX_LINE_BYTES).map_or(u64::MAX, |n| n.saturating_add(1));
    let read = r.by_ref().take(limit).read_line(buf)?;
    if read == 0 {
        return Ok(None);
    }
    let line = buf.strip_suffix('\n').unwrap_or(buf.as_str());
    if line.len() > MAX_LINE_BYTES {
        return Err(FramingError::TooLong);
    }
    Ok(Some(serde_json::from_str(line)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trip() {
        let mut out = Vec::new();
        write_message(&mut out, &serde_json::json!({"a": 1})).unwrap();
        assert_eq!(out, b"{\"a\":1}\n");
        let mut buf = String::new();
        let got: Option<serde_json::Value> = read_message(&mut Cursor::new(out), &mut buf).unwrap();
        assert_eq!(got, Some(serde_json::json!({"a": 1})));
    }

    #[test]
    fn eof_is_none() {
        let mut buf = String::new();
        let got: Option<serde_json::Value> =
            read_message(&mut Cursor::new(Vec::new()), &mut buf).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn oversize_line_is_rejected() {
        let mut data = vec![b'x'; MAX_LINE_BYTES + 10];
        data.push(b'\n');
        let mut buf = String::new();
        let got = read_message::<serde_json::Value>(&mut Cursor::new(data), &mut buf);
        assert!(matches!(got, Err(FramingError::TooLong)));
    }

    #[test]
    fn two_messages_in_sequence() {
        let mut buf = String::new();
        let mut r = Cursor::new(b"1\n2\n".to_vec());
        assert_eq!(read_message::<u32>(&mut r, &mut buf).unwrap(), Some(1));
        assert_eq!(read_message::<u32>(&mut r, &mut buf).unwrap(), Some(2));
        assert_eq!(read_message::<u32>(&mut r, &mut buf).unwrap(), None);
    }
}
