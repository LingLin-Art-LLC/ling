//! SSH logins approved with a security key.
//!
//! The connecting computer has its own FIDO key and an OpenSSH security-key
//! key pair (`ssh-keygen -t ed25519-sk` / `-t ecdsa-sk`); every login it signs
//! with a touch on THAT key (`drivers::sshd`, `ling_fido::ssh`). The first time
//! a given key tries an account that has a security key of its own here
//! (`seckey::enabled`), the login waits while the LingOS desktop shows who is
//! asking; the person at this machine allows it with a touch of one of the
//! account's registered keys. From then on that key logs in with only its own
//! touch on the client.
//!
//! Per account, `sshkeys/<user>` in lingfs, one line per approved key:
//!   k:<kind>:<counter>:<uv 0|1>:<approved unix s>:<last used unix s>:<hex blob>:<label>
//! (`kind` is `ed25519-sk` / `ecdsa-sk`; `label` says where and when it was
//! approved.) The file sits outside `home/`, so SFTP and the SSH file commands
//! cannot write it: the only way a key gets in is [`commit`], after a touch.
//!
//! Accounts without a security key of their own never take publickey logins
//! ([`gate`]): they keep the password (+ authenticator code) logins.
//!
//! Test builds: a kernel compiled with the environment variable
//! `LINGOS_SSHKEY_TEST_TOUCH` set (QEMU has no FIDO key) treats every account
//! as having a key and simulates the approving touch when no key is plugged
//! in. Normal builds never set it; such a kernel says so loudly on the
//! console and on the approval box.

use crate::fs::lingfs;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ling_fido::ssh as sk;

const DIR: &str = "sshkeys";
/// Approved keys per account.
pub const MAX_KEYS: usize = 16;
/// How long a login waits for someone at this machine.
pub const APPROVE_TIMEOUT_MS: u64 = 90_000;
/// After a Deny, the same account's requests are ignored this long (floods).
const DENY_COOLDOWN_MS: u64 = 60_000;
/// After an unanswered request, a short pause before the next one.
const TIMEOUT_COOLDOWN_MS: u64 = 10_000;
/// An approval must be used (the client's signed login) within this long.
const APPROVED_TTL_MS: u64 = 5 * 60_000;

/// Set at compile time only for the QEMU end-to-end test.
pub const TEST_TOUCH: bool = option_env!("LINGOS_SSHKEY_TEST_TOUCH").is_some();

pub struct Approved {
    pub kind: sk::Kind,
    pub blob: Vec<u8>,
    pub label: String,
    pub approved: u64,
    pub last_used: u64,
    pub counter: u32,
    pub uv: bool,
}

impl Approved {
    pub fn fingerprint(&self) -> String {
        sk::fingerprint(&self.blob)
    }
}

fn valid_user(user: &str) -> bool {
    !user.is_empty() && !user.contains('/') && !user.contains('\n') && !user.contains(':')
}

fn kind_from_short(s: &str) -> Option<sk::Kind> {
    match s {
        "ed25519-sk" => Some(sk::Kind::Ed25519Sk),
        "ecdsa-sk" => Some(sk::Kind::EcdsaSk),
        _ => None,
    }
}

/// `user`'s approved keys, oldest first.
pub fn list(user: &str) -> Vec<Approved> {
    let mut out = Vec::new();
    if !valid_user(user) {
        return out;
    }
    let mut buf = alloc::vec![0u8; 16 * 1024];
    let Ok(Some(n)) = lingfs::read_file_all(&format!("/{DIR}/{user}"), &mut buf) else { return out };
    let Ok(text) = core::str::from_utf8(&buf[..n]) else { return out };
    for line in text.lines() {
        let Some(v) = line.strip_prefix("k:") else { continue };
        let f: Vec<&str> = v.splitn(7, ':').collect();
        if f.len() != 7 {
            continue;
        }
        let (Some(kind), Ok(counter), Ok(approved), Ok(last_used), Some(blob)) =
            (kind_from_short(f[0]), f[1].parse::<u32>(), f[3].parse::<u64>(), f[4].parse::<u64>(), unhex(f[5]))
        else {
            continue;
        };
        let label = f[6];
        // a blob that no longer parses as its kind is dropped, not trusted
        if sk::parse_public_key(&blob).map(|p| p.kind) != Some(kind) {
            continue;
        }
        out.push(Approved { kind, blob, label: String::from(label), approved, last_used, counter, uv: f[2] == "1" });
    }
    out
}

