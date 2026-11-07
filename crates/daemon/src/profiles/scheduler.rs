//! The scheduler thread: evaluates time and process triggers and applies profiles.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tt500_lcd_ipc::{IpcError, ProfileReason, ProfileSpec};

use super::ProfileHub;
use super::engine::{self, Inputs, Step};
use super::sources::{Clock, ProcessLister};
use crate::server::{self, ServerContext};

/// How often triggers are evaluated. Monotonic, so it stops while the machine is suspended.
pub const TICK: Duration = Duration::from_secs(2);
/// Pause between retries of a failed activation.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(30);
/// How many times a failed activation is retried before giving up.
pub const RETRY_TRIES: u32 = 10;
/// Granularity at which the sleep between ticks notices a shutdown.
const STOP_POLL: Duration = Duration::from_millis(100);

/// Applies a profile's settings to the panel.
pub trait Applier {
    /// Applies `spec`; on error the previous content stays.
    ///
    /// # Errors
    /// Whatever the failing step reports (missing file, screen not up).
    fn apply(&self, spec: &ProfileSpec) -> Result<(), IpcError>;
}

impl Applier for ServerContext {
    fn apply(&self, spec: &ProfileSpec) -> Result<(), IpcError> {
        server::apply_profile(self, spec).map(drop)
    }
}

/// A failed activation waiting for its next attempt.
#[derive(Debug)]
struct Retry {
    name: String,
    reason: ProfileReason,
    retries: u32,
    next: Instant,
    /// Counters when it was armed; a change means someone else decided meanwhile.
    direct_changes: u64,
    activations: u64,
}

/// Evaluates the triggers tick by tick.
#[derive(Debug)]
pub struct Scheduler<C, L> {
    clock: C,
    lister: L,
    retry: Option<Retry>,
}

impl<C: Clock, L: ProcessLister> Scheduler<C, L> {
    /// A scheduler reading time from `clock` and processes from `lister`.
    pub fn new(clock: C, lister: L) -> Self {
        Self {
            clock,
            lister,
            retry: None,
        }
    }

    /// One evaluation. `now_instant` drives the retry timer.
    pub fn step(&mut self, hub: &ProfileHub, applier: &dyn Applier, now_instant: Instant) {
        if hub.is_stopped() {
            return;
        }
        // Manual and scheduled activations never interleave.
        let _guard = hub.lock_activation();
        let wants_processes = hub.list().iter().any(|p| !p.triggers.processes.is_empty());
        let running = if wants_processes {
            self.lister.running()
        } else {
            Default::default()
        };
        let now = self.clock.now();
        let tick = hub.with_tick(|profiles, cursor, active, last_manual| {
            engine::tick(
                cursor,
                &Inputs {
                    profiles,
                    now,
                    running: &running,
                    active,
                    last_manual,
                },
            )
        });
        if tick.forget_manual {
            hub.forget_manual();
        }
        match tick.step {
            Step::Nothing => self.retry_pending(hub, applier, now_instant),
            Step::Activate { name, reason } => {
                self.retry = None;
                self.attempt(hub, applier, now_instant, &name, reason, 0);
            }
            Step::Relabel { reason } => {
                self.retry = None;
                hub.record_relabel(reason);
            }
        }
    }

    fn retry_pending(&mut self, hub: &ProfileHub, applier: &dyn Applier, now_instant: Instant) {
        let Some(retry) = self.retry.take() else {
            return;
        };
        if retry.direct_changes != hub.direct_changes() || retry.activations != hub.activations() {
            return;
        }
        if now_instant < retry.next {
            self.retry = Some(retry);
            return;
        }
        self.attempt(
            hub,
            applier,
            now_instant,
            &retry.name,
            retry.reason,
            retry.retries,
        );
    }

    fn attempt(
        &mut self,
        hub: &ProfileHub,
        applier: &dyn Applier,
        now_instant: Instant,
        name: &str,
        reason: ProfileReason,
        retries: u32,
    ) {
        let Some(profile) = hub.get(name) else {
            return;
        };
        match applier.apply(&profile.spec) {
            Ok(()) => hub.record_applied(&profile.name, reason, false),
            Err(err) => {
                hub.record_problem(format!(
                    "profile {} ({reason}): {}; keeping the previous content",
                    profile.name, err.message
                ));
                if retries < RETRY_TRIES {
                    self.retry = Some(Retry {
                        name: profile.name,
                        reason,
                        retries: retries + 1,
                        next: now_instant + RETRY_INTERVAL,
                        direct_changes: hub.direct_changes(),
                        activations: hub.activations(),
                    });
                }
            }
        }
    }

