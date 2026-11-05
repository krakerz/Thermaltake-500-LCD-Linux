//! The main window: a sidebar with the Display and Boot pages, banners, brightness, rotation and status.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use tt500_lcd_ipc::{
    BrightnessParams, ContentKind, DEFAULT_BRIGHTNESS, ProfileNameParams, Request, RotationParams,
    Status,
};

use super::area_page::AreaPage;
use super::boot_page::BootPage;
use super::context::{Context, weak_call};
use super::file_source::CHOOSE_FILE;
use super::icons::{self, icon};
use super::profile_switcher::ProfileSwitcher;
use super::profiles_page::ProfilesPage;
use super::static_page::StaticPage;
use super::tray_link::TrayLink;
use crate::daemon::{self, Update};
use crate::settings;
use crate::state::{Connection, START_HINT};
use crate::tray::{Action, Tray};

/// The content-error banner button while a live source is shown.
const EDIT_COMMANDS: &str = "Edit Commands";

/// Pause after the last brightness slider move before it is sent.
const BRIGHTNESS_DEBOUNCE: Duration = Duration::from_millis(150);

/// Presents the main window, or raises the existing one. With `start_hidden` the window stays
/// hidden in the tray (shown anyway when no tray can be registered).
pub fn present(app: &adw::Application, start_hidden: bool) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }
    let shell = Shell::build(app);
    if !start_hidden {
        shell.window.present();
    }
    shell.start_watching();
    super::updates::auto_check(&shell.ctx);
    shell.start_tray(app, start_hidden);
}

/// The brightness slider (global: it applies to every source). Cloning shares it.
#[derive(Clone)]
struct Brightness {
    ctx: Rc<Context>,
    scale: gtk::Scale,
    label: gtk::Label,
    timer: Rc<RefCell<Option<glib::SourceId>>>,
    /// Set while code (not the user) moves the slider, so its handler stays quiet.
    syncing: Rc<Cell<bool>>,
}

impl Brightness {
    fn new(ctx: &Rc<Context>) -> Self {
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
        scale.set_draw_value(false);
        scale.set_hexpand(true);
        let label = gtk::Label::builder()
            .width_chars(4)
            .xalign(1.0)
            .css_classes(["dim-label", "numeric"])
            .build();
        let this = Self {
            ctx: Rc::clone(ctx),
            scale,
            label,
            timer: Rc::new(RefCell::new(None)),
            syncing: Rc::new(Cell::new(false)),
        };
        this.update_label();
        let handler = this.clone();
        this.scale.connect_value_changed(move |_| handler.moved());
        this
    }

    /// Icon, title and value above the slider.
    fn widget(&self) -> gtk::Box {
        let title = gtk::Label::builder()
            .label("Brightness")
            .xalign(0.0)
            .hexpand(true)
            .build();
        let head = gtk::Box::builder().spacing(8).build();
        head.append(&gtk::Image::from_icon_name(icon(icons::BRIGHTNESS)));
        head.append(&title);
        head.append(&self.label);
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        column.append(&head);
        column.append(&self.scale);
        column
    }

    fn update_label(&self) {
        let value = self.scale.value().round();
        let text = if value == 0.0 {
            "Off".to_owned()
        } else {
            format!("{value:.0}%")
        };
        self.label.set_text(&text);
    }

    /// Follows the daemon's value unless the user is mid-drag.
    fn follow(&self, value: u8) {
        if self.timer.borrow().is_none() {
            self.set_quietly(value);
        }
        self.update_label();
    }

    fn set_quietly(&self, value: u8) {
        self.syncing.set(true);
        self.scale.set_value(f64::from(value));
        self.syncing.set(false);
    }

    /// Drops a pending send and moves the slider back to the default.
    fn reset(&self) {
        if let Some(pending) = self.timer.borrow_mut().take() {
            pending.remove();
        }
        self.set_quietly(DEFAULT_BRIGHTNESS);
        self.update_label();
    }

    fn moved(&self) {
        self.update_label();
        if self.syncing.get() {
            return;
        }
        if let Some(pending) = self.timer.borrow_mut().take() {
            pending.remove();
        }
        let this = self.clone();
        let timer = glib::timeout_add_local_once(BRIGHTNESS_DEBOUNCE, move || {
            // The source has fired: forget it so nobody removes a dead id.
            this.timer.borrow_mut().take();
            let value = this.scale.value().round().clamp(0.0, 100.0);
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "clamped to 0..=100"
            )]
            let value = value as u8;
            this.ctx
                .send(Request::SetBrightness(BrightnessParams { value }), "");
        });
        *self.timer.borrow_mut() = Some(timer);
    }
}

