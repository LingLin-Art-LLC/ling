//! A software U2F key behind a fake HID transport, so the whole conversation
//! (CTAPHID framing, APDUs, touches, signatures) runs on the host.

use super::*;
use alloc::collections::VecDeque;
use alloc::vec;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::SigningKey;

struct SoftKey {
    /// Packets the key will answer with.
    out: VecDeque<[u8; PKT]>,
    rx: Reassembler,
    cid: u32,
    /// The user's finger is on the key.
    touch: bool,
    keys: Vec<SigningKey>,
    counter: u32,
    /// Only take ISO short APDUs (an old key).
    short_only: bool,
    /// Send a KEEPALIVE before each answer.
    keepalive: bool,
    /// Make high-S signatures (ECDSA allows both).
    high_s: bool,
}

impl SoftKey {
    fn new() -> Self {
        SoftKey {
            out: VecDeque::new(),
            rx: Reassembler::new(BROADCAST_CID),
            cid: BROADCAST_CID,
            touch: false,
            keys: Vec::new(),
            counter: 0,
            short_only: false,
            keepalive: false,
            high_s: false,
        }
    }

    fn reply(&mut self, cid: u32, cmd: u8, data: &[u8]) {
        if self.keepalive {
            self.out.extend(frames(cid, CMD_KEEPALIVE, &[1]));
        }
        self.out.extend(frames(cid, cmd, data));
    }

    fn handle(&mut self, cmd: u8, data: Vec<u8>) {
        let cid = self.cid;
        match cmd {
            CMD_INIT => {
                let mut r = data[..8].to_vec();
                r.extend_from_slice(&0x0102_0304u32.to_be_bytes());
                r.extend_from_slice(&[2, 5, 4, 7, 1]);
                self.reply(BROADCAST_CID, CMD_INIT, &r);
                self.cid = 0x0102_0304;
            },
            CMD_MSG => {
                let r = self.apdu(&data);
                self.reply(cid, CMD_MSG, &r);
            },
            CMD_WINK => self.reply(cid, CMD_WINK, &[]),
            _ => self.reply(cid, CMD_ERROR, &[1]),
        }
    }

    fn apdu(&mut self, a: &[u8]) -> Vec<u8> {
        let ins = a[1];
        let p1 = a[2];
        let body = if a[4] == 0 && a.len() >= 7 {
            if self.short_only {
                return vec![0x67, 0x00];
            }
            let lc = ((a[5] as usize) << 8) | a[6] as usize;
            &a[7..7 + lc]
        } else {
            let lc = a[4] as usize;
            &a[5..5 + lc]
        };
        let sw = |mut v: Vec<u8>, s: u16| {
            v.extend_from_slice(&s.to_be_bytes());
            v
        };
        match ins {
            INS_REGISTER => {
                if !self.touch {
                    return sw(vec![], 0x6985);
                }
                self.touch = false;
                let mut seed = [0u8; 32];
                seed[0] = 1 + self.keys.len() as u8;
                seed[31] = 0x5A;
                let sk = SigningKey::from_bytes(&seed.into()).unwrap();
                let pk = sk.verifying_key().to_encoded_point(false);
                let kh = vec![0xC0, self.keys.len() as u8, 0xFE, 0xED];
                self.keys.push(sk);
                let mut r = vec![0x05];
                r.extend_from_slice(pk.as_bytes());
                r.push(kh.len() as u8);
                r.extend_from_slice(&kh);
                r.extend_from_slice(&[0x30, 0x03, 0x02, 0x01, 0x00]); // a stand-in certificate
                r.extend_from_slice(&[0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01]);
                sw(r, 0x9000)
            },
            INS_AUTHENTICATE => {
                let challenge: [u8; 32] = body[..32].try_into().unwrap();
                let app: [u8; 32] = body[32..64].try_into().unwrap();
                let kl = body[64] as usize;
                let kh = &body[65..65 + kl];
                if kh.len() != 4 || kh[0] != 0xC0 || kh[1] as usize >= self.keys.len() {
                    return sw(vec![], 0x6A80);
                }
                if p1 == AUTH_CHECK_ONLY || !self.touch {
                    return sw(vec![], 0x6985);
                }
                self.touch = false;
                self.counter += 1;
                let sk = &self.keys[kh[1] as usize];
                let m = signed_bytes(&app, 1, self.counter, &challenge);
                let mut sig: Signature = sk.sign(&m);
                if self.high_s {
                    if let Some(n) = sig.normalize_s() {
                        sig = n;
                    }
                    // flip to the high-S twin: s' = n - s
                    let (r, s) = sig.split_scalars();
                    sig = Signature::from_scalars(r, -*s).unwrap();
                }
                let mut out = vec![1];
                out.extend_from_slice(&self.counter.to_be_bytes());
                out.extend_from_slice(sig.to_der().as_bytes());
                sw(out, 0x9000)
            },
            _ => sw(vec![], 0x6D00),
        }
    }
}

