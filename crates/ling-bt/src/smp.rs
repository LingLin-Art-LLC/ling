//! SMP, the LE Security Manager (Core v5.3 Vol 3 Part H), **LE Secure
//! Connections only**, as the central (initiator).
//!
//! - Crypto: AES-CMAC (RFC 4493) and the toolbox functions f4 / f5 / f6 / g2 /
//!   ah / e, P-256 ECDH. All values are little-endian exactly as they travel in
//!   SMP PDUs (the functions swap to the spec's MSB-first order inside, as
//!   Linux's smp.c does); the tests use the spec's Appendix D vectors.
//! - Pairing: Pairing Request / Response, public keys, then just works,
//!   numeric comparison or passkey entry (we display), the DHKey check, the
//!   LTK, and the responder's identity (IRK + identity address).
//! - Refused: legacy pairing (no SC bit), key sizes under 16, a peer public
//!   key not on the curve, equal to ours or the spec's debug key, any
//!   mismatching confirm / DHKey check.

use crate::BdAddr;
use aes::cipher::{BlockEncrypt, KeyInit};
use aes::Aes128;
use alloc::string::String;
use alloc::vec::Vec;

pub const CID_SMP: u16 = 0x0006;

pub const PAIRING_REQUEST: u8 = 0x01;
pub const PAIRING_RESPONSE: u8 = 0x02;
pub const PAIRING_CONFIRM: u8 = 0x03;
pub const PAIRING_RANDOM: u8 = 0x04;
pub const PAIRING_FAILED: u8 = 0x05;
pub const ENCRYPTION_INFORMATION: u8 = 0x06;
pub const CENTRAL_IDENTIFICATION: u8 = 0x07;
pub const IDENTITY_INFORMATION: u8 = 0x08;
pub const IDENTITY_ADDRESS_INFORMATION: u8 = 0x09;
pub const SIGNING_INFORMATION: u8 = 0x0A;
pub const SECURITY_REQUEST: u8 = 0x0B;
pub const PAIRING_PUBLIC_KEY: u8 = 0x0C;
pub const PAIRING_DHKEY_CHECK: u8 = 0x0D;
pub const KEYPRESS: u8 = 0x0E;

/// Pairing Failed reasons.
pub const ERR_PASSKEY: u8 = 0x01;
pub const ERR_AUTH_REQUIREMENTS: u8 = 0x03;
pub const ERR_CONFIRM: u8 = 0x04;
pub const ERR_NOT_SUPPORTED: u8 = 0x05;
pub const ERR_KEY_SIZE: u8 = 0x06;
pub const ERR_UNSPECIFIED: u8 = 0x08;
pub const ERR_INVALID: u8 = 0x0A;
pub const ERR_DHKEY: u8 = 0x0B;
pub const ERR_NUMERIC: u8 = 0x0C;

/// IO capabilities.
pub const IO_DISPLAY_ONLY: u8 = 0x00;
pub const IO_DISPLAY_YES_NO: u8 = 0x01;
pub const IO_KEYBOARD_ONLY: u8 = 0x02;
pub const IO_NO_IO: u8 = 0x03;
pub const IO_KEYBOARD_DISPLAY: u8 = 0x04;

/// AuthReq bits.
pub const AUTH_BONDING: u8 = 0x01;
pub const AUTH_MITM: u8 = 0x04;
pub const AUTH_SC: u8 = 0x08;

/// Key distribution bits.
pub const DIST_ENC: u8 = 0x01;
pub const DIST_ID: u8 = 0x02;

// ── crypto ──────────────────────────────────────────────────────────────────

fn swap16(a: &[u8; 16]) -> [u8; 16] {
    let mut o = *a;
    o.reverse();
    o
}

/// AES-128 on MSB-first data (the spec's e() before byte swapping).
fn aes_be(key_be: &[u8; 16], block: &mut [u8; 16]) {
    let c = Aes128::new(key_be.into());
    let mut b = aes::Block::from(*block);
    c.encrypt_block(&mut b);
    block.copy_from_slice(&b);
}

