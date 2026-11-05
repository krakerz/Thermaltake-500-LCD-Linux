//! The Preferences dialog: run the daemon as a systemd user service, start this app at login.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use tt500_lcd_setup::{
    DAEMON_BINARY, ServiceState, SystemRunner, config_home, disable_autostart, enable_autostart,
};

use super::context::Context;
use super::player;
use crate::settings::{self, HwDecode, PREVIEW_MAX_FPS, PREVIEW_MIN_FPS, PreviewSettings};

/// Seconds an error toast stays up in the dialog.
const ERROR_TOAST_SECS: u32 = 6;

/// What the service switch should show after an action.
struct ServiceOutcome {
    /// The action's error, if it failed.
    error: Option<String>,
    /// The unit's state afterwards, if it could be read.
    state: Result<ServiceState, String>,
}

/// Installs (`Some(true)`) or removes (`Some(false)`) the service, then reads its state. Blocking.
fn service_task(change: Option<bool>, socket: Option<PathBuf>) -> ServiceOutcome {
    let run = || -> Result<ServiceState, tt500_lcd_setup::SetupError> {
        let config = config_home()?;
        match change {
            Some(true) => {
                let daemon = tt500_lcd_setup::find_binary(DAEMON_BINARY)?;
                tt500_lcd_setup::install_service(
                    &SystemRunner,
                    &config,
                    &daemon,
                    socket.as_deref(),
                )?;
            }
            Some(false) => tt500_lcd_setup::uninstall_service(&SystemRunner, &config)?,
            None => {}
        }
        tt500_lcd_setup::service_status(&SystemRunner, &config)
    };
    let error = match run() {
        Ok(state) => {
            return ServiceOutcome {
                error: None,
                state: Ok(state),
            };
        }
        Err(error) => error.to_string(),
    };
    // The action failed: still report the real state so the switch tells the truth.
    let state = config_home()
        .and_then(|config| tt500_lcd_setup::service_status(&SystemRunner, &config))
        .map_err(|e| e.to_string());
    ServiceOutcome {
        error: Some(error),
        state,
    }
}

/// Opens the dialog over `ctx`'s window.
pub(super) fn show(ctx: &Rc<Context>) {
    let service_row = adw::SwitchRow::builder()
        .title("Run the panel service at login")
        .subtitle("Checking…")
        .sensitive(false)
        .build();
    let autostart_row = adw::SwitchRow::builder()
        .title("Start this app at login")
        .build();
    let tray_row = adw::SwitchRow::builder()
        .title("Keep running in the tray when the window is closed")
        .subtitle("Needs a system tray; without one, closing the window quits")
        .active(settings::keep_in_tray())
        .build();
    tray_row.connect_active_notify(|row| settings::set_keep_in_tray(row.is_active()));
    let group = adw::PreferencesGroup::new();
    group.add(&service_row);
    group.add(&autostart_row);
    group.add(&tray_row);
    let page = adw::PreferencesPage::new();
    page.add(&group);
    page.add(&preview_group());
    page.add(&super::updates::group(ctx));
    let dialog = adw::PreferencesDialog::builder()
        .title("Preferences")
        .build();
    dialog.add(&page);

    let syncing = Rc::new(Cell::new(false));
    let socket = ctx.socket.clone().ok();

    // Runs `change` (if any) off the main thread and updates the row and a toast.
    let apply = {
        let service_row = service_row.clone();
        let dialog = dialog.clone();
        let syncing = Rc::clone(&syncing);
        Rc::new(move |change: Option<bool>| {
            service_row.set_sensitive(false);
            let socket = socket.clone();
            glib::spawn_future_local(glib::clone!(
                #[weak]
                service_row,
                #[weak]
                dialog,
                #[strong]
                syncing,
                async move {
                    let outcome = gio::spawn_blocking(move || service_task(change, socket)).await;
                    let outcome = outcome.unwrap_or_else(|_| ServiceOutcome {
                        error: Some("The service action failed unexpectedly".to_owned()),
                        state: Err("unknown".to_owned()),
                    });
                    if let Some(error) = &outcome.error {
                        let toast = adw::Toast::builder()
                            .title(error)
                            .timeout(ERROR_TOAST_SECS)
                            .use_markup(false)
                            .build();
                        dialog.add_toast(toast);
                    }
                    syncing.set(true);
                    match outcome.state {
                        Ok(state) => {
                            service_row.set_active(state.unit_installed && state.enabled);
                            service_row.set_subtitle(state.label());
                        }
                        Err(reason) => service_row.set_subtitle(&format!("Unknown ({reason})")),
                    }
                    syncing.set(false);
                    service_row.set_sensitive(true);
                }
            ));
        })
    };
    apply(None);

    let on_service = Rc::clone(&apply);
    let flag = Rc::clone(&syncing);
    service_row.connect_active_notify(move |row| {
        if !flag.get() {
            on_service(Some(row.is_active()));
        }
    });

    let flag = Rc::clone(&syncing);
    flag.set(true);
    autostart_row.set_active(config_home().is_ok_and(|c| tt500_lcd_setup::autostart_enabled(&c)));
    flag.set(false);
    autostart_row.connect_active_notify(glib::clone!(
        #[weak]
        dialog,
        move |row| {
            if flag.get() {
                return;
            }
            let wanted = row.is_active();
            if let Err(error) = set_autostart(wanted) {
                dialog.add_toast(
                    adw::Toast::builder()
                        .title(error)
                        .timeout(ERROR_TOAST_SECS)
                        .use_markup(false)
                        .build(),
                );
                flag.set(true);
                row.set_active(!wanted);
                flag.set(false);
            }
        }
    ));

    dialog.present(Some(&ctx.window));
}

