//! hidapi-backed transport (hidraw on Linux).

use std::fmt;

use hidapi::{HidApi, HidDevice};

use crate::packet::{FEATURE_REPORT_LEN, OUTPUT_REPORT_LEN};
use crate::{
    FeatureCommand, FeatureQuery, FeatureReply, OutputPacket, PRODUCT_ID, Transport,
    TransportError, VENDOR_ID,
};

/// Finds and opens the panel.
///
/// Only one `Discovery` may exist per process (hidapi-rs allows one `HidApi` at a time).
pub struct Discovery {
    api: HidApi,
}

impl Discovery {
    /// Initialises hidapi.
    ///
    /// # Errors
    /// Returns [`OpenError::Hid`] if hidapi cannot initialise.
    pub fn new() -> Result<Self, OpenError> {
        let api = HidApi::new().map_err(|e| OpenError::Hid(e.to_string()))?;
        Ok(Self { api })
    }

    /// Rescans and opens the first device with the panel's vendor and product ID.
    ///
    /// # Errors
    /// [`OpenError::NotFound`] if no panel is present, [`OpenError::PermissionDenied`] if
    /// the node cannot be opened, [`OpenError::Hid`] for anything else.
    pub fn open(&mut self) -> Result<HidTransport, OpenError> {
        self.api
            .refresh_devices()
            .map_err(|e| OpenError::Hid(e.to_string()))?;
        let path = self
            .api
            .device_list()
            .find(|info| info.vendor_id() == VENDOR_ID && info.product_id() == PRODUCT_ID)
            .map(|info| info.path().to_owned())
            .ok_or(OpenError::NotFound)?;
        match self.api.open_path(&path) {
            Ok(device) => Ok(HidTransport { device }),
            Err(e) => {
                let message = e.to_string();
                if message.contains("Permission denied") {
                    Err(OpenError::PermissionDenied {
                        path: path.to_string_lossy().into_owned(),
                    })
                } else {
                    Err(OpenError::Hid(message))
                }
            }
        }
    }
}

/// An open panel; dropping it closes the hidraw node.
pub struct HidTransport {
    device: HidDevice,
}

impl fmt::Debug for Discovery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Discovery").finish_non_exhaustive()
    }
}

impl fmt::Debug for HidTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HidTransport").finish_non_exhaustive()
    }
}

impl Transport for HidTransport {
    fn set_feature(&mut self, command: &FeatureCommand) -> Result<(), TransportError> {
        self.device
            .send_feature_report(command.as_bytes())
            .map_err(|e| io_error("send_feature_report", &e))
    }

    fn get_feature(&mut self, query: FeatureQuery) -> Result<FeatureReply, TransportError> {
        let mut buf = [0u8; FEATURE_REPORT_LEN];
        buf[0] = query.report_id();
        let n = self
            .device
            .get_feature_report(&mut buf)
            .map_err(|e| io_error("get_feature_report", &e))?;
        Ok(FeatureReply::new(&buf[..n.min(FEATURE_REPORT_LEN)]))
    }

    fn write_output(&mut self, packet: &OutputPacket) -> Result<(), TransportError> {
        let written = self
            .device
            .write(packet.as_bytes())
            .map_err(|e| io_error("write", &e))?;
        if written == OUTPUT_REPORT_LEN {
            Ok(())
        } else {
            Err(TransportError::ShortWrite {
                written,
                expected: OUTPUT_REPORT_LEN,
            })
        }
    }
}

fn io_error(op: &'static str, error: &hidapi::HidError) -> TransportError {
    TransportError::Io {
        op,
        message: error.to_string(),
    }
}

/// Why the panel could not be opened.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// No device with the panel's vendor and product ID is present.
    #[error("panel not found (USB 264a:232a)")]
    NotFound,
    /// The hidraw node exists but this user may not open it.
    #[error(
        "permission denied opening {path}; install the udev rule that grants access to 264a:232a"
    )]
    PermissionDenied {
        /// The hidraw node path.
        path: String,
    },
    /// Any other hidapi failure.
    #[error("hidapi error: {0}")]
    Hid(String),
}
