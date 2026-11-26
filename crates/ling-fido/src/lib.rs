//! Security keys for LingOS sign-in, freestanding (`no_std` + `alloc`).
//!
//! A USB security key (YubiKey, SoloKey, Google Titan, Feitian, Nitrokey...)
//! is a HID device whose 64-byte reports carry the CTAPHID protocol. Inside
//! CTAPHID this crate speaks FIDO U2F (CTAP1), which every FIDO key made in
//! the last decade still answers:
//!
//! * [`register`] makes a new key pair on the key for one application and
//!   returns its public key plus an opaque *key handle* (the key needs a touch);
//! * [`authenticate`] asks the key to sign a fresh challenge with that key
//!   pair (another touch), and [`verify`] checks the P-256 ECDSA signature
//!   against the public key saved at registration.
//!
//! Nothing here talks to a network: the public key and key handle stay on the
//! machine, and the key's private key never leaves the key.
//!
//! The crate does not own the USB stack. The kernel hands it a [`Transport`]
//! that sends and receives one 64-byte HID report at a time.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

/// One HID report.
pub const PKT: usize = 64;
/// The channel every device listens on before CTAPHID_INIT hands out its own.
pub const BROADCAST_CID: u32 = 0xFFFF_FFFF;

pub const CMD_MSG: u8 = 0x83;
pub const CMD_INIT: u8 = 0x86;
pub const CMD_WINK: u8 = 0x88;
pub const CMD_KEEPALIVE: u8 = 0xBB;
pub const CMD_ERROR: u8 = 0xBF;

/// A U2F public key: SEC1 uncompressed point, 0x04 || X || Y.
pub const PUBKEY_LEN: usize = 65;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The transport failed (device unplugged, USB error).
    Io,
    /// No answer in time.
    Timeout,
    /// The device answered with something that is not CTAPHID / U2F.
    Protocol,
    /// A CTAPHID_ERROR code from the device.
    Device(u8),
    /// The key wants a touch first (U2F status 0x6985). Ask again shortly.
    NeedTouch,
    /// The key does not know this key handle (U2F status 0x6A80): it is a
    /// different key from the one registered.
    WrongKey,
    /// Any other U2F status word.
    Status(u16),
    /// A CTAP2 status byte other than success (see [`ctap2`]).
    Ctap(u8),
}

/// What the kernel's USB driver provides: one 64-byte report out, one in.
pub trait Transport {
    fn send(&mut self, pkt: &[u8; PKT]) -> Result<(), Error>;
    /// Wait up to `timeout_ms` for the next report.
    fn recv(&mut self, pkt: &mut [u8; PKT], timeout_ms: u32) -> Result<(), Error>;
}

// ── CTAPHID framing ─────────────────────────────────────────────────────────

/// Split one message into the init packet and its continuation packets.
pub fn frames(cid: u32, cmd: u8, payload: &[u8]) -> Vec<[u8; PKT]> {
    let mut out = Vec::new();
    let mut p = [0u8; PKT];
    p[..4].copy_from_slice(&cid.to_be_bytes());
    p[4] = cmd | 0x80;
    p[5] = (payload.len() >> 8) as u8;
    p[6] = payload.len() as u8;
    let first = payload.len().min(PKT - 7);
    p[7..7 + first].copy_from_slice(&payload[..first]);
    out.push(p);
    let mut rest = &payload[first..];
    let mut seq = 0u8;
    while !rest.is_empty() {
        let mut c = [0u8; PKT];
        c[..4].copy_from_slice(&cid.to_be_bytes());
        c[4] = seq;
        let n = rest.len().min(PKT - 5);
        c[5..5 + n].copy_from_slice(&rest[..n]);
        out.push(c);
        rest = &rest[n..];
        seq += 1;
    }
    out
}

/// Collects the packets of one response on one channel.
pub struct Reassembler {
    cid: u32,
    cmd: u8,
    need: usize,
    seq: u8,
    pub data: Vec<u8>,
}

impl Reassembler {
    pub fn new(cid: u32) -> Self {
        Reassembler { cid, cmd: 0, need: 0, seq: 0, data: Vec::new() }
    }

    /// Feed one packet. `Ok(Some(cmd))` once the message is complete,
    /// `Ok(None)` while more is needed (or the packet was for another channel).
    pub fn push(&mut self, p: &[u8; PKT]) -> Result<Option<u8>, Error> {
        let cid = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
        if cid != self.cid {
            return Ok(None);
        }
        if p[4] & 0x80 != 0 {
            self.cmd = p[4];
            self.need = ((p[5] as usize) << 8) | p[6] as usize;
            if self.need > 7609 {
                return Err(Error::Protocol);
            }
            self.seq = 0;
            self.data.clear();
            let n = self.need.min(PKT - 7);
            self.data.extend_from_slice(&p[7..7 + n]);
        } else {
            if self.cmd == 0 || p[4] != self.seq {
                return Err(Error::Protocol);
            }
            self.seq += 1;
            let n = (self.need - self.data.len()).min(PKT - 5);
            self.data.extend_from_slice(&p[5..5 + n]);
        }
        if self.data.len() >= self.need {
            Ok(Some(self.cmd))
        } else {
            Ok(None)
        }
    }
}

