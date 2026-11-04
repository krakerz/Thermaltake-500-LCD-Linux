//! `tt500-lcd`: command-line client for the `tt500-lcdd` daemon.

mod cli;
mod format;
mod profile;

use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

use anyhow::Context;
use clap::{CommandFactory, Parser, error::ErrorKind};
use tt500_lcd_ipc::{
    BrightnessParams, Client, ClientError, DesktopSpec, DryRunReply, IPC_VERSION, LiveSource,
    Request, RotationParams, ShowLiveParams, ShowParams, Status, UploadBootAnimationParams,
    UploadStandbyParams, socket_path,
};

use tt500_lcd_setup::SystemRunner;

use crate::cli::{AutostartAction, Cli, Command, ServiceAction, desktop_commands};
use crate::format::format_status;

const START_HINT: &str = "start it with: tt500-lcd service install";

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<(), anyhow::Error> {
    // Resolve the file before touching the socket; the daemon's cwd differs from ours.
    let (request, json) = match command {
        Command::Show {
            file,
            fit,
            crop,
            fps,
        } => {
            let path = fs::canonicalize(&file)
                .with_context(|| format!("error: cannot access {}", file.display()))?;
            (
                Request::Show(ShowParams {
                    path,
                    fit,
                    crop,
                    fps,
                }),
                false,
            )
        }
        Command::ShowScreen {
            output,
            fit,
            crop,
            fps,
            steady,
        } => (
            Request::ShowLive(ShowLiveParams {
                source: LiveSource::Screen { output },
                fit,
                crop,
                fps,
                steady: steady.choice(),
            }),
            false,
        ),
        Command::ShowDesktop {
            size,
            scale,
            run,
            argv,
            fit,
            crop,
            fps,
            steady,
        } => {
            let spec = DesktopSpec {
                commands: desktop_commands(run, &argv),
                size,
                scale,
            };
            if spec.commands.is_empty() {
                Cli::command()
                    .error(
                        ErrorKind::MissingRequiredArgument,
                        "give at least one command",
                    )
                    .exit();
            }
            if let Err(msg) = spec.validate() {
                Cli::command().error(ErrorKind::ValueValidation, msg).exit();
            }
            (
                Request::ShowLive(ShowLiveParams {
                    source: LiveSource::Desktop(spec),
                    fit,
                    crop,
                    fps,
                    steady: steady.choice(),
                }),
                false,
            )
        }
        Command::Preview { output } => return preview(&output),
        Command::Standby {
            file,
            fit,
            crop,
            dry_run,
            yes,
        } => {
            let path = fs::canonicalize(&file)
                .with_context(|| format!("error: cannot access {}", file.display()))?;
            return standby(
                UploadStandbyParams {
                    path,
                    fit,
                    crop,
                    dry_run,
                },
                yes,
            );
        }
        Command::Boot {
            file,
            fit,
            crop,
            dry_run,
            yes,
        } => {
            let path = fs::canonicalize(&file)
                .with_context(|| format!("error: cannot access {}", file.display()))?;
            return boot(
                UploadBootAnimationParams {
                    path,
                    fit,
                    crop,
                    dry_run,
                },
                yes,
            );
        }
        Command::Profile { action } => return profile::run(action),
        Command::Watch { json } => return watch(json),
        Command::Service { action } => return service(action),
        Command::Autostart { action } => return autostart(action),
        Command::Clear => (Request::Clear, false),
        Command::Brightness { value } => {
            (Request::SetBrightness(BrightnessParams { value }), false)
        }
        Command::Rotation { value } => (Request::SetRotation(RotationParams { value }), false),
        Command::Status { json } => (Request::Status, json),
    };

    let socket = socket_path().map_err(|e| anyhow::anyhow!("error: {e}"))?;
    let status = Client::connect(&socket)
        .and_then(|mut client| client.call(request))
        .map_err(|e| describe(&e, &socket))?;

    if status.ipc_version != IPC_VERSION {
        eprintln!(
            "warning: daemon speaks IPC version {}, this client {IPC_VERSION}",
            status.ipc_version
        );
    }
    print_status(&status, json)
}

