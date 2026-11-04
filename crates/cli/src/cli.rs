//! Command-line definition.

use std::path::PathBuf;
use std::str::FromStr;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum, value_parser};
use tt500_lcd_ipc::{Crop, Fit, MAX_FPS, Size};

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
    /// Stop streaming; the panel falls back to its stored image (brightness is kept)
    Clear,
    /// Set the panel brightness in percent (0 turns the panel off)
    Brightness {
        /// Brightness, 0-100
        #[arg(value_parser = value_parser!(u8).range(0..=100))]
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
}
