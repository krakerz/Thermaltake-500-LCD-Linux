//! Profile types: named display setups with schedule and process triggers.
//!
//! These travel over the socket and are persisted as TOML by the daemon. TOML needs
//! scalars before tables, so in every struct the scalar and scalar-array fields come
//! first, then the table-valued ones.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::MAX_FPS;
use crate::message::{Crop, DesktopSpec, Fit, LiveSource, Status};

/// Most profiles the daemon keeps.
pub const PROFILE_MAX_PROFILES: usize = 64;
/// Longest profile name, in characters.
pub const PROFILE_MAX_NAME_CHARS: usize = 64;
/// Most schedule entries per profile.
pub const PROFILE_MAX_SCHEDULE: usize = 16;
/// Most trigger processes per profile.
pub const PROFILE_MAX_PROCESSES: usize = 16;
/// Most time ranges one profile may have.
pub const PROFILE_MAX_RANGES: usize = 16;
/// Name of the built-in profile; fixed, and matched case-insensitively.
pub const DEFAULT_PROFILE_NAME: &str = "Default";
/// Longest trigger process name, in characters.
pub const PROFILE_MAX_PROCESS_CHARS: usize = 64;

/// Checks a profile name and returns it trimmed.
///
/// # Errors
///
/// Returns a human-readable reason when the name is empty, too long or has control
/// characters.
pub fn validate_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    let chars = name.chars().count();
    if chars == 0 || chars > PROFILE_MAX_NAME_CHARS {
        return Err(format!(
            "a profile name needs 1 to {PROFILE_MAX_NAME_CHARS} characters"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("a profile name must not contain control characters".into());
    }
    Ok(name.to_owned())
}

/// A named display setup with its triggers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Profile {
    /// Unique (case-insensitively) display name.
    pub name: String,
    /// Marks a built-in profile; placed before the tables so TOML stays valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<Builtin>,
    /// What the profile applies.
    pub spec: ProfileSpec,
    /// When it switches on by itself.
    #[serde(default)]
    pub triggers: Triggers,
}

impl Profile {
    /// Checks the name, spec and triggers.
    ///
    /// # Errors
    ///
    /// Returns a human-readable reason when a limit is violated.
    pub fn validate(&self) -> Result<(), String> {
        validate_name(&self.name)?;
        self.spec.validate()?;
        self.triggers.validate()
    }
}

/// Identity of a built-in profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Builtin {
    /// The undeletable fallback profile.
    Default,
}

/// The settings a profile applies. `None` fields leave that setting unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProfileSpec {
    /// How the content is fitted to the panel.
    #[serde(default)]
    pub fit: Fit,
    /// Frame-rate cap, 1..=[`MAX_FPS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// Steady mode of a live source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steady: Option<bool>,
    /// Brightness in percent, 0..=100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brightness: Option<u8>,
    /// Rotation step, 0..=3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<u8>,
    /// Region of the source to show, applied before `fit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
    /// What to show; `None` keeps the current content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ProfileSource>,
}

impl ProfileSpec {
    /// Checks fps, brightness, rotation, crop and source.
    ///
    /// # Errors
    ///
    /// Returns a human-readable reason when a value is out of range.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(fps) = self.fps
            && !(1..=MAX_FPS).contains(&fps)
        {
            return Err(format!("fps must be 1 to {MAX_FPS}, got {fps}"));
        }
        if let Some(brightness) = self.brightness
            && brightness > 100
        {
            return Err(format!("brightness must be 0 to 100, got {brightness}"));
        }
        if let Some(rotation) = self.rotation
            && rotation > 3
        {
            return Err(format!("rotation must be 0 to 3, got {rotation}"));
        }
        if let Some(crop) = &self.crop
            && (crop.width == 0 || crop.height == 0)
        {
            return Err("the crop must not be empty".into());
        }
        match &self.source {
            Some(ProfileSource::Static { path }) if !path.is_absolute() => Err(format!(
                "the source path must be absolute: {}",
                path.display()
            )),
            Some(ProfileSource::Live {
                source: LiveSource::Desktop(desktop),
            }) => DesktopSpec::validate(desktop),
            _ => Ok(()),
        }
    }
}

/// Content a profile shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProfileSource {
    /// A file: still image, animation or video.
    Static {
        /// Absolute path of the file.
        path: PathBuf,
    },
    /// A live source.
    Live {
        /// What to capture.
        source: LiveSource,
    },
    /// Nothing: clears the panel (same as `Request::Clear`).
    Clear,
}

