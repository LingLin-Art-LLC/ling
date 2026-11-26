//! Security keys for apps: Ling Wallet and Ling Messenger can require one of
//! the user's FIDO USB keys before their vault opens.
//!
//! Each app gets, per account, a random 32-byte *app secret* S. When the
//! option is on, the app binds its own vault to S: it mixes S into the
//! password it hands its Argon2id vault key derivation (see the apps'
//! `akey_*` helpers), so the vault file cannot be opened with the app
//! password alone -- whoever has the file and the password still needs S.
//! The kernel hands S out only after one of these, LUKS-keyslot style
//! (any one of N):
//!
//! * a **key-bound slot** (FIDO2 / CTAP2 `hmac-secret`): S is wrapped with
//!   ChaCha20-Poly1305 under HKDF(HMAC-SHA-256(CredRandom, salt)), a secret
//!   only that physical key can recompute (with a touch). Mode "key-bound".
//! * a **touch-gated slot** (FIDO U2F, for keys without hmac-secret or that
//!   insist on a PIN): U2F signatures are not deterministic, so nothing can
//!   be derived from them. S stays in the sealed record below and is
//!   released after a fresh U2F assertion over a random challenge verifies
//!   against the key's public key (and its counter moved forward).
//! * **recovery with the account password** (unless the app's record is
//!   *strict*): the LingOS account password is checked again (throttled)
//!   and S is released from the sealed record.
//!
//! The record lives at `home/<user>/.appkeys/<app>`, sealed as a whole with
//! the keyring (`keyring::seal`, purpose "appkeys"): nothing in it is
//! readable without the account's keyring, which only the account password
//! opens. In strict mode the record holds no copy of S at all, only the
//! key-bound wraps; then losing every key leaves the app's own recovery
//! phrase (the wallet's 24 words) as the way back.
//!
//! Talking to a key waits for a touch, so adding and unlocking are step
//! functions: `*_begin` once, then [`poll`] every frame. The USB
//! conversation runs on the security-key task (`fs::seckey`), never on the
//! desktop's frame.

use crate::fs::seckey::{self, Step};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use fido::ctap2;
use fido::keyslot::{self, hex, unhex, wipe, Kind, Record as Rec, Slot};
use ling_fido as fido;

/// A test kernel (built with `LINGOS_APPKEY_TEST_TOUCH` set) adds and accepts
/// a simulated key when no real one is plugged in. Never ship it.
pub const TEST_TOUCH: bool = option_env!("LINGOS_APPKEY_TEST_TOUCH").is_some();

/// Keys per app.
pub const MAX_SLOTS: usize = 8;
const TOUCH_TIMEOUT_MS: u64 = 60_000;
const POLL_MS: u64 = 150;
/// Failed unlocks before a wait (doubling, up to 15 minutes).
const FREE_FAILS: u32 = 5;

pub fn valid_app(app: &str) -> bool {
    matches!(app, "wallet" | "messenger")
}

fn rp_id(app: &str) -> String {
    format!("lingos.{app}")
}

fn path(user: &str, app: &str) -> String {
    format!("home/{user}/.appkeys/{app}")
}

// ── the sealed record ───────────────────────────────────────────────────────

fn signed_in() -> Option<String> {
    crate::keyring::user()
}

fn load(user: &str, app: &str) -> Option<Rec> {
    if !valid_app(app) {
        return None;
    }
    let p = path(user, app);
    let size = crate::fs::lingfs::file_size(&p)?;
    let mut raw = alloc::vec![0u8; size.max(1)];
    let n = crate::fs::lingfs::read_file_all(&p, &mut raw).ok().flatten()?;
    let mut plain = crate::keyring::open("appkeys", p.as_bytes(), &raw[..n])?;
    let rec = Rec::parse(app, &plain);
    wipe(&mut plain);
    rec
}

fn save(user: &str, app: &str, r: &Rec) -> bool {
    let p = path(user, app);
    if r.slots.is_empty() {
        return crate::fs::lingfs::remove_path(&p).is_ok();
    }
    let mut bytes = r.to_text();
    let sealed = crate::keyring::seal("appkeys", p.as_bytes(), &bytes);
    wipe(&mut bytes);
    let Some(sealed) = sealed else { return false };
    crate::fs::lingfs::write_path_any(&p, &sealed).is_ok()
}

fn parse_secret(hex_s: &str) -> Option<[u8; 32]> {
    unhex(hex_s.trim())?.try_into().ok()
}

// ── what the apps ask ───────────────────────────────────────────────────────

/// Can security keys be used for apps right now (a signed-in account with an
/// open keyring and storage)?
pub fn available() -> bool {
    signed_in().is_some() && crate::fs::lingfs::is_mounted()
}

/// Does `app` require a security key for the signed-in user?
pub fn enabled(app: &str) -> bool {
    signed_in().and_then(|u| load(&u, app)).is_some_and(|r| !r.slots.is_empty())
}

