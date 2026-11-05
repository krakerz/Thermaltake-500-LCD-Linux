//! The system-tray icon: StatusNotifierItem service, menu model and icons. No GTK here.
//!
//! The service runs on its own thread; it only sends [`Action`]s over a channel. The GTK main
//! thread receives them and does the work.

use async_channel::Sender;
use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use tt500_lcd_ipc::{Profile, Status};

use crate::state::Connection;

/// Tray icon renders, smallest first (see `scripts/update-tray-icons.sh`).
const ICON_PNGS: [&[u8]; 3] = [
    include_bytes!("../data/tray/tt500-22.png"),
    include_bytes!("../data/tray/tt500-32.png"),
    include_bytes!("../data/tray/tt500-48.png"),
];

/// Brightness choices of the tray menu: label and percent.
const BRIGHTNESS_STEPS: [(&str, u8); 5] = [
    ("Off", 0),
    ("25 %", 25),
    ("50 %", 50),
    ("75 %", 75),
    ("100 %", 100),
];

/// What a click in the tray asks the main thread to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Show and raise the window.
    Open,
    /// Hide the window if shown, else show it.
    Toggle,
    /// Activate the profile with this name.
    ActivateProfile(String),
    /// Set the panel brightness in percent.
    SetBrightness(u8),
    /// Clear the panel.
    ClearPanel,
    /// Quit the app.
    Quit,
}

/// What the tray shows: everything the menu and tooltip depend on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrayState {
    /// Whether the daemon answers; the daemon-bound entries are disabled otherwise.
    pub daemon_up: bool,
    /// Short panel status, e.g. "Connected".
    pub panel: String,
    /// Saved profile names in the daemon's order.
    pub profiles: Vec<String>,
    /// Name of the active profile.
    pub active: Option<String>,
    /// Stored brightness in percent.
    pub brightness: Option<u8>,
}

impl TrayState {
    /// The state for the latest `status` (`None` while the daemon is unreachable).
    #[must_use]
    pub fn from_status(status: Option<&Status>, profiles: &[Profile]) -> Self {
        Self {
            daemon_up: status.is_some(),
            panel: Connection::from_status(status).title().to_owned(),
            profiles: profiles
                .iter()
                .map(|profile| profile.name.clone())
                .collect(),
            active: status
                .and_then(|status| status.profile.as_ref())
                .and_then(|profile| profile.active.as_ref())
                .map(|active| active.name.clone()),
            brightness: status.map(|status| status.brightness),
        }
    }

    /// Tooltip text: the panel status and the active profile.
    #[must_use]
    pub fn tooltip(&self) -> String {
        match &self.active {
            Some(name) => format!("{}\nProfile: {name}", self.panel),
            None => format!("{}\nNo active profile", self.panel),
        }
    }
}

/// One menu entry, independent of the tray library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A plain item.
    Item {
        label: String,
        enabled: bool,
        action: Action,
    },
    /// A submenu of radio items; `selected` indexes `options`.
    Choice {
        label: String,
        enabled: bool,
        options: Vec<(String, Action)>,
        selected: Option<usize>,
    },
    /// A separator line.
    Separator,
}

/// The menu for `state`.
#[must_use]
pub fn menu_entries(state: &TrayState) -> Vec<Entry> {
    let profiles = state
        .profiles
        .iter()
        .map(|name| (name.clone(), Action::ActivateProfile(name.clone())))
        .collect();
    let selected = state
        .active
        .as_ref()
        .and_then(|active| state.profiles.iter().position(|name| name == active));
    let brightness = BRIGHTNESS_STEPS
        .iter()
        .map(|&(label, value)| (label.to_owned(), Action::SetBrightness(value)))
        .collect();
    let level = state
        .brightness
        .and_then(|now| BRIGHTNESS_STEPS.iter().position(|&(_, value)| value == now));
    vec![
        Entry::Item {
            label: "Open TT500 LCD".to_owned(),
            enabled: true,
            action: Action::Open,
        },
        Entry::Choice {
            label: "Profile".to_owned(),
            enabled: state.daemon_up && !state.profiles.is_empty(),
            options: profiles,
            selected,
        },
        Entry::Choice {
            label: "Brightness".to_owned(),
            enabled: state.daemon_up,
            options: brightness,
            selected: level,
        },
        Entry::Item {
            label: "Clear Panel".to_owned(),
            enabled: state.daemon_up,
            action: Action::ClearPanel,
        },
        Entry::Separator,
        Entry::Item {
            label: "Quit".to_owned(),
            enabled: true,
            action: Action::Quit,
        },
    ]
}

