//! The capture seam: grab one full source frame, blocking.

use image::RgbImage;

/// Why a capture failed.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The compositor or output vanished; the caller should reconnect.
    #[error("capture source is gone: {0}")]
    Gone(String),
    /// The user or the compositor ended the source for good; reconnecting would only ask the
    /// user again, so the stream stops with this message.
    #[error("{0}")]
    Closed(String),
    /// This capture failed; trying again may work.
    #[error("capture failed: {0}")]
    Failed(String),
}

/// A blocking source of full frames (a screen, a virtual desktop).
pub trait Capturer: Send {
    /// Grabs one frame, blocking until the source has it.
    ///
    /// # Errors
    ///
    /// [`CaptureError::Gone`] when the source disappeared, [`CaptureError::Failed`] otherwise.
    fn capture(&mut self) -> Result<RgbImage, CaptureError>;

    /// Something the user should fix while the source keeps running, if anything.
    fn problem(&self) -> Option<String> {
        None
    }
}
