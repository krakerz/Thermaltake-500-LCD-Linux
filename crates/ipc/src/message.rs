//! Wire types: requests, responses, events and the shared [`Status`] model.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::profile::{
    ProfileNameParams, ProfileRenameParams, ProfileReorderParams, ProfileSaveParams, ProfileStatus,
    ProfileTriggersParams, ProfilesReply,
};

/// A client request with its correlation id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    /// Echoed in the matching [`Response`].
    pub id: u64,
    /// The call itself.
    #[serde(flatten)]
    pub request: Request,
}

/// A call to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Request {
    /// Report the current [`Status`].
    Status,
    /// Show an image, animation or video on the panel.
    Show(ShowParams),
    /// Stop streaming and release the panel.
    Clear,
    /// Set the panel brightness.
    SetBrightness(BrightnessParams),
    /// Set the panel rotation step (as sent by TT RGB Plus).
    SetRotation(RotationParams),
    /// Reply with the current [`Status`], then push an [`Event::Status`] on every change.
    ///
    /// The connection is event-only afterwards: further input is ignored, and closing the
    /// connection ends the subscription.
    Subscribe,
    /// Render a still and upload it as the panel's standby image (**writes to the panel's
    /// flash**), or with `dry_run` only write the packets to disk.
    UploadStandby(UploadStandbyParams),
    /// Render an animation and upload it as the panel's boot animation (**writes to the
    /// panel's flash**), or with `dry_run` only write the packets to disk.
    UploadBootAnimation(UploadBootAnimationParams),
    /// Stream a live source (virtual desktop or screen) to the panel.
    ShowLive(ShowLiveParams),
    /// Write the latest full live-source frame to a file (crop-editor preview).
    Preview,
    /// List the profiles.
    ProfileList,
    /// Save a profile: from the given spec, or a snapshot of the current content.
    ProfileSave(ProfileSaveParams),
    /// Rename a profile.
    ProfileRename(ProfileRenameParams),
    /// Delete a profile.
    ProfileDelete(ProfileNameParams),
    /// Switch to a profile.
    ProfileActivate(ProfileNameParams),
    /// Replace a profile's triggers.
    ProfileSetTriggers(ProfileTriggersParams),
    /// Set the profile order; `names` must list every profile exactly once.
    ProfileReorder(ProfileReorderParams),
    /// Restore the built-in Default profile to its factory state.
    ProfileReset(ProfileNameParams),
}

/// Parameters of [`Request::UploadBootAnimation`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadBootAnimationParams {
    /// Absolute path of the source animation (GIF, APNG or animated WebP).
    pub path: PathBuf,
    /// How the frames are fitted to the panel.
    #[serde(default)]
    pub fit: Fit,
    /// Region of the source to use, applied before `fit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
    /// `true`: send nothing to the panel; write the exact packets to files and reply with
    /// [`Outcome::DryRun`]. Required, so that a request can never write flash by omission.
    pub dry_run: bool,
}

/// Parameters of [`Request::UploadStandby`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadStandbyParams {
    /// Absolute path of the source still image.
    pub path: PathBuf,
    /// How the image is fitted to the panel.
    #[serde(default)]
    pub fit: Fit,
    /// Region of the source to use, applied before `fit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
    /// `true`: send nothing to the panel; write the exact packets to files and reply with
    /// [`Outcome::DryRun`]. Required, so that a request can never write flash by omission.
    pub dry_run: bool,
}

/// Parameters of [`Request::Show`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowParams {
    /// Absolute path of the source file.
    pub path: PathBuf,
    /// How the image is fitted to the panel.
    #[serde(default)]
    pub fit: Fit,
    /// Region of the source to show, applied before `fit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
    /// Frame-rate cap for videos and animations, 1..=[`MAX_FPS`](crate::MAX_FPS);
    /// [`DEFAULT_FPS`](crate::DEFAULT_FPS) when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
}

/// Parameters of [`Request::ShowLive`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowLiveParams {
    /// What to capture.
    pub source: LiveSource,
    /// How the captured frames are fitted to the panel.
    #[serde(default)]
    pub fit: Fit,
    /// Region of the source frame to show, applied before `fit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
    /// Capture frame rate, 1..=[`MAX_FPS`](crate::MAX_FPS);
    /// [`DEFAULT_FPS`](crate::DEFAULT_FPS) when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// Hold back half-drawn frames: a changed frame is shown only once the next capture
    /// matches it (or after a short wait). Defaults to on for a virtual desktop, off for
    /// a screen or portal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steady: Option<bool>,
}

/// A live source streamed by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LiveSource {
    /// A headless desktop running the given commands.
    Desktop(DesktopSpec),
    /// A monitor of the running Wayland session.
    Screen {
        /// Output (connector) name; the first output when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    /// A monitor or window chosen through the desktop portal (not implemented yet).
    Portal {
        /// Ask again instead of reusing the remembered choice.
        #[serde(default)]
        reselect: bool,
    },
}

/// Default virtual desktop output size (logical 480x128 at scale 2).
pub const DESKTOP_DEFAULT_SIZE: Size = Size {
    width: 960,
    height: 256,
};

/// Default virtual desktop output scale.
pub const DESKTOP_DEFAULT_SCALE: u8 = 2;

/// Most commands a virtual desktop runs.
pub const DESKTOP_MAX_COMMANDS: usize = 8;