/// A key with its own CTAPHID channel.
pub struct Device<T: Transport> {
    pub t: T,
    pub cid: u32,
    /// CTAPHID capability flags (bit 0 wink, bit 2 CBOR / FIDO2, bit 3 no MSG).
    pub caps: u8,
    /// Device version: major, minor, build.
    pub version: [u8; 3],
}

impl<T: Transport> Device<T> {
    /// CTAPHID_INIT on the broadcast channel: the device hands out a channel id.
    /// `nonce` should be random (it pairs the answer with this request).
    pub fn open(mut t: T, nonce: [u8; 8]) -> Result<Self, Error> {
        for p in frames(BROADCAST_CID, CMD_INIT, &nonce) {
            t.send(&p)?;
        }
        let mut r = Reassembler::new(BROADCAST_CID);
        let mut buf = [0u8; PKT];
        for _ in 0..32 {
            t.recv(&mut buf, 1000)?;
            match r.push(&buf) {
                Ok(Some(CMD_INIT)) if r.data.len() >= 17 && r.data[..8] == nonce => {
                    let d = &r.data;
                    return Ok(Device {
                        t,
                        cid: u32::from_be_bytes([d[8], d[9], d[10], d[11]]),
                        version: [d[13], d[14], d[15]],
                        caps: d[16],
                    });
                },
                Ok(Some(CMD_ERROR)) => return Err(Error::Device(r.data.first().copied().unwrap_or(0))),
                Ok(_) => {},
                Err(_) => r = Reassembler::new(BROADCAST_CID), // a stale packet: keep listening
            }
        }
        Err(Error::Protocol)
    }

    /// Send one request and wait for its response (skipping KEEPALIVEs).
    pub fn transact(&mut self, cmd: u8, payload: &[u8]) -> Result<Vec<u8>, Error> {
        for p in frames(self.cid, cmd, payload) {
            self.t.send(&p)?;
        }
        let mut r = Reassembler::new(self.cid);
        let mut buf = [0u8; PKT];
        loop {
            self.t.recv(&mut buf, 3000)?;
            match r.push(&buf)? {
                Some(CMD_KEEPALIVE) => r = Reassembler::new(self.cid),
                Some(CMD_ERROR) => return Err(Error::Device(r.data.first().copied().unwrap_or(0))),
                Some(c) if c == cmd | 0x80 => return Ok(core::mem::take(&mut r.data)),
                Some(_) => return Err(Error::Protocol),
                None => {},
            }
        }
    }

    /// Blink the key's light (when it can), so the user knows which one to touch.
    pub fn wink(&mut self) -> Result<(), Error> {
        if self.caps & 1 == 0 {
            return Ok(());
        }
        self.transact(CMD_WINK, &[]).map(|_| ())
    }

    /// One U2F APDU (extended length; ISO short form if the key insists).
    pub fn apdu(&mut self, ins: u8, p1: u8, data: &[u8]) -> Result<Vec<u8>, Error> {
        let mut a = Vec::with_capacity(data.len() + 9);
        a.extend_from_slice(&[0, ins, p1, 0, 0, (data.len() >> 8) as u8, data.len() as u8]);
        a.extend_from_slice(data);
        a.extend_from_slice(&[0, 0]);
        let mut r = self.transact(CMD_MSG, &a)?;
        if status(&r) == 0x6700 && data.len() < 256 {
            // Wrong length: an old key that only takes short APDUs.
            a.clear();
            a.extend_from_slice(&[0, ins, p1, 0, data.len() as u8]);
            a.extend_from_slice(data);
            a.push(0);
            r = self.transact(CMD_MSG, &a)?;
        }
        match status(&r) {
            0x9000 => {
                r.truncate(r.len() - 2);
                Ok(r)
            },
            0x6985 => Err(Error::NeedTouch),
            0x6A80 => Err(Error::WrongKey),
            0 => Err(Error::Protocol),
            sw => Err(Error::Status(sw)),
        }
    }
}

fn status(r: &[u8]) -> u16 {
    if r.len() < 2 {
        return 0;
    }
    ((r[r.len() - 2] as u16) << 8) | r[r.len() - 1] as u16
}

// ── U2F ─────────────────────────────────────────────────────────────────────

pub const INS_REGISTER: u8 = 0x01;
pub const INS_AUTHENTICATE: u8 = 0x02;
pub const INS_VERSION: u8 = 0x03;
/// Authenticate: the key must see a touch and then signs.
pub const AUTH_ENFORCE: u8 = 0x03;
/// Authenticate: only say whether the key handle is ours (no touch, no signature).
pub const AUTH_CHECK_ONLY: u8 = 0x07;

/// The 32-byte application parameter for a name (SHA-256 of it). LingOS uses
/// one per machine so a key handle made here is useless anywhere else.
pub fn app_id(name: &str) -> [u8; 32] {
    Sha256::digest(name.as_bytes()).into()
}

