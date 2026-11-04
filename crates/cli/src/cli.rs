//! Command-line definition.

use std::path::PathBuf;
use std::str::FromStr;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum, value_parser};
use tt500_lcd_ipc::{Crop, Fit, MAX_FPS, Size, TimeOfDay, Weekday};

/// Control the Thermaltake Tower 500 LCD panel daemon.
#[derive(Debug, Parser)]
#[command(name = "tt500-lcd", version, about, disable_version_flag = true)]
pub struct Cli {
    /// Print version
    #[arg(short = 'v', long, action = ArgAction::Version)]
    version: (),

    #[command(subcommand)]
    pub command: Command,
}

/// The `--steady` / `--no-steady` pair of the live commands.
#[derive(Debug, Clone, Copy, Args)]
pub struct SteadyArgs {
    /// Skip half-drawn frames: show a changed frame once the next capture matches it
    /// (default for a virtual desktop; adds a little delay)
    #[arg(long, conflicts_with = "no_steady")]
    steady: bool,
    /// Show every captured frame at once (default for a screen)
    #[arg(long)]
    no_steady: bool,
}

impl SteadyArgs {
    /// The explicit choice, or `None` to let the daemon decide per source.
    #[must_use]
    pub fn choice(self) -> Option<bool> {
        match (self.steady, self.no_steady) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        }
    }
}

