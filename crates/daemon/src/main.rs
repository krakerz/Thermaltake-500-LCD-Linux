//! `tt500-lcdd`: owns the panel and streams the current image, animation or video to it.

mod args;
mod boot;
mod dirs;
mod hub;
mod live;
mod profiles;
mod server;
mod standby;
mod store;
mod streamer;

use std::env;
use std::fs;
use std::path::Path;
use std::process::{self, ExitCode};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use tracing_subscriber::EnvFilter;
use tt500_lcd_ipc::{DeviceState, IPC_VERSION, ShowLiveParams, Status};
use tt500_lcd_protocol::{Brightness, Rotation};

use crate::args::{Command as Cli, DeviceMode};
use crate::dirs::Dirs;
use crate::hub::StatusHub;
use crate::profiles::ProfileHub;
use crate::profiles::scheduler::Scheduler;
use crate::profiles::sources::{LocalClock, ProcFs};
use crate::server::ServerContext;
use crate::store::{ContentStore, Restored};
use crate::streamer::{Command, Connector, Core, HidConnector, NullConnector};

/// How often a stored live source is started again at login, and how far apart.
const RESTORE_TRIES: u32 = 12;
const RESTORE_RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// The whole shutdown after a signal; past this the process exits anyway.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);
/// How long shutdown waits for the streamer to drop its source and close the panel.
const STREAMER_STOP_WAIT: Duration = Duration::from_millis(4500);

