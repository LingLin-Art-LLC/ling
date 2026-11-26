//! CTAP2 (FIDO2) over CTAPHID: enough of it to get a stable 32-byte secret
//! out of a security key with the `hmac-secret` extension.
//!
//! A key that speaks CTAP2 (CTAPHID capability bit 2, "CBOR") can keep a
//! random `CredRandom` per credential and answer `HMAC-SHA-256(CredRandom,
//! salt)` for a salt we send. The answer is the same every time for the same
//! credential and salt, never leaves the key in the clear (it travels
//! encrypted under an ECDH shared secret), and needs a touch -- so it can wrap
//! a data key the way a LUKS keyslot does. Nothing else about FIDO2 (resident
//! keys, PINs, attestation checks) is needed here.
//!
//! Steps, all over [`Device::transact`] with `CTAPHID_CBOR`:
//! * [`get_info`]: does the key list `hmac-secret`, and which PIN/UV auth
//!   protocols (1 or 2) does it know;
//! * [`make_credential`] (a touch): a new non-resident credential for an RP id
//!   with the `hmac-secret` extension turned on;
//! * [`key_agreement`]: the key's ephemeral P-256 public key (clientPIN
//!   getKeyAgreement) and our half of the ECDH, giving a [`Shared`] secret;
//! * [`hmac_secret`] (a touch): getAssertion with the extension input
//!   (our public key, the salt encrypted and authenticated under the shared
//!   secret); the key's output decrypts to the 32-byte secret.
//!
//! The crate has no random source: callers pass the ephemeral scalar, IVs
//! and client-data hashes in.

use crate::{Device, Error, Transport};
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes256;
use alloc::string::String;
use alloc::vec::Vec;
use hmac::{Hmac, Mac};
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::{EncodedPoint, NonZeroScalar, ProjectivePoint, PublicKey};
use sha2::{Digest, Sha256};

/// CTAPHID_CBOR (0x10 | 0x80).
pub const CMD_CBOR: u8 = 0x90;
/// CTAPHID_CANCEL (0x11 | 0x80): stop a request that waits for a touch.
pub const CMD_CANCEL: u8 = 0x91;

pub const MAKE_CREDENTIAL: u8 = 0x01;
pub const GET_ASSERTION: u8 = 0x02;
pub const GET_INFO: u8 = 0x04;
pub const CLIENT_PIN: u8 = 0x06;

/// COSE algorithm ids.
pub const ALG_ES256: i64 = -7;
pub const ALG_ECDH_ES_HKDF_256: i64 = -25;

/// CTAP2 status codes worth naming.
pub const ERR_INVALID_OPTION: u8 = 0x2C;
pub const ERR_UNSUPPORTED_OPTION: u8 = 0x2B;
pub const ERR_UNSUPPORTED_EXTENSION: u8 = 0x16;
pub const ERR_OPERATION_DENIED: u8 = 0x27;
pub const ERR_NO_CREDENTIALS: u8 = 0x2E;
pub const ERR_USER_ACTION_TIMEOUT: u8 = 0x2F;
pub const ERR_KEEPALIVE_CANCEL: u8 = 0x2D;
pub const ERR_PIN_INVALID: u8 = 0x31;
pub const ERR_PIN_BLOCKED: u8 = 0x32;
pub const ERR_PIN_AUTH_INVALID: u8 = 0x33;
pub const ERR_PIN_NOT_SET: u8 = 0x35;
pub const ERR_PUAT_REQUIRED: u8 = 0x36;
pub const ERR_UV_BLOCKED: u8 = 0x3C;
pub const ERR_UV_INVALID: u8 = 0x3F;

/// A status that means "this key wants a PIN / fingerprint check we do not
/// do" -- the caller falls back to plain U2F for such a key.
pub fn needs_pin_or_uv(code: u8) -> bool {
    matches!(code, ERR_PUAT_REQUIRED | ERR_PIN_INVALID | ERR_PIN_BLOCKED | ERR_PIN_AUTH_INVALID | ERR_PIN_NOT_SET | ERR_UV_BLOCKED | ERR_UV_INVALID | ERR_UNSUPPORTED_OPTION | ERR_INVALID_OPTION | ERR_UNSUPPORTED_EXTENSION)
}

