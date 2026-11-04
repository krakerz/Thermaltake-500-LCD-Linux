//! Minimal command line: `-h`, `-v`.

use std::ffi::OsString;
use std::fmt;

/// What the command line asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Print the help text and exit.
    Help,
    /// Print the version and exit.
    Version,
    /// Run the app.
    Run,
}

/// A command-line usage error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(String);

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// Parses the arguments, program name first.
///
/// # Errors
/// Returns a [`UsageError`] for an unknown option or an unexpected argument.
pub fn parse<I>(args: I) -> Result<Command, UsageError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut options_ended = false;
    for arg in args.into_iter().skip(1) {
        let text = arg.to_string_lossy();
        if options_ended {
            return Err(UsageError(format!("unexpected argument '{text}'")));
        }
        match text.as_ref() {
            "-h" | "--help" => return Ok(Command::Help),
            "-v" | "--version" => return Ok(Command::Version),
            "--" => options_ended = true,
            other if other.starts_with('-') => {
                return Err(UsageError(format!("unknown option '{other}'")));
            }
            other => return Err(UsageError(format!("unexpected argument '{other}'"))),
        }
    }
    Ok(Command::Run)
}

/// The text printed by `--help`.
pub fn help() -> String {
    format!(
        "tt500-lcd-gui {version}\n\
         Window to show images, GIFs and videos on the Thermaltake Tower 500 LCD panel.\n\
         \n\
         Usage: tt500-lcd-gui [OPTIONS]\n\
         \n\
         Options:\n\
         \x20 -h, --help       Print this help\n\
         \x20 -v, --version    Print the version\n\
         \x20     --           End of options\n\
         \n\
         It talks to the tt500-lcdd daemon, which must be running.\n",
        version = env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Command, UsageError> {
        parse(
            std::iter::once("tt500-lcd-gui")
                .chain(args.iter().copied())
                .map(OsString::from),
        )
    }

    #[test]
    fn defaults_to_run() {
        assert_eq!(run(&[]), Ok(Command::Run));
    }

    #[test]
    fn help_and_version() {
        assert_eq!(run(&["-h"]), Ok(Command::Help));
        assert_eq!(run(&["--help"]), Ok(Command::Help));
        assert_eq!(run(&["-v"]), Ok(Command::Version));
        assert_eq!(run(&["--version"]), Ok(Command::Version));
    }

    #[test]
    fn unknown_option_is_an_error() {
        assert!(run(&["--bogus"]).is_err());
        assert!(run(&["-x"]).is_err());
        assert!(run(&["extra"]).is_err());
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(run(&["--"]), Ok(Command::Run));
        assert!(run(&["--", "extra"]).is_err());
    }
}
