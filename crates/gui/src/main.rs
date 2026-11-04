//! `tt500-lcd-gui`: GTK app for the tt500-lcd daemon.

mod area;
mod cli;
mod daemon;
mod geometry;
mod media;
mod playback;
mod settings;
mod state;
mod ui;
mod upload;

use std::process::ExitCode;

use adw::prelude::*;

/// Application id (placeholder until the project owner picks the real one).
const APP_ID: &str = "io.github.krakerz.Tt500Lcd";

fn main() -> ExitCode {
    match cli::parse(std::env::args_os()) {
        Ok(cli::Command::Help) => {
            print!("{}", cli::help());
            ExitCode::SUCCESS
        }
        Ok(cli::Command::Version) => {
            println!("TT500 LCD {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(cli::Command::Run) => run(),
        Err(error) => {
            eprintln!("tt500-lcd-gui: {error}\nTry 'tt500-lcd-gui --help' for more information.");
            ExitCode::from(2)
        }
    }
}

fn run() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| ui::icons::install());
    app.connect_activate(ui::window::present);
    // The command line was parsed above; GTK must not parse it again.
    let code = app.run_with_args::<&str>(&[]);
    if code == gtk::glib::ExitCode::SUCCESS {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