/// AES-CMAC (RFC 4493), MSB-first key / message / result.
pub fn aes_cmac_be(key: &[u8; 16], msg: &[u8]) -> [u8; 16] {
    let c = Aes128::new(key.into());
    let enc = |b: &[u8; 16]| -> [u8; 16] {
        let mut blk = aes::Block::from(*b);
        c.encrypt_block(&mut blk);
        blk.into()
    };
    let dbl = |b: [u8; 16]| -> [u8; 16] {
        let mut o = [0u8; 16];
        let mut carry = 0u8;
        for i in (0..16).rev() {
            o[i] = (b[i] << 1) | carry;
            carry = b[i] >> 7;
        }
        if b[0] & 0x80 != 0 {
            o[15] ^= 0x87;
        }
        o
    };
    let l = enc(&[0u8; 16]);
    let k1 = dbl(l);
    let k2 = dbl(k1);
    let n = msg.len().div_ceil(16).max(1);
    let complete = !msg.is_empty() && msg.len() % 16 == 0;
    let mut x = [0u8; 16];
    for i in 0..n {
        let mut blk = [0u8; 16];
        let chunk = &msg[(i * 16).min(msg.len())..((i + 1) * 16).min(msg.len())];
        blk[..chunk.len()].copy_from_slice(chunk);
        if i == n - 1 {
            if complete {
                for j in 0..16 {
                    blk[j] ^= k1[j];
                }
            } else {
                blk[chunk.len()] = 0x80;
                for j in 0..16 {
                    blk[j] ^= k2[j];
                }
            }
        }
        for j in 0..16 {
            x[j] ^= blk[j];
        }
        x = enc(&x);
    }
    x
}

/// AES-CMAC with little-endian key / message / result (Linux's smp_aes_cmac).
pub fn aes_cmac(k: &[u8; 16], m: &[u8]) -> [u8; 16] {
    let mut msg: Vec<u8> = m.to_vec();
    msg.reverse();
    swap16(&aes_cmac_be(&swap16(k), &msg))
}

/// e(k, r), little-endian.
pub fn e(k: &[u8; 16], r: &[u8; 16]) -> [u8; 16] {
    let mut b = swap16(r);
    aes_be(&swap16(k), &mut b);
    swap16(&b)
}

pub fn f4(u: &[u8; 32], v: &[u8; 32], x: &[u8; 16], z: u8) -> [u8; 16] {
    let mut m = [0u8; 65];
    m[0] = z;
    m[1..33].copy_from_slice(v);
    m[33..65].copy_from_slice(u);
    aes_cmac(x, &m)
}

/// (MacKey, LTK)
pub fn f5(w: &[u8; 32], n1: &[u8; 16], n2: &[u8; 16], a1: &[u8; 7], a2: &[u8; 7]) -> ([u8; 16], [u8; 16]) {
    const BTLE: [u8; 4] = [0x65, 0x6c, 0x74, 0x62];
    const SALT: [u8; 16] = [0xbe, 0x83, 0x60, 0x5a, 0xdb, 0x0b, 0x37, 0x60, 0x38, 0xa5, 0xf5, 0xaa, 0x91, 0x83, 0x88, 0x6c];
    let t = aes_cmac(&SALT, w);
    let mut m = [0u8; 53];
    m[0..2].copy_from_slice(&[0x00, 0x01]);
    m[2..9].copy_from_slice(a2);
    m[9..16].copy_from_slice(a1);
    m[16..32].copy_from_slice(n2);
    m[32..48].copy_from_slice(n1);
    m[48..52].copy_from_slice(&BTLE);
    m[52] = 0;
    let mackey = aes_cmac(&t, &m);
    m[52] = 1;
    let ltk = aes_cmac(&t, &m);
    (mackey, ltk)
}

#[allow(clippy::too_many_arguments)]
pub fn f6(w: &[u8; 16], n1: &[u8; 16], n2: &[u8; 16], r: &[u8; 16], io_cap: &[u8; 3], a1: &[u8; 7], a2: &[u8; 7]) -> [u8; 16] {
    let mut m = [0u8; 65];
    m[0..7].copy_from_slice(a2);
    m[7..14].copy_from_slice(a1);
    m[14..17].copy_from_slice(io_cap);
    m[17..33].copy_from_slice(r);
    m[33..49].copy_from_slice(n2);
    m[49..65].copy_from_slice(n1);
    aes_cmac(w, &m)
}

/// The 6-digit numeric comparison value.
pub fn g2(u: &[u8; 32], v: &[u8; 32], x: &[u8; 16], y: &[u8; 16]) -> u32 {
    let mut m = [0u8; 80];
    m[0..16].copy_from_slice(y);
    m[16..48].copy_from_slice(v);
    m[48..80].copy_from_slice(u);
    let t = aes_cmac(x, &m);
    u32::from_le_bytes([t[0], t[1], t[2], t[3]]) % 1_000_000
}

