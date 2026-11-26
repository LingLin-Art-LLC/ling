//! An in-kernel SSH **server** -- the "ssh into LingOS from another machine"
//! half -- interoperable with real OpenSSH. Built on the pieces that now
//! exist: `netstack`'s passive open (`tcp_listen_accept`), the classical
//! crypto suite (`crypto`: X25519, SHA-256), Ed25519 host-key signing
//! (`ed25519`), and raw ChaCha20 + Poly1305 (for `chacha20-poly1305@openssh.com`).
//!
//! Negotiated algorithms (the modern, pure-software OpenSSH default set, so no
//! AES-NI dependency): key exchange `curve25519-sha256`, host key `ssh-ed25519`,
//! cipher+MAC `chacha20-poly1305@openssh.com` (AEAD). One session channel, a
//! filesystem-backed built-in shell. Runs on its own netstack socket (see the
//! multi-socket TCP section), so an SSH session interleaves with other network
//! users -- a browser fetch, Messenger -- instead of monopolizing the wire.
//!
//! Authentication is **password**, verified against the user DB
//! (`users::verify`): `none` and wrong passwords are rejected, six failures
//! disconnect, no passwordless login. So a real account must exist -- the Live
//! desktop's `live`/`live`, or the root/user the installer created; a bare
//! rescue image with no accounts is intentionally unreachable. Started at boot
//! as a background task when SSH is enabled (`services::ssh_enabled`, set by the
//! installer or the first-boot prompt) -- see `start`; also runnable manually
//! via the rescue `sshd` command.
//!
//! Honest scope, disclosed not hidden:
//! - **One SSH session at a time**: the background task serves one client to
//!   completion, then accepts the next. The session runs on its own socket of
//!   the multi-socket netstack, so the desktop's other network users (browser,
//!   Messenger) keep working while someone is logged in.
//! - The shell is a small built-in command set (help/echo/whoami/uname/uptime/
//!   exit), not yet the full `lsh` (whose console I/O would need redirecting
//!   onto the channel). The point reached is a *real* encrypted, authenticated,
//!   interactive session against an unmodified `ssh` client.

use crate::drivers::netstack;
use crate::fs::{lingfs, users};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use chacha20::cipher::{KeyIvInit, StreamCipher};
use chacha20::ChaCha20Legacy;
use poly1305::universal_hash::{KeyInit, UniversalHash};
use poly1305::Poly1305;

pub const DEFAULT_PORT: u16 = 22;
const IDENT: &[u8] = b"SSH-2.0-LingOS_0.1";

// 0 = not read yet. On the Pi the port is `ssh_port` in setup.conf (the setup
// wizard's "SSH port" field, Settings, `ssh-serve port <n>`); it is only
// latched once the card's filesystem is mounted, so an early call can't pin 22.
static mut PORT: u16 = 0;

/// The TCP port the server listens on.
pub fn port() -> u16 {
    unsafe {
        if PORT != 0 {
            return PORT;
        }
    }
    #[cfg(target_arch = "aarch64")]
    if crate::fs::lingfs::is_mounted() {
        let p = crate::setup::saved("ssh_port").parse::<u16>().ok().filter(|&p| p != 0).unwrap_or(DEFAULT_PORT);
        unsafe { PORT = p };
        return p;
    }
    DEFAULT_PORT
}

/// Change the listening port (1..=65535): persisted on the Pi, and the live
/// listener is moved to it. Connections already accepted stay up.
pub fn set_port(p: u16) -> bool {
    if p == 0 {
        return false;
    }
    let old = port();
    unsafe { PORT = p };
    #[cfg(target_arch = "aarch64")]
    let saved = crate::setup::set_saved("ssh_port", &alloc::format!("{p}"));
    #[cfg(not(target_arch = "aarch64"))]
    let saved = true;
    if old != p {
        netstack::tcp_unlisten(old);
    }
    saved
}

/// Settings > Sign-in "2FA over SSH": whether an account that has two-factor sign-in on must also
/// give its code to log in over SSH / SFTP / scp (they share one login). On unless switched off.
/// Off means the password alone opens SSH for that account -- weaker, and the desktop says so.
pub fn twofa_required() -> bool {
    #[cfg(target_arch = "aarch64")]
    {
        crate::setup::saved("ssh_2fa") != "0"
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        true
    }
}

pub fn set_twofa_required(on: bool) -> bool {
    #[cfg(target_arch = "aarch64")]
    {
        crate::setup::set_saved("ssh_2fa", if on { "1" } else { "0" })
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = on;
        false
    }
}

/// The password step of a password-method login. A two-factor account types its password with the
/// 6-digit code right after it ("hunter2123456", for scripts and clients with no prompt); with the
/// setting off the password alone is enough.
fn password_login(uname: &str, secret: &str) -> bool {
    timed_check(|| password_login_inner(uname, secret))
}

/// A password check (an Argon2id hash, ~0.3 s, on another core), logged with how many frames the
/// screen drew meanwhile -- it should keep drawing, not freeze for the check.
fn timed_check(f: impl FnOnce() -> bool) -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    let (t0, f0) = (crate::arch::timer::now_us(), crate::lingapp::PRESENTED_FRAMES.load(Relaxed));
    let ok = f();
    let (dt, df) = (crate::arch::timer::now_us().saturating_sub(t0), crate::lingapp::PRESENTED_FRAMES.load(Relaxed).saturating_sub(f0));
    log(alloc::format!("sshd: password checked in {} ms; the screen drew {} frames meanwhile\n", dt / 1000, df).as_bytes());
    ok
}

fn password_login_inner(uname: &str, secret: &str) -> bool {
    if crate::fs::seckey::enabled(uname) {
        // the key is at the machine: only password + code (verify_login) will do
        return twofa_required() && users::verify_login(uname, secret);
    }
    if twofa_required() {
        users::verify_login(uname, secret)
    } else {
        users::verify(uname, secret) || users::verify_login(uname, secret)
    }
}

/// One keyboard-interactive round (RFC 4256): show `prompt` (no echo) and return what the client typed.
fn kbdint_ask(sess: &mut Session, name: &str, instruction: &str, prompt: &str) -> Option<String> {
    let mut m = Vec::new();
    m.push(MSG_USERAUTH_INFO_REQUEST);
    put_string(&mut m, name.as_bytes());
    put_string(&mut m, instruction.as_bytes());
    put_string(&mut m, b"");
    put_u32(&mut m, 1);
    put_string(&mut m, prompt.as_bytes());
    m.push(0); // echo: false
    if !sess.send(&m) {
        return None;
    }
    loop {
        let msg = sess.recv()?;
        match msg.first().copied() {
            Some(MSG_USERAUTH_INFO_RESPONSE) => {
                let mut p = 1usize;
                let n = u32::from_be_bytes([*msg.get(p)?, *msg.get(p + 1)?, *msg.get(p + 2)?, *msg.get(p + 3)?]);
                p += 4;
                if n != 1 {
                    return None;
                }
                let r = get_string(&msg, &mut p)?;
                return Some(String::from_utf8_lossy(r).into_owned());
            },
            Some(MSG_IGNORE) | Some(MSG_DEBUG) => {},
            _ => return None,
        }
    }
}

/// A keyboard-interactive round with no prompts: the client just shows `name` + `instruction`
/// (OpenSSH prints them) and answers with an empty response.
fn kbdint_note(sess: &mut Session, name: &str, instruction: &str) {
    let mut m = Vec::new();
    m.push(MSG_USERAUTH_INFO_REQUEST);
    put_string(&mut m, name.as_bytes());
    put_string(&mut m, instruction.as_bytes());
    put_string(&mut m, b"");
    put_u32(&mut m, 0);
    if !sess.send(&m) {
        return;
    }
    while let Some(msg) = sess.recv() {
        match msg.first().copied() {
            Some(MSG_IGNORE) | Some(MSG_DEBUG) => {},
            _ => return,
        }
    }
}

