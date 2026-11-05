//! Human-readable status output.

use std::fmt::Write;

use tt500_lcd_ipc::{ContentInfo, ContentKind, DeviceState, LiveSource, Status};

/// The status as the lines `tt500-lcd status` prints.
pub fn format_status(status: &Status) -> String {
    let panel = match &status.device {
        DeviceState::Idle => "idle".to_owned(),
        DeviceState::Searching { last_error: None } => "searching".to_owned(),
        DeviceState::Searching {
            last_error: Some(err),
        } => format!("searching ({err})"),
        DeviceState::Connected { firmware } => format!("connected (firmware {firmware})"),
        DeviceState::Blocked { reason } => format!("blocked: {reason}"),
        _ => "unknown".to_owned(),
    };
    let showing = status
        .content
        .as_ref()
        .map_or_else(|| "nothing".to_owned(), format_content);
    let brightness = match status.brightness {
        0 => "0% (off)".to_owned(),
        n => format!("{n}%"),
    };
    let mut text = format!("Panel: {panel}\nShowing: {showing}\nBrightness: {brightness}");
    if let Some(rotation) = status.rotation {
        let _ = write!(text, "\nRotation: {rotation}");
    }
    if let Some(err) = &status.content_error {
        let _ = write!(text, "\nProblem: {err}");
    }
    if let Some(profile) = &status.profile {
        if let Some(active) = &profile.active {
            let changed = if active.modified { ", changed" } else { "" };
            let _ = write!(
                text,
                "\nProfile: {} ({}{changed})",
                active.name, active.reason
            );
        }
        if let Some(problem) = &profile.problem {
            let _ = write!(text, "\nProfile problem: {problem}");
        }
    }
    text
}

fn format_content(content: &ContentInfo) -> String {
    let mut detail = match content.kind {
        ContentKind::Animation => format!("animation, {} frames, {}", content.frames, content.fit),
        ContentKind::Video => match content.fps {
            Some(fps) => format!("video, {fps} fps, {}", content.fit),
            None => format!("video, {}", content.fit),
        },
        ContentKind::Live => match &content.live {
            Some(source) => format!("live, {}, {}", format_live(source), content.fit),
            None => format!("live, {}", content.fit),
        },
        _ => format!("still, {}", content.fit),
    };
    if let Some(crop) = content.crop {
        let _ = write!(detail, ", crop {crop}");
    }
    format!("{} ({detail})", content.source)
}

