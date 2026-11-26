//! Dates, local minutes and the trigger arithmetic of LingScheduler: a
//! classic five-field cron expression, an `every <interval> between` window,
//! a one-off `once at`. Pure `alloc` code (no kernel calls) so the host tests
//! in the scratch crate exercise exactly what the kernel runs.
//!
//! Times are **local minutes**: minutes since 1970-01-01 00:00 in the user's
//! zone (the desktop's fixed UTC offset; there is no DST table). `unix =
//! lm * 60 - offset_s`.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub const MIN_PER_DAY: i64 = 1440;

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// (year, month 1-12, day 1-31) for days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 0 = Monday .. 6 = Sunday (1970-01-01 was a Thursday).
pub fn weekday(days: i64) -> u32 {
    (days + 3).rem_euclid(7) as u32
}

pub fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap(y) {
                29
            } else {
                28
            }
        },
    }
}

pub const WD_SHORT: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
pub const MON_SHORT: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// A weekday name (`mon`, `Monday`, `tue`, ...) -> 0 = Monday.
pub fn weekday_named(s: &str) -> Option<u32> {
    let l = s.trim().to_ascii_lowercase();
    if l.len() < 2 {
        return None;
    }
    const NAMES: [&str; 7] = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];
    for (i, n) in NAMES.iter().enumerate() {
        if n.starts_with(l.as_str()) && (l.len() >= 3 || l == "mo" || l == "tu" || l == "we" || l == "th" || l == "fr" || l == "sa" || l == "su") {
            return Some(i as u32);
        }
    }
    None
}

/// `HH:MM` (also `H:MM`, `9am`, `6pm`, `6:30pm`) -> minutes of the day.
pub fn parse_hm(s: &str) -> Option<u32> {
    let l = s.trim().to_ascii_lowercase();
    let (body, pm, am) = if let Some(b) = l.strip_suffix("pm") {
        (b.trim(), true, false)
    } else if let Some(b) = l.strip_suffix("am") {
        (b.trim(), false, true)
    } else {
        (l.as_str(), false, false)
    };
    let (h, m) = match body.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None if pm || am => (body.parse::<u32>().ok()?, 0),
        None => return None,
    };
    let h = if pm && h < 12 {
        h + 12
    } else if am && h == 12 {
        0
    } else {
        h
    };
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

/// `YYYY-MM-DD` -> days since 1970.
pub fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.trim().split('-');
    let y = it.next()?.parse::<i64>().ok()?;
    let m = it.next()?.parse::<u32>().ok()?;
    let d = it.next()?.parse::<u32>().ok()?;
    if it.next().is_some() || !(1900..=9999).contains(&y) || !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

pub fn fmt_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn fmt_hm(min_of_day: i64) -> String {
    let m = min_of_day.rem_euclid(MIN_PER_DAY);
    format!("{:02}:{:02}", m / 60, m % 60)
}

/// `2026-11-14 14:00`
pub fn fmt_lm(lm: i64) -> String {
    format!("{} {}", fmt_date(lm.div_euclid(MIN_PER_DAY)), fmt_hm(lm.rem_euclid(MIN_PER_DAY)))
}

/// A short human form relative to `now_lm`: `Today 14:00`, `Tomorrow 08:00`, `Sun 10:00`, `Nov 20 09:00`.
pub fn fmt_rel(lm: i64, now_lm: i64) -> String {
    let d = lm.div_euclid(MIN_PER_DAY);
    let today = now_lm.div_euclid(MIN_PER_DAY);
    let hm = fmt_hm(lm.rem_euclid(MIN_PER_DAY));
    if d == today {
        format!("Today {hm}")
    } else if d == today + 1 {
        format!("Tomorrow {hm}")
    } else if d > today && d < today + 7 {
        format!("{} {hm}", WD_SHORT[weekday(d) as usize])
    } else {
        let (_, m, dd) = civil_from_days(d);
        format!("{} {dd} {hm}", MON_SHORT[(m - 1) as usize])
    }
}