/// keyboard-interactive login: the password, then -- for an account with two-factor sign-in on, when
/// the setting asks for it -- "Verification code:". The code is asked for whether or not the password
/// was right, so a wrong password can't be told apart from a wrong code.
fn kbdint_login(sess: &mut Session, uname: &str) -> bool {
    let Some(pw) = kbdint_ask(sess, "", "", "Password: ") else { return false };
    let pw_ok = !pw.is_empty() && timed_check(|| users::verify(uname, &pw));
    let needs_code = twofa_required() && crate::fs::twofa::enabled(uname);
    // A security key cannot be touched over SSH: such an account comes in only
    // with its authenticator code as the second factor.
    if crate::fs::seckey::enabled(uname) && !needs_code {
        log(b"sshd: refused -- the account signs in with a security key (turn on two-factor codes for SSH)\n");
        // say so on the connecting side instead of a bare "Permission denied"
        kbdint_note(
            sess,
            "Security key account",
            "This account signs in with a USB security key. Log in with a security key on YOUR computer instead (ssh-keygen -t ed25519-sk, or -t ecdsa-sk), then allow it once at the LingOS PC with a touch of this account's key. Or, on the LingOS PC, open Settings > Sign-in and turn on two-factor codes: SSH then asks for the password, then the 6-digit code.",
        );
        return false;
    }
    if !needs_code {
        // no second factor to ask for (none set up, or the setting is off): the password decides
        return pw_ok;
    }
    let wait = crate::fs::twofa::locked_for_ms(uname);
    if wait > 0 {
        log(b"sshd: two-factor codes are refused for a while after too many wrong ones\n");
    }
    let Some(code) = kbdint_ask(sess, "Two-factor authentication", "Enter the 6-digit code from your authenticator app (or a recovery code).", "Verification code: ") else {
        return false;
    };
    let code_ok = crate::fs::twofa::check(uname, code.trim());
    if pw_ok && !code_ok {
        // The password was right, so whoever this is knows it: say why the code was refused
        // instead of a bare "Password:" again (a right code can fail on a wrong clock, a lockout,
        // or a code used once already).
        let left = crate::fs::twofa::locked_for_ms(uname);
        let why = if left > 0 {
            alloc::format!("Too many wrong codes: every code, even a right one, is refused for another {} s. Wait, then use the newest code.", left.div_ceil(1000))
        } else if crate::fs::twofa::last_was_replay() {
            String::from("That code was used already (each code works once). Wait for the next one.")
        } else {
            let clock = alloc::format!(
                "LingOS's clock reads {} UTC ({}).",
                crate::clock::now_utc_string(),
                if crate::clock::from_network() { "from a time server" } else { "the PC's own clock -- no time server answered" }
            );
            match crate::fs::twofa::diagnose(uname, code.trim()) {
                Some(off) => {
                    let (m, dir) = (off.unsigned_abs() / 60, if off > 0 { "ahead of" } else { "behind" });
                    alloc::format!("The code is from the right authenticator entry, but its time is about {m} min {dir} LingOS's clock. {clock} Set the clock on the device that runs the authenticator app to automatic time, or fix this PC's clock.")
                },
                None => alloc::format!("The code is not from this account's key at any time in the last or next day: the authenticator entry is from an older 2FA setup (every setup makes a new key). Try your other LingOS entries, or turn 2FA off and set it up again. {clock}"),
            }
        };
        log(alloc::format!("sshd: two-factor code refused for {uname}: {why}\n").as_bytes());
        kbdint_note(sess, "Two-factor code refused", &why);
    }
    pw_ok && code_ok
}

// ── publickey with a security key on the client (RFC 4252 §7, PROTOCOL.u2f) ──

/// SSH_MSG_USERAUTH_BANNER: OpenSSH prints it on the connecting side.
fn banner(sess: &mut Session, text: &str) {
    let mut m = Vec::new();
    m.push(MSG_USERAUTH_BANNER);
    put_string(&mut m, text.as_bytes());
    put_string(&mut m, b"");
    sess.send(&m);
}

/// Who is connecting, for the approval box.
struct Peer {
    ip: String,
    version: String,
}

enum PkResult {
    /// USERAUTH_SUCCESS was sent.
    Success,
    /// PK_OK was sent: the client signs next.
    PkOk,
    /// Answer with USERAUTH_FAILURE.
    Failure,
}

/// The person at the LingOS PC decides about a key that is not approved yet.
/// Polls the request while the desktop keeps drawing (this is the sshd task;
/// `yield_now` hands the CPU back between looks). Some(request id) = allowed.
fn wait_approval(sess: &mut Session, uname: &str, peer: &Peer, kind: ling_fido::ssh::Kind, blob: &[u8]) -> Option<u32> {
    use crate::fs::sshkeys::{self, ReqState};
    let id = match sshkeys::request(uname, &peer.ip, kind, blob, &peer.version) {
        Ok(id) => id,
        Err(why) => {
            log(b"sshd: approval not asked: ");
            log(why.as_bytes());
            log(b"\n");
            banner(sess, &alloc::format!("{why}\n"));
            return None;
        },
    };
    banner(
        sess,
        &alloc::format!(
            "This security key ({} {}) is new to {uname}@LingOS.\nWaiting for approval on the LingOS PC: touch your security key there. ({} s)\n",
            kind.short(),
            ling_fido::ssh::fingerprint(blob),
            sshkeys::APPROVE_TIMEOUT_MS / 1000
        ),
    );
    loop {
        match sshkeys::state(id) {
            ReqState::Waiting => {},
            ReqState::Approved => {
                banner(sess, "Approved at the LingOS PC.\n");
                return Some(id);
            },
            ReqState::Denied => {
                banner(sess, "Denied at the LingOS PC.\n");
                sshkeys::cancel(id);
                return None;
            },
            ReqState::TimedOut => {
                banner(sess, "Nobody approved it at the LingOS PC in time.\n");
                sshkeys::cancel(id);
                return None;
            },
            ReqState::Gone => return None,
        }
        if !netstack::tcp_established_h(sess.handle) {
            log(b"sshd: the client left while its key waited for approval\n");
            sshkeys::cancel(id);
            return None;
        }
        crate::proc::sched::yield_now();
    }
}

/// A `publickey` USERAUTH_REQUEST (`msg[p..]` starts at its boolean). Only
/// OpenSSH security keys (`sk-ssh-ed25519@openssh.com`,
/// `sk-ecdsa-sha2-nistp256@openssh.com`), only for accounts with a security
/// key here (`sshkeys::gate`), and a key not seen before is first allowed at
/// the desktop with a touch of that account's key. `approved` carries an
/// approval given on this connection's query until the client's signature
/// proves it holds the key; only then is the key stored.
fn publickey_login(sess: &mut Session, msg: &[u8], mut p: usize, user: &[u8], sid: &[u8; 32], peer: &Peer, approved: &mut Option<(Vec<u8>, u32)>) -> PkResult {
    use crate::fs::sshkeys;
    use ling_fido::ssh as sk;
    let uname = core::str::from_utf8(user).unwrap_or("");
    let Some(&has_sig) = msg.get(p) else { return PkResult::Failure };
    p += 1;
    let (Some(alg), Some(blob)) = (get_string(msg, &mut p), get_string(msg, &mut p)) else { return PkResult::Failure };
    let Some(kind) = sk::Kind::from_alg(alg) else {
        // plain key files (ssh-ed25519, RSA...) are not taken: no touch proves a person is there
        log(b"sshd: publickey: ");
        log(alg);
        log(b" is not a security key -- not accepted\n");
        return PkResult::Failure;
    };
    if sk::parse_public_key(blob).map(|k| k.kind) != Some(kind) {
        log(b"sshd: publickey: malformed security-key public key\n");
        return PkResult::Failure;
    }
    if !sshkeys::gate(uname) {
        log(b"sshd: publickey refused -- the account has no security key here to approve SSH keys with\n");
        return PkResult::Failure;
    }
    let known = sshkeys::is_approved(uname, blob);
    let approved_here = approved.as_ref().is_some_and(|(b, _)| b == blob);
    if has_sig == 0 {
        // the query: "would this key do?"
        if !known && !approved_here {
            let Some(id) = wait_approval(sess, uname, peer, kind, blob) else { return PkResult::Failure };
            *approved = Some((blob.to_vec(), id));
        }
        let mut r = Vec::new();
        r.push(MSG_USERAUTH_PK_OK);
        put_string(&mut r, alg);
        put_string(&mut r, blob);
        sess.send(&r);
        // the client's own key now waits for its owner's touch
        sess.patient = true;
        return PkResult::PkOk;
    }
    let Some(sig) = get_string(msg, &mut p) else { return PkResult::Failure };
    sess.patient = false;
    let data = sk::signed_data(sid, user, alg, blob);
    let s = match sk::verify(blob, &data, sig) {
        Ok(s) => s,
        Err(e) => {
            log(b"sshd: publickey signature refused: ");
            log(e.words().as_bytes());
            log(b"\n");
            return PkResult::Failure;
        },
    };
    let uv = if s.user_verified() { " + fingerprint/PIN" } else { "" };
    if known {
        if let Err(why) = sshkeys::record_use(uname, blob, s.counter, s.user_verified()) {
            banner(sess, &alloc::format!("{why}\n"));
            return PkResult::Failure;
        }
    } else {
        // signed without asking first: the signature is already proven, ask now
        let id = match approved.as_ref() {
            Some((b, id)) if b == blob => *id,
            _ => match wait_approval(sess, uname, peer, kind, blob) {
                Some(id) => id,
                None => return PkResult::Failure,
            },
        };
        *approved = None;
        if !sshkeys::commit(id, uname, blob, s.counter, s.user_verified()) {
            log(b"sshd: the approval was gone or could not be stored\n");
            return PkResult::Failure;
        }
    }
    sess.authed = true;
    sess.send(&[MSG_USERAUTH_SUCCESS]);
    log(alloc::format!("sshd: security-key auth OK for {uname} ({} {}, touch{uv})\n", kind.short(), sk::fingerprint(blob)).as_bytes());
    PkResult::Success
}

