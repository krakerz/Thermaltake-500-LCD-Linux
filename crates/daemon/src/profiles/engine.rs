//! Pure trigger engine: decides, from the clock and running processes, which profile
//! should be on. No I/O; the scheduler feeds it inputs and applies the returned [`Step`].

use std::collections::BTreeSet;

use chrono::{Datelike, Days, NaiveDateTime, TimeDelta};
use tt500_lcd_ipc::{Builtin, Profile, ProfileReason, RangeEntry, process_running};

/// A backward clock jump larger than this is treated as a new start (full resolve);
/// smaller ones (DST fall-back) must not refire anything.
const BACKWARD_JUMP: TimeDelta = TimeDelta::hours(3);

/// How many days back a schedule entry is searched (covers weekday-limited entries).
const LOOKBACK_DAYS: u64 = 7;

const MINUTES_PER_DAY: u32 = 24 * 60;

/// What the engine remembers between ticks.
#[derive(Debug, Clone, Default)]
pub struct Cursor {
    /// Latest schedule instant already handled.
    sched: Option<NaiveDateTime>,
    /// Time of the previous tick; `None` before the first.
    now: Option<NaiveDateTime>,
    /// Profile that won the process contest at the previous tick.
    winner: Option<String>,
    /// Profile that won the time-range contest at the previous tick.
    range_winner: Option<String>,
    /// The profile set changed: resync the schedule cursor without firing.
    dirty: bool,
}

impl Cursor {
    /// A cursor for a freshly started scheduler; the first tick is a full resolve.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Tells the engine the profile list changed (edit, add, remove, reorder).
    pub fn mark_profiles_changed(&mut self) {
        self.dirty = true;
    }
}

/// The profile currently marked active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Active {
    /// Profile name.
    pub name: String,
    /// What switched it on.
    pub reason: ProfileReason,
}

/// Everything one tick looks at.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// All profiles in list order (the tie-break order).
    pub profiles: &'a [Profile],
    /// Current local time.
    pub now: NaiveDateTime,
    /// Names of running processes.
    pub running: &'a BTreeSet<String>,
    /// The active marker, if any.
    pub active: Option<&'a Active>,
    /// The profile last activated by a client, if the screen still shows it.
    pub last_manual: Option<&'a str>,
}

/// What the scheduler should do after a tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Leave everything as it is.
    Nothing,
    /// Apply this profile.
    Activate {
        /// Profile name.
        name: String,
        /// Why.
        reason: ProfileReason,
    },
    /// The wanted profile is already on; only its label changes (no re-render).
    Relabel {
        /// New reason.
        reason: ProfileReason,
    },
}

/// The outcome of one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tick {
    /// What to do with the display.
    pub step: Step,
    /// The last manual choice's own time range ended (or was missed): the scheduler must
    /// clear `last_manual`, so the profile is not revived by the fallback.
    pub forget_manual: bool,
}

/// Whether any time range of `profile` contains `now`.
fn covered(profile: &Profile, now: NaiveDateTime) -> bool {
    profile.triggers.ranges.iter().any(|r| in_range(r, now))
}

/// First profile, in list order, with a trigger process in `running`, and that process.
#[must_use]
pub fn process_winner<'a>(
    profiles: &'a [Profile],
    running: &BTreeSet<String>,
) -> Option<(&'a Profile, &'a str)> {
    profiles.iter().find_map(|profile| {
        profile
            .triggers
            .processes
            .iter()
            .find(|name| process_running(running, name))
            .map(|name| (profile, name.as_str()))
    })
}

/// Whether `now` lies inside `entry`: the range starting today or the one that started
/// yesterday and wraps past midnight. `days` names the day the range starts on.
fn in_range(entry: &RangeEntry, now: NaiveDateTime) -> bool {
    let start = u32::from(entry.start.hour()) * 60 + u32::from(entry.start.minute());
    let end = u32::from(entry.end.hour()) * 60 + u32::from(entry.end.minute());
    let length = TimeDelta::minutes(i64::from((end + MINUTES_PER_DAY - start) % MINUTES_PER_DAY));
    (0..=1).any(|back| {
        let Some(date) = now.date().checked_sub_days(Days::new(back)) else {
            return false;
        };
        let weekday = date.weekday().num_days_from_monday() as usize;
        if !entry.days.is_empty() && !entry.days.iter().any(|d| d.index() == weekday) {
            return false;
        }
        let Some(begin) = date.and_hms_opt(
            u32::from(entry.start.hour()),
            u32::from(entry.start.minute()),
            0,
        ) else {
            return false;
        };
        begin <= now && now - begin < length
    })
}

/// First profile, in list order, with a time range containing `now`, and that range.
#[must_use]
pub fn range_winner(profiles: &[Profile], now: NaiveDateTime) -> Option<(&Profile, &RangeEntry)> {
    profiles.iter().find_map(|profile| {
        profile
            .triggers
            .ranges
            .iter()
            .find(|range| in_range(range, now))
            .map(|range| (profile, range))
    })
}

