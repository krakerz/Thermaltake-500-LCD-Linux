//! Checks GitHub for a newer tt500-lcd release and installs it over the running copy.
//! Nothing here depends on GTK; the GUI and the CLI both drive it, and the tests run it
//! against a fake HTTP server and a fake command runner.

mod error;
mod files;
mod http;
mod install;
mod release;
mod updater;
mod version;

pub use error::UpdateError;
pub use http::{Http, UreqHttp};
pub use install::{InstallKind, UPDATE_URL_ENV};
pub use release::Release;
pub use updater::{Applied, CHECK_INTERVAL_SECS, Updater, check_due, unix_now};
pub use version::Version;