/// ah(irk, prand): the hash half of a resolvable private address.
pub fn ah(irk: &[u8; 16], r: &[u8; 3]) -> [u8; 3] {
    let mut p = [0u8; 16];
    p[..3].copy_from_slice(r);
    let o = e(irk, &p);
    [o[0], o[1], o[2]]
}

/// Is `addr` (as on HCI, LSB first) a resolvable private address made from `irk`?
pub fn rpa_matches(irk: &[u8; 16], addr: &BdAddr) -> bool {
    // RPA: the two top bits of the most significant byte are 01
    if addr[5] >> 6 != 0b01 || irk.iter().all(|&b| b == 0) {
        return false;
    }
    ah(irk, &[addr[3], addr[4], addr[5]]) == [addr[0], addr[1], addr[2]]
}

/// The Core spec's SC debug public key (X || Y, little-endian): a peer using it
/// lets anyone in range derive the keys, so it is refused.
pub const DEBUG_PK: [u8; 64] = [
    0xe6, 0x9d, 0x35, 0x0e, 0x48, 0x01, 0x03, 0xcc, 0xdb, 0xfd, 0xf4, 0xac, 0x11, 0x91, 0xf4, 0xef, 0xb9, 0xa5, 0xf9, 0xe9, 0xa7, 0x83, 0x2c, 0x5e, 0x2c, 0xbe, 0x97, 0xf2, 0xd2, 0x03, 0xb0, 0x20, 0x8b, 0xd2, 0x89, 0x15, 0xd0, 0x8e, 0x1c, 0x74, 0x24, 0x30, 0xed, 0x8f, 0xc2, 0x45, 0x63, 0x76, 0x5c, 0x15, 0x52, 0x5a, 0xbf, 0x9a, 0x32, 0x63, 0x6d, 0xeb, 0x2a, 0x65, 0x49, 0x9c, 0x80, 0xdc,
];

/// A P-256 key pair from 32 random bytes: (secret, public X || Y little-endian).
pub fn keypair(seed: &[u8; 32]) -> Option<([u8; 32], [u8; 64])> {
    let sk = p256::SecretKey::from_slice(seed).ok()?;
    let pk = sk.public_key();
    let pt = p256::elliptic_curve::sec1::ToEncodedPoint::to_encoded_point(&pk, false);
    let b = pt.as_bytes();
    if b.len() != 65 {
        return None;
    }
    let mut out = [0u8; 64];
    for i in 0..32 {
        out[i] = b[32 - i];
        out[32 + i] = b[64 - i];
    }
    Some((*seed, out))
}

/// ECDH: our secret and their public key (X || Y little-endian) -> the DHKey
/// (little-endian). None when their key is not a valid curve point.
pub fn dhkey(sk: &[u8; 32], peer: &[u8; 64]) -> Option<[u8; 32]> {
    let mut sec1 = [0u8; 65];
    sec1[0] = 0x04;
    for i in 0..32 {
        sec1[1 + i] = peer[31 - i];
        sec1[33 + i] = peer[63 - i];
    }
    let pk = p256::PublicKey::from_sec1_bytes(&sec1).ok()?;
    let s = p256::SecretKey::from_slice(sk).ok()?;
    let shared = p256::ecdh::diffie_hellman(s.to_nonzero_scalar(), pk.as_affine());
    let raw = shared.raw_secret_bytes();
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = raw[31 - i];
    }
    Some(out)
}

// ── randomness ──────────────────────────────────────────────────────────────

/// AES-CTR generator over a seed from the kernel's RNG (RDRAND); fresh nonces
/// and keys for each pairing.
pub struct Drbg {
    key: [u8; 16],
    ctr: u128,
}

impl Drbg {
    pub fn new(seed: &[u8; 32]) -> Drbg {
        let mut key = [0u8; 16];
        key.copy_from_slice(&seed[..16]);
        let ctr = u128::from_le_bytes(seed[16..32].try_into().unwrap_or([0; 16]));
        Drbg { key, ctr }
    }

    pub fn fill(&mut self, out: &mut [u8]) {
        for c in out.chunks_mut(16) {
            let mut b = self.ctr.to_le_bytes();
            self.ctr = self.ctr.wrapping_add(1);
            aes_be(&self.key, &mut b);
            c.copy_from_slice(&b[..c.len()]);
        }
    }