/// What to ask the daemon to do.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show an image, animation or video on the panel (videos need ffmpeg)
    Show {
        /// Image, GIF, APNG, WebP or video file
        file: PathBuf,
        /// How to fit the image onto the 480x128 panel
        #[arg(long, value_parser = Fit::from_str, default_value_t = Fit::Cover)]
        fit: Fit,
        /// Show only this region of the source, in source pixels, before fitting
        #[arg(long, value_parser = Crop::from_str, value_name = "X,Y,W,H")]
        crop: Option<Crop>,
        /// Highest frame rate for videos and animations, 1-30 (default: 15 for videos, the animation's own timing for GIFs)
        #[arg(long, value_parser = value_parser!(u32).range(1..=i64::from(MAX_FPS)))]
        fps: Option<u32>,
    },
    /// Mirror a screen on the panel (wlroots compositors such as Hyprland and sway)
    ///
    /// The daemon captures the output and streams it until you run `clear` or show something else.
    ShowScreen {
        /// Output to mirror, e.g. DP-1 (default: the compositor's first output)
        #[arg(long)]
        output: Option<String>,
        /// How to fit the screen onto the 480x128 panel
        #[arg(long, value_parser = Fit::from_str, default_value_t = Fit::Cover)]
        fit: Fit,
        /// Show only this region of the screen, in screen pixels, before fitting
        #[arg(long, value_parser = Crop::from_str, value_name = "X,Y,W,H")]
        crop: Option<Crop>,
        /// Capture frame rate, 1-30
        #[arg(long, value_parser = value_parser!(u32).range(1..=i64::from(MAX_FPS)))]
        fps: Option<u32>,
        #[command(flatten)]
        steady: SteadyArgs,
    },
    /// Run commands on a headless virtual desktop and show it on the panel
    ///
    /// The daemon starts a headless compositor and tiles one window per command. Give one
    /// command after `--`, or several with --run. Example:
    /// tt500-lcd show-desktop -- alacritty -e ~/Scripts/sysmon.sh
    ShowDesktop {
        /// Virtual output size in pixels, WIDTHxHEIGHT
        #[arg(long, value_parser = parse_size, value_name = "WxH", default_value = "960x256")]
        size: Size,
        /// Output scale factor, 1-3 (the size must be divisible by it)
        #[arg(long, default_value_t = 2, value_parser = value_parser!(u8).range(1..=3))]
        scale: u8,
        /// A shell command to run on the desktop; repeat for several
        #[arg(long = "run", value_name = "CMD")]
        run: Vec<String>,
        /// A single command and its arguments, after `--`
        #[arg(last = true)]
        argv: Vec<String>,
        /// How to fit the desktop onto the 480x128 panel
        #[arg(long, value_parser = Fit::from_str, default_value_t = Fit::Cover)]
        fit: Fit,
        /// Show only this region of the desktop, in desktop pixels, before fitting
        #[arg(long, value_parser = Crop::from_str, value_name = "X,Y,W,H")]
        crop: Option<Crop>,
        /// Capture frame rate, 1-30
        #[arg(long, value_parser = value_parser!(u32).range(1..=i64::from(MAX_FPS)))]
        fps: Option<u32>,
        #[command(flatten)]
        steady: SteadyArgs,
    },
    /// Save the latest full frame of the running live source as a JPEG
    Preview {
        /// File to write
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Upload a still image as the panel's standby image (WRITES TO THE PANEL'S FLASH MEMORY)
    ///
    /// The panel shows this image whenever nothing is streaming. Animations and videos are
    /// refused. Use --dry-run first: it sends nothing and writes the exact packets to files.
    Standby {
        /// Still image (PNG, JPEG, BMP, WebP or single-frame GIF)
        file: PathBuf,
        /// How to fit the image onto the 480x128 panel
        #[arg(long, value_parser = Fit::from_str, default_value_t = Fit::Cover)]
        fit: Fit,
        /// Use only this region of the source, in source pixels, before fitting
        #[arg(long, value_parser = Crop::from_str, value_name = "X,Y,W,H")]
        crop: Option<Crop>,
        /// Render and build the packets, write them to the daemon's state directory, send nothing
        #[arg(long)]
        dry_run: bool,
        /// Do not ask for confirmation before writing to the panel
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Upload an animation as the panel's boot animation (WRITES TO THE PANEL'S FLASH MEMORY)
    ///
    /// The panel plays this at power-on. A GIF whose frames all last 40 to 1000 ms keeps
    /// its timing; any other animation is resampled to its most common frame delay (40 to
    /// 1000 ms). It is cut to 75 frames. Stills and videos are refused. Use --dry-run first:
    /// it sends nothing and writes the exact packets to files.
    Boot {
        /// Animation (GIF, animated PNG or WebP)
        file: PathBuf,
        /// How to fit the frames onto the 480x128 panel
        #[arg(long, value_parser = Fit::from_str, default_value_t = Fit::Cover)]
        fit: Fit,
        /// Use only this region of the source, in source pixels, before fitting
        #[arg(long, value_parser = Crop::from_str, value_name = "X,Y,W,H")]
        crop: Option<Crop>,
        /// Render and build the packets, write them to the daemon's state directory, send nothing
        #[arg(long)]
        dry_run: bool,
        /// Do not ask for confirmation before writing to the panel
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Save and switch display profiles
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
    /// Stop streaming; the panel falls back to its stored image (brightness is kept)
    Clear,
    /// Set the panel brightness in percent (0 turns the panel off)
    Brightness {
        /// Brightness, 0-100
        #[arg(value_parser = value_parser!(u8).range(0..=100))]
        value: u8,
    },
    /// Rotate the picture on the panel (stored; applies to everything shown and uploaded)
    Rotation {
        /// 0 = normal, 1 = portrait (90° clockwise), 2 = 180°, 3 = portrait (90° counter-clockwise)
        #[arg(value_parser = value_parser!(u8).range(0..=3))]
        value: u8,
    },
    /// Print the status again every time it changes (Ctrl+C to stop)
    Watch {
        /// Print each status as one JSON line
        #[arg(long)]
        json: bool,
    },
    /// Print the daemon, panel and content status
    Status {
        /// Print the status as JSON
        #[arg(long)]
        json: bool,
    },
    /// Install or remove the daemon as a systemd user service
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Start the GUI at login (XDG autostart)
    Autostart {
        /// on, off or status
        #[arg(value_enum)]
        action: AutostartAction,
    },
}

