//! Running a job: the steps in order on a background scheduler task (a
//! `proc::sched` task, so SSH / SFTP / HTTP waits hand the CPU back to the
//! desktop), one job at a time, then the run log.
//!
//! Step semantics: a step whose `when` is false is skipped; after a step
//! fails, the rest are skipped unless their `when` names the failure (that
//! is the if / else). `on_fail` / `on_ok` run at the end with `{error}`.
//!
//! What a step may touch: files in the owner's home and on media/ only, never
//! the account stores (`vfs::is_account_secret`, `secrets/`, `lingsched/`).
//! Secrets come from the owner's keyring for the duration of the step and
//! are wiped after; the log never holds them (and any message containing a
//! secret value is masked before it is written).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::lcron::{self, Call, Cond, Job, Val};
use super::{cron, jobs, queue, secrets, Work};

#[derive(Clone, Default)]
pub struct StepResult {
    pub ok: bool,
    pub skipped: bool,
    pub path: String,
    pub size: u64,
    pub out: String,
    pub code: i64,
    pub error: String,
    /// A file made by this step but not written yet (a screenshot): uploaded from
    /// memory, dropped by delete(), written only if it is still wanted at the end.
    pub pending: Option<Vec<u8>>,
}

struct Ctx<'a> {
    owner: &'a str,
    job: &'a Job,
    results: Vec<(String, StepResult)>,
    error: String,
    started_lm: i64,
    secret_vals: Vec<Vec<u8>>,
    notes: Vec<String>,
}

impl Drop for Ctx<'_> {
    fn drop(&mut self) {
        for v in self.secret_vals.iter_mut() {
            secrets::wipe(v);
        }
    }
}

static mut TASK: bool = false;
static mut TASK_SINCE_MS: u64 = 0;
/// The result of the last connection test (Keyring > Test): "" running, "ok:..." / "err:...".
static mut TEST: Option<String> = None;
static mut TEST_REQ: Option<(String, String, String)> = None;

pub fn test_result() -> String {
    unsafe { (*(&raw const TEST)).clone().unwrap_or_default() }
}

/// Queue an SSH sign-in test of `target` with keyring secret `key`.
pub fn test_ssh(owner: &str, target: &str, key: &str) {
    unsafe {
        *(&raw mut TEST) = Some(String::new());
        *(&raw mut TEST_REQ) = Some((String::from(owner), String::from(target), String::from(key)));
    }
    kick();
}

/// Is a run going on (for the status line)?
pub fn busy() -> bool {
    unsafe { TASK }
}

/// Start the runner task when there is work and none is running.
pub fn kick() {
    if queue().is_empty() && unsafe { (*(&raw const TEST_REQ)).is_none() } {
        return;
    }
    if unsafe { TASK } {
        return;
    }
    let slot = crate::proc::sched::spawn(runner_task as *const () as usize as u64);
    if slot == u64::MAX {
        // every task slot taken: run on a coroutine of the main task instead
        // after a while (network waits then hold the frame briefly)
        let now = crate::arch::timer::now_ms();
        if unsafe { TASK_SINCE_MS } == 0 {
            unsafe { TASK_SINCE_MS = now };
            return;
        }
        if now.saturating_sub(unsafe { TASK_SINCE_MS }) < 20_000 {
            return;
        }
        let id = crate::coro::spawn_owned("lingsched", false, "system", alloc::boxed::Box::new(|| drain()));
        if id != 0 {
            unsafe {
                TASK = true;
                TASK_SINCE_MS = 0;
            }
        }
        return;
    }
    crate::proc::sched::set_name(slot, "(LingScheduler)");
    unsafe {
        TASK = true;
        TASK_SINCE_MS = 0;
    }
}

extern "C" fn runner_task() {
    drain();
}

fn drain() {
    loop {
        if let Some((owner, target, key)) = unsafe { (*(&raw mut TEST_REQ)).take() } {
            let r = match ssh_test(&owner, &target, &key) {
                Ok(m) => format!("ok:{m}"),
                Err(e) => format!("err:{e}"),
            };
            unsafe { *(&raw mut TEST) = Some(r) };
            continue;
        }
        if queue().is_empty() {
            break;
        }
        let w = queue().remove(0);
        execute(w);
        log_flush_all();
    }
    log_flush_all();
    unsafe { TASK = false };
}

fn ssh_test(owner: &str, target: &str, key: &str) -> Result<String, String> {
    let (user, host, port, _) = lcron::parse_target(target).ok_or_else(|| String::from("write the target as user@host or user@host:port"))?;
    let (kind, mut val) = secrets::get(owner, key)?;
    let r = (|| {
        let mut c = match kind {
            secrets::Kind::SshKey => {
                let seed: [u8; 32] = val.as_slice().try_into().map_err(|_| String::from("bad key"))?;
                super::sshc::session(owner, &user, &host, port, Some(&seed), None)?
            },
            _ => super::sshc::session(owner, &user, &host, port, None, Some(&val))?,
        };
        let (code, out) = c.exec("echo LingOS-ok; uname -n")?;
        c.close();
        if code == 0 {
            Ok(format!("signed in to {host} as {user}: {}", out.lines().nth(1).unwrap_or("").trim()))
        } else {
            Err(format!("signed in, but the test command failed ({code})"))
        }
    })();
    secrets::wipe(&mut val);
    r
}

