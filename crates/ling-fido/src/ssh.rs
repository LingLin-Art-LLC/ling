//! OpenSSH security-key ("sk") public keys and signatures, as a FIDO key on
//! the *connecting* computer makes them (`ssh-keygen -t ed25519-sk` /
//! `-t ecdsa-sk`). LingOS's SSH server checks them here.
//!
//! Formats from OpenSSH's PROTOCOL.u2f:
//!
//! * public key blob
//!   - `sk-ssh-ed25519@openssh.com`: string alg, string pk(32), string application
//!   - `sk-ecdsa-sha2-nistp256@openssh.com`: string alg, string "nistp256",
//!     string Q (SEC1 point), string application
//! * signature blob: string alg, string sig, byte flags, uint32 counter;
//!   for ECDSA `sig` is itself string(mpint r, mpint s).
//! * what the key signs: SHA256(application) || flags || counter (BE) ||
//!   SHA256(data), where `data` is the RFC 4252 §7 publickey signed data
//!   ([`signed_data`]). Ed25519 signs those bytes directly; ECDSA signs their
//!   SHA-256 (the usual ECDSA-with-SHA256).
//!
//! Flag bits: 0x01 the user touched the key (user presence), 0x04 the key
//! checked a fingerprint or PIN (user verification).

use alloc::string::String;
use alloc::vec::Vec;
use p256::ecdsa::signature::Verifier;
use sha2::{Digest, Sha256};

pub const ALG_ED25519_SK: &str = "sk-ssh-ed25519@openssh.com";
pub const ALG_ECDSA_SK: &str = "sk-ecdsa-sha2-nistp256@openssh.com";

pub const FLAG_UP: u8 = 0x01;
pub const FLAG_UV: u8 = 0x04;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Ed25519Sk,
    EcdsaSk,
}

impl Kind {
    pub fn from_alg(alg: &[u8]) -> Option<Kind> {
        match alg {
            a if a == ALG_ED25519_SK.as_bytes() => Some(Kind::Ed25519Sk),
            a if a == ALG_ECDSA_SK.as_bytes() => Some(Kind::EcdsaSk),
            _ => None,
        }
    }
    pub fn alg(self) -> &'static str {
        match self {
            Kind::Ed25519Sk => ALG_ED25519_SK,
            Kind::EcdsaSk => ALG_ECDSA_SK,
        }
    }
    /// The short name `ssh-keygen -t` takes.
    pub fn short(self) -> &'static str {
        match self {
            Kind::Ed25519Sk => "ed25519-sk",
            Kind::EcdsaSk => "ecdsa-sk",
        }
    }
}

/// A parsed security-key public key (borrowing the blob).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicKey<'a> {
    pub kind: Kind,
    /// Ed25519: the 32-byte key. ECDSA: the SEC1 point.
    pub key: &'a [u8],
    /// The FIDO application ("ssh:" unless the user picked another).
    pub application: &'a [u8],
}

/// A parsed signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub kind: Kind,
    /// Ed25519: R || S. ECDSA: r || s, each left-padded to 32 bytes.
    pub raw: [u8; 64],
    pub flags: u8,
    pub counter: u32,
}

impl Signature {
    pub fn user_present(&self) -> bool {
        self.flags & FLAG_UP != 0
    }
    pub fn user_verified(&self) -> bool {
        self.flags & FLAG_UV != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// The public key blob is not a security key we understand.
    BadKey,
    /// The signature blob does not parse, or is for another algorithm.
    BadSignature,
    /// The key did not see a touch (user-presence flag clear).
    NoTouch,
    /// The signature does not check out.
    Mismatch,
}

impl VerifyError {
    pub fn words(self) -> &'static str {
        match self {
            VerifyError::BadKey => "not a security-key public key",
            VerifyError::BadSignature => "malformed signature",
            VerifyError::NoTouch => "the key was not touched (user-presence flag missing)",
            VerifyError::Mismatch => "the signature does not match",
        }
    }
}

// ── wire helpers ─────────────────────────────────────────────────────────────

fn get_string<'a>(b: &'a [u8], p: &mut usize) -> Option<&'a [u8]> {
    let l = u32::from_be_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
    *p += 4;
    let s = b.get(*p..p.checked_add(l)?)?;
    *p += l;
    Some(s)
}

fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

/// An SSH `mpint` holding a P-256 scalar as 32 big-endian bytes.
fn mpint32(m: &[u8]) -> Option<[u8; 32]> {
    // a negative number (top bit set with no 0x00 in front) is not a scalar
    if m.first().is_some_and(|b| b & 0x80 != 0) {
        return None;
    }
    let mut i = 0;
    while i < m.len() && m[i] == 0 {
        i += 1;
    }
    let v = &m[i..];
    if v.is_empty() || v.len() > 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out[32 - v.len()..].copy_from_slice(v);
    Some(out)
}

