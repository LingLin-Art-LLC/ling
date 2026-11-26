//! Host-side tests with synthetic HCI traffic: init, every pairing path and
//! security gate, L2CAP / HID, the block list, the store, the Intel firmware
//! plan (on the real ibt-19-0-4 files), SBC and AVDTP.

use crate::hci::{self, op};
use crate::host::{Host, Output, Prompt, State};
use crate::l2cap;
use crate::store::{self, Store};
use crate::*;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const DEV: BdAddr = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
const COD_MOUSE: u32 = 0x00_2580;
const COD_KBD: u32 = 0x00_2540;
const COD_HEADPHONES: u32 = 0x24_0418;

fn evt(code: u8, p: &[u8]) -> Vec<u8> {
    let mut v = vec![code, p.len() as u8];
    v.extend_from_slice(p);
    v
}

fn cc(opcode: u16, ret: &[u8]) -> Vec<u8> {
    let mut p = vec![1];
    p.extend_from_slice(&opcode.to_le_bytes());
    p.extend_from_slice(ret);
    evt(0x0E, &p)
}

fn cs(opcode: u16, status: u8) -> Vec<u8> {
    let mut p = vec![status, 1];
    p.extend_from_slice(&opcode.to_le_bytes());
    evt(0x0F, &p)
}

fn with_addr(a: &BdAddr, rest: &[u8]) -> Vec<u8> {
    let mut v = a.to_vec();
    v.extend_from_slice(rest);
    v
}

struct Sim {
    h: Host,
    out: Vec<Output>,
    now: u64,
    /// Answer every command like a controller would (Command Status / Complete, success).
    auto: bool,
    answered: usize,
}

fn answer_for(c: &[u8]) -> Vec<u8> {
    let opcode = hci::command_opcode(c).unwrap();
    match opcode {
        op::CREATE_CONNECTION | op::AUTH_REQUESTED | op::SET_CONN_ENCRYPTION | op::INQUIRY | op::REMOTE_NAME_REQUEST | op::ACCEPT_CONNECTION | op::REJECT_CONNECTION | op::DISCONNECT => cs(opcode, 0),
        op::READ_BD_ADDR => cc(opcode, &[0, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5]),
        op::READ_BUFFER_SIZE => cc(opcode, &[0, 0xFD, 0x03, 0x40, 64, 0, 4, 0]),
        _ => cc(opcode, &[0]),
    }
}

impl Sim {
    fn new(store: Store) -> Sim {
        Sim { h: Host::new(store), out: Vec::new(), now: 1000, auto: true, answered: 0 }
    }

    fn take(&mut self) {
        self.out.extend(self.h.take_output());
        if !self.auto {
            return;
        }
        // commands go out one at a time, each after the previous one's answer: answer them in order
        while let Some(c) = self.cmds().get(self.answered).cloned() {
            self.answered += 1;
            self.h.on_event(&answer_for(&c));
            self.out.extend(self.h.take_output());
        }
    }

    fn cmds(&self) -> Vec<Vec<u8>> {
        self.out.iter().filter_map(|o| if let Output::Cmd(c) = o { Some(c.clone()) } else { None }).collect()
    }

    fn has_cmd(&self, opcode: u16) -> bool {
        self.cmds().iter().any(|c| hci::command_opcode(c) == Some(opcode))
    }

    fn last_cmd(&self, opcode: u16) -> Option<Vec<u8>> {
        self.cmds().into_iter().rev().find(|c| hci::command_opcode(c) == Some(opcode))
    }

    fn notices(&self) -> Vec<String> {
        self.out.iter().filter_map(|o| match o { Output::Notice(s) | Output::DeviceNotice(_, s) => Some(s.clone()), _ => None }).collect()
    }

    fn acls(&self) -> Vec<Vec<u8>> {
        self.out.iter().filter_map(|o| if let Output::Acl(c) = o { Some(c.clone()) } else { None }).collect()
    }

    fn ev(&mut self, pkt: Vec<u8>) {
        self.h.on_event(&pkt);
        self.take();
    }

    fn answer_all(&mut self) {
        self.take();
    }

    fn up(store: Store) -> Sim {
        let mut s = Sim::new(store);
        s.h.start(s.now, [1, 2, 3, 4, 5, 6]);
        s.take();
        assert_eq!(s.h.state, State::Ready);
        s.out.clear();
        s.answered = 0;
        s
    }

    /// The device acting on the L2CAP channel we just asked for: accept + configure.
    fn l2_accept_last(&mut self, handle: u16, dcid: u16) {
        let acl = self.acls().last().cloned().expect("an ACL packet");
        let a = hci::parse_acl(&acl).unwrap();
        let (cid, payload) = (le16(a.data, 2).unwrap(), &a.data[4..]);
        assert_eq!(cid, l2cap::CID_SIGNALING);
        let sig = l2cap::parse_sig(payload);
        let l2cap::Sig::ConnReq { id, scid, .. } = sig[0] else { panic!("{sig:?}") };
        let mut f = l2cap::sig_conn_rsp(id, dcid, scid, 0);
        f.extend(l2cap::sig_conf_req(0x40, scid));
        self.acl_in(handle, &l2cap::frame(1, &f));
        // our configuration request came out (before the answer to theirs): accept it
        let id = self.acls().iter().rev().take(3).find_map(|acl| {
            let a = hci::parse_acl(acl)?;
            l2cap::parse_sig(&a.data[4..]).iter().find_map(|s| if let l2cap::Sig::ConfReq { id, .. } = s { Some(*id) } else { None })
        });
        let id = id.expect("our ConfReq");
        // the response names the receiver's channel: ours
        self.acl_in(handle, &l2cap::frame(1, &l2cap::sig_conf_rsp(id, scid, 0, &[])));
        let _ = dcid;
    }