pub(crate) fn format_live(source: &LiveSource) -> String {
    match source {
        LiveSource::Desktop(spec) => format!(
            "virtual desktop {}x{} @{}x, {} command(s)",
            spec.size.width,
            spec.size.height,
            spec.scale,
            spec.commands.len()
        ),
        LiveSource::Screen { output: Some(o) } => format!("screen {o}"),
        LiveSource::Screen { output: None } => "screen".to_owned(),
        LiveSource::Portal { .. } => "screen (portal)".to_owned(),
        _ => "unknown source".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt500_lcd_ipc::{Crop, Fit, IPC_VERSION};

    fn status(device: DeviceState, content: Option<ContentInfo>, brightness: u8) -> Status {
        Status {
            daemon_version: "0.1.0".into(),
            ipc_version: IPC_VERSION,
            device,
            content,
            brightness,
            rotation: None,
            content_error: None,
            profile: None,
        }
    }

    fn gif() -> ContentInfo {
        ContentInfo {
            source: "cat.gif".into(),
            kind: ContentKind::Animation,
            frames: 42,
            fit: Fit::Cover,
            source_size: None,
            crop: None,
            fps: None,
            duration_ms: None,
            path: None,
            max_fps: None,
            live: None,
            steady: None,
        }
    }

    #[test]
    fn connected_with_animation() {
        let s = status(
            DeviceState::Connected {
                firmware: "21.0.5.4".into(),
            },
            Some(gif()),
            40,
        );
        assert_eq!(
            format_status(&s),
            "Panel: connected (firmware 21.0.5.4)\nShowing: cat.gif (animation, 42 frames, cover)\nBrightness: 40%"
        );
    }

    #[test]
    fn content_error_adds_problem_line() {
        let mut s = status(DeviceState::Idle, None, 0);
        s.content_error = Some("clip.mkv not found".into());
        assert!(format_status(&s).ends_with("\nProblem: clip.mkv not found"));
    }

    #[test]
    fn rotation_line_only_when_reported() {
        let mut s = status(DeviceState::Idle, None, 100);
        assert!(!format_status(&s).contains("Rotation"));
        s.rotation = Some(2);
        assert_eq!(
            format_status(&s),
            "Panel: idle\nShowing: nothing\nBrightness: 100%\nRotation: 2"
        );
    }

    #[test]
    fn idle_nothing_brightness_off() {
        let s = status(DeviceState::Idle, None, 0);
        assert_eq!(
            format_status(&s),
            "Panel: idle\nShowing: nothing\nBrightness: 0% (off)"
        );
    }

    #[test]
    fn searching_with_and_without_error() {
        let s = status(DeviceState::Searching { last_error: None }, None, 100);
        assert!(format_status(&s).starts_with("Panel: searching\n"));
        let s = status(
            DeviceState::Searching {
                last_error: Some("no device".into()),
            },
            None,
            100,
        );
        assert!(format_status(&s).starts_with("Panel: searching (no device)\n"));
    }

    #[test]
    fn blocked_and_still() {
        let still = ContentInfo {
            kind: ContentKind::Still,
            frames: 1,
            fit: Fit::Contain,
            ..gif()
        };
        let s = status(
            DeviceState::Blocked {
                reason: "unexpected geometry".into(),
            },
            Some(still),
            100,
        );
        assert_eq!(
            format_status(&s),
            "Panel: blocked: unexpected geometry\nShowing: cat.gif (still, contain)\nBrightness: 100%"
        );
    }

    #[test]
    fn video_with_crop() {
        let video = ContentInfo {
            source: "clip.mp4".into(),
            kind: ContentKind::Video,
            fps: Some(15),
            crop: Some(Crop {
                x: 0,
                y: 40,
                width: 480,
                height: 128,
            }),
            ..gif()
        };
        let s = status(DeviceState::Idle, Some(video), 100);
        assert!(
            format_status(&s)
                .contains("Showing: clip.mp4 (video, 15 fps, cover, crop 0,40,480,128)"),
            "{}",
            format_status(&s)
        );
    }

    #[test]
    fn live_desktop_and_screen() {
        let live = |source| ContentInfo {
            source: "desktop".into(),
            kind: ContentKind::Live,
            live: Some(source),
            ..gif()
        };
        let s = status(
            DeviceState::Idle,
            Some(live(LiveSource::Desktop(tt500_lcd_ipc::DesktopSpec {
                commands: vec!["top".into()],
                size: tt500_lcd_ipc::DESKTOP_DEFAULT_SIZE,
                scale: 2,
            }))),
            100,
        );
        assert!(
            format_status(&s).contains("(live, virtual desktop 960x256 @2x, 1 command(s), cover)"),
            "{}",
            format_status(&s)
        );
        let s = status(
            DeviceState::Idle,
            Some(live(LiveSource::Screen {
                output: Some("DP-1".into()),
            })),
            100,
        );
        assert!(format_status(&s).contains("(live, screen DP-1, cover)"));
    }

    #[test]
    fn profile_lines_only_when_reported() {
        use tt500_lcd_ipc::{ActiveProfile, ProfileReason, ProfileStatus};
        let mut s = status(DeviceState::Idle, None, 0);
        assert!(!format_status(&s).contains("Profile"));
        s.profile = Some(ProfileStatus {
            revision: 1,
            activation: 1,
            active: Some(ActiveProfile {
                name: "Night".into(),
                reason: ProfileReason::Schedule,
                modified: false,
            }),
            problem: Some("clip.mkv not found".into()),
        });
        let text = format_status(&s);
        assert!(text.contains("\nProfile: Night (schedule)"));
        if let Some(p) = s.profile.as_mut().and_then(|p| p.active.as_mut()) {
            p.name = "Default".into();
            p.reason = ProfileReason::Fallback;
            p.modified = true;
        }
        assert!(format_status(&s).contains("\nProfile: Default (fallback, changed)"));
        assert!(text.ends_with("\nProfile problem: clip.mkv not found"));
    }
}