// SSH message numbers (RFC 4253/4252/4254).
const MSG_DISCONNECT: u8 = 1;
const MSG_IGNORE: u8 = 2;
const MSG_UNIMPLEMENTED: u8 = 3;
const MSG_DEBUG: u8 = 4;
const MSG_SERVICE_REQUEST: u8 = 5;
const MSG_SERVICE_ACCEPT: u8 = 6;
const MSG_KEXINIT: u8 = 20;
const MSG_NEWKEYS: u8 = 21;
const MSG_KEX_ECDH_INIT: u8 = 30;
const MSG_KEX_ECDH_REPLY: u8 = 31;
const MSG_USERAUTH_REQUEST: u8 = 50;
const MSG_USERAUTH_FAILURE: u8 = 51;
const MSG_USERAUTH_SUCCESS: u8 = 52;
const MSG_USERAUTH_BANNER: u8 = 53;
/// publickey: "this key would do, sign with it" (shares 60 with INFO_REQUEST)
const MSG_USERAUTH_PK_OK: u8 = 60;
const MSG_USERAUTH_INFO_REQUEST: u8 = 60;
const MSG_USERAUTH_INFO_RESPONSE: u8 = 61;
const MSG_GLOBAL_REQUEST: u8 = 80;
const MSG_REQUEST_FAILURE: u8 = 82;
const MSG_CHANNEL_OPEN: u8 = 90;
const MSG_CHANNEL_OPEN_CONFIRMATION: u8 = 91;
const MSG_CHANNEL_WINDOW_ADJUST: u8 = 93;
const MSG_CHANNEL_DATA: u8 = 94;
const MSG_CHANNEL_EOF: u8 = 96;
const MSG_CHANNEL_CLOSE: u8 = 97;
const MSG_CHANNEL_REQUEST: u8 = 98;
const MSG_CHANNEL_SUCCESS: u8 = 99;
const MSG_CHANNEL_FAILURE: u8 = 100;
const MSG_CHANNEL_OPEN_FAILURE: u8 = 92;

// ── SSH wire encoding helpers ────────────────────────────────────────────────

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s);
}
/// `mpint`: a big-endian integer, minimal, with a leading 0x00 if the top bit
/// of the first byte is set (so it reads as non-negative). `val` is the raw
/// fixed-width big-endian bytes (e.g. the 32-byte X25519 shared secret).
fn put_mpint(out: &mut Vec<u8>, val: &[u8]) {
    let mut i = 0;
    while i < val.len() && val[i] == 0 {
        i += 1;
    }
    let sig = &val[i..];
    if sig.is_empty() {
        put_u32(out, 0);
    } else if sig[0] & 0x80 != 0 {
        put_u32(out, (sig.len() + 1) as u32);
        out.push(0);
        out.extend_from_slice(sig);
    } else {
        put_u32(out, sig.len() as u32);
        out.extend_from_slice(sig);
    }
}

/// Read a `uint32`-length-prefixed string from `buf` at `*pos`, advancing it.
fn get_string<'a>(buf: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    if *pos + 4 > buf.len() {
        return None;
    }
    let len = u32::from_be_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]]) as usize;
    *pos += 4;
    if *pos + len > buf.len() {
        return None;
    }
    let s = &buf[*pos..*pos + len];
    *pos += len;
    Some(s)
}

/// The `ssh-ed25519` public-key blob: `string("ssh-ed25519") ‖ string(pk32)`.
fn host_key_blob(pk: &[u8; 32]) -> Vec<u8> {
    let mut v = Vec::new();
    put_string(&mut v, b"ssh-ed25519");
    put_string(&mut v, pk);
    v
}

/// Persisted 32-byte Ed25519 host-key seed (stable across boots so the client
/// fingerprint doesn't change). Generated from RDRAND on first use.
/// The host key's home. Absolute: a bare name resolves against the console's
/// current directory (`lingfs::resolve`), which would make the key -- and so the
/// fingerprint every client has pinned -- depend on where someone last `cd`'d.
const HOST_SEED_PATH: &str = "/ssh_host_seed";

/// The persisted ed25519 host-key seed, generated once. `None` when there is no
/// stored seed and no hardware randomness to make one: never fall back to a
/// zero or otherwise predictable key, which would let anyone impersonate us.
fn host_seed() -> Option<[u8; 32]> {
    let mut buf = [0u8; 64];
    if let Ok(Some(n)) = lingfs::read_file_all(HOST_SEED_PATH, &mut buf) {
        if n >= 32 {
            let mut s = [0u8; 32];
            s.copy_from_slice(&buf[..32]);
            return Some(s);
        }
    }
    let mut s = [0u8; 32];
    if !crate::crypto::random_bytes(&mut s) {
        return None;
    }
    let _ = lingfs::write_file(HOST_SEED_PATH, &s);
    Some(s)
}

// ── chacha20-poly1305@openssh.com ────────────────────────────────────────────
// Two 256-bit keys: K_2 (main, key[0..32]) encrypts the packet body, K_1
// (header, key[32..64]) encrypts the 4-byte length separately. The nonce is the
// 64-bit packet sequence number, big-endian. Poly1305's key is the first 32
// bytes of K_2's keystream at block 0; the body is encrypted from block 1.

fn chacha_apply(key: &[u8], seq: u32, skip_blocks: bool, data: &mut [u8]) {
    let nonce = (seq as u64).to_be_bytes();
    let mut c = ChaCha20Legacy::new_from_slices(key, &nonce).expect("32-byte key, 8-byte nonce");
    if skip_blocks {
        // Advance past block 0 (used only for the poly1305 key) so the body is
        // encrypted starting at block 1, matching OpenSSH.
        let mut skip = [0u8; 64];
        c.apply_keystream(&mut skip);
    }
    c.apply_keystream(data);
}

fn poly_key(k_main: &[u8], seq: u32) -> [u8; 32] {
    let nonce = (seq as u64).to_be_bytes();
    let mut c = ChaCha20Legacy::new_from_slices(k_main, &nonce).expect("key/nonce");
    let mut pk = [0u8; 32];
    c.apply_keystream(&mut pk);
    pk
}

fn poly_tag(k_main: &[u8], seq: u32, ciphertext: &[u8]) -> [u8; 16] {
    let pk = poly_key(k_main, seq);
    let mac = Poly1305::new_from_slice(&pk).expect("32-byte poly key");
    let tag = mac.compute_unpadded(ciphertext);
    let mut out = [0u8; 16];
    out.copy_from_slice(&tag);
    out
}

// ── Session ──────────────────────────────────────────────────────────────────

struct Session {
    handle: usize,     // netstack socket handle for this connection
    rx: Vec<u8>,       // received bytes not yet consumed
    send_seq: u32,
    recv_seq: u32,
    enc_out: bool,     // encrypt outgoing (after we send NEWKEYS)
    enc_in: bool,      // decrypt incoming (after we receive NEWKEYS)
    k_s2c: [u8; 64],   // server->client key (main[0..32] || header[32..64])
    k_c2s: [u8; 64],   // client->server key
    authed: bool,      // signed in: idle is fine, the session lasts while TCP does
    patient: bool,     // waiting on a touch at the client's own security key: allow a longer quiet spell
    peer_window: u64,  // bytes the client still accepts on our channel
    deferred: Vec<Vec<u8>>, // messages read while waiting for window space
    /// An interactive program (`claude`) holding the session, and our channel to stream its output on.
    tty: Option<crate::drivers::sshline::TtyRef>,
    tty_chan: u32,
    /// The interactive program has ended: `recv` returned None for that, not for a lost client.
    tty_ended: bool,
}

impl Session {
    /// Send what the interactive program wrote (as much as the client's window takes now --
    /// never waiting: this runs inside `ensure`, so it must not read the socket itself).
    fn tty_flush(&mut self) {
        let Some(t) = self.tty.clone() else { return };
        let data: Vec<u8> = {
            let mut tb = t.borrow_mut();
            if tb.output.is_empty() {
                return;
            }
            let room = self.peer_window.min(16 * 1024) as usize;
            if room == 0 {
                return;
            }
            // '\n' -> "\r\n" for the client's pty; stop at `room` bytes of converted text
            let mut out = Vec::new();
            let mut used = 0usize;
            for &b in tb.output.iter() {
                let need = if b == b'\n' { 2 } else { 1 };
                if out.len() + need > room {
                    break;
                }
                if b == b'\n' {
                    out.push(b'\r');
                }
                out.push(b);
                used += 1;
            }
            tb.output.drain(..used);
            out
        };
        let mut off = 0;
        while off < data.len() {
            let end = (off + 1024).min(data.len());
            self.peer_window -= (end - off) as u64;
            let mut r = Vec::new();
            r.push(MSG_CHANNEL_DATA);
            put_u32(&mut r, self.tty_chan);
            put_string(&mut r, &data[off..end]);
            self.send(&r);
            off = end;
        }
    }

    fn new(handle: usize) -> Session {
        Session {
            handle,
            rx: Vec::new(),
            send_seq: 0,
            recv_seq: 0,
            enc_out: false,
            enc_in: false,
            k_s2c: [0; 64],
            k_c2s: [0; 64],
            authed: false,
            patient: false,
            peer_window: 1 << 20,
            deferred: Vec::new(),
            tty: None,
            tty_chan: 0,
            tty_ended: false,
        }
    }

    /// Ensure at least `n` bytes are buffered, reading from TCP with a bounded
    /// idle tolerance. Returns false if the peer went silent.
    fn ensure(&mut self, n: usize) -> bool {
        let mut idle = 0;
        while self.rx.len() < n {
            let mut tmp = [0u8; 4096];
            // an interactive program (claude) streams output while the user is quiet: look
            // for it every 20 ms instead of sleeping 2 s on the socket
            let wait = if self.tty.is_some() { 20_000 } else { 2_000_000 };
            let got = netstack::tcp_read_h(self.handle, &mut tmp, wait);
            if self.tty.is_some() {
                self.tty_flush();
                // the program finished and all its output went out: back to the shell loop (the
                // partial packet, if any, stays in rx -- recv starts it again next time)
                let finished = self.tty.as_ref().is_some_and(|t| { let t = t.borrow(); t.done && t.output.is_empty() });
                if finished && got == 0 {
                    self.tty = None;
                    self.tty_ended = true;
                    return false;
                }
            }
            if got == 0 {
                if !netstack::tcp_established_h(self.handle) {
                    return false; // the client went away
                }
                if self.authed {
                    continue; // a signed-in shell may sit idle as long as it likes
                }
                idle += 1;
                if idle > if self.patient { 45 } else { 8 } {
                    return false;
                }
                continue;
            }
            idle = 0;
            self.rx.extend_from_slice(&tmp[..got]);
        }
        true
    }