// ── CBOR (the subset CTAP2 uses) ────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cbor {
    Uint(u64),
    /// A negative integer -1 - n.
    Neg(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    /// Entries in the order they are written: build maps in CTAP2 canonical
    /// order (shorter encoded key first, then bytewise).
    Map(Vec<(Cbor, Cbor)>),
    Bool(bool),
    Null,
}

impl Cbor {
    pub fn int(v: i64) -> Cbor {
        if v >= 0 {
            Cbor::Uint(v as u64)
        } else {
            Cbor::Neg((-1 - v) as u64)
        }
    }
    pub fn text(s: &str) -> Cbor {
        Cbor::Text(String::from(s))
    }
    pub fn bytes(b: &[u8]) -> Cbor {
        Cbor::Bytes(b.to_vec())
    }
    pub fn as_int(&self) -> Option<i64> {
        match *self {
            Cbor::Uint(v) if v <= i64::MAX as u64 => Some(v as i64),
            Cbor::Neg(v) if v <= i64::MAX as u64 => Some(-1 - v as i64),
            _ => None,
        }
    }
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Cbor::Bytes(b) => Some(b),
            _ => None,
        }
    }
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Cbor::Text(t) => Some(t),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&[Cbor]> {
        match self {
            Cbor::Array(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Cbor::Bool(b) => Some(*b),
            _ => None,
        }
    }
    /// A map's value under `key`.
    pub fn get(&self, key: &Cbor) -> Option<&Cbor> {
        match self {
            Cbor::Map(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn get_i(&self, key: i64) -> Option<&Cbor> {
        self.get(&Cbor::int(key))
    }
    pub fn get_t(&self, key: &str) -> Option<&Cbor> {
        match self {
            Cbor::Map(m) => m.iter().find(|(k, _)| k.as_text() == Some(key)).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Cbor::Uint(v) => head(out, 0, *v),
            Cbor::Neg(v) => head(out, 1, *v),
            Cbor::Bytes(b) => {
                head(out, 2, b.len() as u64);
                out.extend_from_slice(b);
            },
            Cbor::Text(t) => {
                head(out, 3, t.len() as u64);
                out.extend_from_slice(t.as_bytes());
            },
            Cbor::Array(a) => {
                head(out, 4, a.len() as u64);
                for v in a {
                    v.encode(out);
                }
            },
            Cbor::Map(m) => {
                head(out, 5, m.len() as u64);
                for (k, v) in m {
                    k.encode(out);
                    v.encode(out);
                }
            },
            Cbor::Bool(false) => out.push(0xF4),
            Cbor::Bool(true) => out.push(0xF5),
            Cbor::Null => out.push(0xF6),
        }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.encode(&mut v);
        v
    }
}

fn head(out: &mut Vec<u8>, major: u8, v: u64) {
    let m = major << 5;
    if v < 24 {
        out.push(m | v as u8);
    } else if v < 0x100 {
        out.push(m | 24);
        out.push(v as u8);
    } else if v < 0x1_0000 {
        out.push(m | 25);
        out.extend_from_slice(&(v as u16).to_be_bytes());
    } else if v < 0x1_0000_0000 {
        out.push(m | 26);
        out.extend_from_slice(&(v as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&v.to_be_bytes());
    }
}

/// Decode one CBOR item from the front of `b`: the item and the bytes it used.
pub fn decode(b: &[u8]) -> Result<(Cbor, usize), Error> {
    let mut pos = 0;
    let v = decode_at(b, &mut pos, 0)?;
    Ok((v, pos))
}

fn decode_at(b: &[u8], pos: &mut usize, depth: u32) -> Result<Cbor, Error> {
    if depth > 16 {
        return Err(Error::Protocol);
    }
    let ib = *b.get(*pos).ok_or(Error::Protocol)?;
    *pos += 1;
    let major = ib >> 5;
    let ai = ib & 31;
    if major == 7 {
        return match ai {
            20 => Ok(Cbor::Bool(false)),
            21 => Ok(Cbor::Bool(true)),
            22 | 23 => Ok(Cbor::Null),
            _ => Err(Error::Protocol), // floats and simple values: not in CTAP2 answers we read
        };
    }
    let mut take = |n: usize| -> Result<u64, Error> {
        let s = b.get(*pos..*pos + n).ok_or(Error::Protocol)?;
        *pos += n;
        Ok(s.iter().fold(0u64, |a, &x| (a << 8) | x as u64))
    };
    let v = match ai {
        0..=23 => ai as u64,
        24 => take(1)?,
        25 => take(2)?,
        26 => take(4)?,
        27 => take(8)?,
        _ => return Err(Error::Protocol), // indefinite lengths are not canonical CTAP2
    };
    let left = b.len() - *pos;
    match major {
        0 => Ok(Cbor::Uint(v)),
        1 => Ok(Cbor::Neg(v)),
        2 | 3 => {
            if v > left as u64 {
                return Err(Error::Protocol);
            }
            let s = &b[*pos..*pos + v as usize];
            *pos += v as usize;
            if major == 2 {
                Ok(Cbor::Bytes(s.to_vec()))
            } else {
                core::str::from_utf8(s).map(|t| Cbor::Text(String::from(t))).map_err(|_| Error::Protocol)
            }
        },
        4 => {
            if v > left as u64 {
                return Err(Error::Protocol);
            }
            let mut a = Vec::with_capacity(v as usize);
            for _ in 0..v {
                a.push(decode_at(b, pos, depth + 1)?);
            }
            Ok(Cbor::Array(a))
        },
        5 => {
            if v > left as u64 / 2 {
                return Err(Error::Protocol);
            }
            let mut m = Vec::with_capacity(v as usize);
            for _ in 0..v {
                let k = decode_at(b, pos, depth + 1)?;
                let x = decode_at(b, pos, depth + 1)?;
                m.push((k, x));
            }
            Ok(Cbor::Map(m))
        },
        6 => decode_at(b, pos, depth + 1), // a tag: the tagged item is what counts
        _ => Err(Error::Protocol),
    }
}

/// A COSE_Key for a P-256 public key.
pub fn cose_p256(x: &[u8; 32], y: &[u8; 32], alg: i64) -> Cbor {
    Cbor::Map(alloc::vec![
        (Cbor::int(1), Cbor::int(2)),
        (Cbor::int(3), Cbor::int(alg)),
        (Cbor::int(-1), Cbor::int(1)),
        (Cbor::int(-2), Cbor::bytes(x)),
        (Cbor::int(-3), Cbor::bytes(y)),
    ])
}

/// The x / y of a P-256 COSE_Key (EC2, crv 1).
pub fn cose_xy(c: &Cbor) -> Option<([u8; 32], [u8; 32])> {
    if c.get_i(1)?.as_int()? != 2 || c.get_i(-1)?.as_int()? != 1 {
        return None;
    }
    let x: [u8; 32] = c.get_i(-2)?.as_bytes()?.try_into().ok()?;
    let y: [u8; 32] = c.get_i(-3)?.as_bytes()?.try_into().ok()?;
    Some((x, y))
}

/// SEC1 uncompressed 0x04 || x || y.
pub fn sec1(x: &[u8; 32], y: &[u8; 32]) -> [u8; 65] {
    let mut p = [0u8; 65];
    p[0] = 4;
    p[1..33].copy_from_slice(x);
    p[33..].copy_from_slice(y);
    p
}

// ── talking CBOR to the key ─────────────────────────────────────────────────

/// One CTAP2 command: `Ok(answer map)` on status 0, `Err(Ctap(status))` else.
pub fn call<T: Transport>(dev: &mut Device<T>, cmd: u8, params: Option<&Cbor>) -> Result<Cbor, Error> {
    let mut req = alloc::vec![cmd];
    if let Some(p) = params {
        p.encode(&mut req);
    }
    let r = dev.transact(CMD_CBOR, &req)?;
    let (&status, body) = r.split_first().ok_or(Error::Protocol)?;
    if status != 0 {
        return Err(Error::Ctap(status));
    }
    if body.is_empty() {
        return Ok(Cbor::Map(Vec::new()));
    }
    Ok(decode(body)?.0)
}

/// Tell the key to stop waiting for a touch (best effort; no answer expected).
pub fn cancel<T: Transport>(dev: &mut Device<T>) {
    for p in crate::frames(dev.cid, CMD_CANCEL, &[]) {
        let _ = dev.t.send(&p);
    }
}

/// What `authenticatorGetInfo` says about the key.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub versions: Vec<String>,
    pub extensions: Vec<String>,
    pub options: Vec<(String, bool)>,
    pub pin_protocols: Vec<u64>,
}

impl Info {
    pub fn parse(m: &Cbor) -> Info {
        let texts = |k: i64| -> Vec<String> {
            m.get_i(k).and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|v| v.as_text().map(String::from)).collect()).unwrap_or_default()
        };
        let options = match m.get_i(4) {
            Some(Cbor::Map(o)) => o.iter().filter_map(|(k, v)| Some((String::from(k.as_text()?), v.as_bool()?))).collect(),
            _ => Vec::new(),
        };
        let pin_protocols = m.get_i(6).and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|v| v.as_int().map(|i| i as u64)).collect()).unwrap_or_default();
        Info { versions: texts(1), extensions: texts(2), options, pin_protocols }
    }
    pub fn option(&self, name: &str) -> Option<bool> {
        self.options.iter().find(|(k, _)| k == name).map(|(_, v)| *v)
    }
    pub fn hmac_secret(&self) -> bool {
        self.extensions.iter().any(|e| e == "hmac-secret")
    }
    /// Protocol 2 when the key offers it (CTAP 2.1), else 1 (every CTAP 2.0 key).
    pub fn protocol(&self) -> u8 {
        if self.pin_protocols.contains(&2) {
            2
        } else {
            1
        }
    }
    /// The key insists on a PIN / fingerprint for every request (alwaysUv).
    pub fn always_uv(&self) -> bool {
        self.option("alwaysUv") == Some(true)
    }
}

