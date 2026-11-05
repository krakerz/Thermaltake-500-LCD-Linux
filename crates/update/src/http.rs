use std::io::{Read, Write};
use std::time::Duration;

use crate::error::UpdateError;

/// Seconds to wait for a connection or the first response bytes.
const TIMEOUT_SECS: u64 = 20;
/// Bytes read from the network per step, which is also the progress granularity.
const CHUNK: usize = 64 * 1024;

/// Fetches URLs; faked in tests so nothing touches the network.
pub trait Http {
    /// Writes the body of `url` to `sink`, calling `progress(received, total)` as it arrives.
    ///
    /// # Errors
    ///
    /// Fails on a network error, an HTTP error status or a failing `sink`.
    fn get(
        &self,
        url: &str,
        sink: &mut dyn Write,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), UpdateError>;
}

/// The real client: `ureq` with rustls.
#[derive(Debug, Clone)]
pub struct UreqHttp {
    agent: ureq::Agent,
}

impl Default for UreqHttp {
    fn default() -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(TIMEOUT_SECS)))
            .timeout_recv_response(Some(Duration::from_secs(TIMEOUT_SECS)))
            .build()
            .into();
        Self { agent }
    }
}

impl Http for UreqHttp {
    fn get(
        &self,
        url: &str,
        sink: &mut dyn Write,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), UpdateError> {
        let user_agent = concat!("tt500-lcd/", env!("CARGO_PKG_VERSION"));
        let response = self
            .agent
            .get(url)
            .header("User-Agent", user_agent)
            .header(
                "Accept",
                "application/vnd.github+json, application/octet-stream",
            )
            .call()
            .map_err(|e| UpdateError::Http(e.to_string()))?;
        let total = response.body().content_length();
        let mut reader = response.into_body().into_reader();
        let mut buffer = vec![0u8; CHUNK];
        let mut received = 0u64;
        loop {
            let n = reader
                .read(&mut buffer)
                .map_err(|e| UpdateError::Http(e.to_string()))?;
            if n == 0 {
                return Ok(());
            }
            sink.write_all(&buffer[..n])
                .map_err(|e| UpdateError::Http(format!("writing the download: {e}")))?;
            received += n as u64;
            progress(received, total);
        }
    }
}

/// Fetches `url` into memory (the release JSON, the checksum list).
pub(crate) fn get_bytes(http: &dyn Http, url: &str) -> Result<Vec<u8>, UpdateError> {
    let mut body = Vec::new();
    http.get(url, &mut body, &mut |_, _| {})?;
    Ok(body)
}
