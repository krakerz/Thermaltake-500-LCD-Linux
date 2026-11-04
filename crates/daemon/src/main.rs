//! `tt500-lcdd`: owns the panel and streams the current image, animation or video to it.

mod args;
mod boot;
mod dirs;
mod hub;
mod live;
mod server;
mod standby;
mod store;
mod streamer;

use std::env;
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;
use tt500_lcd_ipc::{DeviceState, IPC_VERSION, ShowLiveParams, Status};
use tt500_lcd_protocol::Brightness;

use crate::args::{Command as Cli, DeviceMode};
use crate::dirs::Dirs;
use crate::hub::StatusHub;
use crate::server::ServerContext;
use crate::store::{ContentStore, Restored};
use crate::streamer::{Command, Connector, Core, HidConnector, NullConnector};

/// How often a stored live source is started again at login, and how far apart.
const RESTORE_TRIES: u32 = 12;
const RESTORE_RETRY_INTERVAL: Duration = Duration::from_secs(5);

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
    let dirs = Dirs::new()?;
    let store = ContentStore::new(dirs.clone());
    let brightness = store.brightness();
    let listener = server::bind(&dirs.socket)?;

    let status = Arc::new(StatusHub::new(Status {
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        ipc_version: IPC_VERSION,
        device: DeviceState::Idle,
        content: None,
        brightness: brightness.get(),
        content_error: None,
    }));
    let (commands, receiver) = mpsc::channel();
    match mode {
        DeviceMode::Hardware => {
            spawn_streamer(HidConnector::default, brightness, &status, receiver)?;
        }
        DeviceMode::Null => {
            tracing::info!("--no-device: frames go to a null transport");
            spawn_streamer(|| NullConnector, brightness, &status, receiver)?;
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
    });
    if let Some(params) = live_restore {
        let ctx = Arc::clone(&ctx);
        let generation = ctx.store.generation();
        thread::Builder::new()
            .name("live-restore".into())
            .spawn(move || restore_live(&ctx, &params, generation))
            .context("cannot spawn the live-restore thread")?;
    }
    server::serve(&listener, &ctx);
    Ok(())
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
            let core = Core::new(make_connector(), brightness, status);
            streamer::run(core, &commands);
        })
        .context("cannot spawn the streamer thread")?;
    Ok(())
}