pub fn get_info<T: Transport>(dev: &mut Device<T>) -> Result<Info, Error> {
    Ok(Info::parse(&call(dev, GET_INFO, None)?))
}

// ── shared secret (CTAP2 PIN/UV auth protocols 1 and 2) ────────────────────

/// The ECDH secret shared with the key, and our public key to send it.
pub struct Shared {
    pub protocol: u8,
    /// v1: SHA-256(Z.x) in the first 32 bytes; v2: HMAC key || AES key.
    key: [u8; 64],
    /// Our ephemeral public key as a COSE_Key (alg -25).
    pub platform_key: Cbor,
}

impl Drop for Shared {
    fn drop(&mut self) {
        for b in self.key.iter_mut() {
            unsafe { core::ptr::write_volatile(b, 0) };
        }
    }
}

/// Our public key for the scalar `eph` (big-endian), or None if it is not a valid scalar.
pub fn public_xy(eph: &[u8; 32]) -> Option<([u8; 32], [u8; 32])> {
    let sk: NonZeroScalar = Option::from(NonZeroScalar::from_repr((*eph).into()))?;
    let p = (ProjectivePoint::GENERATOR * *sk).to_affine().to_encoded_point(false);
    Some((p.x()?.as_slice().try_into().ok()?, p.y()?.as_slice().try_into().ok()?))
}

