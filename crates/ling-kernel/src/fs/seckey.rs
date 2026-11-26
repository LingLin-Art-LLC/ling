//! Security keys at sign-in: FIDO USB keys (YubiKey, SoloKey, Titan,
//! Feitian, Nitrokey...), spoken to with FIDO U2F over CTAPHID (the
//! `ling-fido` crate) through the xHCI driver's interrupt endpoints.
//! Nothing goes over the network: the key signs a fresh random challenge and
//! the public key saved when the key was added checks the signature.
//!
//! Per account, `seckey/<user>` in lingfs:
//!   app:<hex 32>                         this account's U2F application id
//!                                        (random, so a key handle made here
//!                                        is useless on any other machine)
//!   key:<counter>:<hex handle>:<hex public key>:<name>
//!   rc:<hex sha256>                      one line per unused recovery code
//! While the file lists a key, every sign-in path asks for one of them after
//! the password (and after the authenticator code when 2FA is on too); a
//! recovery code stands in for a lost key.
//!
//! Talking to a key takes a touch, so both adding a key and signing in are
//! step functions the UI calls every frame: `*_begin` once, then `poll`
//! until it says `Done` or `Failed`.

use crate::fs::lingfs::{self, BLOCK_SIZE};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ling_fido as fido;
use sha2::{Digest, Sha256};

const DIR: &str = "seckey";
/// Keys per account (the record must fit one lingfs block).
pub const MAX_KEYS: usize = 4;
const RECOVERY_CODES: usize = 8;
/// How long a touch is waited for.
const TOUCH_TIMEOUT_MS: u64 = 60_000;
/// Ask the key at most this often while waiting for a touch.
const POLL_MS: u64 = 150;

pub struct Key {
    pub name: String,
    pub counter: u32,
    pub handle: Vec<u8>,
    pub public_key: Vec<u8>,
}

struct Rec {
    app: [u8; 32],
    keys: Vec<Key>,
    recovery: Vec<String>,
}

fn valid_user(user: &str) -> bool {
    !user.is_empty() && !user.contains('/') && !user.contains('\n')
}

fn load(user: &str) -> Option<Rec> {
    if !valid_user(user) {
        return None;
    }
    let mut buf = [0u8; BLOCK_SIZE];
    let n = lingfs::read_file(&format!("{DIR}/{user}"), &mut buf).ok().flatten()?;
    let text = core::str::from_utf8(&buf[..n]).ok()?;
    let mut rec = Rec { app: [0; 32], keys: Vec::new(), recovery: Vec::new() };
    let mut have_app = false;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("app:") {
            let b = unhex(v.trim())?;
            if b.len() != 32 {
                return None;
            }
            rec.app.copy_from_slice(&b);
            have_app = true;
        } else if let Some(v) = line.strip_prefix("key:") {
            let mut it = v.splitn(4, ':');
            let (Some(c), Some(h), Some(p), Some(name)) = (it.next(), it.next(), it.next(), it.next()) else { continue };
            let (Ok(counter), Some(handle), Some(public_key)) = (c.parse::<u32>(), unhex(h), unhex(p)) else { continue };
            rec.keys.push(Key { name: String::from(name), counter, handle, public_key });
        } else if let Some(v) = line.strip_prefix("rc:") {
            rec.recovery.push(String::from(v.trim()));
        }
    }
    have_app.then_some(rec)
}

fn save(user: &str, rec: &Rec) -> bool {
    if rec.keys.is_empty() {
        return lingfs::remove_path(&format!("{DIR}/{user}")).unwrap_or(false);
    }
    let mut s = format!("app:{}\n", hex(&rec.app));
    for k in &rec.keys {
        s.push_str(&format!("key:{}:{}:{}:{}\n", k.counter, hex(&k.handle), hex(&k.public_key), k.name));
    }
    for h in &rec.recovery {
        s.push_str("rc:");
        s.push_str(h);
        s.push('\n');
    }
    if s.len() > BLOCK_SIZE {
        return false;
    }
    lingfs::write_in_dir(DIR, user, s.as_bytes()).is_ok()
}