fn save(user: &str, keys: &[Approved]) -> bool {
    if !valid_user(user) {
        return false;
    }
    if keys.is_empty() {
        return lingfs::remove_path(&format!("/{DIR}/{user}")).is_ok();
    }
    let mut s = String::new();
    for k in keys {
        s.push_str(&format!(
            "k:{}:{}:{}:{}:{}:{}:{}\n",
            k.kind.short(),
            k.counter,
            if k.uv { 1 } else { 0 },
            k.approved,
            k.last_used,
            hex(&k.blob),
            clean(&k.label, 96)
        ));
    }
    lingfs::write_in_dir_any(DIR, user, s.as_bytes()).is_ok()
}

/// Does `user` take publickey logins at all? Only accounts with a security
/// key registered here (which is what approves a new SSH key).
pub fn gate(user: &str) -> bool {
    if !valid_user(user) {
        return false;
    }
    let mut g = [0u8; 32];
    if crate::fs::users::group_of(user, &mut g).is_none() {
        return false;
    }
    crate::fs::seckey::enabled(user) || TEST_TOUCH
}

/// Is this public key blob approved for `user`?
pub fn is_approved(user: &str, blob: &[u8]) -> bool {
    list(user).iter().any(|k| k.blob == blob)
}

/// A genuine FIDO key's counter only goes up. Keys that do not count send 0
/// every time; that is allowed only while the stored counter is 0 too.
pub fn counter_ok(stored: u32, got: u32) -> bool {
    if got == 0 {
        stored == 0
    } else {
        got > stored
    }
}

/// An approved key just signed a login: check its counter, remember the use.
pub fn record_use(user: &str, blob: &[u8], counter: u32, uv: bool) -> Result<(), &'static str> {
    let mut keys = list(user);
    let Some(k) = keys.iter_mut().find(|k| k.blob == blob) else { return Err("this key is not approved for that account") };
    if !counter_ok(k.counter, counter) {
        crate::console_write(format!("sshkeys: {user}: counter went back ({} after {}) -- refused, the key may be cloned\n", counter, k.counter).as_bytes());
        return Err("the key's signature counter went backwards (a cloned key?)");
    }
    k.counter = counter;
    k.uv = uv;
    k.last_used = crate::clock::unix_now();
    // a failed save only loses the bookkeeping; the counter check still had its say
    let _ = save(user, &keys);
    Ok(())
}

/// Forget approved key number `i`.
pub fn remove(user: &str, i: usize) -> bool {
    let mut keys = list(user);
    if i >= keys.len() {
        return false;
    }
    let k = keys.remove(i);
    let ok = save(user, &keys);
    if ok {
        crate::console_write(format!("sshkeys: {user}: removed {} ({})\n", k.fingerprint(), k.label).as_bytes());
    }
    ok
}

/// Forget the approved key whose fingerprint starts with `fp` (one match only).
pub fn remove_fp(user: &str, fp: &str) -> Result<String, &'static str> {
    let keys = list(user);
    let want = fp.trim_start_matches("SHA256:");
    if want.len() < 4 {
        return Err("give at least 4 characters of the fingerprint");
    }
    let hits: Vec<usize> = keys.iter().enumerate().filter(|(_, k)| k.fingerprint()[7..].starts_with(want)).map(|(i, _)| i).collect();
    match hits.as_slice() {
        [] => Err("no approved key has that fingerprint"),
        [i] => {
            let f = keys[*i].fingerprint();
            if remove(user, *i) {
                Ok(f)
            } else {
                Err("could not save (storage not writable?)")
            }
        },
        _ => Err("more than one key matches -- give more of the fingerprint"),
    }
}

// ── the approval request ─────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReqState {
    Waiting,
    Approved,
    Denied,
    TimedOut,
    /// The request is no longer there (the client left, or another replaced it).
    Gone,
}

struct Request {
    id: u32,
    user: String,
    ip: String,
    kind: sk::Kind,
    blob: Vec<u8>,
    version: String,
    started: u64,
    state: ReqState,
    decided: u64,
}

/// One request at a time: the SSH server serves one connection at a time
/// anyway, and a second login while one waits is refused.
static mut REQ: Option<Request> = None;
static mut NEXT_ID: u32 = 1;
/// (user, until ms): requests for these accounts are ignored for a while.
static mut COOLDOWN: Vec<(String, u64)> = Vec::new();