/// What a key returns from registration that we keep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    pub public_key: [u8; PUBKEY_LEN],
    pub key_handle: Vec<u8>,
}

/// A signed answer to a challenge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assertion {
    /// Bit 0: the user touched the key.
    pub flags: u8,
    /// The key's signature counter (never goes down on a genuine key).
    pub counter: u32,
    /// DER-encoded ECDSA signature.
    pub signature: Vec<u8>,
}

/// Make a new credential on the key. `Err(NeedTouch)` until the user touches
/// it: call again every ~200 ms while showing "touch your key".
pub fn register<T: Transport>(dev: &mut Device<T>, challenge: &[u8; 32], app: &[u8; 32]) -> Result<Registration, Error> {
    let mut d = [0u8; 64];
    d[..32].copy_from_slice(challenge);
    d[32..].copy_from_slice(app);
    let r = dev.apdu(INS_REGISTER, 0, &d)?;
    parse_registration(&r)
}

pub fn parse_registration(r: &[u8]) -> Result<Registration, Error> {
    if r.len() < 1 + PUBKEY_LEN + 1 || r[0] != 0x05 || r[1] != 0x04 {
        return Err(Error::Protocol);
    }
    let mut public_key = [0u8; PUBKEY_LEN];
    public_key.copy_from_slice(&r[1..1 + PUBKEY_LEN]);
    let kl = r[1 + PUBKEY_LEN] as usize;
    let ks = 2 + PUBKEY_LEN;
    if kl == 0 || r.len() < ks + kl {
        return Err(Error::Protocol);
    }
    // the public key must be a real point on the curve
    if VerifyingKey::from_sec1_bytes(&public_key).is_err() {
        return Err(Error::Protocol);
    }
    Ok(Registration { public_key, key_handle: r[ks..ks + kl].to_vec() })
}

fn auth_data(challenge: &[u8; 32], app: &[u8; 32], key_handle: &[u8]) -> Result<Vec<u8>, Error> {
    if key_handle.is_empty() || key_handle.len() > 255 {
        return Err(Error::Protocol);
    }
    let mut d = Vec::with_capacity(65 + key_handle.len());
    d.extend_from_slice(challenge);
    d.extend_from_slice(app);
    d.push(key_handle.len() as u8);
    d.extend_from_slice(key_handle);
    Ok(d)
}

/// Is `key_handle` one of this key's? (No touch needed.) `Ok(true)` yes,
/// `Ok(false)` it belongs to another key.
pub fn owns<T: Transport>(dev: &mut Device<T>, app: &[u8; 32], key_handle: &[u8]) -> Result<bool, Error> {
    let d = auth_data(&[0u8; 32], app, key_handle)?;
    match dev.apdu(INS_AUTHENTICATE, AUTH_CHECK_ONLY, &d) {
        Err(Error::NeedTouch) => Ok(true), // "conditions not satisfied" = it is ours
        Err(Error::WrongKey) => Ok(false),
        Ok(_) => Ok(true),
        Err(e) => Err(e),
    }
}

/// Ask the key to sign `challenge`. `Err(NeedTouch)` until it is touched,
/// `Err(WrongKey)` if this key handle belongs to another key.
pub fn authenticate<T: Transport>(dev: &mut Device<T>, challenge: &[u8; 32], app: &[u8; 32], key_handle: &[u8]) -> Result<Assertion, Error> {
    let d = auth_data(challenge, app, key_handle)?;
    let r = dev.apdu(INS_AUTHENTICATE, AUTH_ENFORCE, &d)?;
    if r.len() < 5 + 8 {
        return Err(Error::Protocol);
    }
    Ok(Assertion { flags: r[0], counter: u32::from_be_bytes([r[1], r[2], r[3], r[4]]), signature: r[5..].to_vec() })
}

/// The bytes a U2F key signs when authenticating.
pub fn signed_bytes(app: &[u8; 32], flags: u8, counter: u32, challenge: &[u8; 32]) -> [u8; 69] {
    let mut m = [0u8; 69];
    m[..32].copy_from_slice(app);
    m[32] = flags;
    m[33..37].copy_from_slice(&counter.to_be_bytes());
    m[37..].copy_from_slice(challenge);
    m
}

/// Did the key that registered `public_key` sign this challenge, with a touch?
pub fn verify(public_key: &[u8], app: &[u8; 32], challenge: &[u8; 32], a: &Assertion) -> bool {
    if a.flags & 1 == 0 {
        return false;
    }
    let Ok(vk) = VerifyingKey::from_sec1_bytes(public_key) else { return false };
    let Ok(sig) = Signature::from_der(&a.signature) else { return false };
    // Some keys emit high-S signatures; ECDSA accepts both forms.
    vk.verify(&signed_bytes(app, a.flags, a.counter, challenge), &sig).is_ok()
}

/// CTAP2 (FIDO2): hmac-secret for wrapping data keys with a security key.
pub mod ctap2;

/// Keyslots: an app secret wrapped once per security key (hmac-secret / U2F).
pub mod keyslot;

/// OpenSSH security-key public keys and signatures (the SSH server's `publickey` login).
pub mod ssh;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_ctap2;