/// Narrowest accepted desktop, in physical pixels.
pub const DESKTOP_MIN_WIDTH: u32 = 240;
/// Widest accepted desktop, in physical pixels.
pub const DESKTOP_MAX_WIDTH: u32 = 3840;
/// Shortest accepted desktop, in physical pixels.
pub const DESKTOP_MIN_HEIGHT: u32 = 64;
/// Tallest accepted desktop, in physical pixels.
pub const DESKTOP_MAX_HEIGHT: u32 = 2160;
/// Smallest accepted logical (size / scale) width.
pub const DESKTOP_MIN_LOGICAL_WIDTH: u32 = 120;
/// Smallest accepted logical (size / scale) height.
pub const DESKTOP_MIN_LOGICAL_HEIGHT: u32 = 32;
/// Largest accepted output scale.
pub const DESKTOP_MAX_SCALE: u8 = 3;

/// Longest accepted command, in bytes.
const DESKTOP_MAX_COMMAND_BYTES: usize = 1024;

fn default_scale() -> u8 {
    DESKTOP_DEFAULT_SCALE
}

/// A virtual desktop: output size, scale and the commands to run on it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DesktopSpec {
    /// Shell commands, one client each (`sh -c`).
    pub commands: Vec<String>,
    /// Output size in physical pixels.
    pub size: Size,
    /// Output scale factor.
    #[serde(default = "default_scale")]
    pub scale: u8,
}

impl DesktopSpec {
    /// Checks the command list, size and scale.
    ///
    /// # Errors
    ///
    /// Returns a human-readable reason when a limit is violated.
    pub fn validate(&self) -> Result<(), String> {
        if self.commands.is_empty() || self.commands.len() > DESKTOP_MAX_COMMANDS {
            return Err(format!(
                "give 1 to {DESKTOP_MAX_COMMANDS} commands, got {}",
                self.commands.len()
            ));
        }
        for command in &self.commands {
            if command.trim().is_empty() {
                return Err("commands must not be empty".into());
            }
            if command.len() > DESKTOP_MAX_COMMAND_BYTES {
                return Err(format!(
                    "a command is longer than {DESKTOP_MAX_COMMAND_BYTES} bytes"
                ));
            }
        }
        let Size { width, height } = self.size;
        if !(DESKTOP_MIN_WIDTH..=DESKTOP_MAX_WIDTH).contains(&width)
            || !(DESKTOP_MIN_HEIGHT..=DESKTOP_MAX_HEIGHT).contains(&height)
        {
            return Err(format!(
                "desktop size {width}x{height} out of range \
                 ({DESKTOP_MIN_WIDTH}..={DESKTOP_MAX_WIDTH} x {DESKTOP_MIN_HEIGHT}..={DESKTOP_MAX_HEIGHT})"
            ));
        }
        if width % 2 != 0 || height % 2 != 0 {
            return Err(format!("desktop size {width}x{height} must be even"));
        }
        if !(1..=DESKTOP_MAX_SCALE).contains(&self.scale) {
            return Err(format!(
                "scale {} out of range 1..={DESKTOP_MAX_SCALE}",
                self.scale
            ));
        }
        let scale = u32::from(self.scale);
        if width % scale != 0 || height % scale != 0 {
            return Err(format!(
                "desktop size {width}x{height} is not divisible by scale {scale}"
            ));
        }
        if width / scale < DESKTOP_MIN_LOGICAL_WIDTH || height / scale < DESKTOP_MIN_LOGICAL_HEIGHT
        {
            return Err(format!(
                "logical desktop size {}x{} is below \
                 {DESKTOP_MIN_LOGICAL_WIDTH}x{DESKTOP_MIN_LOGICAL_HEIGHT}",
                width / scale,
                height / scale
            ));
        }
        Ok(())
    }
}

/// Parameters of [`Request::SetBrightness`].
///
/// Plain `u8` on the wire; the daemon validates 0..=100.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrightnessParams {
    /// Brightness in percent.
    pub value: u8,
}

/// Parameters of [`Request::SetRotation`].
///
/// Plain `u8` on the wire; the daemon validates 0..=3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotationParams {
    /// Rotation step: 0 = normal, 1 = portrait turned 90 degrees clockwise, 2 = 180 degrees,
    /// 3 = portrait turned 90 degrees counter-clockwise (the daemon renders the picture so).
    pub value: u8,
}

/// How an image is fitted to the 480x128 panel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    /// Fill the panel, cropping the overflow.
    #[default]
    Cover,
    /// Fit inside the panel, padding the rest.
    Contain,
    /// Resize to the panel size, ignoring the aspect ratio.
    Stretch,
}

impl fmt::Display for Fit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cover => "cover",
            Self::Contain => "contain",
            Self::Stretch => "stretch",
        })
    }
}

/// An unknown fit name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown fit {0:?}, expected cover, contain or stretch")]
pub struct ParseFitError(String);

impl FromStr for Fit {
    type Err = ParseFitError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "cover" => Ok(Self::Cover),
            "contain" => Ok(Self::Contain),
            "stretch" => Ok(Self::Stretch),
            other => Err(ParseFitError(other.to_owned())),
        }
    }
}

/// A region of the source in source pixels (after EXIF/rotation metadata), shown instead
/// of the whole source and then fitted per [`Fit`]. The daemon validates it against the
/// source size (`bad_crop`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Crop {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width, at least 1.
    pub width: u32,
    /// Height, at least 1.
    pub height: u32,
}

impl fmt::Display for Crop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{},{},{}", self.x, self.y, self.width, self.height)
    }
}