/// What to do with the daemon's systemd user service.
/// What to do with display profiles.
#[derive(Debug, Subcommand)]
pub enum ProfileAction {
    /// List the profiles, their triggers and the active one
    List {
        /// Print the profiles as JSON
        #[arg(long)]
        json: bool,
    },
    /// Save what the panel shows now (source, framing, fps, brightness, rotation) as profile
    /// NAME; an existing profile keeps its triggers
    Save {
        /// Profile name
        name: String,
    },
    /// Switch the panel to a profile
    Activate {
        /// Profile name
        name: String,
    },
    /// Delete a profile
    Delete {
        /// Profile name
        name: String,
    },
    /// Rename a profile
    Rename {
        /// Current name
        from: String,
        /// New name
        to: String,
    },
    /// Move a profile in the list (the order breaks ties between triggers)
    Move {
        /// Profile name
        name: String,
        /// New 1-based position, `up` or `down`
        #[arg(value_parser = MoveTarget::from_str)]
        position: MoveTarget,
    },
    /// Reset the built-in Default profile to factory settings (clear the panel, brightness 100,
    /// rotation 0, no triggers)
    Reset {
        /// Profile name (only Default can be reset)
        name: String,
    },
    /// Edit when a profile switches on by itself
    Trigger {
        #[command(subcommand)]
        action: TriggerAction,
    },
}

/// Where `profile move` puts a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveTarget {
    /// A 1-based list position.
    Position(usize),
    /// One place towards the front.
    Up,
    /// One place towards the back.
    Down,
}

impl FromStr for MoveTarget {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "up" => Ok(Self::Up),
            "down" => Ok(Self::Down),
            other => match other.parse::<usize>() {
                Ok(n) if n >= 1 => Ok(Self::Position(n)),
                _ => Err(format!(
                    "position must be a number from 1, `up` or `down`, got {s:?}"
                )),
            },
        }
    }
}

/// Edits of a profile's triggers.
#[derive(Debug, Subcommand)]
pub enum TriggerAction {
    /// Switch to the profile every day (or on some days) at a time of day
    AddTime {
        /// Profile name
        name: String,
        /// Time of day, HH:MM (local time)
        #[arg(value_parser = TimeOfDay::from_str)]
        time: TimeOfDay,
        /// Days: mon,tue,wed,thu,fri,sat,sun or weekdays, weekend, daily (default: every day)
        #[arg(long, value_delimiter = ',', value_parser = parse_day_item)]
        days: Vec<DayItem>,
    },
    /// Switch to the profile while the time of day is inside a range (may wrap past midnight)
    AddRange {
        /// Profile name
        name: String,
        /// Range start, HH:MM (local time)
        #[arg(value_parser = TimeOfDay::from_str)]
        start: TimeOfDay,
        /// Range end, HH:MM; earlier than the start means the next day
        #[arg(value_parser = TimeOfDay::from_str)]
        end: TimeOfDay,
        /// Days the range starts on: mon,tue,... or weekdays, weekend, daily (default: every day)
        #[arg(long, value_delimiter = ',', value_parser = parse_day_item)]
        days: Vec<DayItem>,
    },
    /// Switch to the profile while a process runs (its name or executable name)
    AddProcess {
        /// Profile name
        name: String,
        /// Process name, e.g. game.exe
        process: String,
    },
    /// Remove a time trigger
    RemoveTime {
        /// Profile name
        name: String,
        /// Time of day, HH:MM
        #[arg(value_parser = TimeOfDay::from_str)]
        time: TimeOfDay,
    },
    /// Remove a time range
    RemoveRange {
        /// Profile name
        name: String,
        /// Range start, HH:MM
        #[arg(value_parser = TimeOfDay::from_str)]
        start: TimeOfDay,
        /// Range end, HH:MM
        #[arg(value_parser = TimeOfDay::from_str)]
        end: TimeOfDay,
    },
    /// Remove a process trigger
    RemoveProcess {
        /// Profile name
        name: String,
        /// Process name
        process: String,
    },
    /// Remove every trigger of the profile
    Clear {
        /// Profile name
        name: String,
    },
}

/// One `--days` item: a day or a shortcut standing for several.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayItem {
    /// A single day.
    Day(Weekday),
    /// Monday to Friday.
    Weekdays,
    /// Saturday and Sunday.
    Weekend,
    /// Every day.
    Daily,
}

fn parse_day_item(s: &str) -> Result<DayItem, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "weekdays" => Ok(DayItem::Weekdays),
        "weekend" => Ok(DayItem::Weekend),
        "daily" => Ok(DayItem::Daily),
        _ => s
            .parse::<Weekday>()
            .map(DayItem::Day)
            .map_err(|e| format!("{e}; or use weekdays, weekend or daily")),
    }
}