    fn acl_in(&mut self, handle: u16, frame: &[u8]) {
        let pkt = hci::acl_packet(handle, hci::PB_FIRST, frame);
        self.h.on_acl(&pkt);
        self.take();
    }

    fn found(&mut self, a: &BdAddr, cod: u32, name: &str) {
        self.h.scan(5).unwrap();
        self.take();
        let mut p = vec![1];
        p.extend_from_slice(a);
        p.extend_from_slice(&[1, 0]);
        p.extend_from_slice(&cod.to_le_bytes()[..3]);
        p.extend_from_slice(&[0x34, 0x12, 0xC5]);
        let mut eir = vec![(name.len() + 1) as u8, 0x09];
        eir.extend_from_slice(name.as_bytes());
        eir.resize(240, 0);
        p.extend_from_slice(&eir);
        self.ev(evt(0x2F, &p));
    }

    /// Connect + authenticate a device to the point of the IO capability exchange.
    fn pair_to_io(&mut self, a: &BdAddr, cod: u32, handle: u16) {
        self.found(a, cod, "Dev");
        self.h.pair(a).unwrap();
        self.take();
        assert!(self.has_cmd(op::CREATE_CONNECTION));
        let mut p = vec![0];
        p.extend_from_slice(&handle.to_le_bytes());
        p.extend_from_slice(a);
        p.extend_from_slice(&[1, 0]);
        self.ev(evt(0x03, &p));
        assert!(self.has_cmd(op::AUTH_REQUESTED), "we authenticate every link");
        self.ev(evt(0x17, a));
        assert!(self.has_cmd(op::LINK_KEY_NEG_REPLY), "a pairing gets a fresh key");
        self.ev(evt(0x31, a));
    }

    fn finish_pairing(&mut self, a: &BdAddr, handle: u16, key_type: u8) {
        let mut ssp = vec![0u8];
        ssp.extend_from_slice(a);
        self.ev(evt(0x36, &ssp));
        self.ev(evt(0x18, &with_addr(a, &[[7u8; 16].as_slice(), &[key_type]].concat())));
        self.ev(evt(0x06, &[0, handle as u8, (handle >> 8) as u8]));
    }

    fn encrypt(&mut self, handle: u16, mode: u8, key_size: u8) {
        assert!(self.has_cmd(op::SET_CONN_ENCRYPTION));
        self.ev(evt(0x08, &[0, handle as u8, (handle >> 8) as u8, mode]));
        if mode == 2 {
            assert!(self.has_cmd(op::READ_ENC_KEY_SIZE));
            self.ev(cc(op::READ_ENC_KEY_SIZE, &[0, handle as u8, (handle >> 8) as u8, key_size]));
        }
    }

    fn disconnected(&self, handle: u16) -> bool {
        self.cmds().iter().any(|c| hci::command_opcode(c) == Some(op::DISCONNECT) && le16(c, 3) == Some(handle))
    }
}

fn mouse_store(kt: u8) -> Store {
    let mut st = Store::default();
    st.bonds.push(store::Bond { addr: DEV, key: [9; 16], key_type: kt, cod: COD_MOUSE, allow_kbd: false, allow_ptr: true, audio: false, name: "Mouse".into(), ..Default::default() });
    st
}

// ── gating ──────────────────────────────────────────────────────────────────

#[test]
fn nothing_is_sent_before_start() {
    let mut h = Host::new(mouse_store(store::KEY_UNAUTH_P256));
    // a device paging us, a pairing attempt, a tick: no output at all while off
    h.on_event(&evt(0x04, &with_addr(&DEV, &[0x80, 0x25, 0x00, 1])));
    h.on_event(&evt(0x31, &DEV));
    h.on_acl(&hci::acl_packet(1, 2, &[1, 0, 1, 0, 2]));
    h.tick(5000);
    assert!(h.take_output().is_empty());
    assert_eq!(h.state, State::Off);
}

#[test]
fn init_reaches_ready_and_sets_secure_connections() {
    let mut s = Sim::new(Store::default());
    s.h.start(0, [9; 6]);
    s.take();
    s.answer_all();
    assert_eq!(s.h.state, State::Ready);
    assert_eq!(hci::command_opcode(&s.cmds()[0]), Some(op::RESET));
    assert!(s.has_cmd(op::WRITE_SC_HOST_SUPPORT));
    assert!(s.has_cmd(op::WRITE_SSP_MODE));
    // no bonds, no pairing window: neither discoverable nor connectable
    assert_eq!(s.last_cmd(op::WRITE_SCAN_ENABLE).unwrap()[3], 0);
    assert_eq!(s.h.local_addr, [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5]);
}

#[test]
fn bonds_make_us_connectable_but_not_discoverable() {
    let s = {
        let mut s = Sim::new(mouse_store(store::KEY_UNAUTH_P256));
        s.h.start(0, [9; 6]);
        s.take();
        s.answer_all();
        s
    };
    assert_eq!(s.last_cmd(op::WRITE_SCAN_ENABLE).unwrap()[3], 0b10);
}

#[test]
fn pairing_window_is_discoverable_and_times_out() {
    let mut s = Sim::up(Store::default());
    s.h.set_pairable(true);
    s.take();
    assert_eq!(s.last_cmd(op::WRITE_SCAN_ENABLE).unwrap()[3], 0b11);
    s.answer_all();
    s.h.tick(s.now + crate::host::PAIRABLE_MS + 1);
    s.take();
    assert_eq!(s.last_cmd(op::WRITE_SCAN_ENABLE).unwrap()[3], 0);
}

#[test]
fn controller_without_secure_connections_stays_off() {
    let mut s = Sim::new(Store::default());
    s.auto = false;
    s.h.start(0, [9; 6]);
    s.take();
    for _ in 0..20 {
        let Some(c) = s.cmds().last().cloned() else { break };
        let o = hci::command_opcode(&c).unwrap();
        let n = s.out.len();
        if o == op::WRITE_SC_HOST_SUPPORT {
            s.ev(cc(o, &[0x01]));
        } else {
            s.ev(answer_for(&c));
        }
        if s.out.len() == n {
            break;
        }
    }
    assert_eq!(s.h.state, State::Failed);
    assert!(s.notices().iter().any(|n| n.contains("Secure Connections")));
}

