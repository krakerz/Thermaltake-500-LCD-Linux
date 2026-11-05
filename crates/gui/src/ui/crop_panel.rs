//! The crop editor with its empty page and the Framing and Zoom rows.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use image::RgbaImage;
use tt500_lcd_ipc::{ContentInfo, Fit, Size};

use super::context::weak_call;
use super::editor::{Editor, surface_from_bgra};
use super::icons::{self, icon};
use crate::geometry::CropView;
use crate::media::{self, premultiplied_bgra};
use crate::state::{Framed, Framing};

/// Entries of the framing row, in model order.
const FRAMINGS: [(&str, Framing); 4] = [
    ("Crop — drag to move, scroll to zoom", Framing::Crop),
    ("Fill", Framing::Fit(Fit::Cover)),
    ("Fit", Framing::Fit(Fit::Contain)),
    ("Stretch", Framing::Fit(Fit::Stretch)),
];

/// Index of `framing` in [`FRAMINGS`].
fn framing_index(framing: Framing) -> usize {
    FRAMINGS
        .iter()
        .position(|(_, candidate)| *candidate == framing)
        .unwrap_or(0)
}

struct Inner {
    editor: Editor,
    stack: gtk::Stack,
    picture: adw::PreferencesGroup,
    framing_row: adw::ComboRow,
    zoom_row: adw::ActionRow,
    zoom_scale: gtk::Scale,
    /// `None` while no source is shown.
    state: RefCell<Option<Framed>>,
    /// Set while code (not the user) moves a control, so handlers stay quiet.
    syncing: Cell<bool>,
}

/// The editor, its empty page and the framing controls. Cloning shares the panel.
#[derive(Clone)]
pub(super) struct CropPanel {
    inner: Rc<Inner>,
}

impl CropPanel {
    /// `empty_title` and `open_button` fill the page shown while there is no source.
    pub fn new(empty_title: &str, open_button: &gtk::Button) -> Self {
        let editor = Editor::new();
        let stack = gtk::Stack::new();
        let empty = adw::StatusPage::builder()
            .icon_name(icon(icons::PANEL))
            .title(empty_title)
            .child(open_button)
            .build();
        stack.add_named(&empty, Some("empty"));
        stack.add_named(editor.widget(), Some("editor"));

        let framing_row = adw::ComboRow::builder()
            .title("Framing")
            .model(&gtk::StringList::new(&FRAMINGS.map(|(label, _)| label)))
            .build();
        let zoom_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 1.0, 2.0, 0.01);
        zoom_scale.set_draw_value(false);
        zoom_scale.set_width_request(200);
        zoom_scale.set_valign(gtk::Align::Center);
        let reset_button = gtk::Button::builder()
            .icon_name(icon(icons::RESET))
            .tooltip_text("Reset zoom and position")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let zoom_row = adw::ActionRow::builder().title("Zoom").build();
        zoom_row.add_prefix(&gtk::Image::from_icon_name(icon(icons::ZOOM_IN)));
        zoom_row.add_suffix(&zoom_scale);
        zoom_row.add_suffix(&reset_button);

        let picture = adw::PreferencesGroup::builder().title("Picture").build();
        picture.add(&framing_row);
        picture.add(&zoom_row);

