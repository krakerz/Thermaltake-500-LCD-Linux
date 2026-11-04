//! A file picked from disk (button or drag and drop), decoded, previewed and cropped.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use super::context::{Context, weak_call};
use super::crop_panel::CropPanel;
use super::icons::{self, icon};
use super::player::Player;
use crate::media;
use crate::state::{Draft, Framed};
use crate::upload::Accept;

pub(super) const CHOOSE_FILE: &str = "Choose File…";

struct Inner {
    ctx: Rc<Context>,
    accept: Accept,
    panel: CropPanel,
    content: gtk::Box,
    file_row: adw::ActionRow,
    close_button: gtk::Button,
    player: Player,
    draft: RefCell<Option<Draft>>,
    loading: Cell<bool>,
    /// Bumped when the draft is dropped, so an in-flight load is ignored.
    load_generation: Cell<u64>,
    /// Whether a file the daemon is already showing takes its framing and fps.
    follows_daemon: Cell<bool>,
    changed: RefCell<Option<Rc<dyn Fn()>>>,
    closed: RefCell<Option<Rc<dyn Fn()>>>,
}

/// A [`CropPanel`] fed by a file. Cloning shares the source.
#[derive(Clone)]
pub(super) struct FileSource {
    inner: Rc<Inner>,
}

impl FileSource {
    pub fn new(ctx: &Rc<Context>, empty_title: &str, accept: Accept) -> Self {
        let open_button = gtk::Button::builder()
            .label(CHOOSE_FILE)
            .halign(gtk::Align::Center)
            .css_classes(["suggested-action", "pill"])
            .build();
        let panel = CropPanel::new(empty_title, &open_button);

        let header_open = gtk::Button::builder()
            .icon_name(icon(icons::OPEN))
            .tooltip_text("Open a file")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let close_button = gtk::Button::builder()
            .icon_name(icon(icons::REMOVE))
            .tooltip_text("Close the file (the panel keeps showing it)")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .visible(false)
            .build();
        let file_row = adw::ActionRow::builder()
            .title("File")
            .subtitle("No file")
            .subtitle_lines(1)
            .build();
        file_row.add_suffix(&header_open);
        file_row.add_suffix(&close_button);
        let file_group = adw::PreferencesGroup::new();
        file_group.add(&file_row);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        content.append(panel.widget());
        content.append(&file_group);
        content.append(panel.picture_group());

        let player = Player::new(panel.editor().clone(), &ctx.window);
        let inner = Rc::new(Inner {
            ctx: Rc::clone(ctx),
            accept,
            panel,
            content,
            file_row,
            close_button,
            player,
            draft: RefCell::new(None),
            loading: Cell::new(false),
            load_generation: Cell::new(0),
            follows_daemon: Cell::new(false),
            changed: RefCell::new(None),
            closed: RefCell::new(None),
        });
        let weak = Rc::downgrade(&inner);
        for button in [&open_button, &header_open] {
            let open = weak_call(&weak, |inner, ()| inner.choose_file());
            button.connect_clicked(move |_| open(()));
        }
        let close = weak_call(&weak, |inner, ()| {
            inner.drop_file();
            inner.emit(&inner.closed);
        });
        inner.close_button.connect_clicked(move |_| close(()));

        let drop_target = gtk::DropTarget::new(gio::File::static_type(), gdk::DragAction::COPY);
        let load = weak_call(&weak, |inner: &Rc<Inner>, path: PathBuf| {
            inner.load_file(path)
        });
        drop_target.connect_drop(move |_, value, _, _| {
            match value.get::<gio::File>().ok().and_then(|file| file.path()) {
                Some(path) => {
                    load(path);
                    true
                }
                None => false,
            }
        });
        inner.panel.widget().add_controller(drop_target);
        Self { inner }
    }

    /// Editor, file row and picture controls, stacked.
    pub fn content(&self) -> &gtk::Box {
        &self.inner.content
    }

    /// The "Picture" group, so a page can add rows to it.
    pub fn picture_group(&self) -> &adw::PreferencesGroup {
        self.inner.panel.picture_group()
    }

    /// A copy of the loaded file's draft.
    pub fn draft(&self) -> Option<Draft> {
        self.inner.draft.borrow().clone()
    }

    pub fn framed(&self) -> Option<Framed> {
        self.inner.panel.framed()
    }

    /// Lays the editor out for the panel at rotation `step`.
    pub fn set_rotation(&self, step: u8) {
        self.inner.panel.set_rotation(step);
    }

    /// Takes framing and fps from the daemon's content when it shows the file just opened.
    pub fn follow_daemon(&self) {
        self.inner.follows_daemon.set(true);
    }

