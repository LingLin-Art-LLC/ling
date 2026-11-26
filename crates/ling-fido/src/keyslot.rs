//! Keyslots for an app secret: one random 32-byte secret S per app, wrapped
//! once per security key, LUKS style (any one of N opens it).
//!
//! * A **key-bound** slot ([`Kind::Hmac`]) keeps S under ChaCha20-Poly1305
//!   with a key derived (HKDF-SHA-256) from the security key's hmac-secret
//!   output for this slot's credential and the record's salt. Only that
//!   physical key can recompute it.
//! * A **touch-gated** slot ([`Kind::U2f`]) keeps nothing secret: U2F
//!   signatures are randomized, so no key can be derived from them. The
//!   record's own copy of S ([`Record::s`]) is released by the kernel after a
//!   fresh U2F assertion verifies.
//! * [`Record::s`] is also what the account-password recovery releases. A
//!   *strict* record has no copy at all: only key-bound slots open it.
//!
//! [`Record::check`] tells the right S from a wrong one (a value derived from
//! S, compared in constant time), so a recovered or app-supplied S can be
//! verified without storing S. This module is pure (no I/O): the LingOS
//! kernel (`fs::appkeys`) seals the record text with the account keyring and
//! does the talking to the key.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::ChaCha20Poly1305;
use sha2::Sha256;

pub const HEADER: &str = "lingos-appkeys v1";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Hmac,
    U2f,
    /// A simulated key (test kernels only).
    Test,
}

impl Kind {
    pub fn tag(self) -> &'static str {
        match self {
            Kind::Hmac => "hmac",
            Kind::U2f => "u2f",
            Kind::Test => "test",
        }
    }
    pub fn from_tag(t: &str) -> Option<Kind> {
        match t {
            "hmac" => Some(Kind::Hmac),
            "u2f" => Some(Kind::U2f),
            "test" => Some(Kind::Test),
            _ => None,
        }
    }
    /// For the screen.
    pub fn mode(self) -> &'static str {
        match self {
            Kind::Hmac => "key-bound (FIDO2 hmac-secret)",
            Kind::U2f => "touch-gated (U2F)",
            Kind::Test => "SIMULATED (test kernel)",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub kind: Kind,
    pub name: String,
    pub counter: u32,
    /// The key verified the user (fingerprint / PIN) when the slot was made.
    pub uv: bool,
    /// CTAP2 credential id or U2F key handle.
    pub cred: Vec<u8>,
    /// SEC1 public key of the credential.
    pub public_key: Vec<u8>,
    pub nonce: [u8; 12],
    pub tag: [u8; 16],
    /// S under the key's hmac-secret (key-bound slots only).
    pub wrapped: Vec<u8>,
}

pub struct Record {
    /// HKDF(S, "check:<app>").
    pub chk: [u8; 32],
    /// S itself (recovery and touch-gated slots); None when strict.
    pub s: Option<[u8; 32]>,
    pub strict: bool,
    /// One app-defined setting (the wallet: ask for a touch above this amount).
    pub over: String,
    /// The U2F application parameter for this app's touch-gated slots.
    pub u2f_app: [u8; 32],
    /// The hmac-secret salt every key-bound slot of this app uses.
    pub salt: [u8; 32],
    pub slots: Vec<Slot>,
}

impl Drop for Record {
    fn drop(&mut self) {
        if let Some(s) = self.s.as_mut() {
            wipe(s);
        }
    }
}

pub fn wipe(b: &mut [u8]) {
    for x in b.iter_mut() {
        unsafe { core::ptr::write_volatile(x, 0) };
    }
}

/// Equal, without an early exit on the first difference.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for i in 0..a.len() {
        d |= a[i] ^ b[i];
    }
    unsafe { core::ptr::read_volatile(&d) == 0 }
}

fn hkdf32(ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    hkdf::Hkdf::<Sha256>::new(Some(b"lingos-appkeys"), ikm).expand(info, &mut out).expect("32 bytes");
    out
}

pub fn check_value(app: &str, s: &[u8; 32]) -> [u8; 32] {
    hkdf32(s, format!("check:{app}").as_bytes())
}

fn wrap_aad(user: &str, app: &str, cred: &[u8]) -> Vec<u8> {
    let mut a = format!("lingos-appkeys:{user}:{app}:").into_bytes();
    a.extend_from_slice(cred);
    a
}

/// S wrapped under a key-bound slot's hmac-secret `output`: (ciphertext, tag).
pub fn wrap(user: &str, app: &str, cred: &[u8], output: &[u8; 32], s: &[u8; 32], nonce: &[u8; 12]) -> Option<(Vec<u8>, [u8; 16])> {
    let mut k = hkdf32(output, format!("wrap:{user}:{app}").as_bytes());
    let c = ChaCha20Poly1305::new((&k).into());
    wipe(&mut k);
    let mut buf = s.to_vec();
    let tag = c.encrypt_in_place_detached(nonce.into(), &wrap_aad(user, app, cred), &mut buf).ok()?;
    Some((buf, tag.into()))
}

