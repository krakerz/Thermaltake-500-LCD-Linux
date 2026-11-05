//! Profiles: saved display setups, activated by hand, by schedule or by a running process.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio};
use tt500_lcd_ipc::{
    ActiveProfile, Builtin, Profile, ProfileNameParams, ProfileRenameParams, ProfileReorderParams,
    ProfileSaveParams, Request, Status, validate_name,
};

use super::context::{Context, page_scroller, weak_call};
use super::icons::{self, icon};
use super::trigger_editor::TriggerEditor;
use crate::profiles::{reordered, row_subtitle, source_summary};

/// CSS classes of the line drawn on the edge a dragged row would land on.
const DROP_BEFORE: &str = "tt5-drop-before";
const DROP_AFTER: &str = "tt5-drop-after";
const DROP_CSS: &str = ".tt5-drop-before { box-shadow: 0 -2px 0 0 @accent_bg_color; } \
                       .tt5-drop-after { box-shadow: 0 2px 0 0 @accent_bg_color; }";

const RESET_BODY: &str =
    "Clears the panel, sets brightness to 100 %, rotation to 0 and removes its triggers.";

const SAVE_BODY: &str = "Keeps the content, framing, frame rate, brightness and rotation.";

/// One profile's row and what refreshing it needs.
struct RowParts {
    row: adw::ExpanderRow,
    check: gtk::Image,
    editor: TriggerEditor,
    profile: Profile,
}

struct Inner {
    ctx: Rc<Context>,
    group: adw::PreferencesGroup,
    stack: gtk::Stack,
    empty: adw::StatusPage,
    save_button: gtk::Button,
    problem_group: adw::PreferencesGroup,
    problem_row: adw::ActionRow,
    rows: RefCell<Vec<RowParts>>,
    /// `Some(true)` when the daemon reports profiles, `Some(false)` when it is too old,
    /// `None` before the first status.
    supported: Cell<Option<bool>>,
    daemon_up: Cell<bool>,
    active: RefCell<Option<ActiveProfile>>,
}

/// The Profiles page. Cloning shares it.
#[derive(Clone)]
pub(super) struct ProfilesPage {
    inner: Rc<Inner>,
    stack: adw::ViewStack,
}

impl ProfilesPage {
    pub fn new(ctx: &Rc<Context>) -> Self {
        install_css();
        let save_button = gtk::Button::builder()
            .label("Save Current…")
            .css_classes(["suggested-action"])
            .valign(gtk::Align::Center)
            .sensitive(false)
            .build();
        let header = adw::PreferencesGroup::builder()
            .title("Profiles")
            .description(
                "Saved display setups. Each can switch on at set times or while a program runs.",
            )
            .header_suffix(&save_button)
            .build();
        let problem_row = adw::ActionRow::builder()
            .title("Profile problem")
            .use_markup(false)
            .subtitle_lines(0)
            .build();
        problem_row.add_prefix(&gtk::Image::from_icon_name(icon(icons::WARNING)));
        let problem_group = adw::PreferencesGroup::builder().visible(false).build();
        problem_group.add(&problem_row);
        let group = adw::PreferencesGroup::new();
        let empty = adw::StatusPage::builder()
            .icon_name(icon(icons::PROFILES))
            .title("Profiles unavailable")
            .build();
        let list_stack = gtk::Stack::builder().vhomogeneous(false).build();
        list_stack.add_named(&group, Some("list"));
        list_stack.add_named(&empty, Some("empty"));
        list_stack.set_visible_child_name("empty");

        let scroller = page_scroller(&[
            header.upcast_ref(),
            problem_group.upcast_ref(),
            list_stack.upcast_ref(),
        ]);
        let stack = adw::ViewStack::new();
        stack.add_titled_with_icon(
            &scroller,
            Some("profiles"),
            "Profiles",
            icon(icons::PROFILES),
        );

        let inner = Rc::new(Inner {
            ctx: Rc::clone(ctx),
            group,
            stack: list_stack,
            empty,
            save_button,
            problem_group,
            problem_row,
            rows: RefCell::new(Vec::new()),
            supported: Cell::new(None),
            daemon_up: Cell::new(false),
            active: RefCell::new(None),
        });
        let weak = Rc::downgrade(&inner);
        let handler = weak_call(&weak, |inner, ()| inner.ask_save());
        inner.save_button.connect_clicked(move |_| handler(()));
        inner.update_visibility();
        Self { inner, stack }
    }