#[test]
fn stop_disconnects_and_resets() {
    let mut s = Sim::up(Store::default());
    s.pair_to_io(&DEV, COD_MOUSE, 0x0042);
    let v = s.h.stop();
    let ops: Vec<u16> = v.iter().map(|c| hci::command_opcode(c).unwrap()).collect();
    assert_eq!(ops, vec![op::DISCONNECT, op::WRITE_SCAN_ENABLE, op::LE_SET_SCAN_ENABLE, op::RESET]);
    assert_eq!(s.h.state, State::Off);
    assert!(s.h.conns.is_empty());
}

// ── pairing ─────────────────────────────────────────────────────────────────

/// A fresh HID bond first reads the device's descriptor over SDP (our cid
/// 0x40), then opens HID control (0x41) and interrupt (0x42).
fn sdp_up(s: &mut Sim, handle: u16, desc: &[u8]) {
    s.l2_accept_last(handle, 0x0060);
    for step in 0..2 {
        let acl = s.acls().into_iter().rev().find(|a| hci::parse_acl(a).is_some_and(|p| le16(p.data, 2) == Some(0x0060))).expect("an SDP request");
        let p = hci::parse_acl(&acl).unwrap();
        let tid = u16::from_be_bytes([p.data[5], p.data[6]]);
        let rsp = if step == 0 { crate::tests_le::sdp_rsp(tid, &crate::tests_le::hid_lists(desc)) } else { vec![0x01, (tid >> 8) as u8, tid as u8, 0, 2, 0, 3] };
        s.acl_in(handle, &l2cap::frame(0x0040, &rsp));
    }
}

fn hid_up(s: &mut Sim, handle: u16) {
    // HID control then interrupt, both configured
    s.l2_accept_last(handle, 0x0070);
    s.l2_accept_last(handle, 0x0071);
}

const MOUSE_DESC: &[u8] = &[0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0xC0];
const KBD_DESC: &[u8] = &[0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0xC0];

#[test]
fn mouse_pairs_just_works_and_moves() {
    let mut s = Sim::up(Store::default());
    let hnd = 0x0042;
    s.pair_to_io(&DEV, COD_MOUSE, hnd);
    let io = s.last_cmd(op::IO_CAP_REPLY).expect("io cap reply");
    assert_eq!(io[9], 0x01, "DisplayYesNo");
    assert_eq!(io[11] & 1, 1, "MITM requested");
    s.ev(evt(0x32, &with_addr(&DEV, &[3, 0, 0])));
    s.ev(evt(0x33, &with_addr(&DEV, &123456u32.to_le_bytes())));
    assert!(s.has_cmd(op::USER_CONFIRM_REPLY), "a mouse the user chose: just works");
    s.finish_pairing(&DEV, hnd, store::KEY_UNAUTH_P256);
    let b = s.h.store.bond(&DEV).expect("bonded");
    assert!(b.allow_ptr && !b.allow_kbd);
    assert!(s.out.contains(&Output::StoreChanged));
    s.encrypt(hnd, 2, 16);
    sdp_up(&mut s, hnd, MOUSE_DESC);
    hid_up(&mut s, hnd);
    // SET_PROTOCOL(boot) went out on the control channel
    assert!(s.acls().iter().any(|a| hci::parse_acl(a).unwrap().data.ends_with(&crate::hidp::SET_PROTOCOL_BOOT)));
    let n0 = s.out.len();
    // a mouse report on the interrupt channel (our cid for it is the second one we allocated)
    let intr = s.h.conns[0].clone();
    let _ = intr;
    s.acl_in(hnd, &l2cap::frame(0x0042, &[0xA1, 2, 1, 5, 0xFB]));
    assert!(s.out[n0..].contains(&Output::Input(DEV, crate::hidp::Report::Mouse { buttons: 1, dx: 5, dy: -5, wheel: 0 })));
    // a keyboard report from the mouse: blocked, the user told
    s.acl_in(hnd, &l2cap::frame(0x0042, &[0xA1, 1, 0, 0, 4, 0, 0, 0, 0, 0]));
    assert!(!s.out.iter().any(|o| matches!(o, Output::Input(_, crate::hidp::Report::Keyboard(_)))));
    assert!(s.notices().iter().any(|n| n.contains("keyboard input; blocked")));
}

#[test]
fn keyboard_just_works_is_refused() {
    let mut s = Sim::up(Store::default());
    s.pair_to_io(&DEV, COD_KBD, 0x0042);
    s.ev(evt(0x32, &with_addr(&DEV, &[3, 0, 0])));
    s.ev(evt(0x33, &with_addr(&DEV, &1u32.to_le_bytes())));
    assert!(s.has_cmd(op::USER_CONFIRM_NEG_REPLY));
    assert!(!s.has_cmd(op::USER_CONFIRM_REPLY));
}

#[test]
fn keyboard_with_unauthenticated_key_is_refused() {
    let mut s = Sim::up(Store::default());
    s.pair_to_io(&DEV, COD_KBD, 0x0042);
    s.finish_pairing(&DEV, 0x0042, store::KEY_UNAUTH_P256);
    assert!(s.h.store.bond(&DEV).is_none());
    assert!(s.disconnected(0x0042));
}

