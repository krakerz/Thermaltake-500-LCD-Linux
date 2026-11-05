//! State and helpers shared by every page: the daemon socket, the latest status, toasts, requests.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::{gio, glib};
use tt500_lcd_ipc::{Profile, Request, Status};

use crate::daemon;

/// Seconds a message stays up.
const TOAST_TIMEOUT_SECS: u32 = 2;
/// Upload results carry a longer message (e.g. "plays at the next power-on").
pub(super) const UPLOAD_TOAST_TIMEOUT_SECS: u32 = 6;

/// Wraps a handler so it runs only while `T` is alive.
pub(super) fn weak_call<T: 'static, A>(
    weak: &Weak<T>,
    f: impl Fn(&Rc<T>, A) + 'static,
) -> impl Fn(A) + 'static {
    let weak = weak.clone();
    move |arg| {
        if let Some(target) = weak.upgrade() {
            f(&target, arg);
        }
    }
}

/// A scrolled, width-limited page with `children` stacked vertically.
pub(super) fn page_scroller(children: &[&gtk::Widget]) -> gtk::ScrolledWindow {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    for child in children {
        content.append(*child);
    }
    let clamp = adw::Clamp::builder()
        .maximum_size(820)
        .child(&content)
        .build();
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build()
}

/// What every page needs from the window.
pub(super) struct Context {
    /// The daemon socket, or why it cannot be determined.
    pub socket: Result<PathBuf, String>,
    /// Latest status; `None` while the daemon is unreachable.
    pub status: RefCell<Option<Status>>,
    /// Saved profiles in the daemon's order, as of the last [`Self::reload_profiles`].
    pub profiles: RefCell<Vec<Profile>>,
    pub toasts: adw::ToastOverlay,
    pub window: adw::ApplicationWindow,
    current_toast: RefCell<Option<adw::Toast>>,
}

impl Context {
    pub fn new(window: adw::ApplicationWindow, toasts: adw::ToastOverlay) -> Rc<Self> {
        Rc::new(Self {
            socket: tt500_lcd_ipc::socket_path().map_err(|error| error.to_string()),
            status: RefCell::new(None),
            profiles: RefCell::new(Vec::new()),
            toasts,
            window,
            current_toast: RefCell::new(None),
        })
    }

    /// Whether the daemon answered the last time.
    pub fn daemon_running(&self) -> bool {
        self.status.borrow().is_some()
    }

    pub fn toast(&self, message: &str) {
        self.toast_for(message, TOAST_TIMEOUT_SECS);
    }

    /// Like [`Self::toast`], but stays up for `timeout` seconds (long messages).
    pub fn toast_for(&self, message: &str, timeout: u32) {
        if let Some(previous) = self.current_toast.borrow_mut().take() {
            previous.dismiss();
        }
        let toast = adw::Toast::builder()
            .title(message)
            .timeout(timeout)
            .use_markup(false)
            .build();
        *self.current_toast.borrow_mut() = Some(toast.clone());
        self.toasts.add_toast(toast);
    }

    /// Sends a request on a worker thread; shows `success` (if any) or the error as a toast.
    pub fn send(self: &Rc<Self>, request: Request, success: &'static str) {
        self.send_all(vec![request], success);
    }

    /// Sends requests in order, stopping at the first error; shows `success` (if any) or the error.
    pub fn send_all(self: &Rc<Self>, requests: Vec<Request>, success: &'static str) {
        let socket = match &self.socket {
            Ok(socket) => socket.clone(),
            Err(reason) => {
                self.toast(reason);
                return;
            }
        };
        let ctx = Rc::clone(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || {
                requests
                    .into_iter()
                    .try_for_each(|request| daemon::call(&socket, request).map(drop))
            })
            .await;
            match result {
                Ok(Ok(())) if success.is_empty() => {}
                Ok(Ok(())) => ctx.toast(success),
                Ok(Err(message)) => ctx.toast(&message),
                Err(_) => ctx.toast("The request failed"),
            }
        });
    }

    /// Fetches the profile list on a worker thread, stores it, then calls `done`.
    /// Shows the error as a toast (and does not call `done`) when the request fails.
    pub fn reload_profiles(self: &Rc<Self>, done: impl Fn() + 'static) {
        let socket = match &self.socket {
            Ok(socket) => socket.clone(),
            Err(reason) => {
                self.toast(reason);
                return;
            }
        };
        let ctx = Rc::clone(self);
        glib::spawn_future_local(async move {
            match gio::spawn_blocking(move || daemon::profiles(&socket)).await {
                Ok(Ok(reply)) => {
                    *ctx.profiles.borrow_mut() = reply.profiles;
                    done();
                }
                Ok(Err(message)) => ctx.toast(&message),
                Err(_) => ctx.toast("The request failed"),
            }
        });
    }
}
