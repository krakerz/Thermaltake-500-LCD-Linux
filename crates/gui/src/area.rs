//! The Area tab's decisions, without GTK: the form to live source mapping and preview polling.

use tt500_lcd_ipc::{
    DESKTOP_DEFAULT_SCALE, DESKTOP_DEFAULT_SIZE, DESKTOP_MAX_COMMANDS, DesktopSpec, LiveSource,
    Size,
};

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

/// Custom size shown the first time, taller than the panel so it pans at zoom 1.
pub const DEFAULT_CUSTOM: Geometry = Geometry {
    size: Size {
        width: 960,
        height: 512,
    },
    scale: 1,
};

/// Label of the size row's last entry, after the [`PRESETS`].
pub const CUSTOM_LABEL: &str = "Custom…";

/// A desktop size with its scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// Output size in physical pixels.
    pub size: Size,
    /// Output scale factor.
    pub scale: u8,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            size: DESKTOP_DEFAULT_SIZE,
            scale: DESKTOP_DEFAULT_SCALE,
        }
    }
}

/// The size row's choice: a preset or a custom geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeChoice {
    /// An index into [`PRESETS`].
    Preset(usize),
    /// A user-typed size.
    Custom(Geometry),
}

impl SizeChoice {
    /// The matching preset, else a custom choice with these values.
    #[must_use]
    pub fn from_geometry(geometry: Geometry) -> Self {
        preset_index(geometry.size, geometry.scale).map_or(Self::Custom(geometry), Self::Preset)
    }

    /// Position in the size row: presets first, then "Custom…".
    #[must_use]
    pub fn row_index(self) -> usize {
        match self {
            Self::Preset(index) => index,
            Self::Custom(_) => PRESETS.len(),
        }
    }

    /// The choice at a size row position, `custom` standing for the last entry.
    #[must_use]
    pub fn from_row_index(index: usize, custom: Geometry) -> Self {
        if index < PRESETS.len() {
            Self::Preset(index)
        } else {
            Self::Custom(custom)
        }
    }

    /// Size and scale of the choice.
    #[must_use]
    pub fn geometry(self) -> Geometry {
        match self {
            Self::Preset(index) => {
                let preset = PRESETS.get(index).unwrap_or(&PRESETS[DEFAULT_PRESET]);
                Geometry {
                    size: preset.size,
                    scale: preset.scale,
                }
            }
            Self::Custom(geometry) => geometry,
        }
    }
}

/// Whether a source of `size` has the panel's 15:4 shape, so that at zoom 1 it fits whole.
#[must_use]
pub fn matches_panel_shape(size: Size) -> bool {
    // 480:128 reduced is 15:4.
    u64::from(size.width) * 4 == u64::from(size.height) * 15
}

/// Which kind of live source the form describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AreaSource {
    /// A headless desktop running the user's commands.
    Desktop,
    /// A monitor of the running session.
    Screen,
    /// A window or screen chosen in the desktop portal's picker.
    Portal,
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
    /// Chosen desktop size.
    pub size: SizeChoice,
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
            AreaSource::Portal => Ok(LiveSource::Portal { reselect: false }),
            AreaSource::Desktop => {
                let Geometry { size, scale } = self.size.geometry();
                let spec = DesktopSpec {
                    commands: clean_commands(&self.commands),
                    size,
                    scale,
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
                size: SizeChoice::from_geometry(Geometry {
                    size: spec.size,
                    scale: spec.scale,
                }),
            }),
            LiveSource::Screen { output } => Some(Self {
                source: AreaSource::Screen,
                output: output.clone(),
                commands: Vec::new(),
                size: SizeChoice::Preset(DEFAULT_PRESET),
            }),
            LiveSource::Portal { .. } => Some(Self {
                source: AreaSource::Portal,
                output: None,
                commands: Vec::new(),
                size: SizeChoice::Preset(DEFAULT_PRESET),
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
            size: SizeChoice::Preset(DEFAULT_PRESET),
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
            size: SizeChoice::Preset(DEFAULT_PRESET),
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
        assert_eq!(form.size, SizeChoice::Preset(DEFAULT_PRESET));
        assert_eq!(form.live_source(), Ok(live));
        let portal = Form::from_live(&LiveSource::Portal { reselect: true }).expect("portal");
        assert_eq!(portal.source, AreaSource::Portal);
        assert_eq!(
            portal.live_source(),
            Ok(LiveSource::Portal { reselect: false })
        );
    }

    #[test]
    fn a_custom_size_round_trips_as_custom() {
        let geometry = Geometry {
            size: Size {
                width: 960,
                height: 512,
            },
            scale: 1,
        };
        let live = LiveSource::Desktop(DesktopSpec {
            commands: vec!["top".into()],
            size: geometry.size,
            scale: geometry.scale,
        });
        let form = Form::from_live(&live).expect("desktop");
        assert_eq!(form.size, SizeChoice::Custom(geometry));
        assert_eq!(form.size.row_index(), PRESETS.len());
        assert_eq!(form.live_source(), Ok(live));
    }

    #[test]
    fn an_out_of_range_custom_size_is_an_error() {
        let mut form = desktop_form(&["top"]);
        form.size = SizeChoice::Custom(Geometry {
            size: Size {
                width: 100,
                height: 512,
            },
            scale: 1,
        });
        assert!(form.live_source().is_err());
    }

    #[test]
    fn size_choice_maps_row_positions() {
        for index in 0..PRESETS.len() {
            let choice = SizeChoice::from_row_index(index, DEFAULT_CUSTOM);
            assert_eq!(choice, SizeChoice::Preset(index));
            assert_eq!(choice.row_index(), index);
        }
        assert_eq!(
            SizeChoice::from_row_index(PRESETS.len(), DEFAULT_CUSTOM),
            SizeChoice::Custom(DEFAULT_CUSTOM)
        );
        assert_eq!(
            SizeChoice::from_geometry(Geometry {
                size: PRESETS[0].size,
                scale: PRESETS[0].scale,
            }),
            SizeChoice::Preset(0)
        );
    }

    #[test]
    fn the_default_custom_size_is_valid_and_taller_than_the_panel() {
        let spec = DesktopSpec {
            commands: vec!["x".into()],
            size: DEFAULT_CUSTOM.size,
            scale: DEFAULT_CUSTOM.scale,
        };
        assert_eq!(spec.validate(), Ok(()));
        assert!(!matches_panel_shape(DEFAULT_CUSTOM.size));
    }

    #[test]
    fn panel_shape_detection() {
        for preset in PRESETS {
            assert!(matches_panel_shape(preset.size), "{}", preset.label);
        }
        assert!(!matches_panel_shape(Size {
            width: 2560,
            height: 1440,
        }));
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