    /// The page, for the sidebar's `PageEntry.views`.
    pub fn widget(&self) -> &adw::ViewStack {
        &self.stack
    }

    /// Redraws the list from `ctx.profiles` (after `Context::reload_profiles`).
    pub fn set_profiles(&self) {
        self.inner.set_profiles();
    }

    /// Follows the daemon: active marker, problem line, availability. `None` = daemon down.
    pub fn refresh(&self, status: Option<&Status>) {
        let inner = &self.inner;
        inner.daemon_up.set(status.is_some());
        if let Some(status) = status {
            inner.supported.set(Some(status.profile.is_some()));
            let profile = status.profile.as_ref();
            *inner.active.borrow_mut() = profile.and_then(|p| p.active.clone());
            let problem = profile.and_then(|p| p.problem.as_deref());
            inner.problem_row.set_subtitle(problem.unwrap_or_default());
            inner.problem_group.set_visible(problem.is_some());
        }
        inner.update_active();
        inner.update_visibility();
    }
}

impl Inner {
    fn names(&self) -> Vec<String> {
        self.ctx
            .profiles
            .borrow()
            .iter()
            .map(|profile| profile.name.clone())
            .collect()
    }

    fn set_profiles(self: &Rc<Self>) {
        let profiles = self.ctx.profiles.borrow().clone();
        if self.matches_rows(&profiles) {
            // Nothing visible changed (an echo of this page's own trigger edits): keep the
            // rows, and with them the editors the user may be typing in.
            for (parts, profile) in self.rows.borrow_mut().iter_mut().zip(&profiles) {
                parts.profile = profile.clone();
            }
            self.update_active();
            return;
        }
        let expanded: Vec<String> = self
            .rows
            .borrow()
            .iter()
            .filter(|parts| parts.row.is_expanded())
            .map(|parts| parts.profile.name.clone())
            .collect();
        for parts in self.rows.borrow_mut().drain(..) {
            self.group.remove(&parts.row);
        }
        let count = profiles.len();
        for (index, profile) in profiles.into_iter().enumerate() {
            let parts = self.build_row(index, count, profile);
            parts
                .row
                .set_expanded(expanded.contains(&parts.profile.name));
            self.group.add(&parts.row);
            self.rows.borrow_mut().push(parts);
        }
        self.update_active();
        self.update_visibility();
    }

    /// Whether the rows already show `profiles`: same names, order, specs and editor contents.
    fn matches_rows(&self, profiles: &[Profile]) -> bool {
        let rows = self.rows.borrow();
        rows.len() == profiles.len()
            && rows.iter().zip(profiles).all(|(parts, profile)| {
                parts.profile.name == profile.name
                    && parts.profile.spec == profile.spec
                    && parts.editor.triggers() == profile.triggers
            })
    }