/// A crop that is not `X,Y,WIDTH,HEIGHT`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid crop {0:?}, expected X,Y,WIDTH,HEIGHT in source pixels")]
pub struct ParseCropError(String);

impl FromStr for Crop {
    type Err = ParseCropError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let values: Vec<u32> = s
            .split(',')
            .map(|part| part.trim().parse())
            .collect::<Result<_, _>>()
            .map_err(|_| ParseCropError(s.to_owned()))?;
        let [x, y, width, height] = values[..] else {
            return Err(ParseCropError(s.to_owned()));
        };
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }
}

/// A size in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Size {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

/// Anything the daemon sends: a reply to a request or a pushed event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    /// Reply to a [`RequestEnvelope`].
    Response(Response),
    /// Unsolicited event (after [`Request::Subscribe`]).
    Event(EventEnvelope),
}

/// A reply, correlated by `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    /// The id of the request being answered.
    pub id: u64,
    /// Success or failure.
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// The result of a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Success: the full daemon status after the call.
    Result(Status),
    /// Failure.
    Error(IpcError),
    /// Success of a dry-run upload ([`Request::UploadStandby`] or
    /// [`Request::UploadBootAnimation`]): the status and the files written.
    DryRun(DryRunReply),
    /// Reply to [`Request::Preview`].
    Preview(PreviewReply),
    /// Reply to [`Request::ProfileList`].
    Profiles(ProfilesReply),
}

/// Reply to [`Request::Preview`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewReply {
    /// JPEG file holding the latest full source frame.
    pub path: PathBuf,
    /// Source size, i.e. the crop's coordinate space.
    pub size: Size,
    /// Capture counter; unchanged means the same frame.
    pub seq: u64,
}

/// Reply to a dry-run upload; nothing was sent to the panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunReply {
    /// The daemon status (unchanged by the dry run).
    pub status: Status,
    /// Raw 1024-byte packets, back to back, in send order.
    pub packets: PathBuf,
    /// Hex summary of the packets (text).
    pub summary: PathBuf,
}

/// A pushed event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// The event.
    pub event: Event,
}

/// Event payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    /// The status changed.
    Status(Status),
}

/// Snapshot of the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// Daemon crate version.
    pub daemon_version: String,
    /// [`IPC_VERSION`](crate::IPC_VERSION) the daemon speaks.
    pub ipc_version: u32,
    /// Panel connection state.
    pub device: DeviceState,
    /// Loaded content, if any.
    pub content: Option<ContentInfo>,
    /// Stored brightness setting, 0..=100, also when idle.
    pub brightness: u8,
    /// Stored rotation step, 0..=3; absent from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<u8>,
    /// Why the stored content could not be restored at start-up (e.g. a moved video).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_error: Option<String>,
    /// Profiles, absent from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileStatus>,
}

/// Panel connection state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DeviceState {
    /// Nothing to show; device closed.
    Idle,
    /// Content is set but the panel is not (yet) open.
    Searching {
        /// Last connect error, if any.
        last_error: Option<String>,
    },
    /// Streaming to the panel.
    Connected {
        /// Panel firmware version.
        firmware: String,
    },
    /// Refusing to talk to the device (e.g. unexpected geometry); no retries.
    Blocked {
        /// Why.
        reason: String,
    },
}

/// Description of the loaded content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentInfo {
    /// File name of the original, not the full path.
    pub source: String,
    /// Still, animation or video.
    pub kind: ContentKind,
    /// Number of frames; for a video, frames per loop at the playback rate (0 if unknown).
    pub frames: u32,
    /// Fit used when rendering.
    pub fit: Fit,
    /// Source size (after EXIF/rotation metadata, before cropping): the crop's coordinate space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_size: Option<Size>,
    /// Crop applied before fitting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
    /// Video playback rate in frames per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// Video length of one loop in milliseconds, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Absolute path of the original file given to `show`; videos are played from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Frame-rate cap requested with `show` (animations and videos only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fps: Option<u32>,
    /// The live source being streamed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveSource>,
    /// Whether a live source holds back half-drawn frames (the effective setting).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steady: Option<bool>,
}

/// Kind of loaded content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContentKind {
    /// A single image.
    Still,
    /// A multi-frame GIF, APNG or WebP.
    Animation,
    /// A video file, decoded live by ffmpeg.
    Video,
    /// A live source (virtual desktop or screen), captured by the daemon.
    Live,
}

/// An error reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct IpcError {
    /// Machine-readable category.
    pub code: ErrorCode,
    /// Human-readable detail.
    pub message: String,
}

