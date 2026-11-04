//! Editor for one profile's triggers: times of day, time ranges and "while a process runs".

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use tt500_lcd_ipc::{
    ProfileTriggersParams, RangeEntry, Request, ScheduleEntry, TimeOfDay, Triggers, Weekday,
    running_process_names,
};

use super::context::{Context, weak_call};
use crate::profiles::{
    day_label, days_caption, runs_into_next_day, selectable_processes, toggle_day, two_digits,
};

/// How long spin-button edits wait before they are sent, so typing "22" sends once.
const SPIN_DEBOUNCE: Duration = Duration::from_millis(500);
/// Time a new schedule entry starts with.
const NEW_TIME: (u8, u8) = (8, 0);
/// Start and end a new range begins with.
const NEW_RANGE: ((u8, u8), (u8, u8)) = ((22, 0), (6, 0));

/// Which end of a range a spin-button pair edits.
#[derive(Clone, Copy)]
enum RangeEnd {
    Start,
    End,
}

struct Inner {
    ctx: Rc<Context>,
    name: String,
    /// What the editor shows; the last value sent (or the initial one).
    triggers: RefCell<Triggers>,
    times_group: adw::PreferencesGroup,
    ranges_group: adw::PreferencesGroup,
    processes_group: adw::PreferencesGroup,
    /// Rows currently in the groups, so a rebuild can remove them.
    time_rows: RefCell<Vec<gtk::Widget>>,
    range_rows: RefCell<Vec<gtk::Widget>>,
    process_rows: RefCell<Vec<gtk::Widget>>,
    pending: RefCell<Option<glib::SourceId>>,
}

/// The trigger editor of one profile. Cloning shares it.
#[derive(Clone)]
pub(super) struct TriggerEditor {
    inner: Rc<Inner>,
    root: gtk::Box,
}

impl TriggerEditor {
    pub fn new(ctx: &Rc<Context>, name: &str, triggers: &Triggers) -> Self {
        let times_group = adw::PreferencesGroup::builder().title("Times").build();
        let ranges_group = adw::PreferencesGroup::builder()
            .title("Time ranges")
            .description("Active from the start time until the end time.")
            .build();
        let processes_group = adw::PreferencesGroup::builder()
            .title("While running")
            .build();
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        root.append(&times_group);
        root.append(&ranges_group);
        root.append(&processes_group);
        let inner = Rc::new(Inner {
            ctx: Rc::clone(ctx),
            name: name.to_owned(),
            triggers: RefCell::new(triggers.clone()),
            times_group,
            ranges_group,
            processes_group,
            time_rows: RefCell::new(Vec::new()),
            range_rows: RefCell::new(Vec::new()),
            process_rows: RefCell::new(Vec::new()),
            pending: RefCell::new(None),
        });
        inner.rebuild();
        Self { inner, root }
    }

    pub fn widget(&self) -> &gtk::Widget {
        self.root.upcast_ref()
    }

    /// The triggers the editor currently holds.
    pub fn triggers(&self) -> Triggers {
        self.inner.triggers.borrow().clone()
    }
}

impl Inner {
    /// Redraws every row from `self.triggers`.
    fn rebuild(self: &Rc<Self>) {
        for row in self.time_rows.borrow_mut().drain(..) {
            self.times_group.remove(&row);
        }
        for row in self.range_rows.borrow_mut().drain(..) {
            self.ranges_group.remove(&row);
        }
        for row in self.process_rows.borrow_mut().drain(..) {
            self.processes_group.remove(&row);
        }
        let weak = Rc::downgrade(self);
        let schedule = self.triggers.borrow().schedule.clone();
        for (index, entry) in schedule.iter().enumerate() {
            let row = self.time_row(&weak, index, entry);
            self.times_group.add(&row);
            self.time_rows.borrow_mut().push(row.upcast());
        }
        let add_time = adw::ActionRow::builder()
            .title("Add time")
            .activatable(true)
            .build();
        add_time.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
        let handler = weak_call(&weak, |inner, ()| inner.add_time());
        add_time.connect_activated(move |_| handler(()));
        self.times_group.add(&add_time);
        self.time_rows.borrow_mut().push(add_time.upcast());

        let ranges = self.triggers.borrow().ranges.clone();
        for (index, entry) in ranges.iter().enumerate() {
            let row = self.range_row(&weak, index, entry);
            self.ranges_group.add(&row);
            self.range_rows.borrow_mut().push(row.upcast());
        }
        let add_range = adw::ActionRow::builder()
            .title("Add range")
            .activatable(true)
            .build();
        add_range.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
        let handler = weak_call(&weak, |inner, ()| inner.add_range());
        add_range.connect_activated(move |_| handler(()));
        self.ranges_group.add(&add_range);
        self.range_rows.borrow_mut().push(add_range.upcast());

        let processes = self.triggers.borrow().processes.clone();
        for process in &processes {
            let row = self.process_row(&weak, process);
            self.processes_group.add(&row);
            self.process_rows.borrow_mut().push(row.upcast());
        }
        self.add_process_controls(&weak);
    }