    /// Receive one packet, returning its payload (the message body, i.e. the
    /// byte after padding_length onward). Handles cleartext and encrypted.
    fn recv(&mut self) -> Option<Vec<u8>> {
        if self.enc_in {
            // Decrypt the 4-byte length with the header key (nonce = recv_seq).
            if !self.ensure(4) {
                return None;
            }
            let mut len4 = [self.rx[0], self.rx[1], self.rx[2], self.rx[3]];
            chacha_apply(&self.k_c2s[32..64], self.recv_seq, false, &mut len4);
            let pkt_len = u32::from_be_bytes(len4) as usize;
            if pkt_len < 8 || pkt_len > 65536 {
                return None;
            }
            if !self.ensure(4 + pkt_len + 16) {
                return None;
            }
            // Verify Poly1305 over the encrypted (length || body).
            let tag = poly_tag(&self.k_c2s[0..32], self.recv_seq, &self.rx[0..4 + pkt_len]);
            if tag[..] != self.rx[4 + pkt_len..4 + pkt_len + 16] {
                return None; // bad MAC
            }
            // Decrypt the body (block 1 onward).
            let mut body = self.rx[4..4 + pkt_len].to_vec();
            chacha_apply(&self.k_c2s[0..32], self.recv_seq, true, &mut body);
            let padlen = body[0] as usize;
            if 1 + padlen > body.len() {
                return None;
            }
            let payload = body[1..body.len() - padlen].to_vec();
            self.rx.drain(0..4 + pkt_len + 16);
            self.recv_seq = self.recv_seq.wrapping_add(1);
            Some(payload)
        } else {
            if !self.ensure(4) {
                return None;
            }
            let pkt_len =
                u32::from_be_bytes([self.rx[0], self.rx[1], self.rx[2], self.rx[3]]) as usize;
            if pkt_len < 8 || pkt_len > 65536 {
                return None;
            }
            if !self.ensure(4 + pkt_len) {
                return None;
            }
            let padlen = self.rx[4] as usize;
            if 5 + (pkt_len - 1 - padlen) > 4 + pkt_len {
                return None;
            }
            let payload = self.rx[5..4 + pkt_len - padlen].to_vec();
            self.rx.drain(0..4 + pkt_len);
            self.recv_seq = self.recv_seq.wrapping_add(1);
            Some(payload)
        }
    }

    /// Frame and send one payload as an SSH packet (cleartext or encrypted).
    fn send(&mut self, payload: &[u8]) -> bool {
        let ok = if self.enc_out {
            // padding aligns (padlen_byte + payload + padding) to 8, min 4; the
            // separately-encrypted length field is excluded from the modulus.
            let mut padlen = 8 - ((1 + payload.len()) % 8);
            if padlen < 4 {
                padlen += 8;
            }
            let pkt_len = 1 + payload.len() + padlen;
            let mut body = Vec::with_capacity(pkt_len);
            body.push(padlen as u8);
            body.extend_from_slice(payload);
            let mut pad = alloc::vec![0u8; padlen];
            let _ = crate::crypto::random_bytes(&mut pad);
            body.extend_from_slice(&pad);
            // Encrypt length (header key) and body (main key, block 1+).
            let mut len4 = (pkt_len as u32).to_be_bytes();
            chacha_apply(&self.k_s2c[32..64], self.send_seq, false, &mut len4);
            chacha_apply(&self.k_s2c[0..32], self.send_seq, true, &mut body);
            let mut wire = Vec::with_capacity(4 + pkt_len + 16);
            wire.extend_from_slice(&len4);
            wire.extend_from_slice(&body);
            let tag = poly_tag(&self.k_s2c[0..32], self.send_seq, &wire);
            wire.extend_from_slice(&tag);
            netstack::tcp_write_h(self.handle, &wire)
        } else {
            // include the 4-byte length field in the block-8 alignment.
            let mut padlen = 8 - ((4 + 1 + payload.len()) % 8);
            if padlen < 4 {
                padlen += 8;
            }
            let pkt_len = 1 + payload.len() + padlen;
            let mut wire = Vec::with_capacity(4 + pkt_len);
            put_u32(&mut wire, pkt_len as u32);
            wire.push(padlen as u8);
            wire.extend_from_slice(payload);
            let mut pad = alloc::vec![0u8; padlen];
            let _ = crate::crypto::random_bytes(&mut pad);
            wire.extend_from_slice(&pad);
            netstack::tcp_write_h(self.handle, &wire)
        };
        if ok {
            self.send_seq = self.send_seq.wrapping_add(1);
        }
        ok
    }
}

/// Build our SSH_MSG_KEXINIT payload (the algorithm-negotiation message body).
fn build_kexinit() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(MSG_KEXINIT);
    let mut cookie = [0u8; 16];
    let _ = crate::crypto::random_bytes(&mut cookie);
    p.extend_from_slice(&cookie);
    put_string(&mut p, b"curve25519-sha256,curve25519-sha256@libssh.org");
    put_string(&mut p, b"ssh-ed25519");
    put_string(&mut p, b"chacha20-poly1305@openssh.com");
    put_string(&mut p, b"chacha20-poly1305@openssh.com");
    put_string(&mut p, b"hmac-sha2-256"); // ignored: chachapoly is AEAD
    put_string(&mut p, b"hmac-sha2-256");
    put_string(&mut p, b"none");
    put_string(&mut p, b"none");
    put_string(&mut p, b""); // languages c2s
    put_string(&mut p, b""); // languages s2c
    p.push(0); // first_kex_packet_follows
    put_u32(&mut p, 0); // reserved
    p
}

fn kdf(k_mpint: &[u8], h: &[u8; 32], letter: u8, sid: &[u8; 32], out_len: usize) -> [u8; 64] {
    let mut input = Vec::new();
    input.extend_from_slice(k_mpint);
    input.extend_from_slice(h);
    input.push(letter);
    input.extend_from_slice(sid);
    let mut key: Vec<u8> = crate::crypto::sha256(&input).to_vec();
    while key.len() < out_len {
        let mut inp2 = Vec::new();
        inp2.extend_from_slice(k_mpint);
        inp2.extend_from_slice(h);
        inp2.extend_from_slice(&key);
        key.extend_from_slice(&crate::crypto::sha256(&inp2));
    }
    let _ = out_len;
    let mut out = [0u8; 64];
    out.copy_from_slice(&key[..64]);
    out
}

fn log(msg: &[u8]) {
    crate::console_write(msg);
}

/// Read the client's SSH version line, keeping any bytes after the CR/LF (the
/// client's first packet) in the session RX buffer. Returns V_C (no CR/LF).
fn read_version(sess: &mut Session) -> Option<Vec<u8>> {
    loop {
        // Find a LF in whatever's buffered.
        if let Some(nl) = sess.rx.iter().position(|&b| b == b'\n') {
            let mut line = sess.rx[..nl].to_vec();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            sess.rx.drain(0..nl + 1);
            return Some(line);
        }
        if !sess.ensure(sess.rx.len() + 1) {
            return None;
        }
    }
}