// ── sleeping without stopping the desktop ────────────────────────────────────

fn sleep_ms(ms: u64) {
    let end = crate::arch::timer::now_ms() + ms;
    if crate::proc::sched::is_main() {
        crate::coro::sleep_ms(ms);
        return;
    }
    while crate::arch::timer::now_ms() < end {
        crate::proc::sched::yield_now();
    }
}

// ── the log ──────────────────────────────────────────────────────────────────

fn log_path(user: &str) -> String {
    format!("/lingsched/{user}-log")
}

// The logs live in memory once read (at boot / sign-in): the desktop's views never
// touch storage, so a run writing a big file (a screenshot into lingfs can take
// seconds, holding the filesystem lock) cannot stall the desktop. Disk writes
// happen on the runner task; a line logged from the desktop's tick while a run is
// going waits in memory and goes out with the runner's next write.
static mut LOGS: Vec<(String, Vec<String>, bool)> = Vec::new();

fn log_entry(user: &str) -> &'static mut (String, Vec<String>, bool) {
    let logs = unsafe { &mut *(&raw mut LOGS) };
    if let Some(i) = logs.iter().position(|(u, _, _)| u == user) {
        return &mut logs[i];
    }
    let p = log_path(user);
    let lines: Vec<String> = match crate::fs::lingfs::file_size(&p) {
        Some(n) => {
            let mut b = alloc::vec![0u8; n.max(1)];
            match crate::fs::lingfs::read_file_all(&p, &mut b) {
                Ok(Some(k)) => String::from_utf8_lossy(&b[..k]).lines().map(String::from).collect(),
                _ => Vec::new(),
            }
        },
        None => Vec::new(),
    };
    logs.push((String::from(user), lines, false));
    logs.last_mut().unwrap()
}

/// Read a user's log into memory (at boot / sign-in, when nothing else holds the disk).
pub fn log_preload(user: &str) {
    if crate::fs::lingfs::is_mounted() && !user.is_empty() {
        let _ = log_entry(user);
    }
}

fn log_flush(user: &str) {
    let e = log_entry(user);
    if !e.2 {
        return;
    }
    e.2 = false;
    let mut text = e.1.join("\n");
    text.push('\n');
    let _ = crate::fs::lingfs::write_path_any(&log_path(user), text.as_bytes());
}

/// Write every log with lines waiting (the runner, after a job).
pub fn log_flush_all() {
    let users: Vec<String> = unsafe { (*(&raw const LOGS)).iter().filter(|(_, _, d)| *d).map(|(u, _, _)| u.clone()).collect() };
    for u in users {
        log_flush(&u);
    }
}

/// One line in the owner's run log: `unix|file|job|status|ms|message`.
pub fn log_line(owner: &str, path: &str, job: &str, status: &str, ms: u64, msg: &str) {
    if !crate::fs::lingfs::is_mounted() || owner.is_empty() {
        return;
    }
    let clean = |s: &str| s.replace(['|', '\n', '\r'], " ");
    let file = crate::vfs::basename(path);
    let e = log_entry(owner);
    e.1.push(format!("{}|{}|{}|{}|{}|{}", crate::clock::unix_now(), clean(&file), clean(job), status, ms, clean(msg)));
    // keep the newest 400 lines
    if e.1.len() > 400 {
        let cut = e.1.len() - 400;
        e.1.drain(0..cut);
    }
    e.2 = true;
    // from the desktop's tick during a run: the runner writes it out
    if !(busy() && crate::proc::sched::is_main()) {
        log_flush(owner);
    }
    super::bump_version();
}

/// The newest `n` log lines of a user (optionally only one file), newest first. Memory only.
pub fn log_read(user: &str, file: &str, n: usize) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let logs = unsafe { &*(&raw const LOGS) };
    let Some((_, lines, _)) = logs.iter().find(|(u, _, _)| u == user) else {
        // not read yet: never from a desktop view while a run holds the disk
        if busy() {
            return out;
        }
        let _ = log_entry(user);
        return log_read(user, file, n);
    };
    for l in lines.iter().rev() {
        let f: Vec<String> = l.splitn(6, '|').map(String::from).collect();
        if f.len() < 6 {
            continue;
        }
        if !file.is_empty() && f[1] != file {
            continue;
        }
        out.push(f);
        if out.len() >= n {
            break;
        }
    }
    out
}

// ── one job ──────────────────────────────────────────────────────────────────

