//! LingScheduler + LingCal's kernel side.
//!
//! * **Automations** (`.lcron`, see `lcron.rs`) live in each user's
//!   `Projects/LingCal/automations/`. "Save and arm" records a file in the
//!   user's armed list (`lingsched/<user>-jobs`, a lingfs account store apps
//!   cannot reach); the scheduler keeps every armed job's next run in memory
//!   and `tick` (once a second, from `startup::tick`, so on the desktop's main
//!   flow but cheap: no storage, no parsing) queues the due ones. A
//!   background scheduler task (`run.rs`) runs the queue -- network waits
//!   yield, so the desktop keeps drawing -- and writes the run log
//!   (`lingsched/<user>-log`).
//! * **Who a job runs as**: its owner (the home it lives in). A job that
//!   needs the owner's session -- a keyring secret, SSH/SFTP, the screen,
//!   sound, notices, apps -- runs only while that owner is signed in with the
//!   keyring open; before sign-in, while locked, or with someone else signed
//!   in it waits (`missed catchup` runs it once on sign-in, `missed skip`
//!   drops it). Jobs made only of file / shell / http / log steps also run at
//!   boot before anyone signs in, confined to the owner's home and media/.
//! * **Missed runs** (PC off): a job's last run is persisted; on boot an
//!   occurrence between it and now counts as missed: `skip` logs it,
//!   `catchup` runs once.
//! * **Rate limit**: at least 60 s between two timer runs of a job and at most
//!   `limit N per hour` (default 60); one job runs at a time.
//! * **Reminders**: the signed-in user's `events.lcal` is read at sign-in (and
//!   when LingCal changes it); the next day's reminders are kept in memory and
//!   shown as toasts (`notify.rs`) whether or not LingCal is open.
//! * The service is "LingScheduler" in Settings > Startup (on by default;
//!   `lingsched/config` keeps the switch and the last known time zone).

pub mod cal;
pub mod cron;
pub mod lcron;
pub mod notify;
pub mod secrets;
pub mod sshc;
mod api;
mod run;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use cron::MIN_PER_DAY;

pub const SERVICE: &str = "lingsched";
const CONFIG: &str = "/lingsched/config";

pub struct Armed {
    pub owner: String,
    pub path: String,
    pub job: Option<lcron::Job>,
    pub error: String,
    pub enabled: bool,
    /// Unix seconds of the last run (0 = never).
    pub last_run: u64,
    pub last_status: String,
    pub next_lm: Option<i64>,
    /// Timer runs within the last hour (unix seconds).
    pub recent: Vec<u64>,
    /// A run is owed (missed catch-up / waiting for the owner's session).
    pub owed: bool,
    pub waiting_logged: bool,
}

pub struct Work {
    pub owner: String,
    pub path: String,
    pub reason: &'static str,
    /// Run this text instead of the file (LingCal's Run now of an unsaved edit).
    pub text: Option<String>,
}

static mut JOBS: Vec<Armed> = Vec::new();
static mut QUEUE: Vec<Work> = Vec::new();
static mut ENABLED: bool = true;
static mut BOOTED: bool = false;
static mut TZ_S: i64 = 0;
static mut LAST_USER: Option<String> = None;
static mut VERSION: u64 = 1;

// the signed-in user's calendar and its upcoming reminders
static mut BOOK: Option<(String, cal::Book)> = None;
static mut REMIND: Vec<(i64, u32, i64, u32, bool)> = Vec::new();
static mut FIRED: Vec<(u32, i64, u32)> = Vec::new();
static mut REMIND_SCAN_MS: u64 = 0;

pub(crate) fn jobs() -> &'static mut Vec<Armed> {
    unsafe { &mut *(&raw mut JOBS) }
}
pub(crate) fn queue() -> &'static mut Vec<Work> {
    unsafe { &mut *(&raw mut QUEUE) }
}

pub fn bump_version() {
    unsafe { VERSION += 1 };
}
pub fn version() -> u64 {
    unsafe { VERSION }
}

// ── time ─────────────────────────────────────────────────────────────────────