/// Labels and tooltips of the rotation steps. The picture is turned on the PC: step 1 lays the
/// content out portrait and turns it 90 degrees clockwise (so the panel is mounted turned 90
/// degrees counter-clockwise, content reading top to bottom); step 3 is the mirror case.
const ROTATIONS: [(&str, &str); 4] = [
    ("0°", "Normal"),
    ("90°", "Portrait — panel turned 90° counter-clockwise"),
    ("180°", "Upside down"),
    ("270°", "Portrait — panel turned 90° clockwise"),
];

/// The panel rotation step buttons (global). Cloning shares them.
#[derive(Clone)]
struct Rotation {
    ctx: Rc<Context>,
    buttons: Rc<Vec<gtk::ToggleButton>>,
    /// Set while code (not the user) toggles a button, so its handler stays quiet.
    syncing: Rc<Cell<bool>>,
}

impl Rotation {
    fn new(ctx: &Rc<Context>) -> Self {
        let buttons: Vec<gtk::ToggleButton> = ROTATIONS
            .iter()
            .map(|(label, tooltip)| {
                gtk::ToggleButton::builder()
                    .label(*label)
                    .tooltip_text(*tooltip)
                    .hexpand(true)
                    .build()
            })
            .collect();
        for button in &buttons[1..] {
            button.set_group(Some(&buttons[0]));
        }
        buttons[0].set_active(true);
        let this = Self {
            ctx: Rc::clone(ctx),
            buttons: Rc::new(buttons),
            syncing: Rc::new(Cell::new(false)),
        };
        for (step, button) in this.buttons.iter().enumerate() {
            let handler = this.clone();
            button.connect_toggled(move |button| {
                if button.is_active() && !handler.syncing.get() {
                    #[expect(clippy::cast_possible_truncation, reason = "at most four buttons")]
                    let value = step as u8;
                    handler
                        .ctx
                        .send(Request::SetRotation(RotationParams { value }), "");
                }
            });
        }
        this
    }

    /// Title above a row of four linked toggle buttons.
    fn widget(&self) -> gtk::Box {
        let title = gtk::Label::builder().label("Rotation").xalign(0.0).build();
        let row = gtk::Box::builder()
            .css_classes(["linked"])
            .homogeneous(true)
            .build();
        for button in self.buttons.iter() {
            row.append(button);
        }
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        column.append(&title);
        column.append(&row);
        column
    }

    /// Shows the daemon's step; an out-of-range value is ignored.
    fn follow(&self, value: u8) {
        if let Some(button) = self.buttons.get(usize::from(value)) {
            self.syncing.set(true);
            button.set_active(true);
            self.syncing.set(false);
        }
    }
}

/// A sidebar entry: a page of the content stack and its view switcher stack.
struct PageEntry {
    name: &'static str,
    title: &'static str,
    icon: &'static str,
    views: adw::ViewStack,
}

struct Shell {
    ctx: Rc<Context>,
    window: adw::ApplicationWindow,
    split_view: adw::NavigationSplitView,
    content_stack: gtk::Stack,
    switcher: adw::ViewSwitcher,
    entries: Vec<PageEntry>,
    display_views: adw::ViewStack,
    down_banner: adw::Banner,
    error_banner: adw::Banner,
    status_row: adw::ActionRow,
    status_icon: gtk::Image,
    brightness: Brightness,
    rotation: Rotation,
    /// The step the editors are laid out for, `None` before the first status.
    editor_rotation: Cell<Option<u8>>,
    static_page: StaticPage,
    boot_page: BootPage,
    area_page: AreaPage,
    profiles_page: ProfilesPage,
    profile_switcher: ProfileSwitcher,
    /// The profile-set revision last fetched, `None` before the first status.
    profile_revision: Cell<Option<u64>>,
    /// The activation counter last seen, `None` before the first status.
    profile_activation: Cell<Option<u64>>,
    tray: Rc<TrayLink>,
}