/// Expands `--days` items into a sorted list without duplicates; empty means every day.
pub fn expand_days(items: &[DayItem]) -> Vec<Weekday> {
    let mut days: Vec<Weekday> = Vec::new();
    for item in items {
        match item {
            DayItem::Day(day) => days.push(*day),
            DayItem::Weekdays => days.extend(&Weekday::ALL[..5]),
            DayItem::Weekend => days.extend(&Weekday::ALL[5..]),
            DayItem::Daily => days.extend(Weekday::ALL),
        }
    }
    days.sort();
    days.dedup();
    if days.len() == Weekday::ALL.len() {
        days.clear();
    }
    days
}

#[derive(Debug, Clone, Copy, Subcommand)]
pub enum ServiceAction {
    /// Write the unit, then enable and start it
    Install,
    /// Disable and stop the unit, then remove it
    Uninstall,
    /// Show whether the unit is installed, enabled and running
    Status,
}

/// What to do with the GUI's autostart entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AutostartAction {
    /// Start the GUI at login
    On,
    /// Do not start the GUI at login
    Off,
    /// Show whether the GUI starts at login
    Status,
}

/// Parses `WIDTHxHEIGHT` (e.g. `960x256`).
fn parse_size(text: &str) -> Result<Size, String> {
    let bad = || format!("invalid size {text:?}, expected WIDTHxHEIGHT such as 960x256");
    let (width, height) = text.split_once(['x', 'X']).ok_or_else(bad)?;
    Ok(Size {
        width: width.parse().map_err(|_| bad())?,
        height: height.parse().map_err(|_| bad())?,
    })
}

