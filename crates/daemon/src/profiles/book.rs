//! The saved profiles, which one is active, and their persistence in `profiles.toml`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tt500_lcd_ipc::{
    ActiveProfile, Builtin, DEFAULT_PROFILE_NAME, ErrorCode, IpcError, PROFILE_MAX_PROFILES,
    Profile, ProfileReason, ProfileSource, ProfileSpec, ProfileStatus, Triggers, validate_name,
};

use super::engine::{Active, Cursor};
use crate::hub::StatusHub;
use crate::store;

/// Name of the file inside the state directory.
const PROFILES_FILE: &str = "profiles.toml";
/// Version written to the file.
const FILE_VERSION: u32 = 2;

/// The persisted form. Scalars first: TOML needs them before tables.
#[derive(Debug, Serialize, Deserialize)]
struct FileData {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_manual: Option<String>,
    #[serde(default, rename = "profile")]
    profiles: Vec<Profile>,
}

/// What is saved and marked; edited on a copy so a failed write changes nothing.
#[derive(Debug, Clone, Default)]
struct Data {
    profiles: Vec<Profile>,
    active: Option<Active>,
    last_manual: Option<String>,
}

#[derive(Debug)]
struct Book {
    data: Data,
    cursor: Cursor,
    revision: u64,
    activation: u64,
    problem: Option<String>,
    /// Counts direct (non-profile) content changes, so retries can tell they were superseded.
    direct_changes: u64,
    /// The content was changed by hand since the active profile was applied. Not persisted:
    /// after a restart the stored content is shown as it is.
    modified: bool,
}

/// The profile set, shared by IPC threads and the scheduler.
#[derive(Debug)]
pub struct ProfileHub {
    book: Mutex<Book>,
    activation: Mutex<()>,
    status: Arc<StatusHub>,
    path: PathBuf,
    stopped: AtomicBool,
}

fn bad_request(message: impl Into<String>) -> IpcError {
    IpcError {
        code: ErrorCode::BadRequest,
        message: message.into(),
    }
}

fn reserved_name() -> IpcError {
    bad_request(format!(
        "the name {DEFAULT_PROFILE_NAME:?} belongs to the built-in profile"
    ))
}

fn not_found(name: &str) -> IpcError {
    IpcError {
        code: ErrorCode::NotFound,
        message: format!("no profile named {name:?}"),
    }
}

fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

fn is_builtin(profile: &Profile) -> bool {
    profile.builtin == Some(Builtin::Default)
}

/// What the built-in Default profile is reset to: clear the panel, full brightness, no
/// rotation, no triggers.
fn factory_spec() -> ProfileSpec {
    ProfileSpec {
        source: Some(ProfileSource::Clear),
        brightness: Some(100),
        rotation: Some(0),
        ..ProfileSpec::default()
    }
}

fn position(profiles: &[Profile], name: &str) -> Option<usize> {
    profiles.iter().position(|p| same_name(&p.name, name))
}

impl ProfileHub {
    /// Reads `profiles.toml` from `state_dir` and publishes the initial profile status. An
    /// unreadable file is renamed to `profiles.toml.bad` and the set starts empty.
    ///
    /// A set without the built-in Default profile (first start, or a version 1 file) is
    /// migrated: a user profile named "Default" becomes "Default (2)" or the next free
    /// number, and Default is inserted first with the spec from `seed`.
    pub fn load(
        state_dir: &Path,
        status: Arc<StatusHub>,
        seed: impl FnOnce() -> ProfileSpec,
    ) -> Self {
        let path = state_dir.join(PROFILES_FILE);
        let mut data = read_file(&path);
        let migrated = migrate(&mut data, seed);
        let hub = Self {
            book: Mutex::new(Book {
                data,
                cursor: Cursor::new(),
                revision: 1,
                activation: 0,
                problem: None,
                direct_changes: 0,
                modified: false,
            }),
            activation: Mutex::new(()),
            status,
            path,
            stopped: AtomicBool::new(false),
        };
        {
            let book = hub.lock();
            if migrated {
                hub.save_markers(&book);
            }
            hub.publish(&book);
        }
        hub
    }