    pub fn bytes16(&mut self) -> [u8; 16] {
        let mut b = [0u8; 16];
        self.fill(&mut b);
        b
    }
}

// ── pairing (central / initiator) ───────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Method {
    JustWorks,
    Numeric,
    /// We display, the peer types.
    Passkey,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum St {
    WaitResponse,
    WaitPublicKey,
    WaitConfirm,
    WaitRandom,
    /// Numeric comparison: the user decides.
    WaitUser,
    WaitDhkeyCheck,
    /// Encryption with the new LTK is being started.
    Encrypting,
    /// Encrypted: the peer distributes its identity.
    WaitKeys,
    Done,
    Failed,
}

#[derive(Debug, PartialEq)]
pub enum Event {
    Send(Vec<u8>),
    /// The method is known (the host may refuse just works for some devices).
    Method(Method),
    /// Show this 6-digit number and ask yes / no.
    Compare(u32),
    /// Show this passkey; the peer types it.
    ShowPasskey(u32),
    /// Start encryption with this LTK (LE Start Encryption, EDIV = Rand = 0).
    Encrypt([u8; 16]),
    /// Paired: the LTK, whether it is MITM-protected, the peer's IRK and identity address (type, addr).
    Done { ltk: [u8; 16], authenticated: bool, irk: Option<[u8; 16]>, identity: Option<(u8, BdAddr)> },
    /// Failed; `insecure` = the device only does legacy pairing.
    Failed { why: String, insecure: bool },
}

pub struct Pairing {
    st: St,
    pub method: Method,
    preq: [u8; 7],
    pres: [u8; 7],
    sk: [u8; 32],
    pka: [u8; 64],
    pkb: [u8; 64],
    dh: [u8; 32],
    na: [u8; 16],
    nb: [u8; 16],
    cb: [u8; 16],
    passkey: u32,
    round: u8,
    a: [u8; 7],
    b: [u8; 7],
    ltk: [u8; 16],
    want_keys: u8,
    irk: Option<[u8; 16]>,
    identity: Option<(u8, BdAddr)>,
    rng: Drbg,
}

fn fail_pdu(reason: u8) -> Vec<u8> {
    alloc::vec![PAIRING_FAILED, reason]
}

fn x32(pk: &[u8; 64]) -> [u8; 32] {
    let mut x = [0u8; 32];
    x.copy_from_slice(&pk[..32]);
    x
}

impl Pairing {
    /// Start pairing: our address (type, addr) and the peer's as used on this
    /// connection, and 32 random bytes for the keys and nonces.
    pub fn start(local: (u8, BdAddr), peer: (u8, BdAddr), seed: &[u8; 32]) -> Option<(Pairing, Vec<Event>)> {
        let mut rng = Drbg::new(seed);
        let mut sk_seed = [0u8; 32];
        let (sk, pka) = loop {
            rng.fill(&mut sk_seed);
            if let Some(k) = keypair(&sk_seed) {
                break k;
            }
        };
        let mut a = [0u8; 7];
        a[..6].copy_from_slice(&local.1);
        a[6] = local.0;
        let mut b = [0u8; 7];
        b[..6].copy_from_slice(&peer.1);
        b[6] = peer.0;
        // DisplayYesNo, no OOB, bonding + MITM + SC, key size 16, we distribute nothing, they send their identity
        let preq = [PAIRING_REQUEST, IO_DISPLAY_YES_NO, 0x00, AUTH_BONDING | AUTH_MITM | AUTH_SC, 16, 0x00, DIST_ID];
        let p = Pairing {
            st: St::WaitResponse,
            method: Method::JustWorks,
            preq,
            pres: [0; 7],
            sk,
            pka,
            pkb: [0; 64],
            dh: [0; 32],
            na: [0; 16],
            nb: [0; 16],
            cb: [0; 16],
            passkey: 0,
            round: 0,
            a,
            b,
            ltk: [0; 16],
            want_keys: 0,
            irk: None,
            identity: None,
            rng,
        };
        Some((p, alloc::vec![Event::Send(preq.to_vec())]))
    }

    pub fn done(&self) -> bool {
        matches!(self.st, St::Done | St::Failed)
    }

    fn fail(&mut self, reason: u8, why: &str, insecure: bool) -> Vec<Event> {
        self.st = St::Failed;
        alloc::vec![Event::Send(fail_pdu(reason)), Event::Failed { why: String::from(why), insecure }]
    }

