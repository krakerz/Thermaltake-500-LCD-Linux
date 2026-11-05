//! Updates: the Preferences group (check now, install, check automatically) and the quiet
//! once-a-day check at start-up. The logic lives in `tt500-lcd-update`; this is only wiring.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use tt500_lcd_update::{Applied, InstallKind, Release, Updater, Version, check_due, unix_now};

use super::context::Context;
use crate::settings;

/// How often the progress bar follows the download.
const PROGRESS_TICK_MS: u64 = 150;
/// Seconds the "update available" toast stays up.
const AVAILABLE_TOAST_SECS: u32 = 8;

thread_local! {
    /// The newest release found, kept so the Preferences group can offer it.
    static AVAILABLE: RefCell<Option<Release>> = const { RefCell::new(None) };
}

/// The GUI to start once the application has released its D-Bus name.
static RELAUNCH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Starts the updated GUI. Call it after `app.run()` returns: started earlier, the new copy
/// would find the old process still owning the bus name, activate it and exit.
pub fn spawn_pending_relaunch() {
    let path = RELAUNCH.lock().ok().and_then(|mut slot| slot.take());
    if let Some(path) = path
        && let Err(error) = Command::new(&path).spawn()
    {
        eprintln!("tt500-lcd-gui: cannot start {}: {error}", path.display());
    }
}

fn check_task() -> Result<Option<Release>, String> {
    Updater::for_this_install()
        .and_then(|updater| updater.check())
        .map_err(|e| e.to_string())
}

fn apply_task(
    release: &Release,
    received: &AtomicU64,
    total: &AtomicU64,
) -> Result<Applied, String> {
    let updater = Updater::for_this_install().map_err(|e| e.to_string())?;
    updater
        .apply(release, &mut |done, size| {
            received.store(done, Ordering::Relaxed);
            total.store(size.unwrap_or(0), Ordering::Relaxed);
        })
        .map_err(|e| e.to_string())
}

/// Checks quietly (installed builds with the setting on, at most daily) and, if a newer
/// release exists, shows a toast. Errors stay silent.
pub(super) fn auto_check(ctx: &Rc<Context>) {
    let now = unix_now();
    let enabled = InstallKind::current().can_update()
        && settings::check_updates()
        && check_due(settings::last_update_check(), now);
    if !enabled {
        return;
    }
    let ctx = Rc::clone(ctx);
    glib::spawn_future_local(async move {
        let Ok(Ok(found)) = gio::spawn_blocking(check_task).await else {
            return;
        };
        settings::set_last_update_check(now);
        let Some(release) = found else { return };
        let message = format!("TT500 LCD {} is available", release.version());
        AVAILABLE.with(|slot| *slot.borrow_mut() = Some(release));
        let toast = adw::Toast::builder()
            .title(message)
            .button_label("Update")
            .timeout(AVAILABLE_TOAST_SECS)
            .use_markup(false)
            .build();
        let open = Rc::clone(&ctx);
        toast.connect_button_clicked(move |_| super::preferences::show(&open));
        ctx.toasts.add_toast(toast);
    });
}

/// What the "Check for updates" row is doing.
#[derive(Clone)]
enum Phase {
    Idle,
    Checking,
    Available(Version),
    Downloading,
    Installed(Option<PathBuf>),
}

struct Widgets {
    row: adw::ActionRow,
    button: gtk::Button,
    progress: gtk::ProgressBar,
    phase: RefCell<Phase>,
    /// Set while the download runs, so the progress timer knows when to stop.
    busy: Cell<bool>,
}

impl Widgets {
    fn show(&self, phase: &Phase) {
        *self.phase.borrow_mut() = phase.clone();
        self.progress
            .set_visible(matches!(phase, Phase::Downloading));
        self.button
            .set_sensitive(!matches!(phase, Phase::Checking | Phase::Downloading));
        let (label, subtitle) = match phase {
            Phase::Idle => ("Check", None),
            Phase::Checking => ("Checking…", Some("Looking for a newer release".to_owned())),
            Phase::Available(version) => ("Update", Some(format!("{version} is available"))),
            Phase::Downloading => ("Updating…", Some("Downloading".to_owned())),
            Phase::Installed(_) => ("Restart", Some("Updated; restart to use it".to_owned())),
        };
        self.button.set_label(label);
        if let Some(text) = subtitle {
            self.row.set_subtitle(&text);
        }
    }

    fn fail(&self, what: &str, reason: &str) {
        self.show(&Phase::Idle);
        self.row.set_subtitle(&format!("{what}: {reason}"));
    }
}

fn describe() -> String {
    let kind = InstallKind::current();
    format!("Version {} · {}", Version::current(), kind.label())
}

