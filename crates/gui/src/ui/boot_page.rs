//! Boot: the standby image and the boot animation, written to the panel's flash.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use tt500_lcd_ipc::Request;

use super::context::{Context, UPLOAD_TOAST_TIMEOUT_SECS, page_scroller, weak_call};
use super::file_source::FileSource;
use super::icons::{self, icon};
use crate::upload::{self, UploadKind};

/// One tab: a file source and the button that writes it to the flash.
struct FlashTab {
    kind: UploadKind,
    source: FileSource,
    write_button: gtk::Button,
    /// Shown while this tab's write is running.
    spinner: gtk::Spinner,
}

impl FlashTab {
    fn new(ctx: &Rc<Context>, kind: UploadKind) -> (Self, gtk::ScrolledWindow) {
        let (empty_title, row_title, row_subtitle) = match kind {
            UploadKind::Standby => (
                "Drop a still image here",
                "Standby image",
                "Shown while the PC is off or asleep",
            ),
            UploadKind::BootAnimation => (
                "Drop an animated GIF, PNG or WebP here",
                "Boot animation",
                "Plays at power-on",
            ),
        };
        let source = FileSource::new(ctx, empty_title, kind.accept());
        let about = adw::ActionRow::builder()
            .title(row_title)
            .subtitle(row_subtitle)
            .build();
        let about_group = adw::PreferencesGroup::new();
        about_group.add(&about);
        let write_button = gtk::Button::builder()
            .label("Write to Panel…")
            .css_classes(["destructive-action"])
            .sensitive(false)
            .build();
        let spinner = gtk::Spinner::builder().visible(false).build();
        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk::Align::End)
            .build();
        buttons.append(&spinner);
        buttons.append(&write_button);
        let scroller = page_scroller(&[
            about_group.upcast_ref(),
            source.content().upcast_ref(),
            buttons.upcast_ref(),
        ]);
        (
            Self {
                kind,
                source,
                write_button,
                spinner,
            },
            scroller,
        )
    }
}

struct Inner {
    ctx: Rc<Context>,
    tabs: [FlashTab; 2],
    /// Set from the confirmed write until its reply; blocks a second upload.
    uploading: Cell<bool>,
}

/// The Boot page. Cloning shares it.
#[derive(Clone)]
pub(super) struct BootPage {
    inner: Rc<Inner>,
    stack: adw::ViewStack,
}

impl BootPage {
    pub fn new(ctx: &Rc<Context>) -> Self {
        let stack = adw::ViewStack::new();
        let (standby, standby_page) = FlashTab::new(ctx, UploadKind::Standby);
        let (animation, animation_page) = FlashTab::new(ctx, UploadKind::BootAnimation);
        stack.add_titled_with_icon(
            &standby_page,
            Some("standby"),
            "Standby image",
            icon(icons::PANEL_OFF),
        );
        stack.add_titled_with_icon(
            &animation_page,
            Some("animation"),
            "Boot animation",
            icon(icons::FPS),
        );
        let inner = Rc::new(Inner {
            ctx: Rc::clone(ctx),
            tabs: [standby, animation],
            uploading: Cell::new(false),
        });
        let weak = Rc::downgrade(&inner);
        for (index, tab) in inner.tabs.iter().enumerate() {
            let handler = weak_call(&weak, |inner, ()| inner.update_buttons());
            tab.source.connect_changed(move || handler(()));
            let handler = weak_call(&weak, move |inner, ()| inner.confirm_upload(index));
            tab.write_button.connect_clicked(move |_| handler(()));
        }
        Self { inner, stack }
    }

    /// The tabs, for a view switcher.
    pub fn view_stack(&self) -> &adw::ViewStack {
        &self.stack
    }

    /// Lays the editors out for the panel at rotation `step`.
    pub fn set_rotation(&self, step: u8) {
        for tab in &self.inner.tabs {
            tab.source.set_rotation(step);
        }
    }

    pub fn refresh(&self) {
        self.inner.update_buttons();
    }

    pub fn stop(&self) {
        for tab in &self.inner.tabs {
            tab.source.stop();
        }
    }
}

impl Inner {
    /// Write is available for a loaded file of the tab's kind while no write runs.
    fn update_buttons(&self) {
        let running = self.ctx.daemon_running();
        let idle = !self.uploading.get();
        for tab in &self.tabs {
            tab.write_button
                .set_sensitive(idle && tab.kind.enabled(tab.source.draft().as_ref(), running));
        }
    }

    /// Dry-runs the upload off the main thread, then asks before anything is written.
    fn confirm_upload(self: &Rc<Self>, index: usize) {
        let Ok(socket) = self.ctx.socket.clone() else {
            return;
        };
        let tab = &self.tabs[index];
        let kind = tab.kind;
        let (Some(draft), Some(framed)) = (tab.source.draft(), tab.source.framed()) else {
            return;
        };
        // The same framing is written as was previewed, even if the editor changes meanwhile.
        let dry = kind.request(&draft, &framed, true);
        let real = kind.request(&draft, &framed, false);
        self.set_uploading(index, true);
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || upload::run(&socket, dry)).await;
            this.set_uploading(index, false);
            match result {
                Ok(Ok(summary)) => this.ask_write(index, real, &summary.unwrap_or_default()),
                Ok(Err(message)) => this.ctx.toast(&message),
                Err(_) => this.ctx.toast("The request failed"),
            }
        });
    }

    /// The only place a real (`dry_run: false`) upload is sent: after the user pressed Write.
    fn ask_write(self: &Rc<Self>, index: usize, real: Request, summary: &str) {
        let body = format!(
            "{summary}\n\nThis writes to the panel's flash memory, which has a limited \
             number of write cycles."
        );
        let dialog = adw::AlertDialog::new(Some("Write to the panel's flash?"), Some(body.trim()));
        dialog.add_responses(&[("cancel", "Cancel"), ("write", "Write")]);
        dialog.set_response_appearance("write", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let this = Rc::clone(self);
        let real = RefCell::new(Some(real));
        dialog.connect_response(None, move |_, response| {
            if response == "write"
                && let Some(request) = real.borrow_mut().take()
            {
                this.write_flash(index, request);
            }
        });
        dialog.present(Some(&self.ctx.window));
    }

    fn write_flash(self: &Rc<Self>, index: usize, request: Request) {
        let Ok(socket) = self.ctx.socket.clone() else {
            return;
        };
        let kind = self.tabs[index].kind;
        self.set_uploading(index, true);
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || upload::run(&socket, request)).await;
            this.set_uploading(index, false);
            match result {
                Ok(Ok(_)) => this
                    .ctx
                    .toast_for(kind.done_message(), UPLOAD_TOAST_TIMEOUT_SECS),
                Ok(Err(message)) => this.ctx.toast(&message),
                Err(_) => this.ctx.toast("The request failed"),
            }
        });
    }

    /// Shows the tab's spinner and blocks the write buttons while a request runs.
    fn set_uploading(&self, index: usize, uploading: bool) {
        self.uploading.set(uploading);
        let spinner = &self.tabs[index].spinner;
        spinner.set_visible(uploading);
        spinner.set_spinning(uploading);
        self.update_buttons();
    }
}