    /// Held across every apply, so manual and scheduled activations never interleave.
    pub fn lock_activation(&self) -> MutexGuard<'_, ()> {
        self.activation.lock().expect("lock poisoned")
    }

    fn lock(&self) -> MutexGuard<'_, Book> {
        self.book.lock().expect("lock poisoned")
    }

    /// All profiles in list order.
    pub fn list(&self) -> Vec<Profile> {
        self.lock().data.profiles.clone()
    }

    /// The profile called `name` (case-insensitive).
    pub fn get(&self, name: &str) -> Option<Profile> {
        let book = self.lock();
        position(&book.data.profiles, name).map(|i| book.data.profiles[i].clone())
    }

    /// Creates a profile, or replaces the spec of an existing one (triggers are kept).
    ///
    /// # Errors
    /// `BadRequest` for a bad name or spec, a name that only the built-in Default may have,
    /// or when the profile limit is reached.
    pub fn save(&self, name: &str, spec: ProfileSpec) -> Result<(), IpcError> {
        let name = validate_name(name).map_err(bad_request)?;
        spec.validate().map_err(bad_request)?;
        self.edit(|data| {
            if let Some(i) = position(&data.profiles, &name) {
                if same_name(&name, DEFAULT_PROFILE_NAME) && !is_builtin(&data.profiles[i]) {
                    return Err(reserved_name());
                }
                data.profiles[i].spec = spec;
            } else {
                if same_name(&name, DEFAULT_PROFILE_NAME) {
                    return Err(reserved_name());
                }
                if data.profiles.len() >= PROFILE_MAX_PROFILES {
                    return Err(bad_request(format!(
                        "at most {PROFILE_MAX_PROFILES} profiles are allowed"
                    )));
                }
                data.profiles.push(Profile {
                    name,
                    builtin: None,
                    spec,
                    triggers: Triggers::default(),
                });
            }
            Ok(())
        })
    }

    /// Puts the profiles in the given order, atomically. `names` must list every existing
    /// profile exactly once (case-insensitive).
    ///
    /// # Errors
    /// `BadRequest` for an unknown or repeated name or a list of the wrong length.
    pub fn reorder(&self, names: &[String]) -> Result<(), IpcError> {
        self.edit(|data| {
            let mut order = Vec::with_capacity(names.len());
            for name in names {
                let Some(i) = position(&data.profiles, name) else {
                    return Err(bad_request(format!(
                        "unknown profile {name:?}; the profile list changed, reload"
                    )));
                };
                if order.contains(&i) {
                    return Err(bad_request(format!("profile {name:?} is listed twice")));
                }
                order.push(i);
            }
            if order.len() != data.profiles.len() {
                return Err(bad_request("the profile list changed, reload"));
            }
            let mut old: Vec<Option<Profile>> = data.profiles.drain(..).map(Some).collect();
            data.profiles = order
                .into_iter()
                .map(|i| old[i].take().expect("each index is used once"))
                .collect();
            Ok(())
        })
    }

    /// Renames a profile; the marker follows it.
    ///
    /// # Errors
    /// `NotFound` for an unknown `from`, `BadRequest` for a bad or taken `to`, or for the
    /// built-in Default.
    pub fn rename(&self, from: &str, to: &str) -> Result<(), IpcError> {
        let to = validate_name(to).map_err(bad_request)?;
        self.edit(|data| {
            let i = position(&data.profiles, from).ok_or_else(|| not_found(from))?;
            if is_builtin(&data.profiles[i]) {
                return Err(bad_request("the Default profile cannot be renamed"));
            }
            if same_name(&to, DEFAULT_PROFILE_NAME) {
                return Err(reserved_name());
            }
            if position(&data.profiles, &to).is_some_and(|j| j != i) {
                return Err(bad_request(format!("a profile named {to:?} exists")));
            }
            let old = std::mem::replace(&mut data.profiles[i].name, to.clone());
            if let Some(active) = data.active.as_mut()
                && active.name == old
            {
                active.name.clone_from(&to);
            }
            if data.last_manual.as_deref() == Some(old.as_str()) {
                data.last_manual = Some(to);
            }
            Ok(())
        })
    }

    /// Deletes a profile; a marker naming it is cleared.
    ///
    /// # Errors
    /// `NotFound` for an unknown name, `BadRequest` for the built-in Default.
    pub fn delete(&self, name: &str) -> Result<(), IpcError> {
        self.edit(|data| {
            let i = position(&data.profiles, name).ok_or_else(|| not_found(name))?;
            if is_builtin(&data.profiles[i]) {
                return Err(bad_request("the Default profile cannot be deleted"));
            }
            let removed = data.profiles.remove(i).name;
            if data.active.as_ref().is_some_and(|a| a.name == removed) {
                data.active = None;
            }
            if data.last_manual.as_deref() == Some(removed.as_str()) {
                data.last_manual = None;
            }
            Ok(())
        })
    }

    /// Replaces the triggers of a profile.
    ///
    /// # Errors
    /// `NotFound` for an unknown name, `BadRequest` for invalid triggers.
    pub fn set_triggers(&self, name: &str, triggers: Triggers) -> Result<(), IpcError> {
        triggers.validate().map_err(bad_request)?;
        self.edit(|data| {
            let i = position(&data.profiles, name).ok_or_else(|| not_found(name))?;
            data.profiles[i].triggers = triggers;
            Ok(())
        })
    }

    /// Resets the built-in Default profile to its factory spec and removes its triggers.
    /// Returns the spec to apply when Default is the active profile.
    ///
    /// # Errors
    /// `NotFound` for an unknown name, `BadRequest` for any profile but Default.
    pub fn reset(&self, name: &str) -> Result<Option<ProfileSpec>, IpcError> {
        let mut to_apply = None;
        self.edit(|data| {
            let i = position(&data.profiles, name).ok_or_else(|| not_found(name))?;
            if !is_builtin(&data.profiles[i]) {
                return Err(bad_request("only the Default profile can be reset"));
            }
            let profile = &mut data.profiles[i];
            profile.spec = factory_spec();
            profile.triggers = Triggers::default();
            if data.active.as_ref().is_some_and(|a| a.name == profile.name) {
                to_apply = Some(profile.spec.clone());
            }
            Ok(())
        })?;
        Ok(to_apply)
    }

    /// A change of the profile set: edit a copy, persist it, swap it in, bump `revision`.
    fn edit(&self, change: impl FnOnce(&mut Data) -> Result<(), IpcError>) -> Result<(), IpcError> {
        let mut book = self.lock();
        let mut data = book.data.clone();
        change(&mut data)?;
        if data.profiles == book.data.profiles {
            // Nothing changed for clients (e.g. the order was already right).
            return Ok(());
        }
        self.persist(&data).map_err(|err| IpcError {
            code: ErrorCode::Internal,
            message: format!("cannot save {}: {err}", self.path.display()),
        })?;
        book.data = data;
        book.revision += 1;
        book.cursor.mark_profiles_changed();
        self.publish(&book);
        Ok(())
    }

    /// Records a finished activation: the marker, `activation`, no problem. A manual one is
    /// also what the schedule falls back to.
    pub fn record_applied(&self, name: &str, reason: ProfileReason, manual: bool) {
        let mut book = self.lock();
        let Some(i) = position(&book.data.profiles, name) else {
            return;
        };
        let name = book.data.profiles[i].name.clone();
        if manual {
            book.data.last_manual = Some(name.clone());
        }
        book.data.active = Some(Active { name, reason });
        book.activation += 1;
        book.modified = false;
        book.problem = None;
        self.save_markers(&book);
        self.publish(&book);
    }

    /// Changes only why the active profile is active.
    pub fn record_relabel(&self, reason: ProfileReason) {
        let mut book = self.lock();
        let Some(active) = book.data.active.as_mut() else {
            return;
        };
        if active.reason == reason {
            return;
        }
        active.reason = reason;
        self.publish(&book);
    }

    /// Remembers why the last activation failed.
    pub fn record_problem(&self, message: String) {
        let mut book = self.lock();
        if book.problem.as_deref() == Some(message.as_str()) {
            return;
        }
        book.problem = Some(message);
        self.publish(&book);
    }

    /// A manual `Show`/`ShowLive`/`Clear`: the marker keeps naming the active profile but
    /// is flagged as changed, and a later fallback goes to Default instead of the last
    /// manually chosen profile.
    pub fn note_direct_change(&self) {
        let mut book = self.lock();
        book.direct_changes += 1;
        let had_manual = book.data.last_manual.take().is_some();
        let newly_modified = book.data.active.is_some() && !book.modified;
        book.modified = book.modified || book.data.active.is_some();
        let had_problem = book.problem.take().is_some();
        if had_manual {
            self.save_markers(&book);
        }
        if newly_modified || had_problem {
            self.publish(&book);
        }
    }

    /// Drops the last manual choice (its own time range ended), so a fallback skips it.
    pub fn forget_manual(&self) {
        let mut book = self.lock();
        if book.data.last_manual.take().is_some() {
            self.save_markers(&book);
        }
    }

    /// How many direct changes happened so far.
    pub fn direct_changes(&self) -> u64 {
        self.lock().direct_changes
    }

    /// How many activations were applied so far.
    pub fn activations(&self) -> u64 {
        self.lock().activation
    }

    /// Runs one engine tick over a consistent view of the book.
    pub fn with_tick<R>(
        &self,
        tick: impl FnOnce(&[Profile], &mut Cursor, Option<&Active>, Option<&str>) -> R,
    ) -> R {
        let mut book = self.lock();
        let Book { data, cursor, .. } = &mut *book;
        tick(
            &data.profiles,
            cursor,
            data.active.as_ref(),
            data.last_manual.as_deref(),
        )
    }

    /// Tells the scheduler to finish.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    /// Whether [`ProfileHub::stop`] was called.
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    fn publish(&self, book: &Book) {
        self.status.set_profile(Some(ProfileStatus {
            revision: book.revision,
            activation: book.activation,
            active: book.data.active.as_ref().map(|a| ActiveProfile {
                name: a.name.clone(),
                reason: a.reason.clone(),
                modified: book.modified,
            }),
            problem: book.problem.clone(),
        }));
    }

    fn save_markers(&self, book: &Book) {
        if let Err(err) = self.persist(&book.data) {
            tracing::warn!("cannot save {}: {err}", self.path.display());
        }
    }

    fn persist(&self, data: &Data) -> io::Result<()> {
        let file = FileData {
            version: FILE_VERSION,
            active: data.active.as_ref().map(|a| a.name.clone()),
            last_manual: data.last_manual.clone(),
            profiles: data.profiles.clone(),
        };
        let text = toml::to_string(&file).map_err(io::Error::other)?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        store::write_atomic(&self.path, text.as_bytes())
    }
}