fn req() -> &'static mut Option<Request> {
    unsafe { &mut *(&raw mut REQ) }
}

fn cooldown() -> &'static mut Vec<(String, u64)> {
    unsafe { &mut *(&raw mut COOLDOWN) }
}

fn cool(user: &str, ms: u64) {
    let until = crate::arch::timer::now_ms() + ms;
    let c = cooldown();
    c.retain(|(u, _)| u != user);
    c.push((String::from(user), until));
}

static mut WARNED: bool = false;

/// A login with a not-yet-approved key asks the desktop. Err says why it
/// cannot (in words for the SSH banner).
pub fn request(user: &str, ip: &str, kind: sk::Kind, blob: &[u8], version: &str) -> Result<u32, &'static str> {
    let now = crate::arch::timer::now_ms();
    if TEST_TOUCH && !unsafe { WARNED } {
        unsafe { WARNED = true };
        crate::console_write(b"sshkeys: TEST BUILD (LINGOS_SSHKEY_TEST_TOUCH) -- approving touches are SIMULATED, never ship this kernel\n");
    }
    if cooldown().iter().any(|(u, t)| u == user && now < *t) {
        return Err("A login for this account was just denied or went unanswered; try again in a minute.");
    }
    if let Some(r) = req() {
        if r.state == ReqState::Waiting && !expired(r, now) {
            return Err("Another login is already waiting for approval on the LingOS PC.");
        }
    }
    if list(user).len() >= MAX_KEYS {
        return Err("This account already has the most approved SSH keys it can hold; remove one in Settings > Keys.");
    }
    let id = unsafe {
        let id = NEXT_ID;
        NEXT_ID = NEXT_ID.wrapping_add(1).max(1);
        id
    };
    *req() = Some(Request {
        id,
        user: String::from(user),
        ip: String::from(ip),
        kind,
        blob: blob.to_vec(),
        version: clean(version, 64),
        started: now,
        state: ReqState::Waiting,
        decided: 0,
    });
    crate::console_write(format!("sshkeys: approval asked for {user} from {ip}, {} {}\n", kind.short(), sk::fingerprint(blob)).as_bytes());
    Ok(id)
}

fn expired(r: &Request, now: u64) -> bool {
    now.saturating_sub(r.started) > APPROVE_TIMEOUT_MS
}

/// Where request `id` stands (the SSH session polls this while it waits).
pub fn state(id: u32) -> ReqState {
    let now = crate::arch::timer::now_ms();
    let Some(r) = req().as_mut().filter(|r| r.id == id) else { return ReqState::Gone };
    if r.state == ReqState::Waiting && expired(r, now) {
        r.state = ReqState::TimedOut;
        let u = r.user.clone();
        cool(&u, TIMEOUT_COOLDOWN_MS);
        touch_drop_for(id);
    }
    if r.state == ReqState::Approved && now.saturating_sub(r.decided) > APPROVED_TTL_MS {
        return ReqState::Gone;
    }
    r.state
}

/// The SSH client went away (or the session is done with the request).
pub fn cancel(id: u32) {
    if req().as_ref().is_some_and(|r| r.id == id) {
        *req() = None;
        touch_drop_for(id);
    }
}

/// The approved key proved itself (the client's signature checked out):
/// store it for `user`. Only an approval made with a touch here gets this far.
pub fn commit(id: u32, user: &str, blob: &[u8], counter: u32, uv: bool) -> bool {
    if state(id) != ReqState::Approved {
        return false;
    }
    let Some(r) = req().take() else { return false };
    if r.user != user || r.blob != blob {
        return false;
    }
    let mut keys = list(user);
    if keys.iter().any(|k| k.blob == blob) {
        return true;
    }
    let now = crate::clock::unix_now();
    // the date only: labels are stored without ':' (the field separator), so "20:46" would read "2046"
    let day = date(now);
    let label = format!("{} on {}, {}", r.ip, day.get(..10).unwrap_or(&day), r.version);
    keys.push(Approved { kind: r.kind, blob: r.blob.clone(), label, approved: now, last_used: now, counter, uv });
    let ok = save(user, &keys);
    crate::console_write(
        format!("sshkeys: {user}: {} {} from {} approved{}\n", r.kind.short(), sk::fingerprint(blob), r.ip, if ok { "" } else { " -- but could not be saved" })
            .as_bytes(),
    );
    ok
}