/// Does `user` sign in with a security key as well?
pub fn enabled(user: &str) -> bool {
    load(user).is_some_and(|r| !r.keys.is_empty())
}

/// The names of `user`'s keys, in the order they were added.
pub fn names(user: &str) -> Vec<String> {
    load(user).map(|r| r.keys.into_iter().map(|k| k.name).collect()).unwrap_or_default()
}

/// Unused recovery codes left.
pub fn recovery_left(user: &str) -> usize {
    load(user).map(|r| r.recovery.len()).unwrap_or(0)
}

/// Forget key number `i`. Removing the last key turns security-key sign-in
/// off (and drops its recovery codes).
pub fn remove(user: &str, i: usize) -> bool {
    let Some(mut rec) = load(user) else { return false };
    if i >= rec.keys.len() {
        return false;
    }
    let k = rec.keys.remove(i);
    if rec.keys.is_empty() {
        rec.recovery.clear();
    }
    let ok = save(user, &rec);
    if ok {
        crate::console_write(format!("seckey: removed \"{}\" from {user}\n", k.name).as_bytes());
    }
    ok
}

/// Rename key number `i`.
pub fn rename(user: &str, i: usize, name: &str) -> bool {
    let Some(mut rec) = load(user) else { return false };
    let Some(k) = rec.keys.get_mut(i) else { return false };
    k.name = clean_name(name);
    save(user, &rec)
}

fn clean_name(name: &str) -> String {
    let n: String = name.chars().filter(|c| *c != ':' && *c != '\n' && *c != '\r').take(32).collect();
    let n = String::from(n.trim());
    if n.is_empty() {
        String::from("Security key")
    } else {
        n
    }
}

// ── Talking to the key ──────────────────────────────────────────────────────

pub(crate) struct Usb;

impl fido::Transport for Usb {
    fn send(&mut self, pkt: &[u8; fido::PKT]) -> Result<(), fido::Error> {
        if crate::arch::xhci::fido_send(pkt) {
            Ok(())
        } else {
            Err(fido::Error::Io)
        }
    }
    fn recv(&mut self, pkt: &mut [u8; fido::PKT], timeout_ms: u32) -> Result<(), fido::Error> {
        // In short waits: the IN transfer stays queued between them (xhci keeps it pending and records
        // its completion), so a background task can hand the CPU back in between. A key that holds its
        // answer until it is touched (keepalives for seconds) used to hold the desktop's frame as long.
        let end = crate::arch::timer::now_ms() + timeout_ms as u64;
        loop {
            // an app-key request that was cancelled (fs::appkeys) stops waiting at once
            if crate::fs::appkeys::aborting() {
                return Err(fido::Error::Timeout);
            }
            if crate::arch::xhci::fido_recv(pkt, 2) {
                return Ok(());
            }
            if crate::arch::timer::now_ms() >= end {
                return Err(fido::Error::Timeout);
            }
            if !crate::proc::sched::is_main() {
                crate::proc::sched::yield_now();
            }
        }
    }
}

/// The open CTAPHID channel and the plug-in generation it was opened for.
static mut DEV: Option<(u32, fido::Device<Usb>)> = None;

pub(crate) fn device() -> Option<&'static mut fido::Device<Usb>> {
    let slot = unsafe { &mut *(&raw mut DEV) };
    if !crate::arch::xhci::fido_present() {
        *slot = None;
        return None;
    }
    let gen = crate::arch::xhci::fido_generation();
    if slot.as_ref().map_or(true, |(g, _)| *g != gen) {
        let mut nonce = [0u8; 8];
        crate::crypto::random_bytes(&mut nonce);
        match fido::Device::open(Usb, nonce) {
            Ok(mut d) => {
                let _ = d.wink();
                *slot = Some((gen, d));
            },
            Err(_) => {
                *slot = None;
                return None;
            },
        }
    }
    slot.as_mut().map(|(_, d)| d)
}

/// Forget the open channel (a request was cancelled mid-way): the next
/// [`device`] opens a fresh one, so a late answer to the old request is ignored.
pub(crate) fn reset_device() {
    unsafe { *(&raw mut DEV) = None };
}

/// A sign-in / key add / SSH approval is in progress.
pub(crate) fn busy() -> bool {
    !matches!(op(), Op::Idle)
}

