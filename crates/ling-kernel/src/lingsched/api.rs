//! The builtins LingCal (and the desktop) call: `os_cal_*` for the calendar,
//! `os_sched_*` for automations, `os_secret_*` for the keyring. Registered
//! with `lingrt::register` at boot. Records come back as lists of lists.
//! Every call acts for the signed-in user (keyring open) -- never for a name
//! the caller passes.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::lingrt::{boolean, list_from, mk_str, num, to_f64, to_string, UNIT};

use super::cron::{self, MIN_PER_DAY};
use super::{cal, lcron, run, secrets};

fn s_arg(args: &[u64], i: usize) -> String {
    args.get(i).map(|&v| to_string(v)).unwrap_or_default()
}
fn n_arg(args: &[u64], i: usize, d: f64) -> f64 {
    args.get(i).map(|&v| to_f64(v)).unwrap_or(d)
}
fn b_arg(args: &[u64], i: usize) -> bool {
    args.get(i).map(|&v| crate::lingrt::truthy(v)).unwrap_or(false)
}
fn strs(v: &[String]) -> u64 {
    list_from(v.iter().map(|s| mk_str(s)).collect())
}
fn res(r: Result<(), String>) -> u64 {
    match r {
        Ok(()) => mk_str(""),
        Err(e) => mk_str(&e),
    }
}

/// The signed-in user (keyring open), else the lingfs session user, else None.
fn me() -> Option<String> {
    crate::keyring::user().or_else(|| {
        let u = crate::fs::lingfs::current_user();
        (!u.is_empty()).then(|| String::from(u))
    })
}

fn secret_me() -> Result<String, String> {
    crate::keyring::user().ok_or_else(|| String::from("the keyring is locked: sign in first"))
}

pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    let n = core::str::from_utf8(name).ok()?;
    if !(n.starts_with("os_cal_") || n.starts_with("os_sched_") || n.starts_with("os_secret_")) {
        return None;
    }
    Some(match n {
        // ── time ──
        "os_cal_now" => num(super::now_lm() as f64),
        "os_cal_tz" => num(super::tz_s() as f64 / 3600.0),
        "os_cal_set_tz" => {
            super::set_tz_hours(n_arg(args, 0, 0.0));
            UNIT
        },
        "os_cal_version" => num(super::version() as f64),
        // (y, m, d, weekday 0 = Mon) of a day number
        "os_cal_civil" => {
            let d = n_arg(args, 0, 0.0) as i64;
            let (y, m, dd) = cron::civil_from_days(d);
            list_from(alloc::vec![num(y as f64), num(m as f64), num(dd as f64), num(cron::weekday(d) as f64), num(cron::days_in_month(y, m) as f64)])
        },
        "os_cal_day" => num(cron::days_from_civil(n_arg(args, 0, 2026.0) as i64, n_arg(args, 1, 1.0).clamp(1.0, 12.0) as u32, n_arg(args, 2, 1.0).max(1.0) as u32) as f64),
        // os_cal_fmt(lm, kind): date / hm / rel / long / week / month / dur
        "os_cal_fmt" => {
            let lm = n_arg(args, 0, 0.0) as i64;
            let kind = s_arg(args, 1);
            let d = lm.div_euclid(MIN_PER_DAY);
            let (y, m, dd) = cron::civil_from_days(d);
            const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
            mk_str(&match kind.as_str() {
                "date" => cron::fmt_date(d),
                "hm" => cron::fmt_hm(lm),
                "rel" => cron::fmt_rel(lm, super::now_lm()),
                "long" => format!("{}, {} {dd} {y}", cron::WD_SHORT[cron::weekday(d) as usize], cron::MON_SHORT[(m - 1) as usize]),
                "month" => format!("{} {y}", MONTHS[(m - 1) as usize]),
                "mon" => String::from(cron::MON_SHORT[(m - 1) as usize]),
                "week" => {
                    let e = d + 6;
                    let (y2, m2, d2) = cron::civil_from_days(e);
                    if m2 == m {
                        format!("{} {dd} - {d2}, {y}", cron::MON_SHORT[(m - 1) as usize])
                    } else if y2 == y {
                        format!("{} {dd} - {} {d2}, {y}", cron::MON_SHORT[(m - 1) as usize], cron::MON_SHORT[(m2 - 1) as usize])
                    } else {
                        format!("{} {dd}, {y} - {} {d2}, {y2}", cron::MON_SHORT[(m - 1) as usize], cron::MON_SHORT[(m2 - 1) as usize])
                    }
                },
                "dur" => super::fmt_minutes(lm),
                _ => cron::fmt_lm(lm),
            })
        },

        // ── calendar ──
        "os_cal_load" => match me() {
            Some(u) => mk_str(&super::load_calendar(&u)),
            None => mk_str("nobody is signed in"),
        },
        "os_cal_user" => mk_str(&me().unwrap_or_default()),
        "os_cal_cals" => match super::book() {
            Some(b) => list_from(b.cals.iter().map(|c| list_from(alloc::vec![mk_str(&c.name), num(c.color as f64), boolean(c.hidden)])).collect()),
            None => list_from(Vec::new()),
        },
        "os_cal_colors" => list_from(cal::COLORS.iter().map(|(n, c)| list_from(alloc::vec![mk_str(n), num(c[0] as f64), num(c[1] as f64), num(c[2] as f64)])).collect()),
        // os_cal_cal_set(name, color, hidden [, old name]): add / change / rename a calendar
        "os_cal_cal_set" => {
            let name = s_arg(args, 0);
            let old = s_arg(args, 3);
            let Some(b) = super::book() else { return Some(mk_str("nobody is signed in")) };
            if name.trim().is_empty() || name.len() > 40 {
                return Some(mk_str("a calendar name is 1-40 characters"));
            }
            let color = (n_arg(args, 1, 0.0).max(0.0) as usize) % cal::COLORS.len();
            let hidden = b_arg(args, 2);
            let key = if old.is_empty() { name.clone() } else { old.clone() };
            match b.cals.iter_mut().find(|c| c.name == key) {
                Some(c) => {
                    c.name = name.clone();
                    c.color = color;
                    c.hidden = hidden;
                },
                None => b.cals.push(cal::Calendar { name: name.clone(), color, hidden }),
            }
            if !old.is_empty() && old != name {
                for e in b.events.iter_mut().filter(|e| e.cal == old) {
                    e.cal = name.clone();
                }
            }
            mk_str(if super::save_book() { "" } else { "could not save events.lcal" })
        },
        "os_cal_cal_del" => {
            let name = s_arg(args, 0);
            let Some(b) = super::book() else { return Some(mk_str("nobody is signed in")) };
            if b.events.iter().any(|e| e.cal == name) {
                return Some(mk_str("move or delete its events first"));
            }
            b.cals.retain(|c| c.name != name);
            mk_str(if super::save_book() { "" } else { "could not save events.lcal" })
        },
        // os_cal_range(from_day, ndays [, query]) -> [id, day, start, end, allday, color, title, sub, task, done, recurring, calendar]
        "os_cal_range" => {
            let from = n_arg(args, 0, 0.0) as i64;
            let nd = n_arg(args, 1, 7.0).clamp(1.0, 400.0) as i64;
            let q = s_arg(args, 2).to_ascii_lowercase();
            let Some(b) = super::book() else { return Some(list_from(Vec::new())) };
            let mut out = Vec::new();
            for o in cal::occurrences(b, from, from + nd) {
                let Some(e) = b.get(o.id) else { continue };
                if b.cal_hidden(&e.cal) {
                    continue;
                }
                if !q.is_empty() && !(e.title.to_ascii_lowercase().contains(&q) || e.location.to_ascii_lowercase().contains(&q) || e.note.to_ascii_lowercase().contains(&q)) {
                    continue;
                }
                out.push(occ_rec(b, e, &o));
                if out.len() > 2000 {
                    break;
                }
            }
            list_from(out)
        },
        // the details of one event: [title, cal, allday, start_day, start_min, end_day, end_min, freq, every, days, month_mode, month_day, nth, nth_wd, until(-1), count(0), reminders "m:s,..", location, note, task, done, rule words, of(series id or 0), skips]
        "os_cal_event" => {
            let id = n_arg(args, 0, 0.0) as u32;
            let Some(b) = super::book() else { return Some(list_from(Vec::new())) };
            let Some(e) = b.get(id) else { return Some(list_from(Vec::new())) };
            let (freq, every, days, mm, md, nth, nwd, until, count) = match &e.rule {
                Some(r) => {
                    let f = match r.freq {
                        cal::Freq::Daily => "daily",
                        cal::Freq::Weekly => "weekly",
                        cal::Freq::Monthly => "monthly",
                        cal::Freq::Yearly => "yearly",
                    };
                    let (mm, md, nth, nwd) = match r.month_by {
                        Some(cal::MonthBy::Day(d)) => ("day", d as f64, 0.0, 0.0),
                        Some(cal::MonthBy::Nth(n, w)) => ("nth", 0.0, n as f64, w as f64),
                        None => ("", 0.0, 0.0, 0.0),
                    };
                    (f, r.every as f64, r.days as f64, mm, md, nth, nwd, r.until.map(|u| u as f64).unwrap_or(-1.0), r.count.unwrap_or(0) as f64)
                },
                None => ("", 1.0, 0.0, "", 0.0, 0.0, 0.0, -1.0, 0.0),
            };
            let rem: Vec<String> = e.reminders.iter().map(|(m, s)| format!("{m}:{}", if *s { 1 } else { 0 })).collect();
            list_from(alloc::vec![
                mk_str(&e.title),
                mk_str(&e.cal),
                boolean(e.allday),
                num(e.start_day as f64),
                num(e.start_min as f64),
                num(e.end_day as f64),
                num(e.end_min as f64),
                mk_str(freq),
                num(every),
                num(days),
                mk_str(mm),
                num(md),
                num(nth),
                num(nwd),
                num(until),
                num(count),
                mk_str(&rem.join(",")),
                mk_str(&e.location),
                mk_str(&e.note),
                boolean(e.task),
                boolean(e.done),
                mk_str(&e.rule.as_ref().map(cal::rule_words).unwrap_or_default()),
                num(e.of.map(|o| o.0 as f64).unwrap_or(0.0)),
                num(e.skips.len() as f64),
            ])
        },
        // the event's text block (for "Edit as text")
        "os_cal_block" => {
            let id = n_arg(args, 0, 0.0) as u32;
            match super::book().and_then(|b| b.get(id).map(cal::block)) {
                Some(t) => mk_str(&t),
                None => mk_str(""),
            }
        },
        // os_cal_put(block text [, series id, occurrence day]) -> "ok:<id>" / "err:<msg>"
        // with a series + day: "edit this occurrence only" (the series skips it, the copy keeps `of`)
        "os_cal_put" => {
            let text = s_arg(args, 0);
            let series = n_arg(args, 1, 0.0) as u32;
            let occ_day = n_arg(args, 2, -1.0) as i64;
            let Some(b) = super::book() else { return Some(mk_str("err:nobody is signed in")) };
            match cal::parse_one(&text, b.next_id()) {
                Ok(mut e) => {
                    if series != 0 && occ_day >= 0 {
                        match b.get_mut(series) {
                            Some(s) if s.rule.is_some() => {
                                if !s.skips.contains(&occ_day) {
                                    s.skips.push(occ_day);
                                }
                            },
                            _ => {},
                        }
                        e.id = b.next_id();
                        e.rule = None;
                        e.of = Some((series, occ_day));
                        b.events.push(e.clone());
                    } else if let Some(old) = b.get_mut(e.id) {
                        // keep what the form does not show
                        if e.skips.is_empty() {
                            e.skips = old.skips.clone();
                        }
                        if e.done_on.is_empty() {
                            e.done_on = old.done_on.clone();
                        }
                        if e.of.is_none() {
                            e.of = old.of;
                        }
                        *old = e.clone();
                    } else {
                        b.events.push(e.clone());
                    }
                    if !e.cal.is_empty() && !b.cals.iter().any(|c| c.name == e.cal) {
                        let color = b.cals.len() % cal::COLORS.len();
                        b.cals.push(cal::Calendar { name: e.cal.clone(), color, hidden: false });
                    }
                    if super::save_book() {
                        mk_str(&format!("ok:{}", e.id))
                    } else {
                        mk_str("err:could not save events.lcal")
                    }
                },
                Err(errs) => mk_str(&format!("err:{}", errs.iter().map(|x| x.msg.clone()).collect::<Vec<_>>().join("; "))),
            }
        },
        // os_cal_delete(id, occurrence day or -1 for all)
        "os_cal_delete" => {
            let id = n_arg(args, 0, 0.0) as u32;
            let day = n_arg(args, 1, -1.0) as i64;
            let Some(b) = super::book() else { return Some(boolean(false)) };
            let ok = b.delete(id, if day >= 0 { Some(day) } else { None });
            boolean(ok && super::save_book())
        },
        // os_cal_move(id, occurrence day, new start lm, length min, this one only) -> id holding it
        "os_cal_move" => {
            let id = n_arg(args, 0, 0.0) as u32;
            let day = n_arg(args, 1, 0.0) as i64;
            let start = n_arg(args, 2, 0.0) as i64;
            let len = n_arg(args, 3, 60.0) as i64;
            let one = b_arg(args, 4);
            let Some(b) = super::book() else { return Some(num(0.0)) };
            match b.move_to(id, day, start, len, one) {
                Some(nid) => {
                    super::save_book();
                    num(nid as f64)
                },
                None => num(0.0),
            }
        },
        // tick a task (an occurrence of a repeating one)
        "os_cal_task_toggle" => {
            let id = n_arg(args, 0, 0.0) as u32;
            let day = n_arg(args, 1, 0.0) as i64;
            let Some(b) = super::book() else { return Some(boolean(false)) };
            let Some(e) = b.get_mut(id) else { return Some(boolean(false)) };
            if e.rule.is_some() {
                if let Some(i) = e.done_on.iter().position(|d| *d == day) {
                    e.done_on.remove(i);
                } else {
                    e.done_on.push(day);
                }
            } else {
                e.done = !e.done;
            }
            boolean(super::save_book())
        },
        // events matching a query, with their next occurrence: [id, next lm, title, cal, color]
        "os_cal_search" => {
            let q = s_arg(args, 0).to_ascii_lowercase();
            let Some(b) = super::book() else { return Some(list_from(Vec::new())) };
            let now = super::now_lm();
            let today = now.div_euclid(MIN_PER_DAY);
            let mut out = Vec::new();
            for e in &b.events {
                if !q.is_empty() && !(e.title.to_ascii_lowercase().contains(&q) || e.location.to_ascii_lowercase().contains(&q) || e.note.to_ascii_lowercase().contains(&q) || e.cal.to_ascii_lowercase().contains(&q)) {
                    continue;
                }
                let next = cal::occurrences(&cal::Book { preamble: String::new(), cals: Vec::new(), events: alloc::vec![e.clone()] }, today - 1, today + 400).into_iter().find(|o| o.end >= now).map(|o| o.start).unwrap_or(-1);
                out.push(list_from(alloc::vec![num(e.id as f64), num(next as f64), mk_str(&e.title), mk_str(&e.cal), num(b.cal_color(&e.cal) as f64)]));
                if out.len() >= 50 {
                    break;
                }
            }
            list_from(out)
        },
        // days of a month that have something (the mini month's marks)
        "os_cal_marks" => {
            let from = n_arg(args, 0, 0.0) as i64;
            let nd = n_arg(args, 1, 31.0).clamp(1.0, 62.0) as i64;
            let Some(b) = super::book() else { return Some(list_from(Vec::new())) };
            let mut days: Vec<i64> = Vec::new();
            for o in cal::occurrences(b, from, from + nd) {
                if let Some(e) = b.get(o.id) {
                    if b.cal_hidden(&e.cal) {
                        continue;
                    }
                }
                let d = o.start.div_euclid(MIN_PER_DAY);
                if !days.contains(&d) {
                    days.push(d);
                }
            }
            list_from(days.iter().map(|d| num(*d as f64)).collect())
        },
        // the next timed event from now: [lm, title, id] or []
        "os_cal_next" => {
            let Some(b) = super::book() else { return Some(list_from(Vec::new())) };
            let now = super::now_lm();
            let today = now.div_euclid(MIN_PER_DAY);
            match cal::occurrences(b, today, today + 8).into_iter().find(|o| o.start >= now && b.get(o.id).map(|e| !e.allday && !b.cal_hidden(&e.cal)).unwrap_or(false)) {
                Some(o) => list_from(alloc::vec![num(o.start as f64), mk_str(&b.get(o.id).map(|e| e.title.clone()).unwrap_or_default()), num(o.id as f64)]),
                None => list_from(Vec::new()),
            }
        },
        // a reminder's Open: [event id, occurrence lm] once, or []
        "os_cal_focus" => match super::notify::take_focus() {
            Some((id, occ)) => list_from(alloc::vec![num(id as f64), num(occ as f64)]),
            None => list_from(Vec::new()),
        },
        // a test toast for an event (the inspector's bell button)
        "os_cal_test_remind" => {
            let id = n_arg(args, 0, 0.0) as u32;
            let occ = n_arg(args, 1, 0.0) as i64;
            let Some(u) = crate::keyring::user() else { return Some(boolean(false)) };
            let Some(b) = super::book() else { return Some(boolean(false)) };
            let Some(e) = b.get(id) else { return Some(boolean(false)) };
            let col = cal::COLORS[b.cal_color(&e.cal) % cal::COLORS.len()].1;
            let now = super::now_lm();
            let detail = format!("{} - {}{}{}", cron::fmt_rel(occ, now), cron::fmt_hm(occ + e.duration()), if e.cal.is_empty() { String::new() } else { format!(" · {}", e.cal) }, if e.note.is_empty() { String::new() } else { format!(" · note: {}", e.note) });
            let right = if occ > now { format!("in {}", super::fmt_minutes(occ - now)) } else { String::from("now") };
            super::notify::reminder(&u, &e.title, &detail, &right, id, occ, Some([col[1], col[2]]));
            super::chime("chime");
            boolean(true)
        },
        "os_cal_path" => mk_str(&me().map(|u| super::calendar_path(&u)).unwrap_or_default()),

        // ── automations ──
        "os_sched_dir" => mk_str(&me().map(|u| super::automations_dir(&u)).unwrap_or_default()),
        // [path, name, armed, enabled, describe, cron, next lm, last run lm, last status, error, needs session]
        "os_sched_jobs" => {
            let Some(u) = me() else { return Some(list_from(Vec::new())) };
            let dir = super::automations_dir(&u);
            let mut out = Vec::new();
            let mut seen: Vec<String> = Vec::new();
            for (name, is_dir, _) in crate::vfs::list(&dir) {
                if is_dir || !name.ends_with(".lcron") {
                    continue;
                }
                let p = crate::vfs::join(&dir, &name);
                seen.push(p.clone());
                out.push(job_rec(&u, &p));
            }
            for a in super::jobs().iter().filter(|a| a.owner == u) {
                if !seen.contains(&a.path) {
                    out.push(job_rec(&u, &a.path.clone()));
                }
            }
            list_from(out)
        },
        // problems in a text: [[line, message, secret], ...]
        "os_sched_check" => {
            let t = s_arg(args, 0);
            match lcron::parse(&t) {
                Ok(_) => list_from(Vec::new()),
                Err(e) => list_from(e.iter().map(|x| list_from(alloc::vec![num(x.line as f64), mk_str(&x.msg), boolean(x.secret)])).collect()),
            }
        },
        // [name, timer words, cron, missed, at boot, at login, needs session, secrets, next1..next6]
        "os_sched_info" => {
            let t = s_arg(args, 0);
            match lcron::parse(&t) {
                Ok(j) => {
                    let now = super::now_lm();
                    let mut v = alloc::vec![
                        mk_str(&j.name),
                        mk_str(&j.trigger.describe()),
                        mk_str(&j.trigger.cron_line()),
                        mk_str(if j.missed == lcron::Missed::CatchUp { "catch up once (PC was off)" } else { "skip (PC was off)" }),
                        boolean(j.at_boot),
                        boolean(j.at_login),
                        boolean(j.needs_session()),
                        mk_str(&j.secrets().join(", ")),
                    ];
                    for t in j.trigger.next_n(now, 8) {
                        v.push(mk_str(&cron::fmt_rel(t, now)));
                    }
                    list_from(v)
                },
                Err(_) => list_from(Vec::new()),
            }
        },
        // the Blocks view: [[kind, name, action, args text, condition, line, color key], ...]
        "os_sched_blocks" => {
            let t = s_arg(args, 0);
            match lcron::parse(&t) {
                Ok(j) => {
                    let mut out = Vec::new();
                    for s in &j.steps {
                        out.push(block_rec("step", &s.name, &s.call, s.when.as_ref().map(|c| c.show()).unwrap_or_default(), s.line));
                    }
                    for c in &j.on_fail {
                        out.push(block_rec("on_fail", "", c, String::new(), c.line));
                    }
                    for c in &j.on_ok {
                        out.push(block_rec("on_ok", "", c, String::new(), c.line));
                    }
                    list_from(out)
                },
                Err(_) => list_from(Vec::new()),
            }
        },
        // os_sched_save(path, text): "" or why not (a file with a literal secret is never written)
        "os_sched_save" => {
            let Some(u) = me() else { return Some(mk_str("nobody is signed in")) };
            let p = crate::vfs::norm(&s_arg(args, 0));
            let t = s_arg(args, 1);
            if !p.starts_with(&format!("home/{u}/")) || !p.ends_with(".lcron") {
                return Some(mk_str("automations are .lcron files in your home"));
            }
            if let Err(e) = lcron::parse(&t) {
                if let Some(x) = e.iter().find(|x| x.secret) {
                    return Some(mk_str(&format!("NOT saved -- line {}: {}", x.line, x.msg)));
                }
            }
            super::ensure_dir(&crate::vfs::parent(&p));
            if !crate::vfs::write(&p, t.as_bytes()) {
                return Some(mk_str("could not write the file"));
            }
            // an armed job picks up the new text
            if let Some(a) = super::armed(&u, &p) {
                super::reparse(a);
                super::bump_version();
            }
            mk_str("")
        },
        "os_sched_arm" => {
            let Some(u) = me() else { return Some(mk_str("nobody is signed in")) };
            res(super::arm(&u, &s_arg(args, 0), b_arg(args, 1)))
        },
        // os_sched_run(path [, unsaved text])
        "os_sched_run" => {
            let Some(u) = me() else { return Some(mk_str("nobody is signed in")) };
            let t = s_arg(args, 1);
            res(super::run_now(&u, &crate::vfs::norm(&s_arg(args, 0)), if t.is_empty() { None } else { Some(t) }))
        },
        "os_sched_dry" => {
            let Some(u) = me() else { return Some(list_from(Vec::new())) };
            strs(&run::dry(&u, &s_arg(args, 0)))
        },
        // os_sched_log(file or "", n): [[unix, file, job, status, ms, message], ...] newest first
        "os_sched_log" => {
            let Some(u) = me() else { return Some(list_from(Vec::new())) };
            let n = n_arg(args, 1, 50.0).clamp(1.0, 400.0) as usize;
            let rows = run::log_read(&u, &s_arg(args, 0), n);
            list_from(
                rows.iter()
                    .map(|f| {
                        let unix: i64 = f[0].parse().unwrap_or(0);
                        list_from(alloc::vec![num(super::unix_to_lm(unix) as f64), mk_str(&f[1]), mk_str(&f[2]), mk_str(&f[3]), num(f[4].parse::<f64>().unwrap_or(0.0)), mk_str(&f[5])])
                    })
                    .collect(),
            )
        },
        // [running, armed for me, runs today, failed today, enabled, busy]
        "os_sched_status" => {
            let u = me().unwrap_or_default();
            let armed = super::jobs().iter().filter(|a| a.owner == u && a.enabled).count();
            let today = super::now_lm().div_euclid(MIN_PER_DAY);
            // the log is read only when something changed (a run logs -> the version moves), never per frame
            static mut STATUS_CACHE: Option<(u64, String, i64, usize, usize)> = None;
            let cached = unsafe { (*(&raw const STATUS_CACHE)).clone() }.filter(|c| c.0 == super::version() && c.1 == u && c.2 == today);
            let (runs, failed) = match cached {
                Some(c) => (c.3, c.4),
                None => {
                    let rows = run::log_read(&u, "", 400);
                    let mut runs = 0;
                    let mut failed = 0;
                    for f in &rows {
                        let lm = super::unix_to_lm(f[0].parse().unwrap_or(0));
                        if lm.div_euclid(MIN_PER_DAY) == today && matches!(f[3].as_str(), "ok" | "failed" | "retried") {
                            runs += 1;
                            if f[3] == "failed" {
                                failed += 1;
                            }
                        }
                    }
                    unsafe { *(&raw mut STATUS_CACHE) = Some((super::version(), u.clone(), today, runs, failed)) };
                    (runs, failed)
                },
            };
            list_from(alloc::vec![boolean(super::running()), num(armed as f64), num(runs as f64), num(failed as f64), boolean(super::enabled()), boolean(run::busy())])
        },
        // automation run times in view: [[lm, label, path], ...]
        "os_sched_ticks" => {
            let Some(u) = me() else { return Some(list_from(Vec::new())) };
            let from = n_arg(args, 0, 0.0) as i64;
            let nd = n_arg(args, 1, 7.0).clamp(1.0, 42.0) as i64;
            let v = super::ticks(&u, from * MIN_PER_DAY, (from + nd) * MIN_PER_DAY);
            list_from(v.iter().map(|(t, l, p)| list_from(alloc::vec![num(*t as f64), mk_str(l), mk_str(p)])).collect())
        },
        // the Commands view: one shared shell command (the same set `shell(...)` runs), as the signed-in user
        "os_sched_shell" => {
            let Some(u) = me() else { return Some(mk_str("nobody is signed in")) };
            let cmd = s_arg(args, 0);
            let (name, arg) = cmd.trim().split_once(' ').unwrap_or((cmd.trim(), ""));
            let mut cwd = format!("home/{u}");
            mk_str(&crate::drivers::sshcmds::run(name, arg.trim(), &u, &mut cwd).unwrap_or_else(|| format!("{name}: not a LingOS shell command\n")))
        },
        "os_sched_actions" => list_from(lcron::SPECS.iter().map(|s| list_from(alloc::vec![mk_str(s.name), mk_str(s.help)])).collect()),
        "os_sched_enabled" => boolean(super::enabled()),
        // os_sched_ssh_test(target, keyring name) + os_sched_test_result()
        "os_sched_ssh_test" => match secret_me() {
            Ok(u) => {
                run::test_ssh(&u, &s_arg(args, 0), &s_arg(args, 1));
                mk_str("")
            },
            Err(e) => mk_str(&e),
        },
        "os_sched_test_result" => mk_str(&run::test_result()),

        // ── the keyring ──
        // [[name, kind, public key line], ...] or [["!", why]] when locked
        "os_secret_list" => match secret_me().and_then(|u| secrets::list(&u)) {
            Ok(l) => list_from(l.iter().map(|(n, k, i)| list_from(alloc::vec![mk_str(n), mk_str(k.word()), mk_str(i)])).collect()),
            Err(e) => list_from(alloc::vec![list_from(alloc::vec![mk_str("!"), mk_str(&e)])]),
        },
        "os_secret_add" => {
            let kind = secrets::Kind::from_word(&s_arg(args, 1)).unwrap_or(secrets::Kind::Password);
            let mut v = s_arg(args, 2).into_bytes();
            let r = secret_me().and_then(|u| secrets::add(&u, s_arg(args, 0).trim(), kind, &v));
            secrets::wipe(&mut v);
            res(r)
        },
        "os_secret_rename" => res(secret_me().and_then(|u| secrets::rename(&u, &s_arg(args, 0), s_arg(args, 1).trim()))),
        "os_secret_delete" => res(secret_me().and_then(|u| secrets::delete(&u, &s_arg(args, 0)))),
        // "ok:<public key line>" / "err:<why>"
        "os_secret_genkey" => match secret_me().and_then(|u| secrets::generate_ssh_key(&u, s_arg(args, 0).trim()).map(|l| (u, l))) {
            Ok((u, l)) => {
                // the PUBLIC half also as a file, to copy to a server or a USB stick
                let dir = format!("home/{u}/Projects/LingCal/keys");
                super::ensure_dir(&dir);
                let _ = crate::vfs::write(&format!("{dir}/{}.pub", s_arg(args, 0).trim().replace(' ', "_")), format!("{l}\n").as_bytes());
                mk_str(&format!("ok:{l}"))
            },
            Err(e) => mk_str(&format!("err:{e}")),
        },
        // [[host:port, fingerprint, pinned, when lm], ...] (pending ones: pinned false)
        "os_secret_hosts" => {
            let Ok(u) = secret_me() else { return Some(list_from(Vec::new())) };
            let mut out = Vec::new();
            for (hp, fp) in secrets::pending(&u) {
                out.push(list_from(alloc::vec![mk_str(&hp), mk_str(&fp), boolean(false), num(0.0)]));
            }
            for (hp, fp, when) in secrets::hosts(&u) {
                out.push(list_from(alloc::vec![mk_str(&hp), mk_str(&fp), boolean(true), num(super::unix_to_lm(when as i64) as f64)]));
            }
            list_from(out)
        },
        "os_secret_trust" => res(secret_me().and_then(|u| secrets::confirm(&u, &s_arg(args, 0), &s_arg(args, 1)))),
        "os_secret_reject" => {
            if let Ok(u) = secret_me() {
                secrets::reject(&u, &s_arg(args, 0));
            }
            UNIT
        },
        "os_secret_forget" => boolean(secret_me().map(|u| secrets::forget(&u, &s_arg(args, 0))).unwrap_or(false)),
        "os_secret_status" => mk_str(&crate::keyring::status()),
        _ => return None,
    })
}