/// `[name, mode]` per key, in the order they were added.
pub fn keys(app: &str) -> Vec<(String, &'static str)> {
    signed_in().and_then(|u| load(&u, app)).map(|r| r.slots.iter().map(|s| (s.name.clone(), s.kind.mode())).collect()).unwrap_or_default()
}

pub fn strict(app: &str) -> bool {
    signed_in().and_then(|u| load(&u, app)).is_some_and(|r| r.strict)
}

/// The app's own setting kept with the record (the wallet's send threshold).
pub fn setting(app: &str) -> String {
    signed_in().and_then(|u| load(&u, app)).map(|r| r.over.clone()).unwrap_or_default()
}

pub fn set_setting(app: &str, v: &str) -> bool {
    let Some(u) = signed_in() else { return false };
    let Some(mut r) = load(&u, app) else { return false };
    r.over = v.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.').take(24).collect();
    save(&u, app, &r)
}

/// Apps of the signed-in user that require a key (Settings > Keys).
pub fn apps_using() -> Vec<&'static str> {
    ["wallet", "messenger"].into_iter().filter(|a| enabled(a)).collect()
}

/// Forget key `i` (never the last one: turning the option off goes through
/// [`disable`], which needs S, so the app can re-save its vault first).
pub fn remove(app: &str, i: usize) -> bool {
    let Some(u) = signed_in() else { return fail_msg("sign in first") };
    let Some(mut r) = load(&u, app) else { return false };
    if i >= r.slots.len() {
        return false;
    }
    if r.slots.len() == 1 {
        return fail_msg("that is the last key -- turn the option off instead");
    }
    let s = r.slots.remove(i);
    if r.strict && !r.slots.iter().any(|s| s.kind == Kind::Hmac) {
        return fail_msg("strict mode needs a key-bound key left");
    }
    let ok = save(&u, app, &r);
    if ok {
        crate::console_write(format!("appkeys: removed \"{}\" from {u}'s {app}\n", s.name).as_bytes());
    }
    ok
}

/// Turn the option off: the app has already re-saved its vault without S and
/// proves it held S. Then the record goes.
pub fn disable(app: &str, secret_hex: &str) -> bool {
    let Some(u) = signed_in() else { return fail_msg("sign in first") };
    let Some(r) = load(&u, app) else { return true };
    let Some(mut s) = parse_secret(secret_hex) else { return fail_msg("that is not this app's key secret") };
    let ok = r.check(app, &s);
    wipe(&mut s);
    if !ok {
        return fail_msg("that is not this app's key secret");
    }
    let done = crate::fs::lingfs::remove_path(&path(&u, app)).is_ok();
    if done {
        crate::console_write(format!("appkeys: {u}'s {app} no longer needs a security key\n").as_bytes());
    }
    done
}

/// Strict on: drop the record's copy of S (only key-bound slots open it then);
/// off: keep S again (account-password recovery). Needs S from the app.
pub fn set_strict(app: &str, on: bool, secret_hex: &str) -> bool {
    let Some(u) = signed_in() else { return fail_msg("sign in first") };
    let Some(mut r) = load(&u, app) else { return false };
    let Some(mut s) = parse_secret(secret_hex) else { return fail_msg("unlock the app first") };
    if !r.check(app, &s) {
        wipe(&mut s);
        return fail_msg("unlock the app first");
    }
    if on && r.slots.iter().any(|x| x.kind != Kind::Hmac) {
        wipe(&mut s);
        return fail_msg("strict mode needs every key to be key-bound (FIDO2 hmac-secret) -- remove the touch-gated ones first");
    }
    r.strict = on;
    r.s = if on { None } else { Some(s) };
    wipe(&mut s);
    save(&u, app, &r)
}

// ── handing S to the app ────────────────────────────────────────────────────

static mut SECRET: [u8; 32] = [0; 32];
/// (user, app) the secret in SECRET belongs to.
static mut SECRET_FOR: Option<(String, String)> = None;
static mut LAST_MODE: &str = "";

fn release(user: &str, app: &str, s: &[u8; 32]) {
    unsafe {
        *(&raw mut SECRET) = *s;
        SECRET_FOR = Some((String::from(user), String::from(app)));
    }
}

fn forget_secret() {
    unsafe {
        wipe(&mut *(&raw mut SECRET));
        SECRET_FOR = None;
    }
}

/// S as hex for `app`, handed out once after a successful unlock / first add
/// / recovery; "" otherwise.
pub fn take_secret(app: &str) -> String {
    let who = unsafe { (*(&raw const SECRET_FOR)).clone() };
    let out = match (who, signed_in()) {
        (Some((u, a)), Some(cur)) if u == cur && a == app => hex(unsafe { &*(&raw const SECRET) }),
        _ => String::new(),
    };
    forget_secret();
    out
}

/// The kind of key the last finished step used ("key-bound ..." / "touch-gated ...").
pub fn last_mode() -> &'static str {
    unsafe { LAST_MODE }
}

// ── throttling ──────────────────────────────────────────────────────────────

struct Throttle {
    who: String,
    fails: u32,
    until: u64,
}
static mut THROTTLE: Vec<Throttle> = Vec::new();

