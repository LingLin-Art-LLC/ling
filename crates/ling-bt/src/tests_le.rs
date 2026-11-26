//! Bluetooth LE + classic gamepads: SMP crypto against the Core spec's
//! Appendix D vectors (as Linux's smp.c selftest carries them), full Secure
//! Connections pairings against a simulated responder, GATT / HOGP discovery
//! against a simulated attribute server, the host's LE flows (pair, reconnect
//! through an RPA, block by identity), classic HID gamepads over SDP, and
//! fuzzing of every new parser.

use crate::gatt;
use crate::hci::{self, op};
use crate::host::{Host, Output, Prompt, State};
use crate::sdp;
use crate::smp::{self, *};
use crate::store::{self, Store};
use crate::*;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

fn h(s: &str) -> Vec<u8> {
    crate::unhex(&s.replace(' ', "")).unwrap()
}

// ── crypto ──────────────────────────────────────────────────────────────────

#[test]
fn aes_cmac_rfc4493() {
    let k: [u8; 16] = h("2b7e151628aed2a6abf7158809cf4f3c").try_into().unwrap();
    assert_eq!(aes_cmac_be(&k, &[]).to_vec(), h("bb1d6929e95937287fa37d129b756746"));
    assert_eq!(aes_cmac_be(&k, &h("6bc1bee22e409f96e93d7e117393172a")).to_vec(), h("070a16b46b4d4144f79bdd9dd04a287c"));
    let m40 = h("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e5130c81c46a35ce411");
    assert_eq!(aes_cmac_be(&k, &m40).to_vec(), h("dfa66747de9ae63030ca32611497c827"));
}

const U: [u8; 32] = [0xe6, 0x9d, 0x35, 0x0e, 0x48, 0x01, 0x03, 0xcc, 0xdb, 0xfd, 0xf4, 0xac, 0x11, 0x91, 0xf4, 0xef, 0xb9, 0xa5, 0xf9, 0xe9, 0xa7, 0x83, 0x2c, 0x5e, 0x2c, 0xbe, 0x97, 0xf2, 0xd2, 0x03, 0xb0, 0x20];
const V: [u8; 32] = [0xfd, 0xc5, 0x7f, 0xf4, 0x49, 0xdd, 0x4f, 0x6b, 0xfb, 0x7c, 0x9d, 0xf1, 0xc2, 0x9a, 0xcb, 0x59, 0x2a, 0xe7, 0xd4, 0xee, 0xfb, 0xfc, 0x0a, 0x90, 0x9a, 0xbb, 0xf6, 0x32, 0x3d, 0x8b, 0x18, 0x55];
const X: [u8; 16] = [0xab, 0xae, 0x2b, 0x71, 0xec, 0xb2, 0xff, 0xff, 0x3e, 0x73, 0x77, 0xd1, 0x54, 0x84, 0xcb, 0xd5];
const N2: [u8; 16] = [0xcf, 0xc4, 0x3d, 0xff, 0xf7, 0x83, 0x65, 0x21, 0x6e, 0x5f, 0xa7, 0x25, 0xcc, 0xe7, 0xe8, 0xa6];
const A1: [u8; 7] = [0xce, 0xbf, 0x37, 0x37, 0x12, 0x56, 0x00];
const A2: [u8; 7] = [0xc1, 0xcf, 0x2d, 0x70, 0x13, 0xa7, 0x00];

#[test]
fn spec_f4() {
    let exp = [0x2d, 0x87, 0x74, 0xa9, 0xbe, 0xa1, 0xed, 0xf1, 0x1c, 0xbd, 0xa9, 0x07, 0xf1, 0x16, 0xc9, 0xf2];
    assert_eq!(f4(&U, &V, &X, 0), exp);
}

#[test]
fn spec_f5() {
    let w = [0x98, 0xa6, 0xbf, 0x73, 0xf3, 0x34, 0x8d, 0x86, 0xf1, 0x66, 0xf8, 0xb4, 0x13, 0x6b, 0x79, 0x99, 0x9b, 0x7d, 0x39, 0x0a, 0xa6, 0x10, 0x10, 0x34, 0x05, 0xad, 0xc8, 0x57, 0xa3, 0x34, 0x02, 0xec];
    let (mackey, ltk) = f5(&w, &X, &N2, &A1, &A2);
    assert_eq!(ltk, [0x38, 0x0a, 0x75, 0x94, 0xb5, 0x22, 0x05, 0x98, 0x23, 0xcd, 0xd7, 0x69, 0x11, 0x79, 0x86, 0x69]);
    assert_eq!(mackey, [0x20, 0x6e, 0x63, 0xce, 0x20, 0x6a, 0x3f, 0xfd, 0x02, 0x4a, 0x08, 0xa1, 0x76, 0xf1, 0x65, 0x29]);
}

#[test]
fn spec_f6() {
    let w = [0x20, 0x6e, 0x63, 0xce, 0x20, 0x6a, 0x3f, 0xfd, 0x02, 0x4a, 0x08, 0xa1, 0x76, 0xf1, 0x65, 0x29];
    let r = [0xc8, 0x0f, 0x2d, 0x0c, 0xd2, 0x42, 0xda, 0x08, 0x54, 0xbb, 0x53, 0xb4, 0x3b, 0x34, 0xa3, 0x12];
    let exp = [0x61, 0x8f, 0x95, 0xda, 0x09, 0x0b, 0x6c, 0xd2, 0xc5, 0xe8, 0xd0, 0x9c, 0x98, 0x73, 0xc4, 0xe3];
    assert_eq!(f6(&w, &X, &N2, &r, &[0x02, 0x01, 0x01], &A1, &A2), exp);
}