/// What a step function reports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// No security key is plugged in.
    PlugIn,
    /// The key is there: touch it.
    Touch,
    Done,
    Failed(&'static str),
}

impl Step {
    /// For Ling: 2 plug in, 1 touch, 0 done, -1 failed.
    pub fn code(self) -> i32 {
        match self {
            Step::PlugIn => 2,
            Step::Touch => 1,
            Step::Done => 0,
            Step::Failed(_) => -1,
        }
    }
}

enum Op {
    Idle,
    Add { user: String, name: String, challenge: [u8; 32], started: u64, last: u64 },
    Auth {
        user: String,
        challenge: [u8; 32],
        started: u64,
        last: u64,
        /// Which of the account's keys is plugged in, once known.
        which: Option<usize>,
    },
}

static mut OP: Op = Op::Idle;
/// Bumped whenever an operation starts or is cancelled: a step the key task
/// finishes for an older one is dropped.
static mut OP_GEN: u32 = 0;
/// The key task's latest step for operation `OP_GEN`.
static mut STEP_RESULT: Option<(u32, Step)> = None;
/// 0 = not started, 1 = running, 2 = no task slot (steps run in the caller).
static mut TASK: u8 = 0;

fn op_changed() {
    unsafe {
        OP_GEN = OP_GEN.wrapping_add(1);
        STEP_RESULT = None;
    }
}

/// The key's USB conversation runs on a background task of its own: a
/// sign-in waiting for a touch called the key from the desktop's frame, and
/// each call held the frame 150 ms to 3 s (the key answers only once touched).
extern "C" fn seckey_task() {
    loop {
        if !matches!(op(), Op::Idle) {
            let g = unsafe { OP_GEN };
            let s = poll_inner();
            unsafe {
                if g == OP_GEN {
                    STEP_RESULT = Some((g, s));
                }
            }
        }
        // the apps' security-key requests (wallet / messenger unlock) run here too
        crate::fs::appkeys::task_turn();
        crate::proc::sched::yield_now();
    }
}

pub(crate) fn ensure_task() -> bool {
    unsafe {
        if TASK == 0 {
            let slot = crate::proc::sched::spawn(seckey_task as *const () as usize as u64);
            if slot == u64::MAX {
                TASK = 2;
            } else {
                crate::proc::sched::set_name(slot, "(security key)");
                TASK = 1;
            }
        }
        TASK == 1
    }
}

/// One step, from the key task when there is one (non-blocking), else here.
fn poll_step() -> Step {
    if !ensure_task() {
        return poll_inner();
    }
    // give the key task its turn now (it hands back within a few ms)
    crate::proc::sched::yield_now();
    unsafe {
        match STEP_RESULT {
            Some((g, s)) if g == OP_GEN => {
                if matches!(s, Step::Done | Step::Failed(_)) {
                    STEP_RESULT = None;
                }
                s
            },
            _ => {
                if matches!(op(), Op::Idle) {
                    Step::Failed(last_error())
                } else {
                    Step::Touch
                }
            },
        }
    }
}
/// The Auth in `OP` approves a remote SSH key (`fs::sshkeys`), not a sign-in:
/// only [`approve_poll`] may finish it.
static mut APPROVAL: bool = false;
static mut LAST_FAIL: &str = "";
/// Recovery codes made when the first key was added, shown once.
static mut FRESH_CODES: Vec<String> = Vec::new();

fn op() -> &'static mut Op {
    unsafe { &mut *(&raw mut OP) }
}

fn fail(msg: &'static str) -> Step {
    unsafe { LAST_FAIL = msg };
    *op() = Op::Idle;
    Step::Failed(msg)
}

/// Why the last step failed, in words for the screen.
pub fn last_error() -> &'static str {
    unsafe { LAST_FAIL }
}

fn challenge() -> Option<[u8; 32]> {
    let mut c = [0u8; 32];
    crate::crypto::random_bytes(&mut c).then_some(c)
}