    /// Runs until the hub is stopped.
    pub fn run(mut self, ctx: &Arc<ServerContext>) {
        while !ctx.profiles.is_stopped() {
            self.step(&ctx.profiles, ctx.as_ref(), Instant::now());
            let wake = Instant::now() + TICK;
            while Instant::now() < wake && !ctx.profiles.is_stopped() {
                thread::sleep(STOP_POLL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::Path;

    use chrono::{NaiveDate, NaiveDateTime};
    use tt500_lcd_ipc::{
        DeviceState, ErrorCode, ProfileSource, ScheduleEntry, Status, TimeOfDay, Triggers,
    };

    use super::*;
    use crate::hub::StatusHub;
    use crate::profiles::sources::fakes::{FakeClock, FakeProcs};

    #[derive(Default)]
    struct FakeApplier {
        applied: RefCell<Vec<ProfileSpec>>,
        fail: RefCell<bool>,
    }

    impl Applier for FakeApplier {
        fn apply(&self, spec: &ProfileSpec) -> Result<(), IpcError> {
            if *self.fail.borrow() {
                return Err(IpcError {
                    code: ErrorCode::BadRequest,
                    message: "no such file".to_owned(),
                });
            }
            self.applied.borrow_mut().push(spec.clone());
            Ok(())
        }
    }

    fn at(day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, day)
            .and_then(|d| d.and_hms_opt(hour, minute, 0))
            .expect("valid date")
    }

    fn status_hub() -> Arc<StatusHub> {
        Arc::new(StatusHub::new(Status {
            daemon_version: "test".to_owned(),
            ipc_version: 1,
            device: DeviceState::Idle,
            content: None,
            brightness: 100,
            rotation: None,
            content_error: None,
            profile: None,
        }))
    }

    fn hub(dir: &Path) -> ProfileHub {
        ProfileHub::load(dir, status_hub(), ProfileSpec::default)
    }

    fn spec(brightness: u8) -> ProfileSpec {
        ProfileSpec {
            brightness: Some(brightness),
            source: Some(ProfileSource::Static {
                path: "/tmp/x.png".into(),
            }),
            ..ProfileSpec::default()
        }
    }

    fn add(hub: &ProfileHub, name: &str, brightness: u8, triggers: Triggers) {
        hub.save(name, spec(brightness)).expect("save");
        hub.set_triggers(name, triggers).expect("triggers");
    }

    fn process(name: &str) -> Triggers {
        Triggers {
            processes: vec![name.to_owned()],
            ranges: Vec::new(),
            schedule: Vec::new(),
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        hub: ProfileHub,
        status: Arc<StatusHub>,
        clock: FakeClock,
        procs: FakeProcs,
        applier: FakeApplier,
        scheduler: Scheduler<FakeClock, FakeProcs>,
        t0: Instant,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock = FakeClock::new(at(2, 12, 0));
        let procs = FakeProcs::default();
        let status = status_hub();
        Fixture {
            hub: ProfileHub::load(dir.path(), Arc::clone(&status), ProfileSpec::default),
            status,
            _dir: dir,
            scheduler: Scheduler::new(clock.clone(), procs.clone()),
            clock,
            procs,
            applier: FakeApplier::default(),
            t0: Instant::now(),
        }
    }

    impl Fixture {
        fn step(&mut self, secs: u64) {
            self.scheduler.step(
                &self.hub,
                &self.applier,
                self.t0 + Duration::from_secs(secs),
            );
        }

        fn problem(&self) -> Option<String> {
            self.status.get().profile.and_then(|p| p.problem)
        }

        fn marker(&self) -> String {
            self.status
                .get()
                .profile
                .and_then(|p| p.active)
                .expect("marker")
                .name
        }

        fn brightnesses(&self) -> Vec<Option<u8>> {
            self.applier
                .applied
                .borrow()
                .iter()
                .map(|s| s.brightness)
                .collect()
        }
    }

    #[test]
    fn process_start_applies_and_exit_falls_back_to_default() {
        let mut f = fixture();
        add(&f.hub, "Game", 80, process("game.exe"));
        f.step(0);
        assert!(f.brightnesses().is_empty());
        f.procs.set(&["game.exe"]);
        f.step(2);
        assert_eq!(f.brightnesses(), [Some(80)]);
        f.step(4);
        assert_eq!(f.brightnesses().len(), 1, "no re-apply without an edge");
        f.procs.set(&[]);
        f.step(6);
        assert_eq!(f.brightnesses().len(), 2, "Default is applied");
        let active = f
            .status
            .get()
            .profile
            .and_then(|p| p.active)
            .expect("marker");
        assert_eq!(active.name, "Default");
        assert_eq!(active.reason, ProfileReason::Fallback);
    }

    #[test]
    fn schedule_edge_applies_once() {
        let mut f = fixture();
        add(
            &f.hub,
            "Night",
            10,
            Triggers {
                processes: Vec::new(),
                ranges: Vec::new(),
                schedule: vec![ScheduleEntry {
                    time: TimeOfDay::new(22, 0).expect("valid time"),
                    days: Vec::new(),
                }],
            },
        );
        f.step(0);
        f.clock.set(at(2, 22, 0));
        f.step(2);
        f.step(4);
        assert_eq!(f.brightnesses(), [Some(10)]);
    }

    #[test]
    fn failure_sets_problem_then_retries_after_the_interval() {
        let mut f = fixture();
        add(&f.hub, "Game", 80, process("game.exe"));
        f.step(0);
        *f.applier.fail.borrow_mut() = true;
        f.procs.set(&["game.exe"]);
        f.step(2);
        assert_eq!(f.marker(), "Default", "the marker did not move");
        assert!(f.problem().is_some());
        *f.applier.fail.borrow_mut() = false;
        f.step(10);
        assert!(f.brightnesses().is_empty(), "too early to retry");
        f.step(2 + RETRY_INTERVAL.as_secs());
        assert_eq!(f.brightnesses(), [Some(80)]);
        assert!(f.problem().is_none());
    }

    #[test]
    fn retries_stop_after_the_limit() {
        let mut f = fixture();
        add(&f.hub, "Game", 80, process("game.exe"));
        f.step(0);
        *f.applier.fail.borrow_mut() = true;
        f.procs.set(&["game.exe"]);
        let mut secs = 2;
        f.step(secs);
        for _ in 0..=RETRY_TRIES + 2 {
            secs += RETRY_INTERVAL.as_secs();
            f.step(secs);
        }
        *f.applier.fail.borrow_mut() = false;
        secs += RETRY_INTERVAL.as_secs();
        f.step(secs);
        assert!(f.brightnesses().is_empty());
    }

    #[test]
    fn direct_change_supersedes_a_pending_retry() {
        let mut f = fixture();
        add(&f.hub, "Game", 80, process("game.exe"));
        f.step(0);
        *f.applier.fail.borrow_mut() = true;
        f.procs.set(&["game.exe"]);
        f.step(2);
        *f.applier.fail.borrow_mut() = false;
        f.hub.note_direct_change();
        f.step(2 + RETRY_INTERVAL.as_secs());
        assert!(f.brightnesses().is_empty());
    }

    #[test]
    fn stop_flag_prevents_apply() {
        let mut f = fixture();
        add(&f.hub, "Game", 80, process("game.exe"));
        f.step(0);
        f.procs.set(&["game.exe"]);
        f.hub.stop();
        f.step(2);
        assert!(f.brightnesses().is_empty());
    }

    #[test]
    fn lister_is_not_used_without_process_triggers() {
        #[derive(Default)]
        struct Panicking;
        impl ProcessLister for Panicking {
            fn running(&mut self) -> std::collections::BTreeSet<String> {
                panic!("the lister must not run");
            }
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let hub = hub(dir.path());
        add(&hub, "Plain", 50, Triggers::default());
        let applier = FakeApplier::default();
        let mut scheduler = Scheduler::new(FakeClock::new(at(2, 12, 0)), Panicking);
        scheduler.step(&hub, &applier, Instant::now());
        assert!(applier.applied.borrow().is_empty());
    }
}