/// Serve one inbound SSH connection to completion (up to `budget_us` for the
/// accept). Returns Ok(()) on a clean session, or an error string.
pub fn serve(budget_us: u64) -> Result<(), &'static str> {
    let handle = netstack::tcp_listen_accept(port(), budget_us).ok_or("no client connected")?;
    log(b"sshd: client connected -- starting SSH transport\n");

    let mut sess = Session::new(handle);

    // 1. Version exchange.
    let mut v_s = Vec::new();
    v_s.extend_from_slice(IDENT);
    let mut ident_line = v_s.clone();
    ident_line.extend_from_slice(b"\r\n");
    if !netstack::tcp_write_h(handle, &ident_line) {
        netstack::tcp_close_h(handle);
        return Err("write version failed");
    }
    let Some(v_c) = read_version(&mut sess) else {
        netstack::tcp_close_h(handle);
        return Err("no client version");
    };
    log(b"sshd: client version: ");
    log(&v_c);
    log(b"\n");

    // 2. KEXINIT exchange.
    let i_s = build_kexinit();
    if !sess.send(&i_s) {
        netstack::tcp_close_h(handle);
        return Err("send KEXINIT failed");
    }
    let Some(i_c) = sess.recv() else {
        netstack::tcp_close_h(handle);
        return Err("no client KEXINIT");
    };
    if i_c.first() != Some(&MSG_KEXINIT) {
        netstack::tcp_close_h(handle);
        return Err("expected KEXINIT");
    }
    log(b"sshd: KEXINIT exchanged\n");

    // 3. curve25519-sha256 key exchange.
    let Some(ecdh) = sess.recv() else {
        netstack::tcp_close_h(handle);
        return Err("no KEX_ECDH_INIT");
    };
    if ecdh.first() != Some(&MSG_KEX_ECDH_INIT) {
        netstack::tcp_close_h(handle);
        return Err("expected KEX_ECDH_INIT");
    }
    let mut pos = 1usize;
    let Some(q_c) = get_string(&ecdh, &mut pos) else {
        netstack::tcp_close_h(handle);
        return Err("bad ECDH_INIT");
    };
    if q_c.len() != 32 {
        netstack::tcp_close_h(handle);
        return Err("bad client eph key");
    }
    let mut q_c_arr = [0u8; 32];
    q_c_arr.copy_from_slice(q_c);

    // Our ephemeral X25519 keypair + shared secret.
    let mut eph_secret = [0u8; 32];
    if !crate::crypto::random_bytes(&mut eph_secret) {
        log(b"sshd: no hardware random source -- cannot run the key exchange
");
        netstack::tcp_close_h(handle);
        return Err("no hardware random source");
    }
    let q_s = crate::crypto::x25519_public(eph_secret);
    let k = crate::crypto::x25519(eph_secret, q_c_arr);

    // Host key + exchange hash H.
    let Some(seed) = host_seed() else {
        log(b"sshd: no host key and no random source to make one\n");
        netstack::tcp_close_h(handle);
        return Err("no host key");
    };
    let host_pub = crate::ed25519::public_from_seed(&seed);
    let k_s = host_key_blob(&host_pub);
    let mut hbuf = Vec::new();
    put_string(&mut hbuf, &v_c);
    put_string(&mut hbuf, &v_s);
    put_string(&mut hbuf, &i_c);
    put_string(&mut hbuf, &i_s);
    put_string(&mut hbuf, &k_s);
    put_string(&mut hbuf, &q_c_arr);
    put_string(&mut hbuf, &q_s);
    put_mpint(&mut hbuf, &k);
    let h = crate::crypto::sha256(&hbuf);
    let sig = crate::ed25519::sign(&seed, &h);
    let mut sig_blob = Vec::new();
    put_string(&mut sig_blob, b"ssh-ed25519");
    put_string(&mut sig_blob, &sig);

    // KEX_ECDH_REPLY.
    let mut reply = Vec::new();
    reply.push(MSG_KEX_ECDH_REPLY);
    put_string(&mut reply, &k_s);
    put_string(&mut reply, &q_s);
    put_string(&mut reply, &sig_blob);
    if !sess.send(&reply) {
        netstack::tcp_close_h(handle);
        return Err("send ECDH_REPLY failed");
    }

    // 4. NEWKEYS. Derive directional keys; session_id = H (first kex).
    let mut k_mpint = Vec::new();
    put_mpint(&mut k_mpint, &k);
    let k_c2s = kdf(&k_mpint, &h, b'C', &h, 64);
    let k_s2c = kdf(&k_mpint, &h, b'D', &h, 64);

    if !sess.send(&[MSG_NEWKEYS]) {
        netstack::tcp_close_h(handle);
        return Err("send NEWKEYS failed");
    }
    sess.enc_out = true;
    sess.k_s2c = k_s2c;
    sess.k_c2s = k_c2s;
    let Some(nk) = sess.recv() else {
        netstack::tcp_close_h(handle);
        return Err("no client NEWKEYS");
    };
    if nk.first() != Some(&MSG_NEWKEYS) {
        netstack::tcp_close_h(handle);
        return Err("expected NEWKEYS");
    }
    sess.enc_in = true;
    log(b"sshd: key exchange complete -- encrypted channel up\n");

    // 5. Password userauth, verified against the user DB (`users::verify`). A
    // `none` attempt or a wrong password gets USERAUTH_FAILURE offering
    // `password`; six failures disconnect. So an account with a real password
    // must exist (the Live desktop's `live`/`live`, or the root/user the
    // installer created) -- a passwordless login is not offered.
    let mut username = String::new();
    let mut attempts = 0u32;
    // publickey: OpenSSH offers every key it has, one after another; those are
    // counted apart so a few plain keys can't use up the password tries
    let mut pk_attempts = 0u32;
    let peer = Peer {
        ip: netstack::tcp_remote_h(handle).map(|(ip, _)| alloc::format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])).unwrap_or_else(|| String::from("?")),
        version: String::from_utf8_lossy(&v_c).into_owned(),
    };
    // an approval given on a query, until the client's signed request proves the key
    let mut approved: Option<(Vec<u8>, u32)> = None;
    let drop_approval = |a: &Option<(Vec<u8>, u32)>| {
        if let Some((_, id)) = a {
            crate::fs::sshkeys::cancel(*id);
        }
    };
    loop {
        let Some(msg) = sess.recv() else {
            drop_approval(&approved);
            netstack::tcp_close_h(handle);
            return Err("disconnected during auth");
        };
        match msg.first().copied() {
            Some(MSG_SERVICE_REQUEST) => {
                let mut p = 1;
                let name = get_string(&msg, &mut p).unwrap_or(b"");
                let mut r = Vec::new();
                r.push(MSG_SERVICE_ACCEPT);
                put_string(&mut r, name);
                sess.send(&r);
            },
            Some(MSG_USERAUTH_REQUEST) => {
                let mut p = 1;
                let user = get_string(&msg, &mut p).unwrap_or(b"").to_vec();
                let _service = get_string(&msg, &mut p);
                let method = get_string(&msg, &mut p).unwrap_or(b"").to_vec();
                let uname = core::str::from_utf8(&user).unwrap_or("");
                if method == b"publickey" {
                    match publickey_login(&mut sess, &msg, p, &user, &h, &peer, &mut approved) {
                        PkResult::Success => {
                            username = uname.to_string();
                            break;
                        },
                        PkResult::PkOk => continue,
                        PkResult::Failure => {
                            pk_attempts += 1;
                            let mut r = Vec::new();
                            r.push(MSG_USERAUTH_FAILURE);
                            put_string(&mut r, b"publickey,keyboard-interactive,password");
                            r.push(0);
                            sess.send(&r);
                            if pk_attempts >= 20 {
                                drop_approval(&approved);
                                netstack::tcp_close_h(handle);
                                return Err("too many publickey failures");
                            }
                            continue;
                        },
                    }
                }
                if method == b"keyboard-interactive" {
                    // the password, then the authenticator code: asked as separate prompts, so
                    // ssh / sftp / scp / WinSCP / FileZilla all show a normal "Verification code:" box
                    if kbdint_login(&mut sess, uname) {
                        username = uname.to_string();
                        sess.authed = true;
                        sess.send(&[MSG_USERAUTH_SUCCESS]);
                        log(b"sshd: keyboard-interactive auth OK for ");
                        log(&user);
                        log(b"\n");
                        break;
                    }
                } else if method == b"password" {
                    let mut pp = p + 1; // skip the boolean FALSE
                    let pw = get_string(&msg, &mut pp).unwrap_or(b"");
                    let pws = core::str::from_utf8(pw).unwrap_or("");
                    if !pws.is_empty() && password_login(uname, pws) {
                        username = uname.to_string();
                        sess.authed = true;
                        sess.send(&[MSG_USERAUTH_SUCCESS]);
                        log(b"sshd: password auth OK for ");
                        log(&user);
                        log(b"\n");
                        break;
                    }
                }
                attempts += 1;
                let mut r = Vec::new();
                r.push(MSG_USERAUTH_FAILURE);
                put_string(&mut r, b"publickey,keyboard-interactive,password");
                r.push(0); // partial success = false
                sess.send(&r);
                if attempts >= 6 {
                    drop_approval(&approved);
                    netstack::tcp_close_h(handle);
                    return Err("too many auth failures");
                }
            },
            Some(MSG_IGNORE) | Some(MSG_DEBUG) => {},
            Some(MSG_GLOBAL_REQUEST) => {
                sess.send(&[MSG_REQUEST_FAILURE]);
            },
            Some(MSG_DISCONNECT) => {
                drop_approval(&approved);
                netstack::tcp_close_h(handle);
                return Ok(());
            },
            _ => {},
        }
    }
    drop_approval(&approved);

    // 6. Session channel + interactive shell.
    run_channel(&mut sess, &username);
    netstack::tcp_close_h(handle);
    log(b"sshd: session closed\n");
    Ok(())
}

