//! LingCal's calendar file, `Projects/LingCal/events.lcal`: calendars,
//! events (timed or all-day, recurring, with reminders) and tasks.
//!
//! ```text
//! calendar "Work" navy
//! event "Standup"
//!     id 3
//!     calendar Work
//!     start 2026-11-09 09:30
//!     end 10:00
//!     repeat weekly on mon-fri
//!     until 2027-06-30
//!     skip 2026-11-12
//!     remind 10m sound
//!     location "Room 2"
//!     note "run kernel tests first"
//! task "Pay invoices"
//!     id 4
//!     due 2026-11-11 17:30
//!     done no
//! ```
//!
//! Recurrence: `repeat daily|weekly|monthly|yearly [every N] [on ...]`,
//! weekly `on mon,wed`, monthly `on day 14` / `on 2nd tue` / `on last fri`,
//! ending `until <date>` or `count N`; `skip <date>` drops one occurrence
//! ("delete this one"); "edit this one" skips it in the series and adds a
//! stand-alone copy carrying `of <series id> <date>`. Pure `alloc` code.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::cron::{self, civil_from_days, days_from_civil, days_in_month, weekday, MIN_PER_DAY};

pub const COLORS: &[(&str, [u32; 3])] = &[
    ("red", [0x3d1519, 0xa8323a, 0x7d242b]),
    ("orange", [0x3d2810, 0xb0702a, 0x80501d]),
    ("olive", [0x353312, 0xa39a2a, 0x77701e]),
    ("green", [0x12301f, 0x2f8a55, 0x22663e]),
    ("teal", [0x0e2b38, 0x2a7fa0, 0x1e5d76]),
    ("navy", [0x171a40, 0x3a44a8, 0x2a317b]),
    ("purple", [0x26183b, 0x6d45b0, 0x4f3281]),
];

pub fn color_index(name: &str) -> Option<usize> {
    COLORS.iter().position(|(n, _)| n.eq_ignore_ascii_case(name.trim()))
}

