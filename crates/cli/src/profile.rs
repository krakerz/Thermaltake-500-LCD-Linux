//! `tt500-lcd profile ...`: save, switch, order and schedule display profiles.

use std::fmt::Write;

use anyhow::Context;
use tt500_lcd_ipc::{
    Builtin, Client, Profile, ProfileNameParams, ProfileRenameParams, ProfileReorderParams,
    ProfileSaveParams, ProfileSource, ProfileTriggersParams, ProfilesReply, RangeEntry, Request,
    ScheduleEntry, TimeOfDay, Triggers, Weekday, socket_path,
};

use crate::cli::{MoveTarget, ProfileAction, TriggerAction, expand_days};
use crate::describe;
use crate::format::format_live;

/// A change to a profile's triggers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerEdit {
    /// Add a time of day; an entry with the same time is replaced.
    AddTime { time: TimeOfDay, days: Vec<Weekday> },
    /// Add a time range; an entry with the same start and end is replaced.
    AddRange {
        start: TimeOfDay,
        end: TimeOfDay,
        days: Vec<Weekday>,
    },
    /// Add a process name; a repeated name changes nothing.
    AddProcess(String),
    /// Remove the entry at this time.
    RemoveTime(TimeOfDay),
    /// Remove the range with this start and end.
    RemoveRange { start: TimeOfDay, end: TimeOfDay },
    /// Remove this process name.
    RemoveProcess(String),
    /// Remove everything.
    Clear,
}

/// What an edit did, for the message printed afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditNote {
    Added,
    Replaced,
    Unchanged,
    Removed,
    Cleared,
}

/// Applies `edit` to `triggers`.
///
/// # Errors
///
/// Returns a message when something to remove does not exist or the result exceeds the
/// daemon's limits.
pub fn edit_triggers(triggers: &mut Triggers, edit: TriggerEdit) -> Result<EditNote, String> {
    let note = match edit {
        TriggerEdit::AddTime { time, days } => {
            let entry = ScheduleEntry { time, days };
            match triggers.schedule.iter_mut().find(|e| e.time == time) {
                Some(existing) if *existing == entry => EditNote::Unchanged,
                Some(existing) => {
                    *existing = entry;
                    EditNote::Replaced
                }
                None => {
                    triggers.schedule.push(entry);
                    EditNote::Added
                }
            }
        }
        TriggerEdit::AddRange { start, end, days } => {
            let entry = RangeEntry { start, end, days };
            entry.validate()?;
            match triggers
                .ranges
                .iter_mut()
                .find(|e| e.start == start && e.end == end)
            {
                Some(existing) if *existing == entry => EditNote::Unchanged,
                Some(existing) => {
                    *existing = entry;
                    EditNote::Replaced
                }
                None => {
                    triggers.ranges.push(entry);
                    EditNote::Added
                }
            }
        }
        TriggerEdit::AddProcess(process) => {
            if triggers.processes.contains(&process) {
                EditNote::Unchanged
            } else {
                triggers.processes.push(process);
                EditNote::Added
            }
        }
        TriggerEdit::RemoveTime(time) => {
            let before = triggers.schedule.len();
            triggers.schedule.retain(|e| e.time != time);
            if triggers.schedule.len() == before {
                return Err(format!("no time trigger at {time}"));
            }
            EditNote::Removed
        }
        TriggerEdit::RemoveRange { start, end } => {
            let before = triggers.ranges.len();
            triggers.ranges.retain(|e| e.start != start || e.end != end);
            if triggers.ranges.len() == before {
                return Err(format!("no time range {start}-{end}"));
            }
            EditNote::Removed
        }
        TriggerEdit::RemoveProcess(process) => {
            let before = triggers.processes.len();
            triggers.processes.retain(|p| *p != process);
            if triggers.processes.len() == before {
                return Err(format!("no process trigger {process:?}"));
            }
            EditNote::Removed
        }
        TriggerEdit::Clear => {
            *triggers = Triggers::default();
            EditNote::Cleared
        }
    };
    triggers.validate()?;
    Ok(note)
}