/// ECDH: the x coordinate of eph * peer.
pub fn ecdh_x(eph: &[u8; 32], peer_x: &[u8; 32], peer_y: &[u8; 32]) -> Option<[u8; 32]> {
    let sk: NonZeroScalar = Option::from(NonZeroScalar::from_repr((*eph).into()))?;
    let ep = EncodedPoint::from_affine_coordinates(peer_x.into(), peer_y.into(), false);
    let pk: PublicKey = Option::from(PublicKey::from_encoded_point(&ep))?;
    let z = (pk.to_projective() * *sk).to_affine().to_encoded_point(false);
    z.x()?.as_slice().try_into().ok()
}

impl Shared {
    /// The shared secret for our scalar `eph` and the key's agreement key.
    pub fn new(protocol: u8, eph: &[u8; 32], peer_x: &[u8; 32], peer_y: &[u8; 32]) -> Option<Shared> {
        let (px, py) = public_xy(eph)?;
        let mut z = ecdh_x(eph, peer_x, peer_y)?;
        let mut key = [0u8; 64];
        match protocol {
            1 => key[..32].copy_from_slice(&Sha256::digest(z)),
            2 => {
                let hk = hkdf::Hkdf::<Sha256>::new(Some(&[0u8; 32]), &z);
                hk.expand(b"CTAP2 HMAC key", &mut key[..32]).ok()?;
                hk.expand(b"CTAP2 AES key", &mut key[32..]).ok()?;
            },
            _ => return None,
        }
        for b in z.iter_mut() {
            unsafe { core::ptr::write_volatile(b, 0) };
        }
        Some(Shared { protocol, key, platform_key: cose_p256(&px, &py, ALG_ECDH_ES_HKDF_256) })
    }

