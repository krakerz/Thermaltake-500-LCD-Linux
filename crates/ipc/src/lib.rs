//! IPC between the `tt500-lcd` daemon and its clients (CLI, GUI).
//!
//! Newline-delimited JSON over a Unix stream socket. Every successful reply is a
//! full [`Status`]. This crate does not depend on the protocol crate: values such as
//! brightness travel as plain integers and the daemon validates them.

mod client;
mod framing;
mod message;
mod path;
mod procs;
mod profile;

pub use client::{Client, ClientError, Subscription};
pub use framing::{FramingError, MAX_LINE_BYTES, read_message, write_message};
pub use message::{
    BrightnessParams, ContentInfo, ContentKind, Crop, DESKTOP_DEFAULT_SCALE, DESKTOP_DEFAULT_SIZE,
    DESKTOP_MAX_COMMANDS, DESKTOP_MAX_HEIGHT, DESKTOP_MAX_SCALE, DESKTOP_MAX_WIDTH,
    DESKTOP_MIN_HEIGHT, DESKTOP_MIN_WIDTH, DesktopSpec, DeviceState, DryRunReply, ErrorCode, Event,
    EventEnvelope, Fit, IpcError, LiveSource, Outcome, ParseCropError, ParseFitError, PreviewReply,
    Request, RequestEnvelope, Response, RotationParams, ServerMessage, ShowLiveParams, ShowParams,
    Size, Status, UploadBootAnimationParams, UploadStandbyParams,
};
pub use path::{SOCKET_ENV, SocketPathError, socket_path};
pub use procs::{process_names_in, process_running, running_process_names};
pub use profile::{
    ActiveProfile, Builtin, DEFAULT_PROFILE_NAME, PROFILE_MAX_NAME_CHARS,
    PROFILE_MAX_PROCESS_CHARS, PROFILE_MAX_PROCESSES, PROFILE_MAX_PROFILES, PROFILE_MAX_RANGES,
    PROFILE_MAX_SCHEDULE, ParseProfileError, Profile, ProfileNameParams, ProfileReason,
    ProfileRenameParams, ProfileReorderParams, ProfileSaveParams, ProfileSource, ProfileSpec,
    ProfileStatus, ProfileTriggersParams, ProfilesReply, RangeEntry, ScheduleEntry, TimeOfDay,
    Triggers, Weekday, validate_name,
};

/// Bumped on any incompatible wire change.
pub const IPC_VERSION: u32 = 1;

/// Brightness in percent after a reset (the daemon's default).
pub const DEFAULT_BRIGHTNESS: u8 = 100;

/// Frame-rate cap used when `show` gives none.
pub const DEFAULT_FPS: u32 = 15;

/// Highest frame-rate cap `show` accepts.
pub const MAX_FPS: u32 = 30;