/// Start adding a key to `user` (Settings). Then call [`poll`].
pub fn add_begin(user: &str, name: &str) -> bool {
    if !valid_user(user) || load(user).is_some_and(|r| r.keys.len() >= MAX_KEYS) {
        unsafe { LAST_FAIL = "this account already has the most keys it can hold -- remove one first" };
        return false;
    }
    let Some(c) = challenge() else { return false };
    let now = crate::arch::timer::now_ms();
    unsafe { APPROVAL = false };
    op_changed();
    *op() = Op::Add { user: String::from(user), name: clean_name(name), challenge: c, started: now, last: 0 };
    true
}

/// Start a sign-in check for `user`. Then call [`poll`].
pub fn auth_begin(user: &str) -> bool {
    if !enabled(user) {
        return false;
    }
    let Some(c) = challenge() else { return false };
    let now = crate::arch::timer::now_ms();
    unsafe { APPROVAL = false };
    op_changed();
    *op() = Op::Auth { user: String::from(user), challenge: c, started: now, last: 0, which: None };
    true
}

/// Start the touch that approves a remote SSH login for `user` (the desktop's
/// approval box, `fs::sshkeys`): the same check as signing in, a fresh
/// challenge signed by one of `user`'s keys. Then call [`approve_poll`].
pub fn approve_begin(user: &str) -> bool {
    if !auth_begin(user) {
        return false;
    }
    unsafe { APPROVAL = true };
    true
}

/// One step of an approval started with [`approve_begin`].
pub fn approve_poll() -> Step {
    if !unsafe { APPROVAL } {
        return Step::Failed("the key request was interrupted -- try again");
    }
    let s = poll_step();
    if matches!(s, Step::Done | Step::Failed(_)) {
        unsafe { APPROVAL = false };
    }
    s
}

/// Stop waiting (the user pressed Cancel / Esc).
pub fn cancel() {
    unsafe { APPROVAL = false };
    op_changed();
    *op() = Op::Idle;
}

/// The recovery codes made by the add that just finished (first key only),
/// handed out once.
pub fn take_fresh_codes() -> Vec<String> {
    core::mem::take(unsafe { &mut *(&raw mut FRESH_CODES) })
}

/// One step of whatever is in progress (a sign-in or adding a key; an SSH
/// approval answers only to [`approve_poll`]).
pub fn poll() -> Step {
    if unsafe { APPROVAL } {
        return Step::Failed("the security key is busy approving a remote SSH login -- finish that first");
    }
    poll_step()
}

fn poll_inner() -> Step {
    let now = crate::arch::timer::now_ms();
    let (started, last) = match op() {
        Op::Idle => return Step::Failed(last_error()),
        Op::Add { started, last, .. } | Op::Auth { started, last, .. } => (*started, *last),
    };
    if now.saturating_sub(started) > TOUCH_TIMEOUT_MS {
        return fail("no touch in time -- try again");
    }
    let Some(dev) = device() else { return Step::PlugIn };
    if now.saturating_sub(last) < POLL_MS {
        return Step::Touch;
    }
    match op() {
        Op::Add { last, .. } | Op::Auth { last, .. } => *last = now,
        Op::Idle => {},
    }
    match op() {
        Op::Add { user, name, challenge, .. } => {
            let (user, name, challenge) = (user.clone(), name.clone(), *challenge);
            add_step(dev, &user, &name, &challenge)
        },
        Op::Auth { user, challenge, which, .. } => {
            let (user, challenge, w) = (user.clone(), *challenge, *which);
            auth_step(dev, &user, &challenge, w)
        },
        Op::Idle => Step::Failed(last_error()),
    }
}

