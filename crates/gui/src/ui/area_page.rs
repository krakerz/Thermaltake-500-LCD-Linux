//! Display > Area: stream a virtual desktop or a monitor to the panel, with the crop editor
//! fed by the daemon's live preview.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use image::RgbaImage;
use tt500_lcd_ipc::{
    ContentInfo, ContentKind, DEFAULT_FPS, Fit, LiveSource, MAX_FPS, PreviewReply, Request,
    ShowLiveParams, Status,
};

use super::context::{Context, page_scroller, weak_call};
use super::crop_panel::CropPanel;
use super::icons::{self, icon};
use crate::area::{
    ApplyMode, AreaSource, DEFAULT_PRESET, Form, PRESETS, PreviewStep, PreviewTracker, apply_mode,
    can_add_command, preset_index,
};
use crate::daemon;
use crate::settings::{self, GuiSettings};

/// Time between preview requests while the page is visible.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(250);
const COMMAND_TITLE: &str = "Command, e.g. alacritty -e ~/Scripts/sysmon.sh";
/// Source row entries, in [`AreaSource`] order of this list.
const SOURCES: [(&str, AreaSource); 2] = [
    ("Virtual desktop", AreaSource::Desktop),
    ("Screen", AreaSource::Screen),
];

type Preview = Result<Option<(PreviewReply, RgbaImage)>, String>;

struct Inner {
    ctx: Rc<Context>,
    panel: CropPanel,
    source_row: adw::ComboRow,
    output_row: adw::ComboRow,
    outputs: gtk::StringList,
    size_row: adw::ComboRow,
    commands_group: adw::PreferencesGroup,
    add_button: gtk::Button,
    command_rows: RefCell<Vec<adw::EntryRow>>,
    fps_row: adw::SpinRow,
    steady_row: adw::SwitchRow,
    /// The user's choice; `None` follows the source type's default.
    steady_choice: Cell<Option<bool>>,
    apply_button: gtk::Button,
    stop_button: gtk::Button,
    scroller: gtk::ScrolledWindow,
    settings: RefCell<GuiSettings>,
    tracker: Cell<PreviewTracker>,
    fetching: Cell<bool>,
    /// Set once the controls follow the source: after Apply, or after filling from the daemon.
    adopted: Cell<bool>,
    /// Set while code (not the user) moves a control, so handlers stay quiet.
    syncing: Cell<bool>,
}

/// The Area page. Cloning shares it.
#[derive(Clone)]
pub(super) struct AreaPage {
    inner: Rc<Inner>,
}

impl AreaPage {
    pub fn new(ctx: &Rc<Context>) -> Self {
        let settings = settings::path()
            .map(|path| settings::load(&path))
            .unwrap_or_default();

        let source_row = adw::ComboRow::builder()
            .title("Source")
            .subtitle("Screen or window through the portal: later")
            .model(&gtk::StringList::new(&SOURCES.map(|(label, _)| label)))
            .build();
        let outputs = gtk::StringList::new(&[]);
        let output_row = adw::ComboRow::builder()
            .title("Monitor")
            .model(&outputs)
            .build();
        let size_row = adw::ComboRow::builder()
            .title("Desktop size")
            .subtitle("The panel is 15:4; scale 2 draws at half the size")
            .model(&gtk::StringList::new(&PRESETS.map(|preset| preset.label)))
            .build();
        let source_group = adw::PreferencesGroup::builder().title("Source").build();
        source_group.add(&source_row);
        source_group.add(&output_row);
        source_group.add(&size_row);

        let add_button = gtk::Button::builder()
            .label("Add Command")
            .css_classes(["flat"])
            .build();
        let commands_group = adw::PreferencesGroup::builder()
            .title("Commands")
            .description("Each one starts a program on the virtual desktop (through sh -c).")
            .header_suffix(&add_button)
            .build();

        let panel = CropPanel::new(
            "Start a live source to preview it",
            &gtk::Button::builder().visible(false).build(),
        );
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
        fps_row.set_subtitle("In fps (up to 30).");
        fps_row.add_prefix(&gtk::Image::from_icon_name(icon(icons::FPS)));
        panel.picture_group().add(&fps_row);
        let steady_row = adw::SwitchRow::builder()
            .title("Steady frames")
            .subtitle("Skip half-drawn frames (adds a little delay)")
            .build();
        panel.picture_group().add(&steady_row);

        let stop_button = gtk::Button::builder()
            .label("Stop")
            .css_classes(["destructive-action"])
            .sensitive(false)
            .build();
        let apply_button = gtk::Button::builder()
            .label("Start")
            .css_classes(["suggested-action"])
            .sensitive(false)
            .build();
        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk::Align::End)
            .build();
        buttons.append(&stop_button);
        buttons.append(&apply_button);