fn locked_ms(who: &str) -> u64 {
    let now = crate::arch::timer::now_ms();
    unsafe { (*(&raw const THROTTLE)).iter().find(|t| t.who == who).map(|t| t.until.saturating_sub(now)).unwrap_or(0) }
}

fn note(who: &str, ok: bool) {
    let v = unsafe { &mut *(&raw mut THROTTLE) };
    if ok {
        v.retain(|t| t.who != who);
        return;
    }
    let now = crate::arch::timer::now_ms();
    let t = match v.iter_mut().position(|t| t.who == who) {
        Some(i) => &mut v[i],
        None => {
            v.push(Throttle { who: String::from(who), fails: 0, until: 0 });
            v.last_mut().unwrap()
        },
    };
    t.fails += 1;
    if t.fails >= FREE_FAILS {
        let wait = (30_000u64 << (t.fails - FREE_FAILS).min(5)).min(15 * 60_000);
        t.until = now + wait;
    }
}

/// Recovery: the account password instead of a key (not in strict mode).
/// Shares the per-account limit with sign-in codes.
pub fn recover(app: &str, account_password: &str) -> bool {
    let Some(u) = signed_in() else { return fail_msg("sign in first") };
    let who = format!("{u}#appkeys");
    if crate::fs::twofa::locked_for_ms(&who) > 0 {
        return fail_msg("too many wrong passwords -- wait a little and try again");
    }
    let Some(r) = load(&u, app) else { return fail_msg("this app does not use a security key") };
    if r.strict {
        return fail_msg("strict mode: only your security keys open it (or the app's recovery phrase)");
    }
    let ok = crate::fs::users::verify(&u, account_password);
    crate::fs::twofa::note_attempt_pub(&who, ok);
    if !ok {
        return fail_msg("that is not your LingOS account password");
    }
    let Some(s) = r.s.as_ref() else { return fail_msg("the record holds no recovery copy") };
    release(&u, app, s);
    unsafe { LAST_MODE = "account password" };
    note(&format!("{u}/{app}"), true);
    crate::console_write(format!("appkeys: {u} opened {app} with the account password (recovery)\n").as_bytes());
    true
}

/// Every key lost (strict mode): forget the app's record after the account
/// password checks out. The app's file stays bound to the lost secret; the
/// app then starts over from its recovery phrase.
pub fn reset(app: &str, account_password: &str) -> bool {
    let Some(u) = signed_in() else { return fail_msg("sign in first") };
    let who = format!("{u}#appkeys");
    if crate::fs::twofa::locked_for_ms(&who) > 0 {
        return fail_msg("too many wrong passwords -- wait a little and try again");
    }
    let ok = crate::fs::users::verify(&u, account_password);
    crate::fs::twofa::note_attempt_pub(&who, ok);
    if !ok {
        return fail_msg("that is not your LingOS account password");
    }
    let done = crate::fs::lingfs::remove_path(&path(&u, app)).is_ok();
    if done {
        crate::console_write(format!("appkeys: {u} forgot the security keys of {app} (account password)
").as_bytes());
    }
    done
}

// ── the touch step machine ──────────────────────────────────────────────────

#[derive(Clone)]
enum Op {
    Idle,
    Add { user: String, app: String, name: String, given: Option<[u8; 32]>, started: u64, last: u64 },
    Unlock { user: String, app: String, give: bool, started: u64, last: u64 },
}

static mut OP: Op = Op::Idle;
static mut GEN: u32 = 0;
static mut RESULT: Option<(u32, Step)> = None;
static mut MSG: &str = "";
static mut ERR: &str = "";
/// Set by [`cancel`] while the key task is inside a request: the transport
/// gives up at once.
static mut ABORT: bool = false;

fn op() -> &'static mut Op {
    unsafe { &mut *(&raw mut OP) }
}

pub fn busy() -> bool {
    !matches!(op(), Op::Idle)
}

pub fn aborting() -> bool {
    unsafe { ABORT }
}

fn fail_msg(m: &'static str) -> bool {
    unsafe { ERR = m };
    false
}

/// What to show while waiting ("Touch your security key", ...).
pub fn message() -> &'static str {
    unsafe { MSG }
}

/// Why the last step failed.
pub fn error() -> &'static str {
    unsafe { ERR }
}

fn start(o: Op) -> bool {
    if seckey::busy() {
        return fail_msg("the security key is busy with a sign-in or SSH approval -- finish that first");
    }
    if !seckey::ensure_task() {
        return fail_msg("no background task is free for the security key -- close an app and try again");
    }
    forget_secret();
    unsafe {
        GEN = GEN.wrapping_add(1);
        RESULT = None;
        ABORT = false;
        MSG = "Plug in your security key";
        ERR = "";
        LAST_MODE = "";
    }
    *op() = o;
    true
}

