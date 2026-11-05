//! The sidebar-footer profile switcher: a drop-down over the saved profiles.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use tt500_lcd_ipc::{ActiveProfile, ProfileNameParams, ProfileStatus, Request};

use super::context::Context;
use crate::profiles::reason_text;

/// Drop-down listing the saved profiles in the daemon's order. Cloning shares it.
#[derive(Clone)]
pub struct ProfileSwitcher {
    ctx: Rc<Context>,
    root: gtk::Box,
    caption: gtk::Label,
    dropdown: gtk::DropDown,
    model: gtk::StringList,
    /// The daemon's active profile, to re-select after a rebuild.
    active: Rc<RefCell<Option<ActiveProfile>>>,
    /// Whether the daemon is reachable and speaks profiles.
    available: Rc<Cell<bool>>,
    /// Set while code (not the user) changes the selection, so its handler stays quiet.
    syncing: Rc<Cell<bool>>,
}

impl ProfileSwitcher {
    pub fn new(ctx: &Rc<Context>) -> Self {
        let model = gtk::StringList::new(&[]);
        let dropdown = gtk::DropDown::builder()
            .model(&model)
            .hexpand(true)
            .sensitive(false)
            .build();
        let caption = gtk::Label::builder().label("Profile").xalign(0.0).build();
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        root.append(&caption);
        root.append(&dropdown);
        let this = Self {
            ctx: Rc::clone(ctx),
            root,
            caption,
            dropdown,
            model,
            active: Rc::new(RefCell::new(None)),
            available: Rc::new(Cell::new(false)),
            syncing: Rc::new(Cell::new(false)),
        };
        let handler = this.clone();
        this.dropdown
            .connect_selected_notify(move |_| handler.selected());
        this
    }

    pub fn widget(&self) -> gtk::Box {
        self.root.clone()
    }

    /// Rebuilds the entries from `ctx.profiles` (after `Context::reload_profiles`).
    pub fn set_profiles(&self) {
        let names: Vec<String> = self
            .ctx
            .profiles
            .borrow()
            .iter()
            .map(|profile| profile.name.clone())
            .collect();
        let entries: Vec<&str> = names.iter().map(String::as_str).collect();
        self.syncing.set(true);
        self.model.splice(0, self.model.n_items(), &entries);
        self.syncing.set(false);
        self.sync();
    }

    /// Follows the daemon's profile status; `None` while it is down or too old for profiles.
    pub fn follow(&self, status: Option<&ProfileStatus>) {
        self.available.set(status.is_some());
        *self.active.borrow_mut() = status.and_then(|status| status.active.clone());
        self.sync();
    }

    /// Selects the active profile by name and updates tooltip and sensitivity.
    fn sync(&self) {
        let active = self.active.borrow();
        let index = active
            .as_ref()
            .and_then(|active| {
                self.ctx
                    .profiles
                    .borrow()
                    .iter()
                    .position(|profile| profile.name == active.name)
            })
            .and_then(|position| u32::try_from(position).ok())
            .unwrap_or(gtk::INVALID_LIST_POSITION);
        self.syncing.set(true);
        self.dropdown.set_selected(index);
        self.syncing.set(false);
        self.dropdown
            .set_sensitive(self.available.get() && self.model.n_items() > 0);
        let changed = active.as_ref().is_some_and(|active| active.modified);
        self.caption.set_label(if changed {
            "Profile (changed)"
        } else {
            "Profile"
        });
        let tooltip = active
            .as_ref()
            .map(|active| format!("Active {}", reason_text(&active.reason)));
        self.dropdown.set_tooltip_text(tooltip.as_deref());
    }

    /// The user picked an entry: activate it.
    fn selected(&self) {
        if self.syncing.get() {
            return;
        }
        let slot = self.dropdown.selected() as usize;
        let name = self.ctx.profiles.borrow().get(slot).map(|p| p.name.clone());
        match name {
            Some(name) => self.ctx.send(
                Request::ProfileActivate(ProfileNameParams { name }),
                "Profile activated",
            ),
            None => self.sync(),
        }
    }
}