pub fn tz_s() -> i64 {
    unsafe { TZ_S }
}

pub fn set_tz_hours(h: f64) {
    let s = (h.clamp(-12.0, 14.0) * 3600.0) as i64;
    if s != tz_s() {
        unsafe { TZ_S = s };
        save_config();
        recompute_all();
    }
}

/// Local minutes now.
pub fn now_lm() -> i64 {
    (crate::clock::unix_now() as i64 + tz_s()).div_euclid(60)
}

pub fn lm_to_unix(lm: i64) -> i64 {
    lm * 60 - tz_s()
}

pub fn unix_to_lm(u: i64) -> i64 {
    (u + tz_s()).div_euclid(60)
}

pub fn now_hm() -> String {
    cron::fmt_hm(now_lm().rem_euclid(MIN_PER_DAY))
}

/// The desktop's time zone for `user` (Settings > System, `tz=` in desktop.conf).
fn read_desktop_tz(user: &str) -> Option<f64> {
    let b = crate::vfs::read(&format!("home/{user}/Programs/LingOS/desktop.conf"))?;
    let t = String::from_utf8_lossy(&b).into_owned();
    t.lines().find_map(|l| l.strip_prefix("tz=").and_then(|v| v.trim().parse::<f64>().ok()))
}

// ── the service switch ───────────────────────────────────────────────────────

fn read_lingfs(p: &str) -> Option<Vec<u8>> {
    if !crate::fs::lingfs::is_mounted() || crate::fs::lingfs::path_kind(p) != Some(false) {
        return None;
    }
    let n = crate::fs::lingfs::file_size(p)?;
    let mut buf = alloc::vec![0u8; n.max(1)];
    match crate::fs::lingfs::read_file_all(p, &mut buf) {
        Ok(Some(k)) => {
            buf.truncate(k);
            Some(buf)
        },
        _ => None,
    }
}

fn load_config() {
    let Some(b) = read_lingfs(CONFIG) else { return };
    let t = String::from_utf8_lossy(&b).into_owned();
    for l in t.lines() {
        if l.trim() == "off" {
            unsafe { ENABLED = false };
        }
        if let Some(v) = l.strip_prefix("tz=").and_then(|v| v.trim().parse::<i64>().ok()) {
            unsafe { TZ_S = v.clamp(-12 * 3600, 14 * 3600) };
        }
    }
}

fn save_config() {
    if !crate::fs::lingfs::is_mounted() {
        return;
    }
    let t = format!("{}\ntz={}\n", if enabled() { "on" } else { "off" }, tz_s());
    let _ = crate::fs::lingfs::write_path_any(CONFIG, t.as_bytes());
}

pub fn enabled() -> bool {
    unsafe { ENABLED }
}

pub fn running() -> bool {
    enabled() && unsafe { BOOTED }
}

/// Settings > Startup: switch the scheduler on or off (kept across boots).
pub fn set_enabled(on: bool) -> bool {
    unsafe { ENABLED = on };
    save_config();
    if on {
        boot();
        load_jobs();
        recompute_all();
    }
    true
}

// ── boot ─────────────────────────────────────────────────────────────────────

static mut LOADED: bool = false;

/// From the first desktop frame (`startup::boot_once`): register the
/// builtins, read the armed lists of every user, note missed runs.
pub fn boot() {
    crate::lingrt::register(api::dispatch);
    if unsafe { BOOTED } {
        return;
    }
    unsafe { BOOTED = true };
    load_config();
    if !enabled() {
        crate::console_write(b"lingsched: switched off (Settings > Startup)\n");
        return;
    }
    load_jobs();
}