/// The Preferences "Updates" group.
pub(super) fn group(ctx: &Rc<Context>) -> adw::PreferencesGroup {
    let can_update = InstallKind::current().can_update();
    let row = adw::ActionRow::builder()
        .title("Updates")
        .subtitle(if can_update {
            describe()
        } else {
            format!("{} · updates are only for installed builds", describe())
        })
        .build();
    let button = gtk::Button::builder()
        .label("Check")
        .valign(gtk::Align::Center)
        .sensitive(can_update)
        .build();
    let progress = gtk::ProgressBar::builder()
        .valign(gtk::Align::Center)
        .width_request(120)
        .visible(false)
        .build();
    row.add_suffix(&progress);
    row.add_suffix(&button);
    let widgets = Rc::new(Widgets {
        row,
        button: button.clone(),
        progress,
        phase: RefCell::new(Phase::Idle),
        busy: Cell::new(false),
    });
    if let Some(release) = AVAILABLE.with(|slot| slot.borrow().clone()) {
        widgets.show(&Phase::Available(release.version()));
    }

    let auto_row = adw::SwitchRow::builder()
        .title("Check for updates automatically")
        .subtitle("At most once a day")
        .active(settings::check_updates())
        .sensitive(can_update)
        .build();
    auto_row.connect_active_notify(|row| settings::set_check_updates(row.is_active()));

    let ctx = Rc::clone(ctx);
    let on_click = Rc::clone(&widgets);
    button.connect_clicked(move |_| {
        let phase = on_click.phase.borrow().clone();
        match phase {
            Phase::Idle => check_now(&on_click),
            Phase::Available(_) => install(&on_click, &ctx),
            Phase::Installed(gui) => restart(&ctx, gui),
            Phase::Checking | Phase::Downloading => {}
        }
    });

    let group = adw::PreferencesGroup::builder().title("Updates").build();
    group.add(&widgets.row);
    group.add(&auto_row);
    group
}

fn check_now(widgets: &Rc<Widgets>) {
    widgets.show(&Phase::Checking);
    let widgets = Rc::clone(widgets);
    glib::spawn_future_local(async move {
        match gio::spawn_blocking(check_task).await {
            Ok(Ok(Some(release))) => {
                settings::set_last_update_check(unix_now());
                widgets.show(&Phase::Available(release.version()));
                AVAILABLE.with(|slot| *slot.borrow_mut() = Some(release));
            }
            Ok(Ok(None)) => {
                settings::set_last_update_check(unix_now());
                widgets.show(&Phase::Idle);
                widgets
                    .row
                    .set_subtitle(&format!("{} · up to date", describe()));
            }
            Ok(Err(reason)) => widgets.fail("Couldn't check", &reason),
            Err(_) => widgets.fail("Couldn't check", "the check stopped unexpectedly"),
        }
    });
}

fn install(widgets: &Rc<Widgets>, ctx: &Rc<Context>) {
    let Some(release) = AVAILABLE.with(|slot| slot.borrow().clone()) else {
        check_now(widgets);
        return;
    };
    widgets.show(&Phase::Downloading);
    widgets.progress.set_fraction(0.0);
    widgets.busy.set(true);
    let received = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(0));
    {
        let (received, total) = (Arc::clone(&received), Arc::clone(&total));
        let widgets = Rc::clone(widgets);
        glib::timeout_add_local(Duration::from_millis(PROGRESS_TICK_MS), move || {
            if !widgets.busy.get() {
                return glib::ControlFlow::Break;
            }
            let (done, size) = (
                received.load(Ordering::Relaxed),
                total.load(Ordering::Relaxed),
            );
            let megabytes = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
            let subtitle = match done.checked_mul(100).and_then(|n| n.checked_div(size)) {
                Some(percent) => {
                    widgets.progress.set_fraction(done as f64 / size as f64);
                    format!(
                        "Downloading · {percent} % · {:.1} / {:.1} MB",
                        megabytes(done),
                        megabytes(size)
                    )
                }
                None => {
                    widgets.progress.pulse();
                    format!("Downloading · {:.1} MB", megabytes(done))
                }
            };
            widgets.row.set_subtitle(&subtitle);
            glib::ControlFlow::Continue
        });
    }
    let widgets = Rc::clone(widgets);
    let ctx = Rc::clone(ctx);
    glib::spawn_future_local(async move {
        let outcome = gio::spawn_blocking(move || apply_task(&release, &received, &total)).await;
        widgets.busy.set(false);
        match outcome {
            Ok(Ok(applied)) => {
                AVAILABLE.with(|slot| *slot.borrow_mut() = None);
                widgets.show(&Phase::Installed(applied.gui));
                if let Some(error) = applied.restart_error {
                    ctx.toast_for(
                        &format!("Updated, but the panel service did not restart: {error}"),
                        8,
                    );
                }
            }
            Ok(Err(reason)) => {
                widgets.fail("Update failed", &reason);
                widgets.button.set_label("Retry");
            }
            Err(_) => widgets.fail("Update failed", "the update stopped unexpectedly"),
        }
    });
}

/// Quits so `main` can start the new copy once the bus name is free.
fn restart(ctx: &Rc<Context>, gui: Option<PathBuf>) {
    if let Some(path) = gui
        && let Ok(mut slot) = RELAUNCH.lock()
    {
        *slot = Some(path);
    }
    match ctx.window.application() {
        Some(app) => app.quit(),
        None => ctx.toast("Close and reopen the app to use the new version"),
    }
}
