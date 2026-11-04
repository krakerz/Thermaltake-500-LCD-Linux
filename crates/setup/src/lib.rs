//! Service management shared by the CLI and the GUI: the daemon as a systemd user service,
//! and the GUI's XDG autostart entry. Nothing here depends on GTK or HID.

mod autostart;
mod error;
mod files;
mod runner;
mod service;

pub use autostart::{autostart_enabled, disable_autostart, enable_autostart};
pub use error::SetupError;
pub use files::{config_home, find_binary};
pub use runner::{Runner, SystemRunner};
pub use service::{ServiceState, UNIT_NAME, install_service, service_status, uninstall_service};

/// File name of the daemon binary.
pub const DAEMON_BINARY: &str = "tt500-lcdd";
/// File name of the GUI binary.
pub const GUI_BINARY: &str = "tt500-lcd-gui";