/// When a profile switches on by itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Triggers {
    /// Active while any of these processes runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<String>,
    /// Switch on at these times of day.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedule: Vec<ScheduleEntry>,
    /// Active for as long as the time falls inside one of these ranges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ranges: Vec<RangeEntry>,
}

impl Triggers {
    /// Checks the counts and the process names.
    ///
    /// # Errors
    ///
    /// Returns a human-readable reason when a limit is violated.
    pub fn validate(&self) -> Result<(), String> {
        if self.schedule.len() > PROFILE_MAX_SCHEDULE {
            return Err(format!(
                "at most {PROFILE_MAX_SCHEDULE} schedule entries per profile"
            ));
        }
        if self.ranges.len() > PROFILE_MAX_RANGES {
            return Err(format!(
                "at most {PROFILE_MAX_RANGES} time ranges per profile"
            ));
        }
        for range in &self.ranges {
            range.validate()?;
        }
        if self.processes.len() > PROFILE_MAX_PROCESSES {
            return Err(format!(
                "at most {PROFILE_MAX_PROCESSES} processes per profile"
            ));
        }
        for process in &self.processes {
            let chars = process.chars().count();
            if chars == 0 || chars > PROFILE_MAX_PROCESS_CHARS {
                return Err(format!(
                    "a process name needs 1 to {PROFILE_MAX_PROCESS_CHARS} characters"
                ));
            }
            if process.contains('/') || process.chars().any(char::is_whitespace) {
                return Err(format!(
                    "process name {process:?} must not contain '/' or whitespace"
                ));
            }
        }
        Ok(())
    }
}

/// One time of day, optionally limited to some weekdays.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScheduleEntry {
    /// Local time of day.
    pub time: TimeOfDay,
    /// Days it applies to; empty means every day.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<Weekday>,
}

/// A span of the day during which a profile is active; may wrap midnight.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RangeEntry {
    /// Local time the range starts.
    pub start: TimeOfDay,
    /// Local time the range ends (exclusive); earlier than `start` wraps past midnight.
    pub end: TimeOfDay,
    /// Days the range starts on; empty means every day.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<Weekday>,
}

impl RangeEntry {
    /// Checks that the range is not empty.
    ///
    /// # Errors
    ///
    /// Returns a human-readable reason when `start == end`.
    pub fn validate(&self) -> Result<(), String> {
        if self.start == self.end {
            return Err("a range needs different start and end".into());
        }
        Ok(())
    }
}

/// A local time of day, written `HH:MM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TimeOfDay {
    hour: u8,
    minute: u8,
}

impl TimeOfDay {
    /// Builds a time of day, `None` when out of range.
    #[must_use]
    pub fn new(hour: u8, minute: u8) -> Option<Self> {
        (hour < 24 && minute < 60).then_some(Self { hour, minute })
    }

    /// Hour, 0..=23.
    #[must_use]
    pub fn hour(self) -> u8 {
        self.hour
    }

    /// Minute, 0..=59.
    #[must_use]
    pub fn minute(self) -> u8 {
        self.minute
    }
}

impl fmt::Display for TimeOfDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

/// Error of parsing a [`TimeOfDay`] or [`Weekday`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ParseProfileError(String);

impl FromStr for TimeOfDay {
    type Err = ParseProfileError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || ParseProfileError(format!("time must be HH:MM (00:00 to 23:59), got {s:?}"));
        let (hour, minute) = s.split_once(':').ok_or_else(bad)?;
        if hour.len() != 2 || minute.len() != 2 {
            return Err(bad());
        }
        let hour: u8 = hour.parse().map_err(|_| bad())?;
        let minute: u8 = minute.parse().map_err(|_| bad())?;
        Self::new(hour, minute).ok_or_else(bad)
    }
}

impl TryFrom<String> for TimeOfDay {
    type Error = ParseProfileError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<TimeOfDay> for String {
    fn from(value: TimeOfDay) -> Self {
        value.to_string()
    }
}

/// A day of the week.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Weekday {
    /// Monday.
    Mon,
    /// Tuesday.
    Tue,
    /// Wednesday.
    Wed,
    /// Thursday.
    Thu,
    /// Friday.
    Fri,
    /// Saturday.
    Sat,
    /// Sunday.
    Sun,
}

impl Weekday {
    /// All days, Monday first.
    pub const ALL: [Weekday; 7] = [
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
        Weekday::Thu,
        Weekday::Fri,
        Weekday::Sat,
        Weekday::Sun,
    ];

    /// Position in the week, Monday = 0.
    #[must_use]
    pub fn index(self) -> usize {
        self as usize
    }

    fn name(self) -> &'static str {
        match self {
            Self::Mon => "mon",
            Self::Tue => "tue",
            Self::Wed => "wed",
            Self::Thu => "thu",
            Self::Fri => "fri",
            Self::Sat => "sat",
            Self::Sun => "sun",
        }
    }
}