/// The transport the device sees: the soft key behind a shared cell, so a test
/// can touch the key while a `Device` owns the transport.
struct Wire(alloc::rc::Rc<core::cell::RefCell<SoftKey>>);

impl Transport for Wire {
    fn send(&mut self, pkt: &[u8; PKT]) -> Result<(), Error> {
        let mut k = self.0.borrow_mut();
        let cid = u32::from_be_bytes([pkt[0], pkt[1], pkt[2], pkt[3]]);
        if k.rx.cid != cid {
            k.rx = Reassembler::new(cid);
        }
        if let Some(cmd) = k.rx.push(pkt)? {
            let data = core::mem::take(&mut k.rx.data);
            k.handle(cmd, data);
        }
        Ok(())
    }
    fn recv(&mut self, pkt: &mut [u8; PKT], _timeout_ms: u32) -> Result<(), Error> {
        match self.0.borrow_mut().out.pop_front() {
            Some(p) => {
                *pkt = p;
                Ok(())
            },
            None => Err(Error::Timeout),
        }
    }
}

fn setup() -> (alloc::rc::Rc<core::cell::RefCell<SoftKey>>, Device<Wire>) {
    let k = alloc::rc::Rc::new(core::cell::RefCell::new(SoftKey::new()));
    let d = Device::open(Wire(k.clone()), [9, 8, 7, 6, 5, 4, 3, 2]).unwrap();
    (k, d)
}

const APP: &str = "LingOS sign-in";

#[test]
fn framing_round_trips_long_messages() {
    let payload: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
    let fs = frames(7, CMD_MSG, &payload);
    assert_eq!(fs.len(), 1 + (300 - 57 + 58) / 59);
    let mut r = Reassembler::new(7);
    let mut done = None;
    for f in &fs {
        done = r.push(f).unwrap();
    }
    assert_eq!(done, Some(CMD_MSG));
    assert_eq!(r.data, payload);
}

#[test]
fn reassembler_ignores_other_channels_and_rejects_bad_sequence() {
    let fs = frames(7, CMD_MSG, &[1u8; 100]);
    let mut r = Reassembler::new(8);
    assert_eq!(r.push(&fs[0]).unwrap(), None);
    let mut r = Reassembler::new(7);
    r.push(&fs[0]).unwrap();
    let mut bad = fs[1];
    bad[4] = 5;
    assert_eq!(r.push(&bad), Err(Error::Protocol));
}

#[test]
fn init_gets_a_channel() {
    let (_k, d) = setup();
    assert_eq!(d.cid, 0x0102_0304);
    assert_eq!(d.version, [5, 4, 7]);
    assert_eq!(d.caps, 1);
}