fn load_jobs() {
    if unsafe { LOADED } {
        return;
    }
    unsafe { LOADED = true };
    load_all_registries();
    let now = now_lm();
    let mut n = 0;
    for a in jobs().iter_mut() {
        n += 1;
        let Some(j) = &a.job else { continue };
        if !a.enabled || !j.enabled {
            continue;
        }
        // missed while the PC was off: an occurrence after the last run, before now
        if a.last_run > 0 {
            let last = unix_to_lm(a.last_run as i64);
            if let Some(t) = j.trigger.next_after(last) {
                if t < now - 1 {
                    match j.missed {
                        lcron::Missed::CatchUp => a.owed = true,
                        lcron::Missed::Skip => run::log_line(&a.owner, &a.path, &j.name, "missed", 0, &format!("missed {} while the PC was off: skipped", cron::fmt_rel(t, now))),
                    }
                }
            }
        }
        if j.at_boot {
            if j.needs_session() {
                a.owed = true; // runs at sign-in
            } else {
                queue().push(Work { owner: a.owner.clone(), path: a.path.clone(), reason: "at boot", text: None });
            }
        }
    }
    recompute_all();
    crate::console_write(format!("lingsched: scheduler up -- {n} armed job(s), local time {} (UTC{:+})\n", cron::fmt_lm(now), tz_s() / 3600).as_bytes());
}

fn registry_path(user: &str) -> String {
    format!("/lingsched/{user}-jobs")
}

fn load_all_registries() {
    if !crate::fs::lingfs::is_mounted() {
        return;
    }
    let mut users: Vec<String> = Vec::new();
    let mut name = [0u8; 64];
    let mut i = 0;
    while let Some((n, is_dir)) = crate::fs::lingfs::list_entry("/lingsched", i, &mut name) {
        i += 1;
        if is_dir {
            continue;
        }
        let s = core::str::from_utf8(&name[..n]).unwrap_or("");
        if let Some(u) = s.strip_suffix("-jobs") {
            users.push(String::from(u));
        }
        if i > 64 {
            break;
        }
    }
    for u in users {
        load_registry(&u);
    }
}

fn load_registry(user: &str) {
    jobs().retain(|a| a.owner != user);
    run::log_preload(user);
    let Some(b) = read_lingfs(&registry_path(user)) else { return };
    let t = String::from_utf8_lossy(&b).into_owned();
    for l in t.lines() {
        let f: Vec<&str> = l.split('|').collect();
        if f.len() < 3 || f[0] != "armed" {
            continue;
        }
        let path = String::from(f[1]);
        if !path.starts_with(&format!("home/{user}/")) {
            continue; // a job runs only from its owner's home
        }
        let mut a = Armed {
            owner: String::from(user),
            path,
            job: None,
            error: String::new(),
            enabled: f.get(3).map(|v| *v != "0").unwrap_or(true),
            last_run: f[2].parse().unwrap_or(0),
            last_status: String::from(f.get(4).copied().unwrap_or("")),
            next_lm: None,
            recent: Vec::new(),
            owed: false,
            waiting_logged: false,
        };
        reparse(&mut a);
        jobs().push(a);
    }
}

pub(crate) fn save_registry(user: &str) {
    let mut t = String::from("# LingScheduler: the armed automations of this user (written by the kernel)\n");
    for a in jobs().iter().filter(|a| a.owner == user) {
        t.push_str(&format!("armed|{}|{}|{}|{}\n", a.path, a.last_run, if a.enabled { 1 } else { 0 }, a.last_status.replace('|', "/")));
    }
    let _ = crate::fs::lingfs::write_path_any(&registry_path(user), t.as_bytes());
}

/// Parse an armed job's file again (after Save and arm, or at boot).
pub(crate) fn reparse(a: &mut Armed) {
    match crate::vfs::read(&a.path) {
        Some(b) => match lcron::parse(&String::from_utf8_lossy(&b)) {
            Ok(j) => {
                a.job = Some(j);
                a.error.clear();
            },
            Err(e) => {
                a.job = None;
                a.error = lcron::errors_text(&e);
            },
        },
        None => {
            a.job = None;
            a.error = String::from("the file is gone");
        },
    }
    let now = now_lm();
    a.next_lm = a.job.as_ref().and_then(|j| j.trigger.next_after(now));
}

fn recompute_all() {
    let now = now_lm();
    for a in jobs().iter_mut() {
        a.next_lm = a.job.as_ref().and_then(|j| j.trigger.next_after(now));
    }
    bump_version();
}