/// Start adding a key for `app`. `secret_hex` is S from the unlocked app
/// (required once the app uses keys; "" for the first key: S is made then and handed
/// back by [`take_secret`] so the app can bind its vault to it).
pub fn add_begin(app: &str, name: &str, secret_hex: &str) -> bool {
    let Some(user) = signed_in() else { return fail_msg("sign in to your LingOS account first") };
    if !valid_app(app) {
        return fail_msg("unknown app");
    }
    let rec = load(&user, app);
    if rec.as_ref().is_some_and(|r| r.slots.len() >= MAX_SLOTS) {
        return fail_msg("this app already has the most keys it can hold -- remove one first");
    }
    let given = match (&rec, parse_secret(secret_hex)) {
        (Some(r), Some(mut s)) => {
            if !r.check(app, &s) {
                wipe(&mut s);
                return fail_msg("unlock the app first");
            }
            Some(s)
        },
        // another key only from the unlocked app (it proves it holds S): a key cannot be slipped in
        // from an unattended desktop
        (Some(_), None) => return fail_msg("unlock the app first, then add the key"),
        (None, _) => None,
    };
    let now = crate::arch::timer::now_ms();
    start(Op::Add { user, app: String::from(app), name: keyslot::clean_name(name), given, started: now, last: 0 })
}

/// Start opening `app`: a touch of any one of its keys. With `give`, S is
/// handed out on success ([`take_secret`]); without, it is only a check
/// (Test the key, confirm a big send).
pub fn unlock_begin(app: &str, give: bool) -> bool {
    let Some(user) = signed_in() else { return fail_msg("sign in to your LingOS account first") };
    let Some(r) = load(&user, app) else { return fail_msg("this app does not use a security key") };
    if r.slots.is_empty() {
        return fail_msg("this app does not use a security key");
    }
    let left = locked_ms(&format!("{user}/{app}"));
    if left > 0 {
        return fail_msg("too many failed tries -- wait a little, or use your password / recovery");
    }
    let now = crate::arch::timer::now_ms();
    start(Op::Unlock { user, app: String::from(app), give, started: now, last: 0 })
}

/// Stop waiting (Cancel / Esc).
pub fn cancel() {
    unsafe {
        if busy() {
            ABORT = true;
        }
        GEN = GEN.wrapping_add(1);
        RESULT = None;
        MSG = "";
    }
    *op() = Op::Idle;
    forget_secret();
}

/// One step for the UI: 2 plug in, 1 touch, 0 done, -1 failed ([`error`]).
pub fn poll() -> Step {
    if !busy() {
        if let Some((g, s)) = unsafe { RESULT } {
            if g == unsafe { GEN } {
                unsafe { RESULT = None };
                return s;
            }
        }
        return Step::Failed(error());
    }
    // give the key task its turn now
    crate::proc::sched::yield_now();
    unsafe {
        match RESULT {
            Some((g, s)) if g == GEN && matches!(s, Step::Done | Step::Failed(_)) => {
                RESULT = None;
                s
            },
            Some((g, s)) if g == GEN => s,
            _ => Step::Touch,
        }
    }
}

/// The security-key task's turn (called from `seckey`'s task loop).
pub fn task_turn() {
    let g = unsafe { GEN };
    let now = crate::arch::timer::now_ms();
    let o = match op() {
        Op::Idle => return,
        Op::Add { last, .. } | Op::Unlock { last, .. } => {
            if now.saturating_sub(*last) < POLL_MS {
                return;
            }
            *last = now;
            op().clone()
        },
    };
    unsafe { ABORT = false };
    let s = match o {
        Op::Add { user, app, name, given, started, .. } => {
            let r = add_step(&user, &app, &name, given, started);
            if let Some(mut x) = given {
                wipe(&mut x);
            }
            r
        },
        Op::Unlock { user, app, give, started, .. } => unlock_step(&user, &app, give, started),
        Op::Idle => return,
    };
    if unsafe { ABORT } {
        // cancelled mid-request: tell the key to stop and open a fresh channel next time
        if let Some(dev) = seckey::device() {
            ctap2::cancel(dev);
        }
        seckey::reset_device();
        unsafe { ABORT = false };
    }
    unsafe {
        if g != GEN {
            return;
        }
        RESULT = Some((g, s));
        if let Step::Failed(m) = s {
            ERR = m;
        }
        if matches!(s, Step::Done | Step::Failed(_)) {
            *op() = Op::Idle;
        }
    }
}

fn set_msg(m: &'static str) {
    unsafe { MSG = m };
}

fn random32() -> Option<[u8; 32]> {
    let mut b = [0u8; 32];
    crate::crypto::random_bytes(&mut b).then_some(b)
}

fn test_warn() {
    static mut WARNED: bool = false;
    if !unsafe { WARNED } {
        unsafe { WARNED = true };
        crate::console_write(b"appkeys: TEST BUILD (LINGOS_APPKEY_TEST_TOUCH) -- app security-key touches are SIMULATED, never ship this kernel\n");
    }
}

