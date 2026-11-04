//! The Area tab's decisions, without GTK: the form to live source mapping and preview polling.

use tt500_lcd_ipc::{DESKTOP_MAX_COMMANDS, DesktopSpec, LiveSource, Size};

/// A virtual desktop size with its scale factor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    /// Entry shown in the size row.
    pub label: &'static str,
    /// Output size in physical pixels.
    pub size: Size,
    /// Output scale factor.
    pub scale: u8,
}

/// Virtual desktop sizes, each the panel's 15:4 shape. Index 1 is the default.
pub const PRESETS: [Preset; 3] = [
    Preset {
        label: "480 × 128 (scale 1)",
        size: Size {
            width: 480,
            height: 128,
        },
        scale: 1,
    },
    Preset {
        label: "960 × 256 (scale 2)",
        size: Size {
            width: 960,
            height: 256,
        },
        scale: 2,
    },
    Preset {
        label: "1440 × 384 (scale 3)",
        size: Size {
            width: 1440,
            height: 384,
        },
        scale: 3,
    },
];

/// Index of the preset the daemon defaults to.
pub const DEFAULT_PRESET: usize = 1;

/// Index of the preset with this size and scale.
#[must_use]
pub fn preset_index(size: Size, scale: u8) -> Option<usize> {
    PRESETS
        .iter()
        .position(|preset| preset.size == size && preset.scale == scale)
}

/// Which kind of live source the form describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AreaSource {
    /// A headless desktop running the user's commands.
    Desktop,
    /// A monitor of the running session.
    Screen,
}

/// The Area tab's controls as plain data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    /// Desktop or screen.
    pub source: AreaSource,
    /// Screen output (connector); `None` is the primary one.
    pub output: Option<String>,
    /// Raw command rows.
    pub commands: Vec<String>,
    /// Chosen size, an index into [`PRESETS`].
    pub preset: Option<usize>,
}

/// The commands that count: trimmed, without blank rows.
#[must_use]
pub fn clean_commands(rows: &[String]) -> Vec<String> {
    rows.iter()
        .map(|row| row.trim())
        .filter(|row| !row.is_empty())
        .map(str::to_owned)
        .collect()
}

impl Form {
    /// The source the daemon should stream.
    ///
    /// # Errors
    /// User-facing text when the commands or size are not usable.
    pub fn live_source(&self) -> Result<LiveSource, String> {
        match self.source {
            AreaSource::Screen => Ok(LiveSource::Screen {
                output: self.output.clone(),
            }),
            AreaSource::Desktop => {
                let preset = self
                    .preset
                    .and_then(|index| PRESETS.get(index))
                    .ok_or("choose a desktop size")?;
                let spec = DesktopSpec {
                    commands: clean_commands(&self.commands),
                    size: preset.size,
                    scale: preset.scale,
                };
                spec.validate()?;
                Ok(LiveSource::Desktop(spec))
            }
        }
    }

    /// The form that describes a running source; `None` for sources the tab cannot show.
    #[must_use]
    pub fn from_live(live: &LiveSource) -> Option<Self> {
        match live {
            LiveSource::Desktop(spec) => Some(Self {
                source: AreaSource::Desktop,
                output: None,
                commands: spec.commands.clone(),
                preset: preset_index(spec.size, spec.scale),
            }),
            LiveSource::Screen { output } => Some(Self {
                source: AreaSource::Screen,
                output: output.clone(),
                commands: Vec::new(),
                preset: None,
            }),
            _ => None,
        }
    }
}

/// Whether another command row may be added.
#[must_use]
pub fn can_add_command(rows: usize) -> bool {
    rows < DESKTOP_MAX_COMMANDS
}

/// What the Apply button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyMode {
    /// A new source: fill the panel, crop nothing; the editor then follows the first frame.
    Start,
    /// The source already runs: send only the editor's fit and crop.
    Reframe,
}

/// [`ApplyMode::Reframe`] exactly when `wanted` is what is running.
#[must_use]
pub fn apply_mode(running: Option<&LiveSource>, wanted: &LiveSource) -> ApplyMode {
    if running == Some(wanted) {
        ApplyMode::Reframe
    } else {
        ApplyMode::Start
    }
}

/// What to do with a preview reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewStep {
    /// Same frame as before.
    Skip,
    /// A new frame of the same size: swap the picture only.
    Frame,
    /// First frame or a size change: reload the editor.
    Reload,
}

/// Remembers the last preview shown.
#[derive(Debug, Clone, Copy, Default)]
pub struct PreviewTracker {
    last: Option<(u64, Size)>,
}

impl PreviewTracker {
    /// Whether a reply with this `seq` and `size` was already shown.
    #[must_use]
    pub fn is_current(&self, seq: u64, size: Size) -> bool {
        self.last == Some((seq, size))
    }