// ── parsing ──────────────────────────────────────────────────────────────────

/// Parse a security-key public key blob. Rejects trailing bytes, a wrong
/// curve, an off-curve ECDSA point, a malformed Ed25519 key.
pub fn parse_public_key(blob: &[u8]) -> Option<PublicKey<'_>> {
    let mut p = 0;
    let kind = Kind::from_alg(get_string(blob, &mut p)?)?;
    let key = match kind {
        Kind::Ed25519Sk => {
            let k = get_string(blob, &mut p)?;
            let arr: [u8; 32] = k.try_into().ok()?;
            ed25519_dalek::VerifyingKey::from_bytes(&arr).ok()?;
            k
        },
        Kind::EcdsaSk => {
            if get_string(blob, &mut p)? != b"nistp256" {
                return None;
            }
            let q = get_string(blob, &mut p)?;
            p256::ecdsa::VerifyingKey::from_sec1_bytes(q).ok()?;
            q
        },
    };
    let application = get_string(blob, &mut p)?;
    if p != blob.len() || application.is_empty() || application.len() > 255 {
        return None;
    }
    Some(PublicKey { kind, key, application })
}

/// Parse a signature blob for `kind`.
pub fn parse_signature(kind: Kind, sig: &[u8]) -> Option<Signature> {
    let mut p = 0;
    if Kind::from_alg(get_string(sig, &mut p)?)? != kind {
        return None;
    }
    let inner = get_string(sig, &mut p)?;
    let mut raw = [0u8; 64];
    match kind {
        Kind::Ed25519Sk => raw.copy_from_slice(<&[u8; 64]>::try_from(inner).ok()?),
        Kind::EcdsaSk => {
            let mut q = 0;
            let r = mpint32(get_string(inner, &mut q)?)?;
            let s = mpint32(get_string(inner, &mut q)?)?;
            if q != inner.len() {
                return None;
            }
            raw[..32].copy_from_slice(&r);
            raw[32..].copy_from_slice(&s);
        },
    }
    let flags = *sig.get(p)?;
    let counter = u32::from_be_bytes(sig.get(p + 1..p + 5)?.try_into().ok()?);
    if p + 5 != sig.len() {
        return None;
    }
    Some(Signature { kind, raw, flags, counter })
}

/// RFC 4252 §7: the data a publickey signature covers.
pub fn signed_data(session_id: &[u8], user: &[u8], alg: &[u8], blob: &[u8]) -> Vec<u8> {
    let mut d = Vec::with_capacity(128 + blob.len());
    put_string(&mut d, session_id);
    d.push(50); // SSH_MSG_USERAUTH_REQUEST
    put_string(&mut d, user);
    put_string(&mut d, b"ssh-connection");
    put_string(&mut d, b"publickey");
    d.push(1); // TRUE: a signature follows
    put_string(&mut d, alg);
    put_string(&mut d, blob);
    d
}

/// What the FIDO key itself signed: SHA256(application) || flags || counter || SHA256(data).
pub fn sk_message(application: &[u8], flags: u8, counter: u32, data: &[u8]) -> [u8; 69] {
    let mut m = [0u8; 69];
    m[..32].copy_from_slice(&Sha256::digest(application));
    m[32] = flags;
    m[33..37].copy_from_slice(&counter.to_be_bytes());
    m[37..].copy_from_slice(&Sha256::digest(data));
    m
}

/// Check `sig` (a signature blob) over `data` against the public key `blob`.
/// Requires the user-presence flag. On success returns the parsed signature,
/// whose `counter` the caller compares with the last one it saw and whose
/// `user_verified()` says whether the key checked a fingerprint / PIN.
pub fn verify(blob: &[u8], data: &[u8], sig: &[u8]) -> Result<Signature, VerifyError> {
    let pk = parse_public_key(blob).ok_or(VerifyError::BadKey)?;
    let s = parse_signature(pk.kind, sig).ok_or(VerifyError::BadSignature)?;
    let msg = sk_message(pk.application, s.flags, s.counter, data);
    let good = match pk.kind {
        Kind::Ed25519Sk => {
            let Ok(arr) = <[u8; 32]>::try_from(pk.key) else { return Err(VerifyError::BadKey) };
            let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&arr) else { return Err(VerifyError::BadKey) };
            let es = ed25519_dalek::Signature::from_bytes(&s.raw);
            vk.verify_strict(&msg, &es).is_ok()
        },
        Kind::EcdsaSk => {
            let Ok(vk) = p256::ecdsa::VerifyingKey::from_sec1_bytes(pk.key) else { return Err(VerifyError::BadKey) };
            let Ok(es) = p256::ecdsa::Signature::from_slice(&s.raw) else { return Err(VerifyError::BadSignature) };
            // ECDSA-with-SHA256 over the 69 bytes, as OpenSSH's ssh-ecdsa-sk.c checks it
            vk.verify(&msg, &es).is_ok()
        },
    };
    if !good {
        return Err(VerifyError::Mismatch);
    }
    // checked after the signature so a forged "no touch" can't be told apart cheaply
    if !s.user_present() {
        return Err(VerifyError::NoTouch);
    }
    Ok(s)
}

