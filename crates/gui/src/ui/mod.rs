//! GTK wiring.

mod area_page;
mod boot_page;
mod context;
mod crop_panel;
mod editor;
mod file_source;
pub mod icons;
mod player;
mod preferences;
mod profile_switcher;
mod profiles_page;
mod static_page;
mod tray_link;
mod trigger_editor;
mod updates;
pub mod window;

pub use updates::spawn_pending_relaunch;
