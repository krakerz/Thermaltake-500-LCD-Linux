//! The GTK side of the tray: owns the tray handle and pushes the window's state into it.

use std::cell::RefCell;

use gtk::gio;

use super::context::Context;
use crate::tray::{Tray, TrayState};

/// The tray of the main window, once it is registered. Main thread only.
#[derive(Default)]
pub struct TrayLink {
    tray: RefCell<Option<Tray>>,
    /// What the tray shows now, to skip updates that change nothing.
    shown: RefCell<TrayState>,
    /// Keeps the app running while the window is hidden.
    hold: RefCell<Option<gio::ApplicationHoldGuard>>,
}

impl TrayLink {
    /// Whether a tray is registered, so a hidden window can still be reached.
    pub fn active(&self) -> bool {
        self.tray.borrow().is_some()
    }

    /// The state the tray should show for `ctx`.
    pub fn state_for(ctx: &Context) -> TrayState {
        TrayState::from_status(ctx.status.borrow().as_ref(), &ctx.profiles.borrow())
    }

    /// Takes over a registered `tray`; `hold` keeps the app alive while no window is shown.
    pub fn install(&self, tray: Tray, hold: gio::ApplicationHoldGuard, ctx: &Context) {
        // The tray was registered with an empty state before any status arrived: push the
        // current one now, or `sync` sees no change and the menu stays disabled.
        let state = Self::state_for(ctx);
        tray.update(state.clone());
        *self.shown.borrow_mut() = state;
        *self.tray.borrow_mut() = Some(tray);
        *self.hold.borrow_mut() = Some(hold);
    }

    /// Pushes `ctx`'s status and profiles into the tray menu and tooltip when they changed.
    pub fn sync(&self, ctx: &Context) {
        let tray = self.tray.borrow();
        let Some(tray) = tray.as_ref() else {
            return;
        };
        let state = Self::state_for(ctx);
        if *self.shown.borrow() != state {
            *self.shown.borrow_mut() = state.clone();
            tray.update(state);
        }
    }
}