fn main() -> ExitCode {
    match args::parse(env::args_os()) {
        Ok(Cli::Help) => {
            print!("{}", args::help());
            ExitCode::SUCCESS
        }
        Ok(Cli::Version) => {
            println!("tt500-lcdd {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Cli::Run(mode)) => {
            init_logging();
            match run(mode) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("tt500-lcdd: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Err(err) => {
            eprintln!("tt500-lcdd: {err}");
            eprintln!("Try 'tt500-lcdd --help' for more information.");
            ExitCode::from(2)
        }
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if env::var_os("JOURNAL_STREAM").is_some() {
        builder.without_time().with_ansi(false).init();
    } else {
        builder.init();
    }
}

fn run(mode: DeviceMode) -> Result<()> {
    // Registered before anything is spawned, so a signal is never lost to the default action.
    let mut signals = Signals::new([SIGINT, SIGTERM]).context("cannot install signal handlers")?;
    let dirs = Dirs::new()?;
    let store = ContentStore::new(dirs.clone());
    let brightness = store.brightness();
    let rotation = store.rotation();
    let listener = server::bind(&dirs.socket)?;

    let status = Arc::new(StatusHub::new(Status {
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        ipc_version: IPC_VERSION,
        device: DeviceState::Idle,
        content: None,
        brightness: brightness.get(),
        rotation: Some(rotation.get()),
        content_error: None,
        profile: None,
    }));
    let profiles = ProfileHub::load(&dirs.state, Arc::clone(&status), || store.stored_spec());
    let (commands, receiver) = mpsc::channel();
    match mode {
        DeviceMode::Hardware => {
            spawn_streamer(
                HidConnector::default,
                brightness,
                rotation,
                &status,
                receiver,
            )?;
        }
        DeviceMode::Null => {
            tracing::info!("--no-device: frames go to a null transport");
            spawn_streamer(|| NullConnector, brightness, rotation, &status, receiver)?;
        }
    }

    let mut live_restore = None;
    let restored = match store.restore() {
        Restored::Nothing => None,
        Restored::Loaded(content, info) => {
            tracing::info!("restoring {}", info.source);
            let (reply, _unused) = mpsc::channel();
            Some(Command::Show {
                content,
                info,
                reply,
            })
        }
        Restored::Live(params) => {
            live_restore = Some(params);
            None
        }
        Restored::Failed(message) => Some(Command::RestoreFailed { message }),
    };
    if let Some(command) = restored {
        commands
            .send(command)
            .map_err(|_| anyhow::anyhow!("streamer stopped during start-up"))?;
    }

    tracing::info!("listening on {}", dirs.socket.display());
    let run_dir = dirs
        .socket
        .parent()
        .context("the socket has no parent directory")?
        .join("live");
    let ctx = Arc::new(ServerContext {
        store,
        commands,
        status,
        live: Mutex::new(None),
        run_dir,
        profiles,
    });
    // Sway and desktop commands of a daemon that was killed outright.
    live::vdesktop::clean_leftovers(&ctx.run_dir);
    if let Some(params) = live_restore {
        let ctx = Arc::clone(&ctx);
        let generation = ctx.store.generation();
        thread::Builder::new()
            .name("live-restore".into())
            .spawn(move || restore_live(&ctx, &params, generation))
            .context("cannot spawn the live-restore thread")?;
    }
    {
        let ctx = Arc::clone(&ctx);
        thread::Builder::new()
            .name("profile-scheduler".into())
            .spawn(move || Scheduler::new(LocalClock, ProcFs).run(&ctx))
            .context("cannot spawn the profile-scheduler thread")?;
    }
    {
        let ctx = Arc::clone(&ctx);
        thread::Builder::new()
            .name("ipc-accept".into())
            .spawn(move || server::serve(&listener, &ctx))
            .context("cannot spawn the accept thread")?;
    }
    let signal = signals.forever().next().unwrap_or(SIGTERM);
    shutdown(&ctx, &dirs.socket, signal);
    Ok(())
}

/// Stops the daemon cleanly: no new IPC, the live source (desktop, ffmpeg) and the panel are
/// released by the streamer, the socket is removed. Bounded by [`SHUTDOWN_BUDGET`].
fn shutdown(ctx: &ServerContext, socket: &Path, signal: i32) {
    let name = match signal {
        SIGINT => "SIGINT",
        SIGTERM => "SIGTERM",
        _ => "signal",
    };
    tracing::info!("shutting down ({name})");
    let watchdog = thread::Builder::new()
        .name("shutdown-watchdog".into())
        .spawn(|| {
            thread::sleep(SHUTDOWN_BUDGET);
            tracing::error!("shutdown took more than {SHUTDOWN_BUDGET:?}; exiting anyway");
            process::exit(1);
        });
    if let Err(err) = watchdog {
        tracing::warn!("cannot spawn the shutdown watchdog: {err}");
    }
    ctx.profiles.stop();
    // New connections fail from here on.
    if let Err(err) = fs::remove_file(socket) {
        tracing::warn!("cannot remove {}: {err}", socket.display());
    }
    // Drop our handle on the live source, then have the streamer drop its own and close the
    // panel; the reply comes once that is done.
    *ctx.live.lock().expect("lock poisoned") = None;
    let (reply, stopped) = mpsc::channel();
    if ctx.commands.send(Command::Clear { reply }).is_ok()
        && stopped.recv_timeout(STREAMER_STOP_WAIT).is_err()
    {
        tracing::warn!("the streamer did not stop in time");
    }
}

/// Tries to start the stored live source again; the screen or compositor may not be up yet
/// at login. Gives up quietly when the content changed meanwhile.
fn restore_live(ctx: &ServerContext, params: &ShowLiveParams, generation: u64) {
    let mut last_error = String::new();
    for attempt in 1..=RESTORE_TRIES {
        if ctx.store.generation() != generation {
            return;
        }
        match server::show_live(ctx, params.clone()) {
            Ok(_) => return,
            Err(err) => {
                tracing::warn!("restoring the live source (try {attempt}/{RESTORE_TRIES}): {err}");
                last_error = err.message;
            }
        }
        if attempt < RESTORE_TRIES {
            thread::sleep(RESTORE_RETRY_INTERVAL);
        }
    }
    if ctx.store.generation() == generation {
        // The stored record stays, so the next start tries again.
        let _ = ctx.commands.send(Command::RestoreFailed {
            message: format!("cannot restore the live source: {last_error}"),
        });
    }
}

/// Runs the streamer on its own thread; the connector is built there so it need not be `Send`.
fn spawn_streamer<C, F>(
    make_connector: F,
    brightness: Brightness,
    rotation: Rotation,
    status: &Arc<StatusHub>,
    commands: Receiver<Command>,
) -> Result<()>
where
    C: Connector,
    F: FnOnce() -> C + Send + 'static,
{
    let status = Arc::clone(status);
    thread::Builder::new()
        .name("streamer".into())
        .spawn(move || {
            let core = Core::new(make_connector(), brightness, rotation, status);
            streamer::run(core, &commands);
        })
        .context("cannot spawn the streamer thread")?;
    Ok(())
}