    fn time_row(
        self: &Rc<Self>,
        weak: &std::rc::Weak<Self>,
        index: usize,
        entry: &ScheduleEntry,
    ) -> adw::PreferencesRow {
        let picker = {
            let handler = weak_call(weak, move |inner, (hour, minute): (i32, i32)| {
                inner.edit_time(index, hour, minute);
            });
            time_picker(entry.time, handler)
        };
        let summary = summary_label(&days_caption(&entry.days));
        let days_box = day_toggles(&entry.days, |button, day| {
            let summary = summary.clone();
            let handler = weak_call(weak, move |inner, ()| {
                inner.toggle_entry_day(index, day, &summary);
            });
            button.connect_clicked(move |_| handler(()));
        });
        let remove = remove_button("Remove this time");
        let handler = weak_call(weak, move |inner, ()| inner.remove_time(index));
        remove.connect_clicked(move |_| handler(()));
        two_line_row(&[picker.upcast_ref()], &remove, &days_box, &summary)
    }

    fn range_row(
        self: &Rc<Self>,
        weak: &std::rc::Weak<Self>,
        index: usize,
        entry: &RangeEntry,
    ) -> adw::PreferencesRow {
        let hint = gtk::Label::builder()
            .label(next_day_hint(entry))
            .css_classes(["dim-label"])
            .build();
        let summary = summary_label(&days_caption(&entry.days));
        let pickers =
            [(RangeEnd::Start, entry.start), (RangeEnd::End, entry.end)].map(|(which, time)| {
                let hint = hint.clone();
                let handler = weak_call(weak, move |inner, (hour, minute): (i32, i32)| {
                    inner.edit_range(index, which, hour, minute, &hint);
                });
                time_picker(time, handler)
            });
        let arrow = gtk::Label::new(Some("→"));
        let days_box = day_toggles(&entry.days, |button, day| {
            let summary = summary.clone();
            let handler = weak_call(weak, move |inner, ()| {
                inner.toggle_range_day(index, day, &summary);
            });
            button.connect_clicked(move |_| handler(()));
        });
        let remove = remove_button("Remove this range");
        let handler = weak_call(weak, move |inner, ()| inner.remove_range(index));
        remove.connect_clicked(move |_| handler(()));
        two_line_row(
            &[
                pickers[0].upcast_ref(),
                arrow.upcast_ref(),
                pickers[1].upcast_ref(),
                hint.upcast_ref(),
            ],
            &remove,
            &days_box,
            &summary,
        )
    }

