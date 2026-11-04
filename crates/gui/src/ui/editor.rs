//! The crop editor: a 480:128 drawing area that is also the live panel preview.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::cairo;
use gtk::prelude::*;
use tt500_lcd_ipc::Size;

use crate::geometry::{CropView, PANEL_ASPECT, fit_rect};
use crate::media::{Source, premultiplied_bgra};
use crate::state::Framing;

/// Zoom factor per scroll step.
const SCROLL_ZOOM_STEP: f64 = 1.1;
/// Minimum height of the preview in pixels (the panel's height).
const MIN_HEIGHT: i32 = 128;

type ViewCallback = Rc<dyn Fn(CropView)>;

struct State {
    surface: Option<cairo::ImageSurface>,
    /// Display-copy pixels per source pixel (the copy may be downscaled).
    display_scale: f64,
    size: Size,
    framing: Framing,
    view: CropView,
}

/// The pan/zoom preview of the file being edited. Cloning shares the same editor.
#[derive(Clone)]
pub struct Editor {
    frame: gtk::AspectFrame,
    area: gtk::DrawingArea,
    state: Rc<RefCell<State>>,
    callbacks: Rc<RefCell<Vec<ViewCallback>>>,
}

impl Editor {
    /// An empty editor in crop mode.
    pub fn new() -> Self {
        let size = Size {
            width: 1,
            height: 1,
        };
        let state = Rc::new(RefCell::new(State {
            surface: None,
            display_scale: 1.0,
            size,
            framing: Framing::Crop,
            view: CropView::new(size),
        }));
        let area = gtk::DrawingArea::builder()
            .content_height(MIN_HEIGHT)
            .hexpand(true)
            .css_classes(["card"])
            .build();
        // Rounded corners need the card clip.
        area.set_overflow(gtk::Overflow::Hidden);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a ratio of 3.75 fits an f32"
        )]
        let frame = gtk::AspectFrame::new(0.5, 0.5, PANEL_ASPECT as f32, false);
        frame.set_child(Some(&area));

        let editor = Self {
            frame,
            area,
            state,
            callbacks: Rc::default(),
        };
        editor.install_draw();
        editor.install_gestures();
        editor.update_cursor();
        editor
    }

    /// The widget to put in the window.
    pub fn widget(&self) -> &gtk::Widget {
        self.frame.upcast_ref()
    }

    /// Shows `source` in crop mode with a centred, unzoomed view.
    pub fn set_source(&self, source: &Source) {
        let surface = surface_from_bgra(
            premultiplied_bgra(&source.image),
            source.image.width(),
            source.image.height(),
        );
        {
            let mut state = self.state.borrow_mut();
            state.surface = surface;
            state.display_scale =
                f64::from(source.image.width()) / f64::from(source.size.width.max(1));
            state.size = source.size;
            state.view = CropView::new(source.size);
            state.framing = Framing::Crop;
        }
        self.update_cursor();
        self.area.queue_draw();
    }

    /// Swaps the drawn frame, keeping size, view and framing (for playback).
    pub fn set_frame_surface(&self, surface: &cairo::ImageSurface) {
        {
            let mut state = self.state.borrow_mut();
            state.display_scale = f64::from(surface.width()) / f64::from(state.size.width.max(1));
            state.surface = Some(surface.clone());
        }
        self.area.queue_draw();
    }

    /// Drops the image and resets to an empty crop-mode editor.
    pub fn clear(&self) {
        {
            let mut state = self.state.borrow_mut();
            let size = Size {
                width: 1,
                height: 1,
            };
            state.surface = None;
            state.display_scale = 1.0;
            state.size = size;
            state.view = CropView::new(size);
            state.framing = Framing::Crop;
        }
        self.update_cursor();
        self.area.queue_draw();
    }

    /// Switches between the crop window and a fit placement.
    pub fn set_framing(&self, framing: Framing) {
        self.state.borrow_mut().framing = framing;
        self.update_cursor();
        self.area.queue_draw();
    }

    /// Replaces the pan/zoom state (does not call the view-changed callbacks).
    pub fn set_view(&self, view: CropView) {
        self.state.borrow_mut().view = view;
        self.area.queue_draw();
    }

    /// The current pan/zoom state.
    pub fn view(&self) -> CropView {
        self.state.borrow().view
    }

    /// Calls `callback` after the user pans or zooms.
    pub fn connect_view_changed(&self, callback: impl Fn(CropView) + 'static) {
        self.callbacks.borrow_mut().push(Rc::new(callback));
    }

    fn update_cursor(&self) {
        let cursor = match self.state.borrow().framing {
            Framing::Crop => Some("grab"),
            Framing::Fit(_) => None,
        };
        self.area.set_cursor_from_name(cursor);
    }

    fn install_draw(&self) {
        let state = Rc::clone(&self.state);
        self.area.set_draw_func(move |_, cr, width, height| {
            draw(&state.borrow(), cr, f64::from(width), f64::from(height));
        });
    }

    /// Applies `change` to the view when in crop mode, then redraws and notifies.
    fn edit_view(&self, change: impl FnOnce(&mut CropView)) {
        let view = {
            let mut state = self.state.borrow_mut();
            if state.framing != Framing::Crop || state.surface.is_none() {
                return;
            }
            change(&mut state.view);
            state.view
        };
        self.area.queue_draw();
        let callbacks: Vec<ViewCallback> = self.callbacks.borrow().clone();
        for callback in callbacks {
            callback(view);
        }
    }

    fn install_gestures(&self) {
        let widget_size = {
            let area = self.area.clone();
            move || {
                (
                    f64::from(area.width()).max(1.0),
                    f64::from(area.height()).max(1.0),
                )
            }
        };

        let drag = gtk::GestureDrag::new();
        let last = Rc::new(Cell::new((0.0, 0.0)));
        drag.connect_drag_begin({
            let last = Rc::clone(&last);
            let area = self.area.clone();
            move |_, _, _| {
                last.set((0.0, 0.0));
                area.set_cursor_from_name(Some("grabbing"));
            }
        });
        drag.connect_drag_update({
            let editor = self.clone();
            let last = Rc::clone(&last);
            let widget_size = widget_size.clone();
            move |_, dx, dy| {
                let (px, py) = last.replace((dx, dy));
                let (width, _) = widget_size();
                editor.edit_view(|view| view.pan_by(dx - px, dy - py, width));
            }
        });
        drag.connect_drag_end({
            let editor = self.clone();
            move |_, _, _| editor.update_cursor()
        });
        self.area.add_controller(drag);

        let pointer = Rc::new(Cell::new((0.5, 0.5)));
        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion({
            let pointer = Rc::clone(&pointer);
            let widget_size = widget_size.clone();
            move |_, x, y| {
                let (width, height) = widget_size();
                pointer.set(((x / width).clamp(0.0, 1.0), (y / height).clamp(0.0, 1.0)));
            }
        });
        self.area.add_controller(motion);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.connect_scroll({
            let editor = self.clone();
            let pointer = Rc::clone(&pointer);
            move |_, _, dy| {
                if dy == 0.0 {
                    return gtk::glib::Propagation::Proceed;
                }
                // Scrolling up (negative dy) zooms in.
                let factor = SCROLL_ZOOM_STEP.powf(-dy.signum());
                editor.edit_view(|view| view.zoom_at(factor, pointer.get()));
                gtk::glib::Propagation::Stop
            }
        });
        self.area.add_controller(scroll);

        let pinch = gtk::GestureZoom::new();
        let last_scale = Rc::new(Cell::new(1.0));
        pinch.connect_begin({
            let last_scale = Rc::clone(&last_scale);
            move |_, _| last_scale.set(1.0)
        });
        pinch.connect_scale_changed({
            let editor = self.clone();
            let widget_size = widget_size.clone();
            move |gesture, scale| {
                let factor = scale / last_scale.replace(scale);
                let (width, height) = widget_size();
                let anchor = gesture.bounding_box_center().map_or((0.5, 0.5), |(x, y)| {
                    ((x / width).clamp(0.0, 1.0), (y / height).clamp(0.0, 1.0))
                });
                editor.edit_view(|view| view.zoom_at(factor, anchor));
            }
        });
        self.area.add_controller(pinch);
    }
}