#[derive(Clone, Debug)]
pub struct Calendar {
    pub name: String,
    pub color: usize,
    pub hidden: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MonthBy {
    Day(u32),
    /// n-th (1-5, or -1 = last) weekday (0 = Monday).
    Nth(i32, u32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub freq: Freq,
    pub every: u32,
    pub days: u8,
    pub month_by: Option<MonthBy>,
    pub until: Option<i64>,
    pub count: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub id: u32,
    pub task: bool,
    pub title: String,
    pub cal: String,
    pub allday: bool,
    pub start_day: i64,
    pub start_min: u32,
    /// Timed: the end (may be a later day). All-day: the last day (inclusive).
    pub end_day: i64,
    pub end_min: u32,
    pub rule: Option<Rule>,
    pub skips: Vec<i64>,
    pub reminders: Vec<(u32, bool)>,
    pub location: String,
    pub note: String,
    pub done: bool,
    pub done_on: Vec<i64>,
    /// An edited occurrence of a series: (series id, its date).
    pub of: Option<(u32, i64)>,
}

impl Event {
    pub fn new(id: u32) -> Event {
        Event {
            id,
            task: false,
            title: String::new(),
            cal: String::new(),
            allday: false,
            start_day: 0,
            start_min: 9 * 60,
            end_day: 0,
            end_min: 10 * 60,
            rule: None,
            skips: Vec::new(),
            reminders: Vec::new(),
            location: String::new(),
            note: String::new(),
            done: false,
            done_on: Vec::new(),
            of: None,
        }
    }
    /// Length in minutes (timed events).
    pub fn duration(&self) -> i64 {
        if self.allday {
            return (self.end_day - self.start_day + 1) * MIN_PER_DAY;
        }
        ((self.end_day * MIN_PER_DAY + self.end_min as i64) - (self.start_day * MIN_PER_DAY + self.start_min as i64)).max(0)
    }
    pub fn is_done_on(&self, day: i64) -> bool {
        if self.rule.is_some() {
            self.done_on.contains(&day)
        } else {
            self.done
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Book {
    pub preamble: String,
    pub cals: Vec<Calendar>,
    pub events: Vec<Event>,
}

#[derive(Clone, Debug)]
pub struct Error {
    pub line: usize,
    pub msg: String,
}

// ── parsing ──────────────────────────────────────────────────────────────────

/// Words of a line, keeping "quoted text" as one word (quotes removed), comment cut.
fn words(l: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q = false;
    let mut had = false;
    let mut esc = false;
    for c in l.chars() {
        if esc {
            cur.push(match c {
                'n' => '\n',
                o => o,
            });
            esc = false;
            continue;
        }
        if q {
            match c {
                '\\' => esc = true,
                '"' => q = false,
                o => cur.push(o),
            }
            continue;
        }
        match c {
            '"' => {
                q = true;
                had = true;
            },
            '#' => break,
            c if c.is_whitespace() => {
                if had || !cur.is_empty() {
                    out.push(core::mem::take(&mut cur));
                    had = false;
                }
            },
            o => cur.push(o),
        }
    }
    if had || !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn quote(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn ordinal(w: &str) -> Option<i32> {
    match w.to_ascii_lowercase().as_str() {
        "1st" | "first" => Some(1),
        "2nd" | "second" => Some(2),
        "3rd" | "third" => Some(3),
        "4th" | "fourth" => Some(4),
        "5th" | "fifth" => Some(5),
        "last" => Some(-1),
        _ => None,
    }
}

fn parse_rule(w: &[String], start_day: i64, line: usize) -> Result<Rule, Error> {
    let e = |m: &str| Error { line, msg: String::from(m) };
    let freq = match w.get(1).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("daily") => Freq::Daily,
        Some("weekly") => Freq::Weekly,
        Some("monthly") => Freq::Monthly,
        Some("yearly") | Some("annually") => Freq::Yearly,
        _ => return Err(e("repeat is daily, weekly, monthly or yearly")),
    };
    let mut r = Rule { freq, every: 1, days: 0, month_by: None, until: None, count: None };
    let mut i = 2;
    while i < w.len() {
        let k = w[i].to_ascii_lowercase();
        match k.as_str() {
            "every" => {
                r.every = w.get(i + 1).and_then(|v| v.parse().ok()).filter(|v| (1..=999).contains(v)).ok_or_else(|| e("every N: N is 1-999"))?;
                i += 2;
            },
            "on" => {
                let rest: Vec<&String> = w[i + 1..].iter().take_while(|x| !matches!(x.to_ascii_lowercase().as_str(), "until" | "count" | "every")).collect();
                let n = rest.len();
                if freq == Freq::Monthly {
                    if rest.first().map(|x| x.eq_ignore_ascii_case("day")).unwrap_or(false) {
                        let d: u32 = rest.get(1).and_then(|v| v.parse().ok()).filter(|d| (1..=31).contains(d)).ok_or_else(|| e("monthly on day N: N is 1-31"))?;
                        r.month_by = Some(MonthBy::Day(d));
                    } else {
                        let nth = rest.first().and_then(|x| ordinal(x)).ok_or_else(|| e("monthly on 2nd tue / on last fri / on day 14"))?;
                        let wd = rest.get(1).and_then(|x| cron::weekday_named(x)).ok_or_else(|| e("monthly on 2nd tue: a weekday after the number"))?;
                        r.month_by = Some(MonthBy::Nth(nth, wd));
                    }
                } else {
                    let s: Vec<&str> = rest.iter().map(|x| x.as_str()).collect();
                    r.days = cron::parse_days(&s.join(" ")).ok_or_else(|| e("weekly on mon,wed,fri"))?;
                }
                i += 1 + n;
            },
            "until" => {
                r.until = Some(w.get(i + 1).and_then(|v| cron::parse_date(v)).ok_or_else(|| e("until YYYY-MM-DD"))?);
                i += 2;
            },
            "count" => {
                r.count = Some(w.get(i + 1).and_then(|v| v.parse().ok()).filter(|v| (1..=9999).contains(v)).ok_or_else(|| e("count N: N is 1-9999"))?);
                i += 2;
            },
            _ => return Err(Error { line, msg: format!("`{}` is not part of a repeat rule", w[i]) }),
        }
    }
    if freq == Freq::Weekly && r.days == 0 {
        r.days = 1 << weekday(start_day);
    }
    Ok(r)
}

/// Parse one block's lines into an event (first word `event` / `task`).
fn parse_block(lines: &[(usize, Vec<String>)], next_id: &mut u32, errs: &mut Vec<Error>) -> Option<Event> {
    let (l0, head) = &lines[0];
    let task = head[0].eq_ignore_ascii_case("task");
    let mut ev = Event::new(0);
    ev.task = task;
    ev.title = head.get(1).cloned().unwrap_or_default();
    if ev.title.trim().is_empty() {
        errs.push(Error { line: *l0, msg: String::from("an event needs a title in quotes: event \"Dentist\"") });
        return None;
    }
    let mut have_start = false;
    let mut end_given: Option<(Option<i64>, u32)> = None;
    let mut rule_words: Option<(usize, Vec<String>)> = None;
    let mut extra: Vec<(usize, Vec<String>)> = Vec::new();
    for (ln, w) in &lines[1..] {
        let k = w[0].to_ascii_lowercase();
        let e = |m: String| Error { line: *ln, msg: m };
        match k.as_str() {
            "id" => match w.get(1).and_then(|v| v.parse::<u32>().ok()) {
                Some(v) if v > 0 => ev.id = v,
                _ => errs.push(e(String::from("id is a positive number"))),
            },
            "calendar" | "cal" => ev.cal = w.get(1).cloned().unwrap_or_default(),
            "start" | "due" | "at" => {
                let d = w.get(1).and_then(|v| cron::parse_date(v));
                let t = w.get(2).and_then(|v| cron::parse_hm(v));
                match (d, t) {
                    (Some(d), Some(t)) => {
                        ev.start_day = d;
                        ev.start_min = t;
                        have_start = true;
                    },
                    (Some(d), None) if k == "due" => {
                        ev.start_day = d;
                        ev.start_min = 23 * 60 + 59;
                        have_start = true;
                    },
                    _ => errs.push(e(format!("{k} YYYY-MM-DD HH:MM"))),
                }
            },
            "date" | "day" => {
                // all-day: `date 2026-11-14` or `date 2026-11-14 to 2026-11-16`
                match w.get(1).and_then(|v| cron::parse_date(v)) {
                    Some(d) => {
                        ev.allday = true;
                        ev.start_day = d;
                        ev.end_day = d;
                        ev.start_min = 0;
                        ev.end_min = 0;
                        have_start = true;
                        if w.get(2).map(|x| x.eq_ignore_ascii_case("to")).unwrap_or(false) {
                            match w.get(3).and_then(|v| cron::parse_date(v)) {
                                Some(d2) if d2 >= d => ev.end_day = d2,
                                _ => errs.push(e(String::from("date A to B: B is a date on or after A"))),
                            }
                        }
                    },
                    None => errs.push(e(String::from("date YYYY-MM-DD"))),
                }
            },
            "end" => {
                // `end 10:00` (same day) or `end 2026-11-10 10:00`
                let (d, t) = match (w.get(1), w.get(2)) {
                    (Some(a), Some(b)) => (cron::parse_date(a), cron::parse_hm(b)),
                    (Some(a), None) => (None, cron::parse_hm(a)),
                    _ => (None, None),
                };
                match t {
                    Some(t) => end_given = Some((d, t)),
                    None => errs.push(e(String::from("end HH:MM or end YYYY-MM-DD HH:MM"))),
                }
            },
            "duration" | "for" => match w.get(1).and_then(|v| cron::parse_duration(v)) {
                Some(s) => extra.push((*ln, alloc::vec![String::from("duration"), format!("{}", s / 60)])),
                None => errs.push(e(String::from("duration like 30m or 1h30m"))),
            },
            "repeat" | "repeats" | "rrule" => rule_words = Some((*ln, w.clone())),
            "skip" | "except" => match w.get(1).and_then(|v| cron::parse_date(v)) {
                Some(d) => ev.skips.push(d),
                None => errs.push(e(String::from("skip YYYY-MM-DD"))),
            },
            "remind" | "reminder" | "alert" => {
                let mins = match w.get(1).map(|s| s.to_ascii_lowercase()).as_deref() {
                    Some("now") | Some("at_start") | Some("0") => Some(0),
                    Some(v) => cron::parse_duration(v).map(|s| (s / 60) as u32),
                    None => None,
                };
                let sound = w.iter().any(|x| x.eq_ignore_ascii_case("sound"));
                match mins {
                    Some(m) if m <= 60 * 24 * 14 && ev.reminders.len() < 8 => ev.reminders.push((m, sound)),
                    Some(_) => errs.push(e(String::from("a reminder is at most 14 days before, 8 per event"))),
                    None => errs.push(e(String::from("remind 10m [sound] / remind 1h / remind now"))),
                }
            },
            "location" | "where" => ev.location = w[1..].join(" "),
            "note" | "notes" => ev.note = w[1..].join(" "),
            "done" => {
                ev.done = matches!(w.get(1).map(|s| s.to_ascii_lowercase()).as_deref(), Some("yes") | Some("true") | Some("x"));
            },
            "done_on" => {
                if let Some(d) = w.get(1).and_then(|v| cron::parse_date(v)) {
                    ev.done_on.push(d);
                }
            },
            "of" => {
                let sid = w.get(1).and_then(|v| v.parse::<u32>().ok());
                let d = w.get(2).and_then(|v| cron::parse_date(v));
                if let (Some(s), Some(d)) = (sid, d) {
                    ev.of = Some((s, d));
                }
            },
            _ => errs.push(e(format!("unknown event field `{}`", w[0]))),
        }
    }
    if !have_start {
        errs.push(Error { line: *l0, msg: format!("\"{}\" needs `start YYYY-MM-DD HH:MM` or `date YYYY-MM-DD`", ev.title) });
        return None;
    }
    if !ev.allday {
        match end_given {
            Some((Some(d), t)) => {
                ev.end_day = d;
                ev.end_min = t;
            },
            Some((None, t)) => {
                ev.end_day = ev.start_day + if t <= ev.start_min && !ev.task { 1 } else { 0 };
                ev.end_min = t;
            },
            None => {
                let dur = extra.iter().find(|(_, w)| w[0] == "duration").and_then(|(_, w)| w[1].parse::<i64>().ok()).unwrap_or(if ev.task { 0 } else { 60 });
                let end = ev.start_day * MIN_PER_DAY + ev.start_min as i64 + dur;
                ev.end_day = end.div_euclid(MIN_PER_DAY);
                ev.end_min = end.rem_euclid(MIN_PER_DAY) as u32;
            },
        }
        if ev.duration() < 0 || ev.duration() > 14 * MIN_PER_DAY {
            errs.push(Error { line: *l0, msg: String::from("an event ends after it starts, within 14 days") });
            return None;
        }
    }
    if let Some((ln, rw)) = rule_words {
        match parse_rule(&rw, ev.start_day, ln) {
            Ok(r) => ev.rule = Some(r),
            Err(x) => errs.push(x),
        }
    }
    if ev.id == 0 {
        ev.id = *next_id;
    }
    *next_id = (*next_id).max(ev.id + 1);
    Some(ev)
}

/// Parse a whole `.lcal` file; bad blocks are dropped and reported.
pub fn parse(text: &str) -> (Book, Vec<Error>) {
    let mut book = Book::default();
    let mut errs = Vec::new();
    // leading comment lines are kept when the file is written back
    for l in text.lines() {
        if l.trim_start().starts_with('#') || l.trim().is_empty() {
            book.preamble.push_str(l.trim_end());
            book.preamble.push('\n');
        } else {
            break;
        }
    }
    let mut blocks: Vec<Vec<(usize, Vec<String>)>> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let w = words(raw);
        if w.is_empty() {
            continue;
        }
        let k = w[0].to_ascii_lowercase();
        // a top-level `calendar "Work" navy` (unindented); indented, or bare
        // inside an event, it is that event's calendar field
        let in_event = blocks.last().map(|b| !b.is_empty()).unwrap_or(false);
        if k == "calendar" && !raw.starts_with(char::is_whitespace) && (w.len() >= 3 || !in_event) {
            // calendar "Work" navy [hidden]
            let name = w.get(1).cloned().unwrap_or_default();
            if name.is_empty() {
                errs.push(Error { line: i + 1, msg: String::from("calendar \"Name\" color") });
                continue;
            }
            let color = w.get(2).and_then(|c| color_index(c)).unwrap_or(book.cals.len() % COLORS.len());
            let hidden = w.iter().any(|x| x.eq_ignore_ascii_case("hidden"));
            if !book.cals.iter().any(|c| c.name == name) {
                book.cals.push(Calendar { name, color, hidden });
            }
            blocks.push(Vec::new());
            continue;
        }
        if k == "event" || k == "task" {
            blocks.push(alloc::vec![(i + 1, w)]);
            continue;
        }
        match blocks.last_mut() {
            Some(b) if !b.is_empty() => b.push((i + 1, w)),
            _ => errs.push(Error { line: i + 1, msg: format!("`{}` is outside an event (start one with event \"Title\")", w[0]) }),
        }
    }
    let mut next_id = 1u32;
    // ids first, so new blocks get fresh numbers
    for b in &blocks {
        for (_, w) in b.iter().skip(1) {
            if w[0].eq_ignore_ascii_case("id") {
                if let Some(v) = w.get(1).and_then(|v| v.parse::<u32>().ok()) {
                    next_id = next_id.max(v + 1);
                }
            }
        }
    }
    for b in blocks.iter().filter(|b| !b.is_empty()) {
        if let Some(ev) = parse_block(b, &mut next_id, &mut errs) {
            if book.events.iter().any(|x| x.id == ev.id) {
                let mut ev = ev;
                ev.id = next_id;
                next_id += 1;
                book.events.push(ev);
            } else {
                book.events.push(ev);
            }
        }
    }
    // calendars named by events but not declared
    let names: Vec<String> = book.events.iter().map(|e| e.cal.clone()).filter(|c| !c.is_empty()).collect();
    for n in names {
        if !book.cals.iter().any(|c| c.name == n) {
            let color = book.cals.len() % COLORS.len();
            book.cals.push(Calendar { name: n, color, hidden: false });
        }
    }
    (book, errs)
}

/// One event (block text from the editor); `id` 0 = a new one.
pub fn parse_one(text: &str, next_id: u32) -> Result<Event, Vec<Error>> {
    let (book, errs) = parse(text);
    if !errs.is_empty() {
        return Err(errs);
    }
    match book.events.len() {
        1 => {
            let mut e = book.events.into_iter().next().unwrap();
            if !text.lines().any(|l| words(l).first().map(|w| w.eq_ignore_ascii_case("id")).unwrap_or(false)) {
                e.id = next_id;
            }
            Ok(e)
        },
        n => Err(alloc::vec![Error { line: 1, msg: format!("expected one event, found {n}") }]),
    }
}

fn days_list(mask: u8) -> String {
    const N: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    if mask == 0x1F {
        return String::from("mon-fri");
    }
    if mask == 0x7F {
        return String::from("daily");
    }
    (0..7).filter(|i| mask & (1 << i) != 0).map(|i| N[i]).collect::<Vec<_>>().join(",")
}

pub fn rule_text(r: &Rule) -> String {
    let mut s = String::from(match r.freq {
        Freq::Daily => "daily",
        Freq::Weekly => "weekly",
        Freq::Monthly => "monthly",
        Freq::Yearly => "yearly",
    });
    if r.every > 1 {
        s.push_str(&format!(" every {}", r.every));
    }
    if r.freq == Freq::Weekly && r.days != 0 {
        s.push_str(" on ");
        s.push_str(&days_list(r.days));
    }
    match r.month_by {
        Some(MonthBy::Day(d)) => s.push_str(&format!(" on day {d}")),
        Some(MonthBy::Nth(n, wd)) => {
            const N: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
            let o = match n {
                1 => "1st",
                2 => "2nd",
                3 => "3rd",
                4 => "4th",
                5 => "5th",
                _ => "last",
            };
            s.push_str(&format!(" on {o} {}", N[wd as usize]));
        },
        None => {},
    }
    if let Some(u) = r.until {
        s.push_str(&format!(" until {}", cron::fmt_date(u)));
    }
    if let Some(c) = r.count {
        s.push_str(&format!(" count {c}"));
    }
    s
}

/// A human sentence for the inspector: "Weekly on Mon, Tue, Wed, Thu, Fri".
pub fn rule_words(r: &Rule) -> String {
    let unit = match r.freq {
        Freq::Daily => "day",
        Freq::Weekly => "week",
        Freq::Monthly => "month",
        Freq::Yearly => "year",
    };
    let mut s = if r.every > 1 { format!("Every {} {unit}s", r.every) } else { String::from(match r.freq {
        Freq::Daily => "Daily",
        Freq::Weekly => "Weekly",
        Freq::Monthly => "Monthly",
        Freq::Yearly => "Yearly",
    }) };
    if r.freq == Freq::Weekly {
        s.push_str(" on ");
        s.push_str(&cron::days_words(r.days));
    }
    match r.month_by {
        Some(MonthBy::Day(d)) => s.push_str(&format!(" on day {d}")),
        Some(MonthBy::Nth(n, wd)) => {
            let o = match n {
                1 => "1st",
                2 => "2nd",
                3 => "3rd",
                4 => "4th",
                5 => "5th",
                _ => "last",
            };
            s.push_str(&format!(" on the {o} {}", cron::WD_SHORT[wd as usize]));
        },
        None => {},
    }
    if let Some(u) = r.until {
        s.push_str(&format!(", until {}", cron::fmt_date(u)));
    }
    if let Some(c) = r.count {
        s.push_str(&format!(", {c} times"));
    }
    s
}

/// The block for one event.
pub fn block(e: &Event) -> String {
    let mut s = format!("{} {}\n    id {}\n", if e.task { "task" } else { "event" }, quote(&e.title), e.id);
    if !e.cal.is_empty() {
        s.push_str(&format!("    calendar {}\n", quote(&e.cal)));
    }
    if e.allday {
        if e.end_day > e.start_day {
            s.push_str(&format!("    date {} to {}\n", cron::fmt_date(e.start_day), cron::fmt_date(e.end_day)));
        } else {
            s.push_str(&format!("    date {}\n", cron::fmt_date(e.start_day)));
        }
    } else if e.task {
        s.push_str(&format!("    due {} {}\n", cron::fmt_date(e.start_day), cron::fmt_hm(e.start_min as i64)));
    } else {
        s.push_str(&format!("    start {} {}\n", cron::fmt_date(e.start_day), cron::fmt_hm(e.start_min as i64)));
        if e.end_day == e.start_day || (e.end_day == e.start_day + 1 && e.end_min <= e.start_min) {
            s.push_str(&format!("    end {}\n", cron::fmt_hm(e.end_min as i64)));
        } else {
            s.push_str(&format!("    end {} {}\n", cron::fmt_date(e.end_day), cron::fmt_hm(e.end_min as i64)));
        }
    }
    if let Some(r) = &e.rule {
        s.push_str(&format!("    repeat {}\n", rule_text(r)));
    }
    for d in &e.skips {
        s.push_str(&format!("    skip {}\n", cron::fmt_date(*d)));
    }
    for (m, snd) in &e.reminders {
        let t = if *m == 0 { String::from("now") } else { cron::fmt_duration(*m as u64 * 60) };
        s.push_str(&format!("    remind {t}{}\n", if *snd { " sound" } else { "" }));
    }
    if !e.location.is_empty() {
        s.push_str(&format!("    location {}\n", quote(&e.location)));
    }
    if !e.note.is_empty() {
        s.push_str(&format!("    note {}\n", quote(&e.note)));
    }
    if e.task {
        s.push_str(&format!("    done {}\n", if e.done { "yes" } else { "no" }));
        for d in &e.done_on {
            s.push_str(&format!("    done_on {}\n", cron::fmt_date(*d)));
        }
    }
    if let Some((sid, d)) = e.of {
        s.push_str(&format!("    of {sid} {}\n", cron::fmt_date(d)));
    }
    s
}

pub fn write(book: &Book) -> String {
    let mut s = String::new();
    if book.preamble.trim().is_empty() {
        s.push_str("# LingCal events -- written by LingCal, safe to edit by hand.\n# Secrets never belong here; reminders, places and notes only.\n\n");
    } else {
        s.push_str(&book.preamble);
        if !s.ends_with("\n\n") {
            s.push('\n');
        }
    }
    for c in &book.cals {
        s.push_str(&format!("calendar {} {}{}\n", quote(&c.name), COLORS[c.color % COLORS.len()].0, if c.hidden { " hidden" } else { "" }));
    }
    s.push('\n');
    for e in &book.events {
        s.push_str(&block(e));
        s.push('\n');
    }
    s
}

// ── recurrence ───────────────────────────────────────────────────────────────

/// Is `day` an occurrence date of a rule starting on `start`? (Ignores count.)
fn nth_weekday(y: i64, m: u32, n: i32, wd: u32) -> Option<u32> {
    let first = days_from_civil(y, m, 1);
    let dim = days_in_month(y, m);
    if n > 0 {
        let off = (wd + 7 - weekday(first)) % 7;
        let d = 1 + off + (n as u32 - 1) * 7;
        (d <= dim).then_some(d)
    } else {
        let last = days_from_civil(y, m, dim);
        let off = (weekday(last) + 7 - wd) % 7;
        Some(dim - off)
    }
}

/// Every occurrence start day of `e` from its start up to `to_day` (exclusive),
/// honouring every / until / count; skips are removed. Bounded work.
pub fn occurrence_days(e: &Event, to_day: i64) -> Vec<i64> {
    let Some(r) = &e.rule else {
        return if e.start_day < to_day { alloc::vec![e.start_day] } else { Vec::new() };
    };
    let mut out: Vec<i64> = Vec::new();
    let end = match r.until {
        Some(u) => to_day.min(u + 1),
        None => to_day,
    };
    let limit = r.count.unwrap_or(u32::MAX) as usize;
    let every = r.every.max(1) as i64;
    let mut produced = 0usize;
    let push = |d: i64, out: &mut Vec<i64>, produced: &mut usize| -> bool {
        if d < e.start_day {
            return true;
        }
        if d >= end || *produced >= limit {
            return false;
        }
        *produced += 1;
        if !e.skips.contains(&d) {
            out.push(d);
        }
        true
    };
    let (sy, sm, sd) = civil_from_days(e.start_day);
    match r.freq {
        Freq::Daily => {
            let mut d = e.start_day;
            for _ in 0..40000 {
                if !push(d, &mut out, &mut produced) {
                    break;
                }
                d += every;
            }
        },
        Freq::Weekly => {
            // weeks counted from the Monday of the start's week
            let monday = e.start_day - weekday(e.start_day) as i64;
            let mut wk = monday;
            'w: for _ in 0..6000 {
                for i in 0..7 {
                    if r.days & (1 << i) != 0 {
                        if !push(wk + i as i64, &mut out, &mut produced) {
                            break 'w;
                        }
                    }
                }
                wk += 7 * every;
            }
        },
        Freq::Monthly => {
            let mut k: i64 = 0;
            for _ in 0..2400 {
                let mi = (sm as i64 - 1) + k * every;
                let y = sy + mi.div_euclid(12);
                let m = (mi.rem_euclid(12) + 1) as u32;
                let day = match r.month_by {
                    Some(MonthBy::Nth(n, wd)) => nth_weekday(y, m, n, wd),
                    Some(MonthBy::Day(d)) => (d <= days_in_month(y, m)).then_some(d),
                    None => (sd <= days_in_month(y, m)).then_some(sd),
                };
                if let Some(d) = day {
                    if !push(days_from_civil(y, m, d), &mut out, &mut produced) {
                        break;
                    }
                } else if days_from_civil(y, m, 1) >= end {
                    break;
                }
                k += 1;
            }
        },
        Freq::Yearly => {
            let mut k: i64 = 0;
            for _ in 0..400 {
                let y = sy + k * every;
                if sd <= days_in_month(y, sm) {
                    if !push(days_from_civil(y, sm, sd), &mut out, &mut produced) {
                        break;
                    }
                } else if days_from_civil(y, sm, 1) >= end {
                    break;
                }
                k += 1;
            }
        },
    }
    out
}

#[derive(Clone, Debug)]
pub struct Occ {
    pub id: u32,
    /// The occurrence's first day and its start/end in local minutes.
    pub day: i64,
    pub start: i64,
    pub end: i64,
}

/// Occurrences overlapping `[from_day, to_day)`, sorted by start.
pub fn occurrences(book: &Book, from_day: i64, to_day: i64) -> Vec<Occ> {
    let mut v = Vec::new();
    for e in &book.events {
        let dur = e.duration();
        let span_days = (dur + MIN_PER_DAY - 1) / MIN_PER_DAY + 1;
        // a long rule computes from its start: limit the work to what can reach the range
        for d in occurrence_days(e, to_day) {
            if d + span_days < from_day {
                continue;
            }
            let start = d * MIN_PER_DAY + if e.allday { 0 } else { e.start_min as i64 };
            let end = start + if e.allday { dur } else { dur.max(if e.task { 0 } else { 1 }) };
            if end < from_day * MIN_PER_DAY || start >= to_day * MIN_PER_DAY {
                continue;
            }
            v.push(Occ { id: e.id, day: d, start, end });
        }
    }
    v.sort_by_key(|o| (o.start, o.id));
    v
}

/// Reminders firing in `(from_lm, to_lm]`: (fire, id, occurrence start, minutes before, sound).
pub fn reminders(book: &Book, from_lm: i64, to_lm: i64) -> Vec<(i64, u32, i64, u32, bool)> {
    let mut v = Vec::new();
    let from_day = from_lm.div_euclid(MIN_PER_DAY) - 1;
    let to_day = to_lm.div_euclid(MIN_PER_DAY) + 16;
    for o in occurrences(book, from_day, to_day) {
        let Some(e) = book.events.iter().find(|e| e.id == o.id) else { continue };
        if e.task && e.is_done_on(o.day) {
            continue;
        }
        for (m, snd) in &e.reminders {
            let fire = o.start - *m as i64;
            if fire > from_lm && fire <= to_lm {
                v.push((fire, e.id, o.start, *m, *snd));
            }
        }
    }
    v.sort_by_key(|r| r.0);
    v
}

impl Book {
    pub fn next_id(&self) -> u32 {
        self.events.iter().map(|e| e.id).max().unwrap_or(0) + 1
    }
    pub fn get(&self, id: u32) -> Option<&Event> {
        self.events.iter().find(|e| e.id == id)
    }
    pub fn get_mut(&mut self, id: u32) -> Option<&mut Event> {
        self.events.iter_mut().find(|e| e.id == id)
    }
    pub fn cal_color(&self, name: &str) -> usize {
        self.cals.iter().find(|c| c.name == name).map(|c| c.color).unwrap_or(4)
    }
    pub fn cal_hidden(&self, name: &str) -> bool {
        self.cals.iter().find(|c| c.name == name).map(|c| c.hidden).unwrap_or(false)
    }

    /// Delete one occurrence (`skip`) of a series, or the whole event.
    pub fn delete(&mut self, id: u32, day: Option<i64>) -> bool {
        match (self.get_mut(id), day) {
            (Some(e), Some(d)) if e.rule.is_some() => {
                if !e.skips.contains(&d) {
                    e.skips.push(d);
                }
                true
            },
            (Some(_), _) => {
                self.events.retain(|e| e.id != id);
                true
            },
            (None, _) => false,
        }
    }

    /// Move an event: the whole series (`one` = false) shifts by the same
    /// delta; `one` on a series detaches that occurrence as its own event.
    /// Returns the id of the event that now holds the moved occurrence.
    pub fn move_to(&mut self, id: u32, occ_day: i64, new_start: i64, new_len: i64, one: bool) -> Option<u32> {
        let next = self.next_id();
        let e = self.get_mut(id)?;
        let len = new_len.max(if e.task { 0 } else { 5 });
        if e.rule.is_some() && one {
            let mut copy = e.clone();
            if !e.skips.contains(&occ_day) {
                e.skips.push(occ_day);
            }
            copy.id = next;
            copy.rule = None;
            copy.skips.clear();
            copy.of = Some((id, occ_day));
            set_times(&mut copy, new_start, len);
            self.events.push(copy);
            return Some(next);
        }
        if e.rule.is_some() {
            // shift the series' anchor by the same delta as this occurrence
            let old = occ_day * MIN_PER_DAY + if e.allday { 0 } else { e.start_min as i64 };
            let delta = new_start - old;
            let anchor = e.start_day * MIN_PER_DAY + e.start_min as i64 + delta;
            set_times(e, anchor, len);
            if let Some(r) = &mut e.rule {
                let dd = delta.div_euclid(MIN_PER_DAY);
                if r.freq == Freq::Weekly && dd != 0 {
                    let mut nd = 0u8;
                    for i in 0..7 {
                        if r.days & (1 << i) != 0 {
                            nd |= 1 << ((i as i64 + dd).rem_euclid(7));
                        }
                    }
                    r.days = nd;
                }
            }
            return Some(id);
        }
        set_times(e, new_start, len);
        Some(id)
    }
}

fn set_times(e: &mut Event, start: i64, len: i64) {
    e.start_day = start.div_euclid(MIN_PER_DAY);
    if e.allday {
        let days = (len / MIN_PER_DAY).max(1);
        e.start_min = 0;
        e.end_day = e.start_day + days - 1;
        e.end_min = 0;
    } else {
        e.start_min = start.rem_euclid(MIN_PER_DAY) as u32;
        let end = start + len;
        e.end_day = end.div_euclid(MIN_PER_DAY);
        e.end_min = end.rem_euclid(MIN_PER_DAY) as u32;
    }
}