fn add_step(dev: &mut fido::Device<Usb>, user: &str, name: &str, challenge: &[u8; 32]) -> Step {
    let mut rec = match load(user) {
        Some(r) => r,
        None => {
            let mut app = [0u8; 32];
            if !crate::crypto::random_bytes(&mut app) {
                return fail("no random numbers -- cannot make a key");
            }
            Rec { app, keys: Vec::new(), recovery: Vec::new() }
        },
    };
    // the same physical key twice would only waste a slot
    for k in &rec.keys {
        if let Ok(true) = fido::owns(dev, &rec.app, &k.handle) {
            return fail("this key is already added to your account");
        }
    }
    match fido::register(dev, challenge, &rec.app) {
        Err(fido::Error::NeedTouch) => Step::Touch,
        Err(_) => fail("the key did not answer -- unplug it, plug it back in and try again"),
        Ok(reg) => {
            let first = rec.keys.is_empty();
            rec.keys.push(Key { name: String::from(name), counter: 0, handle: reg.key_handle, public_key: reg.public_key.to_vec() });
            let mut codes = Vec::new();
            if first {
                rec.recovery.clear();
                for _ in 0..RECOVERY_CODES {
                    let mut raw = [0u8; 5];
                    if !crate::crypto::random_bytes(&mut raw) {
                        return fail("no random numbers -- cannot make recovery codes");
                    }
                    let c = base32(&raw);
                    rec.recovery.push(hash_code(&c));
                    codes.push(format!("{}-{}", &c[..4], &c[4..]));
                }
            }
            if !save(user, &rec) {
                return fail("could not save the key (storage full?)");
            }
            unsafe { FRESH_CODES = codes };
            crate::console_write(format!("seckey: added \"{name}\" to {user}\n").as_bytes());
            *op() = Op::Idle;
            Step::Done
        },
    }
}

fn auth_step(dev: &mut fido::Device<Usb>, user: &str, challenge: &[u8; 32], which: Option<usize>) -> Step {
    let Some(mut rec) = load(user) else { return fail("this account has no security key") };
    let which = match which {
        Some(i) if i < rec.keys.len() => i,
        _ => {
            let mut found = None;
            for (i, k) in rec.keys.iter().enumerate() {
                match fido::owns(dev, &rec.app, &k.handle) {
                    Ok(true) => {
                        found = Some(i);
                        break;
                    },
                    Ok(false) => {},
                    Err(_) => return fail("the key did not answer -- unplug it and plug it back in"),
                }
            }
            let Some(i) = found else { return fail("that is not one of this account's security keys") };
            if let Op::Auth { which, .. } = op() {
                *which = Some(i);
            }
            i
        },
    };
    let k = &rec.keys[which];
    match fido::authenticate(dev, challenge, &rec.app, &k.handle) {
        Err(fido::Error::NeedTouch) => Step::Touch,
        Err(fido::Error::WrongKey) => fail("that is not one of this account's security keys"),
        Err(_) => fail("the key did not answer -- unplug it and plug it back in"),
        Ok(a) => {
            if !fido::verify(&k.public_key, &rec.app, challenge, &a) {
                return fail("the key's signature did not check out");
            }
            // A genuine key's counter only goes up; a copy of it lags behind.
            if a.counter != 0 && a.counter <= k.counter {
                crate::console_write(format!("seckey: counter went back for {user}'s \"{}\" -- refused\n", k.name).as_bytes());
                return fail("this key's counter went backwards -- it may be a copy; refused");
            }
            rec.keys[which].counter = a.counter;
            let _ = save(user, &rec);
            *op() = Op::Idle;
            Step::Done
        },
    }
}

/// Sign in with a recovery code instead of the key (used up by this call).
/// Throttled like the 2FA codes.
pub fn use_recovery(user: &str, code: &str) -> bool {
    if crate::fs::twofa::locked_for_ms(user) > 0 {
        return false;
    }
    let Some(mut rec) = load(user) else { return false };
    let norm: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect();
    let h = hash_code(&norm);
    let ok = match rec.recovery.iter().position(|r| *r == h) {
        Some(i) => {
            rec.recovery.remove(i);
            save(user, &rec)
        },
        None => false,
    };
    crate::fs::twofa::note_attempt_pub(user, ok);
    if ok {
        crate::console_write(format!("seckey: {user} signed in with a recovery code ({} left)\n", rec.recovery.len()).as_bytes());
    }
    ok
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn hash_code(code: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"lingos-seckey-recovery:");
    h.update(code.as_bytes());
    hex(&h.finalize())
}

fn base32(data: &[u8]) -> String {
    const A: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(A[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(A[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
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
    if s.len() % 2 != 0 {
        return None;
    }
    let v = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    s.as_bytes().chunks(2).map(|p| Some((v(p[0])? << 4) | v(p[1])?)).collect()
}