#[test]
fn register_waits_for_touch_then_authenticates() {
    let (k, mut d) = setup();
    let app = app_id(APP);
    let ch = [3u8; 32];
    assert_eq!(register(&mut d, &ch, &app), Err(Error::NeedTouch));
    k.borrow_mut().touch = true;
    let reg = register(&mut d, &ch, &app).unwrap();
    assert_eq!(reg.public_key[0], 4);

    assert_eq!(owns(&mut d, &app, &reg.key_handle), Ok(true));
    let ch2 = [0x42u8; 32];
    assert_eq!(authenticate(&mut d, &ch2, &app, &reg.key_handle), Err(Error::NeedTouch));
    k.borrow_mut().touch = true;
    let a = authenticate(&mut d, &ch2, &app, &reg.key_handle).unwrap();
    assert!(verify(&reg.public_key, &app, &ch2, &a));
    assert_eq!(a.counter, 1);

    // a replayed answer does not fit a new challenge
    assert!(!verify(&reg.public_key, &app, &[0x43u8; 32], &a));
    // nor another machine's app id
    assert!(!verify(&reg.public_key, &app_id("elsewhere"), &ch2, &a));
    // nor a doctored counter
    let mut b = a.clone();
    b.counter += 1;
    assert!(!verify(&reg.public_key, &app, &ch2, &b));
    // and a signature without the touch flag is refused
    let mut c = a.clone();
    c.flags = 0;
    assert!(!verify(&reg.public_key, &app, &ch2, &c));
}

#[test]
fn another_keys_handle_is_wrong_key() {
    let (k, mut d) = setup();
    let app = app_id(APP);
    assert_eq!(owns(&mut d, &app, &[1, 2, 3]), Ok(false));
    k.borrow_mut().touch = true;
    assert_eq!(authenticate(&mut d, &[0; 32], &app, &[1, 2, 3]), Err(Error::WrongKey));
}

#[test]
fn signature_from_another_key_fails() {
    let (k, mut d) = setup();
    let app = app_id(APP);
    k.borrow_mut().touch = true;
    let r1 = register(&mut d, &[1; 32], &app).unwrap();
    k.borrow_mut().touch = true;
    let r2 = register(&mut d, &[2; 32], &app).unwrap();
    k.borrow_mut().touch = true;
    let a = authenticate(&mut d, &[7; 32], &app, &r2.key_handle).unwrap();
    assert!(verify(&r2.public_key, &app, &[7; 32], &a));
    assert!(!verify(&r1.public_key, &app, &[7; 32], &a));
}

#[test]
fn short_apdu_keys_and_keepalives_work() {
    let (k, mut d) = setup();
    {
        let mut s = k.borrow_mut();
        s.short_only = true;
        s.keepalive = true;
        s.touch = true;
    }
    let app = app_id(APP);
    let reg = register(&mut d, &[5; 32], &app).unwrap();
    k.borrow_mut().touch = true;
    let a = authenticate(&mut d, &[6; 32], &app, &reg.key_handle).unwrap();
    assert!(verify(&reg.public_key, &app, &[6; 32], &a));
}

#[test]
fn high_s_signatures_verify() {
    let (k, mut d) = setup();
    let app = app_id(APP);
    k.borrow_mut().touch = true;
    let reg = register(&mut d, &[5; 32], &app).unwrap();
    {
        let mut s = k.borrow_mut();
        s.high_s = true;
        s.touch = true;
    }
    let a = authenticate(&mut d, &[6; 32], &app, &reg.key_handle).unwrap();
    assert!(verify(&reg.public_key, &app, &[6; 32], &a));
}

#[test]
fn garbage_registration_is_rejected() {
    assert_eq!(parse_registration(&[5, 4, 1, 2]), Err(Error::Protocol));
    let mut r = vec![5u8, 4];
    r.extend_from_slice(&[0x11; 64]); // not a curve point
    r.extend_from_slice(&[1, 9]);
    assert_eq!(parse_registration(&r), Err(Error::Protocol));
}
