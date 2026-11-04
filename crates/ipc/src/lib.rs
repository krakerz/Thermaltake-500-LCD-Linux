//! IPC between the `tt500-lcd` daemon and its clients (CLI, GUI).
//!
//! Newline-delimited JSON over a Unix stream socket. Every successful reply is a
//! full [`Status`]. This crate does not depend on the protocol crate: values such as
//! brightness travel as plain integers and the daemon validates them.

mod client;
mod framing;
mod message;
mod path;

pub use client::{Client, ClientError, Subscription};
pub use framing::{FramingError, MAX_LINE_BYTES, read_message, write_message};
pub use message::{
    BrightnessParams, ContentInfo, ContentKind, Crop, DESKTOP_DEFAULT_SCALE, DESKTOP_DEFAULT_SIZE,
    DESKTOP_MAX_COMMANDS, DesktopSpec, DeviceState, DryRunReply, ErrorCode, Event, EventEnvelope,
    Fit, IpcError, LiveSource, Outcome, ParseCropError, ParseFitError, PreviewReply, Request,
    RequestEnvelope, Response, ServerMessage, ShowLiveParams, ShowParams, Size, Status,
    UploadBootAnimationParams, UploadStandbyParams,
};
pub use path::{SOCKET_ENV, SocketPathError, socket_path};

/// Bumped on any incompatible wire change.
pub const IPC_VERSION: u32 = 1;

/// Brightness in percent after a reset (the daemon's default).
pub const DEFAULT_BRIGHTNESS: u8 = 100;

/// Frame-rate cap used when `show` gives none.
pub const DEFAULT_FPS: u32 = 15;

/// Highest frame-rate cap `show` accepts.
pub const MAX_FPS: u32 = 30;
