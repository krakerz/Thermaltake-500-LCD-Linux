//! Display > Static: one file shown on the panel, with Apply and Clear Panel.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use tt500_lcd_ipc::{BrightnessParams, DEFAULT_BRIGHTNESS, DEFAULT_FPS, MAX_FPS, Request, Status};

use super::context::{Context, page_scroller, weak_call};
use super::file_source::FileSource;
use super::icons::{self, icon};
use crate::upload::Accept;

struct Inner {
    ctx: Rc<Context>,
    file: FileSource,
    fps_row: adw::SpinRow,
    apply_button: gtk::Button,
    /// Set while code (not the user) moves the fps row, so its handler stays quiet.
    syncing: Cell<bool>,
    /// The last file opened on the daemon's behalf, so a failure is not retried on every status.
    auto_loaded: RefCell<Option<PathBuf>>,
    /// Resets the sidebar brightness control.
    reset_brightness: Box<dyn Fn()>,
}

/// The Static page. Cloning shares it.
#[derive(Clone)]
pub(super) struct StaticPage {
    inner: Rc<Inner>,
    scroller: gtk::ScrolledWindow,
}

impl StaticPage {
    /// `reset_brightness` moves the global brightness control back to its default.
    pub fn new(ctx: &Rc<Context>, reset_brightness: impl Fn() + 'static) -> Self {
        let file = FileSource::new(ctx, "Drop an image, GIF or video here", Accept::Any);
        file.follow_daemon();

        let fps_row = adw::SpinRow::new(
            Some(&gtk::Adjustment::new(
                f64::from(DEFAULT_FPS),
                1.0,
                f64::from(MAX_FPS),
                1.0,
                5.0,
                0.0,
            )),
            1.0,
            0,
        );
        fps_row.set_title("Frame rate");
        fps_row.set_subtitle("Videos and animations, in fps (up to 30).");
        fps_row.add_prefix(&gtk::Image::from_icon_name(icon(icons::FPS)));
        fps_row.set_sensitive(false);
        file.picture_group().add(&fps_row);

        let clear_button = gtk::Button::builder()
            .label("Clear Panel")
            .css_classes(["destructive-action"])
            .build();
        let apply_button = gtk::Button::builder()
            .label("Apply")
            .css_classes(["suggested-action"])
            .sensitive(false)
            .build();
        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk::Align::End)
            .build();
        buttons.append(&clear_button);
        buttons.append(&apply_button);

        let scroller = page_scroller(&[file.content().upcast_ref(), buttons.upcast_ref()]);
        let inner = Rc::new(Inner {
            ctx: Rc::clone(ctx),
            file,
            fps_row,
            apply_button,
            syncing: Cell::new(false),
            auto_loaded: RefCell::new(None),
            reset_brightness: Box::new(reset_brightness),
        });
        let weak = Rc::downgrade(&inner);
        let handler = weak_call(&weak, |inner, ()| inner.draft_changed());
        inner.file.connect_changed(move || handler(()));
        let handler = weak_call(&weak, |inner, ()| inner.file_closed());
        inner.file.connect_closed(move || handler(()));
        let handler = weak_call(&weak, |inner, ()| inner.fps_changed());
        inner.fps_row.connect_value_notify(move |_| handler(()));
        let handler = weak_call(&weak, |inner, ()| inner.apply());
        inner.apply_button.connect_clicked(move |_| handler(()));
        let handler = weak_call(&weak, |inner, ()| inner.clear_panel());
        clear_button.connect_clicked(move |_| handler(()));
        Self { inner, scroller }
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.scroller
    }

    /// Clears the panel, resets its brightness and drops the draft (the Clear Panel button).
    pub fn clear_panel(&self) {
        self.inner.clear_panel();
    }

    pub fn choose_file(&self) {
        self.inner.file.choose_file();
    }

    pub fn stop(&self) {
        self.inner.file.stop();
    }

    /// Reloads the daemon's content into the editor (after a profile was activated).
    pub fn readopt(&self) {
        self.inner.file.drop_file();
        *self.inner.auto_loaded.borrow_mut() = None;
    }

    /// Lays the editor out for the panel at rotation `step`.
    pub fn set_rotation(&self, step: u8) {
        self.inner.file.set_rotation(step);
    }

    /// Redraws what depends on the daemon's status.
    pub fn refresh(&self, status: Option<&Status>) {
        let inner = &self.inner;
        inner
            .apply_button
            .set_sensitive(inner.file.draft().is_some() && status.is_some());
        if let Some(status) = status {
            inner.maybe_open_current(status);
        }
    }
}

impl Inner {
    /// Opens the file the daemon is showing, once, when the page has no draft yet.
    fn maybe_open_current(&self, status: &Status) {
        let Some(path) = status.content.as_ref().and_then(|c| c.path.clone()) else {
            return;
        };
        if self.file.draft().is_some()
            || self.file.is_loading()
            || self.auto_loaded.borrow().as_deref() == Some(path.as_path())
            || !path.exists()
        {
            return;
        }
        *self.auto_loaded.borrow_mut() = Some(path.clone());
        self.file.load_file(path);
    }

    /// Moves the fps row and Apply to the draft.
    fn draft_changed(&self) {
        let draft = self.file.draft();
        self.syncing.set(true);
        match &draft {
            Some(draft) => {
                self.fps_row.set_value(f64::from(draft.fps));
                self.fps_row.set_sensitive(draft.uses_fps());
            }
            None => {
                self.fps_row.set_value(f64::from(DEFAULT_FPS));
                self.fps_row.set_sensitive(false);
            }
        }
        self.syncing.set(false);
        self.apply_button
            .set_sensitive(draft.is_some() && self.ctx.daemon_running());
    }

    /// Closes the file without touching the panel, and keeps the daemon's file from reopening.
    fn file_closed(&self) {
        let current = self
            .ctx
            .status
            .borrow()
            .as_ref()
            .and_then(|s| s.content.as_ref())
            .and_then(|c| c.path.clone());
        *self.auto_loaded.borrow_mut() = current;
    }

    fn fps_changed(&self) {
        if self.syncing.get() {
            return;
        }
        let fps = self.fps_row.value().round().clamp(1.0, f64::from(MAX_FPS));
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to 1..=30"
        )]
        let fps = fps as u32;
        self.file.set_fps(fps);
    }

    fn apply(&self) {
        let (Some(draft), Some(framed)) = (self.file.draft(), self.file.framed()) else {
            return;
        };
        self.ctx.send(
            Request::Show(draft.show_params(&framed)),
            "Sent to the panel",
        );
    }

    /// Clears the panel, resets its brightness and drops the draft.
    fn clear_panel(&self) {
        self.file.drop_file();
        *self.auto_loaded.borrow_mut() = None;
        (self.reset_brightness)();
        self.ctx.send_all(
            vec![
                Request::Clear,
                Request::SetBrightness(BrightnessParams {
                    value: DEFAULT_BRIGHTNESS,
                }),
            ],
            "Panel cleared",
        );
    }
}