/// `service install|uninstall|status`.
fn service(action: ServiceAction) -> Result<(), anyhow::Error> {
    let config = tt500_lcd_setup::config_home()?;
    match action {
        ServiceAction::Install => {
            let daemon = tt500_lcd_setup::find_binary(tt500_lcd_setup::DAEMON_BINARY)?;
            let socket = socket_path().ok();
            tt500_lcd_setup::install_service(&SystemRunner, &config, &daemon, socket.as_deref())?;
            println!(
                "Installed and started the service (runs {})",
                daemon.display()
            );
        }
        ServiceAction::Uninstall => {
            tt500_lcd_setup::uninstall_service(&SystemRunner, &config)?;
            println!("Stopped and removed the service");
        }
        ServiceAction::Status => {
            let state = tt500_lcd_setup::service_status(&SystemRunner, &config)?;
            println!("service: {}", state.label());
            println!("unit installed: {}", yes_no(state.unit_installed));
            println!("enabled at login: {}", yes_no(state.enabled));
            println!("running: {}", yes_no(state.active));
        }
    }
    Ok(())
}

/// `autostart on|off|status`, for the GUI.
fn autostart(action: AutostartAction) -> Result<(), anyhow::Error> {
    let config = tt500_lcd_setup::config_home()?;
    match action {
        AutostartAction::On => {
            let gui = tt500_lcd_setup::find_binary(tt500_lcd_setup::GUI_BINARY)?;
            tt500_lcd_setup::enable_autostart(&config, &gui)?;
            println!("The app will start at login ({})", gui.display());
        }
        AutostartAction::Off => {
            tt500_lcd_setup::disable_autostart(&config)?;
            println!("The app will not start at login");
        }
        AutostartAction::Status => println!(
            "autostart: {}",
            if tt500_lcd_setup::autostart_enabled(&config) {
                "on"
            } else {
                "off"
            }
        ),
    }
    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

const FLASH_PROMPT: &str = "This writes the image to the panel's flash memory. Continue? [y/N] ";
const BOOT_FLASH_PROMPT: &str =
    "This writes the boot animation to the panel's flash memory. Continue? [y/N] ";

/// Asks `prompt` on `output` and reads one line from `input`; only "y"/"yes" confirms.
fn confirm(prompt: &str, input: &mut impl BufRead, output: &mut impl Write) -> io::Result<bool> {
    output.write_all(prompt.as_bytes())?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Confirms a real flash write, before any connection to the daemon; without a terminal it
/// needs `--yes`. A dry run or `--yes` needs no question.
fn confirm_flash_write(prompt: &str, dry_run: bool, yes: bool) -> Result<(), anyhow::Error> {
    if !dry_run && !yes {
        if !io::stdin().is_terminal() {
            anyhow::bail!(
                "error: refusing to write to the panel's flash without confirmation; \
                 run it in a terminal or pass --yes"
            );
        }
        if !confirm(prompt, &mut io::stdin().lock(), &mut io::stderr())? {
            anyhow::bail!("aborted: nothing was written");
        }
    }
    Ok(())
}

/// Prints where a dry run wrote its files.
fn print_dry_run(files: &DryRunReply) {
    println!("Dry run: nothing was sent to the panel.");
    println!("Packets: {}", files.packets.display());
    println!("Summary: {}", files.summary.display());
}

/// Uploads (or dry-runs) the boot animation. The daemon answers once, when the upload is
/// done, so no progress is shown here (the daemon logs it).
fn boot(params: UploadBootAnimationParams, yes: bool) -> Result<(), anyhow::Error> {
    confirm_flash_write(BOOT_FLASH_PROMPT, params.dry_run, yes)?;
    let dry_run = params.dry_run;
    let socket = socket_path().map_err(|e| anyhow::anyhow!("error: {e}"))?;
    let (_status, dry) = Client::connect(&socket)
        .and_then(|mut client| client.upload_boot_animation(params))
        .map_err(|e| describe(&e, &socket))?;
    match dry {
        Some(files) => print_dry_run(&files),
        None if dry_run => {
            anyhow::bail!("error: the daemon ignored --dry-run; it may be too old for this client")
        }
        None => println!("Boot animation uploaded to the panel."),
    }
    Ok(())
}

/// Uploads (or dry-runs) the standby image.
fn standby(params: UploadStandbyParams, yes: bool) -> Result<(), anyhow::Error> {
    confirm_flash_write(FLASH_PROMPT, params.dry_run, yes)?;
    let dry_run = params.dry_run;
    let socket = socket_path().map_err(|e| anyhow::anyhow!("error: {e}"))?;
    let (_status, dry) = Client::connect(&socket)
        .and_then(|mut client| client.upload_standby(params))
        .map_err(|e| describe(&e, &socket))?;
    match dry {
        Some(files) => print_dry_run(&files),
        None if dry_run => {
            anyhow::bail!("error: the daemon ignored --dry-run; it may be too old for this client")
        }
        None => println!("Standby image uploaded to the panel."),
    }
    Ok(())
}

/// Copies the daemon's latest preview frame to `output`.
fn preview(output: &Path) -> Result<(), anyhow::Error> {
    let socket = socket_path().map_err(|e| anyhow::anyhow!("error: {e}"))?;
    let reply = Client::connect(&socket)
        .and_then(|mut client| client.preview())
        .map_err(|e| describe(&e, &socket))?;
    fs::copy(&reply.path, output)
        .with_context(|| format!("error: cannot write {}", output.display()))?;
    println!(
        "Saved {} ({}x{}) to {}",
        reply
            .path
            .file_name()
            .map_or_else(|| "preview".into(), |n| n.to_string_lossy().into_owned()),
        reply.size.width,
        reply.size.height,
        output.display()
    );
    Ok(())
}

/// Prints the status, then again after every change, until the daemon goes away.
fn watch(json: bool) -> Result<(), anyhow::Error> {
    let socket = socket_path().map_err(|e| anyhow::anyhow!("error: {e}"))?;
    let (mut status, mut subscription) = Client::connect(&socket)
        .and_then(Client::subscribe)
        .map_err(|e| describe(&e, &socket))?;
    loop {
        print_status(&status, json)?;
        if !json {
            println!();
        }
        match subscription.next_status() {
            Ok(Some(next)) => status = next,
            Ok(None) => anyhow::bail!("error: tt500-lcdd closed the connection"),
            Err(err) => return Err(describe(&err, &socket)),
        }
    }
}

pub(crate) fn describe(err: &ClientError, socket: &Path) -> anyhow::Error {
    match err {
        ClientError::Connect { source, .. } => anyhow::anyhow!(
            "cannot reach tt500-lcdd at {}: {source}\n{START_HINT}",
            socket.display()
        ),
        ClientError::Daemon(e) => anyhow::anyhow!("error: {}", e.message),
        other => anyhow::anyhow!("error: {other}"),
    }
}

fn print_status(status: &Status, json: bool) -> Result<(), anyhow::Error> {
    if json {
        let text = serde_json::to_string(status).context("error: cannot encode status")?;
        println!("{text}");
    } else {
        println!("{}", format_status(status));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(answer: &str) -> (bool, String) {
        let mut out = Vec::new();
        let yes = confirm(FLASH_PROMPT, &mut answer.as_bytes(), &mut out).unwrap();
        (yes, String::from_utf8(out).unwrap())
    }

    #[test]
    fn only_yes_confirms() {
        for (answer, want) in [
            ("y\n", true),
            ("YES\n", true),
            (" y \n", true),
            ("\n", false),
            ("n\n", false),
            ("yep\n", false),
            ("", false),
        ] {
            let (got, prompt) = ask(answer);
            assert_eq!(got, want, "answer {answer:?}");
            assert_eq!(
                prompt,
                "This writes the image to the panel's flash memory. Continue? [y/N] "
            );
        }
    }

    #[test]
    fn boot_prompt_names_the_boot_animation() {
        let mut out = Vec::new();
        assert!(!confirm(BOOT_FLASH_PROMPT, &mut "n\n".as_bytes(), &mut out).unwrap());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "This writes the boot animation to the panel's flash memory. Continue? [y/N] "
        );
    }
}