/// The "Preview" group: how the crop editor plays animations and videos. Changes apply at once.
fn preview_group() -> adw::PreferencesGroup {
    let current = settings::preview();
    let play_row = adw::SwitchRow::builder()
        .title("Play animations and videos")
        .subtitle("Off shows the first frame and runs no decoder")
        .active(current.play)
        .build();
    let fps_row = adw::SpinRow::builder()
        .title("Preview frame rate")
        .subtitle("Lower uses less CPU")
        .adjustment(&gtk::Adjustment::new(
            f64::from(current.clamped_fps()),
            f64::from(PREVIEW_MIN_FPS),
            f64::from(PREVIEW_MAX_FPS),
            1.0,
            5.0,
            0.0,
        ))
        .build();
    let pause_row = adw::SwitchRow::builder()
        .title("Pause when the window is unfocused")
        .subtitle("A hidden or minimized window always pauses")
        .active(current.pause_unfocused)
        .build();
    let hw_row = adw::ComboRow::builder()
        .title("Hardware decoding")
        .subtitle("VA-API, with automatic fallback to software")
        .model(&gtk::StringList::new(&["Auto", "Off"]))
        .selected(match current.hw_decode {
            HwDecode::Auto => 0,
            HwDecode::Off => 1,
        })
        .build();
    let commit = {
        let (play_row, fps_row, pause_row, hw_row) = (
            play_row.clone(),
            fps_row.clone(),
            pause_row.clone(),
            hw_row.clone(),
        );
        Rc::new(move || {
            settings::set_preview(PreviewSettings {
                play: play_row.is_active(),
                fps: fps_row.value().round() as u32,
                pause_unfocused: pause_row.is_active(),
                hw_decode: if hw_row.selected() == 1 {
                    HwDecode::Off
                } else {
                    HwDecode::Auto
                },
            });
            player::apply_preview_settings();
        })
    };
    let changed = Rc::clone(&commit);
    play_row.connect_active_notify(move |_| changed());
    let changed = Rc::clone(&commit);
    fps_row.connect_value_notify(move |_| changed());
    // Pausing is read live by the running preview; no restart is needed, but saving is.
    let changed = Rc::clone(&commit);
    pause_row.connect_active_notify(move |_| changed());
    hw_row.connect_selected_notify(move |_| commit());
    let group = adw::PreferencesGroup::builder().title("Preview").build();
    group.add(&play_row);
    group.add(&fps_row);
    group.add(&pause_row);
    group.add(&hw_row);
    group
}

fn set_autostart(on: bool) -> Result<(), String> {
    let config = config_home().map_err(|e| e.to_string())?;
    if on {
        let gui =
            tt500_lcd_setup::find_binary(tt500_lcd_setup::GUI_BINARY).map_err(|e| e.to_string())?;
        enable_autostart(&config, &gui)
    } else {
        disable_autostart(&config)
    }
    .map_err(|e| e.to_string())
}