        let scroller = page_scroller(&[
            source_group.upcast_ref(),
            commands_group.upcast_ref(),
            panel.widget().upcast_ref(),
            panel.picture_group().upcast_ref(),
            buttons.upcast_ref(),
        ]);
        let steady_choice = Cell::new(settings.steady);
        let inner = Rc::new(Inner {
            ctx: Rc::clone(ctx),
            panel,
            source_row,
            output_row,
            outputs,
            size_row,
            commands_group,
            add_button,
            command_rows: RefCell::new(Vec::new()),
            fps_row,
            steady_row,
            steady_choice,
            apply_button,
            stop_button,
            scroller,
            settings: RefCell::new(settings),
            tracker: Cell::new(PreviewTracker::default()),
            fetching: Cell::new(false),
            adopted: Cell::new(false),
            syncing: Cell::new(false),
        });
        inner.restore_controls();

        let weak = Rc::downgrade(&inner);
        let handler = weak_call(&weak, |inner, ()| inner.show_source_groups());
        inner
            .source_row
            .connect_selected_notify(move |_| handler(()));
        let handler = weak_call(&weak, |inner, ()| inner.save_settings());
        inner.size_row.connect_selected_notify(move |_| handler(()));
        let handler = weak_call(&weak, |inner, ()| inner.steady_toggled());
        inner.steady_row.connect_active_notify(move |_| handler(()));
        let handler = weak_call(&weak, |inner, ()| inner.add_command_row(""));
        inner.add_button.connect_clicked(move |_| handler(()));
        let handler = weak_call(&weak, |inner, ()| inner.apply());
        inner.apply_button.connect_clicked(move |_| handler(()));
        let handler = weak_call(&weak, |inner, ()| inner.stop());
        inner.stop_button.connect_clicked(move |_| handler(()));

        let weak = Rc::downgrade(&inner);
        glib::timeout_add_local(PREVIEW_INTERVAL, move || match weak.upgrade() {
            Some(inner) => {
                inner.poll_preview();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
        Self { inner }
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.inner.scroller
    }

    /// Redraws what depends on the daemon's status. Returns `true` when the controls were just
    /// filled from a live source that was already running, so the shell can show this page.
    pub fn refresh(&self, status: Option<&Status>) -> bool {
        let inner = &self.inner;
        let live = status.and_then(live_content);
        inner.stop_button.set_sensitive(live.is_some());
        inner.apply_button.set_sensitive(status.is_some());
        inner.update_apply_label(live.and_then(|info| info.live.as_ref()));
        match live {
            Some(info) if !inner.adopted.get() => {
                inner.adopted.set(true);
                inner.fill_from(info);
                true
            }
            Some(_) => false,
            None => {
                if inner.adopted.get() || inner.panel.framed().is_some() {
                    inner.adopted.set(false);
                    inner.reset_preview();
                }
                false
            }
        }
    }
}

/// The content when it is a live source.
fn live_content(status: &Status) -> Option<&ContentInfo> {
    status
        .content
        .as_ref()
        .filter(|info| info.kind == ContentKind::Live)
}

/// What the daemon does for a source type when no choice is sent.
fn default_steady(source: AreaSource) -> bool {
    source == AreaSource::Desktop
}

fn clamp_fps(value: f64) -> u32 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 1..=MAX_FPS"
    )]
    let fps = value.round().clamp(1.0, f64::from(MAX_FPS)) as u32;
    fps
}

/// Connector names of the session's monitors.
fn monitor_names() -> Vec<String> {
    let Some(display) = gdk::Display::default() else {
        return Vec::new();
    };
    let monitors = display.monitors();
    (0..monitors.n_items())
        .filter_map(|index| monitors.item(index))
        .filter_map(|item| item.downcast::<gdk::Monitor>().ok())
        .filter_map(|monitor| monitor.connector())
        .map(|name| name.to_string())
        .collect()
}

impl Inner {
    /// Fills the controls from the saved settings and the session's monitors.
    fn restore_controls(self: &Rc<Self>) {
        self.syncing.set(true);
        let settings = self.settings.borrow().clone();
        self.set_outputs(&monitor_names(), None);
        let preset =
            preset_index(settings.desktop_size, settings.desktop_scale).unwrap_or(DEFAULT_PRESET);
        self.size_row
            .set_selected(u32::try_from(preset).unwrap_or(0));
        self.set_command_rows(&settings.desktop_commands);
        self.show_source_groups();
        self.syncing.set(false);
    }