    fn process_row(self: &Rc<Self>, weak: &std::rc::Weak<Self>, process: &str) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(process)
            .use_markup(false)
            .build();
        let remove = remove_button("Remove this process");
        let process = process.to_owned();
        let handler = weak_call(weak, move |inner, ()| inner.remove_process(&process));
        remove.connect_clicked(move |_| handler(()));
        row.add_suffix(&remove);
        row
    }

    /// The "Add process" and picker rows, below the process rows.
    fn add_process_controls(self: &Rc<Self>, weak: &std::rc::Weak<Self>) {
        let add = adw::ActionRow::builder()
            .title("Add process")
            .activatable(true)
            .build();
        add.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
        let handler = weak_call(weak, |inner, ()| inner.ask_process_name());
        add.connect_activated(move |_| handler(()));
        self.processes_group.add(&add);
        let pick = adw::ActionRow::builder()
            .title("Pick running process…")
            .activatable(true)
            .build();
        pick.add_prefix(&gtk::Image::from_icon_name("system-search-symbolic"));
        let handler = weak_call(weak, |inner, ()| inner.pick_process());
        pick.connect_activated(move |_| handler(()));
        self.processes_group.add(&pick);
        // Not process rows, but they go on the next rebuild like them.
        let mut rows = self.process_rows.borrow_mut();
        rows.push(add.upcast());
        rows.push(pick.upcast());
    }

    /// Asks for a process name in a small dialog; the name is added as a trigger.
    fn ask_process_name(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Add process"),
            Some("The profile switches on while a process with this name runs."),
        );
        let entry = adw::EntryRow::builder().title("Process name").build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        list.append(&entry);
        dialog.set_extra_child(Some(&list));
        dialog.add_responses(&[("cancel", "Cancel"), ("add", "Add")]);
        dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("add"));
        dialog.set_close_response("cancel");
        {
            let dialog = dialog.clone();
            entry.connect_entry_activated(move |_| {
                dialog.close();
            });
        }
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            if let (Some(inner), "add") = (weak.upgrade(), response) {
                inner.add_process(entry.text().trim());
            }
        });
        dialog.present(Some(&self.ctx.window));
    }

    fn add_time(self: &Rc<Self>) {
        let mut next = self.triggers.borrow().clone();
        next.schedule.push(ScheduleEntry {
            time: TimeOfDay::new(NEW_TIME.0, NEW_TIME.1).expect("08:00 is a valid time"),
            days: Vec::new(),
        });
        self.commit(next, true);
    }

    fn add_range(self: &Rc<Self>) {
        let [start, end] = [NEW_RANGE.0, NEW_RANGE.1]
            .map(|(hour, minute)| TimeOfDay::new(hour, minute).expect("valid default time"));
        let mut next = self.triggers.borrow().clone();
        next.ranges.push(RangeEntry {
            start,
            end,
            days: Vec::new(),
        });
        self.commit(next, true);
    }

    fn remove_range(self: &Rc<Self>, index: usize) {
        let mut next = self.triggers.borrow().clone();
        if index < next.ranges.len() {
            next.ranges.remove(index);
            self.commit(next, true);
        }
    }

    /// A range time picker changed: updates the held value and the hint now, sends it after
    /// the debounce. An empty range (`start == end`) is kept and rejected when sent.
    fn edit_range(
        self: &Rc<Self>,
        index: usize,
        which: RangeEnd,
        hour: i32,
        minute: i32,
        hint: &gtk::Label,
    ) {
        let time = u8::try_from(hour)
            .ok()
            .zip(u8::try_from(minute).ok())
            .and_then(|(hour, minute)| TimeOfDay::new(hour, minute));
        let Some(time) = time else {
            return;
        };
        {
            let mut triggers = self.triggers.borrow_mut();
            let Some(entry) = triggers.ranges.get_mut(index) else {
                return;
            };
            let slot = match which {
                RangeEnd::Start => &mut entry.start,
                RangeEnd::End => &mut entry.end,
            };
            if *slot == time {
                return;
            }
            *slot = time;
            hint.set_label(next_day_hint(entry));
        }
        self.debounce_commit();
    }

    fn toggle_range_day(self: &Rc<Self>, index: usize, day: Weekday, summary: &gtk::Label) {
        let mut next = self.triggers.borrow().clone();
        let Some(entry) = next.ranges.get_mut(index) else {
            return;
        };
        toggle_day(&mut entry.days, day);
        summary.set_label(&days_caption(&entry.days));
        self.commit(next, false);
    }

    /// Sends the held triggers once the spin buttons have been quiet for `SPIN_DEBOUNCE`.
    fn debounce_commit(self: &Rc<Self>) {
        if let Some(previous) = self.pending.borrow_mut().take() {
            previous.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(SPIN_DEBOUNCE, move || {
            if let Some(inner) = weak.upgrade() {
                inner.pending.borrow_mut().take();
                let current = inner.triggers.borrow().clone();
                inner.commit(current, false);
            }
        });
        *self.pending.borrow_mut() = Some(id);
    }

    fn remove_time(self: &Rc<Self>, index: usize) {
        let mut next = self.triggers.borrow().clone();
        if index < next.schedule.len() {
            next.schedule.remove(index);
            self.commit(next, true);
        }
    }

    /// A time picker changed: updates the held value now, sends it after the debounce.
    fn edit_time(self: &Rc<Self>, index: usize, hour: i32, minute: i32) {
        let time = u8::try_from(hour)
            .ok()
            .zip(u8::try_from(minute).ok())
            .and_then(|(hour, minute)| TimeOfDay::new(hour, minute));
        let Some(time) = time else {
            return;
        };
        {
            let mut triggers = self.triggers.borrow_mut();
            match triggers.schedule.get_mut(index) {
                Some(entry) if entry.time != time => entry.time = time,
                _ => return,
            }
        }
        self.debounce_commit();
    }

    fn toggle_entry_day(self: &Rc<Self>, index: usize, day: Weekday, summary: &gtk::Label) {
        let mut next = self.triggers.borrow().clone();
        let Some(entry) = next.schedule.get_mut(index) else {
            return;
        };
        toggle_day(&mut entry.days, day);
        summary.set_label(&days_caption(&entry.days));
        self.commit(next, false);
    }

    fn add_process(self: &Rc<Self>, process: &str) {
        if process.is_empty() {
            return;
        }
        let mut next = self.triggers.borrow().clone();
        if next.processes.iter().any(|existing| existing == process) {
            self.ctx.toast("That process is already listed");
            return;
        }
        next.processes.push(process.to_owned());
        self.commit(next, true);
    }

    fn remove_process(self: &Rc<Self>, process: &str) {
        let mut next = self.triggers.borrow().clone();
        next.processes.retain(|existing| existing != process);
        self.commit(next, true);
    }

    /// Validates `next`; on success keeps and sends it, else toasts and restores the old rows.
    fn commit(self: &Rc<Self>, next: Triggers, rebuild: bool) {
        if let Err(message) = next.validate() {
            self.ctx.toast(&message);
            self.rebuild();
            return;
        }
        *self.triggers.borrow_mut() = next.clone();
        self.ctx.send(
            Request::ProfileSetTriggers(ProfileTriggersParams {
                name: self.name.clone(),
                triggers: next,
            }),
            "",
        );
        if rebuild {
            self.rebuild();
        }
    }

    /// Lists the running processes in a searchable dialog; a pick is added as a trigger.
    fn pick_process(self: &Rc<Self>) {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .valign(gtk::Align::Start)
            .build();
        let spinner = gtk::Spinner::builder()
            .spinning(true)
            .halign(gtk::Align::Center)
            .margin_top(12)
            .build();
        list.set_placeholder(Some(&spinner));
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search processes")
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&search);
        content.append(&scroller);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&content));
        let dialog = adw::Dialog::builder()
            .title("Running processes")
            .content_width(380)
            .content_height(520)
            .child(&toolbar)
            .build();

        {
            let (list, search) = (list.clone(), search.clone());
            list.set_filter_func(move |row| {
                let query = search.text().to_lowercase();
                query.is_empty()
                    || row
                        .downcast_ref::<adw::ActionRow>()
                        .is_some_and(|row| row.title().to_lowercase().contains(&query))
            });
        }
        {
            let list = list.clone();
            search.connect_search_changed(move |_| list.invalidate_filter());
        }
        {
            let weak = Rc::downgrade(self);
            let dialog = dialog.clone();
            list.connect_row_activated(move |_, row| {
                if let (Some(inner), Some(row)) =
                    (weak.upgrade(), row.downcast_ref::<adw::ActionRow>())
                {
                    inner.add_process(&row.title());
                    dialog.close();
                }
            });
        }
        dialog.present(Some(&self.ctx.window));

        let existing = self.triggers.borrow().processes.clone();
        glib::spawn_future_local(async move {
            let names = gio::spawn_blocking(move || {
                selectable_processes(&running_process_names(), &existing)
            })
            .await
            .unwrap_or_default();
            spinner.set_spinning(false);
            list.set_placeholder(Some(&gtk::Label::new(Some("No running processes found"))));
            for name in names {
                let row = adw::ActionRow::builder()
                    .title(&name)
                    .use_markup(false)
                    .activatable(true)
                    .build();
                list.append(&row);
            }
        });
    }
}