#[test]
fn spec_g2_ah() {
    assert_eq!(g2(&U, &V, &X, &N2), 0x2f9ed5ba % 1_000_000);
    let irk = [0x9b, 0x7d, 0x39, 0x0a, 0xa6, 0x10, 0x10, 0x34, 0x05, 0xad, 0xc8, 0x57, 0xa3, 0x34, 0x02, 0xec];
    assert_eq!(ah(&irk, &[0x94, 0x81, 0x70]), [0xaa, 0xfb, 0x0d]);
    // an RPA built from it resolves; one bit off does not
    let rpa: BdAddr = [0xaa, 0xfb, 0x0d, 0x94, 0x81, 0x70];
    assert!(rpa_matches(&irk, &rpa));
    let mut bad = rpa;
    bad[0] ^= 1;
    assert!(!rpa_matches(&irk, &bad));
}

#[test]
fn ecdh_debug_keypair_and_agreement() {
    // the spec's debug private key (little-endian in Linux) gives the debug public key
    let mut sk = [0xbd, 0x1a, 0x3c, 0xcd, 0xa6, 0xb8, 0x99, 0x58, 0x99, 0xb7, 0x40, 0xeb, 0x7b, 0x60, 0xff, 0x4a, 0x50, 0x3f, 0x10, 0xd2, 0xe3, 0xb3, 0xc9, 0x74, 0x38, 0x5f, 0xc5, 0xa3, 0xd4, 0xf6, 0x49, 0x3f];
    sk.reverse(); // our keypair() takes the scalar big-endian
    let (_, pk) = keypair(&sk).unwrap();
    assert_eq!(pk, DEBUG_PK);
    let (a_sk, a_pk) = keypair(&[7u8; 32]).unwrap();
    let (b_sk, b_pk) = keypair(&[9u8; 32]).unwrap();
    assert_eq!(dhkey(&a_sk, &b_pk), dhkey(&b_sk, &a_pk));
    let mut off = b_pk;
    off[40] ^= 1;
    assert!(dhkey(&a_sk, &off).is_none(), "a point off the curve is refused");
}

// ── SMP against a simulated responder ──────────────────────────────────────

struct Peer {
    io: u8,
    auth: u8,
    key: u8,
    sk: [u8; 32],
    pk: [u8; 64],
    pka: [u8; 64],
    preq: [u8; 7],
    pres: [u8; 7],
    nb: [u8; 16],
    na: [u8; 16],
    passkey: u32,
    round: u32,
    dh: [u8; 32],
    a: [u8; 7],
    b: [u8; 7],
    pub ltk: Option<[u8; 16]>,
    irk: [u8; 16],
    id: BdAddr,
    use_debug_key: bool,
}

impl Peer {
    fn new(io: u8, auth: u8, a: [u8; 7], b: [u8; 7]) -> Peer {
        let (sk, pk) = keypair(&[0x33; 32]).unwrap();
        Peer { io, auth, key: 16, sk, pk, pka: [0; 64], preq: [0; 7], pres: [0; 7], nb: [0x44; 16], na: [0; 16], passkey: 0, round: 0, dh: [0; 32], a, b, ltk: None, irk: [0x5C; 16], id: [0x11, 0x22, 0x33, 0x44, 0x55, 0xC6], use_debug_key: false }
    }

    fn rb(&self) -> u8 {
        0x80 | ((self.passkey >> self.round) & 1) as u8
    }

    /// One SMP PDU from the initiator: the replies.
    fn input(&mut self, p: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        match p[0] {
            PAIRING_REQUEST => {
                self.preq.copy_from_slice(p);
                self.pres = [PAIRING_RESPONSE, self.io, 0, self.auth, self.key, 0, DIST_ID];
                out.push(self.pres.to_vec());
            },
            PAIRING_PUBLIC_KEY => {
                self.pka.copy_from_slice(&p[1..65]);
                self.dh = dhkey(&self.sk, &self.pka).unwrap();
                let mut v = vec![PAIRING_PUBLIC_KEY];
                v.extend_from_slice(if self.use_debug_key { &DEBUG_PK } else { &self.pk });
                out.push(v);
                if self.io != IO_KEYBOARD_ONLY {
                    let mut c = vec![PAIRING_CONFIRM];
                    c.extend_from_slice(&f4(&x(&self.pk), &x(&self.pka), &self.nb, 0));
                    out.push(c);
                }
            },
            PAIRING_CONFIRM => {
                // passkey entry: answer with ours for this bit
                self.nb = [self.round as u8 + 1; 16];
                let mut c = vec![PAIRING_CONFIRM];
                c.extend_from_slice(&f4(&x(&self.pk), &x(&self.pka), &self.nb, self.rb()));
                out.push(c);
            },
            PAIRING_RANDOM => {
                self.na.copy_from_slice(&p[1..17]);
                let mut r = vec![PAIRING_RANDOM];
                r.extend_from_slice(&self.nb);
                out.push(r);
                if self.io == IO_KEYBOARD_ONLY {
                    self.round += 1;
                }
            },
            PAIRING_DHKEY_CHECK => {
                let (mackey, ltk) = f5(&self.dh, &self.na, &self.nb, &self.a, &self.b);
                let mut r = [0u8; 16];
                if self.io == IO_KEYBOARD_ONLY {
                    r[..4].copy_from_slice(&self.passkey.to_le_bytes());
                }
                let ea = f6(&mackey, &self.na, &self.nb, &r, &[self.preq[1], self.preq[2], self.preq[3]], &self.a, &self.b);
                assert_eq!(&p[1..17], &ea, "the initiator's DHKey check verifies");
                let eb = f6(&mackey, &self.nb, &self.na, &r, &[self.pres[1], self.pres[2], self.pres[3]], &self.b, &self.a);
                let mut v = vec![PAIRING_DHKEY_CHECK];
                v.extend_from_slice(&eb);
                out.push(v);
                self.ltk = Some(ltk);
            },
            _ => {},
        }
        out
    }