/// The schedule entry with the latest instant at or before `now`, looking back up to a week.
/// Equal instants go to the profile earlier in the list.
#[must_use]
pub fn latest_schedule(
    profiles: &[Profile],
    now: NaiveDateTime,
) -> Option<(NaiveDateTime, &Profile)> {
    let mut best: Option<(NaiveDateTime, &Profile)> = None;
    for profile in profiles {
        for entry in &profile.triggers.schedule {
            for back in 0..=LOOKBACK_DAYS {
                let Some(date) = now.date().checked_sub_days(Days::new(back)) else {
                    continue;
                };
                let weekday = date.weekday().num_days_from_monday() as usize;
                if !entry.days.is_empty() && !entry.days.iter().any(|d| d.index() == weekday) {
                    continue;
                }
                let Some(instant) = date.and_hms_opt(
                    u32::from(entry.time.hour()),
                    u32::from(entry.time.minute()),
                    0,
                ) else {
                    continue;
                };
                if instant <= now && best.is_none_or(|(at, _)| instant > at) {
                    best = Some((instant, profile));
                }
            }
        }
    }
    best
}

/// Advances the engine by one tick and says what to do.
///
/// Priority (process, then time range, then schedule, then the last manual choice, then the
/// built-in Default) is applied at edges only: a schedule time crossed, the process or range
/// winner changed, or the first tick after start or a
/// large backward clock jump. Between edges whatever is active holds.
///
/// The last manual choice is what a process or range profile falls back to when it ends,
/// but never a profile whose own time range just ended (or ended unseen, before a start or
/// a clock jump): that one is dropped (`forget_manual`) and the fallback skips it.
pub fn tick(cursor: &mut Cursor, inputs: &Inputs<'_>) -> Tick {
    let first = cursor.now.is_none();
    let jumped_back = cursor
        .now
        .is_some_and(|prev| prev - inputs.now > BACKWARD_JUMP);
    let full = first || jumped_back;
    let prev_now = cursor.now;
    let prev_range = cursor.range_winner.clone();

    let schedule = latest_schedule(inputs.profiles, inputs.now);
    let schedule_at = schedule.map(|(at, _)| at);
    let winner = process_winner(inputs.profiles, inputs.running);
    let winner_name = winner.map(|(profile, _)| profile.name.clone());
    let range = range_winner(inputs.profiles, inputs.now);
    let range_name = range.map(|(profile, _)| profile.name.clone());

    let schedule_edge = !full
        && !cursor.dirty
        && schedule_at.is_some_and(|at| cursor.sched.is_none_or(|seen| at > seen));
    let winner_edge = !full && cursor.winner != winner_name;
    let range_edge = !full && cursor.range_winner != range_name;

    if full || cursor.dirty {
        cursor.sched = schedule_at;
    } else if let Some(at) = schedule_at {
        // Never move back on a small backward jump (DST fall-back must not double-fire).
        cursor.sched = Some(cursor.sched.map_or(at, |seen| seen.max(at)));
    }
    cursor.now = Some(inputs.now);
    cursor.winner = winner_name;
    cursor.range_winner = range_name;
    cursor.dirty = false;

    let forget_manual = inputs
        .last_manual
        .and_then(|name| inputs.profiles.iter().find(|p| p.name == name))
        .is_some_and(|p| {
            if p.triggers.ranges.is_empty() || covered(p, inputs.now) {
                return false;
            }
            if full {
                winner.is_none_or(|(w, _)| w.name != p.name)
                    && schedule.is_none_or(|(_, s)| s.name != p.name)
            } else {
                prev_range.as_deref() == Some(p.name.as_str())
                    || prev_now.is_some_and(|prev| covered(p, prev))
            }
        });
    let last_manual = if forget_manual {
        None
    } else {
        inputs.last_manual
    };

    if !(full || schedule_edge || winner_edge || range_edge) {
        return Tick {
            step: Step::Nothing,
            forget_manual,
        };
    }

    let wanted = if let Some((profile, process)) = winner {
        Some((
            profile.name.as_str(),
            ProfileReason::Process {
                process: process.to_owned(),
            },
        ))
    } else if let Some((profile, entry)) = range {
        Some((
            profile.name.as_str(),
            ProfileReason::Range {
                start: entry.start,
                end: entry.end,
            },
        ))
    } else {
        schedule.map(|(_, profile)| (profile.name.as_str(), ProfileReason::Schedule))
    };

    let step = match wanted {
        Some((name, reason)) => match inputs.active {
            Some(active) if active.name == name && active.reason == reason => Step::Nothing,
            Some(active) if active.name == name => Step::Relabel { reason },
            _ => Step::Activate {
                name: name.to_owned(),
                reason,
            },
        },
        None if first && !forget_manual => match inputs.active {
            // Start-up: restored content stays, only its label is corrected.
            Some(active) if active.reason != ProfileReason::Manual => Step::Relabel {
                reason: ProfileReason::Manual,
            },
            _ => Step::Nothing,
        },
        None => match (last_manual, inputs.active) {
            (Some(name), Some(active)) if active.name == name => {
                if active.reason == ProfileReason::Manual {
                    Step::Nothing
                } else {
                    Step::Relabel {
                        reason: ProfileReason::Manual,
                    }
                }
            }
            (Some(name), _) => Step::Activate {
                name: name.to_owned(),
                reason: ProfileReason::Manual,
            },
            (None, _) => fallback(inputs),
        },
    };
    Tick {
        step,
        forget_manual,
    }
}