/// Handle the connection layer: a `session` channel, then an interactive
/// shell, a one-shot `exec` command, or the `sftp` subsystem.
fn run_channel(sess: &mut Session, username: &str) {
    let mut peer_chan: u32 = 0;
    let my_chan: u32 = 0;
    let mut have_channel = false;
    let mut banner_sent = false;
    // Per-session working directory (lingfs path, no leading slash; "" = root).
    // Kept local so an SSH session doesn't move the console shell's CWD.
    let mut cwd = alloc::format!("home/{username}");
    // the zsh-style line editor (sshline.rs): history from ~/.lsh_history, `cd -` target,
    // whether the last command worked (the prompt arrow turns red if not)
    let mut ed = crate::drivers::sshline::Editor::new(crate::drivers::sshline::load_history(username));
    let mut prev_cwd = cwd.clone();
    let mut last_ok = true;
    let mut tail = String::new();
    let mut sftp: Option<crate::drivers::sftp::Sftp> = None;
    // channel data taken since our last window grant
    let mut consumed: u32 = 0;

    loop {
        let msg = if !sess.deferred.is_empty() {
            sess.deferred.remove(0)
        } else {
            match sess.recv() {
                Some(m) => m,
                // an interactive program (claude) finished: back to the prompt
                None if sess.tty_ended => {
                    sess.tty_ended = false;
                    tail = show_prompt(sess, peer_chan, username, &cwd, last_ok);
                    continue;
                },
                None => {
                    crate::drivers::sshline::save_history(username, ed.history());
                    if let Some(s) = sftp.as_mut() {
                        s.finish();
                    }
                    return;
                },
            }
        };
        match msg.first().copied() {
            Some(MSG_CHANNEL_OPEN) => {
                let mut p = 1;
                let ctype = get_string(&msg, &mut p).unwrap_or(b"").to_vec();
                let rd = |o: usize| -> u32 {
                    if o + 4 <= msg.len() {
                        u32::from_be_bytes([msg[o], msg[o + 1], msg[o + 2], msg[o + 3]])
                    } else {
                        0
                    }
                };
                peer_chan = rd(p);
                let window = rd(p + 4);
                if ctype == b"session" && !have_channel {
                    sess.peer_window = window as u64;
                    let mut r = Vec::new();
                    r.push(MSG_CHANNEL_OPEN_CONFIRMATION);
                    put_u32(&mut r, peer_chan);
                    put_u32(&mut r, my_chan);
                    put_u32(&mut r, 0x0010_0000); // window we grant
                    put_u32(&mut r, 0x0000_8000); // max packet
                    sess.send(&r);
                    have_channel = true;
                } else {
                    let mut r = Vec::new();
                    r.push(MSG_CHANNEL_OPEN_FAILURE);
                    put_u32(&mut r, peer_chan);
                    put_u32(&mut r, 3); // SSH_OPEN_UNKNOWN_CHANNEL_TYPE
                    put_string(&mut r, b"only one session channel");
                    put_string(&mut r, b"en");
                    sess.send(&r);
                }
            },
            Some(MSG_CHANNEL_REQUEST) => {
                // uint32 chan, string type, bool want_reply, type-specific...
                let mut p = 1 + 4;
                let rtype = get_string(&msg, &mut p).unwrap_or(b"").to_vec();
                let want_reply = msg.get(p).copied().unwrap_or(0) != 0;
                p += 1;
                let rd32 = |o: usize| -> u32 { if o + 4 <= msg.len() { u32::from_be_bytes([msg[o], msg[o + 1], msg[o + 2], msg[o + 3]]) } else { 0 } };
                // terminal size: window-change is (cols, rows, ..) right here; pty-req has TERM first
                if rtype == b"window-change" && rd32(p) > 0 {
                    ed.cols = rd32(p) as usize;
                    if let Some(t) = sess.tty.as_ref() {
                        let mut t = t.borrow_mut();
                        t.cols = rd32(p) as u16;
                        t.rows = rd32(p + 4) as u16;
                    }
                }
                let arg = get_string(&msg, &mut p).unwrap_or(b"").to_vec();
                if rtype == b"pty-req" && rd32(p) > 0 {
                    ed.cols = rd32(p) as usize;
                }
                let ok = match rtype.as_slice() {
                    b"shell" | b"exec" | b"pty-req" | b"env" | b"window-change" => have_channel,
                    b"subsystem" => have_channel && arg == b"sftp" && sftp.is_none(),
                    _ => false,
                };
                if want_reply {
                    let mut r = Vec::new();
                    r.push(if ok { MSG_CHANNEL_SUCCESS } else { MSG_CHANNEL_FAILURE });
                    put_u32(&mut r, peer_chan);
                    sess.send(&r);
                }
                if !ok {
                    continue;
                }
                match rtype.as_slice() {
                    b"shell" if !banner_sent => {
                        banner_sent = true;
                        use crate::drivers::ansi;
                        let welcome = alloc::format!(
                            "\r\n  {}  {}welcome, {}{}{}\r\n  {}type 'help' for commands -- Tab completes, Up/Down history, a && b, cd -{}\r\n",
                            ansi::gradient("LingOS SSH"),
                            ansi::DIM,
                            ansi::fg(ansi::GREEN),
                            username,
                            ansi::RESET,
                            ansi::DIM,
                            ansi::RESET
                        );
                        channel_write(sess, peer_chan, welcome.as_bytes());
                        // the message of the day (motd set <file> / motd clear)
                        if let Some(m) = crate::motd::get() {
                            channel_write(sess, peer_chan, b"\r\n");
                            channel_write_text(sess, peer_chan, &m);
                        }
                        tail = show_prompt(sess, peer_chan, username, &cwd, true);
                    },
                    b"exec" => {
                        // `ssh host command`: run it, report, close
                        let cmd = String::from_utf8_lossy(&arg).trim().to_string();
                        let _ = run_cmd(sess, peer_chan, &cmd, username, &mut cwd);
                        let mut st = Vec::new();
                        st.push(MSG_CHANNEL_REQUEST);
                        put_u32(&mut st, peer_chan);
                        put_string(&mut st, b"exit-status");
                        st.push(0);
                        put_u32(&mut st, 0);
                        sess.send(&st);
                        close_channel(sess, peer_chan);
                        return;
                    },
                    b"subsystem" => {
                        log(b"sshd: sftp session for ");
                        log(username.as_bytes());
                        log(b"\n");
                        sftp = Some(crate::drivers::sftp::Sftp::new(username));
                    },
                    _ => {},
                }
            },
            Some(MSG_CHANNEL_DATA) => {
                let mut p = 1 + 4;
                let data = get_string(&msg, &mut p).unwrap_or(b"").to_vec();
                // grant the client more window as we take its data (uploads
                // would stall at the first megabyte otherwise)
                consumed = consumed.saturating_add(data.len() as u32);
                if consumed >= 0x4_0000 {
                    let mut r = Vec::new();
                    r.push(MSG_CHANNEL_WINDOW_ADJUST);
                    put_u32(&mut r, peer_chan);
                    put_u32(&mut r, consumed);
                    sess.send(&r);
                    consumed = 0;
                }
                if let Some(s) = sftp.as_mut() {
                    for resp in s.feed(&data) {
                        channel_write(sess, peer_chan, &resp);
                    }
                    continue;
                }
                // an interactive program (claude) has the session: its input, raw
                if let Some(t) = sess.tty.as_ref() {
                    t.borrow_mut().input.extend_from_slice(&data);
                    continue;
                }
                use crate::drivers::sshline::Key;
                let mut i = 0;
                while i < data.len() {
                    let b = data[i];
                    i += 1;
                    match ed.key(b) {
                        Key::None => {},
                        Key::Redraw => {
                            let r = ed.render(&tail);
                            channel_write(sess, peer_chan, &r);
                        },
                        Key::Echo(c) => {
                            let mut u = [0u8; 4];
                            channel_write(sess, peer_chan, c.encode_utf8(&mut u).as_bytes());
                        },
                        Key::Bell => channel_write(sess, peer_chan, b"\x07"),
                        Key::Line(l) => {
                            channel_write(sess, peer_chan, b"\r\n");
                            match run_line(sess, peer_chan, &l, username, &mut cwd, &mut prev_cwd, ed.history(), ed.cols) {
                                LineEnd::Logout => {
                                    crate::drivers::sshline::save_history(username, ed.history());
                                    close_channel(sess, peer_chan);
                                    return;
                                },
                                LineEnd::Program => {
                                    // the program owns the session now; the rest of this packet is its input
                                    if let Some(t) = sess.tty.as_ref() {
                                        t.borrow_mut().input.extend_from_slice(&data[i..]);
                                    }
                                    break;
                                },
                                LineEnd::Done(ok) => {
                                    last_ok = ok;
                                    crate::drivers::sshline::save_history(username, ed.history());
                                    tail = show_prompt(sess, peer_chan, username, &cwd, last_ok);
                                },
                            }
                        },
                        Key::Interrupt => {
                            channel_write(sess, peer_chan, b"^C\r\n");
                            tail = show_prompt(sess, peer_chan, username, &cwd, last_ok);
                        },
                        Key::Eof => {
                            channel_write(sess, peer_chan, b"logout\r\n");
                            crate::drivers::sshline::save_history(username, ed.history());
                            close_channel(sess, peer_chan);
                            return;
                        },
                        Key::Clear => {
                            channel_write(sess, peer_chan, b"\x1b[2J\x1b[H");
                            tail = show_prompt(sess, peer_chan, username, &cwd, last_ok);
                            let r = ed.render(&tail);
                            channel_write(sess, peer_chan, &r);
                        },
                        Key::Tab => {
                            let (start, word) = ed.word_before();
                            let (done, list) = crate::drivers::sshline::complete(&word, ed.in_command_word(), &cwd, username);
                            if let Some(w) = done {
                                ed.replace_word(start, &w);
                            } else if list.is_empty() {
                                channel_write(sess, peer_chan, b"\x07");
                            } else {
                                // the candidates in columns, then the prompt and line again
                                use crate::drivers::ansi;
                                let wmax = list.iter().map(|s| ansi::width(s)).max().unwrap_or(1) + 2;
                                let per = (ed.cols.max(20) / wmax).max(1);
                                let mut s = String::from("\r\n");
                                for (k, c) in list.iter().take(200).enumerate() {
                                    let col = if c.ends_with('/') { ansi::BLUE } else { ansi::ROYGBIV[k % 6] };
                                    s.push_str(&ansi::paint(col, c));
                                    for _ in ansi::width(c)..wmax {
                                        s.push(' ');
                                    }
                                    if (k + 1) % per == 0 {
                                        s.push_str("\r\n");
                                    }
                                }
                                if list.len() > 200 {
                                    s.push_str(&alloc::format!("\r\n... and {} more", list.len() - 200));
                                }
                                channel_write(sess, peer_chan, s.as_bytes());
                                tail = show_prompt(sess, peer_chan, username, &cwd, last_ok);
                            }
                            let r = ed.render(&tail);
                            channel_write(sess, peer_chan, &r);
                        },
                    }
                }
            },
            Some(MSG_CHANNEL_EOF) => {
                // the sftp client is done: store what is open, report success
                // and close our side (the client waits for that)
                if let Some(s) = sftp.as_mut() {
                    s.finish();
                    let mut st = Vec::new();
                    st.push(MSG_CHANNEL_REQUEST);
                    put_u32(&mut st, peer_chan);
                    put_string(&mut st, b"exit-status");
                    st.push(0);
                    put_u32(&mut st, 0);
                    sess.send(&st);
                    close_channel(sess, peer_chan);
                    return;
                }
            },
            Some(MSG_CHANNEL_CLOSE) => {
                if let Some(s) = sftp.as_mut() {
                    s.finish();
                }
                let mut cl = Vec::new();
                cl.push(MSG_CHANNEL_CLOSE);
                put_u32(&mut cl, peer_chan);
                sess.send(&cl);
                return;
            },
            Some(MSG_CHANNEL_WINDOW_ADJUST) => {
                let p = 1 + 4;
                if p + 4 <= msg.len() {
                    let add = u32::from_be_bytes([msg[p], msg[p + 1], msg[p + 2], msg[p + 3]]);
                    sess.peer_window = sess.peer_window.saturating_add(add as u64);
                }
            },
            Some(MSG_GLOBAL_REQUEST) => {
                let mut p = 1;
                let _name = get_string(&msg, &mut p);
                if msg.get(p).copied().unwrap_or(0) != 0 {
                    sess.send(&[MSG_REQUEST_FAILURE]);
                }
            },
            Some(MSG_DISCONNECT) => {
                if let Some(s) = sftp.as_mut() {
                    s.finish();
                }
                return;
            },
            _ => {},
        }
    }
}