#[test]
fn keyboard_numeric_comparison_then_types() {
    let mut s = Sim::up(Store::default());
    let hnd = 0x0042;
    s.pair_to_io(&DEV, COD_KBD, hnd);
    s.ev(evt(0x32, &with_addr(&DEV, &[1, 0, 5])));
    s.ev(evt(0x33, &with_addr(&DEV, &654321u32.to_le_bytes())));
    assert!(matches!(s.h.prompt, Some(Prompt::Confirm { value: 654321, .. })));
    assert!(!s.has_cmd(op::USER_CONFIRM_REPLY), "waits for the user");
    assert!(s.h.confirm(true));
    s.take();
    assert!(s.has_cmd(op::USER_CONFIRM_REPLY));
    s.finish_pairing(&DEV, hnd, store::KEY_AUTH_P256);
    assert!(s.h.store.bond(&DEV).unwrap().allow_kbd);
    s.encrypt(hnd, 2, 16);
    sdp_up(&mut s, hnd, KBD_DESC);
    hid_up(&mut s, hnd);
    s.acl_in(hnd, &l2cap::frame(0x0042, &[0xA1, 1, 0x02, 0, 4, 0, 0, 0, 0, 0]));
    assert!(s.out.contains(&Output::Input(DEV, crate::hidp::Report::Keyboard([2, 0, 4, 0, 0, 0, 0, 0]))));
    assert!(s.notices().iter().any(|n| n.contains("keyboard connected")));
}

#[test]
fn passkey_is_shown() {
    let mut s = Sim::up(Store::default());
    s.pair_to_io(&DEV, COD_KBD, 0x0042);
    s.ev(evt(0x32, &with_addr(&DEV, &[2, 0, 5])));
    s.ev(evt(0x3B, &with_addr(&DEV, &12345u32.to_le_bytes())));
    assert_eq!(s.h.prompt, Some(Prompt::Passkey { addr: DEV, passkey: 12345 }));
    assert!(s.notices().iter().any(|n| n.contains("type 012345")));
}

#[test]
fn numeric_comparison_times_out_to_no() {
    let mut s = Sim::up(Store::default());
    s.pair_to_io(&DEV, COD_KBD, 0x0042);
    s.ev(evt(0x32, &with_addr(&DEV, &[1, 0, 5])));
    s.ev(evt(0x33, &with_addr(&DEV, &1u32.to_le_bytes())));
    s.answer_all();
    s.h.tick(s.now + crate::host::CONFIRM_MS + 10);
    s.take();
    assert!(s.has_cmd(op::USER_CONFIRM_NEG_REPLY));
    assert!(s.h.prompt.is_none());
}

#[test]
fn legacy_pin_pairing_is_refused() {
    let mut s = Sim::up(Store::default());
    s.pair_to_io(&DEV, COD_MOUSE, 0x0042);
    s.ev(evt(0x16, &DEV));
    assert!(s.has_cmd(op::PIN_CODE_NEG_REPLY));
    assert!(s.disconnected(0x0042));
    assert!(s.notices().iter().any(|n| n.contains("insecure")));
}

#[test]
fn legacy_link_key_is_refused() {
    let mut s = Sim::up(Store::default());
    s.pair_to_io(&DEV, COD_MOUSE, 0x0042);
    s.finish_pairing(&DEV, 0x0042, 0x05); // authenticated P-192: not Secure Connections
    assert!(s.h.store.bond(&DEV).is_none());
    assert!(s.disconnected(0x0042));
}

#[test]
fn short_encryption_key_is_dropped() {
    let mut s = Sim::up(Store::default());
    s.pair_to_io(&DEV, COD_MOUSE, 0x0042);
    s.ev(evt(0x32, &with_addr(&DEV, &[3, 0, 0])));
    s.ev(evt(0x33, &with_addr(&DEV, &1u32.to_le_bytes())));
    s.finish_pairing(&DEV, 0x0042, store::KEY_UNAUTH_P256);
    s.encrypt(0x0042, 2, 7);
    assert!(s.disconnected(0x0042));
    assert!(!s.h.conns[0].secure);
}

#[test]
fn e0_encryption_is_dropped() {
    let mut s = Sim::up(Store::default());
    s.pair_to_io(&DEV, COD_MOUSE, 0x0042);
    s.ev(evt(0x32, &with_addr(&DEV, &[3, 0, 0])));
    s.ev(evt(0x33, &with_addr(&DEV, &1u32.to_le_bytes())));
    s.finish_pairing(&DEV, 0x0042, store::KEY_UNAUTH_P256);
    s.encrypt(0x0042, 1, 16);
    assert!(s.disconnected(0x0042));
}

#[test]
fn early_encryption_waits_for_our_authentication() {
    let mut s = Sim::up(mouse_store(store::KEY_UNAUTH_P256));
    page(&mut s, &DEV);
    let mut p = vec![0, 0x42, 0];
    p.extend_from_slice(&DEV);
    p.extend_from_slice(&[1, 0]);
    s.ev(evt(0x03, &p));
    s.ev(evt(0x17, &DEV));
    // the device encrypts first: no key-size check, no profile, until authentication completes
    s.ev(evt(0x08, &[0, 0x42, 0, 2]));
    assert!(!s.has_cmd(op::READ_ENC_KEY_SIZE));
    assert!(!s.disconnected(0x42));
    s.ev(evt(0x06, &[0, 0x42, 0]));
    assert!(s.has_cmd(op::READ_ENC_KEY_SIZE));
    assert!(!s.has_cmd(op::SET_CONN_ENCRYPTION));
    s.ev(cc(op::READ_ENC_KEY_SIZE, &[0, 0x42, 0, 16]));
    assert!(s.h.conns[0].secure);
}

#[test]
fn authenticated_bond_refuses_downgrade() {
    let mut st = mouse_store(store::KEY_AUTH_P256);
    st.bonds[0].cod = COD_MOUSE;
    let mut s = Sim::up(st);
    s.h.set_pairable(true);
    s.take();
    // the device pages us and re-pairs with just works
    s.ev(evt(0x04, &with_addr(&DEV, &[0x80, 0x25, 0x00, 1])));
    let mut p = vec![0, 0x42, 0];
    p.extend_from_slice(&DEV);
    p.extend_from_slice(&[1, 0]);
    s.ev(evt(0x03, &p));
    s.ev(evt(0x18, &with_addr(&DEV, &[[7u8; 16].as_slice(), &[store::KEY_UNAUTH_P256]].concat())));
    assert!(s.disconnected(0x42));
    assert_eq!(s.h.store.bond(&DEV).unwrap().key, [9; 16], "the old bond stays");
}