/// Seven linked weekday toggles; `connect` wires each button to its day.
fn day_toggles(
    active: &[Weekday],
    mut connect: impl FnMut(&gtk::ToggleButton, Weekday),
) -> gtk::Box {
    let days_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["linked"])
        .valign(gtk::Align::Center)
        .build();
    for day in Weekday::ALL {
        let label = day_label(day);
        let button = gtk::ToggleButton::builder()
            .label(&label[..2])
            .tooltip_text(label)
            .active(active.contains(&day))
            .build();
        connect(&button, day);
        days_box.append(&button);
    }
    days_box
}

/// Hint beside a range's times when it runs past midnight.
fn next_day_hint(entry: &RangeEntry) -> &'static str {
    if runs_into_next_day(entry) {
        "(next day)"
    } else {
        ""
    }
}

/// Dimmed, single-line caption that is ellipsized rather than wrapped.
fn summary_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .halign(gtk::Align::Start)
        .hexpand(true)
        .wrap(false)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["dim-label"])
        .build()
}

/// A two-line row: `first` widgets and the remove button on top, day toggles and caption below.
fn two_line_row(
    first: &[&gtk::Widget],
    remove: &gtk::Button,
    days_box: &gtk::Box,
    summary: &gtk::Label,
) -> adw::PreferencesRow {
    let line_one = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    for widget in first {
        line_one.append(*widget);
    }
    line_one.append(&gtk::Box::builder().hexpand(true).build());
    line_one.append(remove);
    let line_two = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();
    line_two.append(days_box);
    line_two.append(summary);
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&line_one);
    content.append(&line_two);
    adw::PreferencesRow::builder()
        .activatable(false)
        .child(&content)
        .build()
}