fn close_channel(sess: &mut Session, chan: u32) {
    let mut eof = Vec::new();
    eof.push(MSG_CHANNEL_EOF);
    put_u32(&mut eof, chan);
    sess.send(&eof);
    let mut cl = Vec::new();
    cl.push(MSG_CHANNEL_CLOSE);
    put_u32(&mut cl, chan);
    sess.send(&cl);
}

/// Write channel data, split into <=1024-byte pieces so each SSH packet stays
/// under one TCP segment (`netstack::tcp_write` is single-segment).
fn channel_write(sess: &mut Session, chan: u32, data: &[u8]) {
    let mut off = 0usize;
    while off < data.len() {
        let end = (off + 1024).min(data.len());
        // never send past the client's window: wait for its WINDOW_ADJUST,
        // keeping anything else that arrives meanwhile for the main loop
        while sess.peer_window < (end - off) as u64 {
            let Some(m) = sess.recv() else { return };
            if m.first().copied() == Some(MSG_CHANNEL_WINDOW_ADJUST) && m.len() >= 9 {
                let add = u32::from_be_bytes([m[5], m[6], m[7], m[8]]);
                sess.peer_window = sess.peer_window.saturating_add(add as u64);
            } else {
                sess.deferred.push(m);
            }
        }
        sess.peer_window -= (end - off) as u64;
        let mut r = Vec::new();
        r.push(MSG_CHANNEL_DATA);
        put_u32(&mut r, chan);
        put_string(&mut r, &data[off..end]);
        sess.send(&r);
        off = end;
    }
}

/// Like `channel_write` but converts bare `\n` to `\r\n` so file contents (which
/// use Unix line endings) render correctly on the client's pty.
fn channel_write_text(sess: &mut Session, chan: u32, data: &[u8]) {
    let mut out = Vec::with_capacity(data.len() + 16);
    for &b in data {
        match b {
            b'\r' => {},
            b'\n' => {
                out.push(b'\r');
                out.push(b'\n');
            },
            _ => out.push(b),
        }
    }
    channel_write(sess, chan, &out);
}

/// The zsh-style two-line prompt (sshline::prompt); returns its last line, which the
/// editor redraws with the input.
fn show_prompt(sess: &mut Session, chan: u32, user: &str, cwd: &str, ok: bool) -> String {
    let (head, tail) = crate::drivers::sshline::prompt(user, cwd, ok);
    let mut p = head;
    p.push_str("\r\n");
    p.push_str(&tail);
    channel_write(sess, chan, p.as_bytes());
    tail
}

/// Whether the last `run_cmd` worked (LingOS commands have no exit status: see
/// sshline::looks_failed). For `&&` and the prompt's red arrow.
static mut LAST_OK: bool = true;

enum LineEnd {
    /// Ran; the last piece worked or not.
    Done(bool),
    Logout,
    /// An interactive program (claude) took the session over.
    Program,
}

/// One input line: `a ; b && c` pieces, `cd -`, `history`, `clear`, interactive programs,
/// everything else through `run_cmd`.
fn run_line(sess: &mut Session, chan: u32, line: &str, user: &str, cwd: &mut String, prev: &mut String, hist: &[String], cols: usize) -> LineEnd {
    use crate::drivers::sshline;
    let mut ok = true;
    for (piece, need_ok) in sshline::split_chain(line) {
        if need_ok && !ok {
            continue; // `a && b` with a failed: b does not run (a `; c` after it still does)
        }
        let (name, arg) = match piece.find(' ') {
            Some(i) => (&piece[..i], piece[i + 1..].trim()),
            None => (piece.as_str(), ""),
        };
        if name == "cd" && arg == "-" {
            let target = prev.clone();
            if target.is_empty() || crate::vfs::is_dir(&target) {
                *prev = core::mem::replace(cwd, target);
                channel_write_text(sess, chan, alloc::format!("/{cwd}\n").as_bytes());
                ok = true;
            } else {
                channel_write_text(sess, chan, b"cd: -: no previous directory\n");
                ok = false;
            }
            continue;
        }
        if name == "history" {
            let mut s = String::new();
            let start = hist.len().saturating_sub(50);
            for (k, h) in hist.iter().enumerate().skip(start) {
                s.push_str(&alloc::format!("{}{:>5}{}  {h}\n", crate::drivers::ansi::DIM, k + 1, crate::drivers::ansi::RESET));
            }
            channel_write_text(sess, chan, s.as_bytes());
            ok = true;
            continue;
        }
        if name == "clear" {
            channel_write(sess, chan, b"\x1b[2J\x1b[H");
            ok = true;
            continue;
        }
        if let Some(start) = sshline::program(name) {
            let tty = alloc::rc::Rc::new(core::cell::RefCell::new(sshline::Tty { input: Vec::new(), output: Vec::new(), done: false, cols: cols as u16, rows: 24 }));
            sess.tty = Some(tty.clone());
            sess.tty_chan = chan;
            if start(arg, user, cwd, tty) {
                return LineEnd::Program;
            }
            sess.tty = None;
            channel_write_text(sess, chan, alloc::format!("{name}: could not start\n").as_bytes());
            ok = false;
            continue;
        }
        let before = cwd.clone();
        if run_cmd(sess, chan, &piece, user, cwd) {
            return LineEnd::Logout;
        }
        ok = unsafe { LAST_OK };
        if name == "cd" && *cwd != before {
            *prev = before;
        }
    }
    LineEnd::Done(ok)
}

/// Join a path argument against the session CWD into a lingfs path (no leading
/// slash; "" is root). An absolute `/x` argument resets to root-relative.
fn join_path(cwd: &str, arg: &str) -> String {
    if let Some(abs) = arg.strip_prefix('/') {
        return abs.to_string();
    }
    if arg.is_empty() {
        return cwd.to_string();
    }
    if cwd.is_empty() {
        arg.to_string()
    } else {
        let mut s = String::from(cwd);
        s.push('/');
        s.push_str(arg);
        s
    }
}

