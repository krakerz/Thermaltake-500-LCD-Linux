//! Pure helpers for the Profiles page: summaries, ordering and trigger edits, without GTK.

use std::collections::BTreeSet;

use tt500_lcd_ipc::{
    LiveSource, PROFILE_MAX_PROCESS_CHARS, ProfileReason, ProfileSource, ProfileSpec, RangeEntry,
    ScheduleEntry, Triggers, Weekday,
};

/// Shortest run of consecutive days written as a range ("Mon-Wed").
const MIN_RANGE_DAYS: usize = 3;

/// The order after moving the profile at `from` in front of slot `to_slot`.
///
/// Slot `n` is the gap before the profile at index `n`; `to_slot` runs over `0..=names.len()`
/// (the last value is the gap after the final profile). Dropping a row onto its own edge, or an
/// out-of-range `from`, leaves the order unchanged; `to_slot` beyond the end is clamped.
#[must_use]
pub fn reordered(names: &[String], from: usize, to_slot: usize) -> Vec<String> {
    let mut order = names.to_vec();
    if from >= order.len() {
        return order;
    }
    let name = order.remove(from);
    let slot = if to_slot > from { to_slot - 1 } else { to_slot };
    order.insert(slot.min(order.len()), name);
    order
}

/// Three-letter day name for buttons and summaries.
#[must_use]
pub fn day_label(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

/// "Every day" for none or all days, else ranges and lists such as "Mon-Fri" or "Sat, Sun".
#[must_use]
pub fn days_summary(days: &[Weekday]) -> String {
    let mut sorted: Vec<Weekday> = days.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.is_empty() || sorted.len() == Weekday::ALL.len() {
        return "Every day".to_owned();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut start = 0;
    while start < sorted.len() {
        let mut end = start;
        while end + 1 < sorted.len() && sorted[end + 1].index() == sorted[end].index() + 1 {
            end += 1;
        }
        if end + 1 - start >= MIN_RANGE_DAYS {
            parts.push(format!(
                "{}-{}",
                day_label(sorted[start]),
                day_label(sorted[end])
            ));
        } else {
            parts.extend(
                sorted[start..=end]
                    .iter()
                    .map(|day| day_label(*day).to_owned()),
            );
        }
        start = end + 1;
    }
    parts.join(", ")
}

/// Short caption for a day selection: "Every day", "Weekdays", "Weekend", else [`days_summary`].
#[must_use]
pub fn days_caption(days: &[Weekday]) -> String {
    let mut sorted: Vec<Weekday> = days.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted
        == [
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
        ]
    {
        "Weekdays".to_owned()
    } else if sorted == [Weekday::Sat, Weekday::Sun] {
        "Weekend".to_owned()
    } else {
        days_summary(&sorted)
    }
}

/// A number as two digits ("00", "07", "23"), for hour and minute fields; negatives show as "00".
#[must_use]
pub fn two_digits(value: i32) -> String {
    format!("{:02}", value.max(0))
}

/// Whether a range ends at or before its start, so it runs into the next day.
#[must_use]
pub fn runs_into_next_day(entry: &RangeEntry) -> bool {
    entry.end <= entry.start
}

/// `triggers` without the one that `reason` names, so a subtitle does not repeat it.
#[must_use]
pub fn without_reason(triggers: &Triggers, reason: &ProfileReason) -> Triggers {
    let mut rest = triggers.clone();
    match reason {
        ProfileReason::Range { start, end } => rest
            .ranges
            .retain(|range| range.start != *start || range.end != *end),
        ProfileReason::Process { process } => rest.processes.retain(|name| name != process),
        _ => {}
    }
    rest
}

/// Subtitle of a profile row. The active reason is stated once; only the other triggers follow.
#[must_use]
pub fn row_subtitle(triggers: &Triggers, active: Option<(&ProfileReason, bool)>) -> String {
    let Some((reason, modified)) = active else {
        return describe_triggers(triggers);
    };
    let changed = if modified { " (changed)" } else { "" };
    let head = format!("Active ({}){changed}", reason_text(reason));
    let rest = without_reason(triggers, reason);
    if rest == Triggers::default() {
        head
    } else {
        format!("{head} · {}", describe_triggers(&rest))
    }
}

/// Adds `day` to `days`, or removes it when present; the result stays in week order.
pub fn toggle_day(days: &mut Vec<Weekday>, day: Weekday) {
    if let Some(position) = days.iter().position(|d| *d == day) {
        days.remove(position);
    } else {
        days.push(day);
        days.sort_unstable();
    }
}

/// One line for a profile's triggers, e.g. "22:00 Mon-Fri, 07:30; 22:00–06:00 Fri; while game.exe runs".
#[must_use]
pub fn describe_triggers(triggers: &Triggers) -> String {
    let times: Vec<String> = triggers.schedule.iter().map(describe_entry).collect();
    let mut parts = Vec::new();
    if !times.is_empty() {
        parts.push(times.join(", "));
    }
    let ranges: Vec<String> = triggers.ranges.iter().map(range_summary).collect();
    if !ranges.is_empty() {
        parts.push(ranges.join(", "));
    }
    if !triggers.processes.is_empty() {
        parts.push(format!("while {} runs", triggers.processes.join(", ")));
    }
    if parts.is_empty() {
        "No triggers".to_owned()
    } else {
        parts.join("; ")
    }
}

/// A range as "22:00–06:00", with the days it starts on when not every day: "22:00–06:00 Fri".
#[must_use]
pub fn range_summary(entry: &RangeEntry) -> String {
    if entry.days.is_empty() || entry.days.len() == Weekday::ALL.len() {
        format!("{}–{}", entry.start, entry.end)
    } else {
        format!(
            "{}–{} {}",
            entry.start,
            entry.end,
            days_summary(&entry.days)
        )
    }
}

fn describe_entry(entry: &ScheduleEntry) -> String {
    if entry.days.is_empty() || entry.days.len() == Weekday::ALL.len() {
        entry.time.to_string()
    } else {
        format!("{} {}", entry.time, days_summary(&entry.days))
    }
}

/// What a profile shows and sets, e.g. "photo.png · Brightness 20% · Rotation 90°".
#[must_use]
pub fn source_summary(spec: &ProfileSpec) -> String {
    let mut parts = vec![match &spec.source {
        None => "Keeps the current content".to_owned(),
        Some(ProfileSource::Static { path }) => path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        ),
        Some(ProfileSource::Clear) => "Clear".to_owned(),
        Some(ProfileSource::Live { source }) => match source {
            LiveSource::Desktop(_) => "Virtual desktop".to_owned(),
            LiveSource::Screen {
                output: Some(output),
            } => format!("Screen {output}"),
            LiveSource::Screen { output: None } => "Screen".to_owned(),
            LiveSource::Portal { .. } => "Screen portal".to_owned(),
            _ => "Live source".to_owned(),
        },
    }];
    if let Some(brightness) = spec.brightness {
        parts.push(format!("Brightness {brightness}%"));
    }
    if let Some(rotation) = spec.rotation {
        parts.push(format!("Rotation {}°", u32::from(rotation) * 90));
    }
    parts.join(" · ")
}

/// Why a profile is active, in words for a subtitle or tooltip.
#[must_use]
pub fn reason_text(reason: &ProfileReason) -> String {
    match reason {
        ProfileReason::Manual => "set manually".to_owned(),
        ProfileReason::Schedule => "by schedule".to_owned(),
        ProfileReason::Process { process } => format!("while {process} runs"),
        ProfileReason::Range { start, end } => format!("time range {start}–{end}"),
        ProfileReason::Fallback => "fallback".to_owned(),
        ProfileReason::Unknown => "unknown".to_owned(),
    }
}

/// Running process names the user may add: valid as a trigger and not yet in `existing`.
#[must_use]
pub fn selectable_processes(running: &BTreeSet<String>, existing: &[String]) -> Vec<String> {
    running
        .iter()
        .filter(|name| {
            !name.is_empty()
                && name.chars().count() <= PROFILE_MAX_PROCESS_CHARS
                && !name.contains('/')
                && !name.chars().any(char::is_whitespace)
                && !existing.contains(name)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt500_lcd_ipc::TimeOfDay;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    fn entry(hour: u8, minute: u8, days: &[Weekday]) -> ScheduleEntry {
        ScheduleEntry {
            time: TimeOfDay::new(hour, minute).expect("valid time"),
            days: days.to_vec(),
        }
    }

    #[test]
    fn reordered_inserts_before_the_slot() {
        let order = names(&["a", "b", "c", "d"]);
        let cases: &[(usize, usize, &[&str])] = &[
            (0, 2, &["b", "a", "c", "d"]),
            (0, 4, &["b", "c", "d", "a"]),
            (3, 0, &["d", "a", "b", "c"]),
            (2, 1, &["a", "c", "b", "d"]),
            (3, 4, &["a", "b", "c", "d"]),
            (1, 1, &["a", "b", "c", "d"]),
            (1, 2, &["a", "b", "c", "d"]),
            (9, 0, &["a", "b", "c", "d"]),
            (0, 99, &["b", "c", "d", "a"]),
        ];
        for (from, slot, want) in cases {
            assert_eq!(
                reordered(&order, *from, *slot),
                names(want),
                "{from} -> {slot}"
            );
        }
        assert!(reordered(&[], 0, 0).is_empty());
    }

    #[test]
    fn days_summary_forms() {
        use Weekday::*;
        assert_eq!(days_summary(&[]), "Every day");
        assert_eq!(days_summary(&Weekday::ALL), "Every day");
        assert_eq!(days_summary(&[Mon, Tue, Wed, Thu, Fri]), "Mon-Fri");
        assert_eq!(days_summary(&[Sun, Sat]), "Sat, Sun");
        assert_eq!(days_summary(&[Mon, Wed, Fri]), "Mon, Wed, Fri");
        assert_eq!(
            days_summary(&[Mon, Tue, Thu, Fri, Sat]),
            "Mon, Tue, Thu-Sat"
        );
        assert_eq!(days_summary(&[Tue]), "Tue");
    }

    #[test]
    fn toggle_day_keeps_week_order() {
        let mut days = vec![Weekday::Fri];
        toggle_day(&mut days, Weekday::Mon);
        assert_eq!(days, [Weekday::Mon, Weekday::Fri]);
        toggle_day(&mut days, Weekday::Fri);
        assert_eq!(days, [Weekday::Mon]);
        toggle_day(&mut days, Weekday::Mon);
        assert!(days.is_empty());
    }

    #[test]
    fn describe_triggers_lists_times_then_processes() {
        assert_eq!(describe_triggers(&Triggers::default()), "No triggers");
        let triggers = Triggers {
            processes: vec!["game.exe".into(), "steam".into()],
            ranges: Vec::new(),
            schedule: vec![
                entry(
                    22,
                    0,
                    &[
                        Weekday::Mon,
                        Weekday::Tue,
                        Weekday::Wed,
                        Weekday::Thu,
                        Weekday::Fri,
                    ],
                ),
                entry(7, 30, &[]),
            ],
        };
        assert_eq!(
            describe_triggers(&triggers),
            "22:00 Mon-Fri, 07:30; while game.exe, steam runs"
        );
        let only_process = Triggers {
            processes: vec!["a".into()],
            ranges: Vec::new(),
            schedule: Vec::new(),
        };
        assert_eq!(describe_triggers(&only_process), "while a runs");
    }

    #[test]
    fn range_summary_and_description() {
        let night = RangeEntry {
            start: TimeOfDay::new(22, 0).unwrap(),
            end: TimeOfDay::new(6, 0).unwrap(),
            days: Vec::new(),
        };
        let friday = RangeEntry {
            days: vec![Weekday::Fri],
            ..night.clone()
        };
        assert_eq!(range_summary(&night), "22:00–06:00");
        assert_eq!(range_summary(&friday), "22:00–06:00 Fri");
        let triggers = Triggers {
            processes: vec!["a".into()],
            ranges: vec![friday],
            schedule: vec![entry(7, 30, &[])],
        };
        assert_eq!(
            describe_triggers(&triggers),
            "07:30; 22:00–06:00 Fri; while a runs"
        );
    }

    #[test]
    fn source_summary_names_file_and_settings() {
        let spec = ProfileSpec {
            brightness: Some(20),
            rotation: Some(1),
            source: Some(ProfileSource::Static {
                path: "/tmp/pics/night.png".into(),
            }),
            ..ProfileSpec::default()
        };
        assert_eq!(
            source_summary(&spec),
            "night.png · Brightness 20% · Rotation 90°"
        );
        assert_eq!(
            source_summary(&ProfileSpec::default()),
            "Keeps the current content"
        );
        let live = ProfileSpec {
            source: Some(ProfileSource::Live {
                source: LiveSource::Screen { output: None },
            }),
            ..ProfileSpec::default()
        };
        assert_eq!(source_summary(&live), "Screen");
    }

    #[test]
    fn two_digit_fields() {
        assert_eq!(two_digits(0), "00");
        assert_eq!(two_digits(7), "07");
        assert_eq!(two_digits(23), "23");
        assert_eq!(two_digits(-1), "00");
    }

    #[test]
    fn days_caption_names_common_sets() {
        use Weekday::*;
        assert_eq!(days_caption(&[]), "Every day");
        assert_eq!(days_caption(&[Fri, Mon, Tue, Wed, Thu]), "Weekdays");
        assert_eq!(days_caption(&[Sun, Sat]), "Weekend");
        assert_eq!(days_caption(&[Fri]), "Fri");
    }

    #[test]
    fn row_subtitle_does_not_repeat_the_active_reason() {
        let night = RangeEntry {
            start: TimeOfDay::new(22, 0).unwrap(),
            end: TimeOfDay::new(6, 0).unwrap(),
            days: Vec::new(),
        };
        let triggers = Triggers {
            processes: vec!["a".into()],
            ranges: vec![night.clone()],
            schedule: Vec::new(),
        };
        let reason = ProfileReason::Range {
            start: night.start,
            end: night.end,
        };
        assert_eq!(
            row_subtitle(&triggers, Some((&reason, false))),
            "Active (time range 22:00–06:00) · while a runs"
        );
        let only = Triggers {
            ranges: vec![night],
            ..Triggers::default()
        };
        assert_eq!(
            row_subtitle(&only, Some((&reason, true))),
            "Active (time range 22:00–06:00) (changed)"
        );
        assert_eq!(row_subtitle(&only, None), "22:00–06:00");
    }

    #[test]
    fn reason_text_reads_naturally() {
        assert_eq!(reason_text(&ProfileReason::Schedule), "by schedule");
        assert_eq!(
            reason_text(&ProfileReason::Process {
                process: "x".into()
            }),
            "while x runs"
        );
    }

    #[test]
    fn selectable_processes_hides_invalid_and_added_names() {
        let running: BTreeSet<String> = ["bash", "Web Content", "a/b", "game", ""]
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        assert_eq!(
            selectable_processes(&running, &["game".to_owned()]),
            vec!["bash".to_owned()]
        );
    }
}