/// `30s`, `5m`, `2h`, `1d`, `90` (seconds), `1.5h` -> seconds.
pub fn parse_duration(s: &str) -> Option<u64> {
    let l = s.trim().to_ascii_lowercase();
    let split = l.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(l.len());
    let (num, unit) = l.split_at(split);
    let v: f64 = num.parse().ok()?;
    let mult = match unit.trim() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 86400.0,
        "w" | "week" | "weeks" => 604800.0,
        _ => return None,
    };
    if !(v >= 0.0) || v * mult > 400.0 * 86400.0 {
        return None;
    }
    Some((v * mult) as u64)
}

pub fn fmt_duration(secs: u64) -> String {
    if secs % 86400 == 0 && secs >= 86400 {
        format!("{}d", secs / 86400)
    } else if secs % 3600 == 0 && secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs % 60 == 0 && secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

// ── cron ─────────────────────────────────────────────────────────────────────

/// A parsed five-field cron expression (minute hour day-of-month month day-of-week).
#[derive(Clone, Debug, PartialEq)]
pub struct Cron {
    pub min: u64,   // bits 0..59
    pub hour: u32,  // bits 0..23
    pub dom: u32,   // bits 1..31
    pub mon: u16,   // bits 1..12
    pub dow: u8,    // bits 0..6, 0 = Sunday (cron's numbering)
    pub dom_star: bool,
    pub dow_star: bool,
}

fn name_value(s: &str, names: &[&str], base: u32) -> Option<u32> {
    let l = s.to_ascii_lowercase();
    names.iter().position(|n| *n == l).map(|i| i as u32 + base)
}

const CRON_MON: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const CRON_DOW: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn field_bits(f: &str, lo: u32, hi: u32, names: &[&str], name_base: u32, what: &str) -> Result<(u64, bool), String> {
    let mut bits = 0u64;
    let star = f == "*" || f == "?";
    for part in f.split(',') {
        if part.is_empty() {
            return Err(format!("empty list item in the {what} field"));
        }
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let st: u32 = s.parse().map_err(|_| format!("bad step `/{s}` in the {what} field"))?;
                if st == 0 {
                    return Err(format!("a step of 0 in the {what} field"));
                }
                (r, st)
            },
            None => (part, 1),
        };
        let val = |t: &str| -> Result<u32, String> {
            if let Ok(v) = t.parse::<u32>() {
                return Ok(v);
            }
            name_value(t, names, name_base).ok_or_else(|| format!("`{t}` is not a valid {what}"))
        };
        let (a, b) = if range == "*" || range == "?" {
            (lo, hi)
        } else if let Some((x, y)) = range.split_once('-') {
            (val(x)?, val(y)?)
        } else {
            let v = val(range)?;
            // `5/15` = from 5 to the end, every 15
            if part.contains('/') {
                (v, hi)
            } else {
                (v, v)
            }
        };
        if a < lo || b > hi || a > b {
            return Err(format!("{what} `{range}` is outside {lo}-{hi}"));
        }
        let mut v = a;
        while v <= b {
            bits |= 1u64 << v;
            v += step;
        }
    }
    // cron allows 7 for Sunday
    if what == "day-of-week" && bits & (1 << 7) != 0 {
        bits = (bits & !(1 << 7)) | 1;
    }
    Ok((bits, star))
}

impl Cron {
    pub fn parse(s: &str) -> Result<Cron, String> {
        let s = s.trim();
        let s = match s {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            o => o,
        };
        let f: Vec<&str> = s.split_whitespace().collect();
        if f.len() != 5 {
            return Err(format!("a cron line has 5 fields (minute hour day month weekday), this one has {}", f.len()));
        }
        let (min, _) = field_bits(f[0], 0, 59, &[], 0, "minute")?;
        let (hour, _) = field_bits(f[1], 0, 23, &[], 0, "hour")?;
        let (dom, dom_star) = field_bits(f[2], 1, 31, &[], 0, "day-of-month")?;
        let (mon, _) = field_bits(f[3], 1, 12, &CRON_MON, 1, "month")?;
        let (dow, dow_star) = field_bits(f[4], 0, 7, &CRON_DOW, 0, "day-of-week")?;
        Ok(Cron { min, hour: hour as u32, dom: dom as u32, mon: mon as u16, dow: dow as u8 & 0x7F, dom_star, dow_star })
    }