        let inner = Rc::new(Inner {
            editor,
            stack,
            picture,
            framing_row,
            zoom_row,
            zoom_scale,
            state: RefCell::new(None),
            syncing: Cell::new(false),
        });
        let weak = Rc::downgrade(&inner);
        let handler = weak_call(&weak, |inner, ()| inner.framing_selected());
        inner
            .framing_row
            .connect_selected_notify(move |_| handler(()));
        let handler = weak_call(&weak, |inner, ()| inner.zoom_moved());
        inner.zoom_scale.connect_value_changed(move |_| handler(()));
        inner
            .editor
            .connect_view_changed(weak_call(&weak, |inner, view| inner.view_changed(view)));
        let handler = weak_call(&weak, |inner, ()| inner.reset_view());
        reset_button.connect_clicked(move |_| handler(()));
        Self { inner }
    }

    /// The empty page / editor stack.
    pub fn widget(&self) -> &gtk::Stack {
        &self.inner.stack
    }

    /// The "Picture" group, so a page can add rows to it.
    pub fn picture_group(&self) -> &adw::PreferencesGroup {
        &self.inner.picture
    }

    pub fn editor(&self) -> &Editor {
        &self.inner.editor
    }

    /// Lays the editor out for the panel at rotation `step` (portrait for 1 and 3) and re-centres
    /// the crop for the new shape. The picture stays upright, as on the mounted panel.
    pub fn set_rotation(&self, step: u8) {
        let inner = &self.inner;
        inner.editor.set_rotation(step);
        let framed = {
            let mut state = inner.state.borrow_mut();
            let Some(framed) = state.as_mut() else {
                return;
            };
            framed.set_aspect(inner.editor.aspect());
            *framed
        };
        inner.apply_to_editor(&framed);
    }

    /// Shows `source` with a fresh, centred crop.
    pub fn set_source(&self, source: &media::Source) {
        let inner = &self.inner;
        let framed = Framed::new(source.size, inner.editor.aspect());
        inner.editor.set_source(source);
        *inner.state.borrow_mut() = Some(framed);
        inner.apply_to_editor(&framed);
        inner.stack.set_visible_child_name("editor");
    }

    /// Shows a live frame of a source `size` big. A frame of the size already shown only swaps
    /// the picture, keeping crop and framing; otherwise the editor restarts with a fresh crop.
    /// Returns whether it restarted.
    pub fn set_live(&self, image: RgbaImage, size: Size) -> bool {
        let same_size = self.framed().is_some_and(|framed| framed.size == size);
        if same_size {
            let (width, height) = (image.width(), image.height());
            if let Some(surface) = surface_from_bgra(premultiplied_bgra(&image), width, height) {
                self.inner.editor.set_frame_surface(&surface);
            }
            return false;
        }
        self.set_source(&media::Source {
            image,
            size,
            kind: media::SourceKind::Still,
        });
        true
    }

    /// Takes framing and crop from the daemon's content when it shows this same source.
    pub fn adopt(&self, info: &ContentInfo) {
        let framed = {
            let mut state = self.inner.state.borrow_mut();
            let Some(framed) = state.as_mut() else {
                return;
            };
            framed.adopt(info);
            *framed
        };
        self.inner.apply_to_editor(&framed);
    }

    /// The current framing, `None` while no source is shown.
    pub fn framed(&self) -> Option<Framed> {
        *self.inner.state.borrow()
    }

    /// Back to the empty page with default controls.
    pub fn clear(&self) {
        let inner = &self.inner;
        *inner.state.borrow_mut() = None;
        inner.editor.clear();
        inner.stack.set_visible_child_name("empty");
        inner.syncing.set(true);
        inner.framing_row.set_selected(0);
        inner.zoom_row.set_visible(true);
        inner.zoom_scale.set_range(1.0, 2.0);
        inner.zoom_scale.set_value(1.0);
        inner.syncing.set(false);
    }
}

impl Inner {
    /// Moves the editor and the controls to `framed`.
    fn apply_to_editor(&self, framed: &Framed) {
        self.editor.set_framing(framed.framing);
        self.editor.set_view(framed.view);
        self.syncing.set(true);
        self.framing_row
            .set_selected(u32::try_from(framing_index(framed.framing)).unwrap_or(0));
        self.zoom_row.set_visible(framed.framing == Framing::Crop);
        self.zoom_scale
            .set_range(1.0, framed.view.max_zoom().max(1.01));
        self.zoom_scale.set_value(framed.view.zoom());
        self.syncing.set(false);
    }

    fn framing_selected(&self) {
        if self.syncing.get() {
            return;
        }
        let index = usize::try_from(self.framing_row.selected()).unwrap_or(0);
        let Some((_, framing)) = FRAMINGS.get(index) else {
            return;
        };
        if let Some(framed) = self.state.borrow_mut().as_mut() {
            framed.framing = *framing;
        }
        self.editor.set_framing(*framing);
        self.zoom_row.set_visible(*framing == Framing::Crop);
    }

    fn zoom_moved(&self) {
        if self.syncing.get() {
            return;
        }
        let mut view = self.editor.view();
        view.set_zoom(self.zoom_scale.value());
        self.editor.set_view(view);
        if let Some(framed) = self.state.borrow_mut().as_mut() {
            framed.view = view;
        }
    }

    fn reset_view(&self) {
        let size = match self.state.borrow().as_ref() {
            Some(framed) => framed.size,
            None => return,
        };
        let view = CropView::new(size, self.editor.aspect());
        self.editor.set_view(view);
        self.view_changed(view);
    }

    fn view_changed(&self, view: CropView) {
        if let Some(framed) = self.state.borrow_mut().as_mut() {
            framed.view = view;
        }
        self.syncing.set(true);
        self.zoom_scale.set_value(view.zoom());
        self.syncing.set(false);
    }
}