// ── incoming connections, block list, rate limit ───────────────────────────

fn page(s: &mut Sim, a: &BdAddr) -> Option<Vec<u8>> {
    let n = s.out.len();
    s.ev(evt(0x04, &with_addr(a, &[0x80, 0x25, 0x00, 1])));
    s.out[n..].iter().find_map(|o| match o {
        Output::Cmd(c) if matches!(hci::command_opcode(c), Some(op::ACCEPT_CONNECTION) | Some(op::REJECT_CONNECTION)) => Some(c.clone()),
        _ => None,
    })
}

#[test]
fn unknown_devices_are_rejected_bonded_accepted() {
    let mut s = Sim::up(mouse_store(store::KEY_UNAUTH_P256));
    let r = page(&mut s, &[9, 9, 9, 9, 9, 9]).unwrap();
    assert_eq!(hci::command_opcode(&r), Some(op::REJECT_CONNECTION));
    s.answer_all();
    let r = page(&mut s, &DEV).unwrap();
    assert_eq!(hci::command_opcode(&r), Some(op::ACCEPT_CONNECTION));
    assert_eq!(r[9], 0x00, "we stay central");
}

#[test]
fn blocked_device_is_rejected_first_and_hidden() {
    let mut s = Sim::up(mouse_store(store::KEY_UNAUTH_P256));
    s.h.block(&DEV).unwrap();
    s.take();
    assert!(s.h.store.bond(&DEV).is_none(), "blocking deletes the keys");
    s.answer_all();
    let r = page(&mut s, &DEV).unwrap();
    assert_eq!(hci::command_opcode(&r), Some(op::REJECT_CONNECTION));
    assert_eq!(r[9], 0x0F);
    // hidden from scans unless "show blocked"
    s.answer_all();
    s.found(&DEV, COD_MOUSE, "Mouse");
    assert!(s.h.visible().is_empty());
    s.h.store.show_blocked = true;
    assert_eq!(s.h.visible().len(), 1);
    assert!(s.h.pair(&DEV).is_err());
    assert!(s.h.unblock(&DEV));
    s.h.store.show_blocked = false;
}

#[test]
fn block_disconnects_a_connected_device() {
    let mut s = Sim::up(Store::default());
    s.pair_to_io(&DEV, COD_MOUSE, 0x0042);
    s.h.block(&DEV).unwrap();
    s.take();
    assert!(s.disconnected(0x0042));
}

#[test]
fn name_pattern_blocks() {
    let mut s = Sim::up(Store::default());
    assert!(s.h.block_name("evil"));
    s.found(&DEV, COD_MOUSE, "Totally EVIL mouse");
    assert!(s.h.visible().is_empty());
    s.answer_all();
    // the name is known from the scan, so its connection request is rejected too
    s.h.set_pairable(true);
    s.take();
    let r = page(&mut s, &DEV).unwrap();
    assert_eq!(hci::command_opcode(&r), Some(op::REJECT_CONNECTION));
}

#[test]
fn repeated_attempts_are_rate_limited() {
    let mut s = Sim::up(mouse_store(store::KEY_UNAUTH_P256));
    let mut rejected = 0;
    for _ in 0..8 {
        if let Some(r) = page(&mut s, &DEV) {
            if hci::command_opcode(&r) == Some(op::REJECT_CONNECTION) {
                rejected += 1;
            }
        }
        s.answer_all();
    }
    assert!(rejected >= 3);
    assert!(s.notices().iter().any(|n| n.contains("keeps trying")));
}

#[test]
fn unsolicited_pairing_is_refused() {
    let mut s = Sim::up(mouse_store(store::KEY_UNAUTH_P256));
    page(&mut s, &DEV);
    let mut p = vec![0, 0x42, 0];
    p.extend_from_slice(&DEV);
    p.extend_from_slice(&[1, 0]);
    s.ev(evt(0x03, &p));
    s.ev(evt(0x31, &DEV));
    assert!(s.has_cmd(op::IO_CAP_NEG_REPLY));
}

#[test]
fn reconnecting_bond_hid_waits_for_security() {
    let mut s = Sim::up(mouse_store(store::KEY_UNAUTH_P256));
    page(&mut s, &DEV);
    let mut p = vec![0, 0x42, 0];
    p.extend_from_slice(&DEV);
    p.extend_from_slice(&[1, 0]);
    s.ev(evt(0x03, &p));
    // the device opens HID control at once: answered "pending"
    s.acl_in(0x42, &l2cap::frame(1, &l2cap::sig_conn_req(3, l2cap::PSM_HID_CONTROL, 0x0050)));
    let last = s.acls().last().cloned().unwrap();
    let sig = l2cap::parse_sig(&hci::parse_acl(&last).unwrap().data[4..]);
    assert!(matches!(sig[0], l2cap::Sig::ConnRsp { result: l2cap::RES_PENDING, .. }));
    // SDP: never offered
    s.acl_in(0x42, &l2cap::frame(1, &l2cap::sig_conn_req(4, l2cap::PSM_SDP, 0x0051)));
    let last = s.acls().last().cloned().unwrap();
    let sig = l2cap::parse_sig(&hci::parse_acl(&last).unwrap().data[4..]);
    assert!(matches!(sig[0], l2cap::Sig::ConnRsp { result: l2cap::RES_PSM_NOT_SUPPORTED, .. }));
    // the stored key, our authentication, AES-CCM, 16 bytes: the request is accepted
    s.ev(evt(0x17, &DEV));
    assert!(s.has_cmd(op::LINK_KEY_REPLY));
    s.ev(evt(0x06, &[0, 0x42, 0]));
    s.encrypt(0x42, 2, 16);
    let last = s.acls().iter().rev().nth(1).cloned().unwrap();
    let sig = l2cap::parse_sig(&hci::parse_acl(&last).unwrap().data[4..]);
    assert!(matches!(sig[0], l2cap::Sig::ConnRsp { result: l2cap::RES_SUCCESS, scid: 0x0050, .. }), "{sig:?}");
}