/// Error categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCode {
    /// Malformed or invalid request.
    BadRequest,
    /// A file does not exist.
    NotFound,
    /// Unsupported file format.
    Unsupported,
    /// Input too large.
    TooLarge,
    /// Rendering failed.
    Render,
    /// The crop rectangle is empty or outside the source.
    BadCrop,
    /// A runtime dependency is missing (ffmpeg for video).
    Unavailable,
    /// Daemon busy.
    Busy,
    /// Anything else.
    Internal,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BadRequest => "bad_request",
            Self::NotFound => "not_found",
            Self::Unsupported => "unsupported",
            Self::TooLarge => "too_large",
            Self::Render => "render",
            Self::BadCrop => "bad_crop",
            Self::Unavailable => "unavailable",
            Self::Busy => "busy",
            Self::Internal => "internal",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{ActiveProfile, ProfileReason, RangeEntry, TimeOfDay};

    fn status() -> Status {
        Status {
            daemon_version: "0.1.0".into(),
            ipc_version: 1,
            device: DeviceState::Connected {
                firmware: "21.0.5.4".into(),
            },
            content: None,
            brightness: 100,
            rotation: None,
            content_error: None,
            profile: None,
        }
    }

    fn check_request(json: &str, want: &RequestEnvelope) {
        assert_eq!(serde_json::to_string(want).unwrap(), json);
        assert_eq!(
            &serde_json::from_str::<RequestEnvelope>(json).unwrap(),
            want
        );
    }

    fn desktop() -> DesktopSpec {
        DesktopSpec {
            commands: vec!["foot -e top".into()],
            size: DESKTOP_DEFAULT_SIZE,
            scale: 2,
        }
    }

    #[test]
    fn show_live_desktop_request_golden() {
        check_request(
            r#"{"id":5,"method":"show_live","params":{"source":{"kind":"desktop","commands":["foot -e top"],"size":{"width":960,"height":256},"scale":2},"fit":"cover","fps":15,"steady":false}}"#,
            &RequestEnvelope {
                id: 5,
                request: Request::ShowLive(ShowLiveParams {
                    source: LiveSource::Desktop(desktop()),
                    fit: Fit::Cover,
                    crop: None,
                    fps: Some(15),
                    steady: Some(false),
                }),
            },
        );
        // Scale defaults when absent.
        let json = r#"{"commands":["a"],"size":{"width":960,"height":256}}"#;
        assert_eq!(serde_json::from_str::<DesktopSpec>(json).unwrap().scale, 2);
    }

    #[test]
    fn show_live_screen_request_golden() {
        check_request(
            r#"{"id":6,"method":"show_live","params":{"source":{"kind":"screen","output":"DP-1"},"fit":"contain","crop":{"x":0,"y":0,"width":1920,"height":512}}}"#,
            &RequestEnvelope {
                id: 6,
                request: Request::ShowLive(ShowLiveParams {
                    source: LiveSource::Screen {
                        output: Some("DP-1".into()),
                    },
                    fit: Fit::Contain,
                    crop: Some(Crop {
                        x: 0,
                        y: 0,
                        width: 1920,
                        height: 512,
                    }),
                    fps: None,
                    steady: None,
                }),
            },
        );
        check_request(
            r#"{"id":7,"method":"show_live","params":{"source":{"kind":"screen"},"fit":"cover"}}"#,
            &RequestEnvelope {
                id: 7,
                request: Request::ShowLive(ShowLiveParams {
                    source: LiveSource::Screen { output: None },
                    fit: Fit::Cover,
                    crop: None,
                    fps: None,
                    steady: None,
                }),
            },
        );
    }

    #[test]
    fn preview_request_and_reply_golden() {
        check_request(
            r#"{"id":8,"method":"preview"}"#,
            &RequestEnvelope {
                id: 8,
                request: Request::Preview,
            },
        );
        let json = r#"{"id":8,"preview":{"path":"/run/x/preview.jpg","size":{"width":1920,"height":1080},"seq":42}}"#;
        let want = ServerMessage::Response(Response {
            id: 8,
            outcome: Outcome::Preview(PreviewReply {
                path: "/run/x/preview.jpg".into(),
                size: Size {
                    width: 1920,
                    height: 1080,
                },
                seq: 42,
            }),
        });
        assert_eq!(serde_json::to_string(&want).unwrap(), json);
        assert_eq!(serde_json::from_str::<ServerMessage>(json).unwrap(), want);
    }

    #[test]
    fn live_content_info_round_trips() {
        let json = r#"{"source":"Screen DP-1","kind":"live","frames":0,"fit":"cover","live":{"kind":"portal","reselect":true}}"#;
        let info: ContentInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.kind, ContentKind::Live);
        assert_eq!(info.live, Some(LiveSource::Portal { reselect: true }));
        assert_eq!(serde_json::to_string(&info).unwrap(), json);
        let old = r#"{"source":"a.png","kind":"still","frames":1,"fit":"cover"}"#;
        assert_eq!(serde_json::from_str::<ContentInfo>(old).unwrap().live, None);
    }

    #[test]
    fn desktop_spec_validation() {
        assert_eq!(desktop().validate(), Ok(()));
        let bad = |f: &dyn Fn(&mut DesktopSpec)| {
            let mut spec = desktop();
            f(&mut spec);
            spec.validate().is_err()
        };
        assert!(bad(&|s| s.commands.clear()));
        assert!(bad(
            &|s| s.commands = vec!["x".into(); DESKTOP_MAX_COMMANDS + 1]
        ));
        assert!(bad(&|s| s.commands = vec!["  ".into()]));
        assert!(bad(&|s| s.commands = vec!["x".repeat(1025)]));
        assert!(bad(&|s| s.size.width = 238));
        assert!(bad(&|s| s.size.width = 3842));
        assert!(bad(&|s| s.size.height = 62));
        assert!(bad(&|s| {
            s.size = Size {
                width: 354,
                height: 96,
            };
            s.scale = 3;
        }));
        assert!(!bad(&|s| {
            s.size = Size {
                width: 960,
                height: 512,
            };
            s.scale = 1;
        }));
        assert!(!bad(&|s| {
            s.size = Size {
                width: 240,
                height: 64,
            };
            s.scale = 2;
        }));
        assert!(bad(&|s| s.size.height = 2162));
        assert!(bad(&|s| s.size.width = 961));
        assert!(bad(&|s| s.scale = 0));
        assert!(bad(&|s| s.scale = 4));
        assert!(bad(&|s| {
            s.size = Size {
                width: 962,
                height: 256,
            };
            s.scale = 3;
        }));
        assert!(!bad(
            &|s| s.commands = vec!["x".repeat(1024); DESKTOP_MAX_COMMANDS]
        ));
        assert!(!bad(&|s| {
            s.size = Size {
                width: 480,
                height: 128,
            };
            s.scale = 1;
        }));
    }

    #[test]
    fn subscribe_request() {
        check_request(
            r#"{"id":7,"method":"subscribe"}"#,
            &RequestEnvelope {
                id: 7,
                request: Request::Subscribe,
            },
        );
    }

    #[test]
    fn show_request_with_fps() {
        check_request(
            r#"{"id":2,"method":"show","params":{"path":"/tmp/a.mp4","fit":"cover","fps":30}}"#,
            &RequestEnvelope {
                id: 2,
                request: Request::Show(ShowParams {
                    path: "/tmp/a.mp4".into(),
                    fit: Fit::Cover,
                    crop: None,
                    fps: Some(30),
                }),
            },
        );
    }

    #[test]
    fn status_without_content_error_parses() {
        let json = r#"{"daemon_version":"0.1.0","ipc_version":1,"device":{"state":"idle"},"content":null,"brightness":5}"#;
        let got: Status = serde_json::from_str(json).unwrap();
        assert_eq!(got.content_error, None);
        assert_eq!(serde_json::to_string(&got).unwrap(), json);
    }

    #[test]
    fn status_with_content_error_round_trips() {
        let want = Status {
            content_error: Some("clip.mkv not found".into()),
            ..status()
        };
        let json = serde_json::to_string(&want).unwrap();
        assert!(json.contains(r#""content_error":"clip.mkv not found""#));
        assert_eq!(serde_json::from_str::<Status>(&json).unwrap(), want);
    }

    #[test]
    fn profile_requests_have_stable_json() {
        use crate::profile::{
            ProfileSource, ProfileSpec, ScheduleEntry, TimeOfDay, Triggers, Weekday,
        };
        let cases: Vec<(&str, RequestEnvelope)> = vec![
            (
                r#"{"id":3,"method":"profile_list"}"#,
                RequestEnvelope {
                    id: 3,
                    request: Request::ProfileList,
                },
            ),
            (
                r#"{"id":4,"method":"profile_activate","params":{"name":"Night"}}"#,
                RequestEnvelope {
                    id: 4,
                    request: Request::ProfileActivate(ProfileNameParams {
                        name: "Night".into(),
                    }),
                },
            ),
            (
                r#"{"id":5,"method":"profile_save","params":{"name":"Night"}}"#,
                RequestEnvelope {
                    id: 5,
                    request: Request::ProfileSave(ProfileSaveParams {
                        name: "Night".into(),
                        spec: None,
                    }),
                },
            ),
            (
                r#"{"id":5,"method":"profile_save","params":{"name":"Night","spec":{"fit":"cover","brightness":10,"source":{"type":"static","path":"/tmp/a.png"}}}}"#,
                RequestEnvelope {
                    id: 5,
                    request: Request::ProfileSave(ProfileSaveParams {
                        name: "Night".into(),
                        spec: Some(ProfileSpec {
                            fit: Fit::Cover,
                            brightness: Some(10),
                            source: Some(ProfileSource::Static {
                                path: "/tmp/a.png".into(),
                            }),
                            ..ProfileSpec::default()
                        }),
                    }),
                },
            ),
            (
                r#"{"id":6,"method":"profile_rename","params":{"from":"a","to":"b"}}"#,
                RequestEnvelope {
                    id: 6,
                    request: Request::ProfileRename(ProfileRenameParams {
                        from: "a".into(),
                        to: "b".into(),
                    }),
                },
            ),
            (
                r#"{"id":7,"method":"profile_delete","params":{"name":"a"}}"#,
                RequestEnvelope {
                    id: 7,
                    request: Request::ProfileDelete(ProfileNameParams { name: "a".into() }),
                },
            ),
            (
                r#"{"id":9,"method":"profile_reorder","params":{"names":["b","a"]}}"#,
                RequestEnvelope {
                    id: 9,
                    request: Request::ProfileReorder(ProfileReorderParams {
                        names: vec!["b".into(), "a".into()],
                    }),
                },
            ),
            (
                r#"{"id":8,"method":"profile_set_triggers","params":{"name":"Night","triggers":{"processes":["game.exe"],"schedule":[{"time":"22:00","days":["mon","tue"]},{"time":"07:30"}]}}}"#,
                RequestEnvelope {
                    id: 8,
                    request: Request::ProfileSetTriggers(ProfileTriggersParams {
                        name: "Night".into(),
                        triggers: Triggers {
                            processes: vec!["game.exe".into()],
                            schedule: vec![
                                ScheduleEntry {
                                    time: TimeOfDay::new(22, 0).unwrap(),
                                    days: vec![Weekday::Mon, Weekday::Tue],
                                },
                                ScheduleEntry {
                                    time: TimeOfDay::new(7, 30).unwrap(),
                                    days: Vec::new(),
                                },
                            ],
                            ranges: Vec::new(),
                        },
                    }),
                },
            ),
            (
                r#"{"id":10,"method":"profile_reset","params":{"name":"Default"}}"#,
                RequestEnvelope {
                    id: 10,
                    request: Request::ProfileReset(ProfileNameParams {
                        name: "Default".into(),
                    }),
                },
            ),
            (
                r#"{"id":11,"method":"profile_set_triggers","params":{"name":"Night","triggers":{"ranges":[{"start":"22:00","end":"06:00","days":["fri"]}]}}}"#,
                RequestEnvelope {
                    id: 11,
                    request: Request::ProfileSetTriggers(ProfileTriggersParams {
                        name: "Night".into(),
                        triggers: Triggers {
                            processes: Vec::new(),
                            schedule: Vec::new(),
                            ranges: vec![RangeEntry {
                                start: TimeOfDay::new(22, 0).unwrap(),
                                end: TimeOfDay::new(6, 0).unwrap(),
                                days: vec![Weekday::Fri],
                            }],
                        },
                    }),
                },
            ),
        ];
        for (json, want) in &cases {
            check_request(json, want);
        }
    }

    #[test]
    fn status_without_profile_round_trips_byte_identical() {
        let json = serde_json::to_string(&status()).unwrap();
        assert!(!json.contains("profile"));
        let got: Status = serde_json::from_str(&json).unwrap();
        assert_eq!(got.profile, None);
        assert_eq!(serde_json::to_string(&got).unwrap(), json);
    }

    #[test]
    fn status_with_profile_round_trips() {
        let json = r#"{"daemon_version":"0.1.0","ipc_version":1,"device":{"state":"idle"},"content":null,"brightness":100,"profile":{"revision":3,"activation":2,"active":{"name":"Night","reason":{"kind":"schedule"}}}}"#;
        let got: Status = serde_json::from_str(json).unwrap();
        let profile = got.profile.as_ref().expect("profile parsed");
        assert_eq!(profile.revision, 3);
        assert_eq!(profile.activation, 2);
        assert_eq!(
            profile.active,
            Some(ActiveProfile {
                name: "Night".into(),
                reason: ProfileReason::Schedule,
                modified: false,
            })
        );
        assert_eq!(serde_json::to_string(&got).unwrap(), json);

        let json = r#"{"daemon_version":"0.1.0","ipc_version":1,"device":{"state":"idle"},"content":null,"brightness":100,"profile":{"revision":1,"activation":1,"active":{"name":"Game","reason":{"kind":"process","process":"game.exe"}},"problem":"clip.mkv not found"}}"#;
        let got: Status = serde_json::from_str(json).unwrap();
        let profile = got.profile.as_ref().expect("profile parsed");
        assert_eq!(
            profile.active.as_ref().map(|a| &a.reason),
            Some(&ProfileReason::Process {
                process: "game.exe".into()
            })
        );
        assert_eq!(profile.problem.as_deref(), Some("clip.mkv not found"));
        assert_eq!(serde_json::to_string(&got).unwrap(), json);
    }

    #[test]
    fn status_v2_reasons_round_trip() {
        let json = r#"{"daemon_version":"0.1.0","ipc_version":1,"device":{"state":"idle"},"content":null,"brightness":100,"profile":{"revision":1,"activation":1,"active":{"name":"Default","reason":{"kind":"fallback"},"modified":true}}}"#;
        let got: Status = serde_json::from_str(json).unwrap();
        let active = got
            .profile
            .as_ref()
            .and_then(|p| p.active.as_ref())
            .unwrap();
        assert_eq!(active.reason, ProfileReason::Fallback);
        assert!(active.modified);
        assert_eq!(serde_json::to_string(&got).unwrap(), json);

        let json = r#"{"name":"Night","reason":{"kind":"range","start":"22:00","end":"06:00"}}"#;
        let got: ActiveProfile = serde_json::from_str(json).unwrap();
        assert_eq!(
            got.reason,
            ProfileReason::Range {
                start: TimeOfDay::new(22, 0).unwrap(),
                end: TimeOfDay::new(6, 0).unwrap(),
            }
        );
        assert!(!got.modified);
        assert_eq!(serde_json::to_string(&got).unwrap(), json);

        let got: ActiveProfile =
            serde_json::from_str(r#"{"name":"x","reason":{"kind":"lunar"}}"#).unwrap();
        assert_eq!(got.reason, ProfileReason::Unknown);
    }

    #[test]
    fn clear_source_golden() {
        use crate::profile::ProfileSource;
        let json = r#"{"type":"clear"}"#;
        let got: ProfileSource = serde_json::from_str(json).unwrap();
        assert_eq!(got, ProfileSource::Clear);
        assert_eq!(serde_json::to_string(&got).unwrap(), json);
    }

    #[test]
    fn profiles_outcome_round_trips() {
        use crate::profile::{Profile, ProfileSpec, Triggers};
        let want = ServerMessage::Response(Response {
            id: 10,
            outcome: Outcome::Profiles(ProfilesReply {
                profiles: vec![Profile {
                    name: "Dim".into(),
                    builtin: None,
                    spec: ProfileSpec {
                        brightness: Some(5),
                        ..ProfileSpec::default()
                    },
                    triggers: Triggers::default(),
                }],
                status: status(),
            }),
        });
        let json = serde_json::to_string(&want).unwrap();
        assert!(
            json.starts_with(r#"{"id":10,"profiles":{"profiles":["#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<ServerMessage>(&json).unwrap(), want);
    }

    #[test]
    fn content_info_with_path_and_max_fps_round_trips() {
        let json = r#"{"source":"a.gif","kind":"animation","frames":3,"fit":"cover","path":"/tmp/a.gif","max_fps":15}"#;
        let info: ContentInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.path, Some(PathBuf::from("/tmp/a.gif")));
        assert_eq!(info.max_fps, Some(15));
        assert_eq!(serde_json::to_string(&info).unwrap(), json);
    }

    #[test]
    fn upload_standby_request_golden() {
        check_request(
            r#"{"id":3,"method":"upload_standby","params":{"path":"/tmp/a.png","fit":"contain","crop":{"x":1,"y":2,"width":3,"height":4},"dry_run":true}}"#,
            &RequestEnvelope {
                id: 3,
                request: Request::UploadStandby(UploadStandbyParams {
                    path: "/tmp/a.png".into(),
                    fit: Fit::Contain,
                    crop: Some(Crop {
                        x: 1,
                        y: 2,
                        width: 3,
                        height: 4,
                    }),
                    dry_run: true,
                }),
            },
        );
        check_request(
            r#"{"id":4,"method":"upload_standby","params":{"path":"/tmp/a.png","fit":"cover","dry_run":false}}"#,
            &RequestEnvelope {
                id: 4,
                request: Request::UploadStandby(UploadStandbyParams {
                    path: "/tmp/a.png".into(),
                    fit: Fit::Cover,
                    crop: None,
                    dry_run: false,
                }),
            },
        );
    }

    #[test]
    fn upload_standby_requires_dry_run() {
        let json = r#"{"id":4,"method":"upload_standby","params":{"path":"/tmp/a.png"}}"#;
        assert!(serde_json::from_str::<RequestEnvelope>(json).is_err());
    }

    #[test]
    fn upload_boot_animation_request_golden() {
        check_request(
            r#"{"id":5,"method":"upload_boot_animation","params":{"path":"/tmp/a.gif","fit":"contain","crop":{"x":1,"y":2,"width":3,"height":4},"dry_run":true}}"#,
            &RequestEnvelope {
                id: 5,
                request: Request::UploadBootAnimation(UploadBootAnimationParams {
                    path: "/tmp/a.gif".into(),
                    fit: Fit::Contain,
                    crop: Some(Crop {
                        x: 1,
                        y: 2,
                        width: 3,
                        height: 4,
                    }),
                    dry_run: true,
                }),
            },
        );
        check_request(
            r#"{"id":6,"method":"upload_boot_animation","params":{"path":"/tmp/a.gif","fit":"cover","dry_run":false}}"#,
            &RequestEnvelope {
                id: 6,
                request: Request::UploadBootAnimation(UploadBootAnimationParams {
                    path: "/tmp/a.gif".into(),
                    fit: Fit::Cover,
                    crop: None,
                    dry_run: false,
                }),
            },
        );
    }

    #[test]
    fn upload_boot_animation_requires_dry_run() {
        let json = r#"{"id":4,"method":"upload_boot_animation","params":{"path":"/tmp/a.gif"}}"#;
        assert!(serde_json::from_str::<RequestEnvelope>(json).is_err());
    }

    #[test]
    fn dry_run_reply_golden() {
        let want = Response {
            id: 3,
            outcome: Outcome::DryRun(DryRunReply {
                status: status(),
                packets: "/s/standby-dryrun.bin".into(),
                summary: "/s/standby-dryrun.txt".into(),
            }),
        };
        let json = serde_json::to_string(&want).unwrap();
        assert_eq!(
            json,
            r#"{"id":3,"dry_run":{"status":{"daemon_version":"0.1.0","ipc_version":1,"device":{"state":"connected","firmware":"21.0.5.4"},"content":null,"brightness":100},"packets":"/s/standby-dryrun.bin","summary":"/s/standby-dryrun.txt"}}"#
        );
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), want);
    }

    #[test]
    fn status_request() {
        check_request(
            r#"{"id":1,"method":"status"}"#,
            &RequestEnvelope {
                id: 1,
                request: Request::Status,
            },
        );
    }

    #[test]
    fn show_request() {
        check_request(
            r#"{"id":2,"method":"show","params":{"path":"/tmp/a.gif","fit":"cover"}}"#,
            &RequestEnvelope {
                id: 2,
                request: Request::Show(ShowParams {
                    path: "/tmp/a.gif".into(),
                    fit: Fit::Cover,
                    crop: None,
                    fps: None,
                }),
            },
        );
    }

    #[test]
    fn show_without_fit_defaults_to_cover() {
        let got: RequestEnvelope =
            serde_json::from_str(r#"{"id":2,"method":"show","params":{"path":"/tmp/a.gif"}}"#)
                .unwrap();
        assert_eq!(
            got.request,
            Request::Show(ShowParams {
                path: "/tmp/a.gif".into(),
                fit: Fit::Cover,
                crop: None,
                fps: None,
            })
        );
    }

    #[test]
    fn show_request_with_crop() {
        check_request(
            r#"{"id":5,"method":"show","params":{"path":"/tmp/a.mp4","fit":"contain","crop":{"x":10,"y":20,"width":480,"height":128}}}"#,
            &RequestEnvelope {
                id: 5,
                request: Request::Show(ShowParams {
                    path: "/tmp/a.mp4".into(),
                    fit: Fit::Contain,
                    crop: Some(Crop {
                        x: 10,
                        y: 20,
                        width: 480,
                        height: 128,
                    }),
                    fps: None,
                }),
            },
        );
    }

    #[test]
    fn video_content_info() {
        let json = r#"{"source":"clip.mp4","kind":"video","frames":180,"fit":"cover","source_size":{"width":1920,"height":1080},"crop":{"x":0,"y":476,"width":1920,"height":512},"fps":15,"duration_ms":12000}"#;
        let want = ContentInfo {
            source: "clip.mp4".into(),
            kind: ContentKind::Video,
            frames: 180,
            fit: Fit::Cover,
            source_size: Some(Size {
                width: 1920,
                height: 1080,
            }),
            crop: Some(Crop {
                x: 0,
                y: 476,
                width: 1920,
                height: 512,
            }),
            fps: Some(15),
            duration_ms: Some(12_000),
            path: None,
            max_fps: None,
            live: None,
            steady: None,
        };
        assert_eq!(serde_json::to_string(&want).unwrap(), json);
        assert_eq!(serde_json::from_str::<ContentInfo>(json).unwrap(), want);
    }

    #[test]
    fn content_info_without_new_fields_round_trips() {
        let json = r#"{"source":"a.png","kind":"still","frames":1,"fit":"cover"}"#;
        let info: ContentInfo = serde_json::from_str(json).unwrap();
        assert_eq!(
            (info.source_size, info.crop, info.fps, info.duration_ms),
            (None, None, None, None)
        );
        assert_eq!(serde_json::to_string(&info).unwrap(), json);
    }

    #[test]
    fn crop_display_and_parse() {
        let crop = Crop {
            x: 10,
            y: 20,
            width: 480,
            height: 128,
        };
        assert_eq!(crop.to_string(), "10,20,480,128");
        assert_eq!("10,20,480,128".parse::<Crop>(), Ok(crop));
        assert_eq!(" 10, 20,480 ,128".parse::<Crop>(), Ok(crop));
        for bad in ["", "1,2,3", "1,2,3,4,5", "a,b,c,d", "-1,0,1,1", "1;2;3;4"] {
            assert!(bad.parse::<Crop>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn new_error_codes_match_their_wire_names() {
        for (code, name) in [
            (ErrorCode::BadCrop, "bad_crop"),
            (ErrorCode::Unavailable, "unavailable"),
        ] {
            assert_eq!(code.to_string(), name);
            assert_eq!(serde_json::to_string(&code).unwrap(), format!("\"{name}\""));
        }
    }

    #[test]
    fn clear_request() {
        check_request(
            r#"{"id":3,"method":"clear"}"#,
            &RequestEnvelope {
                id: 3,
                request: Request::Clear,
            },
        );
    }

    #[test]
    fn set_brightness_request() {
        check_request(
            r#"{"id":4,"method":"set_brightness","params":{"value":40}}"#,
            &RequestEnvelope {
                id: 4,
                request: Request::SetBrightness(BrightnessParams { value: 40 }),
            },
        );
    }

    #[test]
    fn set_rotation_request() {
        check_request(
            r#"{"id":5,"method":"set_rotation","params":{"value":2}}"#,
            &RequestEnvelope {
                id: 5,
                request: Request::SetRotation(RotationParams { value: 2 }),
            },
        );
    }

    #[test]
    fn status_rotation_is_omitted_when_none_and_round_trips() {
        let mut s = status();
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("rotation"), "{json}");
        s.rotation = Some(3);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""brightness":100,"rotation":3"#), "{json}");
        assert_eq!(serde_json::from_str::<Status>(&json).unwrap(), s);
        // A status from an older daemon has no rotation field.
        let old = r#"{"daemon_version":"0.1.0","ipc_version":1,"device":{"state":"idle"},"content":null,"brightness":5}"#;
        assert_eq!(serde_json::from_str::<Status>(old).unwrap().rotation, None);
    }

    #[test]
    fn result_response() {
        let json = r#"{"id":1,"result":{"daemon_version":"0.1.0","ipc_version":1,"device":{"state":"connected","firmware":"21.0.5.4"},"content":null,"brightness":100}}"#;
        let want = ServerMessage::Response(Response {
            id: 1,
            outcome: Outcome::Result(status()),
        });
        assert_eq!(serde_json::to_string(&want).unwrap(), json);
        assert_eq!(serde_json::from_str::<ServerMessage>(json).unwrap(), want);
    }

    #[test]
    fn error_response() {
        let json = r#"{"id":2,"error":{"code":"not_found","message":"no such file"}}"#;
        let want = ServerMessage::Response(Response {
            id: 2,
            outcome: Outcome::Error(IpcError {
                code: ErrorCode::NotFound,
                message: "no such file".into(),
            }),
        });
        assert_eq!(serde_json::to_string(&want).unwrap(), json);
        assert_eq!(serde_json::from_str::<ServerMessage>(json).unwrap(), want);
    }

    #[test]
    fn event_parses_as_event() {
        let json = r#"{"event":{"type":"status","daemon_version":"0.1.0","ipc_version":1,"device":{"state":"idle"},"content":null,"brightness":50}}"#;
        let got: ServerMessage = serde_json::from_str(json).unwrap();
        let ServerMessage::Event(EventEnvelope {
            event: Event::Status(s),
        }) = got.clone()
        else {
            panic!("expected event, got {got:?}");
        };
        assert_eq!(s.device, DeviceState::Idle);
        assert_eq!(s.brightness, 50);
        assert_eq!(serde_json::to_string(&got).unwrap(), json);
    }

    #[test]
    fn fit_display_and_parse() {
        for fit in [Fit::Cover, Fit::Contain, Fit::Stretch] {
            assert_eq!(fit.to_string().parse::<Fit>(), Ok(fit));
        }
        assert!("zoom".parse::<Fit>().is_err());
    }
}