impl fmt::Display for Weekday {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Weekday {
    type Err = ParseProfileError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|day| day.name() == lower)
            .ok_or_else(|| {
                ParseProfileError(format!(
                    "day must be mon, tue, wed, thu, fri, sat or sun, got {s:?}"
                ))
            })
    }
}

/// Profile state carried in [`Status`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileStatus {
    /// Bumps on any change of the profile set; clients refetch the list.
    pub revision: u64,
    /// Bumps on every applied activation; editors re-adopt the daemon's content.
    pub activation: u64,
    /// The profile the screen currently represents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<ActiveProfile>,
    /// Why the last activation failed, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// The active profile and why it is active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveProfile {
    /// Profile name.
    pub name: String,
    /// What switched it on.
    pub reason: ProfileReason,
    /// The content was changed by hand after the profile was applied.
    #[serde(default, skip_serializing_if = "is_false")]
    pub modified: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde signature")]
fn is_false(value: &bool) -> bool {
    !*value
}

/// What switched a profile on.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProfileReason {
    /// A client asked for it.
    Manual,
    /// A schedule entry fired.
    Schedule,
    /// A trigger process is running.
    Process {
        /// The process name that matched.
        process: String,
    },
    /// The time is inside a range of the profile.
    Range {
        /// Range start.
        start: TimeOfDay,
        /// Range end.
        end: TimeOfDay,
    },
    /// No trigger applies, so the Default profile is shown.
    Fallback,
    /// A reason added by a newer daemon.
    #[serde(other)]
    Unknown,
}

impl fmt::Display for ProfileReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manual => f.write_str("manual"),
            Self::Schedule => f.write_str("schedule"),
            Self::Process { process } => write!(f, "process {process}"),
            Self::Range { start, end } => write!(f, "range {start}-{end}"),
            Self::Fallback => f.write_str("fallback"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

/// Reply to `Request::ProfileList`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilesReply {
    /// All profiles, in list (switcher and tie-break) order.
    pub profiles: Vec<Profile>,
    /// The daemon status.
    pub status: Status,
}

/// Parameters of `Request::ProfileSave`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileSaveParams {
    /// Profile name; an existing one gets its spec replaced and keeps its triggers.
    pub name: String,
    /// Settings to store; absent snapshots the current content, brightness and rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<ProfileSpec>,
}

/// Parameters naming one profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileNameParams {
    /// Profile name, matched case-insensitively.
    pub name: String,
}

/// Parameters of `Request::ProfileRename`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileRenameParams {
    /// Current name.
    pub from: String,
    /// New name.
    pub to: String,
}

/// Parameters of `Request::ProfileSetTriggers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileTriggersParams {
    /// Profile name.
    pub name: String,
    /// Replaces the profile's triggers as a whole.
    pub triggers: Triggers,
}