    fn build_row(self: &Rc<Self>, index: usize, count: usize, profile: Profile) -> RowParts {
        let weak = Rc::downgrade(self);
        let row = adw::ExpanderRow::builder()
            .title(&profile.name)
            .use_markup(false)
            .build();
        let handle = gtk::Image::builder()
            .icon_name(icon(icons::HANDLE))
            .tooltip_text("Drag to reorder")
            .css_classes(["dim-label"])
            .build();
        row.add_prefix(&handle);
        let check = gtk::Image::from_icon_name(icon(icons::APPLY));
        check.set_visible(false);
        row.add_prefix(&check);
        self.add_drag_source(&handle, &row, &profile.name);
        self.add_drop_target(&row, &profile.name);
        let builtin = profile.builtin == Some(Builtin::Default);
        row.add_suffix(&self.move_menu(index, count, &profile.name, builtin));

        let content = adw::ActionRow::builder()
            .title("Shows")
            .subtitle(source_summary(&profile.spec))
            .use_markup(false)
            .subtitle_lines(0)
            .build();
        row.add_row(&content);
        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .halign(gtk::Align::End)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        let actions: &[(&str, bool)] = if builtin {
            &[("Activate", false)]
        } else {
            &[("Rename…", false), ("Delete…", true), ("Activate", false)]
        };
        for &(title, destructive) in actions {
            let action = gtk::Button::builder().label(title).build();
            if destructive {
                action.add_css_class("destructive-action");
            }
            let name = profile.name.clone();
            let handler = weak_call(&weak, move |inner, ()| match title {
                "Activate" => inner.ctx.send(
                    Request::ProfileActivate(ProfileNameParams { name: name.clone() }),
                    "Profile activated",
                ),
                "Rename…" => inner.ask_rename(&name),
                _ => inner.ask_delete(&name),
            });
            action.connect_clicked(move |_| handler(()));
            buttons.append(&action);
        }
        let actions_row = adw::PreferencesRow::builder()
            .activatable(false)
            .child(&buttons)
            .build();
        row.add_row(&actions_row);
        let editor = TriggerEditor::new(&self.ctx, &profile.name, &profile.triggers);
        row.add_row(editor.widget());
        RowParts {
            row,
            check,
            editor,
            profile,
        }
    }

    /// Marks the active profile and rewrites each subtitle.
    fn update_active(&self) {
        let active = self.active.borrow();
        for parts in self.rows.borrow().iter() {
            let is_active = active
                .as_ref()
                .filter(|active| active.name == parts.profile.name);
            parts.check.set_visible(is_active.is_some());
            let mut subtitle = row_subtitle(
                &parts.profile.triggers,
                is_active.map(|active| (&active.reason, active.modified)),
            );
            if parts.profile.builtin == Some(Builtin::Default) {
                subtitle = format!("Built-in · {subtitle}");
            }
            parts.row.set_subtitle(&subtitle);
        }
    }

    fn update_visibility(&self) {
        let ready = self.supported.get() == Some(true);
        self.save_button
            .set_sensitive(ready && self.daemon_up.get());
        if ready && !self.rows.borrow().is_empty() {
            self.stack.set_visible_child_name("list");
            return;
        }
        match self.supported.get() {
            Some(true) => {
                self.empty.set_title("No profiles yet");
                self.empty.set_description(Some(
                    "Set up the display, then choose Save Current to keep it as a profile.",
                ));
            }
            Some(false) => {
                self.empty.set_title("tt500-lcdd is too old for profiles");
                self.empty
                    .set_description(Some("Update the daemon to use profiles."));
            }
            None => {
                self.empty.set_title("Profiles unavailable");
                self.empty.set_description(Some("Waiting for tt500-lcdd."));
            }
        }
        self.stack.set_visible_child_name("empty");
    }

    // Reordering

    /// Sends the order with the profile named `from` moved before `slot`.
    fn reorder(&self, from: &str, slot: usize) {
        let names = self.names();
        let Some(index) = names.iter().position(|name| name == from) else {
            return;
        };
        let order = reordered(&names, index, slot);
        if order != names {
            self.ctx.send(
                Request::ProfileReorder(ProfileReorderParams { names: order }),
                "",
            );
        }
    }

    fn add_drag_source(&self, handle: &gtk::Image, row: &adw::ExpanderRow, name: &str) {
        let source = gtk::DragSource::builder()
            .actions(gdk::DragAction::MOVE)
            .build();
        let name = name.to_owned();
        source.connect_prepare(move |_, _, _| {
            Some(gdk::ContentProvider::for_value(&name.to_value()))
        });
        let row = row.clone();
        source.connect_drag_begin(move |_, drag| {
            let paintable = gtk::WidgetPaintable::new(Some(&row));
            gtk::DragIcon::set_from_paintable(drag, &paintable, 0, 0);
        });
        handle.add_controller(source);
    }