/// The list order after moving `name` to `target`.
///
/// Moving `up` from the first or `down` from the last place changes nothing.
///
/// # Errors
///
/// Returns a message for an unknown name or a position outside `1..=len`.
pub fn moved_order(
    names: &[String],
    name: &str,
    target: MoveTarget,
) -> Result<Vec<String>, String> {
    let from = position_of(names, name).ok_or_else(|| format!("no profile named {name:?}"))?;
    let to = match target {
        MoveTarget::Position(n) if (1..=names.len()).contains(&n) => n - 1,
        MoveTarget::Position(n) => {
            return Err(format!(
                "position {n} is outside 1 to {}, the number of profiles",
                names.len()
            ));
        }
        MoveTarget::Up => from.saturating_sub(1),
        MoveTarget::Down => (from + 1).min(names.len() - 1),
    };
    let mut order = names.to_vec();
    let moved = order.remove(from);
    order.insert(to, moved);
    Ok(order)
}

fn position_of(names: &[String], name: &str) -> Option<usize> {
    let wanted = name.to_lowercase();
    names.iter().position(|n| n.to_lowercase() == wanted)
}

/// Runs a `profile` subcommand against the daemon.
pub fn run(action: ProfileAction) -> Result<(), anyhow::Error> {
    let socket = socket_path().map_err(|e| anyhow::anyhow!("error: {e}"))?;
    let mut client = Client::connect(&socket).map_err(|e| describe(&e, &socket))?;
    let fail = |e| describe(&e, &socket);
    match action {
        ProfileAction::List { json } => {
            let reply = client.profiles().map_err(fail)?;
            if json {
                let text = serde_json::to_string_pretty(&reply.profiles)
                    .context("error: cannot encode profiles")?;
                println!("{text}");
            } else {
                print!("{}", format_profiles(&reply));
            }
        }
        ProfileAction::Save { name } => {
            let params = ProfileSaveParams { name, spec: None };
            client
                .call(Request::ProfileSave(params.clone()))
                .map_err(fail)?;
            println!("Saved profile {}", params.name);
        }
        ProfileAction::Activate { name } => {
            client
                .call(Request::ProfileActivate(ProfileNameParams {
                    name: name.clone(),
                }))
                .map_err(fail)?;
            println!("Activated {name}");
        }
        ProfileAction::Delete { name } => {
            client
                .call(Request::ProfileDelete(ProfileNameParams {
                    name: name.clone(),
                }))
                .map_err(fail)?;
            println!("Deleted profile {name}");
        }
        ProfileAction::Rename { from, to } => {
            client
                .call(Request::ProfileRename(ProfileRenameParams {
                    from: from.clone(),
                    to: to.clone(),
                }))
                .map_err(fail)?;
            println!("Renamed {from} to {to}");
        }
        ProfileAction::Move { name, position } => {
            let names = profile_names(&client.profiles().map_err(fail)?);
            let names =
                moved_order(&names, &name, position).map_err(|e| anyhow::anyhow!("error: {e}"))?;
            let place = position_of(&names, &name).map_or(0, |i| i + 1);
            client
                .call(Request::ProfileReorder(ProfileReorderParams { names }))
                .map_err(fail)?;
            println!("Moved {name} to position {place}");
        }
        ProfileAction::Reset { name } => {
            client
                .call(Request::ProfileReset(ProfileNameParams {
                    name: name.clone(),
                }))
                .map_err(fail)?;
            println!("Reset {name} to factory settings");
        }
        ProfileAction::Trigger { action } => {
            let (name, edit) = trigger_edit(action);
            let reply = client.profiles().map_err(fail)?;
            let profile = reply
                .profiles
                .iter()
                .find(|p| p.name.to_lowercase() == name.to_lowercase())
                .ok_or_else(|| anyhow::anyhow!("error: no profile named {name:?}"))?;
            let mut triggers = profile.triggers.clone();
            let note =
                edit_triggers(&mut triggers, edit).map_err(|e| anyhow::anyhow!("error: {e}"))?;
            if note != EditNote::Unchanged {
                client
                    .call(Request::ProfileSetTriggers(ProfileTriggersParams {
                        name: profile.name.clone(),
                        triggers,
                    }))
                    .map_err(fail)?;
            }
            println!("{} for {}", note_text(note), profile.name);
        }
    }
    Ok(())
}