fn occ_rec(b: &cal::Book, e: &cal::Event, o: &cal::Occ) -> u64 {
    let sub = if e.task {
        if e.allday {
            String::from("task")
        } else {
            format!("task · due {}", cron::fmt_hm(e.start_min as i64))
        }
    } else if let Some(r) = &e.rule {
        let w = cal::rule_words(r);
        if !e.location.is_empty() {
            format!("{} · {}", e.location, w.to_ascii_lowercase())
        } else {
            format!("repeats {}", w.to_ascii_lowercase().trim_start_matches("weekly ").trim_start_matches("daily").trim())
        }
    } else if !e.location.is_empty() {
        e.location.clone()
    } else if !e.note.is_empty() {
        e.note.clone()
    } else {
        String::new()
    };
    let sub = if sub == "repeats" { String::from("repeats daily") } else { sub };
    list_from(alloc::vec![
        num(e.id as f64),
        num(o.day as f64),
        num(o.start as f64),
        num(o.end as f64),
        boolean(e.allday),
        num(b.cal_color(&e.cal) as f64),
        mk_str(&e.title),
        mk_str(&sub),
        boolean(e.task),
        boolean(e.is_done_on(o.day)),
        boolean(e.rule.is_some()),
        mk_str(&e.cal),
    ])
}