/// Quotes `arg` for `sh -c`: words made of safe characters stay as they are.
fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c));
    if safe {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// Joins the arguments after `--` into one shell command.
fn join_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The desktop's commands: every `--run` command, then the `--` command if any.
pub fn desktop_commands(run: Vec<String>, argv: &[String]) -> Vec<String> {
    let mut commands = run;
    if !argv.is_empty() {
        commands.push(join_argv(argv));
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn definition_is_valid() {
        Cli::command().debug_assert();
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("tt500-lcd").chain(args.iter().copied()))
    }

    #[test]
    fn brightness_range_is_enforced() {
        assert!(parse(&["brightness", "100"]).is_ok());
        assert!(parse(&["brightness", "0"]).is_ok());
        let err = parse(&["brightness", "101"]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(parse(&["brightness", "-1"]).is_err());
    }

    #[test]
    fn rotation_range_is_enforced() {
        for step in ["0", "1", "2", "3"] {
            assert!(parse(&["rotation", step]).is_ok(), "{step}");
        }
        let err = parse(&["rotation", "4"]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(parse(&["rotation", "-1"]).is_err());
        assert!(parse(&["rotation"]).is_err());
    }

    #[test]
    fn show_crop_parses() {
        let cli = parse(&["show", "a.mp4", "--crop", "0,40,480,128"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Show {
                crop: Some(Crop {
                    x: 0,
                    y: 40,
                    width: 480,
                    height: 128
                }),
                ..
            }
        ));
        assert!(matches!(
            parse(&["show", "a.png"]).unwrap().command,
            Command::Show { crop: None, .. }
        ));
        assert_eq!(
            parse(&["show", "a.png", "--crop", "1,2,3"])
                .unwrap_err()
                .exit_code(),
            2
        );
    }

    #[test]
    fn show_fit_defaults_to_cover() {
        let cli = parse(&["show", "a.gif"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Show {
                fit: Fit::Cover,
                ..
            }
        ));
        let cli = parse(&["show", "a.gif", "--fit", "contain"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Show {
                fit: Fit::Contain,
                ..
            }
        ));
        assert!(parse(&["show", "a.gif", "--fit", "zoom"]).is_err());
    }

    #[test]
    fn show_fps_is_optional_and_bounded() {
        assert!(matches!(
            parse(&["show", "a.mp4"]).unwrap().command,
            Command::Show { fps: None, .. }
        ));
        assert!(matches!(
            parse(&["show", "a.mp4", "--fps", "30"]).unwrap().command,
            Command::Show { fps: Some(30), .. }
        ));
        for bad in ["31", "0"] {
            let err = parse(&["show", "a.mp4", "--fps", bad]).unwrap_err();
            assert_eq!(err.exit_code(), 2);
        }
    }

    #[test]
    fn standby_options_parse() {
        let cli = parse(&["standby", "a.png"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Standby {
                fit: Fit::Cover,
                crop: None,
                dry_run: false,
                yes: false,
                ..
            }
        ));
        let cli = parse(&[
            "standby",
            "a.png",
            "--fit",
            "contain",
            "--crop",
            "1,2,3,4",
            "--dry-run",
            "--yes",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Standby {
                fit: Fit::Contain,
                crop: Some(_),
                dry_run: true,
                yes: true,
                ..
            }
        ));
    }

    #[test]
    fn boot_options_parse() {
        let cli = parse(&["boot", "a.gif"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Boot {
                fit: Fit::Cover,
                crop: None,
                dry_run: false,
                yes: false,
                ..
            }
        ));
        let cli = parse(&["boot", "a.gif", "--fit", "stretch", "--dry-run", "-y"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Boot {
                fit: Fit::Stretch,
                dry_run: true,
                yes: true,
                ..
            }
        ));
    }

    #[test]
    fn watch_parses() {
        assert!(matches!(
            parse(&["watch"]).unwrap().command,
            Command::Watch { json: false }
        ));
        assert!(matches!(
            parse(&["watch", "--json"]).unwrap().command,
            Command::Watch { json: true }
        ));
    }

    #[test]
    fn unknown_option_exits_2() {
        assert_eq!(parse(&["--bogus", "clear"]).unwrap_err().exit_code(), 2);
        assert_eq!(parse(&["clear", "extra"]).unwrap_err().exit_code(), 2);
    }

    #[test]
    fn version_flags_exit_0() {
        for flag in ["-v", "--version"] {
            let err = parse(&[flag]).unwrap_err();
            assert_eq!(err.exit_code(), 0);
            assert!(err.to_string().starts_with("tt500-lcd "));
        }
    }

    #[test]
    fn size_parses() {
        assert_eq!(
            parse_size("960x256").unwrap(),
            Size {
                width: 960,
                height: 256
            }
        );
        for bad in ["960", "x256", "960x", "axb", "-1x2", "960x256x1"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn argv_is_shell_quoted() {
        let argv = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            join_argv(&argv(&["alacritty", "-e", "/a/b.sh"])),
            "alacritty -e /a/b.sh"
        );
        assert_eq!(
            join_argv(&argv(&["echo", "it's a $x", ""])),
            "echo 'it'\\''s a $x' ''"
        );
        assert_eq!(
            desktop_commands(vec!["top".into()], &argv(&["a b"])),
            vec!["top".to_owned(), "'a b'".to_owned()]
        );
    }

    #[test]
    fn show_desktop_parses() {
        let cli = parse(&["show-desktop", "--", "alacritty", "-e", "top"]).unwrap();
        let Command::ShowDesktop {
            size,
            scale,
            run,
            argv,
            ..
        } = cli.command
        else {
            panic!("not show-desktop");
        };
        assert_eq!((size.width, size.height, scale), (960, 256, 2));
        assert!(run.is_empty());
        assert_eq!(argv, ["alacritty", "-e", "top"]);
        let cli = parse(&[
            "show-desktop",
            "--size",
            "480x128",
            "--scale",
            "1",
            "--run",
            "a",
            "--run",
            "b",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::ShowDesktop { scale: 1, ref run, .. } if run.len() == 2
        ));
        assert_eq!(
            parse(&["show-desktop", "--scale", "4"])
                .unwrap_err()
                .exit_code(),
            2
        );
    }

    #[test]
    fn show_screen_and_preview_parse() {
        assert!(matches!(
            parse(&["show-screen"]).unwrap().command,
            Command::ShowScreen {
                output: None,
                fps: None,
                ..
            }
        ));
        assert!(matches!(
            parse(&["show-screen", "--output", "DP-1", "--fps", "30"])
                .unwrap()
                .command,
            Command::ShowScreen {
                output: Some(_),
                fps: Some(30),
                ..
            }
        ));
        let steady = |args: &[&str]| match parse(args).unwrap().command {
            Command::ShowScreen { steady, .. } => steady.choice(),
            _ => panic!("not show-screen"),
        };
        assert_eq!(steady(&["show-screen"]), None);
        assert_eq!(steady(&["show-screen", "--steady"]), Some(true));
        assert_eq!(steady(&["show-screen", "--no-steady"]), Some(false));
        assert_eq!(
            parse(&["show-screen", "--steady", "--no-steady"])
                .unwrap_err()
                .exit_code(),
            2
        );
        assert_eq!(
            parse(&["show-screen", "--fps", "31"])
                .unwrap_err()
                .exit_code(),
            2
        );
        assert!(matches!(
            parse(&["preview", "-o", "p.jpg"]).unwrap().command,
            Command::Preview { .. }
        ));
        assert_eq!(parse(&["preview"]).unwrap_err().exit_code(), 2);
    }
    #[test]
    fn move_target_parses() {
        assert_eq!("3".parse(), Ok(MoveTarget::Position(3)));
        assert_eq!("UP".parse(), Ok(MoveTarget::Up));
        assert_eq!("down".parse(), Ok(MoveTarget::Down));
        assert!("0".parse::<MoveTarget>().is_err());
        assert!("-1".parse::<MoveTarget>().is_err());
        assert!("sideways".parse::<MoveTarget>().is_err());
    }

    #[test]
    fn days_shortcuts_expand() {
        let weekdays = expand_days(&[DayItem::Weekdays]);
        assert_eq!(weekdays, Weekday::ALL[..5]);
        assert_eq!(expand_days(&[DayItem::Weekend]), Weekday::ALL[5..]);
        assert!(expand_days(&[DayItem::Daily]).is_empty());
        assert!(expand_days(&[]).is_empty());
        assert_eq!(
            expand_days(&[
                DayItem::Day(Weekday::Sun),
                DayItem::Day(Weekday::Mon),
                DayItem::Weekdays
            ]),
            [
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
                Weekday::Sun
            ]
        );
        assert!(parse_day_item("funday").is_err());
        assert_eq!(parse_day_item("Tue"), Ok(DayItem::Day(Weekday::Tue)));
    }

    #[test]
    fn profile_trigger_add_time_parses() {
        let cli = parse(&[
            "profile", "trigger", "add-time", "Night", "22:00", "--days", "weekdays",
        ])
        .unwrap();
        let Command::Profile {
            action:
                ProfileAction::Trigger {
                    action: TriggerAction::AddTime { name, time, days },
                },
        } = cli.command
        else {
            panic!("wrong command");
        };
        assert_eq!(name, "Night");
        assert_eq!(time, TimeOfDay::new(22, 0).unwrap());
        assert_eq!(days, [DayItem::Weekdays]);
        assert!(parse(&["profile", "trigger", "add-time", "Night", "24:00"]).is_err());
    }

    #[test]
    fn profile_range_and_reset_parse() {
        let cli = parse(&[
            "profile",
            "trigger",
            "add-range",
            "Night",
            "22:00",
            "06:00",
            "--days",
            "weekdays",
        ])
        .unwrap();
        let Command::Profile {
            action:
                ProfileAction::Trigger {
                    action:
                        TriggerAction::AddRange {
                            name,
                            start,
                            end,
                            days,
                        },
                },
        } = cli.command
        else {
            panic!("wrong command");
        };
        assert_eq!(name, "Night");
        assert_eq!(start, TimeOfDay::new(22, 0).unwrap());
        assert_eq!(end, TimeOfDay::new(6, 0).unwrap());
        assert_eq!(days, [DayItem::Weekdays]);
        assert!(
            parse(&[
                "profile",
                "trigger",
                "remove-range",
                "Night",
                "22:00",
                "06:00"
            ])
            .is_ok()
        );
        assert!(parse(&["profile", "trigger", "add-range", "Night", "22:00"]).is_err());
        let cli = parse(&["profile", "reset", "Default"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Profile {
                action: ProfileAction::Reset { .. }
            }
        ));
    }

    #[test]
    fn profile_move_parses() {
        assert!(parse(&["profile", "move", "Night", "2"]).is_ok());
        assert!(parse(&["profile", "move", "Night", "up"]).is_ok());
        assert!(parse(&["profile", "move", "Night", "0"]).is_err());
        assert!(parse(&["profile", "move", "Night"]).is_err());
    }
}
