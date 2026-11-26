//! Fuzz-style tests: random, truncated and mutated packets into every parser
//! and into a running host. Nothing may panic; limits must hold.

use crate::hci::{self, op};
use crate::host::{Host, Output, State};
use crate::store::{self, Store};
use crate::*;
use alloc::vec;
use alloc::vec::Vec;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let n = (self.next() as usize) % (max + 1);
        (0..n).map(|_| self.byte()).collect()
    }
}

/// Plausible-looking event: a real code, a length that may or may not match.
fn rand_event(r: &mut Rng) -> Vec<u8> {
    const CODES: [u8; 26] = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x0E, 0x0F, 0x10, 0x13, 0x16, 0x17, 0x18, 0x22, 0x2F, 0x30, 0x31, 0x32, 0x33, 0x34, 0x36, 0x3B, 0x3E, 0xFF];
    let code = CODES[(r.next() % CODES.len() as u64) as usize];
    let mut p = r.bytes(260);
    if r.next() % 2 == 0 {
        p.truncate(255);
        let mut v = vec![code, p.len() as u8];
        v.extend(p);
        v
    } else {
        let mut v = vec![code, r.byte()];
        v.extend(p);
        v
    }
}

#[test]
fn parsers_never_panic() {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let mut asm = l2cap::Reassembler::default();
    let mut st = hci::Stream::new(hci::Kind::Acl);
    let mut se = hci::Stream::new(hci::Kind::Event);
    let mut sbc_st = [[0f32; 160]; 2];
    for i in 0..60_000 {
        let b = r.bytes(300);
        let _ = hci::parse_event(&b);
        let _ = hci::parse_event(&rand_event(&mut r));
        let _ = hci::parse_acl(&b);
        let _ = hci::eir_name(&b);
        let _ = l2cap::parse_sig(&b);
        let _ = hidp::parse_input(&b);
        let _ = hidp::parse_handshake(&b);
        let _ = btintel::parse_vendor(&b);
        let _ = btintel::parse_boot_params(&b);
        let _ = btintel::parse_version(&b);
        let _ = btintel::tlv_fw_name(&b);
        let _ = btintel::ddc_cmds(&b);
        let _ = avdtp::sbc_caps(&b);
        if i % 50 == 0 {
            let _ = btintel::plan_sfi(&r.bytes(4000));
            let mut f = r.bytes(200);
            if !f.is_empty() {
                f[0] = 0x9C;
            }
            let _ = sbc::decode(&f, &mut sbc_st);
            let _ = Store::parse(&alloc::string::String::from_utf8_lossy(&b));
        }
        let pb = r.byte() & 3;
        let _ = asm.push(r.byte() as u16 & 3, pb, &b);
        st.push(&b);
        while st.next().is_some() {}
        se.push(&b);
        while se.next().is_some() {}
    }
}

#[test]
fn avdtp_session_never_panics() {
    let mut r = Rng(42);
    for _ in 0..200 {
        let mut s = avdtp::Session::new(48000);
        let _ = s.start();
        for _ in 0..200 {
            let mut m = r.bytes(40);
            if m.len() >= 2 && r.next() % 2 == 0 {
                m[1] = (r.byte() % 14) + 1;
            }
            let _ = s.input(&m);
            let _ = s.media_opened();
        }
    }
}

fn ready_host(store: Store) -> Host {
    let mut h = Host::new(store);
    h.start(0, [3; 6]);
    for _ in 0..40 {
        let outs = h.take_output();
        let Some(c) = outs.iter().rev().find_map(|o| if let Output::Cmd(c) = o { Some(c.clone()) } else { None }) else { break };
        let o = hci::command_opcode(&c).unwrap();
        let mut p = vec![1];
        p.extend_from_slice(&o.to_le_bytes());
        p.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 8, 0]);
        h.on_event(&[&[0x0E, p.len() as u8][..], &p].concat());
    }
    assert_eq!(h.state, State::Ready);
    h
}

#[test]
fn host_survives_random_traffic_and_keeps_limits() {
    let mut st = Store::default();
    st.bonds.push(store::Bond { addr: [1, 2, 3, 4, 5, 6], key: [1; 16], key_type: 7, cod: 0x2580, allow_kbd: false, allow_ptr: true, audio: false, name: "m".into(), ..Default::default() });
    let mut h = ready_host(st);
    let mut r = Rng(7);
    let mut inputs = 0;
    for i in 0..200_000u64 {
        match r.next() % 4 {
            0 | 1 => h.on_event(&rand_event(&mut r)),
            2 => {
                // ACL on a small set of handles, sometimes a valid L2CAP signalling frame
                let mut d = r.bytes(80);
                if r.next() % 3 == 0 {
                    d = l2cap::frame(r.byte() as u16 % 3 + 0x40 * (r.byte() as u16 % 2), &d);
                }
                h.on_acl(&hci::acl_packet(r.byte() as u16 % 4 + 0x40, r.byte() & 3, &d));
            },
            _ => h.tick(i * 3),
        }
        for o in h.take_output() {
            if let Output::Input(..) = o {
                inputs += 1;
            }
        }
        assert!(h.conns.len() <= host::MAX_CONNS);
        assert!(h.seen.len() <= host::MAX_SEEN);
        assert!(h.acl_backlog() <= 256);
    }
    // random traffic never produced a secure link, so never any input
    assert_eq!(inputs, 0);
    assert!(h.conns.iter().all(|c| !c.secure));
    let _ = op::RESET;
}