    /// Calls `callback` when a file is loaded or dropped.
    pub fn connect_changed(&self, callback: impl Fn() + 'static) {
        *self.inner.changed.borrow_mut() = Some(Rc::new(callback));
    }

    /// Calls `callback` after the user closes the file.
    pub fn connect_closed(&self, callback: impl Fn() + 'static) {
        *self.inner.closed.borrow_mut() = Some(Rc::new(callback));
    }

    /// Sets the frame-rate cap of the loaded file and of its preview.
    pub fn set_fps(&self, fps: u32) {
        if let Some(draft) = self.inner.draft.borrow_mut().as_mut() {
            draft.fps = fps;
            self.inner.player.set_fps_cap(fps);
        }
    }

    /// Whether a file is being decoded.
    pub fn is_loading(&self) -> bool {
        self.inner.loading.get()
    }

    pub fn choose_file(&self) {
        self.inner.choose_file();
    }

    pub fn load_file(&self, path: PathBuf) {
        self.inner.load_file(path);
    }

    /// Drops the draft and the preview; the panel is not touched.
    pub fn drop_file(&self) {
        self.inner.drop_file();
    }

    /// Stops the preview playback.
    pub fn stop(&self) {
        self.inner.player.stop();
    }
}

impl Inner {
    fn emit(&self, slot: &RefCell<Option<Rc<dyn Fn()>>>) {
        let callback = slot.borrow().clone();
        if let Some(callback) = callback {
            callback();
        }
    }

    fn choose_file(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        let title = match self.accept {
            Accept::Any => {
                filter.set_name(Some("Images and videos"));
                filter.add_mime_type("image/*");
                filter.add_mime_type("video/*");
                "Open an image, GIF or video"
            }
            Accept::Still => {
                filter.set_name(Some("Images"));
                filter.add_mime_type("image/*");
                "Open a still image"
            }
            Accept::Animation => {
                filter.set_name(Some("Animated images"));
                for mime in ["image/gif", "image/apng", "image/png", "image/webp"] {
                    filter.add_mime_type(mime);
                }
                "Open an animated GIF, PNG or WebP"
            }
        };
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title(title)
            .filters(&filters)
            .default_filter(&filter)
            .build();
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            match dialog.open_future(Some(&this.ctx.window)).await {
                Ok(file) => match file.path() {
                    Some(path) => this.load_file(path),
                    None => this.ctx.toast("Only local files can be shown"),
                },
                Err(error) if error.matches(gtk::DialogError::Dismissed) => {}
                Err(error) => this
                    .ctx
                    .toast(&format!("Cannot open the file dialog: {error}")),
            }
        });
    }

    fn load_file(self: &Rc<Self>, path: PathBuf) {
        if self.loading.replace(true) {
            return;
        }
        let generation = self.load_generation.get();
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let worker_path = path.clone();
            let result = gio::spawn_blocking(move || media::load(&worker_path)).await;
            this.loading.set(false);
            match result {
                Ok(Ok(source)) if !this.accept.allows(source.kind) => {
                    this.ctx
                        .toast(&format!("{}: {}", file_name(&path), this.accept.refusal()))
                }
                Ok(Ok(source)) => {
                    if this.load_generation.get() == generation {
                        this.set_source(path, &source);
                    }
                }
                Ok(Err(error)) => this.ctx.toast(&format!("{}: {error}", file_name(&path))),
                Err(_) => this
                    .ctx
                    .toast(&format!("{}: loading failed", file_name(&path))),
            }
        });
    }

    fn set_source(&self, path: PathBuf, source: &media::Source) {
        let mut draft = Draft::new(path, source.kind);
        self.panel.set_source(source);
        if self.follows_daemon.get() {
            let status = self.ctx.status.borrow();
            let info = status
                .as_ref()
                .and_then(|s| s.content.as_ref())
                .filter(|info| info.path.as_deref() == Some(draft.path.as_path()));
            if let Some(info) = info {
                draft.adopt(info);
                self.panel.adopt(info);
            }
        }
        self.player.play(&draft.path, source, draft.fps);
        self.file_row.set_subtitle(&file_name(&draft.path));
        *self.draft.borrow_mut() = Some(draft);
        self.close_button.set_visible(true);
        self.emit(&self.changed);
    }

    fn drop_file(&self) {
        self.player.stop();
        self.load_generation.set(self.load_generation.get() + 1);
        *self.draft.borrow_mut() = None;
        self.panel.clear();
        self.file_row.set_subtitle("No file");
        self.close_button.set_visible(false);
        self.emit(&self.changed);
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}
