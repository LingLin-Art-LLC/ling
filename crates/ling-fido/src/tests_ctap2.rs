//! CTAP2 hmac-secret: known-answer vectors made independently with Python
//! (`cryptography` + `cbor2`, see ctap2_vectors.txt), and a software CTAP2
//! key behind the fake HID transport that runs the whole conversation.

use super::ctap2::*;
use super::*;
use alloc::collections::VecDeque;
use alloc::vec;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::SigningKey;

fn vector(name: &str) -> Vec<u8> {
    let line = include_str!("ctap2_vectors.txt").lines().find(|l| l.split(' ').next() == Some(name)).expect("vector");
    let h = line.split(' ').nth(1).unwrap();
    (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect()
}

fn a32(v: &[u8]) -> [u8; 32] {
    v.try_into().unwrap()
}

fn sha(s: &[u8]) -> [u8; 32] {
    Sha256::digest(s).into()
}

const RP: &str = "lingos.wallet";

#[test]
fn ecdh_and_shared_secrets_match_python() {
    let a = sha(b"platform");
    let b = sha(b"authenticator");
    assert_eq!(public_xy(&a).unwrap(), (a32(&vector("ax")), a32(&vector("ay"))));
    assert_eq!(public_xy(&b).unwrap(), (a32(&vector("bx")), a32(&vector("by"))));
    assert_eq!(ecdh_x(&a, &a32(&vector("bx")), &a32(&vector("by"))).unwrap().to_vec(), vector("z"));
    let s1 = Shared::new(1, &a, &a32(&vector("bx")), &a32(&vector("by"))).unwrap();
    assert_eq!(s1.key_bytes(), &vector("v1")[..]);
    let s2 = Shared::new(2, &a, &a32(&vector("bx")), &a32(&vector("by"))).unwrap();
    assert_eq!(s2.key_bytes(), &vector("v2")[..]);
}

#[test]
fn salt_encryption_and_output_decryption_match_python() {
    let a = sha(b"platform");
    let (bx, by) = (a32(&vector("bx")), a32(&vector("by")));
    let salt = sha(b"salt");
    let iv: [u8; 16] = core::array::from_fn(|i| i as u8);
    let s1 = Shared::new(1, &a, &bx, &by).unwrap();
    let e1 = s1.encrypt(&iv, &salt);
    assert_eq!(e1, vector("enc1"));
    assert_eq!(s1.authenticate(&e1), vector("auth1"));
    assert_eq!(s1.decrypt(&vector("oenc1")).unwrap(), vector("out"));
    let s2 = Shared::new(2, &a, &bx, &by).unwrap();
    let e2 = s2.encrypt(&iv, &salt);
    assert_eq!(e2, vector("enc2"));
    assert_eq!(s2.authenticate(&e2), vector("auth2"));
    assert_eq!(s2.decrypt(&vector("oenc2")).unwrap(), vector("out"));
}

#[test]
fn request_encoding_matches_python_canonical_cbor() {
    let a = sha(b"platform");
    let s2 = Shared::new(2, &a, &a32(&vector("bx")), &a32(&vector("by"))).unwrap();
    let iv: [u8; 16] = core::array::from_fn(|i| i as u8);
    let cid = [0xC1u8, 0xED].repeat(8);
    let req = get_assertion_request(RP, &sha(b"clientdata"), &[cid], hmac_secret_input(&s2, &sha(b"salt"), &iv));
    let mut bytes = vec![GET_ASSERTION];
    req.encode(&mut bytes);
    assert_eq!(bytes, vector("ga_v2"));
    let mc = make_credential_request(RP, &[0u8; 8], "ling", &sha(b"clientdata"));
    let mut bytes = vec![MAKE_CREDENTIAL];
    mc.encode(&mut bytes);
    assert_eq!(bytes, vector("mc"));
}

#[test]
fn cbor_decodes_what_it_encodes_and_rejects_junk() {
    let v = Cbor::Map(vec![
        (Cbor::int(1), Cbor::Array(vec![Cbor::text("FIDO_2_0"), Cbor::int(-25), Cbor::Uint(70000), Cbor::Uint(1 << 40)])),
        (Cbor::text("x"), Cbor::Bool(true)),
        (Cbor::int(-3), Cbor::bytes(&[7u8; 300])),
    ]);
    let enc = v.to_vec();
    assert_eq!(decode(&enc).unwrap(), (v, enc.len()));
    assert!(decode(&[0x5A, 0xFF, 0xFF, 0xFF, 0xFF]).is_err()); // a byte string longer than the input
    assert!(decode(&[0x9F]).is_err()); // indefinite length
    assert!(decode(&[0xB9, 0xFF, 0xFF]).is_err()); // a map bigger than the input
    let mut deep = vec![0x81u8; 40];
    deep.push(0);
    assert!(decode(&deep).is_err());
}

// ── a software CTAP2 key ────────────────────────────────────────────────────

struct Cred {
    id: Vec<u8>,
    sk: SigningKey,
    cred_random: [u8; 32],
    rp: String,
}

struct Soft2 {
    out: VecDeque<[u8; PKT]>,
    rx: Reassembler,
    cid: u32,
    touch: bool,
    creds: Vec<Cred>,
    counter: u32,
    protocols: Vec<i64>,
    hmac: bool,
    /// Ask for a PIN on makeCredential (like a key with alwaysUv).
    want_pin: bool,
    /// Send a KEEPALIVE (status 2, "touch me") before each answer.
    keepalive: bool,
}

impl Soft2 {
    fn new() -> Self {
        Soft2 { out: VecDeque::new(), rx: Reassembler::new(BROADCAST_CID), cid: BROADCAST_CID, touch: false, creds: Vec::new(), counter: 0, protocols: vec![2, 1], hmac: true, want_pin: false, keepalive: true }
    }

    fn reply(&mut self, cid: u32, cmd: u8, data: &[u8]) {
        if self.keepalive && cmd == CMD_CBOR {
            self.out.extend(frames(cid, CMD_KEEPALIVE, &[2]));
        }
        self.out.extend(frames(cid, cmd, data));
    }

    fn handle(&mut self, cmd: u8, data: Vec<u8>) {
        let cid = self.cid;
        match cmd {
            CMD_INIT => {
                let mut r = data[..8].to_vec();
                r.extend_from_slice(&0x0A0B_0C0Du32.to_be_bytes());
                r.extend_from_slice(&[2, 5, 4, 7, 0x05]); // wink + CBOR
                self.reply(BROADCAST_CID, CMD_INIT, &r);
                self.cid = 0x0A0B_0C0D;
            },
            CMD_CBOR => {
                let r = self.cbor(&data);
                self.reply(cid, CMD_CBOR, &r);
            },
            CMD_CANCEL => {},
            _ => self.reply(cid, CMD_ERROR, &[1]),
        }
    }

    fn auth_key(&self) -> [u8; 32] {
        sha(b"authenticator")
    }

    fn cbor(&mut self, d: &[u8]) -> Vec<u8> {
        let ok = |c: Cbor| {
            let mut v = vec![0u8];
            c.encode(&mut v);
            v
        };
        let req = if d.len() > 1 { decode(&d[1..]).unwrap().0 } else { Cbor::Map(vec![]) };
        match d[0] {
            GET_INFO => {
                let mut ext = vec![];
                if self.hmac {
                    ext.push(Cbor::text("hmac-secret"));
                }
                ok(Cbor::Map(vec![
                    (Cbor::int(1), Cbor::Array(vec![Cbor::text("U2F_V2"), Cbor::text("FIDO_2_0")])),
                    (Cbor::int(2), Cbor::Array(ext)),
                    (Cbor::int(3), Cbor::bytes(&[0u8; 16])),
                    (Cbor::int(6), Cbor::Array(self.protocols.iter().map(|p| Cbor::int(*p)).collect())),
                ]))
            },
            CLIENT_PIN => {
                let (x, y) = public_xy(&self.auth_key()).unwrap();
                ok(Cbor::Map(vec![(Cbor::int(1), cose_p256(&x, &y, ALG_ECDH_ES_HKDF_256))]))
            },
            MAKE_CREDENTIAL => {
                if self.want_pin {
                    return vec![ERR_PUAT_REQUIRED];
                }
                if !self.touch {
                    return vec![ERR_USER_ACTION_TIMEOUT];
                }
                self.touch = false;
                let rp = req.get_i(2).unwrap().get_t("id").unwrap().as_text().unwrap().to_string();
                let n = self.creds.len() as u8;
                let mut seed = [0u8; 32];
                seed[0] = 0x40 + n;
                seed[31] = 3;
                let sk = SigningKey::from_bytes(&seed.into()).unwrap();
                let id = vec![0xCD, n, 0x11, 0x22];
                let pt = sk.verifying_key().to_encoded_point(false);
                let cose = cose_p256(pt.x().unwrap().as_slice().try_into().unwrap(), pt.y().unwrap().as_slice().try_into().unwrap(), ALG_ES256);
                let mut ad = sha(rp.as_bytes()).to_vec();
                ad.push(FLAG_UP | FLAG_AT | FLAG_ED);
                ad.extend_from_slice(&self.counter.to_be_bytes());
                ad.extend_from_slice(&[0u8; 16]);
                ad.extend_from_slice(&(id.len() as u16).to_be_bytes());
                ad.extend_from_slice(&id);
                cose.encode(&mut ad);
                Cbor::Map(vec![(Cbor::text("hmac-secret"), Cbor::Bool(self.hmac))]).encode(&mut ad);
                self.creds.push(Cred { id, sk, cred_random: sha(&[0x77, n]), rp });
                ok(Cbor::Map(vec![(Cbor::int(1), Cbor::text("none")), (Cbor::int(2), Cbor::Bytes(ad)), (Cbor::int(3), Cbor::Map(vec![]))]))
            },
            GET_ASSERTION => {
                let rp = req.get_i(1).unwrap().as_text().unwrap().to_string();
                let cdh = req.get_i(2).unwrap().as_bytes().unwrap().to_vec();
                let allow: Vec<Vec<u8>> = req.get_i(3).unwrap().as_array().unwrap().iter().map(|c| c.get_t("id").unwrap().as_bytes().unwrap().to_vec()).collect();
                let Some(ci) = self.creds.iter().position(|c| c.rp == rp && allow.contains(&c.id)) else { return vec![ERR_NO_CREDENTIALS] };
                let up = req.get_i(5).and_then(|o| o.get_t("up")).and_then(|v| v.as_bool()) != Some(false);
                if up && !self.touch {
                    return vec![ERR_USER_ACTION_TIMEOUT];
                }
                let mut flags = 0;
                let mut ext = None;
                if up {
                    self.touch = false;
                    flags |= FLAG_UP;
                    if let Some(h) = req.get_i(4).and_then(|e| e.get_t("hmac-secret")) {
                        let (px, py) = cose_xy(h.get_i(1).unwrap()).unwrap();
                        let proto = h.get_i(4).and_then(|v| v.as_int()).unwrap_or(1) as u8;
                        let sh = Shared::new(proto, &self.auth_key(), &px, &py).unwrap();
                        let enc = h.get_i(2).unwrap().as_bytes().unwrap();
                        if sh.authenticate(enc) != h.get_i(3).unwrap().as_bytes().unwrap() {
                            return vec![ERR_PIN_AUTH_INVALID];
                        }
                        let salt = sh.decrypt(enc).unwrap();
                        let mut m = <hmac::Hmac<Sha256> as hmac::Mac>::new_from_slice(&self.creds[ci].cred_random).unwrap();
                        hmac::Mac::update(&mut m, &salt);
                        let o = hmac::Mac::finalize(m).into_bytes();
                        let iv: [u8; 16] = core::array::from_fn(|i| 0x10 + i as u8);
                        ext = Some(Cbor::Map(vec![(Cbor::text("hmac-secret"), Cbor::Bytes(sh.encrypt(&iv, &o)))]));
                        flags |= FLAG_ED;
                    }
                }
                self.counter += 1;
                let mut ad = sha(rp.as_bytes()).to_vec();
                ad.push(flags);
                ad.extend_from_slice(&self.counter.to_be_bytes());
                if let Some(e) = &ext {
                    e.encode(&mut ad);
                }
                let mut m = ad.clone();
                m.extend_from_slice(&cdh);
                let sig: Signature = self.creds[ci].sk.sign(&m);
                let id = self.creds[ci].id.clone();
                ok(Cbor::Map(vec![
                    (Cbor::int(1), Cbor::Map(vec![(Cbor::text("id"), Cbor::Bytes(id)), (Cbor::text("type"), Cbor::text("public-key"))])),
                    (Cbor::int(2), Cbor::Bytes(ad)),
                    (Cbor::int(3), Cbor::Bytes(sig.to_der().as_bytes().to_vec())),
                ]))
            },
            _ => vec![0x01],
        }
    }
}

struct Wire2(alloc::rc::Rc<core::cell::RefCell<Soft2>>);

impl Transport for Wire2 {
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

fn setup2() -> (alloc::rc::Rc<core::cell::RefCell<Soft2>>, Device<Wire2>) {
    let k = alloc::rc::Rc::new(core::cell::RefCell::new(Soft2::new()));
    let d = Device::open(Wire2(k.clone()), [1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    (k, d)
}

fn eph(n: u8) -> [u8; 32] {
    sha(&[b'e', n])
}

#[test]
fn hmac_secret_is_stable_per_credential_and_salt() {
    for proto in [1u8, 2] {
        let (k, mut d) = setup2();
        assert_eq!(d.caps & 4, 4);
        let info = get_info(&mut d).unwrap();
        assert!(info.hmac_secret());
        k.borrow_mut().protocols = vec![proto as i64];
        assert_eq!(get_info(&mut d).unwrap().protocol(), proto);
        k.borrow_mut().touch = true;
        let cdh = sha(b"cdh");
        let cred = make_credential(&mut d, RP, &[9u8; 16], "ling", &cdh).unwrap();
        assert!(cred.hmac_secret);
        let salt = sha(b"slot salt");
        let mut outs = vec![];
        for round in 0..2u8 {
            k.borrow_mut().touch = true;
            let sh = key_agreement(&mut d, proto, &eph(round)).unwrap();
            let iv = [round; 16];
            let a = hmac_secret(&mut d, RP, &cdh, &[cred.id.clone()], &salt, &sh, &iv).unwrap();
            assert!(a.verify(&cred.public_key, &cdh));
            assert!(!a.verify(&cred.public_key, &sha(b"other")));
            assert_eq!(a.credential_id, cred.id);
            outs.push(a.output);
        }
        assert_eq!(outs[0], outs[1], "same credential + salt -> same secret");
        // another salt, another secret
        k.borrow_mut().touch = true;
        let sh = key_agreement(&mut d, proto, &eph(9)).unwrap();
        let a = hmac_secret(&mut d, RP, &cdh, &[cred.id.clone()], &sha(b"other salt"), &sh, &[0; 16]).unwrap();
        assert_ne!(a.output, outs[0]);
    }
}

#[test]
fn picks_the_plugged_credential_and_reports_strangers() {
    let (k, mut d) = setup2();
    let cdh = sha(b"cdh");
    k.borrow_mut().touch = true;
    let c1 = make_credential(&mut d, RP, &[1u8; 16], "ling", &cdh).unwrap();
    let other = vec![0xEE, 0xEE];
    assert!(owns_any(&mut d, RP, &[other.clone(), c1.id.clone()]).unwrap());
    assert!(!owns_any(&mut d, RP, &[other.clone()]).unwrap());
    assert!(!owns_any(&mut d, "lingos.messenger", &[c1.id.clone()]).unwrap());
    k.borrow_mut().touch = true;
    let sh = key_agreement(&mut d, 2, &eph(1)).unwrap();
    let a = hmac_secret(&mut d, RP, &cdh, &[other.clone(), c1.id.clone()], &sha(b"s"), &sh, &[0; 16]).unwrap();
    assert_eq!(a.credential_id, c1.id);
    let sh = key_agreement(&mut d, 2, &eph(2)).unwrap();
    match hmac_secret(&mut d, RP, &cdh, &[other], &sha(b"s"), &sh, &[0; 16]) {
        Err(Error::Ctap(ERR_NO_CREDENTIALS)) => {},
        Err(e) => panic!("{e:?}"),
        Ok(_) => panic!("a stranger's credential answered"),
    }
}

#[test]
fn no_touch_and_pin_keys_say_so() {
    let (k, mut d) = setup2();
    let cdh = sha(b"cdh");
    match make_credential(&mut d, RP, &[1u8; 16], "ling", &cdh) {
        Err(Error::Ctap(ERR_USER_ACTION_TIMEOUT)) => {},
        _ => panic!("expected a timeout"),
    }
    k.borrow_mut().want_pin = true;
    match make_credential(&mut d, RP, &[1u8; 16], "ling", &cdh) {
        Err(Error::Ctap(c)) => assert!(needs_pin_or_uv(c)),
        _ => panic!("expected a PIN request"),
    }
}

#[test]
fn a_tampered_answer_is_refused() {
    let (k, mut d) = setup2();
    let cdh = sha(b"cdh");
    k.borrow_mut().touch = true;
    let c = make_credential(&mut d, RP, &[1u8; 16], "ling", &cdh).unwrap();
    k.borrow_mut().touch = true;
    let sh = key_agreement(&mut d, 2, &eph(1)).unwrap();
    let req = get_assertion_request(RP, &cdh, &[c.id.clone()], hmac_secret_input(&sh, &sha(b"s"), &[0; 16]));
    let r = call(&mut d, GET_ASSERTION, Some(&req)).unwrap();
    // a different shared secret cannot read the output
    let sh_other = Shared::new(2, &eph(5), &public_xy(&sha(b"authenticator")).unwrap().0, &public_xy(&sha(b"authenticator")).unwrap().1).unwrap();
    assert!(matches!(parse_hmac_assertion(&r, RP, &[c.id.clone()], &sh_other), Ok(a) if a.output != parse_hmac_assertion(&r, RP, &[c.id.clone()], &sh).unwrap().output));
    // the wrong RP id fails the rpIdHash check
    assert!(parse_hmac_assertion(&r, "lingos.messenger", &[c.id.clone()], &sh).is_err());
    // flip a bit of authData: the signature no longer checks out
    let a = parse_hmac_assertion(&r, RP, &[c.id.clone()], &sh).unwrap();
    assert!(a.verify(&c.public_key, &cdh));
    let mut bad = HmacAssertion { credential_id: a.credential_id.clone(), output: a.output, flags: a.flags, counter: a.counter, auth_data: a.auth_data.clone(), signature: a.signature.clone() };
    bad.auth_data[33] ^= 1;
    assert!(!bad.verify(&c.public_key, &cdh));
}

// ── keyslots (keyslot.rs) ───────────────────────────────────────────────────

use super::keyslot::{self, Kind, Record, Slot};

fn slot_from(user: &str, app: &str, cred: &Credential, output: &[u8; 32], s: &[u8; 32], nonce: u8) -> Slot {
    let (wrapped, tag) = keyslot::wrap(user, app, &cred.id, output, s, &[nonce; 12]).unwrap();
    Slot { kind: Kind::Hmac, name: "key".into(), counter: 1, uv: false, cred: cred.id.clone(), public_key: cred.public_key.to_vec(), nonce: [nonce; 12], tag, wrapped }
}

fn touch_secret(k: &alloc::rc::Rc<core::cell::RefCell<Soft2>>, d: &mut Device<Wire2>, creds: &[Vec<u8>], salt: &[u8; 32], n: u8) -> ([u8; 32], Vec<u8>) {
    k.borrow_mut().touch = true;
    let sh = key_agreement(d, 2, &eph(100 + n)).unwrap();
    let a = hmac_secret(d, RP, &sha(b"cdh"), creds, salt, &sh, &[n; 16]).unwrap();
    (a.output, a.credential_id.clone())
}

#[test]
fn keyslots_open_with_any_one_key_and_nothing_else() {
    let (k, mut d) = setup2();
    let s = sha(b"the app secret");
    let mut rec = Record::new("wallet", &s, sha(b"u2f app"), sha(b"salt"));
    let mut creds = vec![];
    for _ in 0..2 {
        k.borrow_mut().touch = true;
        creds.push(make_credential(&mut d, RP, &[1u8; 16], "ling", &sha(b"cdh")).unwrap());
    }
    // enrol: one hmac-secret touch per key, S wrapped under each
    for (i, c) in creds.iter().enumerate() {
        let (out, _) = touch_secret(&k, &mut d, core::slice::from_ref(&c.id), &rec.salt, i as u8);
        rec.slots.push(slot_from("ling", "wallet", c, &out, &s, i as u8));
    }
    // unlock: either key, asked with every credential allowed
    let all: Vec<Vec<u8>> = creds.iter().map(|c| c.id.clone()).collect();
    let (out, which) = touch_secret(&k, &mut d, &all, &rec.salt, 7);
    let slot = rec.slots.iter().find(|x| x.cred == which).unwrap();
    let got = keyslot::unwrap("ling", "wallet", slot, &out).unwrap();
    assert_eq!(got, s);
    assert!(rec.check("wallet", &got));
    // the other key's output does not open this slot; nor another user's / app's record
    let other = rec.slots.iter().find(|x| x.cred != which).unwrap();
    assert!(keyslot::unwrap("ling", "wallet", other, &out).is_none());
    assert!(keyslot::unwrap("sanny", "wallet", slot, &out).is_none());
    assert!(keyslot::unwrap("ling", "messenger", slot, &out).is_none());
    // a tampered slot does not open
    let mut bad = slot.clone();
    bad.wrapped[0] ^= 1;
    assert!(keyslot::unwrap("ling", "wallet", &bad, &out).is_none());
    let mut bad = slot.clone();
    bad.tag[15] ^= 0x80;
    assert!(keyslot::unwrap("ling", "wallet", &bad, &out).is_none());
    // a different salt (another record) gives a different output
    let (out2, _) = touch_secret(&k, &mut d, &all, &sha(b"another salt"), 8);
    assert!(keyslot::unwrap("ling", "wallet", slot, &out2).is_none());
    assert!(!rec.check("wallet", &sha(b"wrong")));
    assert!(!rec.check("messenger", &s));
}

#[test]
fn records_round_trip_and_strict_records_hold_no_secret() {
    let s = sha(b"secret");
    let mut rec = Record::new("messenger", &s, sha(b"u"), sha(b"salt"));
    rec.over = "2500".into();
    rec.slots.push(Slot { kind: Kind::U2f, name: "Blue: key\nx".into(), counter: 9, uv: false, cred: vec![1, 2, 3], public_key: vec![4; 65], nonce: [0; 12], tag: [0; 16], wrapped: vec![] });
    rec.slots.push(Slot { kind: Kind::Hmac, name: "Bio".into(), counter: 3, uv: true, cred: vec![5; 40], public_key: vec![6; 65], nonce: [7; 12], tag: [8; 16], wrapped: vec![9; 32] });
    let text = rec.to_text();
    let back = Record::parse("messenger", &text).unwrap();
    assert_eq!(back.s, Some(s));
    assert_eq!(back.over, "2500");
    assert_eq!(back.slots.len(), 2);
    assert_eq!(back.slots[0].name, "Blue keyx"); // no ':' or line breaks in names
    assert_eq!(back.slots[1], rec.slots[1]);
    // the recovery copy must belong to the app it is read for
    assert!(Record::parse("wallet", &text).is_none());
    // strict: the text carries no S, and parsing drops one that was slipped in
    rec.strict = true;
    let t = rec.to_text();
    let st = core::str::from_utf8(&t).unwrap();
    assert!(!st.contains(&keyslot::hex(&s)));
    let back = Record::parse("messenger", &t).unwrap();
    assert!(back.strict && back.s.is_none());
    let mut sneaky = t.clone();
    sneaky.extend_from_slice(alloc::format!("s:{}\n", keyslot::hex(&s)).as_bytes());
    assert!(Record::parse("messenger", &sneaky).unwrap().s.is_none());
    // junk is refused
    assert!(Record::parse("messenger", b"hello").is_none());
    assert!(Record::parse("messenger", b"lingos-appkeys v1\nsalt:00\n").is_none());
    assert!(keyslot::ct_eq(b"abc", b"abc") && !keyslot::ct_eq(b"abc", b"abd") && !keyslot::ct_eq(b"ab", b"abc"));
}
