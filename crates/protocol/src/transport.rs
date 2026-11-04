//! The device abstraction: three operations, all taking fixed protocol types.

use crate::packet::{FeatureCommand, FeatureQuery, FeatureReply, OutputPacket};

/// Moves the fixed protocol packets to a device; implementations never invent bytes.
pub trait Transport {
    /// Sends one start-up SET_FEATURE report.
    ///
    /// # Errors
    /// Returns an error if the device rejects or cannot take the report.
    fn set_feature(&mut self, command: &FeatureCommand) -> Result<(), TransportError>;

    /// Reads one feature report.
    ///
    /// # Errors
    /// Returns an error if the device cannot be read.
    fn get_feature(&mut self, query: FeatureQuery) -> Result<FeatureReply, TransportError>;

    /// Writes one image output report.
    ///
    /// # Errors
    /// Returns an error on a failed or short write.
    fn write_output(&mut self, packet: &OutputPacket) -> Result<(), TransportError>;
}

impl<T: Transport + ?Sized> Transport for Box<T> {
    fn set_feature(&mut self, command: &FeatureCommand) -> Result<(), TransportError> {
        (**self).set_feature(command)
    }

    fn get_feature(&mut self, query: FeatureQuery) -> Result<FeatureReply, TransportError> {
        (**self).get_feature(query)
    }

    fn write_output(&mut self, packet: &OutputPacket) -> Result<(), TransportError> {
        (**self).write_output(packet)
    }
}

/// A transport-level failure; the session should be dropped and the device rescanned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TransportError {
    /// The OS or driver reported an error.
    #[error("{op} failed: {message}")]
    Io {
        /// The operation that failed.
        op: &'static str,
        /// The underlying error text.
        message: String,
    },
    /// The device accepted fewer bytes than the report holds.
    #[error("short write: {written} of {expected} bytes")]
    ShortWrite {
        /// Bytes written.
        written: usize,
        /// Bytes expected.
        expected: usize,
    },
}
