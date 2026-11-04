//! The editor draft and the connection row, without GTK.

use std::path::PathBuf;

use tt500_lcd_ipc::{ContentInfo, Crop, DEFAULT_FPS, Fit, ShowParams, Size, Status};

use crate::geometry::CropView;
use crate::media::SourceKind;

/// Hint shown while the daemon is not running.
pub const START_HINT: &str = "Start it with: systemctl --user enable --now tt500-lcd.service";

/// How the source is placed on the panel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Framing {
    /// Pan/zoom crop, sent as `cover` plus a crop rectangle.
    Crop,
    /// The whole source placed by a fit mode, no crop.
    Fit(Fit),
}

/// Pan/zoom and framing of a source of known size: what the crop editor shows.
#[derive(Debug, Clone, Copy)]
pub struct Framed {
    /// Decoded source size, the daemon's crop space.
    pub size: Size,
    /// Pan/zoom state of the crop editor.
    pub view: CropView,
    /// Crop or fit mode.
    pub framing: Framing,
}

impl Framed {
    /// Crop mode with a centred view.
    #[must_use]
    pub fn new(size: Size) -> Self {
        Self {
            size,
            view: CropView::new(size),
            framing: Framing::Crop,
        }
    }

    /// Takes framing and crop from the daemon's content when it shows this same source.
    pub fn adopt(&mut self, info: &ContentInfo) {
        match info.crop {
            Some(crop) => {
                self.framing = Framing::Crop;
                self.view = CropView::from_crop(self.size, crop);
            }
            None => self.framing = Framing::Fit(info.fit),
        }
    }

    /// The fit mode and crop Apply and the flash uploads send.
    #[must_use]
    pub fn fit_and_crop(&self) -> (Fit, Option<Crop>) {
        match self.framing {
            Framing::Crop => (Fit::Cover, Some(self.view.crop())),
            Framing::Fit(fit) => (fit, None),
        }
    }
}

/// The file being edited and what Apply will send, apart from its framing.
#[derive(Debug, Clone)]
pub struct Draft {
    /// Absolute path of the file.
    pub path: PathBuf,
    /// Still, animation or video.
    pub kind: SourceKind,
    /// Frame-rate cap, used by animations and videos.
    pub fps: u32,
}

impl Draft {
    /// A fresh draft with the default frame-rate cap.
    #[must_use]
    pub fn new(path: PathBuf, kind: SourceKind) -> Self {
        Self {
            path,
            kind,
            fps: DEFAULT_FPS,
        }
    }

    /// Takes the frame-rate cap from the daemon's content when it shows this same file.
    pub fn adopt(&mut self, info: &ContentInfo) {
        self.fps = info.max_fps.unwrap_or(DEFAULT_FPS);
    }

    /// The parameters Apply sends to the daemon.
    #[must_use]
    pub fn show_params(&self, framed: &Framed) -> ShowParams {
        let (fit, crop) = framed.fit_and_crop();
        ShowParams {
            path: self.path.clone(),
            fit,
            crop,
            fps: self.uses_fps().then_some(self.fps),
        }
    }

    /// Whether the frame-rate cap matters for this file.
    #[must_use]
    pub fn uses_fps(&self) -> bool {
        matches!(self.kind, SourceKind::Animation | SourceKind::Video)
    }
}

/// What the status row shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    /// The daemon cannot be reached.
    NotRunning,
    /// Nothing is shown; the panel is closed.
    Idle,
    /// Looking for the panel, with the last connect error if any.
    Searching(Option<String>),
    /// Streaming to the panel; carries the firmware version.
    Connected(String),
    /// The daemon refuses to talk to the device; carries the reason.
    Blocked(String),
}

impl Connection {
    /// The row state for the latest status, `None` when the daemon is unreachable.
    #[must_use]
    pub fn from_status(status: Option<&Status>) -> Self {
        use tt500_lcd_ipc::DeviceState;
        match status.map(|s| &s.device) {
            None => Self::NotRunning,
            Some(DeviceState::Searching { last_error }) => Self::Searching(last_error.clone()),
            Some(DeviceState::Connected { firmware }) => Self::Connected(firmware.clone()),
            Some(DeviceState::Blocked { reason }) => Self::Blocked(reason.clone()),
            Some(_) => Self::Idle,
        }
    }