    /// Output row entries: "Primary", then `names`; selects `selected` (or Primary).
    fn set_outputs(&self, names: &[String], selected: Option<&str>) {
        let mut entries: Vec<&str> = vec!["Primary"];
        entries.extend(names.iter().map(String::as_str));
        let mut position = 0;
        if let Some(wanted) = selected {
            match entries.iter().position(|entry| *entry == wanted) {
                Some(index) => position = index,
                None => {
                    entries.push(wanted);
                    position = entries.len() - 1;
                }
            }
        }
        self.outputs.splice(0, self.outputs.n_items(), &entries);
        self.output_row
            .set_selected(u32::try_from(position).unwrap_or(0));
    }

    fn set_command_rows(self: &Rc<Self>, commands: &[String]) {
        let old: Vec<adw::EntryRow> = self.command_rows.borrow_mut().drain(..).collect();
        for row in old {
            self.commands_group.remove(&row);
        }
        for command in commands {
            self.add_command_row(command);
        }
        if self.command_rows.borrow().is_empty() {
            self.add_command_row("");
        }
    }

    fn add_command_row(self: &Rc<Self>, text: &str) {
        if !can_add_command(self.command_rows.borrow().len()) {
            return;
        }
        let row = adw::EntryRow::builder().title(COMMAND_TITLE).build();
        row.set_text(text);
        let remove = gtk::Button::builder()
            .icon_name(icon(icons::REMOVE))
            .tooltip_text("Remove this command")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        row.add_suffix(&remove);
        self.commands_group.add(&row);
        self.command_rows.borrow_mut().push(row.clone());

        let weak = Rc::downgrade(self);
        let handler = weak_call(&weak, |inner, ()| inner.save_settings());
        row.connect_changed(move |_| handler(()));
        let row_weak = row.downgrade();
        let handler = weak_call(&weak, move |inner, ()| {
            if let Some(row) = row_weak.upgrade() {
                inner.remove_command_row(&row);
            }
        });
        remove.connect_clicked(move |_| handler(()));
        self.update_add_button();
        self.save_settings();
    }

    fn remove_command_row(&self, row: &adw::EntryRow) {
        let last = self.command_rows.borrow().len() <= 1;
        if last {
            row.set_text("");
            return;
        }
        self.command_rows.borrow_mut().retain(|r| r != row);
        self.commands_group.remove(row);
        self.update_add_button();
        self.save_settings();
    }

    fn update_add_button(&self) {
        self.add_button
            .set_sensitive(can_add_command(self.command_rows.borrow().len()));
    }

    fn source(&self) -> AreaSource {
        usize::try_from(self.source_row.selected())
            .ok()
            .and_then(|index| SOURCES.get(index))
            .map_or(AreaSource::Desktop, |(_, source)| *source)
    }

    fn show_source_groups(&self) {
        let desktop = self.source() == AreaSource::Desktop;
        self.size_row.set_visible(desktop);
        self.commands_group.set_visible(desktop);
        self.output_row.set_visible(!desktop);
        self.show_steady();
    }

    /// Shows the saved choice, or the source type's default.
    fn show_steady(&self) {
        let wanted = self
            .steady_choice
            .get()
            .unwrap_or_else(|| default_steady(self.source()));
        let before = self.syncing.replace(true);
        self.steady_row.set_active(wanted);
        self.syncing.set(before);
    }

    fn steady_toggled(&self) {
        if self.syncing.get() {
            return;
        }
        self.steady_choice.set(Some(self.steady_row.is_active()));
        self.save_settings();
    }

    fn rows_text(&self) -> Vec<String> {
        self.command_rows
            .borrow()
            .iter()
            .map(|row| row.text().to_string())
            .collect()
    }

    /// The controls as a [`Form`].
    fn form(&self) -> Form {
        let output = match self.output_row.selected() {
            0 => None,
            index => self.outputs.string(index).map(|name| name.to_string()),
        };
        Form {
            source: self.source(),
            output,
            commands: self.rows_text(),
            preset: Some(usize::try_from(self.size_row.selected()).unwrap_or(DEFAULT_PRESET)),
        }
    }

    fn fps(&self) -> u32 {
        clamp_fps(self.fps_row.value())
    }

    fn save_settings(&self) {
        if self.syncing.get() {
            return;
        }
        let preset = usize::try_from(self.size_row.selected())
            .ok()
            .and_then(|index| PRESETS.get(index))
            .unwrap_or(&PRESETS[DEFAULT_PRESET]);
        let settings = GuiSettings {
            desktop_commands: crate::area::clean_commands(&self.rows_text()),
            desktop_size: preset.size,
            desktop_scale: preset.scale,
            steady: self.steady_choice.get(),
        };
        if *self.settings.borrow() == settings {
            return;
        }
        if let Some(path) = settings::path()
            && let Err(error) = settings::save(&path, &settings)
        {
            tracing::warn!("cannot save {}: {error}", path.display());
        }
        *self.settings.borrow_mut() = settings;
    }