// ── tick ─────────────────────────────────────────────────────────────────────

/// Once a second from `startup::tick`. No storage reads except at a sign-in.
pub fn tick(now_ms: u64) {
    if !unsafe { BOOTED } {
        return;
    }
    notify::tick();
    let user = crate::keyring::user();
    if user != unsafe { (*(&raw const LAST_USER)).clone() } {
        unsafe { *(&raw mut LAST_USER) = user.clone() };
        if let Some(u) = &user {
            signed_in(u);
        } else {
            unsafe { *(&raw mut BOOK) = None };
            unsafe { (*(&raw mut REMIND)).clear() };
        }
    }
    if !enabled() {
        return;
    }
    let now = now_lm();
    let now_u = crate::clock::unix_now();
    for a in jobs().iter_mut() {
        let Some(j) = &a.job else { continue };
        if !a.enabled || !j.enabled {
            continue;
        }
        let due = a.next_lm.map(|t| t <= now).unwrap_or(false);
        if !due && !a.owed {
            continue;
        }
        if due {
            a.next_lm = j.trigger.next_after(now);
        }
        let session_ok = !j.needs_session() || user.as_deref() == Some(a.owner.as_str());
        if !session_ok {
            if due {
                if j.missed == lcron::Missed::CatchUp {
                    a.owed = true;
                }
                if !a.waiting_logged {
                    a.waiting_logged = true;
                    let why = if user.is_none() { "nobody is signed in (or the screen is locked)" } else { "another user is signed in" };
                    run::log_line(&a.owner, &a.path, &j.name, "waiting", 0, &format!("waiting: {why}; this job needs {}'s keyring", a.owner));
                }
            }
            continue;
        }
        a.waiting_logged = false;
        // the rate limit
        a.recent.retain(|t| now_u.saturating_sub(*t) < 3600);
        if a.recent.last().map(|t| now_u.saturating_sub(*t) < 60).unwrap_or(false) || a.recent.len() as u32 >= j.limit_per_hour {
            if due {
                run::log_line(&a.owner, &a.path, &j.name, "skipped", 0, "rate limit: ran too recently");
            }
            a.owed = false;
            continue;
        }
        if queue().iter().any(|w| w.path == a.path && w.owner == a.owner) {
            continue;
        }
        a.recent.push(now_u);
        let reason = if a.owed && !due { "catch up" } else { "timer" };
        a.owed = false;
        queue().push(Work { owner: a.owner.clone(), path: a.path.clone(), reason, text: None });
    }
    reminders_tick(now_ms, now);
    run::kick();
}

fn signed_in(user: &str) {
    run::log_preload(user);
    if let Some(h) = read_desktop_tz(user) {
        let s = (h * 3600.0) as i64;
        if s != tz_s() {
            unsafe { TZ_S = s };
            save_config();
            recompute_all();
        }
    }
    // the user's jobs may have been armed on another boot: read their list once
    if !jobs().iter().any(|a| a.owner == user) {
        load_registry(user);
        recompute_all();
    }
    for a in jobs().iter_mut().filter(|a| a.owner == user) {
        if let Some(j) = &a.job {
            if j.at_login && a.enabled && j.enabled && enabled() {
                queue().push(Work { owner: a.owner.clone(), path: a.path.clone(), reason: "at sign-in", text: None });
            }
        }
    }
    load_calendar(user);
}

// ── the calendar ─────────────────────────────────────────────────────────────

pub fn calendar_path(user: &str) -> String {
    format!("home/{user}/Projects/LingCal/events.lcal")
}

/// (Re)read `events.lcal` of `user`; returns the parse problems.
pub fn load_calendar(user: &str) -> String {
    let (book, errs) = match crate::vfs::read(&calendar_path(user)) {
        Some(b) => cal::parse(&String::from_utf8_lossy(&b)),
        None => (cal::Book::default(), Vec::new()),
    };
    unsafe { *(&raw mut BOOK) = Some((String::from(user), book)) };
    unsafe { REMIND_SCAN_MS = 0 };
    bump_version();
    errs.iter().map(|e| format!("line {}: {}", e.line, e.msg)).collect::<Vec<_>>().join("\n")
}