    fn passkey_r(&self) -> [u8; 16] {
        let mut r = [0u8; 16];
        if self.method == Method::Passkey {
            r[..4].copy_from_slice(&self.passkey.to_le_bytes());
        }
        r
    }

    fn passkey_bit(&self) -> u8 {
        0x80 | ((self.passkey >> self.round) & 1) as u8
    }

    fn send_confirm(&mut self) -> Event {
        self.na = self.rng.bytes16();
        let z = if self.method == Method::Passkey { self.passkey_bit() } else { 0 };
        let c = f4(&x32(&self.pka), &x32(&self.pkb), &self.na, z);
        let mut v = alloc::vec![PAIRING_CONFIRM];
        v.extend_from_slice(&c);
        Event::Send(v)
    }

    fn send_random(&self) -> Event {
        let mut v = alloc::vec![PAIRING_RANDOM];
        v.extend_from_slice(&self.na);
        Event::Send(v)
    }

    fn dhkey_check(&mut self) -> Vec<Event> {
        let (mackey, ltk) = f5(&self.dh, &self.na, &self.nb, &self.a, &self.b);
        self.ltk = ltk;
        let r = self.passkey_r();
        let io: [u8; 3] = [self.preq[1], self.preq[2], self.preq[3]];
        let ea = f6(&mackey, &self.na, &self.nb, &r, &io, &self.a, &self.b);
        // what the peer must answer, kept as cb
        let io_b: [u8; 3] = [self.pres[1], self.pres[2], self.pres[3]];
        self.cb = f6(&mackey, &self.nb, &self.na, &r, &io_b, &self.b, &self.a);
        self.st = St::WaitDhkeyCheck;
        let mut v = alloc::vec![PAIRING_DHKEY_CHECK];
        v.extend_from_slice(&ea);
        alloc::vec![Event::Send(v)]
    }

    /// The user's answer to [`Event::Compare`].
    pub fn user_confirm(&mut self, yes: bool) -> Vec<Event> {
        if self.st != St::WaitUser {
            return Vec::new();
        }
        if !yes {
            return self.fail(ERR_NUMERIC, "the numbers did not match (or the user said no)", false);
        }
        self.dhkey_check()
    }

    /// The host refuses the method (just works for a device that may type).
    pub fn refuse(&mut self, why: &str) -> Vec<Event> {
        self.fail(ERR_AUTH_REQUIREMENTS, why, false)
    }

    /// Encryption with the new LTK is on: wait for the peer's keys.
    pub fn encrypted(&mut self) -> Vec<Event> {
        if self.st != St::Encrypting {
            return Vec::new();
        }
        self.st = St::WaitKeys;
        self.maybe_done()
    }

    fn maybe_done(&mut self) -> Vec<Event> {
        if self.st != St::WaitKeys {
            return Vec::new();
        }
        let need_id = self.want_keys & DIST_ID != 0;
        if need_id && (self.irk.is_none() || self.identity.is_none()) {
            return Vec::new();
        }
        self.st = St::Done;
        alloc::vec![Event::Done { ltk: self.ltk, authenticated: self.method != Method::JustWorks, irk: self.irk, identity: self.identity }]
    }

