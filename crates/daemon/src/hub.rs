//! The shared status snapshot and its subscribers.

use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};

use tt500_lcd_ipc::{ProfileStatus, Status};

/// The current [`Status`]: read by IPC threads, pushed to subscribers when it changes.
#[derive(Debug)]
pub struct StatusHub {
    current: Mutex<Status>,
    subscribers: Mutex<Vec<Sender<Status>>>,
}

impl StatusHub {
    pub fn new(initial: Status) -> Self {
        Self {
            current: Mutex::new(initial),
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// A copy of the current status.
    pub fn get(&self) -> Status {
        self.current.lock().expect("lock poisoned").clone()
    }

    /// Stores `status`, keeping the profile part it already holds; if that changed anything,
    /// pushes it to every live subscriber.
    pub fn set(&self, mut status: Status) {
        // Lock order everywhere: `current`, then `subscribers`.
        let mut current = self.current.lock().expect("lock poisoned");
        status.profile.clone_from(&current.profile);
        if *current == status {
            return;
        }
        *current = status;
        let mut subscribers = self.subscribers.lock().expect("lock poisoned");
        subscribers.retain(|tx| tx.send(current.clone()).is_ok());
    }

    /// Replaces the profile part of the status; pushes it if it changed.
    pub fn set_profile(&self, profile: Option<ProfileStatus>) {
        let mut current = self.current.lock().expect("lock poisoned");
        if current.profile == profile {
            return;
        }
        current.profile = profile;
        let mut subscribers = self.subscribers.lock().expect("lock poisoned");
        subscribers.retain(|tx| tx.send(current.clone()).is_ok());
    }

    /// The current status and a receiver for every later change, with none missed in between.
    pub fn subscribe(&self) -> (Status, Receiver<Status>) {
        let current = self.current.lock().expect("lock poisoned");
        let (tx, rx) = mpsc::channel();
        self.subscribers.lock().expect("lock poisoned").push(tx);
        (current.clone(), rx)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::TryRecvError;

    use tt500_lcd_ipc::{DeviceState, Status};

    use super::*;

    fn status(brightness: u8) -> Status {
        Status {
            daemon_version: "test".to_owned(),
            ipc_version: 1,
            device: DeviceState::Idle,
            content: None,
            brightness,
            rotation: None,
            content_error: None,
            profile: None,
        }
    }

    #[test]
    fn unchanged_set_pushes_nothing() {
        let hub = StatusHub::new(status(50));
        let (_, rx) = hub.subscribe();
        hub.set(status(50));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn change_reaches_every_subscriber() {
        let hub = StatusHub::new(status(50));
        let (_, a) = hub.subscribe();
        let (_, b) = hub.subscribe();
        hub.set(status(60));
        assert_eq!(a.try_recv(), Ok(status(60)));
        assert_eq!(b.try_recv(), Ok(status(60)));
        assert_eq!(hub.get(), status(60));
    }

    #[test]
    fn dropped_receiver_is_pruned() {
        let hub = StatusHub::new(status(50));
        let (_, rx) = hub.subscribe();
        drop(rx);
        hub.set(status(60));
        assert!(hub.subscribers.lock().expect("lock poisoned").is_empty());
    }

    #[test]
    fn set_keeps_the_profile() {
        let hub = StatusHub::new(status(50));
        let profile = ProfileStatus {
            revision: 2,
            ..ProfileStatus::default()
        };
        hub.set_profile(Some(profile.clone()));
        hub.set(status(60));
        assert_eq!(hub.get().profile, Some(profile));
        assert_eq!(hub.get().brightness, 60);
    }

    #[test]
    fn set_profile_pushes_once() {
        let hub = StatusHub::new(status(50));
        let (_, rx) = hub.subscribe();
        let profile = Some(ProfileStatus::default());
        hub.set_profile(profile.clone());
        hub.set_profile(profile);
        assert!(rx.try_recv().is_ok());
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn subscribe_returns_snapshot() {
        let hub = StatusHub::new(status(50));
        let (snapshot, _rx) = hub.subscribe();
        assert_eq!(snapshot, status(50));
    }
}