/// The signed-in user's calendar (loaded on demand).
pub fn book() -> Option<&'static mut cal::Book> {
    let user = crate::keyring::user().or_else(|| {
        let u = crate::fs::lingfs::current_user();
        (!u.is_empty()).then(|| String::from(u))
    })?;
    let cur = unsafe { (*(&raw const BOOK)).as_ref().map(|(u, _)| u.clone()) };
    if cur.as_deref() != Some(user.as_str()) {
        load_calendar(&user);
    }
    unsafe { (*(&raw mut BOOK)).as_mut().map(|(_, b)| b) }
}

pub fn book_user() -> Option<String> {
    unsafe { (*(&raw const BOOK)).as_ref().map(|(u, _)| u.clone()) }
}

/// Write the calendar back after an edit.
pub fn save_book() -> bool {
    let Some(user) = book_user() else { return false };
    let Some(b) = book() else { return false };
    let text = cal::write(b);
    let p = calendar_path(&user);
    ensure_dir(&crate::vfs::parent(&p));
    let ok = crate::vfs::write(&p, text.as_bytes());
    unsafe { REMIND_SCAN_MS = 0 };
    bump_version();
    ok
}

pub fn ensure_dir(p: &str) {
    if p.is_empty() || crate::vfs::is_dir(p) {
        return;
    }
    let parent = crate::vfs::parent(p);
    if !parent.is_empty() && parent != p {
        ensure_dir(&parent);
    }
    crate::vfs::mkdir(p);
}

fn reminders_tick(now_ms: u64, now: i64) {
    let Some(user) = crate::keyring::user() else { return };
    // the next day's reminders, refreshed every 10 minutes and after edits
    if unsafe { REMIND_SCAN_MS } == 0 || now_ms.saturating_sub(unsafe { REMIND_SCAN_MS }) > 600_000 {
        unsafe { REMIND_SCAN_MS = now_ms.max(1) };
        let list = match book() {
            Some(b) if book_user().as_deref() == Some(user.as_str()) => cal::reminders(b, now - 15, now + MIN_PER_DAY),
            _ => Vec::new(),
        };
        unsafe { *(&raw mut REMIND) = list };
    }
    let rem = unsafe { &mut *(&raw mut REMIND) };
    let fired = unsafe { &mut *(&raw mut FIRED) };
    let mut due: Vec<(i64, u32, i64, u32, bool)> = Vec::new();
    rem.retain(|r| {
        if r.0 <= now {
            due.push(*r);
            false
        } else {
            true
        }
    });
    for (fire, id, occ, before, sound) in due {
        if now - fire > 15 || fired.contains(&(id, occ, before)) {
            continue;
        }
        if fired.len() > 256 {
            fired.drain(0..128);
        }
        fired.push((id, occ, before));
        let Some(b) = book() else { continue };
        let Some(e) = b.get(id) else { continue };
        let col = cal::COLORS[b.cal_color(&e.cal) % cal::COLORS.len()].1;
        let when = if e.allday {
            String::from("All day")
        } else {
            format!("{} - {}", cron::fmt_rel(occ, now), cron::fmt_hm(occ + e.duration()))
        };
        let mut detail = when;
        if !e.cal.is_empty() {
            detail.push_str(&format!(" · {}", e.cal));
        }
        if !e.location.is_empty() {
            detail.push_str(&format!(" · {}", e.location));
        }
        if !e.note.is_empty() {
            detail.push_str(&format!(" · note: {}", e.note));
        }
        let right = if before == 0 { String::from("now") } else { format!("in {}", fmt_minutes(occ - now)) };
        notify::reminder(&user, &e.title, &detail, &right, id, occ, Some([col[1], col[2]]));
        if sound {
            chime("chime");
        }
    }
}