/// Falls back to the built-in Default profile; `Nothing` when the list has none.
fn fallback(inputs: &Inputs<'_>) -> Step {
    let Some(default) = inputs
        .profiles
        .iter()
        .find(|profile| profile.builtin == Some(Builtin::Default))
    else {
        return Step::Nothing;
    };
    match inputs.active {
        Some(active) if active.name == default.name => {
            if active.reason == ProfileReason::Fallback {
                Step::Nothing
            } else {
                Step::Relabel {
                    reason: ProfileReason::Fallback,
                }
            }
        }
        _ => Step::Activate {
            name: default.name.clone(),
            reason: ProfileReason::Fallback,
        },
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;
    use tt500_lcd_ipc::{ProfileSpec, ScheduleEntry, TimeOfDay, Triggers, Weekday};

    use super::*;

    // 2026-11-02 is a Monday.
    fn at(day: u32, h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, day)
            .and_then(|d| d.and_hms_opt(h, m, 0))
            .expect("valid test date")
    }

    fn entry(h: u8, m: u8, days: &[Weekday]) -> ScheduleEntry {
        ScheduleEntry {
            time: TimeOfDay::new(h, m).expect("valid time"),
            days: days.to_vec(),
        }
    }

    fn profile(name: &str, procs: &[&str], schedule: Vec<ScheduleEntry>) -> Profile {
        Profile {
            builtin: None,
            name: name.to_owned(),
            spec: ProfileSpec::default(),
            triggers: Triggers {
                processes: procs.iter().map(|p| (*p).to_owned()).collect(),
                ranges: Vec::new(),
                schedule,
            },
        }
    }

    fn default_profile() -> Profile {
        Profile {
            builtin: Some(Builtin::Default),
            ..profile("Default", &[], vec![])
        }
    }

    fn range(sh: u8, sm: u8, eh: u8, em: u8, days: &[Weekday]) -> RangeEntry {
        RangeEntry {
            start: TimeOfDay::new(sh, sm).expect("valid time"),
            end: TimeOfDay::new(eh, em).expect("valid time"),
            days: days.to_vec(),
        }
    }

    fn ranged(name: &str, ranges: Vec<RangeEntry>) -> Profile {
        let mut p = profile(name, &[], vec![]);
        p.triggers.ranges = ranges;
        p
    }

    fn range_reason(sh: u8, sm: u8, eh: u8, em: u8) -> ProfileReason {
        ProfileReason::Range {
            start: TimeOfDay::new(sh, sm).expect("valid time"),
            end: TimeOfDay::new(eh, em).expect("valid time"),
        }
    }

    fn range_name(profiles: &[Profile], now: NaiveDateTime) -> Option<&str> {
        range_winner(profiles, now).map(|(p, _)| p.name.as_str())
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    fn active(name: &str, reason: ProfileReason) -> Active {
        Active {
            name: name.to_owned(),
            reason,
        }
    }

    fn activate(name: &str, reason: ProfileReason) -> Step {
        Step::Activate {
            name: name.to_owned(),
            reason,
        }
    }

    fn proc_reason(p: &str) -> ProfileReason {
        ProfileReason::Process {
            process: p.to_owned(),
        }
    }

    fn run(
        cursor: &mut Cursor,
        profiles: &[Profile],
        now: NaiveDateTime,
        running: &BTreeSet<String>,
        active: Option<&Active>,
        last_manual: Option<&str>,
    ) -> Step {
        run_tick(cursor, profiles, now, running, active, last_manual).step
    }

    fn run_tick(
        cursor: &mut Cursor,
        profiles: &[Profile],
        now: NaiveDateTime,
        running: &BTreeSet<String>,
        active: Option<&Active>,
        last_manual: Option<&str>,
    ) -> Tick {
        tick(
            cursor,
            &Inputs {
                profiles,
                now,
                running,
                active,
                last_manual,
            },
        )
    }

    #[test]
    fn startup_picks_yesterdays_entry() {
        let profiles = [profile("night", &[], vec![entry(22, 0, &[])])];
        let mut c = Cursor::new();
        let step = run(&mut c, &profiles, at(3, 7, 0), &set(&[]), None, None);
        assert_eq!(step, activate("night", ProfileReason::Schedule));
    }

    #[test]
    fn startup_without_triggers_is_nothing() {
        let profiles = [profile("plain", &[], vec![])];
        let mut c = Cursor::new();
        assert_eq!(
            run(&mut c, &profiles, at(3, 7, 0), &set(&[]), None, None),
            Step::Nothing
        );
    }

    #[test]
    fn startup_relabels_persisted_marker_as_manual() {
        let profiles = [profile("plain", &[], vec![])];
        let a = active("plain", ProfileReason::Schedule);
        let mut c = Cursor::new();
        assert_eq!(
            run(&mut c, &profiles, at(3, 7, 0), &set(&[]), Some(&a), None),
            Step::Relabel {
                reason: ProfileReason::Manual
            }
        );
    }

    #[test]
    fn schedule_edge_fires_once() {
        let profiles = [
            profile("day", &[], vec![entry(7, 0, &[])]),
            profile("night", &[], vec![entry(22, 0, &[])]),
        ];
        let mut c = Cursor::new();
        let none = set(&[]);
        let a = active("day", ProfileReason::Schedule);
        assert_eq!(
            run(&mut c, &profiles, at(3, 21, 59), &none, Some(&a), None),
            Step::Nothing
        );
        let fired = run(&mut c, &profiles, at(3, 22, 0), &none, Some(&a), None);
        assert_eq!(fired, activate("night", ProfileReason::Schedule));
        let b = active("night", ProfileReason::Schedule);
        assert_eq!(
            run(&mut c, &profiles, at(3, 22, 1), &none, Some(&b), None),
            Step::Nothing
        );
    }

    #[test]
    fn equal_minute_entries_go_to_list_order() {
        let a = profile("a", &[], vec![entry(8, 0, &[])]);
        let b = profile("b", &[], vec![entry(8, 0, &[])]);
        let now = at(3, 9, 0);
        let fwd = [a.clone(), b.clone()];
        let won = latest_schedule(&fwd, now).map(|(_, p)| p.name.as_str());
        assert_eq!(won, Some("a"));
        let rev = [b, a];
        let won = latest_schedule(&rev, now).map(|(_, p)| p.name.as_str());
        assert_eq!(won, Some("b"));
    }

    #[test]
    fn weekday_filter_keeps_monday_entry_latest_on_tuesday() {
        let profiles = [profile("mon", &[], vec![entry(9, 0, &[Weekday::Mon])])];
        let got = latest_schedule(&profiles, at(3, 12, 0));
        assert_eq!(got.map(|(t, _)| t), Some(at(2, 9, 0)));
        let two = [
            profile("mon", &[], vec![entry(9, 0, &[Weekday::Mon])]),
            profile("tue", &[], vec![entry(10, 0, &[Weekday::Tue])]),
        ];
        let got = latest_schedule(&two, at(3, 12, 0));
        assert_eq!(got.map(|(_, p)| p.name.as_str()), Some("tue"));
        // A week later, Monday before 09:00 still finds last Monday.
        let got = latest_schedule(&profiles, at(9, 8, 0));
        assert_eq!(got.map(|(t, _)| t), Some(at(2, 9, 0)));
    }

    #[test]
    fn process_start_stop_and_handover() {
        let profiles = [
            profile("a", &["game-a"], vec![]),
            profile("b", &["game-b"], vec![]),
        ];
        let mut c = Cursor::new();
        let t = at(3, 12, 0);
        assert_eq!(
            run(&mut c, &profiles, t, &set(&[]), None, None),
            Step::Nothing
        );

        let a_up = set(&["game-a"]);
        assert_eq!(
            run(&mut c, &profiles, t, &a_up, None, None),
            activate("a", proc_reason("game-a"))
        );
        let act_a = active("a", proc_reason("game-a"));
        assert_eq!(
            run(&mut c, &profiles, t, &a_up, Some(&act_a), None),
            Step::Nothing
        );

        let b_up = set(&["game-b"]);
        assert_eq!(
            run(&mut c, &profiles, t, &b_up, Some(&act_a), None),
            activate("b", proc_reason("game-b"))
        );
        let act_b = active("b", proc_reason("game-b"));
        assert_eq!(
            run(&mut c, &profiles, t, &set(&[]), Some(&act_b), None),
            Step::Nothing
        );
    }

    #[test]
    fn process_beats_schedule_at_same_tick() {
        let profiles = [
            profile("night", &[], vec![entry(22, 0, &[])]),
            profile("game", &["game"], vec![]),
        ];
        let mut c = Cursor::new();
        run(&mut c, &profiles, at(3, 21, 0), &set(&[]), None, None);
        let step = run(&mut c, &profiles, at(3, 22, 0), &set(&["game"]), None, None);
        assert_eq!(step, activate("game", proc_reason("game")));
    }

    #[test]
    fn process_exit_falls_back_to_schedule_then_manual_then_nothing_without_default() {
        let t = at(3, 23, 0);
        let g = active("game", proc_reason("game"));
        let profiles = [
            profile("night", &[], vec![entry(22, 0, &[])]),
            profile("game", &["game"], vec![]),
            profile("manual", &[], vec![]),
        ];
        let mut c = Cursor::new();
        run(&mut c, &profiles, t, &set(&["game"]), None, None);
        assert_eq!(
            run(&mut c, &profiles, t, &set(&[]), Some(&g), Some("manual")),
            activate("night", ProfileReason::Schedule)
        );

        let profiles = [
            profile("game", &["game"], vec![]),
            profile("manual", &[], vec![]),
        ];
        let mut c = Cursor::new();
        run(&mut c, &profiles, t, &set(&["game"]), None, None);
        assert_eq!(
            run(&mut c, &profiles, t, &set(&[]), Some(&g), Some("manual")),
            activate("manual", ProfileReason::Manual)
        );
        let mut c = Cursor::new();
        run(&mut c, &profiles, t, &set(&["game"]), None, None);
        assert_eq!(
            run(&mut c, &profiles, t, &set(&[]), Some(&g), None),
            Step::Nothing
        );
    }

    #[test]
    fn manual_hold_survives_ticks_until_an_edge() {
        let profiles = [
            profile("night", &[], vec![entry(22, 0, &[])]),
            profile("manual", &[], vec![]),
        ];
        let mut c = Cursor::new();
        let none = set(&[]);
        run(&mut c, &profiles, at(3, 20, 0), &none, None, None);
        let m = active("manual", ProfileReason::Manual);
        for minute in [0, 30, 59] {
            let now = at(3, 21, minute);
            assert_eq!(
                run(&mut c, &profiles, now, &none, Some(&m), Some("manual")),
                Step::Nothing
            );
        }
        assert_eq!(
            run(
                &mut c,
                &profiles,
                at(3, 22, 0),
                &none,
                Some(&m),
                Some("manual")
            ),
            activate("night", ProfileReason::Schedule)
        );
    }

    #[test]
    fn manual_hold_survives_while_process_runs() {
        let profiles = [
            profile("game", &["game"], vec![]),
            profile("manual", &[], vec![]),
        ];
        let mut c = Cursor::new();
        let up = set(&["game"]);
        let g = active("game", proc_reason("game"));
        run(&mut c, &profiles, at(3, 12, 0), &up, Some(&g), None);
        let m = active("manual", ProfileReason::Manual);
        assert_eq!(
            run(
                &mut c,
                &profiles,
                at(3, 12, 1),
                &up,
                Some(&m),
                Some("manual")
            ),
            Step::Nothing
        );
    }

    #[test]
    fn long_gap_applies_only_latest_entry() {
        let profiles = [
            profile("a", &[], vec![entry(9, 0, &[])]),
            profile("b", &[], vec![entry(12, 0, &[])]),
            profile("c", &[], vec![entry(15, 0, &[])]),
        ];
        let mut c = Cursor::new();
        let none = set(&[]);
        let a = active("a", ProfileReason::Schedule);
        run(&mut c, &profiles, at(3, 9, 30), &none, Some(&a), None);
        assert_eq!(
            run(&mut c, &profiles, at(3, 17, 30), &none, Some(&a), None),
            activate("c", ProfileReason::Schedule)
        );
    }

    #[test]
    fn small_backward_jump_does_not_refire_and_big_one_resyncs() {
        let profiles = [profile("night", &[], vec![entry(2, 30, &[])])];
        let mut c = Cursor::new();
        let none = set(&[]);
        run(&mut c, &profiles, at(3, 2, 29), &none, None, None);
        assert_eq!(
            run(&mut c, &profiles, at(3, 2, 30), &none, None, None),
            activate("night", ProfileReason::Schedule)
        );
        let n = active("night", ProfileReason::Schedule);
        // Fold: the clock goes back one hour, then passes 02:30 again.
        assert_eq!(
            run(&mut c, &profiles, at(3, 1, 30), &none, Some(&n), None),
            Step::Nothing
        );
        assert_eq!(
            run(&mut c, &profiles, at(3, 2, 30), &none, Some(&n), None),
            Step::Nothing
        );
        // Five hours back: full resolve like a start-up.
        assert_eq!(
            run(&mut c, &profiles, at(2, 22, 0), &none, None, None),
            activate("night", ProfileReason::Schedule)
        );
    }

    #[test]
    fn spring_forward_fires_skipped_entry() {
        let profiles = [profile("night", &[], vec![entry(2, 30, &[])])];
        let mut c = Cursor::new();
        let none = set(&[]);
        run(&mut c, &profiles, at(3, 1, 59), &none, None, None);
        assert_eq!(
            run(&mut c, &profiles, at(3, 3, 0), &none, None, None),
            activate("night", ProfileReason::Schedule)
        );
    }

    #[test]
    fn edit_adding_past_entry_does_not_fire() {
        let mut profiles = vec![profile("p", &[], vec![entry(8, 0, &[])])];
        let mut c = Cursor::new();
        let none = set(&[]);
        let p = active("p", ProfileReason::Schedule);
        run(&mut c, &profiles, at(3, 10, 0), &none, Some(&p), None);
        profiles.push(profile("q", &[], vec![entry(9, 0, &[])]));
        c.mark_profiles_changed();
        assert_eq!(
            run(&mut c, &profiles, at(3, 10, 1), &none, Some(&p), None),
            Step::Nothing
        );
        assert_eq!(
            run(&mut c, &profiles, at(3, 10, 2), &none, Some(&p), None),
            Step::Nothing
        );
    }

    #[test]
    fn truncated_comm_matches() {
        let profiles = [profile("long", &["a-very-long-process-name"], vec![])];
        let found = process_winner(&profiles, &set(&["a-very-long-pro"]));
        assert_eq!(
            found.map(|(p, n)| (p.name.as_str(), n)),
            Some(("long", "a-very-long-process-name"))
        );
    }

    #[test]
    fn same_profile_other_reason_relabels() {
        let profiles = [profile("p", &["game"], vec![entry(8, 0, &[])])];
        let mut c = Cursor::new();
        let none = set(&[]);
        let up = set(&["game"]);
        let sched = active("p", ProfileReason::Schedule);
        run(&mut c, &profiles, at(3, 9, 0), &none, Some(&sched), None);
        assert_eq!(
            run(&mut c, &profiles, at(3, 9, 1), &up, Some(&sched), None),
            Step::Relabel {
                reason: proc_reason("game")
            }
        );
        let by_proc = active("p", proc_reason("game"));
        assert_eq!(
            run(&mut c, &profiles, at(3, 9, 2), &none, Some(&by_proc), None),
            Step::Relabel {
                reason: ProfileReason::Schedule
            }
        );
    }

    #[test]
    fn order_is_the_tie_break() {
        let a = profile("a", &["game"], vec![entry(8, 0, &[])]);
        let b = profile("b", &["game"], vec![entry(8, 0, &[])]);
        let up = set(&["game"]);
        let now = at(3, 9, 0);

        let fwd = [a.clone(), b.clone()];
        let rev = [b, a];
        let winner = |s: &[Profile]| process_winner(s, &up).map(|(p, _)| p.name.clone());
        assert_eq!(winner(&fwd).as_deref(), Some("a"));
        assert_eq!(winner(&rev).as_deref(), Some("b"));
        let sched = |s: &[Profile]| latest_schedule(s, now).map(|(_, p)| p.name.clone());
        assert_eq!(sched(&fwd).as_deref(), Some("a"));
        assert_eq!(sched(&rev).as_deref(), Some("b"));

        // Reordering while the old winner still runs is a process edge to the new first match.
        let mut c = Cursor::new();
        let act = active("a", proc_reason("game"));
        run(&mut c, &fwd, now, &up, Some(&act), None);
        c.mark_profiles_changed();
        assert_eq!(
            run(&mut c, &rev, now, &up, Some(&act), None),
            activate("b", proc_reason("game"))
        );
    }

    #[test]
    fn wrapped_range_membership() {
        let p = [ranged("night", vec![range(22, 0, 6, 0, &[])])];
        for (day, h, m, want) in [
            (3, 23, 0, true),
            (4, 5, 59, true),
            (4, 6, 0, false),
            (3, 21, 59, false),
            (3, 12, 0, false),
            (3, 22, 0, true),
        ] {
            assert_eq!(
                range_name(&p, at(day, h, m)).is_some(),
                want,
                "{day} {h}:{m}"
            );
        }
    }

    #[test]
    fn same_day_range_membership() {
        let p = [ranged("work", vec![range(9, 0, 17, 0, &[])])];
        assert!(range_winner(&p, at(3, 9, 0)).is_some());
        assert!(range_winner(&p, at(3, 16, 59)).is_some());
        assert!(range_winner(&p, at(3, 17, 0)).is_none());
        assert!(range_winner(&p, at(3, 8, 59)).is_none());
    }

    #[test]
    fn weekday_names_the_start_day() {
        // 2026-11-06 is a Friday, the 7th a Saturday.
        let p = [ranged("fri", vec![range(22, 0, 6, 0, &[Weekday::Fri])])];
        assert!(range_winner(&p, at(6, 23, 0)).is_some());
        assert!(range_winner(&p, at(7, 3, 0)).is_some());
        assert!(range_winner(&p, at(7, 23, 0)).is_none());
        assert!(range_winner(&p, at(6, 3, 0)).is_none());
        assert!(range_winner(&p, at(8, 3, 0)).is_none());
    }

    #[test]
    fn startup_inside_range_applies_it() {
        let p = [
            default_profile(),
            ranged("night", vec![range(22, 0, 6, 0, &[])]),
        ];
        let mut c = Cursor::new();
        let step = run(&mut c, &p, at(3, 23, 0), &set(&[]), None, None);
        assert_eq!(step, activate("night", range_reason(22, 0, 6, 0)));
    }

    #[test]
    fn range_start_and_end_are_edges() {
        let p = [
            default_profile(),
            ranged("night", vec![range(22, 0, 6, 0, &[])]),
        ];
        let mut c = Cursor::new();
        let none = set(&[]);
        let d = active("Default", ProfileReason::Fallback);
        run(&mut c, &p, at(3, 21, 59), &none, Some(&d), None);
        let step = run(&mut c, &p, at(3, 22, 0), &none, Some(&d), None);
        assert_eq!(step, activate("night", range_reason(22, 0, 6, 0)));
        let n = active("night", range_reason(22, 0, 6, 0));
        assert_eq!(
            run(&mut c, &p, at(4, 3, 0), &none, Some(&n), None),
            Step::Nothing
        );
        assert_eq!(
            run(&mut c, &p, at(4, 6, 0), &none, Some(&n), None),
            activate("Default", ProfileReason::Fallback)
        );
        assert_eq!(
            run(&mut c, &p, at(4, 6, 1), &none, Some(&d), None),
            Step::Nothing
        );
    }

    #[test]
    fn resume_gap_applies_enter_and_leave_on_first_tick() {
        let p = [
            default_profile(),
            ranged("night", vec![range(22, 0, 6, 0, &[])]),
        ];
        let none = set(&[]);
        // Suspended at 20:00, resumed 8 h later inside the range.
        let mut c = Cursor::new();
        let d = active("Default", ProfileReason::Fallback);
        run(&mut c, &p, at(3, 20, 0), &none, Some(&d), None);
        assert_eq!(
            run(&mut c, &p, at(4, 4, 0), &none, Some(&d), None),
            activate("night", range_reason(22, 0, 6, 0))
        );
        // Suspended at 23:00, resumed 8 h later after the range ended.
        let mut c = Cursor::new();
        let n = active("night", range_reason(22, 0, 6, 0));
        run(&mut c, &p, at(3, 23, 0), &none, Some(&n), None);
        assert_eq!(
            run(&mut c, &p, at(4, 7, 0), &none, Some(&n), None),
            activate("Default", ProfileReason::Fallback)
        );
    }

    #[test]
    fn backward_jumps_keep_membership_consistent() {
        let p = [
            default_profile(),
            ranged("night", vec![range(22, 0, 6, 0, &[])]),
        ];
        let none = set(&[]);
        let n = active("night", range_reason(22, 0, 6, 0));
        // 1 h back (still inside): nothing happens.
        let mut c = Cursor::new();
        run(&mut c, &p, at(3, 23, 30), &none, Some(&n), None);
        assert_eq!(
            run(&mut c, &p, at(3, 22, 30), &none, Some(&n), None),
            Step::Nothing
        );
        // 5 h back (now outside): full resolve falls back to Default.
        let mut c = Cursor::new();
        run(&mut c, &p, at(4, 2, 0), &none, Some(&n), None);
        assert_eq!(
            run(&mut c, &p, at(3, 21, 0), &none, Some(&n), None),
            activate("Default", ProfileReason::Fallback)
        );
        // 5 h back into the range: applies it.
        let d = active("Default", ProfileReason::Fallback);
        let mut c = Cursor::new();
        run(&mut c, &p, at(4, 8, 0), &none, Some(&d), None);
        assert_eq!(
            run(&mut c, &p, at(4, 3, 0), &none, Some(&d), None),
            activate("night", range_reason(22, 0, 6, 0))
        );
    }

    #[test]
    fn process_beats_range() {
        let p = [
            ranged("night", vec![range(22, 0, 6, 0, &[])]),
            profile("game", &["game"], vec![]),
        ];
        let mut c = Cursor::new();
        let none = set(&[]);
        run(&mut c, &p, at(3, 21, 0), &none, None, None);
        run(&mut c, &p, at(3, 22, 0), &none, None, None);
        let step = run(&mut c, &p, at(3, 22, 1), &set(&["game"]), None, None);
        assert_eq!(step, activate("game", proc_reason("game")));
        let g = active("game", proc_reason("game"));
        assert_eq!(
            run(&mut c, &p, at(3, 22, 2), &set(&["game"]), Some(&g), None),
            Step::Nothing
        );
        // Game exits while the range is on: the range takes over.
        assert_eq!(
            run(&mut c, &p, at(3, 22, 3), &none, Some(&g), None),
            activate("night", range_reason(22, 0, 6, 0))
        );
    }

    #[test]
    fn process_exit_inside_range_returns_to_range() {
        let p = [
            default_profile(),
            ranged("night", vec![range(22, 0, 6, 0, &[])]),
            profile("game", &["game"], vec![]),
        ];
        let mut c = Cursor::new();
        let up = set(&["game"]);
        let g = active("game", proc_reason("game"));
        run(&mut c, &p, at(3, 23, 0), &up, Some(&g), None);
        assert_eq!(
            run(&mut c, &p, at(3, 23, 1), &set(&[]), Some(&g), None),
            activate("night", range_reason(22, 0, 6, 0))
        );
    }

    #[test]
    fn range_beats_schedule_firing_inside_it() {
        let p = [
            default_profile(),
            ranged("night", vec![range(22, 0, 6, 0, &[])]),
            profile("late", &[], vec![entry(23, 0, &[])]),
        ];
        let mut c = Cursor::new();
        let none = set(&[]);
        let n = active("night", range_reason(22, 0, 6, 0));
        run(&mut c, &p, at(3, 22, 30), &none, Some(&n), None);
        assert_eq!(
            run(&mut c, &p, at(3, 23, 0), &none, Some(&n), None),
            Step::Nothing
        );
    }

    #[test]
    fn range_end_falls_to_schedule_then_manual_then_default() {
        let night = ranged("night", vec![range(22, 0, 6, 0, &[])]);
        let n = active("night", range_reason(22, 0, 6, 0));
        let none = set(&[]);
        let sched = profile("morning", &[], vec![entry(5, 0, &[])]);
        let manual = profile("manual", &[], vec![]);

        let p = [default_profile(), night.clone(), sched, manual.clone()];
        let mut c = Cursor::new();
        run(&mut c, &p, at(4, 5, 30), &none, Some(&n), None);
        assert_eq!(
            run(&mut c, &p, at(4, 6, 0), &none, Some(&n), Some("manual")),
            activate("morning", ProfileReason::Schedule)
        );

        let p = [default_profile(), night.clone(), manual];
        let mut c = Cursor::new();
        run(&mut c, &p, at(4, 5, 30), &none, Some(&n), None);
        assert_eq!(
            run(&mut c, &p, at(4, 6, 0), &none, Some(&n), Some("manual")),
            activate("manual", ProfileReason::Manual)
        );

        let p = [default_profile(), night];
        let mut c = Cursor::new();
        run(&mut c, &p, at(4, 5, 30), &none, Some(&n), None);
        assert_eq!(
            run(&mut c, &p, at(4, 6, 0), &none, Some(&n), None),
            activate("Default", ProfileReason::Fallback)
        );
    }

    #[test]
    fn fallback_relabels_when_default_is_already_on() {
        let p = [default_profile(), profile("game", &["game"], vec![])];
        let mut c = Cursor::new();
        let g = active("game", proc_reason("game"));
        run(&mut c, &p, at(3, 12, 0), &set(&["game"]), Some(&g), None);
        let manual_default = active("Default", ProfileReason::Manual);
        assert_eq!(
            run(
                &mut c,
                &p,
                at(3, 12, 1),
                &set(&[]),
                Some(&manual_default),
                None
            ),
            Step::Relabel {
                reason: ProfileReason::Fallback
            }
        );
    }

    #[test]
    fn overlapping_ranges_list_order_wins() {
        let a = ranged("a", vec![range(20, 0, 23, 0, &[])]);
        let b = ranged("b", vec![range(21, 0, 22, 0, &[])]);
        let now = at(3, 21, 30);
        let fwd = [a.clone(), b.clone()];
        assert_eq!(range_name(&fwd, now), Some("a"));
        let rev = [b, a];
        assert_eq!(range_name(&rev, now), Some("b"));
    }

    #[test]
    fn manual_hold_survives_ticks_inside_unchanged_range() {
        let p = [
            default_profile(),
            ranged("night", vec![range(22, 0, 6, 0, &[])]),
            profile("manual", &[], vec![]),
        ];
        let mut c = Cursor::new();
        let none = set(&[]);
        let n = active("night", range_reason(22, 0, 6, 0));
        run(&mut c, &p, at(3, 23, 0), &none, Some(&n), None);
        let m = active("manual", ProfileReason::Manual);
        for minute in [1, 30, 59] {
            assert_eq!(
                run(
                    &mut c,
                    &p,
                    at(3, 23, minute),
                    &none,
                    Some(&m),
                    Some("manual")
                ),
                Step::Nothing
            );
        }
    }

    fn gpu_profiles() -> [Profile; 2] {
        [
            default_profile(),
            ranged("gpu", vec![range(5, 0, 18, 0, &[])]),
        ]
    }

    #[test]
    fn manual_in_range_then_range_end_goes_to_default() {
        let p = gpu_profiles();
        let none = set(&[]);
        let mut c = Cursor::new();
        let g = active("gpu", range_reason(5, 0, 18, 0));
        run(&mut c, &p, at(3, 12, 0), &none, Some(&g), Some("gpu"));
        let m = active("gpu", ProfileReason::Manual);
        let t = run_tick(&mut c, &p, at(3, 18, 0), &none, Some(&m), Some("gpu"));
        assert!(t.forget_manual);
        assert_eq!(t.step, activate("Default", ProfileReason::Fallback));
    }

    #[test]
    fn manual_outside_range_holds_until_range_start_then_default_at_end() {
        let p = gpu_profiles();
        let none = set(&[]);
        let mut c = Cursor::new();
        let m = active("gpu", ProfileReason::Manual);
        run(&mut c, &p, at(3, 20, 0), &none, Some(&m), None);
        let t = run_tick(&mut c, &p, at(3, 22, 0), &none, Some(&m), Some("gpu"));
        assert_eq!(t.step, Step::Nothing);
        assert!(!t.forget_manual);
        let t = run_tick(&mut c, &p, at(4, 5, 0), &none, Some(&m), Some("gpu"));
        assert_eq!(
            t.step,
            Step::Relabel {
                reason: range_reason(5, 0, 18, 0)
            }
        );
        assert!(!t.forget_manual);
        let r = active("gpu", range_reason(5, 0, 18, 0));
        let t = run_tick(&mut c, &p, at(4, 18, 0), &none, Some(&r), Some("gpu"));
        assert!(t.forget_manual);
        assert_eq!(t.step, activate("Default", ProfileReason::Fallback));
    }

    #[test]
    fn process_exit_returns_to_manual_non_ranged_profile() {
        let p = [
            default_profile(),
            profile("game", &["game"], vec![]),
            profile("manual", &[], vec![]),
        ];
        let mut c = Cursor::new();
        let m = active("manual", ProfileReason::Manual);
        run(
            &mut c,
            &p,
            at(3, 12, 0),
            &set(&[]),
            Some(&m),
            Some("manual"),
        );
        let g = active("game", proc_reason("game"));
        run(
            &mut c,
            &p,
            at(3, 12, 1),
            &set(&["game"]),
            Some(&m),
            Some("manual"),
        );
        let t = run_tick(
            &mut c,
            &p,
            at(3, 12, 2),
            &set(&[]),
            Some(&g),
            Some("manual"),
        );
        assert!(!t.forget_manual);
        assert_eq!(t.step, activate("manual", ProfileReason::Manual));
    }

    #[test]
    fn resume_after_range_ended_while_suspended_goes_to_default() {
        let p = gpu_profiles();
        let none = set(&[]);
        let mut c = Cursor::new();
        let m = active("gpu", ProfileReason::Manual);
        run(&mut c, &p, at(3, 17, 0), &none, Some(&m), Some("gpu"));
        let t = run_tick(&mut c, &p, at(3, 20, 33), &none, Some(&m), Some("gpu"));
        assert!(t.forget_manual);
        assert_eq!(t.step, activate("Default", ProfileReason::Fallback));
    }

    #[test]
    fn start_outside_range_does_not_hold_manual_ranged_profile() {
        let p = gpu_profiles();
        let mut c = Cursor::new();
        let m = active("gpu", ProfileReason::Manual);
        let t = run_tick(&mut c, &p, at(3, 20, 33), &set(&[]), Some(&m), Some("gpu"));
        assert!(t.forget_manual);
        assert_eq!(t.step, activate("Default", ProfileReason::Fallback));
    }
}