#[test]
fn changed_device_class_holds_input() {
    let mut s = Sim::up(mouse_store(store::KEY_UNAUTH_P256));
    // the bonded "mouse" now says it is a keyboard
    s.ev(evt(0x04, &with_addr(&DEV, &[0x40, 0x25, 0x00, 1])));
    let mut p = vec![0, 0x42, 0];
    p.extend_from_slice(&DEV);
    p.extend_from_slice(&[1, 0]);
    s.ev(evt(0x03, &p));
    assert!(s.h.conns[0].cod_changed);
    assert!(s.notices().iter().any(|n| n.contains("different device class")));
    assert!(s.h.allow_input(&DEV, true, false).is_err(), "no keyboard rights on a just-works bond");
    assert!(s.h.allow_input(&DEV, false, true).is_ok());
    assert!(!s.h.conns[0].cod_changed);
}

#[test]
fn connection_limit() {
    let mut st = Store::default();
    for i in 0..6u8 {
        st.bonds.push(store::Bond { addr: [i, 1, 1, 1, 1, 1], key: [1; 16], key_type: 7, cod: COD_MOUSE, allow_kbd: false, allow_ptr: true, audio: false, name: String::new(), ..Default::default() });
    }
    let mut s = Sim::up(st);
    for i in 0..6u8 {
        let a = [i, 1, 1, 1, 1, 1];
        page(&mut s, &a);
        let mut p = vec![0, 0x40 + i, 0];
        p.extend_from_slice(&a);
        p.extend_from_slice(&[1, 0]);
        s.ev(evt(0x03, &p));
        s.answer_all();
    }
    assert_eq!(s.h.conns.len(), crate::host::MAX_CONNS);
}

// ── store ───────────────────────────────────────────────────────────────────

#[test]
fn store_round_trip_and_rejects_junk() {
    let mut st = mouse_store(store::KEY_AUTH_P256);
    st.power = true;
    st.bonds[0].allow_kbd = true;
    st.block(&[1, 1, 1, 1, 1, 1], "Spam Speaker").unwrap();
    st.block_name("evil");
    let t = st.to_text();
    assert_eq!(Store::parse(&t), st);
    let junk = "bond zz\nbond 01:02:03:04:05:06 00 07 2580 p\nbond 01:02:03:04:05:06 0909090909090909090909090909090 07 2580 p x\nblock nope\npower maybe\n\u{0}\u{1}";
    let p = Store::parse(junk);
    assert!(p.bonds.is_empty() && p.blocked.is_empty() && !p.power);
    // legacy key types never load
    let legacy = "bond 06:05:04:03:02:01 09090909090909090909090909090909 05 2580 p Old\n";
    assert!(Store::parse(legacy).bonds.is_empty());
}

#[test]
fn addr_format() {
    assert_eq!(addr_str(&DEV), "06:05:04:03:02:01");
    assert_eq!(parse_addr("06:05:04:03:02:01"), Some(DEV));
    assert_eq!(parse_addr("06:05:04:03:02"), None);
    assert_eq!(parse_addr("g6:05:04:03:02:01"), None);
}

// ── hci / l2cap pieces ──────────────────────────────────────────────────────

#[test]
fn stream_rebuilds_packets() {
    let mut st = hci::Stream::new(hci::Kind::Event);
    let mut big = vec![0x0E, 70];
    big.extend((0..70).map(|i| i as u8));
    st.push(&big[..64]);
    assert!(st.next().is_none());
    st.push(&big[64..]);
    st.push(&[0x0F, 4, 0, 1, 3, 0x0C]);
    assert_eq!(st.next().unwrap(), big);
    assert_eq!(st.next().unwrap(), vec![0x0F, 4, 0, 1, 3, 0x0C]);
    assert!(st.next().is_none());
}

#[test]
fn acl_fragment_and_reassemble() {
    let payload: Vec<u8> = (0..900u32).map(|i| i as u8).collect();
    let f = l2cap::frame(0x0041, &payload);
    let pkts = hci::fragment(0x0042, &f, 310);
    assert_eq!(pkts.len(), 3);
    let mut r = l2cap::Reassembler::default();
    let mut got = None;
    for p in &pkts {
        let a = hci::parse_acl(p).unwrap();
        got = r.push(a.handle, a.pb, a.data);
    }
    assert_eq!(got, Some((0x0041, payload)));
    // oversize frames are refused
    let big = l2cap::frame(0x41, &vec![0u8; 4000]);
    assert_eq!(r.push(1, hci::PB_FIRST, &big[..300]), None);
    assert_eq!(r.push(1, hci::PB_CONT, &big[300..600]), None);
    assert!(r.dropped >= 1);
}

#[test]
fn parse_le_report_and_names() {
    let mut p = vec![0x02, 1, 0x00, 0x01];
    p.extend_from_slice(&DEV);
    let data = [2, 1, 6, 5, 0x09, b'L', b'i', b'n', b'g'];
    p.push(data.len() as u8);
    p.extend_from_slice(&data);
    p.push(0xC0);
    match hci::parse_event(&evt(0x3E, &p)).unwrap() {
        hci::Event::LeAdvertisingReport(v) => {
            assert_eq!(v[0].name.as_deref(), Some("Ling"));
            assert_eq!(v[0].rssi, -64);
        },
        e => panic!("{e:?}"),
    }
    assert_eq!(hci::cod_kind(COD_KBD), "keyboard");
    assert_eq!(hci::cod_kind(COD_MOUSE), "mouse");
    assert_eq!(hci::cod_kind(COD_HEADPHONES), "headphones");
    assert_eq!(hci::clean_name(b"a\x1b[31mb\nc"), "a[31mbc");
}