/// Run one shell command over the channel against the real lingfs. A genuine
/// (Rust-side) shell sharing the kernel's filesystem and user DB -- not the
/// `.ling` lsh binary (whose VGA/keyboard console I/O would need redirecting
/// onto the channel first), but real navigation and inspection. Returns true if
/// the session should end (`exit`).
fn run_cmd(sess: &mut Session, chan: u32, cmd: &str, username: &str, cwd: &mut String) -> bool {
    if cmd.is_empty() {
        return false;
    }
    let (name, arg) = match cmd.find(' ') {
        Some(i) => (&cmd[..i], cmd[i + 1..].trim()),
        None => (cmd, ""),
    };
    unsafe { LAST_OK = true };
    // files, machine info, desktop control, motd (sshcmds.rs); the rest below and the Pi console
    if let Some(out) = crate::drivers::sshcmds::run(name, arg, username, cwd) {
        unsafe { LAST_OK = !crate::drivers::sshline::looks_failed(name, &out) };
        channel_write_text(sess, chan, out.as_bytes());
        return false;
    }
    match name {
        "exit" | "logout" | "quit" => {
            channel_write(sess, chan, b"bye\r\n");
            return true;
        },
        "help" => {
            channel_write(sess, chan, b"commands:\r\n");
            channel_write(sess, chan, b"  ls [dir]   cat <file>   cd <dir>   pwd\r\n");
            channel_write(sess, chan, b"  echo <t>   whoami   hostname   uname   uptime   free\r\n");
            channel_write(sess, chan, b"  clear   net <host>   help   exit\r\n");
        },
        "ls" => {
            let path = join_path(cwd, arg);
            let mut i = 0usize;
            let mut any = false;
            loop {
                let mut nb = [0u8; 64];
                let Some((n, is_dir)) = lingfs::list_entry(&path, i, &mut nb) else { break };
                i += 1;
                any = true;
                let mut lineb = Vec::with_capacity(n + 3);
                lineb.extend_from_slice(&nb[..n]);
                if is_dir {
                    lineb.push(b'/');
                }
                lineb.extend_from_slice(b"\r\n");
                channel_write(sess, chan, &lineb);
                if i > 512 {
                    break;
                }
            }
            if !any {
                channel_write(sess, chan, b"(empty)\r\n");
            }
        },
        "cat" => {
            if arg.is_empty() {
                channel_write(sess, chan, b"usage: cat <file>\r\n");
            } else {
                let path = join_path(cwd, arg);
                let mut buf = alloc::vec![0u8; 256 * 1024];
                match lingfs::read_file_all(&path, &mut buf) {
                    Ok(Some(n)) => channel_write_text(sess, chan, &buf[..n]),
                    _ => {
                        let mut s = String::from("cat: ");
                        s.push_str(arg);
                        s.push_str(": no such file\r\n");
                        channel_write(sess, chan, s.as_bytes());
                    },
                }
            }
        },
        "cd" => {
            let new = if arg.is_empty() || arg == "/" || arg == "~" {
                String::new()
            } else if arg == ".." {
                match cwd.rfind('/') {
                    Some(i) => cwd[..i].to_string(),
                    None => String::new(),
                }
            } else {
                join_path(cwd, arg)
            };
            if new.is_empty() {
                *cwd = new;
            } else {
                let mut nb = [0u8; 64];
                if lingfs::list_entry(&new, 0, &mut nb).is_some() {
                    *cwd = new;
                } else {
                    let mut s = String::from("cd: ");
                    s.push_str(arg);
                    s.push_str(": no such directory\r\n");
                    channel_write(sess, chan, s.as_bytes());
                }
            }
        },
        "pwd" => {
            let mut s = String::from("/");
            s.push_str(cwd);
            s.push_str("\r\n");
            channel_write(sess, chan, s.as_bytes());
        },
        "echo" => {
            channel_write(sess, chan, arg.as_bytes());
            channel_write(sess, chan, b"\r\n");
        },
        "whoami" => {
            channel_write(sess, chan, username.as_bytes());
            channel_write(sess, chan, b"\r\n");
        },
        "hostname" => {
            let mut b = [0u8; lingfs::BLOCK_SIZE];
            match lingfs::read_file("/hostname", &mut b) {
                Ok(Some(n)) => {
                    channel_write(sess, chan, &b[..n]);
                    channel_write(sess, chan, b"\r\n");
                },
                _ => channel_write(sess, chan, b"lingos\r\n"),
            }
        },
        "uname" => {
            #[cfg(target_arch = "x86_64")]
            channel_write(sess, chan, b"LingOS x86_64 (a from-scratch OS written in Ling)\r\n");
            #[cfg(target_arch = "aarch64")]
            channel_write(sess, chan, b"LingOS aarch64 Raspberry Pi (a from-scratch OS written in Ling)\r\n");
        },
        "uptime" => {
            let secs = crate::arch::timer::now_ms() / 1000;
            let mut s = String::from("up ");
            push_u64(&mut s, secs);
            s.push_str("s\r\n");
            channel_write(sess, chan, s.as_bytes());
        },
        "free" => {
            let kib = crate::mm::frame::free_frame_count() * 4;
            let mut s = String::from("free: ");
            push_u64(&mut s, kib as u64);
            s.push_str(" KiB\r\n");
            channel_write(sess, chan, s.as_bytes());
        },
        "clear" => {
            channel_write(sess, chan, b"\x1b[2J\x1b[H");
        },
        "get" => {
            // the same fetch Horizon uses (DNS, TCP, TLS, HTTP): status, size, a peek
            if arg.is_empty() {
                channel_write(sess, chan, b"usage: get <http(s)://url>\r\n");
            } else {
                let t0 = crate::arch::timer::now_ms();
                let (status, body) = crate::netapi::http_get(arg);
                let ms = crate::arch::timer::now_ms() - t0;
                let mut s = alloc::format!("HTTP {status}, {} bytes in {ms} ms\r\n", body.len());
                for h in ["content-type", "location", "transfer-encoding", "server"] {
                    let v = crate::netapi::last_header(h);
                    if !v.is_empty() {
                        s.push_str(&alloc::format!("{h}: {v}\r\n"));
                    }
                }
                let peek: String = body.chars().take(160).filter(|c| !c.is_control() || *c == '\n').collect();
                s.push_str(&peek.replace('\n', "\r\n"));
                s.push_str("\r\n");
                channel_write(sess, chan, s.as_bytes());
            }
        },
        #[cfg(target_arch = "x86_64")]
        "nic" => {
            crate::drivers::netdev::ensure_init();
            let mut s = crate::drivers::netdev::status_line();
            s.push_str("\r\n");
            channel_write(sess, chan, s.as_bytes());
        },
        "net" => {
            // Concurrency probe: open a CLIENT TCP connection (a separate
            // netstack socket) while this SSH server session is live. With the
            // old single-connection stack this would have torn the session
            // down; with multi-socket it coexists, and the shell keeps working.
            if arg.is_empty() {
                channel_write(sess, chan, b"usage: net <host>  (opens a client TCP conn to <host>:443)\r\n");
            } else {
                match netstack::dns_resolve(arg) {
                    Some(ip) => {
                        let ok = netstack::tcp_connect(ip, 443);
                        netstack::tcp_close();
                        if ok {
                            channel_write(sess, chan, b"net: client socket connected to :443 -- this SSH session is still alive\r\n");
                        } else {
                            channel_write(sess, chan, b"net: client connect failed, but this SSH session survived it\r\n");
                        }
                    },
                    None => channel_write(sess, chan, b"net: could not resolve host\r\n"),
                }
            }
        },
        _ => {
            // On the Pi every console command works over SSH too (date,
            // board, wifi, net, ps, run, lingfu, sd, ...), its output captured.
            #[cfg(target_arch = "aarch64")]
            {
                let full_screen = cmd == "setup" || cmd.starts_with("ling run") || cmd.starts_with("ling demo") || cmd == "sd format" || cmd == "trace";
                if !full_screen {
                    crate::console_capture_begin();
                    crate::rpi_shell_exec(cmd.as_bytes());
                    let out = crate::console_capture_end().to_vec();
                    channel_write_text(sess, chan, &out);
                    return false;
                }
                channel_write(sess, chan, b"that command takes over the Pi's screen -- run it on the Pi\r\n");
                return false;
            }
            #[allow(unreachable_code)]
            {
                unsafe { LAST_OK = false };
                let mut s = String::from(name);
                s.push_str(": command not found (try 'help')\r\n");
                channel_write(sess, chan, s.as_bytes());
            }
        },
    }
    false
}

// ── Boot-time background listener ────────────────────────────────────────────

static mut SSHD_STARTED: bool = false;
static mut SSHD_ENABLED: bool = true;

/// Turn the server on or off (Settings); off closes the listener on port 22.
pub fn set_enabled(on: bool) {
    unsafe { SSHD_ENABLED = on };
    if on {
        start();
    } else {
        netstack::tcp_unlisten(port());
    }
}

pub fn enabled() -> bool {
    unsafe { SSHD_STARTED && SSHD_ENABLED }
}

/// Cooperative background task (same model as `pkgman::icon_task`): loop
/// accepting SSH connections. It does NOT hold the net lock across the session
/// -- the netstack now takes the lock only briefly per read/write/pump (see the
/// multi-socket TCP section), so an SSH session interleaves with other network
/// users (a browser fetch, Messenger) on their own sockets instead of blocking
/// them. Each iteration listens with a short accept window, runs the session if
/// a client arrives (yielding during reads so the desktop keeps rendering),
/// then yields.
extern "C" fn sshd_task() {
    loop {
        // 400ms accept window per idle iteration; a real SYN completes the
        // handshake + session inside this call. Errors (incl. "no client") are
        // silent here -- only a real connection logs (see `serve`).
        // (switched off: just pass the CPU on -- a 2 ms busy wait here cost
        // the desktop 2 ms of every frame)
        if unsafe { SSHD_ENABLED } {
            let _ = serve(400_000);
        }
        crate::proc::sched::yield_now();
    }
}

/// Start the SSH server as a background task, once. Call at boot when SSH is
/// enabled (`services::ssh_enabled`), after the NIC is up and the scheduler's
/// main task exists.
pub fn start() {
    // interactive shell programs (sshline::register): `claude`
    crate::claude::register();
    unsafe {
        SSHD_ENABLED = true;
        if SSHD_STARTED {
            return;
        }
    }
    // The scheduler has a handful of fixed task slots. Claim one *before*
    // calling ourselves started: a full table used to leave `enabled()` true
    // (Settings said "running", the boot log said "listening") with no task
    // behind it, so port 22 stayed shut.
    let slot = crate::proc::sched::spawn(sshd_task as *const () as usize as u64);
    crate::proc::sched::set_name(slot, "(SSH server)");
    if slot == u64::MAX {
        crate::console_write(b"sshd: no free task slot -- SSH server NOT started\n");
        return;
    }
    unsafe { SSHD_STARTED = true };
    crate::console_write(alloc::format!("sshd: SSH server enabled -- listening on port {}\n", port()).as_bytes());
}

fn push_u64(s: &mut String, mut v: u64) {
    if v == 0 {
        s.push('0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 20;
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    s.push_str(core::str::from_utf8(&buf[i..]).unwrap_or(""));
}