fn read_file(path: &Path) -> Data {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Data::default(),
        Err(err) => {
            tracing::warn!("cannot read {}: {err}", path.display());
            set_aside(path);
            return Data::default();
        }
    };
    match toml::from_str::<FileData>(&text) {
        Ok(file) => {
            let exists = |name: &String| position(&file.profiles, name).is_some();
            let active = file.active.clone().filter(exists).map(|name| Active {
                name,
                reason: ProfileReason::Manual,
            });
            let last_manual = file.last_manual.clone().filter(exists);
            Data {
                profiles: file.profiles,
                active,
                last_manual,
            }
        }
        Err(err) => {
            tracing::warn!("{} is not valid: {err}", path.display());
            set_aside(path);
            Data::default()
        }
    }
}

/// Adds the built-in Default profile to a set that lacks it. Returns whether it changed.
fn migrate(data: &mut Data, seed: impl FnOnce() -> ProfileSpec) -> bool {
    if data.profiles.iter().any(is_builtin) {
        return false;
    }
    if let Some(i) = position(&data.profiles, DEFAULT_PROFILE_NAME) {
        let free = (2u32..)
            .map(|n| format!("{DEFAULT_PROFILE_NAME} ({n})"))
            .find(|candidate| position(&data.profiles, candidate).is_none())
            .expect("a free name exists");
        let old = std::mem::replace(&mut data.profiles[i].name, free.clone());
        if let Some(active) = data.active.as_mut()
            && same_name(&active.name, &old)
        {
            active.name.clone_from(&free);
        }
        if data
            .last_manual
            .as_deref()
            .is_some_and(|name| same_name(name, &old))
        {
            data.last_manual = Some(free);
        }
    }
    data.profiles.insert(
        0,
        Profile {
            name: DEFAULT_PROFILE_NAME.to_owned(),
            builtin: Some(Builtin::Default),
            spec: seed(),
            triggers: Triggers::default(),
        },
    );
    if data.active.is_none() {
        data.active = Some(Active {
            name: DEFAULT_PROFILE_NAME.to_owned(),
            reason: ProfileReason::Manual,
        });
    }
    true
}