    /// The raw shared key bytes (32 for protocol 1, 64 for protocol 2), for tests.
    pub fn key_bytes(&self) -> &[u8] {
        if self.protocol == 1 {
            &self.key[..32]
        } else {
            &self.key
        }
    }

    fn aes_key(&self) -> [u8; 32] {
        let mut k = [0u8; 32];
        k.copy_from_slice(if self.protocol == 1 { &self.key[..32] } else { &self.key[32..] });
        k
    }

    /// Encrypt whole blocks: protocol 1 with a zero IV, protocol 2 with `iv`
    /// sent in front of the ciphertext.
    pub fn encrypt(&self, iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
        let mut k = self.aes_key();
        let out = if self.protocol == 1 {
            cbc_encrypt(&k, &[0u8; 16], data)
        } else {
            let mut v = iv.to_vec();
            v.extend_from_slice(&cbc_encrypt(&k, iv, data));
            v
        };
        k.iter_mut().for_each(|b| unsafe { core::ptr::write_volatile(b, 0) });
        out
    }

    pub fn decrypt(&self, data: &[u8]) -> Option<Vec<u8>> {
        let mut k = self.aes_key();
        let out = if self.protocol == 1 {
            cbc_decrypt(&k, &[0u8; 16], data)
        } else {
            if data.len() < 16 {
                return None;
            }
            let iv: [u8; 16] = data[..16].try_into().ok()?;
            cbc_decrypt(&k, &iv, &data[16..])
        };
        k.iter_mut().for_each(|b| unsafe { core::ptr::write_volatile(b, 0) });
        out
    }

    /// HMAC-SHA-256 under the shared secret: 16 bytes for protocol 1, 32 for 2.
    pub fn authenticate(&self, msg: &[u8]) -> Vec<u8> {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&self.key[..32]).expect("any key length");
        m.update(msg);
        let t = m.finalize().into_bytes();
        if self.protocol == 1 {
            t[..16].to_vec()
        } else {
            t.to_vec()
        }
    }
}

fn cbc_encrypt(key: &[u8; 32], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let c = Aes256::new(key.into());
    let mut prev = *iv;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut blk = [0u8; 16];
        blk[..chunk.len()].copy_from_slice(chunk);
        for i in 0..16 {
            blk[i] ^= prev[i];
        }
        let mut ga = blk.into();
        c.encrypt_block(&mut ga);
        prev.copy_from_slice(&ga);
        out.extend_from_slice(&ga);
    }
    out
}