    /// The row title.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::NotRunning => "Daemon not running",
            Self::Idle => "Idle",
            Self::Searching(_) => "Looking for the panel",
            Self::Connected(_) => "Connected",
            Self::Blocked(_) => "Blocked",
        }
    }

    /// The row subtitle.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::NotRunning => START_HINT.to_owned(),
            Self::Idle => "Nothing is shown".to_owned(),
            Self::Searching(Some(error)) => error.clone(),
            Self::Searching(None) => "Waiting for the panel to appear".to_owned(),
            Self::Connected(firmware) => format!("Firmware {firmware}"),
            Self::Blocked(reason) => reason.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt500_lcd_ipc::{ContentKind, Crop, DeviceState};

    fn size() -> Size {
        Size {
            width: 1920,
            height: 1080,
        }
    }

    fn draft(kind: SourceKind) -> Draft {
        Draft::new(PathBuf::from("/m/clip"), kind)
    }

    fn framed() -> Framed {
        Framed::new(size())
    }

    fn info(crop: Option<Crop>, fit: Fit, max_fps: Option<u32>) -> ContentInfo {
        ContentInfo {
            source: "clip".into(),
            kind: ContentKind::Video,
            frames: 1,
            fit,
            source_size: Some(size()),
            crop,
            fps: None,
            duration_ms: None,
            path: Some(PathBuf::from("/m/clip")),
            max_fps,
            live: None,
            steady: None,
        }
    }

    fn status(device: DeviceState) -> Status {
        Status {
            daemon_version: "0".into(),
            ipc_version: 1,
            device,
            content: None,
            brightness: 50,
            content_error: None,
        }
    }

    #[test]
    fn crop_params() {
        let params = draft(SourceKind::Still).show_params(&framed());
        assert_eq!(params.fit, Fit::Cover);
        assert_eq!(
            params.crop,
            Some(Crop {
                x: 0,
                y: 284,
                width: 1920,
                height: 512
            })
        );
        assert_eq!(params.fps, None);
        assert_eq!(params.path, PathBuf::from("/m/clip"));
    }

    #[test]
    fn fit_params_have_no_crop() {
        let mut f = framed();
        f.framing = Framing::Fit(Fit::Contain);
        let params = draft(SourceKind::Still).show_params(&f);
        assert_eq!((params.fit, params.crop), (Fit::Contain, None));
    }

    #[test]
    fn fps_only_for_animations_and_videos() {
        let mut video = draft(SourceKind::Video);
        video.fps = 24;
        assert_eq!(video.show_params(&framed()).fps, Some(24));
        assert!(video.uses_fps());
        let animation = draft(SourceKind::Animation);
        assert_eq!(animation.show_params(&framed()).fps, Some(15));
        assert!(!draft(SourceKind::Still).uses_fps());
    }

    #[test]
    fn adopt_with_crop() {
        let mut f = framed();
        let crop = Crop {
            x: 100,
            y: 100,
            width: 960,
            height: 256,
        };
        f.framing = Framing::Fit(Fit::Stretch);
        f.adopt(&info(Some(crop), Fit::Cover, Some(24)));
        assert_eq!(f.framing, Framing::Crop);
        assert_eq!(f.view.crop(), crop);
        assert_eq!(f.fit_and_crop(), (Fit::Cover, Some(crop)));
    }

    #[test]
    fn adopt_without_crop() {
        let mut f = framed();
        f.adopt(&info(None, Fit::Contain, None));
        assert_eq!(f.framing, Framing::Fit(Fit::Contain));
        assert_eq!(f.fit_and_crop(), (Fit::Contain, None));
    }

    #[test]
    fn draft_adopts_the_fps_cap() {
        let mut d = draft(SourceKind::Video);
        d.adopt(&info(None, Fit::Cover, Some(24)));
        assert_eq!(d.fps, 24);
        d.adopt(&info(None, Fit::Cover, None));
        assert_eq!(d.fps, DEFAULT_FPS);
    }

    #[test]
    fn connection_per_device_state() {
        assert_eq!(Connection::from_status(None), Connection::NotRunning);
        let cases = [
            (DeviceState::Idle, Connection::Idle),
            (
                DeviceState::Searching { last_error: None },
                Connection::Searching(None),
            ),
            (
                DeviceState::Connected {
                    firmware: "1.2".into(),
                },
                Connection::Connected("1.2".into()),
            ),
            (
                DeviceState::Blocked {
                    reason: "odd".into(),
                },
                Connection::Blocked("odd".into()),
            ),
        ];
        for (device, want) in cases {
            assert_eq!(Connection::from_status(Some(&status(device))), want);
        }
    }

    #[test]
    fn connection_texts() {
        assert_eq!(Connection::NotRunning.title(), "Daemon not running");
        assert_eq!(Connection::NotRunning.detail(), START_HINT);
        assert_eq!(Connection::Idle.detail(), "Nothing is shown");
        assert_eq!(Connection::Connected("1.2".into()).detail(), "Firmware 1.2");
        assert_eq!(Connection::Searching(Some("busy".into())).detail(), "busy");
        assert_eq!(Connection::Searching(None).title(), "Looking for the panel");
        assert_eq!(Connection::Blocked("x".into()).title(), "Blocked");
    }
}