fn job_rec(user: &str, p: &str) -> u64 {
    let text = crate::vfs::read(p).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let parsed = lcron::parse(&text);
    let a = super::armed(user, p);
    let now = super::now_lm();
    let (name, desc, cron_l, err, sess) = match &parsed {
        Ok(j) => (j.name.clone(), j.trigger.describe(), j.trigger.cron_line(), String::new(), j.needs_session()),
        Err(e) => (crate::vfs::basename(p), String::from("has errors"), String::new(), e.first().map(|x| format!("line {}: {}", x.line, x.msg)).unwrap_or_default(), false),
    };
    let next = match (&parsed, &a) {
        (Ok(j), Some(a)) if a.enabled => j.trigger.next_after(now).map(|t| t as f64).unwrap_or(-1.0),
        _ => -1.0,
    };
    let (armed, enabled, last, status) = match a {
        Some(a) => (true, a.enabled, if a.last_run > 0 { super::unix_to_lm(a.last_run as i64) as f64 } else { -1.0 }, a.last_status.clone()),
        None => (false, false, -1.0, String::new()),
    };
    list_from(alloc::vec![
        mk_str(p),
        mk_str(&name),
        boolean(armed),
        boolean(enabled),
        mk_str(&desc),
        mk_str(&cron_l),
        num(next),
        num(last),
        mk_str(&status),
        mk_str(&err),
        boolean(sess),
    ])
}

fn block_rec(kind: &str, name: &str, c: &lcron::Call, cond: String, line: usize) -> u64 {
    let args = lcron::show_call(c);
    let args = args.split_once('(').map(|(_, r)| r.trim_end_matches(')')).unwrap_or("");
    let color = match c.action.as_str() {
        "screenshot" | "screenrec" => "purple",
        "sftp.put" | "ssh.run" => "teal",
        "shell" | "app.open" => "navy",
        "notify" | "remind" | "play" | "log" => "green",
        "copy" | "move" | "delete" | "write" | "http.get" => "orange",
        _ => "olive",
    };
    list_from(alloc::vec![mk_str(kind), mk_str(name), mk_str(&c.action), mk_str(args), mk_str(&cond), num(line as f64), mk_str(color)])
}