// ── Intel firmware ──────────────────────────────────────────────────────────

const SFI: &[u8] = include_bytes!("../../ling-kernel/firmware/ibt-19-0-4.sfi");
const DDC: &[u8] = include_bytes!("../../ling-kernel/firmware/ibt-19-0-4.ddc");

#[test]
fn ax201_firmware_is_pinned_and_plans() {
    btintel::verify("ibt-19-0-4.sfi", SFI).unwrap();
    btintel::verify("ibt-19-16-4.sfi", SFI).unwrap(); // a WHENCE alias
    btintel::verify("ibt-19-0-4.ddc", DDC).unwrap();
    let mut bad = SFI.to_vec();
    bad[1000] ^= 1;
    assert!(btintel::verify("ibt-19-0-4.sfi", &bad).is_err());
    assert!(btintel::verify("ibt-99-0-0.sfi", SFI).is_err());
    let plan = btintel::plan_sfi(SFI).unwrap();
    assert_ne!(plan.boot_addr, 0);
    assert!(plan.chunks.iter().all(|c| c.len <= 252 && c.len > 0));
    assert_eq!((plan.chunks[0].frag, plan.chunks[0].len), (0, 128));
    // the chunks cover the header pieces and the whole payload exactly once
    let data: usize = plan.chunks.iter().filter(|c| c.frag == 1).map(|c| c.len).sum();
    assert_eq!(data, SFI.len() - 644);
    let cmd = btintel::secure_send_cmd(SFI, &plan.chunks[0]);
    assert_eq!(&cmd[..4], &[0x09, 0xFC, 129, 0x00]);
    let ddc = btintel::ddc_cmds(DDC).unwrap();
    assert_eq!(ddc.len(), 2);
    assert_eq!(ddc[0], vec![0x8B, 0xFC, 4, 0x03, 0x28, 0x01, 0x18]);
    assert!(btintel::plan_sfi(&SFI[..700]).is_err() || btintel::plan_sfi(&SFI[..700]).is_ok());
}

#[test]
fn intel_version_and_names() {
    let ret = [0x00, 0x37, 0x13, 0x00, 0x06, 0x04, 0x01, 0x02, 0x14, 0x00];
    assert!(!btintel::is_tlv(&ret));
    let v = btintel::parse_version(&ret).unwrap();
    v.supported().unwrap();
    let mut bp = [0u8; 23];
    bp[4] = 0x10;
    let p = btintel::parse_boot_params(&bp).unwrap();
    assert_eq!(btintel::fw_name(&v, &p, "sfi").unwrap(), "ibt-19-0-4.sfi");
    let v11 = btintel::Version { hw_variant: 0x0B, ..v };
    assert_eq!(btintel::fw_name(&v11, &p, "sfi").unwrap(), "ibt-11-16.sfi");
    assert_eq!(btintel::parse_vendor(&[0x06, 0x00, 0x09, 0xFC, 0x00]), Some(btintel::VendorEvt::SecureSendResult { result: 0, opcode: 0xFC09, status: 0 }));
    assert_eq!(btintel::reset_cmd(0x12345678), vec![0x01, 0xFC, 8, 0, 1, 0, 1, 0x78, 0x56, 0x34, 0x12]);
}

// ── SBC / AVDTP ─────────────────────────────────────────────────────────────

fn snr_round_trip(cfg: crate::sbc::Config, freq: f32) -> f32 {
    let mut enc = crate::sbc::Encoder::new(cfg);
    let n = cfg.samples();
    let frames = 60;
    let mut input = Vec::new();
    for i in 0..n * frames {
        let t = i as f32 / cfg.rate as f32;
        let v = (libm::sinf(2.0 * core::f32::consts::PI * freq * t) * 12000.0) as i16;
        input.push(v);
        input.push(v / 2);
    }
    let mut st = [[0f32; 160]; 2];
    let mut out = Vec::new();
    for f in 0..frames {
        let fr = enc.encode(&input[f * n * 2..(f + 1) * n * 2]);
        assert_eq!(fr.len(), cfg.frame_len());
        let (c2, pcm) = crate::sbc::decode(&fr, &mut st).expect("our frame decodes");
        assert_eq!(c2, cfg);
        out.extend(pcm);
    }
    // find the filterbank delay, then the error on the left channel
    let l_in: Vec<f32> = input.iter().step_by(2).map(|&x| x as f32).collect();
    let l_out: Vec<f32> = out.iter().step_by(2).map(|&x| x as f32).collect();
    let mut best = (f32::MAX, 0usize);
    for d in 0..200 {
        let e: f32 = (1000..l_in.len() - 300).map(|i| (l_out[i + d] - l_in[i]).powi(2)).sum();
        if e < best.0 {
            best = (e, d);
        }
    }
    let sig: f32 = (1000..l_in.len() - 300).map(|i| l_in[i].powi(2)).sum();
    10.0 * libm::log10f(sig / best.0)
}

#[test]
fn sbc_round_trip_quality() {
    let cfg = crate::sbc::Config::default();
    let snr = snr_round_trip(cfg, 1000.0);
    assert!(snr > 25.0, "SBC round trip SNR {snr} dB");
    let snr = snr_round_trip(crate::sbc::Config { rate: 44100, mode: crate::sbc::Mode::Joint, alloc: crate::sbc::Alloc::Snr, blocks: 16, bitpool: 35 }, 440.0);
    assert!(snr > 20.0, "joint SNR {snr} dB");
}