    /// After encryption: the identity keys.
    fn keys(&self) -> Vec<Vec<u8>> {
        let mut i = vec![IDENTITY_INFORMATION];
        i.extend_from_slice(&self.irk);
        let mut a = vec![IDENTITY_ADDRESS_INFORMATION, 1];
        a.extend_from_slice(&self.id);
        vec![i, a]
    }
}

fn x(pk: &[u8; 64]) -> [u8; 32] {
    pk[..32].try_into().unwrap()
}

const LOCAL: (u8, BdAddr) = (0, [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5]);
const PEER: (u8, BdAddr) = (1, [0x01, 0x02, 0x03, 0x04, 0x05, 0x76]);

fn addr7(a: (u8, BdAddr)) -> [u8; 7] {
    let mut v = [0u8; 7];
    v[..6].copy_from_slice(&a.1);
    v[6] = a.0;
    v
}

/// Run a pairing to its end: (events seen, the peer).
fn run(io: u8, auth: u8, confirm: Option<bool>, tweak: impl Fn(&mut Peer)) -> (Vec<smp::Event>, Peer) {
    let mut peer = Peer::new(io, auth, addr7(LOCAL), addr7(PEER));
    tweak(&mut peer);
    let (mut p, mut ev) = Pairing::start(LOCAL, PEER, &[0x77; 32]).unwrap();
    let mut seen = Vec::new();
    for _ in 0..200 {
        let Some(e) = (if ev.is_empty() { None } else { Some(ev.remove(0)) }) else { break };
        match &e {
            smp::Event::Send(pdu) => {
                for r in peer.input(pdu) {
                    ev.extend(p.input(&r));
                }
            },
            smp::Event::ShowPasskey(k) => peer.passkey = *k,
            smp::Event::Compare(_) => {
                if let Some(y) = confirm {
                    ev.extend(p.user_confirm(y));
                }
            },
            smp::Event::Encrypt(_) => {
                ev.extend(p.encrypted());
                for k in peer.keys() {
                    ev.extend(p.input(&k));
                }
            },
            _ => {},
        }
        seen.push(e);
    }
    (seen, peer)
}

fn done(ev: &[smp::Event]) -> Option<(bool, [u8; 16], Option<[u8; 16]>, Option<(u8, BdAddr)>)> {
    ev.iter().find_map(|e| if let smp::Event::Done { ltk, authenticated, irk, identity } = e { Some((*authenticated, *ltk, *irk, *identity)) } else { None })
}

#[test]
fn sc_just_works() {
    let (ev, peer) = run(IO_NO_IO, AUTH_BONDING | AUTH_SC, None, |_| {});
    assert!(ev.contains(&smp::Event::Method(smp::Method::JustWorks)));
    let (auth, ltk, irk, id) = done(&ev).expect("paired");
    assert!(!auth);
    assert_eq!(Some(ltk), peer.ltk, "both sides derived the same LTK");
    assert_eq!(irk, Some([0x5C; 16]));
    assert_eq!(id, Some((1, peer.id)));
}

#[test]
fn sc_numeric_comparison() {
    let (ev, peer) = run(IO_DISPLAY_YES_NO, AUTH_BONDING | AUTH_MITM | AUTH_SC, Some(true), |_| {});
    let shown = ev.iter().find_map(|e| if let smp::Event::Compare(v) = e { Some(*v) } else { None }).unwrap();
    assert!(shown < 1_000_000);
    let (auth, ltk, ..) = done(&ev).unwrap();
    assert!(auth);
    assert_eq!(Some(ltk), peer.ltk);
    // "no" ends it
    let (ev, _) = run(IO_DISPLAY_YES_NO, AUTH_BONDING | AUTH_MITM | AUTH_SC, Some(false), |_| {});
    assert!(done(&ev).is_none());
    assert!(ev.iter().any(|e| matches!(e, smp::Event::Failed { .. })));
}

#[test]
fn sc_passkey_entry() {
    let (ev, peer) = run(IO_KEYBOARD_ONLY, AUTH_BONDING | AUTH_MITM | AUTH_SC, None, |_| {});
    assert!(ev.iter().any(|e| matches!(e, smp::Event::ShowPasskey(_))));
    let (auth, ltk, ..) = done(&ev).expect("20 rounds then the DHKey check");
    assert!(auth);
    assert_eq!(Some(ltk), peer.ltk);
    assert_eq!(peer.round, 20);
}

#[test]
fn sc_refusals() {
    // legacy pairing (no SC bit)
    let (ev, _) = run(IO_NO_IO, AUTH_BONDING, None, |_| {});
    assert!(ev.iter().any(|e| matches!(e, smp::Event::Failed { insecure: true, .. })));
    assert!(done(&ev).is_none());
    // a 7-byte key
    let (ev, _) = run(IO_NO_IO, AUTH_BONDING | AUTH_SC, None, |p| p.key = 7);
    assert!(ev.iter().any(|e| matches!(e, smp::Event::Failed { insecure: false, .. })));
    // the debug key
    let (ev, _) = run(IO_NO_IO, AUTH_BONDING | AUTH_SC, None, |p| p.use_debug_key = true);
    assert!(done(&ev).is_none());
    // a confirm that does not match (an attacker who cannot do the math)
    let mut peer = Peer::new(IO_NO_IO, AUTH_BONDING | AUTH_SC, addr7(LOCAL), addr7(PEER));
    let (mut p, ev) = Pairing::start(LOCAL, PEER, &[0x77; 32]).unwrap();
    let smp::Event::Send(req) = &ev[0] else { panic!() };
    for r in peer.input(req) {
        let ev2 = p.input(&r);
        for e in ev2 {
            if let smp::Event::Send(pk) = e {
                if pk[0] == PAIRING_PUBLIC_KEY {
                    let rs = peer.input(&pk);
                    p.input(&rs[0]);
                    let mut c = rs[1].clone();
                    c[3] ^= 0xFF;
                    p.input(&c);
                    let mut r = vec![PAIRING_RANDOM];
                    r.extend_from_slice(&peer.nb);
                    let out = p.input(&r);
                    assert!(out.iter().any(|e| matches!(e, smp::Event::Failed { .. })), "{out:?}");
                }
            }
        }
    }
}