fn trigger_edit(action: TriggerAction) -> (String, TriggerEdit) {
    match action {
        TriggerAction::AddTime { name, time, days } => {
            let days = expand_days(&days);
            (name, TriggerEdit::AddTime { time, days })
        }
        TriggerAction::AddRange {
            name,
            start,
            end,
            days,
        } => {
            let days = expand_days(&days);
            (name, TriggerEdit::AddRange { start, end, days })
        }
        TriggerAction::RemoveRange { name, start, end } => {
            (name, TriggerEdit::RemoveRange { start, end })
        }
        TriggerAction::AddProcess { name, process } => (name, TriggerEdit::AddProcess(process)),
        TriggerAction::RemoveTime { name, time } => (name, TriggerEdit::RemoveTime(time)),
        TriggerAction::RemoveProcess { name, process } => {
            (name, TriggerEdit::RemoveProcess(process))
        }
        TriggerAction::Clear { name } => (name, TriggerEdit::Clear),
    }
}

fn note_text(note: EditNote) -> &'static str {
    match note {
        EditNote::Added => "Added trigger",
        EditNote::Replaced => "Replaced trigger",
        EditNote::Unchanged => "Trigger already set",
        EditNote::Removed => "Removed trigger",
        EditNote::Cleared => "Cleared triggers",
    }
}

fn profile_names(reply: &ProfilesReply) -> Vec<String> {
    reply.profiles.iter().map(|p| p.name.clone()).collect()
}

/// The `profile list` table: one row per profile, `*` marks the active one.
pub fn format_profiles(reply: &ProfilesReply) -> String {
    if reply.profiles.is_empty() {
        return "No profiles. Save one with: tt500-lcd profile save NAME\n".to_owned();
    }
    let active = reply
        .status
        .profile
        .as_ref()
        .and_then(|p| p.active.as_ref())
        .map(|a| a.name.as_str());
    let labels: Vec<String> = reply.profiles.iter().map(profile_label).collect();
    let width = labels
        .iter()
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0);
    let mut text = String::new();
    for (profile, label) in reply.profiles.iter().zip(&labels) {
        let marker = if active == Some(profile.name.as_str()) {
            "*"
        } else {
            " "
        };
        let _ = writeln!(
            text,
            "{marker} {label:<width$}  {}  {}",
            source_summary(profile),
            describe_triggers(&profile.triggers),
        );
    }
    if let Some(problem) = reply
        .status
        .profile
        .as_ref()
        .and_then(|p| p.problem.as_ref())
    {
        let _ = writeln!(text, "Problem: {problem}");
    }
    text
}

fn profile_label(profile: &Profile) -> String {
    if profile.builtin == Some(Builtin::Default) {
        format!("{} (built-in)", profile.name)
    } else {
        profile.name.clone()
    }
}

fn source_summary(profile: &Profile) -> String {
    match &profile.spec.source {
        Some(ProfileSource::Static { path }) => path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        ),
        Some(ProfileSource::Live { source }) => format_live(source),
        Some(ProfileSource::Clear) => "clear".to_owned(),
        None => "keeps the content".to_owned(),
    }
}

fn describe_triggers(triggers: &Triggers) -> String {
    let times: Vec<String> = triggers
        .schedule
        .iter()
        .map(|entry| match days_summary(&entry.days) {
            Some(days) => format!("{} {days}", entry.time),
            None => entry.time.to_string(),
        })
        .collect();
    let mut parts = Vec::new();
    if !times.is_empty() {
        parts.push(times.join(", "));
    }
    let ranges: Vec<String> = triggers
        .ranges
        .iter()
        .map(|entry| match days_summary(&entry.days) {
            Some(days) => format!("{}-{} {days}", entry.start, entry.end),
            None => format!("{}-{}", entry.start, entry.end),
        })
        .collect();
    if !ranges.is_empty() {
        parts.push(ranges.join(", "));
    }
    if !triggers.processes.is_empty() {
        parts.push(format!("while {} runs", triggers.processes.join(", ")));
    }
    if parts.is_empty() {
        "no triggers".to_owned()
    } else {
        parts.join("; ")
    }
}

/// `Mon-Fri` for a run of three or more consecutive days, else `Sat, Sun`; `None` = every day.
fn days_summary(days: &[Weekday]) -> Option<String> {
    let mut days = days.to_vec();
    days.sort();
    days.dedup();
    if days.is_empty() || days.len() == Weekday::ALL.len() {
        return None;
    }
    let consecutive = days.windows(2).all(|w| w[1].index() == w[0].index() + 1);
    if consecutive && days.len() >= 3 {
        return Some(format!(
            "{}-{}",
            capitalized(days[0]),
            capitalized(days[days.len() - 1])
        ));
    }
    let names: Vec<String> = days.into_iter().map(capitalized).collect();
    Some(names.join(", "))
}