    /// Records a reply and says what to do with it.
    pub fn step(&mut self, seq: u64, size: Size) -> PreviewStep {
        let step = match self.last {
            Some(last) if last == (seq, size) => PreviewStep::Skip,
            Some((_, last_size)) if last_size == size => PreviewStep::Frame,
            _ => PreviewStep::Reload,
        };
        self.last = Some((seq, size));
        step
    }

    /// Forgets the last frame, so the next one reloads the editor.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt500_lcd_ipc::{DESKTOP_DEFAULT_SCALE, DESKTOP_DEFAULT_SIZE};

    fn desktop_form(commands: &[&str]) -> Form {
        Form {
            source: AreaSource::Desktop,
            output: None,
            commands: commands.iter().map(|c| (*c).to_owned()).collect(),
            preset: Some(DEFAULT_PRESET),
        }
    }

    #[test]
    fn the_default_preset_is_the_daemon_default() {
        let preset = PRESETS[DEFAULT_PRESET];
        assert_eq!(preset.size, DESKTOP_DEFAULT_SIZE);
        assert_eq!(preset.scale, DESKTOP_DEFAULT_SCALE);
    }

    #[test]
    fn every_preset_passes_the_daemon_validation() {
        for preset in PRESETS {
            let spec = DesktopSpec {
                commands: vec!["x".into()],
                size: preset.size,
                scale: preset.scale,
            };
            assert_eq!(spec.validate(), Ok(()), "{}", preset.label);
        }
    }

    #[test]
    fn blank_rows_are_dropped_and_commands_trimmed() {
        let source = desktop_form(&["  top ", "", "   "])
            .live_source()
            .expect("valid");
        let LiveSource::Desktop(spec) = source else {
            panic!("expected a desktop, got {source:?}");
        };
        assert_eq!(spec.commands, ["top"]);
        assert_eq!(spec.size, PRESETS[DEFAULT_PRESET].size);
    }

    #[test]
    fn no_command_is_an_error() {
        assert!(desktop_form(&["", " "]).live_source().is_err());
    }

    #[test]
    fn too_many_commands_are_an_error() {
        let rows = vec!["top"; DESKTOP_MAX_COMMANDS + 1];
        assert!(desktop_form(&rows).live_source().is_err());
        assert!(can_add_command(DESKTOP_MAX_COMMANDS - 1));
        assert!(!can_add_command(DESKTOP_MAX_COMMANDS));
    }

    #[test]
    fn a_screen_needs_no_commands() {
        let form = Form {
            source: AreaSource::Screen,
            output: Some("DP-1".into()),
            commands: Vec::new(),
            preset: None,
        };
        assert_eq!(
            form.live_source(),
            Ok(LiveSource::Screen {
                output: Some("DP-1".into())
            })
        );
    }

    #[test]
    fn a_running_source_fills_the_form_and_converts_back() {
        let live = desktop_form(&["top", "htop"]).live_source().expect("valid");
        let form = Form::from_live(&live).expect("desktop");
        assert_eq!(form.preset, Some(DEFAULT_PRESET));
        assert_eq!(form.live_source(), Ok(live));
        assert!(Form::from_live(&LiveSource::Portal { reselect: false }).is_none());
    }

    #[test]
    fn a_custom_size_has_no_preset() {
        let live = LiveSource::Desktop(DesktopSpec {
            commands: vec!["top".into()],
            size: Size {
                width: 600,
                height: 160,
            },
            scale: 1,
        });
        let form = Form::from_live(&live).expect("desktop");
        assert_eq!(form.preset, None);
        assert!(form.live_source().is_err());
    }

    #[test]
    fn apply_reframes_only_the_running_source() {
        let a = desktop_form(&["top"]).live_source().expect("valid");
        let b = desktop_form(&["htop"]).live_source().expect("valid");
        assert_eq!(apply_mode(Some(&a), &a), ApplyMode::Reframe);
        assert_eq!(apply_mode(Some(&a), &b), ApplyMode::Start);
        assert_eq!(apply_mode(None, &a), ApplyMode::Start);
    }

    #[test]
    fn preview_steps() {
        let small = Size {
            width: 960,
            height: 256,
        };
        let big = Size {
            width: 1920,
            height: 512,
        };
        let mut tracker = PreviewTracker::default();
        assert_eq!(tracker.step(1, small), PreviewStep::Reload);
        assert!(tracker.is_current(1, small));
        assert_eq!(tracker.step(1, small), PreviewStep::Skip);
        assert_eq!(tracker.step(2, small), PreviewStep::Frame);
        assert_eq!(tracker.step(3, big), PreviewStep::Reload);
        tracker.reset();
        assert_eq!(tracker.step(3, big), PreviewStep::Reload);
    }
}