/// Converts RGBA pixels to ARGB32 in network byte order (A, R, G, B), as the protocol wants.
#[must_use]
pub fn argb32_from_rgba(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4)
        .flat_map(|pixel| [pixel[3], pixel[0], pixel[1], pixel[2]])
        .collect()
}

fn decode_icon(png: &[u8]) -> Result<ksni::Icon, image::ImageError> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)?.into_rgba8();
    let (width, height) = image.dimensions();
    Ok(ksni::Icon {
        width: i32::try_from(width).unwrap_or(0),
        height: i32::try_from(height).unwrap_or(0),
        data: argb32_from_rgba(image.as_raw()),
    })
}

/// The embedded icon renders; one that fails to decode is logged and skipped.
fn icons() -> Vec<ksni::Icon> {
    ICON_PNGS
        .iter()
        .filter_map(|png| {
            decode_icon(png)
                .inspect_err(|error| tracing::warn!("cannot decode a tray icon: {error}"))
                .ok()
        })
        .collect()
}

/// The StatusNotifierItem. Lives on the tray thread, so it holds no GTK objects.
#[derive(Debug)]
pub struct Tt500Tray {
    state: TrayState,
    icons: Vec<ksni::Icon>,
    actions: Sender<Action>,
}

impl Tt500Tray {
    fn send(&self, action: Action) {
        // The channel is unbounded; it only fails once the app is shutting down.
        if self.actions.try_send(action).is_err() {
            tracing::debug!("tray action dropped: the app is closing");
        }
    }

    fn convert(&self, entry: Entry) -> MenuItem<Self> {
        match entry {
            Entry::Separator => MenuItem::Separator,
            Entry::Item {
                label,
                enabled,
                action,
            } => StandardItem {
                label,
                enabled,
                activate: Box::new(move |tray: &mut Self| tray.send(action.clone())),
                ..Default::default()
            }
            .into(),
            Entry::Choice {
                label,
                enabled,
                options,
                selected,
            } => {
                let (items, actions): (Vec<_>, Vec<_>) = options
                    .into_iter()
                    .map(|(label, action)| {
                        (
                            RadioItem {
                                label,
                                enabled,
                                ..Default::default()
                            },
                            action,
                        )
                    })
                    .unzip();
                let group = RadioGroup {
                    // Out of range marks no option.
                    selected: selected.unwrap_or(usize::MAX),
                    select: Box::new(move |tray: &mut Self, index| {
                        if let Some(action) = actions.get(index) {
                            tray.send(action.clone());
                        }
                    }),
                    options: items,
                };
                SubMenu {
                    label,
                    enabled,
                    submenu: vec![group.into()],
                    ..Default::default()
                }
                .into()
            }
        }
    }
}

impl ksni::Tray for Tt500Tray {
    fn id(&self) -> String {
        crate::APP_ID.to_owned()
    }

    fn title(&self) -> String {
        "TT500 LCD".to_owned()
    }

    fn icon_name(&self) -> String {
        // Hosts prefer a name over the pixmap and draw a "missing icon" placeholder when the
        // name isn't in the icon theme (e.g. running from the build tree), so only name it
        // once the app icon is installed.
        if app_icon_installed() {
            crate::APP_ID.to_owned()
        } else {
            String::new()
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icons.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "TT500 LCD".to_owned(),
            description: self.state.tooltip(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Action::Toggle);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        menu_entries(&self.state)
            .into_iter()
            .map(|entry| self.convert(entry))
            .collect()
    }
}

/// Whether the app icon is in a hicolor theme dir (`$XDG_DATA_HOME`, then `$XDG_DATA_DIRS`).
fn app_icon_installed() -> bool {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".local/share"))
        });
    let dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_owned());
    home.into_iter()
        .chain(
            dirs.split(':')
                .filter(|d| !d.is_empty())
                .map(std::path::PathBuf::from),
        )
        .any(|dir| {
            let apps = dir.join("icons/hicolor/scalable/apps");
            apps.join(format!("{}.svg", crate::APP_ID)).is_file()
        })
}