fn capitalized(day: Weekday) -> String {
    let name = day.to_string();
    let mut chars = name.chars();
    chars
        .next()
        .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt500_lcd_ipc::{
        ActiveProfile, DeviceState, IPC_VERSION, ProfileReason, ProfileSpec, ProfileStatus, Status,
    };

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn time(h: u8, m: u8) -> TimeOfDay {
        TimeOfDay::new(h, m).unwrap()
    }

    #[test]
    fn moved_order_cases() {
        let list = names(&["a", "b", "c"]);
        let go = |n: &str, t| moved_order(&list, n, t);
        assert_eq!(
            go("c", MoveTarget::Position(1)),
            Ok(names(&["c", "a", "b"]))
        );
        assert_eq!(
            go("a", MoveTarget::Position(3)),
            Ok(names(&["b", "c", "a"]))
        );
        assert_eq!(go("B", MoveTarget::Down), Ok(names(&["a", "c", "b"])));
        assert_eq!(go("b", MoveTarget::Up), Ok(names(&["b", "a", "c"])));
        assert_eq!(go("a", MoveTarget::Up), Ok(list.clone()));
        assert_eq!(go("c", MoveTarget::Down), Ok(list.clone()));
        assert_eq!(go("b", MoveTarget::Position(2)), Ok(list.clone()));
        assert!(go("a", MoveTarget::Position(4)).is_err());
        assert!(go("zzz", MoveTarget::Up).is_err());
    }

    #[test]
    fn add_time_replaces_same_time() {
        let mut t = Triggers::default();
        let add = |t: &mut Triggers, days: Vec<Weekday>| {
            edit_triggers(
                t,
                TriggerEdit::AddTime {
                    time: time(22, 0),
                    days,
                },
            )
        };
        assert_eq!(add(&mut t, vec![]), Ok(EditNote::Added));
        assert_eq!(add(&mut t, vec![]), Ok(EditNote::Unchanged));
        assert_eq!(add(&mut t, vec![Weekday::Mon]), Ok(EditNote::Replaced));
        assert_eq!(t.schedule.len(), 1);
        assert_eq!(t.schedule[0].days, [Weekday::Mon]);
    }

    #[test]
    fn process_edits() {
        let mut t = Triggers::default();
        let add = |t: &mut Triggers| edit_triggers(t, TriggerEdit::AddProcess("game.exe".into()));
        assert_eq!(add(&mut t), Ok(EditNote::Added));
        assert_eq!(add(&mut t), Ok(EditNote::Unchanged));
        assert_eq!(t.processes.len(), 1);
        let err = edit_triggers(&mut t, TriggerEdit::RemoveProcess("other".into())).unwrap_err();
        assert!(err.contains("other"), "{err}");
        assert_eq!(
            edit_triggers(&mut t, TriggerEdit::RemoveProcess("game.exe".into())),
            Ok(EditNote::Removed)
        );
        let err = edit_triggers(&mut t, TriggerEdit::RemoveTime(time(7, 30))).unwrap_err();
        assert!(err.contains("07:30"), "{err}");
    }

    #[test]
    fn limits_and_clear() {
        let mut t = Triggers::default();
        for i in 0..tt500_lcd_ipc::PROFILE_MAX_SCHEDULE {
            let m = u8::try_from(i).unwrap();
            edit_triggers(
                &mut t,
                TriggerEdit::AddTime {
                    time: time(1, m),
                    days: vec![],
                },
            )
            .unwrap();
        }
        let over = TriggerEdit::AddTime {
            time: time(2, 0),
            days: vec![],
        };
        assert!(edit_triggers(&mut t, over).is_err());
        assert_eq!(
            edit_triggers(&mut t, TriggerEdit::Clear),
            Ok(EditNote::Cleared)
        );
        assert_eq!(t, Triggers::default());
    }

    #[test]
    fn days_are_summarised() {
        assert_eq!(days_summary(&[]), None);
        assert_eq!(days_summary(&Weekday::ALL), None);
        assert_eq!(days_summary(&Weekday::ALL[..5]).as_deref(), Some("Mon-Fri"));
        assert_eq!(
            days_summary(&Weekday::ALL[5..]).as_deref(),
            Some("Sat, Sun")
        );
        assert_eq!(
            days_summary(&[Weekday::Mon, Weekday::Wed]).as_deref(),
            Some("Mon, Wed")
        );
    }

    fn profile(name: &str, triggers: Triggers) -> Profile {
        Profile {
            builtin: None,
            name: name.into(),
            spec: ProfileSpec {
                source: Some(ProfileSource::Static {
                    path: "/pics/cat.png".into(),
                }),
                ..ProfileSpec::default()
            },
            triggers,
        }
    }

    fn reply(profiles: Vec<Profile>, profile: Option<ProfileStatus>) -> ProfilesReply {
        ProfilesReply {
            profiles,
            status: Status {
                daemon_version: "0.1.0".into(),
                ipc_version: IPC_VERSION,
                device: DeviceState::Idle,
                content: None,
                brightness: 0,
                rotation: None,
                content_error: None,
                profile,
            },
        }
    }

    #[test]
    fn table_marks_active_and_lists_triggers() {
        let night = Triggers {
            processes: vec!["game.exe".into()],
            ranges: Vec::new(),
            schedule: vec![
                ScheduleEntry {
                    time: time(22, 0),
                    days: Weekday::ALL[..5].to_vec(),
                },
                ScheduleEntry {
                    time: time(7, 30),
                    days: vec![],
                },
            ],
        };
        let status = ProfileStatus {
            revision: 1,
            activation: 1,
            active: Some(ActiveProfile {
                name: "Night".into(),
                reason: ProfileReason::Manual,
                modified: false,
            }),
            problem: Some("cat.png not found".into()),
        };
        let text = format_profiles(&reply(
            vec![profile("Night", night), profile("Day", Triggers::default())],
            Some(status),
        ));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "* Night  cat.png  22:00 Mon-Fri, 07:30; while game.exe runs"
        );
        assert_eq!(lines[1], "  Day    cat.png  no triggers");
        assert_eq!(lines[2], "Problem: cat.png not found");
    }

    #[test]
    fn builtin_and_ranges_are_listed() {
        let night = Triggers {
            ranges: vec![RangeEntry {
                start: time(22, 0),
                end: time(6, 0),
                days: vec![Weekday::Fri],
            }],
            ..Triggers::default()
        };
        let mut default = profile(tt500_lcd_ipc::DEFAULT_PROFILE_NAME, Triggers::default());
        default.builtin = Some(Builtin::Default);
        let text = format_profiles(&reply(vec![default, profile("Night", night)], None));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "  Default (built-in)  cat.png  no triggers");
        assert_eq!(lines[1], "  Night               cat.png  22:00-06:00 Fri");
    }

    #[test]
    fn range_edits() {
        let (start, end) = (time(22, 0), time(6, 0));
        let mut t = Triggers::default();
        let add = |days: Vec<Weekday>| TriggerEdit::AddRange { start, end, days };
        assert_eq!(edit_triggers(&mut t, add(vec![])), Ok(EditNote::Added));
        assert_eq!(edit_triggers(&mut t, add(vec![])), Ok(EditNote::Unchanged));
        assert_eq!(
            edit_triggers(&mut t, add(vec![Weekday::Fri])),
            Ok(EditNote::Replaced)
        );
        assert_eq!(t.ranges.len(), 1);
        assert_eq!(t.ranges[0].days, [Weekday::Fri]);
        assert!(
            edit_triggers(
                &mut t,
                TriggerEdit::AddRange {
                    start,
                    end: start,
                    days: vec![]
                }
            )
            .is_err()
        );
        assert!(
            edit_triggers(
                &mut t,
                TriggerEdit::RemoveRange {
                    start: time(1, 0),
                    end
                }
            )
            .is_err()
        );
        assert_eq!(
            edit_triggers(&mut t, TriggerEdit::RemoveRange { start, end }),
            Ok(EditNote::Removed)
        );
        assert!(t.ranges.is_empty());
        for h in 0..16 {
            let edit = TriggerEdit::AddRange {
                start: time(h, 0),
                end: time(h, 30),
                days: vec![],
            };
            edit_triggers(&mut t, edit).unwrap();
        }
        let extra = TriggerEdit::AddRange {
            start: time(17, 0),
            end: time(18, 0),
            days: vec![],
        };
        assert!(edit_triggers(&mut t, extra).is_err());
    }

    #[test]
    fn empty_list_hints_at_save() {
        assert!(format_profiles(&reply(vec![], None)).contains("profile save"));
    }
}