/// S from a key-bound slot, given the key's hmac-secret `output`; None on a
/// wrong output (another key, another user / app, a different UV state) or
/// a tampered slot.
pub fn unwrap(user: &str, app: &str, slot: &Slot, output: &[u8; 32]) -> Option<[u8; 32]> {
    if slot.kind != Kind::Hmac || slot.wrapped.len() != 32 {
        return None;
    }
    let mut k = hkdf32(output, format!("wrap:{user}:{app}").as_bytes());
    let c = ChaCha20Poly1305::new((&k).into());
    wipe(&mut k);
    let mut buf = slot.wrapped.clone();
    if c.decrypt_in_place_detached((&slot.nonce).into(), &wrap_aad(user, app, &slot.cred), &mut buf, (&slot.tag).into()).is_err() {
        wipe(&mut buf);
        return None;
    }
    let mut s = [0u8; 32];
    s.copy_from_slice(&buf);
    wipe(&mut buf);
    Some(s)
}

impl Record {
    /// A fresh record for `app` around the secret `s` (recovery copy kept).
    pub fn new(app: &str, s: &[u8; 32], u2f_app: [u8; 32], salt: [u8; 32]) -> Record {
        Record { chk: check_value(app, s), s: Some(*s), strict: false, over: String::new(), u2f_app, salt, slots: Vec::new() }
    }

    /// Is `s` this record's secret? (constant time)
    pub fn check(&self, app: &str, s: &[u8; 32]) -> bool {
        ct_eq(&check_value(app, s), &self.chk)
    }

    /// The record of `app` from its text; None when damaged.
    pub fn parse(app: &str, plain: &[u8]) -> Option<Record> {
        let text = core::str::from_utf8(plain).ok()?;
        if !text.starts_with(HEADER) {
            return None;
        }
        let mut r = Record { chk: [0; 32], s: None, strict: false, over: String::new(), u2f_app: [0; 32], salt: [0; 32], slots: Vec::new() };
        let fixed = |v: &str| -> Option<[u8; 32]> { unhex(v.trim())?.try_into().ok() };
        let mut have_chk = false;
        for line in text.lines() {
            let Some((k, v)) = line.split_once(':') else { continue };
            match k {
                "chk" => {
                    r.chk = fixed(v)?;
                    have_chk = true;
                },
                "s" => r.s = Some(fixed(v)?),
                "strict" => r.strict = v.trim() == "1",
                "over" => r.over = String::from(v.trim()),
                "u2fapp" => r.u2f_app = fixed(v)?,
                "salt" => r.salt = fixed(v)?,
                "slot" => {
                    let f: Vec<&str> = v.splitn(9, ':').collect();
                    if f.len() != 9 {
                        continue;
                    }
                    let Some(kind) = Kind::from_tag(f[0]) else { continue };
                    let (Ok(counter), Some(cred), Some(public_key), Some(nonce), Some(tag), Some(wrapped)) =
                        (f[1].parse::<u32>(), unhex(f[3]), unhex(f[4]), unhex(f[5]), unhex(f[6]), unhex(f[7]))
                    else {
                        continue;
                    };
                    let (Ok(nonce), Ok(tag)) = (<[u8; 12]>::try_from(nonce.as_slice()), <[u8; 16]>::try_from(tag.as_slice())) else { continue };
                    r.slots.push(Slot { kind, name: String::from(f[8]), counter, uv: f[2] == "1", cred, public_key, nonce, tag, wrapped });
                },
                _ => {},
            }
        }
        if !have_chk {
            return None;
        }
        // a strict record never carries S, and a recovery copy must be the right one
        if r.strict {
            if let Some(s) = r.s.as_mut() {
                wipe(s);
            }
            r.s = None;
        } else if let Some(s) = r.s {
            if !r.check(app, &s) {
                return None;
            }
        }
        Some(r)
    }

    /// The record as text (secret when it holds S: seal it before storing).
    pub fn to_text(&self) -> Vec<u8> {
        let mut t = format!("{HEADER}\nchk:{}\nstrict:{}\nover:{}\nu2fapp:{}\nsalt:{}\n", hex(&self.chk), if self.strict { 1 } else { 0 }, self.over, hex(&self.u2f_app), hex(&self.salt));
        if let (Some(s), false) = (&self.s, self.strict) {
            t.push_str("s:");
            t.push_str(&hex(s));
            t.push('\n');
        }
        for s in &self.slots {
            t.push_str(&format!(
                "slot:{}:{}:{}:{}:{}:{}:{}:{}:{}\n",
                s.kind.tag(),
                s.counter,
                if s.uv { 1 } else { 0 },
                hex(&s.cred),
                hex(&s.public_key),
                hex(&s.nonce),
                hex(&s.tag),
                hex(&s.wrapped),
                clean_name(&s.name)
            ));
        }
        t.into_bytes()
    }
}

/// A key's name as stored: no ':' or line breaks, at most 32 characters.
pub fn clean_name(name: &str) -> String {
    let n: String = name.chars().filter(|c| *c != ':' && *c != '\n' && *c != '\r').take(32).collect();
    let n = String::from(n.trim());
    if n.is_empty() {
        String::from("Security key")
    } else {
        n
    }
}

pub fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
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