    fn day_ok(&self, days: i64) -> bool {
        let (_, m, d) = civil_from_days(days);
        if self.mon & (1 << m) == 0 {
            return false;
        }
        let cron_dow = (weekday(days) + 1) % 7; // 0 = Sunday
        let dom_ok = self.dom & (1 << d) != 0;
        let dow_ok = self.dow & (1 << cron_dow) != 0;
        // vixie cron: both restricted = either matches
        match (self.dom_star, self.dow_star) {
            (true, true) => true,
            (true, false) => dow_ok,
            (false, true) => dom_ok,
            (false, false) => dom_ok || dow_ok,
        }
    }

    /// The first matching local minute strictly after `lm` (within ~5 years).
    pub fn next_after(&self, lm: i64) -> Option<i64> {
        let start = lm + 1;
        let mut day = start.div_euclid(MIN_PER_DAY);
        let mut first_min = start.rem_euclid(MIN_PER_DAY);
        for _ in 0..(366 * 5) {
            if self.day_ok(day) {
                let h0 = (first_min / 60) as u32;
                for h in h0..24 {
                    if self.hour & (1 << h) == 0 {
                        continue;
                    }
                    let m0 = if h == h0 { (first_min % 60) as u32 } else { 0 };
                    for m in m0..60 {
                        if self.min & (1u64 << m) != 0 {
                            return Some(day * MIN_PER_DAY + h as i64 * 60 + m as i64);
                        }
                    }
                }
            }
            day += 1;
            first_min = 0;
        }
        None
    }
}

// ── triggers ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum Trigger {
    /// Only manual / at boot / at login.
    None,
    Cron(Cron, String),
    /// Every `secs` (a whole number of minutes, at most a day), anchored at the
    /// window's start (midnight without a window), inside `[start, end]`
    /// minutes of the day, on the weekdays of `days` (bit 0 = Monday).
    Every { secs: u64, start: u32, end: u32, days: u8 },
    /// A single run at this local minute.
    Once(i64),
}

impl Trigger {
    pub fn next_after(&self, lm: i64) -> Option<i64> {
        match self {
            Trigger::None => None,
            Trigger::Cron(c, _) => c.next_after(lm),
            Trigger::Once(t) => (*t > lm).then_some(*t),
            Trigger::Every { secs, start, end, days } => {
                let step = (*secs / 60).max(1) as i64;
                let mut day = lm.div_euclid(MIN_PER_DAY);
                for _ in 0..15 {
                    if days & (1 << weekday(day)) != 0 {
                        let base = day * MIN_PER_DAY;
                        let mut t = base + *start as i64;
                        let last = base + *end as i64;
                        if t <= lm {
                            // jump close to lm, then step
                            let k = (lm - t) / step;
                            t += k * step;
                            while t <= lm {
                                t += step;
                            }
                        }
                        if t <= last {
                            return Some(t);
                        }
                    }
                    day += 1;
                }
                None
            },
        }
    }

    /// The next `n` run times after `lm`.
    pub fn next_n(&self, lm: i64, n: usize) -> Vec<i64> {
        let mut out = Vec::new();
        let mut t = lm;
        while out.len() < n {
            match self.next_after(t) {
                Some(x) => {
                    out.push(x);
                    t = x;
                },
                None => break,
            }
        }
        out
    }

