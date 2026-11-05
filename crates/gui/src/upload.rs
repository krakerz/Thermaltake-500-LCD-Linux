//! Writing the loaded file to the panel's flash (standby image, boot animation), without GTK.

use std::fs;
use std::path::Path;

use tt500_lcd_ipc::{Client, Request, UploadBootAnimationParams, UploadStandbyParams};

use crate::daemon::describe;
use crate::media::SourceKind;
use crate::state::{Draft, Framed};

/// Most summary lines shown in the confirmation dialog.
const MAX_SUMMARY_LINES: usize = 10;

/// What is written to the flash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadKind {
    /// The image the panel shows while the PC is off or asleep.
    Standby,
    /// The animation the panel plays at power-on.
    BootAnimation,
}

/// Which kinds of file a file source takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accept {
    /// Stills, animations and videos.
    Any,
    /// Stills only.
    Still,
    /// Animated GIF, APNG and WebP only.
    Animation,
}

impl Accept {
    /// Whether a source of `kind` is taken.
    #[must_use]
    pub fn allows(self, kind: SourceKind) -> bool {
        match self {
            Self::Any => true,
            Self::Still => UploadKind::Standby.accepts(kind),
            Self::Animation => UploadKind::BootAnimation.accepts(kind),
        }
    }

    /// Why a file was refused, for a toast.
    #[must_use]
    pub fn refusal(self) -> &'static str {
        match self {
            Self::Any => "unsupported file",
            Self::Still => "not a still image",
            Self::Animation => "not an animated GIF, PNG or WebP",
        }
    }
}

impl UploadKind {
    /// Whether a source of `kind` can be written this way: stills as standby images,
    /// animated GIF/APNG/WebP as boot animations (videos are neither).
    #[must_use]
    pub fn accepts(self, kind: SourceKind) -> bool {
        matches!(
            (self, kind),
            (Self::Standby, SourceKind::Still) | (Self::BootAnimation, SourceKind::Animation)
        )
    }

    /// Whether the action is enabled: a loaded draft of a fitting kind and a running daemon.
    #[must_use]
    pub fn enabled(self, draft: Option<&Draft>, daemon_running: bool) -> bool {
        daemon_running && draft.is_some_and(|draft| self.accepts(draft.kind))
    }

    /// What the file picker and the drop target take for this upload.
    #[must_use]
    pub fn accept(self) -> Accept {
        match self {
            Self::Standby => Accept::Still,
            Self::BootAnimation => Accept::Animation,
        }
    }

    /// The toast shown once the write is done.
    #[must_use]
    pub fn done_message(self) -> &'static str {
        match self {
            Self::Standby => "Standby image written",
            Self::BootAnimation => {
                "Boot animation written — it plays at the next power-on (full shutdown)"
            }
        }
    }

    /// The upload request for `draft` with `framed`'s framing and crop, as Apply sends it.
    #[must_use]
    pub fn request(self, draft: &Draft, framed: &Framed, dry_run: bool) -> Request {
        let (fit, crop) = framed.fit_and_crop();
        let path = draft.path.clone();
        match self {
            Self::Standby => Request::UploadStandby(UploadStandbyParams {
                path,
                fit,
                crop,
                dry_run,
            }),
            Self::BootAnimation => Request::UploadBootAnimation(UploadBootAnimationParams {
                path,
                fit,
                crop,
                dry_run,
            }),
        }
    }
}

/// Sends an upload request; for a dry run returns the summary text to show.
/// Blocking: run it with `gio::spawn_blocking`.
///
/// # Errors
/// Returns user-facing text: a missing daemon, or the daemon's own message.
pub fn run(socket: &Path, request: Request) -> Result<Option<String>, String> {
    let mut client = Client::connect(socket).map_err(describe)?;
    let (_status, dry) = match request {
        Request::UploadStandby(params) => client.upload_standby(params),
        Request::UploadBootAnimation(params) => client.upload_boot_animation(params),
        _ => return Err("not an upload request".to_owned()),
    }
    .map_err(describe)?;
    Ok(dry.map(|reply| {
        fs::read_to_string(&reply.summary)
            .map(|text| display_summary(&text))
            .unwrap_or_default()
    }))
}