/// Parameters of `Request::ProfileReorder`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileReorderParams {
    /// The complete new order: every existing profile exactly once.
    pub names: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Size;

    fn full_profile() -> Profile {
        Profile {
            name: "Night".into(),
            builtin: Some(Builtin::Default),
            spec: ProfileSpec {
                fit: Fit::Cover,
                fps: Some(10),
                steady: Some(true),
                brightness: Some(20),
                rotation: Some(1),
                crop: Some(Crop {
                    x: 1,
                    y: 2,
                    width: 300,
                    height: 400,
                }),
                source: Some(ProfileSource::Live {
                    source: LiveSource::Desktop(DesktopSpec {
                        commands: vec!["foot".into()],
                        size: Size {
                            width: 800,
                            height: 480,
                        },
                        scale: 1,
                    }),
                }),
            },
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
                ranges: vec![RangeEntry {
                    start: TimeOfDay::new(22, 0).unwrap(),
                    end: TimeOfDay::new(6, 0).unwrap(),
                    days: vec![Weekday::Fri],
                }],
            },
        }
    }

    #[test]
    fn range_validation() {
        let mut triggers = Triggers {
            ranges: vec![RangeEntry {
                start: TimeOfDay::new(8, 0).unwrap(),
                end: TimeOfDay::new(8, 0).unwrap(),
                days: Vec::new(),
            }],
            ..Triggers::default()
        };
        assert!(triggers.validate().is_err());
        triggers.ranges[0].end = TimeOfDay::new(9, 0).unwrap();
        assert!(triggers.validate().is_ok());
        triggers.ranges = vec![triggers.ranges[0].clone(); PROFILE_MAX_RANGES + 1];
        assert!(triggers.validate().is_err());
    }

    #[test]
    fn toml_round_trip() {
        let profile = full_profile();
        let text = toml::to_string(&profile).expect("serialises");
        let back: Profile = toml::from_str(&text).expect("parses");
        assert_eq!(back, profile, "{text}");
    }

    #[test]
    fn toml_round_trip_static_and_empty() {
        let profile = Profile {
            name: "Dim".into(),
            builtin: None,
            spec: ProfileSpec {
                brightness: Some(5),
                ..ProfileSpec::default()
            },
            triggers: Triggers::default(),
        };
        let text = toml::to_string(&profile).expect("serialises");
        assert_eq!(toml::from_str::<Profile>(&text).unwrap(), profile, "{text}");
        let profile = Profile {
            name: "Pic".into(),
            builtin: None,
            spec: ProfileSpec {
                source: Some(ProfileSource::Static {
                    path: "/tmp/a.png".into(),
                }),
                ..ProfileSpec::default()
            },
            triggers: Triggers::default(),
        };
        let text = toml::to_string(&profile).expect("serialises");
        assert_eq!(toml::from_str::<Profile>(&text).unwrap(), profile, "{text}");
    }

    #[test]
    fn time_of_day_parses_and_rejects() {
        assert_eq!("07:05".parse::<TimeOfDay>().unwrap().to_string(), "07:05");
        assert_eq!(
            "23:59".parse::<TimeOfDay>().unwrap(),
            TimeOfDay::new(23, 59).unwrap()
        );
        for bad in ["24:00", "7:5", "12:60", "12", "ab:cd", "", "12:3"] {
            assert!(bad.parse::<TimeOfDay>().is_err(), "{bad:?}");
        }
        assert!(TimeOfDay::new(7, 30) < TimeOfDay::new(22, 0));
    }

    #[test]
    fn weekday_parses_and_rejects() {
        assert_eq!("Mon".parse::<Weekday>().unwrap(), Weekday::Mon);
        assert_eq!("sun".parse::<Weekday>().unwrap().index(), 6);
        assert_eq!(Weekday::Wed.to_string(), "wed");
        assert!("funday".parse::<Weekday>().is_err());
        assert_eq!(serde_json::to_string(&Weekday::Thu).unwrap(), r#""thu""#);
    }

    #[test]
    fn name_validation() {
        assert_eq!(validate_name("  Night ").unwrap(), "Night");
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"x".repeat(65)).is_err());
        assert!(validate_name("a\nb").is_err());
        assert!(validate_name(&"x".repeat(64)).is_ok());
    }

    #[test]
    fn spec_validation() {
        assert!(full_profile().validate().is_ok());
        let bad = [
            ProfileSpec {
                fps: Some(0),
                ..ProfileSpec::default()
            },
            ProfileSpec {
                fps: Some(MAX_FPS + 1),
                ..ProfileSpec::default()
            },
            ProfileSpec {
                brightness: Some(101),
                ..ProfileSpec::default()
            },
            ProfileSpec {
                rotation: Some(4),
                ..ProfileSpec::default()
            },
            ProfileSpec {
                crop: Some(Crop {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 5,
                }),
                ..ProfileSpec::default()
            },
            ProfileSpec {
                source: Some(ProfileSource::Static {
                    path: "relative.png".into(),
                }),
                ..ProfileSpec::default()
            },
        ];
        for spec in &bad {
            assert!(spec.validate().is_err(), "{spec:?}");
        }
    }

    #[test]
    fn trigger_validation() {
        let mut triggers = Triggers::default();
        assert!(triggers.validate().is_ok());
        triggers.processes = vec!["a b".into()];
        assert!(triggers.validate().is_err());
        triggers.processes = vec!["a/b".into()];
        assert!(triggers.validate().is_err());
        triggers.processes = vec![String::new()];
        assert!(triggers.validate().is_err());
        triggers.processes = vec!["p".into(); PROFILE_MAX_PROCESSES + 1];
        assert!(triggers.validate().is_err());
        triggers.processes.clear();
        triggers.schedule = vec![
            ScheduleEntry {
                time: TimeOfDay::new(1, 0).unwrap(),
                days: Vec::new(),
            };
            PROFILE_MAX_SCHEDULE + 1
        ];
        assert!(triggers.validate().is_err());
    }

    #[test]
    fn reason_display() {
        assert_eq!(ProfileReason::Manual.to_string(), "manual");
        assert_eq!(ProfileReason::Schedule.to_string(), "schedule");
        let reason = ProfileReason::Process {
            process: "game.exe".into(),
        };
        assert_eq!(reason.to_string(), "process game.exe");
    }
}
