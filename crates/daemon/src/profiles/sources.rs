//! Time and process sources, behind traits so the scheduler is testable.

use std::collections::BTreeSet;

use chrono::{Local, NaiveDateTime};

/// Wall-clock time in the user's local zone, as a naive date-time.
pub trait Clock {
    /// The current local date and time.
    fn now(&self) -> NaiveDateTime;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalClock;

impl Clock for LocalClock {
    fn now(&self) -> NaiveDateTime {
        Local::now().naive_local()
    }
}

/// Lists the names of running processes.
pub trait ProcessLister {
    /// Names (`comm` and executable basenames) of every running process.
    fn running(&mut self) -> BTreeSet<String>;
}

/// Reads `/proc`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcFs;

impl ProcessLister for ProcFs {
    fn running(&mut self) -> BTreeSet<String> {
        tt500_lcd_ipc::running_process_names()
    }
}

#[cfg(test)]
pub(crate) mod fakes {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A settable clock; clones share the time.
    #[derive(Debug, Clone)]
    pub(crate) struct FakeClock(Arc<Mutex<NaiveDateTime>>);

    impl FakeClock {
        pub(crate) fn new(now: NaiveDateTime) -> Self {
            Self(Arc::new(Mutex::new(now)))
        }

        pub(crate) fn set(&self, now: NaiveDateTime) {
            *self.0.lock().expect("lock poisoned") = now;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> NaiveDateTime {
            *self.0.lock().expect("lock poisoned")
        }
    }

    /// A settable process list; clones share it.
    #[derive(Debug, Clone, Default)]
    pub(crate) struct FakeProcs(Arc<Mutex<BTreeSet<String>>>);

    impl FakeProcs {
        pub(crate) fn set(&self, names: &[&str]) {
            *self.0.lock().expect("lock poisoned") =
                names.iter().map(|n| (*n).to_owned()).collect();
        }
    }

    impl ProcessLister for FakeProcs {
        fn running(&mut self) -> BTreeSet<String> {
            self.0.lock().expect("lock poisoned").clone()
        }
    }

    #[test]
    fn fakes_share_state_between_clones() {
        let clock = FakeClock::new(NaiveDateTime::default());
        let other = clock.clone();
        let later = NaiveDateTime::default() + chrono::TimeDelta::hours(1);
        other.set(later);
        assert_eq!(clock.now(), later);

        let procs = FakeProcs::default();
        let mut lister = procs.clone();
        procs.set(&["a"]);
        assert!(lister.running().contains("a"));
    }
}