fn cbc_decrypt(key: &[u8; 32], iv: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    if data.len() % 16 != 0 {
        return None;
    }
    let c = Aes256::new(key.into());
    let mut prev = *iv;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut ga: aes::Block = (*<&[u8; 16]>::try_from(chunk).ok()?).into();
        c.decrypt_block(&mut ga);
        for i in 0..16 {
            out.push(ga[i] ^ prev[i]);
        }
        prev.copy_from_slice(chunk);
    }
    Some(out)
}

/// clientPIN getKeyAgreement, then our half of the ECDH with scalar `eph`.
pub fn key_agreement<T: Transport>(dev: &mut Device<T>, protocol: u8, eph: &[u8; 32]) -> Result<Shared, Error> {
    let req = Cbor::Map(alloc::vec![(Cbor::int(1), Cbor::int(protocol as i64)), (Cbor::int(2), Cbor::int(2))]);
    let r = call(dev, CLIENT_PIN, Some(&req))?;
    let (x, y) = r.get_i(1).and_then(cose_xy).ok_or(Error::Protocol)?;
    Shared::new(protocol, eph, &x, &y).ok_or(Error::Protocol)
}

// ── authenticator data ──────────────────────────────────────────────────────

pub const FLAG_UP: u8 = 0x01;
pub const FLAG_UV: u8 = 0x04;
pub const FLAG_AT: u8 = 0x40;
pub const FLAG_ED: u8 = 0x80;

#[derive(Clone, Debug)]
pub struct AuthData {
    pub flags: u8,
    pub counter: u32,
    /// From makeCredential: the credential id and its public key (SEC1).
    pub credential: Option<(Vec<u8>, [u8; 65])>,
    pub extensions: Option<Cbor>,
}

pub fn parse_auth_data(a: &[u8], rp_id: &str) -> Result<AuthData, Error> {
    if a.len() < 37 {
        return Err(Error::Protocol);
    }
    if a[..32] != Sha256::digest(rp_id.as_bytes())[..] {
        return Err(Error::Protocol);
    }
    let flags = a[32];
    let counter = u32::from_be_bytes([a[33], a[34], a[35], a[36]]);
    let mut pos = 37;
    let mut credential = None;
    if flags & FLAG_AT != 0 {
        if a.len() < pos + 18 {
            return Err(Error::Protocol);
        }
        pos += 16; // AAGUID
        let n = ((a[pos] as usize) << 8) | a[pos + 1] as usize;
        pos += 2;
        let id = a.get(pos..pos + n).ok_or(Error::Protocol)?.to_vec();
        pos += n;
        let (cose, used) = decode(&a[pos..])?;
        pos += used;
        if cose.get_i(3).and_then(|v| v.as_int()) != Some(ALG_ES256) {
            return Err(Error::Protocol); // asked for ES256 only
        }
        let (x, y) = cose_xy(&cose).ok_or(Error::Protocol)?;
        let pk = sec1(&x, &y);
        if VerifyingKey::from_sec1_bytes(&pk).is_err() {
            return Err(Error::Protocol);
        }
        credential = Some((id, pk));
    }
    let mut extensions = None;
    if flags & FLAG_ED != 0 {
        let (e, used) = decode(&a[pos..])?;
        pos += used;
        extensions = Some(e);
    }
    if pos != a.len() {
        return Err(Error::Protocol);
    }
    Ok(AuthData { flags, counter, credential, extensions })
}

// ── makeCredential ──────────────────────────────────────────────────────────