fn execute(w: Work) {
    let t0 = crate::arch::timer::now_ms();
    let src = match &w.text {
        Some(t) => Some(t.clone()),
        None => crate::vfs::read(&w.path).map(|b| String::from_utf8_lossy(&b).into_owned()),
    };
    let Some(src) = src else {
        log_line(&w.owner, &w.path, "", "failed", 0, "the .lcron file could not be read");
        return;
    };
    let job = match lcron::parse(&src) {
        Ok(j) => j,
        Err(e) => {
            log_line(&w.owner, &w.path, "", "failed", 0, &format!("not run: {}", lcron::errors_text(&e).lines().next().unwrap_or("")));
            return;
        },
    };
    if !w.path.starts_with(&format!("home/{}/", w.owner)) {
        log_line(&w.owner, &w.path, &job.name, "failed", 0, "refused: a job runs only from its owner's home");
        return;
    }
    if job.needs_session() && crate::keyring::user().as_deref() != Some(w.owner.as_str()) {
        log_line(&w.owner, &w.path, &job.name, "waiting", 0, &format!("not run ({}): needs {} signed in with the keyring open", w.reason, w.owner));
        if let Some(a) = super::armed(&w.owner, &w.path) {
            if job.missed == lcron::Missed::CatchUp {
                a.owed = true;
            }
        }
        return;
    }
    let mut cx = Ctx { owner: &w.owner, job: &job, results: Vec::new(), error: String::new(), started_lm: super::now_lm(), secret_vals: Vec::new(), notes: Vec::new() };
    let mut failed = false;
    let mut retried = false;
    for st in &job.steps {
        let wanted = match &st.when {
            Some(c) => eval(&cx, c),
            None => !failed,
        };
        if !wanted {
            cx.results.push((st.name.clone(), StepResult { skipped: true, ..Default::default() }));
            continue;
        }
        let retries = match st.call.named("retries") {
            Some(Val::Num(n)) => (*n as u32).min(5),
            _ => 0,
        };
        let mut r = StepResult::default();
        for attempt in 0..=retries {
            r = action(&mut cx, &st.call);
            if r.ok {
                break;
            }
            // a host key that is new or changed is the user's call, not a network hiccup
            if r.error.starts_with("unknown host") || r.error.starts_with("HOST KEY MISMATCH") {
                break;
            }
            if attempt < retries {
                retried = true;
                cx.notes.push(format!("retry {} ({}/{}): {}", st.call.action, attempt + 1, retries, r.error));
                sleep_ms(5000 * (attempt as u64 + 1));
            }
        }
        if !r.ok {
            failed = true;
            cx.error = format!("{}: {}", st.name, r.error);
        }
        cx.results.push((st.name.clone(), r));
    }
    let handlers: Vec<Call> = if failed { job.on_fail.clone() } else { job.on_ok.clone() };
    // a screenshot nobody deleted is kept, like PrtSc's (written now, by this task)
    flush_pending(&mut cx);
    for c in &handlers {
        let r = action(&mut cx, c);
        if !r.ok {
            cx.notes.push(format!("{} failed: {}", c.action, r.error));
        }
    }
    let ms = crate::arch::timer::now_ms().saturating_sub(t0);
    let status = if failed {
        "failed"
    } else if retried {
        "retried"
    } else {
        "ok"
    };
    let mut msg = if failed {
        cx.error.clone()
    } else {
        // what the steps did, briefly
        cx.results
            .iter()
            .filter(|(_, r)| !r.skipped)
            .map(|(n, r)| if !r.path.is_empty() { format!("{n}: {}", crate::vfs::basename(&r.path)) } else if !r.out.is_empty() { format!("{n}: {}", first_line(&r.out)) } else { format!("{n} ok") })
            .collect::<Vec<_>>()
            .join(", ")
    };
    for n in &cx.notes {
        msg.push_str(" | ");
        msg.push_str(n);
    }
    if w.reason != "timer" {
        msg = format!("[{}] {msg}", w.reason);
    }
    let msg = mask(&cx, &msg);
    log_line(&w.owner, &w.path, &job.name, status, ms, &msg);
    let unix = crate::clock::unix_now();
    for a in jobs().iter_mut().filter(|a| a.owner == w.owner && a.path == w.path) {
        a.last_run = unix;
        a.last_status = String::from(status);
    }
    if w.text.is_none() && jobs().iter().any(|a| a.owner == w.owner && a.path == w.path) {
        super::save_registry(&w.owner);
    }
    crate::console_write(format!("lingsched: {} ({}) {status} in {ms} ms\n", job.name, w.reason).as_bytes());
}

fn first_line(s: &str) -> String {
    let l = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    l.chars().take(80).collect()
}

/// Replace any secret value the run fetched with `***`.
fn mask(cx: &Ctx, s: &str) -> String {
    let mut out = String::from(s);
    for v in &cx.secret_vals {
        if v.len() >= 4 {
            if let Ok(t) = core::str::from_utf8(v) {
                if out.contains(t) {
                    out = out.replace(t, "***");
                }
            }
        }
    }
    out
}

fn result_of<'a>(cx: &'a Ctx, name: &str) -> Option<&'a StepResult> {
    cx.results.iter().find(|(n, _)| n == name).map(|(_, r)| r)
}

fn eval(cx: &Ctx, c: &Cond) -> bool {
    match c {
        Cond::Ok(s) => result_of(cx, s).map(|r| r.ok && !r.skipped).unwrap_or(false),
        Cond::Failed(s) => result_of(cx, s).map(|r| !r.ok && !r.skipped).unwrap_or(false),
        Cond::Skipped(s) => result_of(cx, s).map(|r| r.skipped).unwrap_or(true),
        Cond::Contains(s, t) => result_of(cx, s).map(|r| r.out.contains(t.as_str())).unwrap_or(false),
        Cond::Not(c) => !eval(cx, c),
    }
}