/// A button showing "HH:MM" that opens a popover with hour and minute fields.
/// `on_change` gets the hour and minute whenever either field changes.
fn time_picker(time: TimeOfDay, on_change: impl Fn((i32, i32)) + 'static) -> gtk::MenuButton {
    let hour = spin(time.hour(), 23);
    let minute = spin(time.minute(), 59);
    let fields = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    fields.append(&hour);
    fields.append(&gtk::Label::new(Some(":")));
    fields.append(&minute);
    let button = gtk::MenuButton::builder()
        .label(time.to_string())
        .popover(&gtk::Popover::builder().child(&fields).build())
        .valign(gtk::Align::Center)
        .build();
    let on_change = Rc::new(on_change);
    for field in [&hour, &minute] {
        let (hour, minute, button) = (hour.clone(), minute.clone(), button.clone());
        let on_change = Rc::clone(&on_change);
        field.connect_value_changed(move |_| {
            let (h, m) = (hour.value_as_int(), minute.value_as_int());
            button.set_label(&format!("{}:{}", two_digits(h), two_digits(m)));
            on_change((h, m));
        });
    }
    button
}

/// A vertical hour or minute field that always shows two digits.
fn spin(value: u8, max: u8) -> gtk::SpinButton {
    let spin = gtk::SpinButton::with_range(0.0, f64::from(max), 1.0);
    spin.set_orientation(gtk::Orientation::Vertical);
    spin.set_value(f64::from(value));
    spin.set_wrap(true);
    spin.set_numeric(true);
    spin.set_width_chars(2);
    spin.set_valign(gtk::Align::Center);
    spin.connect_output(|spin| {
        spin.set_text(&two_digits(spin.value_as_int()));
        glib::Propagation::Stop
    });
    spin
}

fn remove_button(tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name("list-remove-symbolic")
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build()
}