/// The request body (without the command byte), canonical CBOR.
pub fn make_credential_request(rp_id: &str, user_id: &[u8], user_name: &str, client_data_hash: &[u8; 32]) -> Cbor {
    use alloc::vec;
    Cbor::Map(vec![
        (Cbor::int(1), Cbor::bytes(client_data_hash)),
        (Cbor::int(2), Cbor::Map(vec![(Cbor::text("id"), Cbor::text(rp_id)), (Cbor::text("name"), Cbor::text("LingOS"))])),
        (Cbor::int(3), Cbor::Map(vec![(Cbor::text("id"), Cbor::bytes(user_id)), (Cbor::text("name"), Cbor::text(user_name))])),
        (Cbor::int(4), Cbor::Array(vec![Cbor::Map(vec![(Cbor::text("alg"), Cbor::int(ALG_ES256)), (Cbor::text("type"), Cbor::text("public-key"))])])),
        (Cbor::int(6), Cbor::Map(vec![(Cbor::text("hmac-secret"), Cbor::Bool(true))])),
    ])
}

#[derive(Clone, Debug)]
pub struct Credential {
    pub id: Vec<u8>,
    pub public_key: [u8; 65],
    /// The key confirmed it turned hmac-secret on for this credential.
    pub hmac_secret: bool,
    pub flags: u8,
}

/// A new (non-resident) credential with hmac-secret on. Waits for a touch.
pub fn make_credential<T: Transport>(dev: &mut Device<T>, rp_id: &str, user_id: &[u8], user_name: &str, client_data_hash: &[u8; 32]) -> Result<Credential, Error> {
    let r = call(dev, MAKE_CREDENTIAL, Some(&make_credential_request(rp_id, user_id, user_name, client_data_hash)))?;
    let ad = r.get_i(2).and_then(|v| v.as_bytes()).ok_or(Error::Protocol)?;
    let a = parse_auth_data(ad, rp_id)?;
    if a.flags & FLAG_UP == 0 {
        return Err(Error::Protocol);
    }
    let (id, public_key) = a.credential.ok_or(Error::Protocol)?;
    let hmac_secret = a.extensions.as_ref().and_then(|e| e.get_t("hmac-secret")).and_then(|v| v.as_bool()) == Some(true);
    Ok(Credential { id, public_key, hmac_secret, flags: a.flags })
}

// ── getAssertion with hmac-secret ───────────────────────────────────────────

/// The hmac-secret extension input for one 32-byte salt.
pub fn hmac_secret_input(shared: &Shared, salt: &[u8; 32], iv: &[u8; 16]) -> Cbor {
    let enc = shared.encrypt(iv, salt);
    let auth = shared.authenticate(&enc);
    let mut m = alloc::vec![(Cbor::int(1), shared.platform_key.clone()), (Cbor::int(2), Cbor::Bytes(enc)), (Cbor::int(3), Cbor::Bytes(auth))];
    if shared.protocol != 1 {
        m.push((Cbor::int(4), Cbor::int(shared.protocol as i64)));
    }
    Cbor::Map(m)
}

pub fn get_assertion_request(rp_id: &str, client_data_hash: &[u8; 32], allow: &[Vec<u8>], ext: Cbor) -> Cbor {
    use alloc::vec;
    let list = allow.iter().map(|id| Cbor::Map(vec![(Cbor::text("id"), Cbor::bytes(id)), (Cbor::text("type"), Cbor::text("public-key"))])).collect();
    Cbor::Map(vec![
        (Cbor::int(1), Cbor::text(rp_id)),
        (Cbor::int(2), Cbor::bytes(client_data_hash)),
        (Cbor::int(3), Cbor::Array(list)),
        (Cbor::int(4), Cbor::Map(vec![(Cbor::text("hmac-secret"), ext)])),
    ])
}

/// What the key answered: which credential, the 32-byte secret, and what to
/// check the signature with.
pub struct HmacAssertion {
    pub credential_id: Vec<u8>,
    pub output: [u8; 32],
    pub flags: u8,
    pub counter: u32,
    pub auth_data: Vec<u8>,
    pub signature: Vec<u8>,
}

impl Drop for HmacAssertion {
    fn drop(&mut self) {
        for b in self.output.iter_mut() {
            unsafe { core::ptr::write_volatile(b, 0) };
        }
    }
}

