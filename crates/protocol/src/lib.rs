//! Protocol for the Thermaltake Tower 500 LCD panel kit ("RC Pro", USB 264a:232a).
//!
//! Everything this crate can send was observed in a USB capture of TT RGB Plus 3.0.
//! There is deliberately no API for sending arbitrary reports: unknown commands may
//! change persistent panel settings or firmware.

mod boot;
mod frame;
mod hid;
#[cfg(any(test, feature = "test-util"))]
pub mod mock;
mod packet;
mod session;
mod standby;
mod transport;

use std::time::Duration;

pub use boot::{
    BOOT_BLOCK_BYTES, BootAnimation, BootError, MAX_BOOT_BLOCKS, MAX_BOOT_DATA_BYTES,
    MAX_BOOT_FRAME_BYTES, MAX_BOOT_FRAME_DELAY_MS, MAX_BOOT_FRAMES, MIN_BOOT_FRAME_DELAY_MS,
};
pub use frame::{Frame, FrameError, MAX_FRAME_BYTES};
pub use hid::{Discovery, HidTransport, OpenError};
pub use packet::{
    Brightness, BrightnessError, FeatureCommand, FeatureQuery, FeatureReply, OutputPacket,
};
pub use session::{Panel, PanelInfo, SessionError, StatusReply};
pub use standby::{MAX_STANDBY_JPEG_BYTES, StandbyImage};
pub use transport::{Transport, TransportError};

/// USB vendor ID (Thermaltake).
pub const VENDOR_ID: u16 = 0x264a;
/// USB product ID of the Tower 500 LCD kit.
pub const PRODUCT_ID: u16 = 0x232a;
/// Panel width in pixels (GET 0x07 must report it).
pub const WIDTH: u16 = 480;
/// Panel height in pixels (GET 0x07 must report it).
pub const HEIGHT: u16 = 128;
/// How often a still image is re-sent; TT RGB Plus used 1.04 s.
pub const STILL_REFRESH: Duration = Duration::from_millis(1040);
/// Shortest frame interval used: 30 fps. The capture shows 50-150 ms between GIF frames;
/// rates above 20 fps were chosen by the user and need a hardware check.
pub const MIN_FRAME_INTERVAL: Duration = Duration::from_nanos(33_333_333);
/// GET 0x0f poll interval while streaming, as in the capture.
pub const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(2);