    fn add_drop_target(self: &Rc<Self>, row: &adw::ExpanderRow, name: &str) {
        let target = gtk::DropTarget::new(String::static_type(), gdk::DragAction::MOVE);
        {
            let row = row.clone();
            target.connect_motion(move |_, _, y| {
                let after = is_lower_half(y, row.height());
                row.remove_css_class(if after { DROP_BEFORE } else { DROP_AFTER });
                row.add_css_class(if after { DROP_AFTER } else { DROP_BEFORE });
                gdk::DragAction::MOVE
            });
        }
        {
            let row = row.clone();
            target.connect_leave(move |_| clear_drop_marks(&row));
        }
        let weak = Rc::downgrade(self);
        let (row_for_drop, name) = (row.clone(), name.to_owned());
        target.connect_drop(move |_, value, _, y| {
            clear_drop_marks(&row_for_drop);
            let (Some(inner), Ok(from)) = (weak.upgrade(), value.get::<String>()) else {
                return false;
            };
            let Some(index) = inner.names().iter().position(|n| *n == name) else {
                return false;
            };
            let slot = index + usize::from(is_lower_half(y, row_for_drop.height()));
            inner.reorder(&from, slot);
            true
        });
        row.add_controller(target);
    }

    /// "Move up" / "Move down" menu for keyboard users, disabled at the ends of the list.
    fn move_menu(
        self: &Rc<Self>,
        index: usize,
        count: usize,
        name: &str,
        builtin: bool,
    ) -> gtk::MenuButton {
        let menu = gio::Menu::new();
        menu.append(Some("Move up"), Some("row.move-up"));
        menu.append(Some("Move down"), Some("row.move-down"));
        let group = gio::SimpleActionGroup::new();
        if builtin {
            menu.append(Some("Reset to Defaults"), Some("row.reset"));
            let action = gio::SimpleAction::new("reset", None);
            let weak = Rc::downgrade(self);
            let name = name.to_owned();
            action.connect_activate(move |_, _| {
                if let Some(inner) = weak.upgrade() {
                    inner.ask_reset(&name);
                }
            });
            group.add_action(&action);
        }
        for (action_name, enabled, slot) in [
            ("move-up", index > 0, index.saturating_sub(1)),
            ("move-down", index + 1 < count, index + 2),
        ] {
            let action = gio::SimpleAction::new(action_name, None);
            action.set_enabled(enabled);
            let weak = Rc::downgrade(self);
            let name = name.to_owned();
            action.connect_activate(move |_, _| {
                if let Some(inner) = weak.upgrade() {
                    inner.reorder(&name, slot);
                }
            });
            group.add_action(&action);
        }
        let button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .menu_model(&menu)
            .tooltip_text("Move or reset")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        button.insert_action_group("row", Some(&group));
        button
    }

    // Dialogs