// ── values ───────────────────────────────────────────────────────────────────

fn two(n: u32) -> String {
    format!("{n:02}")
}

/// `{date}`, `{step.name}` ... in a text.
fn expand(cx: &Ctx, s: &str) -> String {
    let lm = cx.started_lm;
    let (y, mo, d) = cron::civil_from_days(lm.div_euclid(cron::MIN_PER_DAY));
    let mm = lm.rem_euclid(cron::MIN_PER_DAY);
    let secs = crate::clock::unix_now() % 60;
    let mut out = String::new();
    let mut rest = s;
    while let Some(a) = rest.find('{') {
        out.push_str(&rest[..a]);
        let after = &rest[a + 1..];
        let Some(b) = after.find('}') else {
            out.push_str(&rest[a..]);
            rest = "";
            break;
        };
        let key = after[..b].trim();
        let v = match key {
            "date" => format!("{y:04}-{}-{}", two(mo), two(d)),
            "time" => format!("{}-{}-{}", two((mm / 60) as u32), two((mm % 60) as u32), two(secs as u32)),
            "datetime" => format!("{y:04}-{}-{}_{}-{}", two(mo), two(d), two((mm / 60) as u32), two((mm % 60) as u32)),
            "year" => format!("{y:04}"),
            "month" => two(mo),
            "day" => two(d),
            "hour" => two((mm / 60) as u32),
            "minute" => two((mm % 60) as u32),
            "user" => String::from(cx.owner),
            "job" => cx.job.name.clone(),
            "error" => cx.error.clone(),
            "host" => String::from("lingos"),
            k => {
                let (st, f) = k.split_once('.').unwrap_or((k, ""));
                match result_of(cx, st) {
                    Some(r) => field(r, f),
                    None => format!("{{{k}}}"),
                }
            },
        };
        out.push_str(&v);
        rest = &after[b + 1..];
    }
    out.push_str(rest);
    out
}

fn field(r: &StepResult, f: &str) -> String {
    match f {
        "path" => r.path.clone(),
        "name" => crate::vfs::basename(&r.path),
        "size" => human_size(r.size),
        "out" => r.out.clone(),
        "code" => format!("{}", r.code),
        "ok" => String::from(if r.ok { "yes" } else { "no" }),
        "failed" => String::from(if r.ok { "no" } else { "yes" }),
        _ => {
            if !r.path.is_empty() {
                r.path.clone()
            } else {
                r.out.clone()
            }
        },
    }
}

fn human_size(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MB", n as f64 / 1048576.0)
    } else if n >= 1024 {
        format!("{} KB", n / 1024)
    } else {
        format!("{n} B")
    }
}

fn text_of(cx: &Ctx, v: Option<&Val>) -> String {
    match v {
        Some(Val::Str(s)) => expand(cx, s),
        Some(Val::Num(n)) => {
            if *n == (*n as i64) as f64 {
                format!("{}", *n as i64)
            } else {
                format!("{n}")
            }
        },
        Some(Val::Dur(d)) => format!("{d}"),
        Some(Val::Bool(b)) => String::from(if *b { "yes" } else { "no" }),
        Some(Val::Ref(s, f)) => result_of(cx, s).map(|r| field(r, f)).unwrap_or_default(),
        Some(Val::Secret(_)) | None => String::new(),
    }
}

fn num_of(v: Option<&Val>, d: f64) -> f64 {
    match v {
        Some(Val::Num(n)) => *n,
        Some(Val::Dur(s)) => *s as f64,
        Some(Val::Str(s)) => s.trim().parse().unwrap_or(d),
        _ => d,
    }
}

/// A path argument: `/x` from the root (media/usb0/...), anything else in the
/// owner's home. Refused outside the home and media/, and on account stores.
fn path_of(cx: &Ctx, v: Option<&Val>) -> Result<String, String> {
    let raw = match v {
        Some(Val::Ref(s, f)) => {
            let r = result_of(cx, s).ok_or_else(|| format!("`{s}` did not run"))?;
            if f.is_empty() || f == "path" {
                r.path.clone()
            } else {
                field(r, f)
            }
        },
        other => text_of(cx, other),
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(String::from("no path"));
    }
    let home = format!("home/{}", cx.owner);
    let p = if raw.starts_with("home/") || raw.starts_with("media/") {
        crate::vfs::norm(raw)
    } else if let Some(abs) = raw.strip_prefix('/') {
        crate::vfs::norm(abs)
    } else if let Some(r) = raw.strip_prefix("~/") {
        crate::vfs::join(&home, r)
    } else {
        crate::vfs::join(&home, raw)
    };
    let top = p.split('/').next().unwrap_or("");
    if crate::vfs::is_account_secret(&p) || top == "secrets" || top == "lingsched" {
        return Err(format!("{p} is the system's account data"));
    }
    if !(p == home || p.starts_with(&format!("{home}/")) || p.starts_with("media/")) {
        return Err(format!("{p} is outside your home and media/ (a job only touches those)"));
    }
    if p.split('/').any(|s| s == "..") {
        return Err(String::from("no .. in paths"));
    }
    Ok(p)
}