/// `SHA256:<base64, no padding>` -- the fingerprint `ssh-keygen -lf` prints.
pub fn fingerprint(blob: &[u8]) -> String {
    let mut s = String::from("SHA256:");
    s.push_str(&base64_nopad(&Sha256::digest(blob)));
    s
}

/// The public key as an `authorized_keys`-style line body (base64 of the blob).
pub fn base64_blob(blob: &[u8]) -> String {
    base64_std(blob)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_std(d: &[u8]) -> String {
    let mut s = base64_nopad(d);
    while s.len() % 4 != 0 {
        s.push('=');
    }
    s
}

fn base64_nopad(d: &[u8]) -> String {
    let mut s = String::with_capacity(d.len() * 4 / 3 + 4);
    for c in d.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        let chars = c.len() + 1;
        for i in 0..chars {
            s.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn ed_blob(pk: &[u8; 32], app: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        put_string(&mut b, ALG_ED25519_SK.as_bytes());
        put_string(&mut b, pk);
        put_string(&mut b, app);
        b
    }

    fn ec_blob(q: &[u8], app: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        put_string(&mut b, ALG_ECDSA_SK.as_bytes());
        put_string(&mut b, b"nistp256");
        put_string(&mut b, q);
        put_string(&mut b, app);
        b
    }

    fn mpint(v: &[u8]) -> Vec<u8> {
        let mut i = 0;
        while i < v.len() && v[i] == 0 {
            i += 1;
        }
        let mut m = Vec::new();
        if v[i] & 0x80 != 0 {
            m.push(0);
        }
        m.extend_from_slice(&v[i..]);
        let mut out = Vec::new();
        put_string(&mut out, &m);
        out
    }

    /// A software ed25519-sk key: signs like a FIDO key would.
    fn ed_sign(sk: &ed25519_dalek::SigningKey, app: &[u8], flags: u8, counter: u32, data: &[u8]) -> Vec<u8> {
        use ed25519_dalek::Signer;
        let m = sk_message(app, flags, counter, data);
        let sig = sk.sign(&m).to_bytes();
        let mut b = Vec::new();
        put_string(&mut b, ALG_ED25519_SK.as_bytes());
        put_string(&mut b, &sig);
        b.push(flags);
        b.extend_from_slice(&counter.to_be_bytes());
        b
    }

    fn ec_sign(sk: &p256::ecdsa::SigningKey, app: &[u8], flags: u8, counter: u32, data: &[u8]) -> Vec<u8> {
        use p256::ecdsa::signature::Signer;
        let m = sk_message(app, flags, counter, data);
        let sig: p256::ecdsa::Signature = sk.sign(&m);
        let (r, s) = sig.split_bytes();
        let mut inner = mpint(&r);
        inner.extend_from_slice(&mpint(&s));
        let mut b = Vec::new();
        put_string(&mut b, ALG_ECDSA_SK.as_bytes());
        put_string(&mut b, &inner);
        b.push(flags);
        b.extend_from_slice(&counter.to_be_bytes());
        b
    }

    #[test]
    fn ed25519_sk_round_trip() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let blob = ed_blob(&sk.verifying_key().to_bytes(), b"ssh:");
        let data = signed_data(&[1u8; 32], b"ling", ALG_ED25519_SK.as_bytes(), &blob);
        let sig = ed_sign(&sk, b"ssh:", FLAG_UP | FLAG_UV, 42, &data);
        let s = verify(&blob, &data, &sig).unwrap();
        assert_eq!(s.counter, 42);
        assert!(s.user_verified());
        // another session's data does not verify
        let other = signed_data(&[2u8; 32], b"ling", ALG_ED25519_SK.as_bytes(), &blob);
        assert_eq!(verify(&blob, &other, &sig), Err(VerifyError::Mismatch));
        // no touch
        let sig0 = ed_sign(&sk, b"ssh:", 0, 43, &data);
        assert_eq!(verify(&blob, &data, &sig0), Err(VerifyError::NoTouch));
        // flipping the flags breaks the signature
        let mut bad = sig.clone();
        let n = bad.len();
        bad[n - 5] ^= FLAG_UV;
        assert_eq!(verify(&blob, &data, &bad), Err(VerifyError::Mismatch));
    }

    #[test]
    fn ecdsa_sk_round_trip() {
        let sk = p256::ecdsa::SigningKey::from_slice(&[9u8; 32]).unwrap();
        let q = sk.verifying_key().to_encoded_point(false);
        let blob = ec_blob(q.as_bytes(), b"ssh:");
        let data = signed_data(&[3u8; 32], b"root", ALG_ECDSA_SK.as_bytes(), &blob);
        let sig = ec_sign(&sk, b"ssh:", FLAG_UP, 7, &data);
        let s = verify(&blob, &data, &sig).unwrap();
        assert_eq!((s.counter, s.user_verified()), (7, false));
        // the wrong application (the key signs SHA256(app))
        let blob2 = ec_blob(q.as_bytes(), b"ssh:other");
        assert_eq!(verify(&blob2, &data, &sig), Err(VerifyError::Mismatch));
        // an ed25519-sk signature blob is not an ecdsa one
        let esk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        assert_eq!(verify(&blob, &data, &ed_sign(&esk, b"ssh:", 1, 1, &data)), Err(VerifyError::BadSignature));
    }

    #[test]
    fn rejects_malformed_keys() {
        assert!(parse_public_key(b"").is_none());
        let sk = p256::ecdsa::SigningKey::from_slice(&[9u8; 32]).unwrap();
        let q = sk.verifying_key().to_encoded_point(false);
        let mut b = ec_blob(q.as_bytes(), b"ssh:");
        b.push(0); // trailing junk
        assert!(parse_public_key(&b).is_none());
        let mut c = Vec::new();
        put_string(&mut c, ALG_ECDSA_SK.as_bytes());
        put_string(&mut c, b"nistp384");
        put_string(&mut c, q.as_bytes());
        put_string(&mut c, b"ssh:");
        assert!(parse_public_key(&c).is_none());
        let mut off = q.as_bytes().to_vec();
        off[40] ^= 1; // off the curve
        assert!(parse_public_key(&ec_blob(&off, b"ssh:")).is_none());
        // plain ssh-ed25519 is not a security key
        let mut d = Vec::new();
        put_string(&mut d, b"ssh-ed25519");
        put_string(&mut d, &[1u8; 32]);
        assert!(parse_public_key(&d).is_none());
    }

    #[test]
    fn fingerprint_format() {
        // `ssh-keygen -lf` style: SHA256 + base64 without padding (43 chars)
        let f = fingerprint(b"abc");
        assert_eq!(f, "SHA256:ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0");
        assert_eq!(base64_blob(b"ab"), "YWI=");
        assert_eq!(base64_blob(&vec![0u8; 3]), "AAAA");
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// Vectors from a Python software sk key (LingOS scratchpad sk_vectors.py) whose SSHSIG
    /// signatures the real `ssh-keygen -Y verify` (OpenSSH 9.0 and 10.3) accepts, and
    /// fingerprints as `ssh-keygen -lf` printed them.
    #[test]
    fn openssh_cross_checked_vectors() {
        let mut n = 0;
        for line in include_str!("ssh_vectors.txt").lines().filter(|l| !l.is_empty()) {
            let f: Vec<&str> = line.split(' ').collect();
            if f[0] == "fp" {
                assert_eq!(fingerprint(&unhex(f[1])), f[2]);
                n += 1;
                continue;
            }
            let (sid, user, blob, sig) = (unhex(f[1]), f[2].as_bytes(), unhex(f[3]), unhex(f[4]));
            let alg = Kind::from_alg(&blob[4..4 + blob[3] as usize]).unwrap().alg();
            let data = signed_data(&sid, user, alg.as_bytes(), &blob);
            let got = verify(&blob, &data, &sig);
            match f[0] {
                "ok" => {
                    let s = got.unwrap();
                    assert_eq!(s.counter, f[5].parse::<u32>().unwrap());
                    assert_eq!(s.user_verified(), f[6] == "1");
                },
                "notouch" => assert_eq!(got, Err(VerifyError::NoTouch)),
                "mismatch" => assert_eq!(got, Err(VerifyError::Mismatch)),
                w => panic!("unknown vector kind {w}"),
            }
            n += 1;
        }
        assert_eq!(n, 11);
    }

    #[test]
    fn mpint_bounds() {
        assert_eq!(mpint32(&[0x00, 0x80, 1]).unwrap()[30..], [0x80, 1]);
        assert!(mpint32(&[0x80]).is_none()); // negative
        assert!(mpint32(&[1u8; 33]).is_none()); // too big
        assert!(mpint32(&[]).is_none()); // zero
    }
}