fn add_step(user: &str, app: &str, name: &str, given: Option<[u8; 32]>, started: u64) -> Step {
    let now = crate::arch::timer::now_ms();
    if now.saturating_sub(started) > TOUCH_TIMEOUT_MS {
        return Step::Failed("no touch in time -- try again");
    }
    // the record as it is now, or a new one with a fresh S
    let mut fresh = false;
    let mut rec = match load(user, app) {
        Some(r) => r,
        None => {
            let (Some(s), Some(u2f_app), Some(salt)) = (random32(), random32(), random32()) else {
                return Step::Failed("no random numbers -- cannot make a key secret");
            };
            fresh = true;
            Rec::new(app, &s, u2f_app, salt)
        },
    };
    // S: from the record, else from the app (strict mode)
    let s = match (rec.s, given) {
        (Some(s), _) => s,
        (None, Some(s)) => s,
        (None, None) => return Step::Failed("strict mode: unlock the app first, then add the key"),
    };
    if TEST_TOUCH && !crate::arch::xhci::fido_present() {
        test_warn();
        set_msg("TEST kernel: simulating a touch...");
        if now.saturating_sub(started) < 1500 {
            return Step::Touch;
        }
        if rec.strict {
            return Step::Failed("strict mode needs a key-bound key (the test key is not one)");
        }
        let n = rec.slots.len() as u8;
        rec.slots.push(Slot { kind: Kind::Test, name: String::from(name), counter: 0, uv: false, cred: alloc::vec![0x7E, n], public_key: alloc::vec![0], nonce: [0; 12], tag: [0; 16], wrapped: Vec::new() });
        return finish_add(user, app, name, rec, fresh, s, Kind::Test);
    }
    let Some(dev) = seckey::device() else {
        set_msg("Plug in your security key");
        return Step::PlugIn;
    };
    // the same physical key twice would only waste a slot
    let hmac_creds: Vec<Vec<u8>> = rec.slots.iter().filter(|x| x.kind == Kind::Hmac).map(|x| x.cred.clone()).collect();
    for k in rec.slots.iter().filter(|x| x.kind == Kind::U2f) {
        if let Ok(true) = fido::owns(dev, &rec.u2f_app, &k.cred) {
            return Step::Failed("this key is already added to this app");
        }
    }
    let cbor = dev.caps & 4 != 0;
    if cbor && !hmac_creds.is_empty() {
        if let Ok(true) = ctap2::owns_any(dev, &rp_id(app), &hmac_creds) {
            return Step::Failed("this key is already added to this app");
        }
    }
    // FIDO2 hmac-secret when the key has it
    let mut why_u2f = "the key does not do FIDO2";
    if cbor {
        match ctap2::get_info(dev) {
            Ok(info) if info.hmac_secret() && !info.always_uv() => match add_hmac(dev, user, app, name, &rec, &s, info.protocol()) {
                Ok(slot) => {
                    rec.slots.push(slot);
                    return finish_add(user, app, name, rec, fresh, s, Kind::Hmac);
                },
                Err(AddErr::Step(st)) => return st,
                Err(AddErr::NoHmac(w)) => why_u2f = w,
            },
            Ok(info) if info.always_uv() => why_u2f = "the key asks for its PIN / fingerprint on every request",
            Ok(_) => why_u2f = "the key has no hmac-secret",
            Err(_) if aborting() => return Step::Failed("cancelled"),
            Err(_) => why_u2f = "the key did not answer its FIDO2 info",
        }
    }
    if rec.strict {
        return Step::Failed("strict mode needs a key with FIDO2 hmac-secret; this one can only be touch-gated (U2F)");
    }
    crate::console_write(format!("appkeys: {app}: U2F (touch-gated) slot -- {why_u2f}\n").as_bytes());
    // U2F: register (polled until touched)
    let Some(challenge) = random32() else { return Step::Failed("no random numbers") };
    set_msg("Touch your security key");
    match fido::register(dev, &challenge, &rec.u2f_app) {
        Err(fido::Error::NeedTouch) => Step::Touch,
        Err(_) if aborting() => Step::Failed("cancelled"),
        Err(_) => Step::Failed("the key did not answer -- unplug it, plug it back in and try again"),
        Ok(reg) => {
            rec.slots.push(Slot { kind: Kind::U2f, name: String::from(name), counter: 0, uv: false, cred: reg.key_handle, public_key: reg.public_key.to_vec(), nonce: [0; 12], tag: [0; 16], wrapped: Vec::new() });
            finish_add(user, app, name, rec, fresh, s, Kind::U2f)
        },
    }
}

fn finish_add(user: &str, app: &str, name: &str, rec: Rec, fresh: bool, s: [u8; 32], kind: Kind) -> Step {
    let mut s = s;
    if !save(user, app, &rec) {
        wipe(&mut s);
        return Step::Failed("could not save the key (storage full, or the keyring is locked?)");
    }
    if fresh {
        // the app binds its vault to S now
        release(user, app, &s);
    }
    wipe(&mut s);
    unsafe { LAST_MODE = kind.mode() };
    crate::console_write(format!("appkeys: added \"{name}\" to {user}'s {app} ({})\n", kind.mode()).as_bytes());
    Step::Done
}