// ── GATT against a simulated server ─────────────────────────────────────────

/// A gamepad's GATT database (DIS PnP Xbox, HID with a report map, one input
/// report id 1 with a CCCD, one output report id 3, protocol mode), MTU 23 so
/// the map takes Read Blobs.
struct Server {
    attrs: Vec<(u16, u16, Vec<u8>)>,
    groups: Vec<(u16, u16, u16)>,
    pub cccd_written: Vec<u16>,
}

const PAD_MAP: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x04, 0x81, 0x02, 0x05, 0x09, 0x19, 0x01, 0x29, 0x0F, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0F, 0x81, 0x02, 0x75, 0x01, 0x95, 0x01, 0x81, 0x03, 0x85, 0x03, 0x06, 0x0F, 0x00, 0x09, 0x97, 0x75, 0x08, 0x95, 0x08, 0x91, 0x02, 0xC0,
];

fn chr(decl: u16, props: u8, value: u16, uuid: u16) -> (u16, u16, Vec<u8>) {
    let mut v = vec![props];
    v.extend_from_slice(&value.to_le_bytes());
    v.extend_from_slice(&uuid.to_le_bytes());
    (decl, gatt::UUID_CHARACTERISTIC, v)
}

impl Server {
    fn gamepad(map: &[u8], vid: u16) -> Server {
        let mut pnp = vec![0x02];
        pnp.extend_from_slice(&vid.to_le_bytes());
        pnp.extend_from_slice(&0x0B13u16.to_le_bytes());
        pnp.extend_from_slice(&0x0100u16.to_le_bytes());
        let attrs = vec![
            (0x0001, gatt::UUID_PRIMARY, 0x180Au16.to_le_bytes().to_vec()),
            chr(0x0002, 0x02, 0x0003, gatt::CHR_PNP_ID),
            (0x0003, gatt::CHR_PNP_ID, pnp),
            (0x0010, gatt::UUID_PRIMARY, 0x1812u16.to_le_bytes().to_vec()),
            chr(0x0011, 0x02, 0x0012, gatt::CHR_REPORT_MAP),
            (0x0012, gatt::CHR_REPORT_MAP, map.to_vec()),
            chr(0x0013, 0x12, 0x0014, gatt::CHR_REPORT),
            (0x0014, gatt::CHR_REPORT, vec![0; 6]),
            (0x0015, gatt::UUID_CCCD, vec![0, 0]),
            (0x0016, gatt::UUID_REPORT_REF, vec![1, 1]),
            chr(0x0017, 0x0C, 0x0018, gatt::CHR_REPORT),
            (0x0018, gatt::CHR_REPORT, vec![0; 8]),
            (0x0019, gatt::UUID_REPORT_REF, vec![3, 2]),
            chr(0x001A, 0x06, 0x001B, gatt::CHR_PROTOCOL_MODE),
            (0x001B, gatt::CHR_PROTOCOL_MODE, vec![1]),
        ];
        Server { attrs, groups: vec![(0x0001, 0x0003, 0x180A), (0x0010, 0x001B, 0x1812)], cccd_written: Vec::new() }
    }

    fn input(&mut self, p: &[u8]) -> Option<Vec<u8>> {
        let a16 = |o| le16(p, o).unwrap();
        let err = |op: u8, hd: u16| Some(gatt::error_rsp(op, hd, gatt::ERR_NOT_FOUND));
        match p[0] {
            gatt::MTU_REQ => Some(vec![gatt::MTU_RSP, 23, 0]),
            gatt::READ_BY_GROUP_REQ => {
                let s = a16(1);
                let g: Vec<_> = self.groups.iter().filter(|g| g.0 >= s).collect();
                if g.is_empty() {
                    return err(p[0], s);
                }
                let mut v = vec![gatt::READ_BY_GROUP_RSP, 6];
                for (a, b, u) in g.iter().take(3) {
                    for x in [*a, *b, *u] {
                        v.extend_from_slice(&x.to_le_bytes());
                    }
                }
                Some(v)
            },
            gatt::READ_BY_TYPE_REQ => {
                let (s, e) = (a16(1), a16(3));
                let c: Vec<_> = self.attrs.iter().filter(|a| a.0 >= s && a.0 <= e && a.1 == gatt::UUID_CHARACTERISTIC).collect();
                if c.is_empty() {
                    return err(p[0], s);
                }
                let mut v = vec![gatt::READ_BY_TYPE_RSP, 7];
                for a in c.iter().take(2) {
                    v.extend_from_slice(&a.0.to_le_bytes());
                    v.extend_from_slice(&a.2);
                }
                Some(v)
            },
            gatt::FIND_INFO_REQ => {
                let (s, e) = (a16(1), a16(3));
                let d: Vec<_> = self.attrs.iter().filter(|a| a.0 >= s && a.0 <= e).collect();
                if d.is_empty() {
                    return err(p[0], s);
                }
                let mut v = vec![gatt::FIND_INFO_RSP, 1];
                for a in d.iter().take(4) {
                    v.extend_from_slice(&a.0.to_le_bytes());
                    v.extend_from_slice(&a.1.to_le_bytes());
                }
                Some(v)
            },
            gatt::READ_REQ | gatt::READ_BLOB_REQ => {
                let hd = a16(1);
                let off = if p[0] == gatt::READ_BLOB_REQ { a16(3) as usize } else { 0 };
                let a = self.attrs.iter().find(|a| a.0 == hd)?;
                if off > a.2.len() {
                    return Some(gatt::error_rsp(p[0], hd, 0x07));
                }
                let mut v = vec![p[0] + 1];
                v.extend_from_slice(&a.2[off..(off + 22).min(a.2.len())]);
                Some(v)
            },
            gatt::WRITE_REQ => {
                self.cccd_written.push(a16(1));
                Some(vec![gatt::WRITE_RSP])
            },
            _ => None,
        }
    }
}