/// For the desktop: the request waiting for someone here, if any.
pub struct Pending {
    pub id: u32,
    pub user: String,
    pub ip: String,
    pub kind: &'static str,
    pub fingerprint: String,
    pub version: String,
    pub secs_left: u64,
}

pub fn pending() -> Option<Pending> {
    let now = crate::arch::timer::now_ms();
    let r = req().as_ref()?;
    let id = r.id;
    if state(id) != ReqState::Waiting {
        return None;
    }
    let r = req().as_ref()?;
    Some(Pending {
        id,
        user: r.user.clone(),
        ip: r.ip.clone(),
        kind: r.kind.short(),
        fingerprint: sk::fingerprint(&r.blob),
        version: r.version.clone(),
        secs_left: (APPROVE_TIMEOUT_MS.saturating_sub(now.saturating_sub(r.started)) + 999) / 1000,
    })
}

/// Deny request `id` (the Deny button / Esc).
pub fn deny(id: u32) {
    let Some(r) = req().as_mut().filter(|r| r.id == id && r.state == ReqState::Waiting) else { return };
    r.state = ReqState::Denied;
    r.decided = crate::arch::timer::now_ms();
    let u = r.user.clone();
    cool(&u, DENY_COOLDOWN_MS);
    touch_drop_for(id);
    crate::console_write(format!("sshkeys: {u}: login denied at the desktop\n").as_bytes());
}

// ── the touch here (approve a request, or remove an approved key) ────────────

enum Touch {
    None,
    Allow { id: u32, started: u64 },
    Remove { user: String, blob: Vec<u8>, started: u64 },
}

static mut TOUCH: Touch = Touch::None;
static mut TOUCH_ERR: &str = "";

fn touch() -> &'static mut Touch {
    unsafe { &mut *(&raw mut TOUCH) }
}

fn touch_drop_for(id: u32) {
    if matches!(touch(), Touch::Allow { id: t, .. } if *t == id) {
        *touch() = Touch::None;
        crate::fs::seckey::cancel();
    }
}

fn touch_fail(why: &'static str) -> i32 {
    unsafe { TOUCH_ERR = why };
    *touch() = Touch::None;
    -1
}

/// Why the last touch failed.
pub fn touch_error() -> &'static str {
    unsafe { TOUCH_ERR }
}

/// A simulated touch (test builds, no key plugged in) lands after this long.
const SIM_TOUCH_MS: u64 = 1_500;

fn simulated() -> bool {
    TEST_TOUCH && !crate::arch::xhci::fido_present()
}

/// "Allow": start the touch that approves request `id`. Then [`touch_poll`].
pub fn allow_begin(id: u32) -> bool {
    let Some(user) = req().as_ref().filter(|r| r.id == id && r.state == ReqState::Waiting).map(|r| r.user.clone()) else {
        unsafe { TOUCH_ERR = "that login is no longer waiting" };
        return false;
    };
    if !simulated() && !crate::fs::seckey::approve_begin(&user) {
        unsafe { TOUCH_ERR = "this account has no security key here to approve with" };
        return false;
    }
    *touch() = Touch::Allow { id, started: crate::arch::timer::now_ms() };
    true
}

/// "Remove" in Settings: start the touch that removes approved key `i`.
pub fn remove_begin(user: &str, i: usize) -> bool {
    let Some(k) = list(user).into_iter().nth(i) else {
        unsafe { TOUCH_ERR = "no such key" };
        return false;
    };
    if !crate::fs::seckey::enabled(user) && !TEST_TOUCH {
        // no key left to touch (the account's security keys were removed):
        // the approvals are inert anyway (`gate`), let them go without one
        unsafe { TOUCH_ERR = "" };
        *touch() = Touch::None;
        return remove(user, i);
    }
    if !simulated() && !crate::fs::seckey::approve_begin(user) {
        unsafe { TOUCH_ERR = "could not ask the security key" };
        return false;
    }
    *touch() = Touch::Remove { user: String::from(user), blob: k.blob, started: crate::arch::timer::now_ms() };
    true
}