pub fn fmt_minutes(m: i64) -> String {
    let m = m.max(0);
    if m >= 1440 {
        format!("{} d", m / 1440)
    } else if m >= 60 {
        format!("{}:{:02} h", m / 60, m % 60)
    } else {
        format!("{m} min")
    }
}

/// A short sound: "chime" / "soft" (the ping), "alarm" (the error sound), "none".
pub fn chime(kind: &str) {
    #[cfg(target_arch = "x86_64")]
    {
        use crate::drivers::mixer;
        match kind {
            "none" | "off" | "" => {},
            "alarm" | "error" | "loud" => {
                mixer::jingle(mixer::EVENT_ERROR);
            },
            _ => mixer::jingle(mixer::EVENT_PING),
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = kind;
}

// ── LingCal's views of the scheduler ─────────────────────────────────────────

pub fn automations_dir(user: &str) -> String {
    format!("home/{user}/Projects/LingCal/automations")
}

/// Arm (or disarm) a job file of the signed-in user.
pub fn arm(user: &str, path: &str, on: bool) -> Result<(), String> {
    let path = crate::vfs::norm(path);
    if !path.starts_with(&format!("home/{user}/")) || !path.ends_with(".lcron") {
        return Err(String::from("only .lcron files in your own home can be armed"));
    }
    if !on {
        jobs().retain(|a| !(a.owner == user && a.path == path));
        save_registry(user);
        bump_version();
        return Ok(());
    }
    let text = crate::vfs::read(&path).map(|b| String::from_utf8_lossy(&b).into_owned()).ok_or_else(|| String::from("the file could not be read"))?;
    lcron::parse(&text).map_err(|e| lcron::errors_text(&e))?;
    let existing = jobs().iter().position(|a| a.owner == user && a.path == path);
    match existing {
        Some(i) => {
            let a = &mut jobs()[i];
            a.enabled = true;
            reparse(a);
        },
        None => {
            let mut a = Armed {
                owner: String::from(user),
                path: path.clone(),
                job: None,
                error: String::new(),
                enabled: true,
                last_run: 0,
                last_status: String::new(),
                next_lm: None,
                recent: Vec::new(),
                owed: false,
                waiting_logged: false,
            };
            reparse(&mut a);
            jobs().push(a);
        },
    }
    save_registry(user);
    bump_version();
    Ok(())
}

pub fn armed(user: &str, path: &str) -> Option<&'static mut Armed> {
    jobs().iter_mut().find(|a| a.owner == user && a.path == path)
}

/// Queue a manual run (Run now) of a file -- or of unsaved text.
pub fn run_now(user: &str, path: &str, text: Option<String>) -> Result<(), String> {
    if !path.starts_with(&format!("home/{user}/")) {
        return Err(String::from("only your own automations"));
    }
    let src = match &text {
        Some(t) => t.clone(),
        None => crate::vfs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).ok_or_else(|| String::from("the file could not be read"))?,
    };
    lcron::parse(&src).map_err(|e| lcron::errors_text(&e))?;
    if queue().len() >= 8 {
        return Err(String::from("too many runs waiting"));
    }
    queue().push(Work { owner: String::from(user), path: String::from(path), reason: "Run now", text });
    run::kick();
    Ok(())
}

/// Automation run times to mark on the calendar: (local minute, short label).
pub fn ticks(user: &str, from_lm: i64, to_lm: i64) -> Vec<(i64, String, String)> {
    let mut out = Vec::new();
    for a in jobs().iter().filter(|a| a.owner == user && a.enabled) {
        let Some(j) = &a.job else { continue };
        let label: String = short_label(&j.name);
        let mut t = from_lm - 1;
        let mut n = 0;
        while let Some(x) = j.trigger.next_after(t) {
            if x >= to_lm || n > 200 {
                break;
            }
            out.push((x, label.clone(), a.path.clone()));
            t = x;
            n += 1;
        }
    }
    out
}

fn short_label(name: &str) -> String {
    let w = name.split_whitespace().next().unwrap_or(name).to_ascii_lowercase();
    let w: String = w.chars().take(8).collect();
    w
}