enum AddErr {
    Step(Step),
    /// Fall back to U2F, for this reason.
    NoHmac(&'static str),
}

fn ctap_fail(e: fido::Error) -> AddErr {
    match e {
        _ if aborting() => AddErr::Step(Step::Failed("cancelled")),
        fido::Error::Ctap(ctap2::ERR_USER_ACTION_TIMEOUT) => AddErr::Step(Step::Touch), // ask again (the add times out at 60 s)
        fido::Error::Ctap(ctap2::ERR_OPERATION_DENIED) => AddErr::Step(Step::Failed("the key refused (touch declined?) -- try again")),
        fido::Error::Ctap(c) if ctap2::needs_pin_or_uv(c) => AddErr::NoHmac("the key wants its PIN for FIDO2"),
        _ => AddErr::Step(Step::Failed("the key did not answer -- unplug it, plug it back in and try again")),
    }
}

fn add_hmac(dev: &mut fido::Device<seckey::Usb>, user: &str, app: &str, name: &str, rec: &Rec, s: &[u8; 32], protocol: u8) -> Result<Slot, AddErr> {
    let rp = rp_id(app);
    let (Some(cdh), Some(uid)) = (random32(), random32()) else { return Err(AddErr::Step(Step::Failed("no random numbers"))) };
    set_msg("Touch your security key (1 of 2)");
    let cred = ctap2::make_credential(dev, &rp, &uid[..16], user, &cdh).map_err(ctap_fail)?;
    if !cred.hmac_secret {
        return Err(AddErr::NoHmac("the key did not turn hmac-secret on"));
    }
    set_msg("Touch your security key again (2 of 2)");
    let (Some(mut eph), Some(cdh2)) = (random32(), random32()) else { return Err(AddErr::Step(Step::Failed("no random numbers"))) };
    let mut iv = [0u8; 16];
    crate::crypto::random_bytes(&mut iv);
    let shared = ctap2::key_agreement(dev, protocol, &eph).map_err(ctap_fail);
    wipe(&mut eph);
    let shared = shared?;
    let a = loop {
        match ctap2::hmac_secret(dev, &rp, &cdh2, core::slice::from_ref(&cred.id), &rec.salt, &shared, &iv) {
            Err(fido::Error::Ctap(ctap2::ERR_USER_ACTION_TIMEOUT)) if !aborting() => continue,
            r => break r.map_err(ctap_fail)?,
        }
    };
    if !a.verify(&cred.public_key, &cdh2) {
        return Err(AddErr::Step(Step::Failed("the key's signature did not check out")));
    }
    let mut nonce = [0u8; 12];
    crate::crypto::random_bytes(&mut nonce);
    let Some((wrapped, tag)) = keyslot::wrap(user, app, &cred.id, &a.output, s, &nonce) else {
        return Err(AddErr::Step(Step::Failed("could not wrap the key secret")));
    };
    Ok(Slot { kind: Kind::Hmac, name: String::from(name), counter: a.counter, uv: a.flags & ctap2::FLAG_UV != 0, cred: cred.id.clone(), public_key: cred.public_key.to_vec(), nonce, tag, wrapped })
}

fn unlock_step(user: &str, app: &str, give: bool, started: u64) -> Step {
    let now = crate::arch::timer::now_ms();
    if now.saturating_sub(started) > TOUCH_TIMEOUT_MS {
        return Step::Failed("no touch in time -- try again");
    }
    let who = format!("{user}/{app}");
    let Some(mut rec) = load(user, app) else { return Step::Failed("this app does not use a security key") };
    let ok = |rec: &Rec, s: &[u8; 32], kind: Kind| -> Step {
        if !rec.check(app, s) {
            note(&who, false);
            return Step::Failed("the key's secret did not match -- use your password / recovery");
        }
        note(&who, true);
        if give {
            release(user, app, s);
        }
        unsafe { LAST_MODE = kind.mode() };
        Step::Done
    };
    if TEST_TOUCH && !crate::arch::xhci::fido_present() && rec.slots.iter().any(|x| x.kind == Kind::Test) {
        test_warn();
        set_msg("TEST kernel: simulating a touch...");
        if now.saturating_sub(started) < 1500 {
            return Step::Touch;
        }
        return match rec.s {
            Some(s) => ok(&rec, &s, Kind::Test),
            None => Step::Failed("strict mode: the test key cannot open it"),
        };
    }
    let Some(dev) = seckey::device() else {
        set_msg("Plug in your security key");
        return Step::PlugIn;
    };
    set_msg("Touch your security key");
    // a touch-gated (U2F) key?
    let mut u2f = None;
    for (i, k) in rec.slots.iter().enumerate().filter(|(_, x)| x.kind == Kind::U2f) {
        match fido::owns(dev, &rec.u2f_app, &k.cred) {
            Ok(true) => {
                u2f = Some(i);
                break;
            },
            Ok(false) => {},
            Err(_) if aborting() => return Step::Failed("cancelled"),
            Err(_) => return Step::Failed("the key did not answer -- unplug it and plug it back in"),
        }
    }
    if let Some(i) = u2f {
        let Some(challenge) = random32() else { return Step::Failed("no random numbers") };
        return match fido::authenticate(dev, &challenge, &rec.u2f_app, &rec.slots[i].cred) {
            Err(fido::Error::NeedTouch) => Step::Touch,
            Err(_) if aborting() => Step::Failed("cancelled"),
            Err(_) => Step::Failed("the key did not answer -- unplug it and plug it back in"),
            Ok(a) => {
                if !fido::verify(&rec.slots[i].public_key, &rec.u2f_app, &challenge, &a) {
                    note(&who, false);
                    return Step::Failed("the key's signature did not check out");
                }
                if a.counter != 0 && a.counter <= rec.slots[i].counter {
                    note(&who, false);
                    crate::console_write(format!("appkeys: counter went back for {user}'s {app} key \"{}\" -- refused\n", rec.slots[i].name).as_bytes());
                    return Step::Failed("this key's counter went backwards -- it may be a copy; refused");
                }
                rec.slots[i].counter = a.counter;
                let _ = save(user, app, &rec);
                match rec.s {
                    Some(s) => ok(&rec, &s, Kind::U2f),
                    None => Step::Failed("strict mode: touch-gated keys cannot open it"),
                }
            },
        };
    }
    // a key-bound (hmac-secret) key?
    let creds: Vec<Vec<u8>> = rec.slots.iter().filter(|x| x.kind == Kind::Hmac).map(|x| x.cred.clone()).collect();
    if creds.is_empty() || dev.caps & 4 == 0 {
        note(&who, false);
        return Step::Failed("that is not one of this app's security keys");
    }
    let protocol = match ctap2::get_info(dev) {
        Ok(i) => i.protocol(),
        Err(_) if aborting() => return Step::Failed("cancelled"),
        Err(_) => return Step::Failed("the key did not answer -- unplug it and plug it back in"),
    };
    let (Some(mut eph), Some(cdh)) = (random32(), random32()) else { return Step::Failed("no random numbers") };
    let mut iv = [0u8; 16];
    crate::crypto::random_bytes(&mut iv);
    let shared = ctap2::key_agreement(dev, protocol, &eph);
    wipe(&mut eph);
    let shared = match shared {
        Ok(s) => s,
        Err(_) if aborting() => return Step::Failed("cancelled"),
        Err(_) => return Step::Failed("the key did not answer -- unplug it and plug it back in"),
    };
    let a = match ctap2::hmac_secret(dev, &rp_id(app), &cdh, &creds, &rec.salt, &shared, &iv) {
        Ok(a) => a,
        Err(_) if aborting() => return Step::Failed("cancelled"),
        Err(fido::Error::Ctap(ctap2::ERR_USER_ACTION_TIMEOUT)) => return Step::Touch, // asked again next turn
        Err(fido::Error::Ctap(ctap2::ERR_NO_CREDENTIALS)) => {
            note(&who, false);
            return Step::Failed("that is not one of this app's security keys");
        },
        Err(fido::Error::Ctap(c)) if ctap2::needs_pin_or_uv(c) => return Step::Failed("the key wants its PIN / fingerprint set up differently than when it was added -- use your password / recovery"),
        Err(_) => return Step::Failed("the key did not answer -- unplug it and plug it back in"),
    };
    let Some(i) = rec.slots.iter().position(|x| x.kind == Kind::Hmac && x.cred == a.credential_id) else {
        return Step::Failed("that is not one of this app's security keys");
    };
    if !a.verify(&rec.slots[i].public_key, &cdh) {
        note(&who, false);
        return Step::Failed("the key's signature did not check out");
    }
    if a.counter != 0 && a.counter <= rec.slots[i].counter {
        note(&who, false);
        crate::console_write(format!("appkeys: counter went back for {user}'s {app} key \"{}\" -- refused\n", rec.slots[i].name).as_bytes());
        return Step::Failed("this key's counter went backwards -- it may be a copy; refused");
    }
    let sl = &rec.slots[i];
    let Some(mut s) = keyslot::unwrap(user, app, sl, &a.output) else {
        note(&who, false);
        let uv_now = a.flags & ctap2::FLAG_UV != 0;
        return Step::Failed(if uv_now != sl.uv {
            "the key checked you differently than when it was added (fingerprint / PIN vs. touch) -- try again, or use your password / recovery"
        } else {
            "the key's secret did not unwrap -- use your password / recovery"
        });
    };
    rec.slots[i].counter = a.counter;
    let _ = save(user, app, &rec);
    let r = ok(&rec, &s, Kind::Hmac);
    wipe(&mut s);
    r
}

// ── `appkeys` (SSH / terminal) ──────────────────────────────────────────────

fn wait_step() -> Step {
    let t0 = crate::arch::timer::now_ms();
    loop {
        let s = poll();
        if matches!(s, Step::Done | Step::Failed(_)) {
            return s;
        }
        if crate::arch::timer::now_ms().saturating_sub(t0) > 90_000 {
            cancel();
            return Step::Failed("selftest: no answer in 90 s");
        }
        crate::proc::sched::yield_now();
    }
}

/// `appkeys` lists which apps need a key (and how each key protects them);
/// a test kernel also runs `appkeys selftest <app> <account password>`.
pub fn command(arg: &str, user: &str) -> String {
    let mut w = arg.split_whitespace();
    match w.next().unwrap_or("status") {
        "status" | "list" | "ls" => {
            if signed_in().as_deref() != Some(user) {
                return format!("appkeys: {user}'s keyring is not open (sign in at the desktop first)\n");
            }
            let mut s = String::new();
            for app in ["wallet", "messenger"] {
                let k = keys(app);
                if k.is_empty() {
                    s.push_str(&format!("{app}: no security key required\n"));
                    continue;
                }
                s.push_str(&format!("{app}: a security key is required ({} key{}, {})\n", k.len(), if k.len() == 1 { "" } else { "s" }, if strict(app) { "strict: no account-password recovery" } else { "recovery: account password" }));
                for (i, (n, m)) in k.iter().enumerate() {
                    s.push_str(&format!("  {}  {n}  -- {m}\n", i + 1));
                }
            }
            if TEST_TOUCH {
                s.push_str("(TEST BUILD: app security-key touches are simulated)\n");
            }
            s
        },
        "selftest" if TEST_TOUCH => {
            let (Some(app), Some(pw)) = (w.next(), w.next()) else { return String::from("usage: appkeys selftest <wallet|messenger> <account password>\n") };
            selftest(app, pw)
        },
        _ => String::from("usage: appkeys [status]\n"),
    }
}

fn selftest(app: &str, pw: &str) -> String {
    let mut out = String::new();
    let mut fails = 0;
    let mut check = |out: &mut String, ok: bool, what: &str| {
        out.push_str(if ok { "  PASS  " } else { "  FAIL  " });
        out.push_str(what);
        out.push('\n');
        if !ok {
            fails += 1;
        }
    };
    if enabled(app) {
        return format!("appkeys selftest: {app} already uses keys -- run it on an app without\n");
    }
    // first key: a fresh secret comes back
    let ok = add_begin(app, "Test key A", "") && wait_step() == Step::Done;
    check(&mut out, ok, "add the first (simulated) key");
    let s1 = take_secret(app);
    check(&mut out, s1.len() == 64, "the first add hands out the app secret");
    check(&mut out, take_secret(app).is_empty(), "the secret is handed out only once");
    check(&mut out, enabled(app) && keys(app).len() == 1, "the app now requires a key");
    // the sealed record holds nothing readable without the keyring
    let raw = signed_in().and_then(|u| {
        let p = path(&u, app);
        let n = crate::fs::lingfs::file_size(&p)?;
        let mut b = alloc::vec![0u8; n];
        crate::fs::lingfs::read_file_all(&p, &mut b).ok().flatten().map(|m| b[..m].to_vec())
    });
    let sealed = raw.as_ref().is_some_and(|r| crate::keyring::is_sealed(r) && !r.windows(17).any(|w| w == b"lingos-appkeys v1") && !String::from_utf8_lossy(r).contains(&s1));
    check(&mut out, sealed, "the record on disk is sealed (no header, no secret in the clear)");
    // unlock: the same secret again
    let ok = unlock_begin(app, true) && wait_step() == Step::Done;
    let s2 = take_secret(app);
    check(&mut out, ok && s2 == s1, "unlock with a touch gives the same secret");
    // a check-only touch gives nothing out
    let ok = unlock_begin(app, false) && wait_step() == Step::Done;
    check(&mut out, ok && take_secret(app).is_empty(), "a test touch hands out no secret");
    // a second key, then remove the first
    let ok = add_begin(app, "Test key B", &s1) && wait_step() == Step::Done;
    check(&mut out, ok && keys(app).len() == 2 && take_secret(app).is_empty(), "add a second key (no new secret)");
    check(&mut out, !add_begin(app, "bad", "00"), "adding with a wrong secret is refused");
    check(&mut out, remove(app, 0) && keys(app).len() == 1, "remove the first key");
    check(&mut out, !remove(app, 0), "the last key cannot be removed (turn it off instead)");
    // recovery with the account password
    check(&mut out, !recover(app, "not-the-password"), "recovery with a wrong account password fails");
    let ok = recover(app, pw);
    check(&mut out, ok && take_secret(app) == s1, "recovery with the account password gives the secret");
    // strict: no recovery, and a simulated (non key-bound) key cannot be added
    check(&mut out, !set_strict(app, true, &s1), "strict mode is refused while a key is not key-bound");
    // turning off needs the secret
    check(&mut out, !disable(app, "11"), "turning off with a wrong secret is refused");
    check(&mut out, disable(app, &s1) && !enabled(app), "turn off with the secret");
    out.push_str(&format!("appkeys selftest ({app}): {}\n", if fails == 0 { String::from("all passed") } else { format!("{fails} FAILED") }));
    crate::console_write(out.as_bytes());
    out
}