    /// One SMP PDU from the peer.
    pub fn input(&mut self, p: &[u8]) -> Vec<Event> {
        let Some(&code) = p.first() else { return Vec::new() };
        if matches!(self.st, St::Done | St::Failed) {
            return Vec::new();
        }
        let body = &p[1..];
        let need = |n: usize| body.len() == n;
        match (self.st, code) {
            (_, PAIRING_FAILED) => {
                self.st = St::Failed;
                let r = body.first().copied().unwrap_or(0);
                alloc::vec![Event::Failed { why: alloc::format!("the device ended pairing (reason {r:#04x})"), insecure: r == ERR_AUTH_REQUIREMENTS }]
            },
            (St::WaitResponse, PAIRING_RESPONSE) if need(6) => {
                self.pres.copy_from_slice(p);
                let (io, auth, max_key, rdist) = (body[0], body[2], body[3], body[5]);
                if auth & AUTH_SC == 0 {
                    return self.fail(ERR_AUTH_REQUIREMENTS, "only supports insecure pairing (no LE Secure Connections)", true);
                }
                if max_key < 16 {
                    return self.fail(ERR_KEY_SIZE, "offers an encryption key shorter than 16 bytes", false);
                }
                if io > IO_KEYBOARD_DISPLAY {
                    return self.fail(ERR_INVALID, "sent an invalid IO capability", false);
                }
                self.want_keys = rdist & self.preq[6];
                let mitm = (auth | self.preq[3]) & AUTH_MITM != 0;
                self.method = if !mitm {
                    Method::JustWorks
                } else {
                    match io {
                        IO_DISPLAY_YES_NO | IO_KEYBOARD_DISPLAY => Method::Numeric,
                        IO_KEYBOARD_ONLY => Method::Passkey,
                        _ => Method::JustWorks,
                    }
                };
                self.st = St::WaitPublicKey;
                let mut v = alloc::vec![PAIRING_PUBLIC_KEY];
                v.extend_from_slice(&self.pka);
                alloc::vec![Event::Method(self.method), Event::Send(v)]
            },
            (St::WaitPublicKey, PAIRING_PUBLIC_KEY) if need(64) => {
                self.pkb.copy_from_slice(body);
                if self.pkb == self.pka || self.pkb == DEBUG_PK {
                    return self.fail(ERR_INVALID, "sent a public key that cannot be trusted", false);
                }
                let Some(dh) = dhkey(&self.sk, &self.pkb) else {
                    return self.fail(ERR_INVALID, "sent a public key that is not on the curve", false);
                };
                self.dh = dh;
                self.st = St::WaitConfirm;
                if self.method == Method::Passkey {
                    let mut b = [0u8; 4];
                    self.rng.fill(&mut b);
                    self.passkey = u32::from_le_bytes(b) % 1_000_000;
                    self.round = 0;
                    let c = self.send_confirm();
                    return alloc::vec![Event::ShowPasskey(self.passkey), c];
                }
                Vec::new()
            },
            (St::WaitConfirm, PAIRING_CONFIRM) if need(16) => {
                self.cb.copy_from_slice(body);
                if self.method != Method::Passkey {
                    self.na = self.rng.bytes16();
                }
                self.st = St::WaitRandom;
                alloc::vec![self.send_random()]
            },
            (St::WaitRandom, PAIRING_RANDOM) if need(16) => {
                self.nb.copy_from_slice(body);
                let z = if self.method == Method::Passkey { self.passkey_bit() } else { 0 };
                let expect = f4(&x32(&self.pkb), &x32(&self.pka), &self.nb, z);
                if expect != self.cb {
                    return self.fail(if self.method == Method::Passkey { ERR_PASSKEY } else { ERR_CONFIRM }, "confirm value mismatch (wrong passkey, or an attacker in the middle)", false);
                }
                match self.method {
                    Method::Passkey => {
                        self.round += 1;
                        if self.round < 20 {
                            self.st = St::WaitConfirm;
                            return alloc::vec![self.send_confirm()];
                        }
                        self.dhkey_check()
                    },
                    Method::Numeric => {
                        self.st = St::WaitUser;
                        alloc::vec![Event::Compare(g2(&x32(&self.pka), &x32(&self.pkb), &self.na, &self.nb))]
                    },
                    Method::JustWorks => self.dhkey_check(),
                }
            },
            (St::WaitDhkeyCheck, PAIRING_DHKEY_CHECK) if need(16) => {
                if body != self.cb {
                    return self.fail(ERR_DHKEY, "DHKey check failed", false);
                }
                self.st = St::Encrypting;
                alloc::vec![Event::Encrypt(self.ltk)]
            },
            (St::WaitKeys, IDENTITY_INFORMATION) if need(16) => {
                let mut k = [0u8; 16];
                k.copy_from_slice(body);
                self.irk = Some(k);
                self.maybe_done()
            },
            (St::WaitKeys, IDENTITY_ADDRESS_INFORMATION) if need(7) => {
                let mut a = [0u8; 6];
                a.copy_from_slice(&body[1..7]);
                // an identity address is public, or random static (top bits 11)
                if body[0] > 1 || (body[0] == 1 && a[5] >> 6 != 0b11) {
                    return self.fail(ERR_INVALID, "sent an invalid identity address", false);
                }
                self.identity = Some((body[0], a));
                self.maybe_done()
            },
            // legacy key distribution (EncKey) and signing keys: not used with SC, ignored
            (St::WaitKeys, ENCRYPTION_INFORMATION | CENTRAL_IDENTIFICATION | SIGNING_INFORMATION) => Vec::new(),
            (_, KEYPRESS | SECURITY_REQUEST) => Vec::new(),
            // anything else, or a wrong length: the pairing is over
            _ => self.fail(ERR_INVALID, "sent an unexpected pairing message", false),
        }
    }
}