/// A running tray. Dropping it removes the icon.
pub struct Tray {
    handle: ksni::blocking::Handle<Tt500Tray>,
}

impl Tray {
    /// Registers the icon with the session's StatusNotifierHost. Blocks on D-Bus: call it off
    /// the GTK thread.
    ///
    /// # Errors
    /// Fails when there is no session bus, no StatusNotifierWatcher or no host to show it.
    pub fn spawn(state: TrayState, actions: Sender<Action>) -> Result<Self, ksni::Error> {
        let handle = Tt500Tray {
            state,
            icons: icons(),
            actions,
        }
        .spawn()?;
        Ok(Self { handle })
    }

    /// Shows `state` in the menu and tooltip.
    pub fn update(&self, state: TrayState) {
        self.handle.update(|tray| tray.state = state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> TrayState {
        TrayState {
            daemon_up: true,
            panel: "Connected".to_owned(),
            profiles: vec!["Day".to_owned(), "Night".to_owned()],
            active: Some("Night".to_owned()),
            brightness: Some(50),
        }
    }

    #[test]
    fn argb_moves_alpha_first() {
        assert_eq!(
            argb32_from_rgba(&[1, 2, 3, 4, 10, 20, 30, 40]),
            [4, 1, 2, 3, 40, 10, 20, 30]
        );
    }

    #[test]
    fn embedded_icons_decode_at_their_sizes() {
        let sizes: Vec<(i32, usize)> = icons()
            .iter()
            .map(|icon| (icon.width, icon.data.len()))
            .collect();
        assert_eq!(
            sizes,
            [(22, 22 * 22 * 4), (32, 32 * 32 * 4), (48, 48 * 48 * 4)]
        );
        assert!(icons().iter().all(|icon| icon.width == icon.height));
    }

    #[test]
    fn menu_follows_profiles_in_daemon_order() {
        let entries = menu_entries(&state());
        let Entry::Choice {
            options,
            selected,
            enabled,
            ..
        } = &entries[1]
        else {
            panic!("second entry is the profile submenu: {entries:?}");
        };
        assert!(enabled);
        assert_eq!(*selected, Some(1));
        assert_eq!(
            options,
            &[
                ("Day".to_owned(), Action::ActivateProfile("Day".to_owned())),
                (
                    "Night".to_owned(),
                    Action::ActivateProfile("Night".to_owned())
                ),
            ]
        );
    }

    #[test]
    fn brightness_marks_only_an_exact_step() {
        let selected = |brightness| {
            let state = TrayState {
                brightness,
                ..state()
            };
            match &menu_entries(&state)[2] {
                Entry::Choice { selected, .. } => *selected,
                other => panic!("not a submenu: {other:?}"),
            }
        };
        assert_eq!(selected(Some(0)), Some(0));
        assert_eq!(selected(Some(75)), Some(3));
        assert_eq!(selected(Some(60)), None);
        assert_eq!(selected(None), None);
    }

    #[test]
    fn daemon_entries_are_disabled_while_it_is_down() {
        let down = TrayState::from_status(None, &[]);
        assert_eq!(down.panel, "Daemon not running");
        let entries = menu_entries(&down);
        let enabled: Vec<bool> = entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Item { enabled, .. } | Entry::Choice { enabled, .. } => Some(*enabled),
                Entry::Separator => None,
            })
            .collect();
        // Open, Profile, Brightness, Clear Panel, Quit.
        assert_eq!(enabled, [true, false, false, false, true]);
    }

    #[test]
    fn menu_ends_with_a_separator_and_quit() {
        let entries = menu_entries(&state());
        assert_eq!(entries.len(), 6);
        assert_eq!(entries[4], Entry::Separator);
        assert!(matches!(
            &entries[5],
            Entry::Item {
                action: Action::Quit,
                ..
            }
        ));
    }

    #[test]
    fn tooltip_names_status_and_profile() {
        assert_eq!(state().tooltip(), "Connected\nProfile: Night");
        let none = TrayState {
            active: None,
            ..state()
        };
        assert_eq!(none.tooltip(), "Connected\nNo active profile");
    }
}