impl HmacAssertion {
    /// The credential's key signed authData || clientDataHash.
    pub fn verify(&self, public_key: &[u8], client_data_hash: &[u8; 32]) -> bool {
        let Ok(vk) = VerifyingKey::from_sec1_bytes(public_key) else { return false };
        let Ok(sig) = Signature::from_der(&self.signature) else { return false };
        let mut m = self.auth_data.clone();
        m.extend_from_slice(client_data_hash);
        vk.verify(&m, &sig).is_ok()
    }
}

/// HMAC-SHA-256(CredRandom, salt) from whichever credential in `allow` lives
/// on this key. Waits for a touch. `Err(Ctap(ERR_NO_CREDENTIALS))` when none
/// of them is on this key.
pub fn hmac_secret<T: Transport>(dev: &mut Device<T>, rp_id: &str, client_data_hash: &[u8; 32], allow: &[Vec<u8>], salt: &[u8; 32], shared: &Shared, iv: &[u8; 16]) -> Result<HmacAssertion, Error> {
    let req = get_assertion_request(rp_id, client_data_hash, allow, hmac_secret_input(shared, salt, iv));
    let r = call(dev, GET_ASSERTION, Some(&req))?;
    parse_hmac_assertion(&r, rp_id, allow, shared)
}

pub fn parse_hmac_assertion(r: &Cbor, rp_id: &str, allow: &[Vec<u8>], shared: &Shared) -> Result<HmacAssertion, Error> {
    let credential_id = match r.get_i(1).and_then(|c| c.get_t("id")).and_then(|v| v.as_bytes()) {
        Some(id) => id.to_vec(),
        None if allow.len() == 1 => allow[0].clone(), // may be left out when only one was allowed
        None => return Err(Error::Protocol),
    };
    if !allow.iter().any(|a| *a == credential_id) {
        return Err(Error::Protocol);
    }
    let auth_data = r.get_i(2).and_then(|v| v.as_bytes()).ok_or(Error::Protocol)?.to_vec();
    let signature = r.get_i(3).and_then(|v| v.as_bytes()).ok_or(Error::Protocol)?.to_vec();
    let a = parse_auth_data(&auth_data, rp_id)?;
    if a.flags & FLAG_UP == 0 {
        return Err(Error::Protocol);
    }
    let enc = a.extensions.as_ref().and_then(|e| e.get_t("hmac-secret")).and_then(|v| v.as_bytes()).ok_or(Error::Ctap(ERR_UNSUPPORTED_EXTENSION))?;
    let mut plain = shared.decrypt(enc).ok_or(Error::Protocol)?;
    if plain.len() != 32 {
        plain.iter_mut().for_each(|b| *b = 0);
        return Err(Error::Protocol);
    }
    let mut output = [0u8; 32];
    output.copy_from_slice(&plain);
    plain.iter_mut().for_each(|b| unsafe { core::ptr::write_volatile(b, 0) });
    Ok(HmacAssertion { credential_id, output, flags: a.flags, counter: a.counter, auth_data, signature })
}

/// A getAssertion with `up: false` (no touch): is one of `allow` on this key?
/// Used to tell which registered key is plugged in before asking for a touch.
pub fn owns_any<T: Transport>(dev: &mut Device<T>, rp_id: &str, allow: &[Vec<u8>]) -> Result<bool, Error> {
    use alloc::vec;
    let list = allow.iter().map(|id| Cbor::Map(vec![(Cbor::text("id"), Cbor::bytes(id)), (Cbor::text("type"), Cbor::text("public-key"))])).collect();
    let req = Cbor::Map(vec![
        (Cbor::int(1), Cbor::text(rp_id)),
        (Cbor::int(2), Cbor::Bytes(vec![0u8; 32])),
        (Cbor::int(3), Cbor::Array(list)),
        (Cbor::int(5), Cbor::Map(vec![(Cbor::text("up"), Cbor::Bool(false))])),
    ]);
    match call(dev, GET_ASSERTION, Some(&req)) {
        Ok(_) => Ok(true),
        Err(Error::Ctap(ERR_NO_CREDENTIALS)) => Ok(false),
        Err(e) => Err(e),
    }
}