#[test]
fn sbc_frame_lengths() {
    // 48 kHz stereo, 16 blocks, 8 subbands, bitpool 53: the "high quality" 118-byte frame
    assert_eq!(crate::sbc::Config::default().frame_len(), 118);
    let c = crate::sbc::Config { rate: 44100, mode: crate::sbc::Mode::Joint, alloc: crate::sbc::Alloc::Loudness, blocks: 16, bitpool: 53 };
    assert_eq!(c.frame_len(), 119);
}

#[test]
fn sbc_corrupt_frame_is_rejected() {
    let mut enc = crate::sbc::Encoder::new(crate::sbc::Config::default());
    let mut fr = enc.encode(&vec![1000i16; 256]);
    fr[5] ^= 0x10; // a scale factor
    let mut st = [[0f32; 160]; 2];
    assert!(crate::sbc::decode(&fr, &mut st).is_none());
}

#[test]
fn avdtp_source_flow() {
    use crate::avdtp::*;
    let mut s = Session::new(48000);
    let ev = s.start();
    let Event::Send(m) = &ev[0] else { panic!() };
    assert_eq!(m[1], SIG_DISCOVER);
    let label = m[0] >> 4;
    // one sink SEP (seid 2), audio
    let ev = s.input(&[(label << 4) | 2, SIG_DISCOVER, 2 << 2, 0x08]);
    let Event::Send(m) = &ev[0] else { panic!() };
    assert_eq!((m[1], m[2]), (SIG_GET_CAPS, 2 << 2));
    let label = m[0] >> 4;
    let ev = s.input(&[(label << 4) | 2, SIG_GET_CAPS, 0x01, 0x00, 0x07, 0x06, 0x00, 0x00, 0xFF, 0xFF, 2, 250]);
    let Event::Send(m) = &ev[0] else { panic!() };
    assert_eq!(m[1], SIG_SET_CONFIG);
    assert_eq!(&m[10..14], &[0x12, 0x15, 53, 53]);
    let label = m[0] >> 4;
    let ev = s.input(&[(label << 4) | 2, SIG_SET_CONFIG]);
    let Event::Send(m) = &ev[0] else { panic!() };
    let label = m[0] >> 4;
    assert_eq!(s.input(&[(label << 4) | 2, SIG_OPEN]), vec![Event::OpenMedia]);
    let ev = s.media_opened();
    let Event::Send(m) = &ev[0] else { panic!() };
    let label = m[0] >> 4;
    let ev = s.input(&[(label << 4) | 2, SIG_START]);
    assert!(matches!(ev[0], Event::Streaming(c) if c.rate == 48000 && c.bitpool == 53));
    // the headset suspends
    let ev = s.input(&[(5 << 4) | 0, SIG_SUSPEND, 2 << 2]);
    assert_eq!(ev[1], Event::Suspended);
    assert_eq!(media(1, 0, &[0x9C], 1)[..13], [0x80, 0x60, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 1]);
}

// ── classic HID gamepads (Switch Pro / DualShock personalities) ─────────────

#[test]
fn classic_gamepad_pairs_reads_its_descriptor_and_reports() {
    let mut s = Sim::up(Store::default());
    let hnd = 0x0042;
    s.pair_to_io(&DEV, 0x00_2508, hnd); // peripheral / gamepad
    s.ev(evt(0x32, &with_addr(&DEV, &[3, 0, 0])));
    s.ev(evt(0x33, &with_addr(&DEV, &1u32.to_le_bytes())));
    assert!(s.has_cmd(op::USER_CONFIRM_REPLY), "a pad cannot type: just works is allowed");
    s.finish_pairing(&DEV, hnd, store::KEY_UNAUTH_P256);
    s.encrypt(hnd, 2, 16);
    let pad_desc = [0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x3F, 0xC0];
    sdp_up(&mut s, hnd, &pad_desc);
    let b = s.h.store.bond(&DEV).unwrap().clone();
    assert!(b.gamepad && !b.allow_kbd && b.hid_desc == pad_desc);
    hid_up(&mut s, hnd);
    assert!(s.acls().iter().any(|a| hci::parse_acl(a).unwrap().data.ends_with(&crate::hidp::SET_PROTOCOL_REPORT)), "report protocol for a pad");
    assert!(s.out.iter().any(|o| matches!(o, Output::PadAttach { addr, map, le: false, .. } if *addr == DEV && map[..] == pad_desc)));
    // a 0x3F report: to the pad layer, never the keyboard
    s.acl_in(hnd, &l2cap::frame(0x0042, &[0xA1, 0x3F, 1, 2, 8, 0, 0x80, 0, 0x80, 0, 0x80, 0, 0x80]));
    assert!(s.out.contains(&Output::PadReport { addr: DEV, data: vec![0x3F, 1, 2, 8, 0, 0x80, 0, 0x80, 0, 0x80, 0, 0x80] }));
    s.acl_in(hnd, &l2cap::frame(0x0042, &[0xA1, 1, 0, 0, 4, 0, 0, 0, 0, 0]));
    assert!(!s.out.iter().any(|o| matches!(o, Output::Input(..))));
    // its init / rumble packets go out as output reports on the interrupt channel
    assert!(s.h.pad_output(&DEV, &[0x01, 0x00, 0x03, 0x30]));
    s.take();
    let last = s.acls().last().cloned().unwrap();
    assert!(hci::parse_acl(&last).unwrap().data.ends_with(&[0xA2, 0x01, 0x00, 0x03, 0x30]));
    // gone: detached
    s.ev(evt(0x05, &[0, 0x42, 0, 0x13]));
    assert!(s.out.contains(&Output::PadDetach(DEV)));
    // reconnecting later needs no SDP: the descriptor is stored (and sealed with the rest)
    let t = s.h.store.to_text();
    assert!(Store::parse(&t).bond(&DEV).unwrap().hid_desc == pad_desc);
}