/// Wraps premultiplied BGRA pixels (stride `4 * width`) in a cairo surface.
pub fn surface_from_bgra(bgra: Vec<u8>, width: u32, height: u32) -> Option<cairo::ImageSurface> {
    let width = i32::try_from(width).unwrap_or(i32::MAX);
    let height = i32::try_from(height).unwrap_or(i32::MAX);
    match cairo::ImageSurface::create_for_data(
        bgra,
        cairo::Format::ARgb32,
        width,
        height,
        width.saturating_mul(4),
    ) {
        Ok(surface) => Some(surface),
        Err(error) => {
            tracing::warn!("cannot build the preview surface: {error}");
            None
        }
    }
}

fn draw(state: &State, cr: &cairo::Context, width: f64, height: f64) {
    cr.set_source_rgb(0.0, 0.0, 0.0);
    if let Err(error) = cr.paint() {
        tracing::warn!("preview paint failed: {error}");
        return;
    }
    let Some(surface) = &state.surface else {
        return;
    };
    cr.rectangle(0.0, 0.0, width, height);
    cr.clip();
    match state.framing {
        Framing::Crop => {
            let visible = state.view.visible();
            let scale = width / visible.width;
            cr.translate(-visible.x * scale, -visible.y * scale);
            let factor = scale / state.display_scale;
            cr.scale(factor, factor);
        }
        Framing::Fit(fit) => {
            let rect = fit_rect(state.size, fit, width, height);
            cr.translate(rect.x, rect.y);
            cr.scale(
                rect.width / f64::from(surface.width()),
                rect.height / f64::from(surface.height()),
            );
        }
    }
    if cr.set_source_surface(surface, 0.0, 0.0).is_err() {
        return;
    }
    cr.source().set_filter(cairo::Filter::Good);
    if let Err(error) = cr.paint() {
        tracing::warn!("preview paint failed: {error}");
    }
}