impl Shell {
    fn build(app: &adw::Application) -> Rc<Self> {
        let toasts = adw::ToastOverlay::new();
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("TT500 LCD")
            .default_width(980)
            .default_height(680)
            .content(&toasts)
            .build();
        let ctx = Context::new(window.clone(), toasts.clone());
        let brightness = Brightness::new(&ctx);
        let rotation = Rotation::new(&ctx);
        let reset = brightness.clone();
        let static_page = StaticPage::new(&ctx, move || reset.reset());
        let boot_page = BootPage::new(&ctx);
        let area_page = AreaPage::new(&ctx);
        let profiles_page = ProfilesPage::new(&ctx);
        let profile_switcher = ProfileSwitcher::new(&ctx);

        let display_views = adw::ViewStack::new();
        display_views.add_titled_with_icon(
            static_page.widget(),
            Some("static"),
            "Static",
            icon(icons::CROP),
        );
        display_views.add_titled_with_icon(
            area_page.widget(),
            Some("area"),
            "Area",
            icon(icons::AREA),
        );
        let entries = vec![
            PageEntry {
                name: "display",
                title: "Display",
                icon: icons::DISPLAY,
                views: display_views.clone(),
            },
            PageEntry {
                name: "boot",
                title: "Boot",
                icon: icons::POWER,
                views: boot_page.view_stack().clone(),
            },
            PageEntry {
                name: "profiles",
                title: "Profiles",
                icon: icons::PROFILES,
                views: profiles_page.widget().clone(),
            },
        ];

        let content_stack = gtk::Stack::new();
        for entry in &entries {
            content_stack.add_named(&entry.views, Some(entry.name));
        }
        let down_banner = adw::Banner::new(&format!("tt500-lcdd is not running. {START_HINT}"));
        let error_banner = adw::Banner::builder().button_label(CHOOSE_FILE).build();
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.append(&down_banner);
        page.append(&error_banner);
        content_stack.set_vexpand(true);
        page.append(&content_stack);
        let switcher = adw::ViewSwitcher::builder()
            .policy(adw::ViewSwitcherPolicy::Wide)
            .build();
        let content_header = adw::HeaderBar::builder().title_widget(&switcher).build();
        let menu = gio::Menu::new();
        menu.append(Some("Preferences"), Some("win.preferences"));
        content_header.pack_end(
            &gtk::MenuButton::builder()
                .icon_name("open-menu-symbolic")
                .menu_model(&menu)
                .tooltip_text("Main menu")
                .primary(true)
                .build(),
        );
        let content_toolbar = adw::ToolbarView::new();
        content_toolbar.add_top_bar(&content_header);
        content_toolbar.set_content(Some(&page));
        let content_page = adw::NavigationPage::builder()
            .title("TT500 LCD")
            .child(&content_toolbar)
            .build();

        let sidebar_list = gtk::ListBox::builder()
            .css_classes(["navigation-sidebar"])
            .vexpand(true)
            .build();
        for entry in &entries {
            let row = gtk::Box::builder()
                .spacing(12)
                .margin_top(6)
                .margin_bottom(6)
                .build();
            row.append(&gtk::Image::from_icon_name(icon(entry.icon)));
            row.append(&gtk::Label::new(Some(entry.title)));
            sidebar_list.append(&row);
        }
        let status_icon = gtk::Image::from_icon_name(icon(icons::PANEL_OFF));
        let status_row = adw::ActionRow::builder().title("Status").build();
        status_row.add_prefix(&status_icon);
        let status_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        status_list.append(&status_row);
        let bottom = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        bottom.append(&profile_switcher.widget());
        bottom.append(&brightness.widget());
        bottom.append(&rotation.widget());
        bottom.append(&status_list);
        let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar_box.append(&sidebar_list);
        sidebar_box.append(&bottom);
        let sidebar_toolbar = adw::ToolbarView::new();
        sidebar_toolbar.add_top_bar(&adw::HeaderBar::new());
        sidebar_toolbar.set_content(Some(&sidebar_box));
        let sidebar_page = adw::NavigationPage::builder()
            .title("TT500 LCD")
            .child(&sidebar_toolbar)
            .build();

        let split_view = adw::NavigationSplitView::builder()
            .sidebar(&sidebar_page)
            .content(&content_page)
            .min_sidebar_width(230.0)
            .max_sidebar_width(300.0)
            .build();
        let breakpoint = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 600sp")
                .expect("the breakpoint condition is valid"),
        );
        breakpoint.add_setter(&split_view, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(breakpoint);
        toasts.set_child(Some(&split_view));

        let shell = Rc::new(Self {
            ctx,
            window,
            split_view,
            content_stack,
            switcher,
            entries,
            display_views,
            down_banner,
            error_banner,
            status_row,
            status_icon,
            brightness,
            rotation,
            editor_rotation: Cell::new(None),
            static_page,
            boot_page,
            area_page,
            profiles_page,
            profile_switcher,
            profile_revision: Cell::new(None),
            profile_activation: Cell::new(None),
            tray: Rc::default(),
        });
        let weak = Rc::downgrade(&shell);
        let select = weak_call(&weak, |shell, index: i32| shell.select_page(index));
        sidebar_list.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                select(row.index());
            }
        });
        let preferences = gio::SimpleAction::new("preferences", None);
        let ctx = Rc::clone(&shell.ctx);
        preferences.connect_activate(move |_, _| super::preferences::show(&ctx));
        shell.window.add_action(&preferences);
        let open = weak_call(&weak, |shell, ()| shell.fix_content_error());
        shell.error_banner.connect_button_clicked(move |_| open(()));
        let close = weak_call(&weak, |shell, ()| shell.close_requested());
        shell.window.connect_close_request(move |_| {
            close(());
            glib::Propagation::Stop
        });
        if let Some(first) = sidebar_list.row_at_index(0) {
            sidebar_list.select_row(Some(&first));
        }
        // Start on the content, not on the sidebar, when the window opens collapsed.
        shell.split_view.set_show_content(true);
        shell.refresh();
        shell
    }

    /// The window's close button: hide to the tray when there is one and the user wants that,
    /// else quit.
    fn close_requested(&self) {
        self.hide();
        if !(self.tray.active() && settings::keep_in_tray()) {
            match self.window.application() {
                Some(app) => app.quit(),
                None => self.window.destroy(),
            }
        }
    }

    /// Stops the previews and hides the window.
    fn hide(&self) {
        self.static_page.stop();
        self.boot_page.stop();
        self.window.set_visible(false);
    }

    /// Registers the tray icon (off the main thread) and serves its menu. Without a tray, logs
    /// it once and carries on; a window that was meant to stay hidden is then shown.
    fn start_tray(self: &Rc<Self>, app: &adw::Application, start_hidden: bool) {
        let (actions_tx, actions) = async_channel::unbounded();
        // Keeps the app alive while the window is hidden or the tray is still registering.
        let hold = app.hold();
        let initial = TrayLink::state_for(&self.ctx);
        let shell = Rc::downgrade(self);
        let app = app.clone();
        glib::spawn_future_local(async move {
            let spawned = gio::spawn_blocking(move || Tray::spawn(initial, actions_tx)).await;
            let Some(first) = shell.upgrade() else {
                return;
            };
            match spawned {
                Ok(Ok(tray)) => {
                    first.tray.install(tray, hold, &first.ctx);
                    // The status may have changed while the tray registered.
                    first.tray.sync(&first.ctx);
                }
                Ok(Err(error)) => {
                    tracing::info!("no system tray, running without one: {error}");
                    drop(hold);
                    first.window.present();
                    return;
                }
                Err(_) => {
                    tracing::info!("no system tray: the registration failed");
                    drop(hold);
                    first.window.present();
                    return;
                }
            }
            drop(first);
            if start_hidden {
                tracing::debug!("started hidden in the tray");
            }
            while let Ok(action) = actions.recv().await {
                let Some(shell) = shell.upgrade() else {
                    break;
                };
                shell.run_tray_action(&app, action);
            }
        });
    }

    /// Does what a tray click asked for. Runs on the main thread.
    fn run_tray_action(&self, app: &adw::Application, action: Action) {
        match action {
            Action::Open => self.window.present(),
            Action::Toggle if self.window.is_visible() => self.hide(),
            Action::Toggle => self.window.present(),
            Action::ActivateProfile(name) => self.ctx.send(
                Request::ProfileActivate(ProfileNameParams { name }),
                "Profile activated",
            ),
            Action::SetBrightness(value) => {
                self.ctx
                    .send(Request::SetBrightness(BrightnessParams { value }), "");
            }
            Action::ClearPanel => self.static_page.clear_panel(),
            Action::Quit => {
                self.static_page.stop();
                self.boot_page.stop();
                app.quit();
            }
        }
    }

    /// Shows the page of sidebar row `index` and gives its views to the switcher.
    fn select_page(&self, index: i32) {
        let Some(entry) = usize::try_from(index)
            .ok()
            .and_then(|i| self.entries.get(i))
        else {
            return;
        };
        self.content_stack.set_visible_child_name(entry.name);
        self.switcher.set_stack(Some(&entry.views));
        self.split_view.set_show_content(true);
    }

    /// Whether the daemon currently shows a live source (area or virtual desktop).
    fn showing_live(&self) -> bool {
        self.ctx
            .status
            .borrow()
            .as_ref()
            .and_then(|s| s.content.as_ref())
            .is_some_and(|c| c.kind == ContentKind::Live)
    }

    /// The content-error banner's button: for a live source back to Display > Area, else to
    /// Display > Static to pick a file.
    fn fix_content_error(&self) {
        if self.showing_live() {
            self.show_area();
        } else {
            self.choose_static_file();
        }
    }

    /// Back to Display > Static, then pick a file.
    fn choose_static_file(&self) {
        self.content_stack.set_visible_child_name("display");
        self.switcher.set_stack(Some(&self.display_views));
        self.display_views.set_visible_child_name("static");
        self.static_page.choose_file();
    }

    /// Follows the daemon's status for as long as the window lives.
    fn start_watching(self: &Rc<Self>) {
        let socket = match &self.ctx.socket {
            Ok(socket) => socket.clone(),
            Err(reason) => {
                self.down_banner
                    .set_title(&format!("Cannot find the daemon socket: {reason}"));
                self.down_banner.set_revealed(true);
                return;
            }
        };
        let updates = daemon::watch(socket);
        // This future owns the window state; it ends if the watcher thread ever stops.
        let shell = Rc::clone(self);
        glib::spawn_future_local(async move {
            while let Ok(update) = updates.recv().await {
                match update {
                    Update::Status(status) => *shell.ctx.status.borrow_mut() = Some(*status),
                    Update::NotRunning(reason) => {
                        tracing::debug!("daemon unreachable: {reason}");
                        *shell.ctx.status.borrow_mut() = None;
                    }
                }
                shell.refresh();
            }
        });
    }

    /// Redraws everything that depends on the daemon's status.
    fn refresh(&self) {
        let status = self.ctx.status.borrow().clone();
        let connection = Connection::from_status(status.as_ref());
        if self.ctx.socket.is_ok() {
            self.down_banner
                .set_revealed(connection == Connection::NotRunning);
        }
        let error = status.as_ref().and_then(|s| s.content_error.as_deref());
        if let Some(error) = error {
            self.error_banner.set_title(error);
            self.error_banner
                .set_button_label(Some(if self.showing_live() {
                    EDIT_COMMANDS
                } else {
                    CHOOSE_FILE
                }));
        }
        self.error_banner.set_revealed(error.is_some());

        self.status_row.set_title(connection.title());
        self.status_row.set_subtitle(&connection.detail());
        let icon_name = match connection {
            Connection::Connected(_) => icons::PANEL,
            Connection::Searching(_) => icons::SEARCHING,
            Connection::Blocked(_) => icons::WARNING,
            Connection::NotRunning | Connection::Idle => icons::PANEL_OFF,
        };
        self.status_icon.set_icon_name(Some(icon(icon_name)));

        match &status {
            Some(status) => {
                self.brightness.follow(status.brightness);
                if let Some(step) = status.rotation {
                    self.rotation.follow(step);
                    if self.editor_rotation.replace(Some(step)) != Some(step) {
                        self.static_page.set_rotation(step);
                        self.boot_page.set_rotation(step);
                        self.area_page.set_rotation(step);
                    }
                }
            }
            None => self.brightness.update_label(),
        }
        self.refresh_profiles(status.as_ref());
        self.tray.sync(&self.ctx);
        self.static_page.refresh(status.as_ref());
        self.boot_page.refresh();
        if self.area_page.refresh(status.as_ref()) {
            self.show_area();
        }
    }

    /// Follows the profile status: refetches the list when the set changed, makes the editors
    /// re-adopt the daemon's content after an activation.
    fn refresh_profiles(&self, status: Option<&Status>) {
        let profile = status.and_then(|status| status.profile.as_ref());
        if let Some(profile) = profile {
            if self.profile_revision.get() != Some(profile.revision) {
                self.profile_revision.set(Some(profile.revision));
                let page = self.profiles_page.clone();
                let switcher = self.profile_switcher.clone();
                let tray = Rc::clone(&self.tray);
                let ctx = Rc::clone(&self.ctx);
                self.ctx.reload_profiles(move || {
                    page.set_profiles();
                    switcher.set_profiles();
                    tray.sync(&ctx);
                });
            }
            let seen = self.profile_activation.replace(Some(profile.activation));
            if seen.is_some_and(|seen| profile.activation > seen) {
                self.static_page.readopt();
                self.area_page.readopt();
            }
        }
        self.profiles_page.refresh(status);
        self.profile_switcher.follow(profile);
    }

    /// Display > Area, when the daemon already streams a live source.
    fn show_area(&self) {
        self.content_stack.set_visible_child_name("display");
        self.switcher.set_stack(Some(&self.display_views));
        self.display_views.set_visible_child_name("area");
    }
}