#[test]
fn gatt_discovers_a_hogp_gamepad() {
    let mut srv = Server::gamepad(PAD_MAP, 0x045E);
    let mut c = gatt::Client::new();
    let mut ev = c.start();
    let mut discovered = false;
    for _ in 0..200 {
        let Some(e) = (if ev.is_empty() { None } else { Some(ev.remove(0)) }) else { break };
        match e {
            gatt::Event::Send(p) => {
                if let Some(r) = srv.input(&p) {
                    ev.extend(c.input(&r));
                }
            },
            gatt::Event::Discovered => {
                discovered = true;
                let ins: Vec<u16> = c.reports.iter().filter(|r| r.kind == 1).map(|r| r.value).collect();
                ev.extend(c.subscribe(&ins));
            },
            gatt::Event::Failed(w) => panic!("{w}"),
            _ => {},
        }
    }
    assert!(discovered);
    assert_eq!(c.map, PAD_MAP, "the whole map, read in 22-byte blobs");
    assert_eq!(c.pnp, Some((2, 0x045E, 0x0B13, 0x0100)));
    assert_eq!(c.reports, vec![gatt::Report { value: 0x14, cccd: 0x15, id: 1, kind: 1 }, gatt::Report { value: 0x18, cccd: 0, id: 3, kind: 2 }]);
    assert_eq!(c.protocol_mode, Some(0x1B));
    assert_eq!(srv.cccd_written, vec![0x15]);
    assert_eq!(gatt::hid_kinds(PAD_MAP), (false, false, true));
    assert!(gatt::uses_report_ids(PAD_MAP));
    // a notification
    assert_eq!(c.input(&[gatt::NOTIFY, 0x14, 0, 1, 2, 3]), vec![gatt::Event::Value(0x14, vec![1, 2, 3])]);
    // the peer reading us: nothing to read
    assert!(matches!(&c.input(&[gatt::READ_REQ, 3, 0])[0], gatt::Event::Send(v) if v[0] == gatt::ERROR_RSP));
}

#[test]
fn xbox_ble_buttons_remap() {
    // A (bit 0), X (bit 3), Menu (bit 11), RS (bit 14) at bit offset 8
    let mut r = vec![0u8, 0b0000_1001, 0b0100_1000];
    gatt::xbox_ble_remap(&mut r, 8);
    // canonical: A 0, X 2, Start 7, RS 9
    assert_eq!(r, vec![0, 0b1000_0101, 0b0000_0010]);
}

// ── the host: LE gamepad end to end ─────────────────────────────────────────

const PAD_RPA: BdAddr = [0x10, 0x20, 0x30, 0x40, 0x50, 0x62];

struct LeSim {
    h: Host,
    out: Vec<Output>,
    answered: usize,
    peer: Peer,
    srv: Server,
    now: u64,
}

fn cs(o: u16) -> Vec<u8> {
    let mut v = vec![0x0F, 4, 0, 1];
    v.extend_from_slice(&o.to_le_bytes());
    v
}

fn cc0(o: u16) -> Vec<u8> {
    let mut v = vec![0x0E, 4, 1];
    v.extend_from_slice(&o.to_le_bytes());
    v.push(0);
    v
}

impl LeSim {
    fn new(store: Store, io: u8, auth: u8, map: &[u8]) -> LeSim {
        let mut h = Host::new(store);
        h.seed(&[0x21; 32]);
        h.start(0, [1; 6]);
        let mut s = LeSim { h, out: Vec::new(), answered: 0, peer: Peer::new(io, auth, [0; 7], [0; 7]), srv: Server::gamepad(map, 0x045E), now: 0 };
        s.pump();
        assert_eq!(s.h.state, State::Ready);
        s
    }

    fn pump(&mut self) {
        for _ in 0..2000 {
            self.out.extend(self.h.take_output());
            let cmds: Vec<Vec<u8>> = self.out.iter().filter_map(|o| if let Output::Cmd(c) = o { Some(c.clone()) } else { None }).collect();
            let acls: Vec<Vec<u8>> = self.out.iter().filter_map(|o| if let Output::Acl(c) = o { Some(c.clone()) } else { None }).collect();
            let mut did = false;
            if let Some(c) = cmds.get(self.answered).cloned() {
                self.answered += 1;
                let o = hci::command_opcode(&c).unwrap();
                let r = match o {
                    op::LE_CREATE_CONNECTION | op::LE_START_ENCRYPTION | op::LE_CONNECTION_UPDATE | op::DISCONNECT => cs(o),
                    op::READ_BD_ADDR => {
                        let mut v = vec![0x0E, 10, 1];
                        v.extend_from_slice(&o.to_le_bytes());
                        v.push(0);
                        v.extend_from_slice(&LOCAL.1);
                        v
                    },
                    _ => cc0(o),
                };
                self.h.on_event(&r);
                if o == op::LE_START_ENCRYPTION {
                    // the controller encrypts (AES-CCM) and the peer then sends its keys
                    self.h.on_event(&[0x08, 4, 0, 0x40, 0, 1]);
                    if self.peer.ltk.is_some() && self.peer.round != 99 {
                        self.peer.round = 99;
                        for k in self.peer.keys() {
                            self.h.on_acl(&hci::acl_packet(0x40, 2, &l2cap::frame(smp::CID_SMP, &k)));
                        }
                    }
                }
                did = true;
            }
            // ACL from the host to the peer (SMP and ATT on fixed channels)
            let n_acl = acls.len();
            if n_acl > self.acl_done() {
                let a = acls[self.acl_done()].clone();
                self.out.push(Output::Notice(String::from("\u{0}acl")));
                let p = hci::parse_acl(&a).unwrap();
                let (cid, body) = (le16(p.data, 2).unwrap(), p.data[4..].to_vec());
                let replies: Vec<(u16, Vec<u8>)> = match cid {
                    smp::CID_SMP => self.peer.input(&body).into_iter().map(|r| (smp::CID_SMP, r)).collect(),
                    gatt::CID_ATT => self.srv.input(&body).into_iter().map(|r| (gatt::CID_ATT, r)).collect(),
                    _ => Vec::new(),
                };
                for (c, r) in replies {
                    self.h.on_acl(&hci::acl_packet(0x40, 2, &l2cap::frame(c, &r)));
                }
                // give the controller its buffers back
                self.h.on_event(&[0x13, 5, 1, 0x40, 0, 1, 0]);
                did = true;
            }
            if !did {
                break;
            }
        }
        self.out.extend(self.h.take_output());
    }