fn set_aside(path: &Path) {
    let mut bad = path.as_os_str().to_owned();
    bad.push(".bad");
    if let Err(err) = fs::rename(path, &bad) {
        tracing::warn!("cannot move {} aside: {err}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use chrono::NaiveDate;
    use tt500_lcd_ipc::{DeviceState, ScheduleEntry, Status, TimeOfDay};

    use super::*;
    use crate::profiles::engine;

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
            ..ProfileSpec::default()
        }
    }

    /// The user profiles' names in order; the built-in Default is left out.
    fn names(hub: &ProfileHub) -> Vec<String> {
        hub.list()
            .into_iter()
            .filter(|p| !is_builtin(p))
            .map(|p| p.name)
            .collect()
    }

    /// A full order for `reorder`: Default first, then `names`.
    fn with_default(names: &[&str]) -> Vec<String> {
        let mut all = vec![DEFAULT_PROFILE_NAME.to_owned()];
        all.extend(names.iter().map(|n| (*n).to_owned()));
        all
    }

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    fn profile_status(hub: &ProfileHub) -> ProfileStatus {
        hub.status.get().profile.expect("profile status")
    }

    fn three(dir: &Path) -> ProfileHub {
        let hub = hub(dir);
        for (name, b) in [("a", 10), ("b", 20), ("c", 30)] {
            hub.save(name, spec(b)).expect("save");
        }
        hub
    }

    fn timed(process: &str) -> Triggers {
        Triggers {
            processes: vec![process.to_owned()],
            ranges: Vec::new(),
            schedule: vec![ScheduleEntry {
                time: TimeOfDay::new(22, 0).expect("time"),
                days: Vec::new(),
            }],
        }
    }

    #[test]
    fn round_trip_keeps_order_markers_and_triggers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hub = three(tmp.path());
        hub.set_triggers("b", timed("game")).expect("triggers");
        hub.reorder(&with_default(&["c", "a", "b"]))
            .expect("reorder");
        hub.record_applied("a", ProfileReason::Manual, true);

        let again = ProfileHub::load(tmp.path(), status_hub(), ProfileSpec::default);
        assert_eq!(names(&again), ["c", "a", "b"]);
        assert_eq!(again.get("B").expect("b").triggers, timed("game"));
        let active = profile_status(&again).active.expect("active");
        assert_eq!(active.name, "a");
        assert_eq!(active.reason, ProfileReason::Manual);
    }

    #[test]
    fn save_existing_keeps_triggers_and_spelling() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hub = hub(tmp.path());
        hub.save("Night", spec(10)).expect("save");
        hub.set_triggers("night", timed("x")).expect("triggers");
        hub.save("NIGHT", spec(20)).expect("save again");
        assert_eq!(names(&hub), ["Night"]);
        let night = hub.get("night").expect("night");
        assert_eq!(night.spec.brightness, Some(20));
        assert_eq!(night.triggers, timed("x"));
    }

    #[test]
    fn reorder_happy_path_bumps_revision_and_changes_tie_break() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hub = three(tmp.path());
        for name in ["a", "b"] {
            hub.set_triggers(name, timed("game")).expect("triggers");
        }
        let running = BTreeSet::from(["game".to_owned()]);
        let list = hub.list();
        let winner = engine::process_winner(&list, &running).expect("winner");
        assert_eq!(winner.0.name, "a");
        let before = profile_status(&hub).revision;
        hub.reorder(&with_default(&["B", "c", "a"]))
            .expect("reorder");
        assert_eq!(names(&hub), ["b", "c", "a"]);
        assert_eq!(profile_status(&hub).revision, before + 1);
        let list = hub.list();
        let winner = engine::process_winner(&list, &running).expect("winner");
        assert_eq!(winner.0.name, "b");
        let now = NaiveDate::from_ymd_opt(2026, 11, 3)
            .and_then(|d| d.and_hms_opt(23, 0, 0))
            .expect("date");
        let latest = engine::latest_schedule(&list, now).expect("schedule");
        assert_eq!(latest.1.name, "b");
    }

    #[test]
    fn reorder_rejects_bad_lists_and_changes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hub = three(tmp.path());
        let before = profile_status(&hub).revision;
        for bad in [
            strings(&["a", "b"]),
            strings(&["a", "b", "c", "d"]),
            strings(&["a", "a", "b"]),
            strings(&["a", "A", "b"]),
            strings(&["a", "b", "x"]),
        ] {
            let err = hub.reorder(&bad).expect_err("rejected");
            assert_eq!(err.code, ErrorCode::BadRequest, "{bad:?}");
            assert_eq!(names(&hub), ["a", "b", "c"], "{bad:?}");
            assert_eq!(profile_status(&hub).revision, before, "{bad:?}");
        }
        let again = ProfileHub::load(tmp.path(), status_hub(), ProfileSpec::default);
        assert_eq!(names(&again), ["a", "b", "c"]);
    }

    #[test]
    fn same_order_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hub = three(tmp.path());
        let before = profile_status(&hub).revision;
        hub.reorder(&with_default(&["a", "b", "c"]))
            .expect("reorder");
        assert_eq!(profile_status(&hub).revision, before);
    }

    #[test]
    fn unreadable_file_is_set_aside() {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::write(tmp.path().join(PROFILES_FILE), "version = [").expect("write");
        let hub = hub(tmp.path());
        assert!(names(&hub).is_empty());
        assert_eq!(hub.list().len(), 1, "only the migrated Default");
        assert!(tmp.path().join("profiles.toml.bad").exists());
    }

    #[test]
    fn rename_and_delete_fix_up_markers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hub = three(tmp.path());
        hub.record_applied("a", ProfileReason::Manual, true);
        hub.rename("A", "Alpha").expect("rename");
        assert_eq!(profile_status(&hub).active.expect("active").name, "Alpha");
        let taken = hub.rename("b", "ALPHA").expect_err("taken");
        assert_eq!(taken.code, ErrorCode::BadRequest);
        let missing = hub.rename("zz", "q").expect_err("missing");
        assert_eq!(missing.code, ErrorCode::NotFound);
        hub.rename("b", "B").expect("case change");
        hub.delete("alpha").expect("delete");
        assert!(profile_status(&hub).active.is_none());
        hub.with_tick(|_, _, active, manual| {
            assert!(active.is_none());
            assert!(manual.is_none());
        });
        let gone = hub.delete("alpha").expect_err("gone");
        assert_eq!(gone.code, ErrorCode::NotFound);
    }

    #[test]
    fn direct_change_flags_the_marker_and_reaches_subscribers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hub = three(tmp.path());
        let (_, rx) = hub.status.subscribe();
        hub.record_applied("b", ProfileReason::Manual, true);
        let pushed = rx.try_recv().expect("pushed");
        let active = pushed.profile.expect("p").active.expect("a");
        assert_eq!(active.name, "b");
        assert!(!active.modified);
        hub.note_direct_change();
        let pushed = rx.try_recv().expect("pushed");
        let active = pushed.profile.expect("p").active.expect("a");
        assert_eq!(active.name, "b");
        assert!(active.modified);
        assert_eq!(hub.direct_changes(), 1);
        hub.with_tick(|_, _, active, manual| {
            assert_eq!(active.expect("active").name, "b");
            assert!(manual.is_none());
        });
        hub.record_applied("b", ProfileReason::Manual, true);
        assert!(!profile_status(&hub).active.expect("a").modified);
    }

    #[test]
    fn fresh_start_seeds_default_and_activates_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hub = ProfileHub::load(tmp.path(), status_hub(), || spec(42));
        let list = hub.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, DEFAULT_PROFILE_NAME);
        assert_eq!(list[0].builtin, Some(Builtin::Default));
        assert_eq!(list[0].spec.brightness, Some(42));
        let active = profile_status(&hub).active.expect("active");
        assert_eq!(active.name, DEFAULT_PROFILE_NAME);
        assert_eq!(active.reason, ProfileReason::Manual);
    }

    #[test]
    fn v1_file_with_a_user_default_is_migrated_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let v1 = "version = 1\nactive = \"default\"\nlast_manual = \"default\"\n\n\
                  [[profile]]\nname = \"default\"\n[profile.spec]\nbrightness = 7\n\n\
                  [[profile]]\nname = \"Default (2)\"\n[profile.spec]\n";
        fs::write(tmp.path().join(PROFILES_FILE), v1).expect("write");
        let hub = ProfileHub::load(tmp.path(), status_hub(), || spec(55));
        let list = hub.list();
        let listed: Vec<_> = list.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(listed, ["Default", "Default (3)", "Default (2)"]);
        assert_eq!(list[0].spec.brightness, Some(55));
        assert_eq!(list[1].spec.brightness, Some(7));
        assert_eq!(profile_status(&hub).active.expect("a").name, "Default (3)");
        hub.with_tick(|_, _, _, manual| assert_eq!(manual, Some("Default (3)")));
        let text = fs::read_to_string(tmp.path().join(PROFILES_FILE)).expect("read");
        assert!(text.contains("version = 2"));
        let again = ProfileHub::load(tmp.path(), status_hub(), || spec(1));
        assert_eq!(again.list(), list, "the second load is a no-op");
    }

    #[test]
    fn default_cannot_be_deleted_renamed_or_shadowed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hub = three(tmp.path());
        for err in [
            hub.delete("default").expect_err("delete"),
            hub.rename("Default", "Home").expect_err("rename"),
            hub.rename("a", "DEFAULT").expect_err("take the name"),
            hub.reset("a").map(drop).expect_err("reset a user profile"),
        ] {
            assert_eq!(err.code, ErrorCode::BadRequest);
        }
        hub.save("Default", spec(9)).expect("save over Default");
        let default = hub.get("Default").expect("default");
        assert_eq!(default.builtin, Some(Builtin::Default));
        assert_eq!(default.spec.brightness, Some(9));
        hub.set_triggers("Default", timed("game"))
            .expect("triggers");
        hub.reorder(&with_default(&["a", "b", "c"]))
            .expect("reorder");
        hub.reorder(&strings(&["c", "Default", "a", "b"]))
            .expect("Default can move");
    }

    #[test]
    fn reset_restores_the_factory_spec_and_bumps_the_revision() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hub = three(tmp.path());
        hub.save("Default", spec(9)).expect("save");
        hub.set_triggers("Default", timed("game"))
            .expect("triggers");
        let before = profile_status(&hub).revision;
        let applied = hub
            .reset("default")
            .expect("reset")
            .expect("Default is active");
        assert_eq!(applied, factory_spec());
        let default = hub.get("Default").expect("default");
        assert_eq!(default.spec, factory_spec());
        assert_eq!(default.triggers, Triggers::default());
        assert_eq!(profile_status(&hub).revision, before + 1);
        hub.record_applied("a", ProfileReason::Manual, true);
        assert!(hub.reset("Default").expect("reset").is_none());
    }

    #[test]
    fn problem_clears_on_success() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hub = three(tmp.path());
        hub.record_problem("boom".to_owned());
        assert_eq!(profile_status(&hub).problem.as_deref(), Some("boom"));
        hub.record_applied("a", ProfileReason::Schedule, false);
        assert!(profile_status(&hub).problem.is_none());
        assert_eq!(hub.activations(), 1);
    }

    #[test]
    fn limits_and_names_are_enforced() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hub = hub(tmp.path());
        assert!(hub.save("  ", spec(1)).is_err());
        for i in 0..PROFILE_MAX_PROFILES - 1 {
            hub.save(&format!("p{i}"), spec(1)).expect("save");
        }
        assert!(hub.save("one more", spec(1)).is_err());
    }
}