    /// The equivalent cron line, when there is an exact one ("" if not).
    pub fn cron_line(&self) -> String {
        match self {
            Trigger::Cron(_, s) => s.clone(),
            Trigger::Every { secs, start, end, days } => {
                let dow = dow_field(*days);
                let m = secs / 60;
                if m == 0 {
                    return String::new();
                }
                if *start == *end {
                    return format!("{} {} * * {dow}", start % 60, start / 60);
                }
                let full_day = *start == 0 && *end >= 1439;
                if m < 60 && 60 % m == 0 {
                    // minutes step within whole hours
                    if start % 60 == 0 && (full_day || end % 60 == 60 - m as u32) {
                        let h = if full_day { String::from("*") } else { format!("{}-{}", start / 60, end / 60) };
                        return format!("*/{m} {h} * * {dow}");
                    }
                    return String::new();
                }
                if m % 60 == 0 && m <= 1440 {
                    let hs = m / 60;
                    let h0 = start / 60;
                    let h1 = if full_day { 23 } else { end / 60 };
                    if hs == 24 {
                        return format!("{} {} * * {dow}", start % 60, h0);
                    }
                    if end % 60 >= start % 60 || full_day {
                        let hours = if hs == 1 { format!("{h0}-{h1}") } else { format!("{h0}-{h1}/{hs}") };
                        let hours = if full_day && hs == 1 { String::from("*") } else if full_day && h0 == 0 { format!("*/{hs}") } else { hours };
                        return format!("{} {hours} * * {dow}", start % 60);
                    }
                }
                String::new()
            },
            Trigger::Once(t) => {
                let (_, mo, d) = civil_from_days(t.div_euclid(MIN_PER_DAY));
                let mm = t.rem_euclid(MIN_PER_DAY);
                format!("{} {} {d} {mo} *  (once)", mm % 60, mm / 60)
            },
            Trigger::None => String::new(),
        }
    }

    /// A short description for the editor: "every 2h, 08:00 - 22:00".
    pub fn describe(&self) -> String {
        match self {
            Trigger::None => String::from("no timer (at boot / at login / Run now only)"),
            Trigger::Cron(_, s) => format!("cron {s}"),
            Trigger::Once(t) => format!("once at {}", fmt_lm(*t)),
            Trigger::Every { secs, start, end, days } => {
                if *secs == 86400 || start == end {
                    let at = fmt_hm(*start as i64);
                    return match days & 0x7F {
                        0x7F => format!("daily at {at}"),
                        0x1F => format!("weekdays at {at}"),
                        0x60 => format!("weekends at {at}"),
                        d if d.count_ones() == 1 => format!("weekly on {} at {at}", WD_SHORT[d.trailing_zeros() as usize]),
                        d => format!("{} at {at}", days_words(d)),
                    };
                }
                let mut s = format!("every {}", fmt_duration(*secs));
                if start != end && !(*start == 0 && *end >= 1439) {
                    s.push_str(&format!(", {} - {}", fmt_hm(*start as i64), fmt_hm(*end as i64)));
                }
                if *days != 0x7F {
                    s.push_str(", on ");
                    s.push_str(&days_words(*days));
                }
                s
            },
        }
    }
}

/// `mon,wed,fri` style cron day-of-week field for a Monday-first mask.
fn dow_field(days: u8) -> String {
    if days & 0x7F == 0x7F {
        return String::from("*");
    }
    if days == 0x1F {
        return String::from("1-5");
    }
    let mut v = Vec::new();
    for i in 0..7u32 {
        if days & (1 << i) != 0 {
            v.push(format!("{}", (i + 1) % 7));
        }
    }
    v.join(",")
}

pub fn days_words(days: u8) -> String {
    if days & 0x7F == 0x7F {
        return String::from("every day");
    }
    if days == 0x1F {
        return String::from("weekdays");
    }
    if days == 0x60 {
        return String::from("weekends");
    }
    let mut v = Vec::new();
    for i in 0..7 {
        if days & (1 << i) != 0 {
            v.push(WD_SHORT[i]);
        }
    }
    v.join(", ")
}

/// `mon,wed,fri` / `weekdays` / `weekends` / `daily` -> Monday-first mask.
pub fn parse_days(s: &str) -> Option<u8> {
    let l = s.trim().to_ascii_lowercase();
    match l.as_str() {
        "weekdays" | "workdays" => return Some(0x1F),
        "weekends" | "weekend" => return Some(0x60),
        "daily" | "every day" | "everyday" | "*" | "all" => return Some(0x7F),
        _ => {},
    }
    let mut mask = 0u8;
    for part in l.split([',', ' ']) {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if let Some((a, b)) = p.split_once('-') {
            let (a, b) = (weekday_named(a)?, weekday_named(b)?);
            let mut i = a;
            loop {
                mask |= 1 << i;
                if i == b {
                    break;
                }
                i = (i + 1) % 7;
            }
        } else {
            mask |= 1 << weekday_named(p)?;
        }
    }
    (mask != 0).then_some(mask)
}