/// One step of the touch: 2 plug the key in, 1 touch it, 0 done, -1 failed
/// ([`touch_error`]), 3 nothing in progress.
pub fn touch_poll() -> i32 {
    let started = match touch() {
        Touch::None => return 3,
        Touch::Allow { started, .. } | Touch::Remove { started, .. } => *started,
    };
    let step = if simulated() {
        if crate::arch::timer::now_ms().saturating_sub(started) < SIM_TOUCH_MS {
            crate::fs::seckey::Step::Touch
        } else {
            crate::console_write(b"sshkeys: TEST BUILD -- simulated security-key touch\n");
            crate::fs::seckey::Step::Done
        }
    } else {
        crate::fs::seckey::approve_poll()
    };
    match step {
        crate::fs::seckey::Step::Done => {},
        crate::fs::seckey::Step::Failed(why) => return touch_fail(why),
        s => return s.code(),
    }
    match core::mem::replace(touch(), Touch::None) {
        Touch::Allow { id, .. } => {
            let Some(r) = req().as_mut().filter(|r| r.id == id && r.state == ReqState::Waiting) else {
                return touch_fail("that login is no longer waiting");
            };
            r.state = ReqState::Approved;
            r.decided = crate::arch::timer::now_ms();
            crate::console_write(format!("sshkeys: {}: login from {} allowed with a touch\n", r.user, r.ip).as_bytes());
            0
        },
        Touch::Remove { user, blob, .. } => match list(&user).iter().position(|k| k.blob == blob) {
            Some(i) if remove(&user, i) => 0,
            Some(_) => touch_fail("could not save (storage not writable?)"),
            None => 0,
        },
        Touch::None => 3,
    }
}

/// Stop waiting for the touch (Cancel / Esc in the approval box goes back to Allow / Deny).
pub fn touch_cancel() {
    if !matches!(touch(), Touch::None) {
        *touch() = Touch::None;
        crate::fs::seckey::cancel();
    }
}

// ── `ssh-keys` (console, Terminal, SSH) ──────────────────────────────────────

pub fn command(arg: &str, user: &str) -> String {
    let mut w = arg.split_whitespace();
    match w.next().unwrap_or("list") {
        "list" | "ls" => {
            let keys = list(user);
            let mut s = format!(
                "SSH keys approved for {user}: {}{}\n",
                keys.len(),
                if gate(user) { "" } else { "  (inactive: this account has no security key, so SSH takes no keys)" }
            );
            for (i, k) in keys.iter().enumerate() {
                s.push_str(&format!(
                    "{:>2}  {:<10} {}\n    {}\n    approved {}, last used {}, fingerprint/PIN on the key: {}, counter {}\n",
                    i + 1,
                    k.kind.short(),
                    k.fingerprint(),
                    k.label,
                    date(k.approved),
                    date(k.last_used),
                    if k.uv { "yes" } else { "no" },
                    k.counter
                ));
            }
            if keys.is_empty() {
                s.push_str("  none. A new key is approved at the LingOS PC with a touch of this account's security key the first time it logs in.\n");
            }
            if TEST_TOUCH {
                s.push_str("  (TEST BUILD: approving touches are simulated)\n");
            }
            s
        },
        "remove" | "rm" | "del" => {
            let Some(which) = w.next() else { return String::from("usage: ssh-keys remove <number | fingerprint>\n") };
            let n = list(user).len();
            match which.parse::<usize>() {
                Ok(i) if i >= 1 && i <= n => {
                    if remove(user, i - 1) {
                        format!("removed key {i}\n")
                    } else {
                        String::from("could not save (storage not writable?)\n")
                    }
                },
                Ok(_) => format!("there are {n} approved keys\n"),
                Err(_) => match remove_fp(user, which) {
                    Ok(f) => format!("removed {f}\n"),
                    Err(e) => format!("ssh-keys: {e}\n"),
                },
            }
        },
        "add" => String::from(
            "ssh-keys: keys are not added here. Log in from the other computer with its security key (ssh-keygen -t ed25519-sk) and allow it at the LingOS PC with a touch of your key.\n",
        ),
        _ => String::from("usage: ssh-keys [list] | ssh-keys remove <number | fingerprint>\n"),
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn clean(s: &str, max: usize) -> String {
    let t: String = s.chars().filter(|c| *c != ':' && *c != '\n' && *c != '\r' && !c.is_control()).take(max).collect();
    String::from(t.trim())
}

/// "2026-11-14 09:30" (UTC) from Unix seconds; "never" for 0.
pub fn date(unix: u64) -> String {
    if unix == 0 {
        return String::from("never");
    }
    let days = (unix / 86_400) as i64;
    let rem = unix % 86_400;
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || s.is_empty() {
        return None;
    }
    let v = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    };
    s.as_bytes().chunks(2).map(|p| Some((v(p[0])? << 4) | v(p[1])?)).collect()
}