    fn acl_done(&self) -> usize {
        self.out.iter().filter(|o| matches!(o, Output::Notice(s) if s == "\u{0}acl")).count()
    }

    fn adv(&mut self, addr: &BdAddr, evt: u8, appearance: u16, name: &str) {
        let mut data = vec![3, 0x19];
        data.extend_from_slice(&appearance.to_le_bytes());
        data.push(name.len() as u8 + 1);
        data.push(0x09);
        data.extend_from_slice(name.as_bytes());
        let mut p = vec![0x02, 1, evt, 1];
        p.extend_from_slice(addr);
        p.push(data.len() as u8);
        p.extend(data);
        p.push(0xC0);
        let mut e = vec![0x3E, p.len() as u8];
        e.extend(p);
        self.h.on_event(&e);
        self.pump();
    }

    fn connected(&mut self, peer: &BdAddr, peer_type: u8) {
        let mut p = vec![0x01, 0, 0x40, 0, 0, peer_type];
        p.extend_from_slice(peer);
        p.extend_from_slice(&[6, 0, 0, 0, 200, 0, 0]);
        let mut e = vec![0x3E, p.len() as u8];
        e.extend(p);
        self.h.on_event(&e);
        self.pump();
    }

    fn notices(&self) -> Vec<String> {
        self.out.iter().filter_map(|o| match o {
            Output::Notice(s) | Output::DeviceNotice(_, s) if !s.starts_with('\u{0}') => Some(s.clone()),
            _ => None,
        }).collect()
    }
}

fn pair_pad(io: u8, auth: u8, appearance: u16) -> LeSim {
    let mut s = LeSim::new(Store::default(), io, auth, PAD_MAP);
    s.peer.a = addr7(LOCAL);
    s.peer.b = addr7((1, PAD_RPA));
    s.h.scan(5).unwrap();
    s.pump();
    s.adv(&PAD_RPA, 0, appearance, "Aurora");
    assert_eq!(s.h.visible()[0].name.as_deref(), Some("Aurora"));
    s.h.pair(&PAD_RPA).unwrap();
    s.pump();
    assert!(s.out.iter().any(|o| matches!(o, Output::Cmd(c) if hci::command_opcode(c) == Some(op::LE_CREATE_CONNECTION))));
    s.connected(&PAD_RPA, 1);
    s
}

#[test]
fn le_gamepad_pairs_and_reports() {
    let mut s = pair_pad(IO_NO_IO, AUTH_BONDING | AUTH_SC, 0x03C4);
    let id = s.peer.id;
    let b = s.h.store.bond(&id).expect("bonded under its identity address").clone();
    assert!(b.le && b.gamepad && !b.allow_kbd && b.irk == Some([0x5C; 16]));
    assert_eq!(b.key_type, store::KEY_UNAUTH_P256);
    assert_eq!(Some(b.key), s.peer.ltk);
    let attach = s.out.iter().find_map(|o| if let Output::PadAttach { addr, map, vid, pid, .. } = o { Some((*addr, map.clone(), *vid, *pid)) } else { None });
    assert_eq!(attach, Some((id, PAD_MAP.to_vec(), 0x045E, 0x0B13)));
    assert!(s.notices().iter().any(|n| n.contains("gamepad connected")));
    // a report notification -> PadReport with the report id first
    s.h.on_acl(&hci::acl_packet(0x40, 2, &l2cap::frame(gatt::CID_ATT, &[gatt::NOTIFY, 0x14, 0, 0x80, 0x7F, 0x80, 0x80, 1, 0])));
    s.pump();
    assert!(s.out.contains(&Output::PadReport { addr: id, data: vec![1, 0x80, 0x7F, 0x80, 0x80, 1, 0] }));
    assert!(!s.out.iter().any(|o| matches!(o, Output::Input(..))), "a pad never types");
    assert_eq!(s.h.conn_state(&id), Some("connected (gamepad)"));
    // rumble: the Xbox output report 3, from `bt rumble` and from drivers::gamepad's packet
    s.h.pad_rumble(&id, 1.0, 0.5, 200).unwrap();
    s.pump();
    let n = s.out.len();
    assert!(s.h.pad_output(&id, &[0x03, 0x03, 0, 0, 80, 20, 0xFF, 0, 0]));
    s.pump();
    let w = s.out[n..].iter().find_map(|o| if let Output::Acl(a) = o { Some(a.clone()) } else { None }).expect("a GATT write");
    let a = hci::parse_acl(&w).unwrap();
    assert_eq!(&a.data[4..], &[gatt::WRITE_CMD, 0x18, 0x00, 0x03, 0, 0, 80, 20, 0xFF, 0, 0], "Write Command to the output report 3 characteristic, without the id");
    // the link drops: the pad is detached and the background scan comes back for it
    s.h.on_event(&[0x05, 4, 0, 0x40, 0, 0x13]);
    s.pump();
    assert!(s.out.contains(&Output::PadDetach(id)));
}