    fn ask_save(self: &Rc<Self>) {
        let existing = self.names();
        let dialog = adw::AlertDialog::new(Some("Save current display"), Some(SAVE_BODY));
        let entry = adw::EntryRow::builder().title("Profile name").build();
        dialog.set_extra_child(Some(&entry_list(&entry)));
        dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled("save", false);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        {
            let dialog = dialog.clone();
            entry.connect_changed(move |entry| {
                let name = entry.text().trim().to_lowercase();
                dialog.set_response_enabled("save", !name.is_empty());
                let exists = existing.iter().any(|n| n.to_lowercase() == name);
                dialog.set_body(if exists {
                    "A profile with this name exists. It is replaced; its triggers are kept."
                } else {
                    SAVE_BODY
                });
            });
        }
        {
            let dialog = dialog.clone();
            entry.connect_entry_activated(move |_| {
                if dialog.is_response_enabled("save") {
                    dialog.close();
                }
            });
        }
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            let (Some(inner), "save") = (weak.upgrade(), response) else {
                return;
            };
            match validate_name(&entry.text()) {
                Ok(name) => inner.ctx.send(
                    Request::ProfileSave(ProfileSaveParams { name, spec: None }),
                    "Profile saved",
                ),
                Err(message) => inner.ctx.toast(&message),
            }
        });
        dialog.present(Some(&self.ctx.window));
    }

    fn ask_rename(self: &Rc<Self>, from: &str) {
        let dialog = adw::AlertDialog::new(Some("Rename profile"), None);
        let entry = adw::EntryRow::builder()
            .title("New name")
            .text(from)
            .build();
        dialog.set_extra_child(Some(&entry_list(&entry)));
        dialog.add_responses(&[("cancel", "Cancel"), ("rename", "Rename")]);
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");
        {
            let dialog = dialog.clone();
            entry.connect_entry_activated(move |_| {
                dialog.close();
            });
        }
        let weak = Rc::downgrade(self);
        let from = from.to_owned();
        dialog.connect_response(None, move |_, response| {
            let (Some(inner), "rename") = (weak.upgrade(), response) else {
                return;
            };
            match validate_name(&entry.text()) {
                Ok(to) if to == from => {}
                Ok(to) => inner.ctx.send(
                    Request::ProfileRename(ProfileRenameParams {
                        from: from.clone(),
                        to,
                    }),
                    "Profile renamed",
                ),
                Err(message) => inner.ctx.toast(&message),
            }
        });
        dialog.present(Some(&self.ctx.window));
    }

    fn ask_reset(self: &Rc<Self>, name: &str) {
        let dialog = adw::AlertDialog::new(Some("Reset to defaults?"), Some(RESET_BODY));
        dialog.add_responses(&[("cancel", "Cancel"), ("reset", "Reset")]);
        dialog.set_response_appearance("reset", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        let name = name.to_owned();
        dialog.connect_response(None, move |_, response| {
            if let (Some(inner), "reset") = (weak.upgrade(), response) {
                inner.ctx.send(
                    Request::ProfileReset(ProfileNameParams { name: name.clone() }),
                    "Reset to defaults",
                );
            }
        });
        dialog.present(Some(&self.ctx.window));
    }

    fn ask_delete(self: &Rc<Self>, name: &str) {
        let dialog = adw::AlertDialog::new(
            Some("Delete profile?"),
            Some(&format!("“{name}” and its triggers are removed.")),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        let name = name.to_owned();
        dialog.connect_response(None, move |_, response| {
            if let (Some(inner), "delete") = (weak.upgrade(), response) {
                inner.ctx.send(
                    Request::ProfileDelete(ProfileNameParams { name: name.clone() }),
                    "Profile deleted",
                );
            }
        });
        dialog.present(Some(&self.ctx.window));
    }
}

/// A boxed list holding `entry`, as a dialog's extra child.
fn entry_list(entry: &adw::EntryRow) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(entry);
    list
}

/// Whether `y` is in the lower half of a row `height` tall: the drop lands after it.
fn is_lower_half(y: f64, height: i32) -> bool {
    y > f64::from(height) / 2.0
}

fn clear_drop_marks(row: &adw::ExpanderRow) {
    row.remove_css_class(DROP_BEFORE);
    row.remove_css_class(DROP_AFTER);
}

/// Adds the drop-indicator styles to the display.
fn install_css() {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(DROP_CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_slot_half() {
        assert!(!is_lower_half(10.0, 100));
        assert!(!is_lower_half(50.0, 100));
        assert!(is_lower_half(51.0, 100));
        assert!(!is_lower_half(0.0, 0));
    }

    #[test]
    fn drop_css_names_both_marks() {
        assert!(DROP_CSS.contains(DROP_BEFORE) && DROP_CSS.contains(DROP_AFTER));
    }
}