fn fail(e: String) -> StepResult {
    StepResult { ok: false, error: e, ..Default::default() }
}

fn okr() -> StepResult {
    StepResult { ok: true, ..Default::default() }
}

// ── actions ──────────────────────────────────────────────────────────────────

/// The unwritten bytes of the step a value refers to, if any.
fn pending_of(cx: &Ctx, v: Option<&Val>) -> Option<Vec<u8>> {
    let Some(Val::Ref(s, f)) = v else { return None };
    if !(f.is_empty() || f == "path") {
        return None;
    }
    cx.results.iter().find(|(n, _)| n == s).and_then(|(_, r)| r.pending.clone())
}

/// Write every step file still only in memory (before a file action that needs it on
/// disk, and at the end of the run).
fn flush_pending(cx: &mut Ctx) {
    for (_, r) in cx.results.iter_mut() {
        if let Some(data) = r.pending.take() {
            super::ensure_dir(&crate::vfs::parent(&r.path));
            if !crate::vfs::write(&r.path, &data) {
                cx.notes.push(format!("could not save {}", r.path));
            }
        }
    }
}

fn action(cx: &mut Ctx, c: &Call) -> StepResult {
    // delete(shot) of a screenshot never written: just forget it
    if c.action == "delete" {
        if let Some(Val::Ref(s, f)) = c.arg(0, "path") {
            if f.is_empty() || f == "path" {
                if let Some((_, r)) = cx.results.iter_mut().find(|(n, _)| n == s) {
                    if r.pending.take().is_some() {
                        return StepResult { ok: true, out: format!("deleted {} (it was never written to disk)", crate::vfs::basename(&r.path)), ..Default::default() };
                    }
                }
            }
        }
    }
    if matches!(c.action.as_str(), "copy" | "move" | "delete" | "write" | "shell") {
        flush_pending(cx);
    }
    match c.action.as_str() {
        "screenshot" => screenshot(cx, c),
        "screenrec" => screenrec(cx, c),
        "sftp.put" | "ssh.run" => ssh_action(cx, c),
        "shell" => {
            let cmd = text_of(cx, c.arg(0, "command"));
            let (name, arg) = cmd.trim().split_once(' ').unwrap_or((cmd.trim(), ""));
            let mut cwd = format!("home/{}", cx.owner);
            match crate::drivers::sshcmds::run(name, arg.trim(), cx.owner, &mut cwd) {
                Some(out) => {
                    let bad = out.contains("permission denied") || out.starts_with(&format!("{name}: usage")) || out.starts_with("usage:");
                    StepResult { ok: !bad, out: out.clone(), error: if bad { first_line(&out) } else { String::new() }, ..Default::default() }
                },
                None => fail(format!("`{name}` is not a LingOS shell command (try hwinfo, hitches, report, df, ls)")),
            }
        },
        "app.open" => {
            let n = text_of(cx, c.arg(0, "name"));
            let app = app_alias(&n);
            if crate::remote::push(&format!("open:{app}")) {
                StepResult { ok: true, out: format!("asked the desktop to open {app}"), ..Default::default() }
            } else {
                fail(String::from("the desktop's command queue is full"))
            }
        },
        "notify" | "remind" => {
            let text = text_of(cx, c.arg(0, "text"));
            let title = match c.named("title") {
                Some(v) => text_of(cx, Some(v)),
                None => cx.job.name.clone(),
            };
            let sound = match c.named("sound") {
                Some(v) => text_of(cx, Some(v)),
                None => String::from(if c.action == "remind" { "chime" } else { "none" }),
            };
            let failed_notice = !cx.error.is_empty();
            if c.action == "remind" {
                super::notify::push(super::notify::Toast {
                    id: 0,
                    owner: String::from(cx.owner),
                    header: String::from("LingCal reminder"),
                    right: super::now_hm(),
                    title: text.clone(),
                    detail: format!("from the automation \"{}\"", cx.job.name),
                    accent: [0x6d45b0, 0x4f3281],
                    reminder: true,
                    event: u32::MAX,
                    occ: 0,
                    created_ms: crate::arch::timer::now_ms(),
                    ttl_ms: 0,
                });
            } else {
                super::notify::notice(cx.owner, &title, &text, failed_notice, false);
            }
            super::chime(&sound);
            StepResult { ok: true, out: text, ..Default::default() }
        },
        "play" => {
            let s = text_of(cx, c.arg(0, "sound"));
            super::chime(if s.is_empty() { "chime" } else { &s });
            okr()
        },
        "copy" | "move" => {
            let (from, to) = match (path_of(cx, c.arg(0, "from")), path_of(cx, c.arg(1, "to"))) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) => return fail(e),
            };
            if !crate::vfs::exists(&from) && !crate::vfs::is_dir(&from) {
                return fail(format!("{from} does not exist"));
            }
            let to = if crate::vfs::is_dir(&to) || to.ends_with('/') { crate::vfs::join(&to, &crate::vfs::basename(&from)) } else { to };
            super::ensure_dir(&crate::vfs::parent(&to));
            let ok = if c.action == "copy" { crate::vfs::copy(&from, &to) } else { crate::vfs::rename(&from, &to) };
            if ok {
                StepResult { ok: true, path: to, ..Default::default() }
            } else {
                fail(format!("could not {} {from} to {to}", c.action))
            }
        },
        "delete" => {
            let p = match path_of(cx, c.arg(0, "path")) {
                Ok(p) => p,
                Err(e) => return fail(e),
            };
            if p == format!("home/{}", cx.owner) {
                return fail(String::from("refusing to delete your whole home"));
            }
            let base = crate::vfs::basename(&p);
            if base.contains('*') {
                // a pattern in the last part: Downloads/*.tmp
                let dir = crate::vfs::parent(&p);
                let mut n = 0;
                for (name, is_dir, _) in crate::vfs::list(&dir) {
                    if !is_dir && glob(&base, &name) && crate::vfs::delete(&crate::vfs::join(&dir, &name)) {
                        n += 1;
                    }
                }
                StepResult { ok: true, out: format!("deleted {n} file(s) in {dir}"), ..Default::default() }
            } else if crate::vfs::is_dir(&p) {
                if crate::vfs::delete_tree(&p) {
                    StepResult { ok: true, out: format!("deleted the folder {p}"), ..Default::default() }
                } else {
                    fail(format!("could not delete {p}"))
                }
            } else if crate::vfs::delete(&p) {
                StepResult { ok: true, out: format!("deleted {}", base), ..Default::default() }
            } else if !crate::vfs::exists(&p) {
                fail(format!("{p} does not exist"))
            } else {
                fail(format!("could not delete {p}"))
            }
        },
        "write" => {
            let text = text_of(cx, c.arg(0, "text"));
            let p = match path_of(cx, c.arg(1, "path")) {
                Ok(p) => p,
                Err(e) => return fail(e),
            };
            let append = matches!(c.named("append"), Some(Val::Bool(true)));
            let mut data = if append { crate::vfs::read(&p).unwrap_or_default() } else { Vec::new() };
            data.extend_from_slice(text.as_bytes());
            super::ensure_dir(&crate::vfs::parent(&p));
            if crate::vfs::write(&p, &data) {
                StepResult { ok: true, path: p, size: data.len() as u64, ..Default::default() }
            } else {
                fail(format!("could not write {p}"))
            }
        },
        "http.get" => {
            let url = text_of(cx, c.arg(0, "url"));
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return fail(String::from("the address must start with http:// or https://"));
            }
            let (code, body) = crate::netapi::http_get(&url);
            let mut r = StepResult { ok: (200..300).contains(&code), code: code as i64, out: body.chars().take(65536).collect(), ..Default::default() };
            if !r.ok {
                r.error = if code == 0 { String::from("no response") } else { format!("HTTP {code}") };
            } else if let Some(v) = c.named("save") {
                match path_of(cx, Some(v)) {
                    Ok(p) => {
                        super::ensure_dir(&crate::vfs::parent(&p));
                        if crate::vfs::write(&p, body.as_bytes()) {
                            r.path = p;
                            r.size = body.len() as u64;
                        }
                    },
                    Err(e) => return fail(e),
                }
            }
            r
        },
        "wait" => {
            let s = num_of(c.arg(0, "duration"), 1.0).clamp(0.0, 3600.0);
            sleep_ms((s * 1000.0) as u64);
            okr()
        },
        "log" => {
            let t = text_of(cx, c.arg(0, "text"));
            cx.notes.push(t.clone());
            StepResult { ok: true, out: t, ..Default::default() }
        },
        other => fail(format!("unknown action {other}")),
    }
}