#[test]
fn le_keyboard_appearance_refuses_just_works() {
    let s = pair_pad(IO_NO_IO, AUTH_BONDING | AUTH_SC, 0x03C1);
    assert!(s.h.store.bonds.is_empty());
    assert!(s.notices().iter().any(|n| n.contains("keyboard must pair")));
}

#[test]
fn le_legacy_only_device_is_refused() {
    let s = pair_pad(IO_NO_IO, AUTH_BONDING, 0x03C4);
    assert!(s.h.store.bonds.is_empty());
    assert!(s.notices().iter().any(|n| n.contains("only supports insecure pairing")));
}

#[test]
fn le_numeric_comparison_through_the_prompt() {
    let mut s = LeSim::new(Store::default(), IO_DISPLAY_YES_NO, AUTH_BONDING | AUTH_MITM | AUTH_SC, PAD_MAP);
    s.peer.a = addr7(LOCAL);
    s.peer.b = addr7((1, PAD_RPA));
    s.h.scan(5).unwrap();
    s.adv(&PAD_RPA, 0, 0x03C4, "Aurora");
    s.h.pair(&PAD_RPA).unwrap();
    s.pump();
    s.connected(&PAD_RPA, 1);
    assert!(matches!(s.h.prompt, Some(Prompt::Confirm { .. })));
    assert!(s.h.confirm(true));
    s.pump();
    assert_eq!(s.h.store.bond(&s.peer.id).unwrap().key_type, store::KEY_AUTH_P256);
}

#[test]
fn le_bonded_pad_comes_back_through_its_rpa_and_blocking_follows_the_identity() {
    let s = pair_pad(IO_NO_IO, AUTH_BONDING | AUTH_SC, 0x03C4);
    let st = s.h.store.clone();
    let id = s.peer.id;
    let irk = st.bond(&id).unwrap().irk.unwrap();
    // a new boot: the pad advertises from a fresh RPA made with its IRK
    let mut s = LeSim::new(st, IO_NO_IO, AUTH_BONDING | AUTH_SC, PAD_MAP);
    let prand = [0x01, 0x02, 0x43];
    let hash = smp::ah(&irk, &prand);
    let rpa: BdAddr = [hash[0], hash[1], hash[2], prand[0], prand[1], prand[2]];
    assert!(s.out.iter().any(|o| matches!(o, Output::Cmd(c) if hci::command_opcode(c) == Some(op::LE_SET_SCAN_ENABLE) && c[3] == 1)), "background scan for the bonded pad");
    s.adv(&rpa, 0, 0x03C4, "");
    let create = s.out.iter().rev().find_map(|o| if let Output::Cmd(c) = o { (hci::command_opcode(c) == Some(op::LE_CREATE_CONNECTION)).then(|| c.clone()) } else { None }).expect("reconnects");
    assert_eq!(&create[9..15], &rpa);
    // reconnect: encrypted with the stored LTK, no pairing
    s.peer.round = 99;
    s.connected(&rpa, 1);
    assert!(s.out.iter().any(|o| matches!(o, Output::Cmd(c) if hci::command_opcode(c) == Some(op::LE_START_ENCRYPTION))));
    assert!(s.out.iter().any(|o| matches!(o, Output::PadAttach { addr, .. } if *addr == id)));
    // block the identity: the IRK stays, later RPAs are blocked too, the keys are gone
    s.h.block(&id).unwrap();
    s.pump();
    assert!(s.h.store.bond(&id).is_none());
    let prand2 = [0x09, 0x08, 0x47];
    let h2 = smp::ah(&irk, &prand2);
    let rpa2: BdAddr = [h2[0], h2[1], h2[2], prand2[0], prand2[1], prand2[2]];
    assert!(s.h.store.is_blocked(&rpa2, None));
    let st2 = Store::parse(&s.h.store.to_text());
    assert!(st2.is_blocked(&rpa2, None), "the IRK is saved with the block entry");
}

#[test]
fn le_unsolicited_pairing_and_connections_are_refused() {
    let mut s = LeSim::new(Store::default(), IO_NO_IO, AUTH_BONDING | AUTH_SC, PAD_MAP);
    // a connection complete nobody asked for
    s.connected(&PAD_RPA, 1);
    assert!(s.out.iter().any(|o| matches!(o, Output::Cmd(c) if hci::command_opcode(c) == Some(op::DISCONNECT))));
    assert!(s.h.conns.is_empty());
}

#[test]
fn le_connection_parameter_requests() {
    let mut s = pair_pad(IO_NO_IO, AUTH_BONDING | AUTH_SC, 0x03C4);
    let n = s.out.len();
    s.h.on_acl(&hci::acl_packet(0x40, 2, &l2cap::frame(5, &[0x12, 7, 8, 0, 6, 0, 9, 0, 0, 0, 0xC8, 0])));
    s.pump();
    assert!(s.out[n..].iter().any(|o| matches!(o, Output::Cmd(c) if hci::command_opcode(c) == Some(op::LE_CONNECTION_UPDATE))));
    let n = s.out.len();
    s.h.on_acl(&hci::acl_packet(0x40, 2, &l2cap::frame(5, &[0x12, 8, 8, 0, 1, 0, 1, 0, 0, 0, 1, 0])));
    s.pump();
    assert!(!s.out[n..].iter().any(|o| matches!(o, Output::Cmd(c) if hci::command_opcode(c) == Some(op::LE_CONNECTION_UPDATE))), "insane parameters refused");
}

// ── classic HID gamepads over SDP ───────────────────────────────────────────

pub(crate) fn sdp_rsp(tid: u16, lists: &[u8]) -> Vec<u8> {
    let mut p = (lists.len() as u16).to_be_bytes().to_vec();
    p.extend_from_slice(lists);
    p.push(0);
    let mut v = vec![0x07];
    v.extend_from_slice(&tid.to_be_bytes());
    v.extend_from_slice(&(p.len() as u16).to_be_bytes());
    v.extend(p);
    v
}