    fn running_live(&self) -> Option<LiveSource> {
        let status = self.ctx.status.borrow();
        status.as_ref()?.content.as_ref()?.live.clone()
    }

    fn update_apply_label(&self, running: Option<&LiveSource>) {
        let same = match (running, self.form().live_source()) {
            (Some(_), Ok(wanted)) => apply_mode(running, &wanted) == ApplyMode::Reframe,
            _ => false,
        };
        self.apply_button
            .set_label(if same { "Apply" } else { "Start" });
    }

    /// Moves the controls to a source the daemon already streams.
    fn fill_from(self: &Rc<Self>, info: &ContentInfo) {
        let Some(form) = info.live.as_ref().and_then(Form::from_live) else {
            return;
        };
        self.syncing.set(true);
        let source_index = SOURCES
            .iter()
            .position(|(_, source)| *source == form.source)
            .unwrap_or(0);
        self.source_row
            .set_selected(u32::try_from(source_index).unwrap_or(0));
        match form.source {
            AreaSource::Desktop => {
                self.set_command_rows(&form.commands);
                if let Some(preset) = form.preset {
                    self.size_row
                        .set_selected(u32::try_from(preset).unwrap_or(0));
                }
            }
            AreaSource::Screen => {
                self.set_outputs(&monitor_names(), form.output.as_deref());
            }
        }
        if let Some(fps) = info.max_fps.or(info.fps) {
            self.fps_row.set_value(f64::from(fps));
        }
        self.steady_choice.set(
            info.steady
                .filter(|steady| *steady != default_steady(form.source)),
        );
        self.show_source_groups();
        self.syncing.set(false);
    }

    fn apply(&self) {
        let source = match self.form().live_source() {
            Ok(source) => source,
            Err(reason) => {
                self.ctx.toast(&reason);
                return;
            }
        };
        let mode = apply_mode(self.running_live().as_ref(), &source);
        let (fit, crop) = match (mode, self.panel.framed()) {
            (ApplyMode::Reframe, Some(framed)) => framed.fit_and_crop(),
            _ => (Fit::Cover, None),
        };
        if mode == ApplyMode::Start {
            self.reset_preview();
        }
        self.adopted.set(true);
        self.ctx.send(
            Request::ShowLive(ShowLiveParams {
                source,
                fit,
                crop,
                fps: Some(self.fps()),
                steady: self.steady_choice.get(),
            }),
            "Sent to the panel",
        );
    }

    fn stop(&self) {
        self.reset_preview();
        self.adopted.set(false);
        self.ctx.send(Request::Clear, "Stopped");
    }

    /// Back to the empty editor; the next frame loads it afresh.
    fn reset_preview(&self) {
        let mut tracker = self.tracker.get();
        tracker.reset();
        self.tracker.set(tracker);
        self.panel.clear();
    }

    /// Fetches the latest frame when the page is on screen and a live source runs.
    fn poll_preview(self: &Rc<Self>) {
        if self.fetching.get() || !self.scroller.is_mapped() {
            return;
        }
        let live = self
            .ctx
            .status
            .borrow()
            .as_ref()
            .is_some_and(|status| live_content(status).is_some());
        let (true, Ok(socket)) = (live, &self.ctx.socket) else {
            return;
        };
        let socket = socket.clone();
        let tracker = self.tracker.get();
        self.fetching.set(true);
        let inner = Rc::clone(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || fetch_preview(&socket, &tracker)).await;
            inner.fetching.set(false);
            match result {
                Ok(Ok(Some((reply, image)))) => inner.show_frame(&reply, image),
                Ok(Ok(None)) => {}
                Ok(Err(reason)) => tracing::debug!("no preview: {reason}"),
                Err(_) => tracing::warn!("the preview worker panicked"),
            }
        });
    }

    fn show_frame(&self, reply: &PreviewReply, image: RgbaImage) {
        let mut tracker = self.tracker.get();
        let step = tracker.step(reply.seq, reply.size);
        self.tracker.set(tracker);
        if step == PreviewStep::Skip {
            return;
        }
        let restarted = self.panel.set_live(image, reply.size);
        if restarted {
            let status = self.ctx.status.borrow();
            if let Some(info) = status.as_ref().and_then(live_content)
                && (info.crop.is_some() || info.fit != Fit::Cover)
            {
                self.panel.adopt(info);
            }
        }
    }
}

/// Worker thread: the newest preview, decoded; `None` when it is the frame already shown.
fn fetch_preview(socket: &std::path::Path, shown: &PreviewTracker) -> Preview {
    let reply = daemon::preview(socket)?;
    if shown.is_current(reply.seq, reply.size) {
        return Ok(None);
    }
    let image = image::open(&reply.path)
        .map_err(|error| format!("cannot read the preview: {error}"))?
        .to_rgba8();
    Ok(Some((reply, image)))
}