/// Every file under a folder (depth 8, hidden files and account stores left out).
fn walk(dir: &str, depth: usize, out: &mut Vec<String>) {
    if depth > 8 || out.len() > 2000 {
        return;
    }
    for (name, is_dir, _) in crate::vfs::list(dir) {
        if name.starts_with('.') {
            continue;
        }
        let p = crate::vfs::join(dir, &name);
        if crate::vfs::is_account_secret(&p) {
            continue;
        }
        if is_dir {
            walk(&p, depth + 1, out);
        } else {
            out.push(p);
        }
    }
}

fn glob(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    fn go(p: &[char], n: &[char]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a.eq_ignore_ascii_case(b) => go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    go(&p, &n)
}

/// App names people write -> the image's app names.
fn app_alias(n: &str) -> String {
    let flat: String = n.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_ascii_lowercase();
    let m = match flat.as_str() {
        "lingstationx" | "lingstation" | "stationx" => "lingstation",
        "lingdev" | "dev" => "lingdev",
        "lingmodeler" | "modeler" => "lingmodeler",
        "horizon" | "browser" => "horizon",
        "wallet" | "lingwallet" => "wallet",
        "lingcal" | "calendar" => "lingcal",
        "lingcoder" | "coder" => "lingcoder",
        "lingsys" | "sys" => "lingsys",
        "messenger" | "lingmessenger" => "messenger",
        "files" => "files",
        "terminal" => "terminal",
        "settings" => "settings",
        _ => "",
    };
    if m.is_empty() {
        String::from(n.trim())
    } else {
        String::from(m)
    }
}

#[cfg(target_arch = "x86_64")]
fn screenshot(cx: &mut Ctx, c: &Call) -> StepResult {
    let Some((frame, w, h)) = crate::drivers::framebuffer::read_screen() else { return fail(String::from("no screen to capture")) };
    let mut rgb = Vec::with_capacity(w * h * 3);
    for p in &frame {
        rgb.push((p >> 16) as u8);
        rgb.push((p >> 8) as u8);
        rgb.push(*p as u8);
    }
    drop(frame);
    let png = ling_paint::os::png::encode_raw(w, h, 3, 8, &rgb);
    drop(rgb);
    let dest = match c.named("to") {
        Some(v) => match path_of(cx, Some(v)) {
            Ok(p) => p,
            Err(e) => return fail(e),
        },
        None => {
            let lm = cx.started_lm;
            let (y, mo, d) = cron::civil_from_days(lm.div_euclid(cron::MIN_PER_DAY));
            let mm = lm.rem_euclid(cron::MIN_PER_DAY);
            format!("home/{}/Photos/Screenshots/LingCal-{y:04}{mo:02}{d:02}-{:02}{:02}{:02}.png", cx.owner, mm / 60, mm % 60, crate::clock::unix_now() % 60)
        },
    };
    let dest = if dest.ends_with('/') || crate::vfs::is_dir(&dest) { crate::vfs::join(&dest, &format!("shot-{}.png", crate::clock::unix_now())) } else { dest };
    // not written yet: a big file into lingfs holds the filesystem lock for seconds,
    // and upload-then-delete jobs never need it on disk at all (see flush_pending)
    let size = png.len() as u64;
    StepResult { ok: true, path: dest, size, out: format!("{w}x{h}"), pending: Some(png), ..Default::default() }
}

#[cfg(not(target_arch = "x86_64"))]
fn screenshot(_cx: &mut Ctx, _c: &Call) -> StepResult {
    fail(String::from("screenshots are x86-only for now"))
}

#[cfg(target_arch = "x86_64")]
fn screenrec(_cx: &mut Ctx, c: &Call) -> StepResult {
    let secs = num_of(c.arg(0, "secs"), 30.0).clamp(1.0, 3600.0);
    if crate::drivers::screenrec::active() {
        return fail(String::from("a screen recording is already running"));
    }
    if let Err(e) = crate::drivers::screenrec::start(0, false) {
        return fail(e);
    }
    // frames are taken by the desktop's present(); this task just waits
    sleep_ms((secs * 1000.0) as u64);
    match crate::drivers::screenrec::stop() {
        Ok(p) => {
            let size = crate::vfs::read(&p).map(|b| b.len() as u64).unwrap_or(0);
            StepResult { ok: true, path: p, size, ..Default::default() }
        },
        Err(e) => fail(e),
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn screenrec(_cx: &mut Ctx, _c: &Call) -> StepResult {
    fail(String::from("screen recording is x86-only for now"))
}

fn ssh_action(cx: &mut Ctx, c: &Call) -> StepResult {
    let put = c.action == "sftp.put";
    let target = text_of(cx, c.arg(if put { 1 } else { 0 }, "target"));
    let Some((user, host, mut port, rpath)) = lcron::parse_target(&target) else { return fail(format!("`{target}` is not user@host:/path")) };
    if let Some(p) = c.named("port") {
        port = num_of(Some(p), port as f64) as u16;
    }
    // the credentials, for this step only
    let mut seed: Option<[u8; 32]> = None;
    let mut pw: Option<Vec<u8>> = None;
    for (n, v) in &c.named {
        if let Val::Secret(name) = v {
            match secrets::get(cx.owner, name) {
                Ok((kind, val)) => match (n.as_str(), kind) {
                    ("key", secrets::Kind::SshKey) => {
                        seed = val.as_slice().try_into().ok();
                        cx.secret_vals.push(val);
                    },
                    ("key", _) => return fail(format!("keyring(\"{name}\") is not an SSH key")),
                    (_, secrets::Kind::SshKey) => return fail(format!("keyring(\"{name}\") is an SSH key: give it as key:")),
                    _ => {
                        pw = Some(val.clone());
                        cx.secret_vals.push(val);
                    },
                },
                Err(e) => return fail(e),
            }
        }
    }
    let r = (|| -> Result<StepResult, String> {
        let mut conn = super::sshc::session(cx.owner, &user, &host, port, seed.as_ref(), pw.as_deref())?;
        if put {
            let local = path_of(cx, c.arg(0, "file"))?;
            let name = crate::vfs::basename(&local);
            if crate::vfs::is_dir(&local) {
                // a folder: every file under it, in one SFTP session, into <target>/<folder>/...
                let mut files: Vec<String> = Vec::new();
                walk(&local, 0, &mut files);
                if files.len() > 2000 {
                    return Err(format!("{local} holds more than 2000 files: pick a smaller folder"));
                }
                let base = if rpath.is_empty() || rpath.ends_with('/') { format!("{rpath}{name}") } else { rpath.clone() };
                conn.sftp_start()?;
                let mut total = 0u64;
                for f in &files {
                    let data = crate::vfs::read(f).ok_or_else(|| format!("cannot read {f}"))?;
                    let rel = f.strip_prefix(&format!("{local}/")).unwrap_or(f);
                    conn.sftp_put_file(&data, &format!("{base}/{rel}"))?;
                    total += data.len() as u64;
                }
                conn.close();
                return Ok(StepResult { ok: true, path: local, size: total, out: format!("{} file(s) to {host}:{base}/", files.len()), ..Default::default() });
            }
            let data = match pending_of(cx, c.arg(0, "file")) {
                Some(d) => d,
                None => crate::vfs::read(&local).ok_or_else(|| format!("cannot read {local}"))?,
            };
            let remote = if rpath.is_empty() || rpath.ends_with('/') { format!("{rpath}{name}") } else { rpath.clone() };
            conn.sftp_put(&data, &remote)?;
            conn.close();
            Ok(StepResult { ok: true, path: local, size: data.len() as u64, out: format!("{host}:{remote}"), ..Default::default() })
        } else {
            let cmd = text_of(cx, c.arg(1, "command"));
            let (code, out) = conn.exec(&cmd)?;
            conn.close();
            Ok(StepResult { ok: code == 0, code: code as i64, error: if code == 0 { String::new() } else { format!("exit status {code}: {}", first_line(&out)) }, out, ..Default::default() })
        }
    })();
    if let Some(mut s) = seed {
        secrets::wipe(&mut s);
    }
    if let Some(mut p) = pw {
        secrets::wipe(&mut p);
    }
    match r {
        Ok(x) => x,
        Err(e) => {
            if e.starts_with("HOST KEY MISMATCH") || e.starts_with("unknown host") {
                super::notify::notice(cx.owner, if e.starts_with("HOST") { "SSH host key MISMATCH" } else { "New SSH host: confirm it" }, &first_line(&e), true, true);
            }
            fail(e)
        },
    }
}

// ── dry run ──────────────────────────────────────────────────────────────────

/// What a run would do, without doing it.
pub fn dry(owner: &str, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let job = match lcron::parse(text) {
        Ok(j) => j,
        Err(e) => {
            out.push(String::from("Not runnable -- fix these first:"));
            for x in e {
                out.push(format!("  line {}: {}", x.line, x.msg));
            }
            return out;
        },
    };
    let now = super::now_lm();
    out.push(format!("job \"{}\"  runs as {owner}", job.name));
    out.push(format!("timer: {}", job.trigger.describe()));
    let cl = job.trigger.cron_line();
    if !cl.is_empty() {
        out.push(format!("cron:  {cl}"));
    }
    let next: Vec<String> = job.trigger.next_n(now, 5).iter().map(|t| cron::fmt_rel(*t, now)).collect();
    if !next.is_empty() {
        out.push(format!("next:  {}", next.join(" · ")));
    }
    if job.at_boot {
        out.push(String::from(if job.needs_session() { "at boot: yes -- runs when you sign in (it needs your keyring)" } else { "at boot: yes -- runs at boot, before sign-in" }));
    }
    out.push(format!("if missed: {}", if job.missed == lcron::Missed::CatchUp { "catch up once" } else { "skip" }));
    if job.needs_session() {
        out.push(format!("needs {owner} signed in (keyring open){}", if super::secrets::available(owner) { " -- yes, now" } else { " -- not now: it would wait" }));
    }
    for name in job.secrets() {
        let have = match secrets::list(owner) {
            Ok(l) => l.iter().any(|(n, _, _)| *n == name),
            Err(_) => false,
        };
        out.push(format!("keyring(\"{name}\"): {}", if have { "present" } else if secrets::available(owner) { "MISSING -- add it in Keyring" } else { "unknown (keyring locked)" }));
    }
    for (i, st) in job.steps.iter().enumerate() {
        let cond = st.when.as_ref().map(|c| format!("   when {}", c.show())).unwrap_or_default();
        out.push(format!("{}. {} = {}{cond}", i + 1, st.name, lcron::show_call(&st.call)));
        if let Some(t) = match st.call.action.as_str() {
            "sftp.put" => st.call.arg(1, "target"),
            "ssh.run" => st.call.arg(0, "target"),
            _ => None,
        } {
            if let Val::Str(t) = t {
                if let Some((_, host, port, _)) = lcron::parse_target(t) {
                    let pin = secrets::pinned(owner, &host, port);
                    out.push(format!("     host {host}:{port}: {}", match pin {
                        Some(b) => format!("pinned {}", secrets::fingerprint(&b)),
                        None => String::from("not pinned yet -- the first run asks you to confirm its key"),
                    }));
                }
            }
        }
    }
    for c in &job.on_fail {
        out.push(format!("on_fail: {}", lcron::show_call(c)));
    }
    for c in &job.on_ok {
        out.push(format!("on_ok:   {}", lcron::show_call(c)));
    }
    out.push(String::from("(dry run: nothing was done)"));
    out.iter().map(|s| s.to_string()).collect()
}