/// AttributeLists: one record with 0x0206 = ((0x22, desc)).
pub(crate) fn hid_lists(desc: &[u8]) -> Vec<u8> {
    let mut inner = vec![0x08, 0x22, 0x25, desc.len() as u8];
    inner.extend_from_slice(desc);
    let mut l1 = vec![0x35, inner.len() as u8];
    l1.extend(inner);
    let mut attr = vec![0x09, 0x02, 0x06, 0x35, l1.len() as u8];
    attr.extend(l1);
    let mut rec = vec![0x35, attr.len() as u8];
    rec.extend(attr);
    let mut lists = vec![0x35, rec.len() as u8];
    lists.extend(rec);
    lists
}

#[test]
fn sdp_reads_descriptor_and_device_id() {
    let mut c = sdp::Client::default();
    let ev = c.start();
    let sdp::Event::Send(req) = &ev[0] else { panic!() };
    assert_eq!(req[0], 0x06);
    let ev = c.input(&sdp_rsp(1, &hid_lists(PAD_MAP)));
    let sdp::Event::Send(req2) = &ev[0] else { panic!("{ev:?}") };
    assert_eq!(&req2[5..10], &[0x35, 3, 0x19, 0x12, 0x00]);
    // Device ID: vendor 057E, product 2009 (a Switch Pro personality)
    let lists = [0x35, 14, 0x35, 12, 0x09, 0x02, 0x01, 0x09, 0x05, 0x7E, 0x09, 0x02, 0x02, 0x09, 0x20, 0x09];
    let ev = c.input(&sdp_rsp(2, &lists));
    assert_eq!(ev, vec![sdp::Event::Done { desc: PAD_MAP.to_vec(), vid: 0x057E, pid: 0x2009 }]);
}

#[test]
fn sdp_continuation_is_followed() {
    let mut c = sdp::Client::default();
    c.start();
    let lists = hid_lists(PAD_MAP);
    let (a, b) = lists.split_at(30);
    let mut r1 = sdp_rsp(1, a);
    let n = r1.len();
    r1[n - 1] = 2;
    r1.extend_from_slice(&[0xAB, 0xCD]);
    let plen = (r1.len() - 5) as u16;
    r1[3..5].copy_from_slice(&plen.to_be_bytes());
    let ev = c.input(&r1);
    let sdp::Event::Send(again) = &ev[0] else { panic!() };
    assert!(again.ends_with(&[2, 0xAB, 0xCD]));
    let ev = c.input(&sdp_rsp(2, b));
    assert!(matches!(&ev[0], sdp::Event::Send(_)), "on to the Device ID");
}

// ── fuzzing ─────────────────────────────────────────────────────────────────

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let n = (self.next() as usize) % (max + 1);
        (0..n).map(|_| self.next() as u8).collect()
    }
}

#[test]
fn le_parsers_never_panic() {
    let mut r = Rng(0xDEAD_BEEF_1234_5678);
    for i in 0..20_000 {
        let b = r.bytes(300);
        let _ = sdp::parse_de(&b, 0);
        let _ = gatt::app_usages(&b);
        let _ = gatt::uses_report_ids(&b);
        let mut b2 = b.clone();
        gatt::xbox_ble_remap(&mut b2, (r.next() % 64) as usize);
        let _ = hci::parse_event(&b);
        if i % 4 == 0 {
            let mut c = gatt::Client::new();
            let _ = c.start();
            for _ in 0..20 {
                let mut p = r.bytes(40);
                if !p.is_empty() && r.next() % 2 == 0 {
                    p[0] = [0x01, 0x03, 0x05, 0x09, 0x0B, 0x0D, 0x11, 0x13, 0x1B, 0x1D][(r.next() % 10) as usize];
                }
                let _ = c.input(&p);
            }
            let (mut s, _) = Pairing::start(LOCAL, PEER, &[(i % 251) as u8; 32]).unwrap();
            for _ in 0..10 {
                let mut p = r.bytes(70);
                if !p.is_empty() {
                    p[0] = (r.next() % 16) as u8;
                }
                let _ = s.input(&p);
            }
            let mut sc = sdp::Client::default();
            sc.start();
            let mut p = r.bytes(120);
            if p.len() > 3 {
                p[0] = 7;
                p[1] = 0;
                p[2] = 1;
            }
            let _ = sc.input(&p);
        }
    }
}

#[test]
fn host_le_survives_random_traffic() {
    let mut st = Store::default();
    st.bonds.push(store::Bond { addr: [1, 2, 3, 4, 5, 0xC6], key: [1; 16], key_type: 7, le: true, addr_type: 1, irk: Some([3; 16]), gamepad: true, ..Default::default() });
    let mut s = LeSim::new(st, IO_NO_IO, AUTH_BONDING | AUTH_SC, PAD_MAP);
    let mut r = Rng(99);
    let mut inputs = 0;
    for i in 0..100_000u64 {
        match r.next() % 3 {
            0 => {
                let mut p = vec![0x3E];
                let body = r.bytes(60);
                p.push(body.len() as u8);
                p.extend(body);
                if p.len() > 2 {
                    p[2] = [1, 2, 3, 6][(r.next() % 4) as usize];
                }
                s.h.on_event(&p);
            },
            1 => {
                let cid = [4u16, 5, 6][(r.next() % 3) as usize];
                s.h.on_acl(&hci::acl_packet(0x40, 2, &l2cap::frame(cid, &r.bytes(70))));
            },
            _ => s.h.tick(i * 7),
        }
        for o in s.h.take_output() {
            if matches!(o, Output::Input(..) | Output::PadReport { .. }) {
                inputs += 1;
            }
        }
        assert!(s.h.conns.len() <= crate::host::MAX_CONNS);
    }
    assert_eq!(inputs, 0, "random traffic never produces input");
}