/// The dry-run summary without the hex dumps: header and per-packet/per-block detail lines
/// are dropped, at most [`MAX_SUMMARY_LINES`] lines are kept.
#[must_use]
pub fn display_summary(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let line = line.trim_end();
            !(line.is_empty()
                || line.starts_with(char::is_whitespace)
                || line.starts_with("block ")
                || line.contains("dry run")
                || line
                    .split_once(':')
                    .is_some_and(|(head, _)| head.trim().chars().all(|c| c.is_ascii_digit())))
        })
        .take(MAX_SUMMARY_LINES)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tt500_lcd_ipc::Size;

    fn draft(kind: SourceKind) -> Draft {
        Draft::new(PathBuf::from("/m/clip"), kind)
    }

    fn framed() -> Framed {
        Framed::new(
            Size {
                width: 1920,
                height: 1080,
            },
            crate::geometry::PANEL_ASPECT,
        )
    }

    #[test]
    fn standby_takes_stills_and_boot_takes_animations() {
        assert!(UploadKind::Standby.accepts(SourceKind::Still));
        assert!(!UploadKind::Standby.accepts(SourceKind::Animation));
        assert!(!UploadKind::Standby.accepts(SourceKind::Video));
        assert!(UploadKind::BootAnimation.accepts(SourceKind::Animation));
        assert!(!UploadKind::BootAnimation.accepts(SourceKind::Still));
        assert!(!UploadKind::BootAnimation.accepts(SourceKind::Video));
    }

    #[test]
    fn accept_filters_by_kind() {
        for kind in [SourceKind::Still, SourceKind::Animation, SourceKind::Video] {
            assert!(Accept::Any.allows(kind));
            assert_eq!(Accept::Still.allows(kind), kind == SourceKind::Still);
            assert_eq!(
                Accept::Animation.allows(kind),
                kind == SourceKind::Animation
            );
        }
        assert_eq!(UploadKind::Standby.accept(), Accept::Still);
        assert_eq!(UploadKind::BootAnimation.accept(), Accept::Animation);
    }

    #[test]
    fn needs_a_draft_and_a_running_daemon() {
        let still = draft(SourceKind::Still);
        assert!(UploadKind::Standby.enabled(Some(&still), true));
        assert!(!UploadKind::Standby.enabled(Some(&still), false));
        assert!(!UploadKind::Standby.enabled(None, true));
        assert!(!UploadKind::BootAnimation.enabled(Some(&still), true));
    }

    #[test]
    fn request_carries_the_draft_framing_and_the_dry_run_flag() {
        let draft = draft(SourceKind::Animation);
        let framed = framed();
        let (fit, crop) = framed.fit_and_crop();
        for dry_run in [true, false] {
            match UploadKind::BootAnimation.request(&draft, &framed, dry_run) {
                Request::UploadBootAnimation(p) => {
                    assert_eq!((p.fit, p.crop, p.dry_run), (fit, crop, dry_run));
                    assert_eq!(p.path, PathBuf::from("/m/clip"));
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(matches!(
            UploadKind::Standby.request(&draft, &framed, true),
            Request::UploadStandby(p) if p.dry_run
        ));
    }

    #[test]
    fn summary_keeps_the_facts_and_drops_hex() {
        let text = "boot animation dry run: nothing was sent to the panel\n\
                    frames: 3\n\
                    truncated: no (3 source frames, 3 at 10 fps)\n\
                    packets total: 5\n\
                    block 0: 1 packet(s)\n  first: 00 01\n";
        assert_eq!(
            display_summary(text),
            "frames: 3\ntruncated: no (3 source frames, 3 at 10 fps)\npackets total: 5"
        );
        let standby = "standby dry run: nothing was sent to the panel\n\
                       jpeg bytes: 10\npackets total: 2\n  0: 00 01\n  1: 02 03\n";
        assert_eq!(display_summary(standby), "jpeg bytes: 10\npackets total: 2");
        assert_eq!(display_summary("0: 00 01\n"), "");
    }
}
