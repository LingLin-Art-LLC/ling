//! The Bluetooth host: a sans-IO state machine. The kernel feeds it HCI
//! events and ACL packets ([`Host::on_event`], [`Host::on_acl`]) and the
//! clock ([`Host::tick`]); it answers with [`Output`]s: commands and ACL data
//! to send, input reports, notices for the user, "the store changed".
//!
//! Security policy (all enforced here, all tested):
//! - Nothing is sent before [`Host::start`]; the kernel calls it only while a
//!   user is signed in with the screen unlocked and their Bluetooth switch on,
//!   and [`Host::stop`] (disconnect everything, scans off, HCI Reset) on lock
//!   or sign-out.
//! - Secure Connections only: SSP with Write Secure Connections Host Support;
//!   PIN (legacy) pairing is refused; only P-256 link keys (types 7 / 8) are
//!   kept; a link must encrypt with AES-CCM (mode 2) with a 16-byte key (KNOB)
//!   before any profile channel opens; a bond made authenticated is never
//!   accepted unauthenticated again (BIAS-style downgrade).
//! - Keyboards must pair authenticated (numeric comparison / passkey, MITM);
//!   just-works is allowed only for devices that cannot type (headphones,
//!   mice), and only when the user pressed Pair on that device.
//! - We authenticate every link ourselves (also when the device connected to
//!   us), never allow role switches, and accept incoming links only from
//!   bonded, unblocked addresses -- or from anyone while the user has the
//!   pairing window open (time-limited, also the only time we are
//!   discoverable).
//! - Input: a bond carries the input kinds approved at pairing (keyboard only
//!   with an authenticated key); other reports are blocked with a notice, and
//!   a device whose Class of Device changed since pairing is held until the
//!   user approves it again.
//! - Block list: blocked addresses (and name patterns, weaker) are rejected at
//!   Connection Request, hidden from scans, disconnected, their bonds deleted.
//! - Limits: connections, channels, reassembly size, scan list size, incoming
//!   connection attempts per device and per minute.

use crate::avdtp;
use crate::hci::{self, op, Event};
use crate::hidp;
use crate::l2cap::{self, L2cap, Note};
use crate::store::{self, Store};
use crate::{addr_str, BdAddr};
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[path = "host_le.rs"]
mod le;

pub const MAX_CONNS: usize = 4;
pub const MAX_SEEN: usize = 64;
/// Pairing window (discoverable + connectable to unknown devices).
pub const PAIRABLE_MS: u64 = 120_000;
/// A numeric comparison waits this long for the user.
pub const CONFIRM_MS: u64 = 60_000;
/// Incoming connection attempts: per device per minute / all per minute.
pub const MAX_ATTEMPTS_PER_DEV: u32 = 4;
pub const MAX_ATTEMPTS_ALL: u32 = 20;
const CMD_TIMEOUT_MS: u64 = 8_000;

/// Our IO capability: we can show a number and take yes / no.
const IO_DISPLAY_YES_NO: u8 = 0x01;
const IO_KEYBOARD_ONLY: u8 = 0x02;
const IO_NO_IO: u8 = 0x03;
const IO_DISPLAY_ONLY: u8 = 0x00;

#[derive(Debug, PartialEq, Clone)]
pub enum Output {
    Cmd(Vec<u8>),
    Acl(Vec<u8>),
    /// HID input from an approved device.
    Input(BdAddr, hidp::Report),
    /// Something the user should see (also written to the log).
    Notice(String),
    /// A notice about one device (the desktop offers Block / Allow for it).
    DeviceNotice(BdAddr, String),
    /// The store (bonds / block list) changed: save it.
    StoreChanged,
    /// An A2DP stream is ready for media packets ([`Host::send_media`]).
    AudioStart(BdAddr, crate::sbc::Config, u16),
    AudioStop(BdAddr),
    /// A gamepad is ready: its HID report descriptor and USB-style ids (for drivers::hidpad::identify).
    PadAttach { addr: BdAddr, map: Vec<u8>, vid: u16, pid: u16, name: String, le: bool },
    /// One input report from it (starting with the report id when the descriptor uses ids).
    PadReport { addr: BdAddr, data: Vec<u8> },
    PadDetach(BdAddr),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    Off,
    Init,
    Ready,
    /// The controller cannot do what we require (no Secure Connections...).
    Failed,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Seen {
    /// Classic: the device address; LE: its identity when an IRK resolved it, else the advertised address.
    pub addr: BdAddr,
    pub name: Option<String>,
    pub cod: u32,
    pub rssi: Option<i8>,
    pub le: bool,
    pub pscan_rep_mode: u8,
    pub clock_offset: Option<u16>,
    pub blocked: bool,
    /// LE: GAP appearance, the address (and type) to connect to now, connectable advertising seen.
    pub appearance: u16,
    pub conn_addr: BdAddr,
    pub conn_type: u8,
    pub connectable: bool,
}

/// What the user is asked during a pairing.
#[derive(Clone, Debug, PartialEq)]
pub enum Prompt {
    /// Numeric comparison: does the device show this number?
    Confirm { addr: BdAddr, value: u32, until: u64 },
    /// Passkey entry: type this on the keyboard, then Enter.
    Passkey { addr: BdAddr, passkey: u32 },
}

#[derive(Clone, Debug)]
pub struct Conn {
    pub handle: u16,
    pub addr: BdAddr,
    /// We created it (vs. the device paged us).
    pub outgoing: bool,
    /// The user pressed Pair for this device.
    pub pairing: bool,
    pub cod: u32,
    remote_io: Option<u8>,
    /// The key type in use on this link (from the stored bond or a fresh pairing).
    key_type: Option<u8>,
    auth_sent: bool,
    authenticated: bool,
    enc_mode: u8,
    /// Every gate passed: SC key, our authentication, AES-CCM, 16-byte key.
    pub secure: bool,
    /// The Class of Device differs from the one seen at pairing.
    pub cod_changed: bool,
    hid_ctrl: Option<u16>,
    hid_intr: Option<u16>,
    hid_ready: bool,
    blocked_kbd_noted: bool,
    blocked_ptr_noted: bool,
    /// L2CAP requests that arrived before the link was secure: answered "pending".
    waiting: Vec<(u8, u16, u16)>,
    avdtp_sig: Option<u16>,
    avdtp_media: Option<u16>,
    av: Option<avdtp::Session>,
    closing: bool,
    /// Classic HID: the SDP query for the report descriptor (its channel and client).
    sdp: Option<(u16, crate::sdp::Client)>,
    pad_attached: bool,
}

pub struct Host {
    pub state: State,
    pub store: Store,
    pub local_addr: BdAddr,
    pub seen: Vec<Seen>,
    pub conns: Vec<Conn>,
    pub prompt: Option<Prompt>,
    pub scanning: bool,
    scan_until: u64,
    pub pairable_until: u64,
    /// The device a Pair / Connect is in progress for.
    connecting: Option<(BdAddr, bool, u64)>,
    l2: L2cap,
    asm: l2cap::Reassembler,
    cmd_q: VecDeque<Vec<u8>>,
    inflight: Option<(u16, u64)>,
    acl_mtu: usize,
    acl_credits: u16,
    acl_q: VecDeque<Vec<u8>>,
    names_todo: Vec<BdAddr>,
    /// Connection requests we accepted (a Connection Complete must match one, or our Create Connection).
    accepted: Vec<(BdAddr, u64)>,
    attempts: Vec<(BdAddr, u64, u32)>,
    attempts_all: (u64, u32),
    now: u64,
    out: Vec<Output>,
    le_random: BdAddr,
    page_scan: Option<(bool, bool)>,
    /// The mixer's rate (A2DP picks 48 or 44.1 kHz to match).
    pub audio_rate: u32,
    media_seq: u16,
    le_links: Vec<le::LeLink>,
    le_connecting: Option<le::Connecting>,
    bg_scan: bool,
    reconnect_at: Vec<(BdAddr, u64)>,
    le_mtu: usize,
    drbg: crate::smp::Drbg,
}

/// A classic device whose HID channels we open / accept: a peripheral by
/// class, a bonded pad, or one with input rights.
fn hid_capable(b: &store::Bond) -> bool {
    !b.le && (b.allow_kbd || b.allow_ptr || b.gamepad || (b.cod >> 8) & 0x1F == 5)
}

fn ok(ret: &[u8]) -> bool {
    ret.first() == Some(&0)
}

impl Default for Host {
    fn default() -> Self {
        Host::new(Store::default())
    }
}

impl Host {
    pub fn new(store: Store) -> Host {
        Host {
            state: State::Off,
            store,
            local_addr: [0; 6],
            seen: Vec::new(),
            conns: Vec::new(),
            prompt: None,
            scanning: false,
            scan_until: 0,
            pairable_until: 0,
            connecting: None,
            l2: L2cap::default(),
            asm: l2cap::Reassembler::default(),
            cmd_q: VecDeque::new(),
            inflight: None,
            acl_mtu: 310,
            acl_credits: 0,
            acl_q: VecDeque::new(),
            names_todo: Vec::new(),
            accepted: Vec::new(),
            attempts: Vec::new(),
            attempts_all: (0, 0),
            now: 0,
            out: Vec::new(),
            le_random: [0; 6],
            page_scan: None,
            audio_rate: 48000,
            media_seq: 0,
            le_links: Vec::new(),
            le_connecting: None,
            bg_scan: false,
            reconnect_at: Vec::new(),
            le_mtu: 27,
            drbg: crate::smp::Drbg::new(&[0x5A; 32]),
        }
    }

    pub fn take_output(&mut self) -> Vec<Output> {
        core::mem::take(&mut self.out)
    }

    fn notice(&mut self, s: String) {
        self.out.push(Output::Notice(s));
    }

    fn dev_notice(&mut self, a: &BdAddr, s: String) {
        self.out.push(Output::DeviceNotice(*a, s));
    }

    fn cmd(&mut self, pkt: Vec<u8>) {
        if self.cmd_q.len() < 64 {
            self.cmd_q.push_back(pkt);
        }
        self.pump();
    }

    fn pump(&mut self) {
        if self.inflight.is_some() {
            return;
        }
        if let Some(p) = self.cmd_q.pop_front() {
            self.inflight = Some((hci::command_opcode(&p).unwrap_or(0), self.now));
            self.out.push(Output::Cmd(p));
        }
    }

    fn name_of(&self, a: &BdAddr) -> String {
        if let Some(b) = self.store.bond(a) {
            if !b.name.is_empty() {
                return b.name.clone();
            }
        }
        self.seen.iter().find(|s| &s.addr == a).and_then(|s| s.name.clone()).unwrap_or_else(|| addr_str(a))
    }

    fn known_name(&self, a: &BdAddr) -> Option<String> {
        self.store.bond(a).map(|b| b.name.clone()).filter(|n| !n.is_empty()).or_else(|| self.seen.iter().find(|s| &s.addr == a).and_then(|s| s.name.clone()))
    }

    fn blocked(&self, a: &BdAddr) -> bool {
        self.store.is_blocked(a, self.known_name(a).as_deref())
    }

    // ── power ───────────────────────────────────────────────────────────────

    /// Seed the generator for LE pairing keys and nonces (32 bytes from the kernel's RNG).
    pub fn seed(&mut self, s: &[u8; 32]) {
        self.drbg = crate::smp::Drbg::new(s);
    }

    /// Bring the controller up. `random` seeds our LE private address.
    pub fn start(&mut self, now: u64, random: [u8; 6]) {
        self.now = now;
        self.state = State::Init;
        self.le_random = hci::nrpa(random);
        self.page_scan = None;
        for p in [
            hci::reset(),
            hci::command(op::READ_LOCAL_VERSION, &[]),
            hci::command(op::READ_BD_ADDR, &[]),
            hci::command(op::READ_BUFFER_SIZE, &[]),
            hci::set_event_mask(hci::event_mask()),
            hci::command(op::WRITE_SSP_MODE, &[1]),
            hci::command(op::WRITE_SC_HOST_SUPPORT, &[1]),
            hci::command(op::WRITE_LE_HOST_SUPPORTED, &[1, 0]),
            // sniff allowed, role switch not
            hci::command(op::WRITE_DEFAULT_LINK_POLICY, &0x0004u16.to_le_bytes()),
            hci::command(op::WRITE_INQUIRY_MODE, &[2]),
            hci::write_class_of_device(0x00_0104),
            hci::write_local_name("LingOS"),
            hci::command(op::WRITE_PAGE_TIMEOUT, &0x2000u16.to_le_bytes()),
            hci::le_set_event_mask(),
            hci::command(op::LE_READ_BUFFER_SIZE, &[]),
            hci::le_set_random_address(&self.le_random),
        ] {
            self.cmd(p);
        }
    }

    /// Everything off: links dropped, scans off, the controller reset. The
    /// kernel sends these and then stops feeding the host.
    pub fn stop(&mut self) -> Vec<Vec<u8>> {
        let mut v = Vec::new();
        for c in &self.conns {
            v.push(hci::disconnect(c.handle, 0x15)); // power off
        }
        v.push(hci::write_scan_enable(false, false));
        v.push(hci::le_set_scan_enable(false));
        v.push(hci::reset());
        self.state = State::Off;
        self.conns.clear();
        self.l2 = L2cap::default();
        self.asm = l2cap::Reassembler::default();
        self.cmd_q.clear();
        self.inflight = None;
        self.acl_q.clear();
        self.prompt = None;
        self.scanning = false;
        self.pairable_until = 0;
        self.connecting = None;
        self.le_links.clear();
        self.le_connecting = None;
        self.bg_scan = false;
        self.out.clear();
        v
    }

    /// Page / inquiry scan for the current policy: connectable when there are
    /// bonds (unknown addresses are rejected at Connection Request) or the
    /// pairing window is open; discoverable only in the pairing window.
    fn update_scan(&mut self) {
        if self.state != State::Ready {
            return;
        }
        let pairable = self.now < self.pairable_until;
        let want = (pairable, pairable || !self.store.bonds.is_empty());
        if self.page_scan != Some(want) {
            self.page_scan = Some(want);
            self.cmd(hci::write_scan_enable(want.0, want.1));
        }
        self.le_update_scan();
    }

    // ── user actions ────────────────────────────────────────────────────────

    pub fn scan(&mut self, secs: u32) -> Result<(), String> {
        if self.state != State::Ready {
            return Err(String::from("Bluetooth is not on"));
        }
        if self.scanning {
            return Ok(());
        }
        self.seen.retain(|s| self.store.bond(&s.addr).is_some());
        if self.bg_scan {
            self.cmd(hci::le_set_scan_enable(false));
            self.bg_scan = false;
        }
        self.scanning = true;
        self.scan_until = self.now + secs.clamp(2, 30) as u64 * 1000 + 2000;
        let len = ((secs.clamp(2, 30) as u64 * 100) / 128).max(1) as u8;
        self.cmd(hci::inquiry(len));
        self.cmd(hci::le_set_scan_params());
        self.cmd(hci::le_set_scan_enable(true));
        Ok(())
    }

    fn scan_done(&mut self) {
        if !self.scanning {
            return;
        }
        self.scanning = false;
        self.cmd(hci::le_set_scan_enable(false));
        self.le_update_scan();
        // names for classic devices that did not send one in their EIR
        self.names_todo = self.seen.iter().filter(|s| !s.le && s.name.is_none() && !s.blocked).map(|s| s.addr).take(6).collect();
        self.next_name();
    }

    fn next_name(&mut self) {
        if let Some(a) = self.names_todo.pop() {
            let (m, co) = self.seen.iter().find(|s| s.addr == a).map(|s| (s.pscan_rep_mode, s.clock_offset)).unwrap_or((1, None));
            self.cmd(hci::remote_name_request(&a, m, co));
        }
    }

    /// Open the pairing window: discoverable + connectable for unknown devices.
    pub fn set_pairable(&mut self, on: bool) {
        self.pairable_until = if on { self.now + PAIRABLE_MS } else { 0 };
        self.update_scan();
    }

    /// Pair with a device from the scan (or connect to a bonded one).
    pub fn pair(&mut self, a: &BdAddr) -> Result<(), String> {
        if self.state != State::Ready {
            return Err(String::from("Bluetooth is not on"));
        }
        if self.blocked(a) {
            return Err(format!("{} is blocked (Unblock it first)", self.name_of(a)));
        }
        let s = self.seen.iter().find(|s| &s.addr == a).cloned();
        if self.conns.iter().any(|c| &c.addr == a) {
            return Err(String::from("already connected"));
        }
        if self.conns.len() >= MAX_CONNS || self.connecting.is_some() || self.le_connecting.is_some() {
            return Err(String::from("busy: another connection is being set up"));
        }
        if s.as_ref().map(|s| s.le).unwrap_or_else(|| self.store.bond(a).is_some_and(|b| b.le)) {
            return self.le_pair(a);
        }
        let pairing = self.store.bond(a).is_none();
        if self.scanning {
            self.cmd(hci::command(op::INQUIRY_CANCEL, &[]));
            self.scan_done();
        }
        let (m, co) = s.map(|s| (s.pscan_rep_mode, s.clock_offset)).unwrap_or((1, None));
        self.connecting = Some((*a, pairing, self.now));
        self.cmd(hci::create_connection(a, m, co));
        Ok(())
    }

    pub fn disconnect(&mut self, a: &BdAddr) -> bool {
        let hs: Vec<u16> = self.conns.iter().filter(|c| &c.addr == a).map(|c| c.handle).collect();
        for h in &hs {
            self.cmd(hci::disconnect(*h, 0x13));
        }
        !hs.is_empty()
    }

    /// Remove a bond: the device is told (HID virtual cable unplug), the link
    /// dropped, the key deleted.
    pub fn remove(&mut self, a: &BdAddr) -> bool {
        if let Some(c) = self.conns.iter().find(|c| &c.addr == a) {
            if let Some(cid) = c.hid_ctrl {
                if let Some(f) = self.l2.send(cid, &hidp::VIRTUAL_CABLE_UNPLUG) {
                    self.acl(f);
                }
            }
        }
        self.disconnect(a);
        let r = self.store.remove_bond(a);
        if r {
            self.out.push(Output::StoreChanged);
            self.update_scan();
        }
        r
    }

    pub fn block(&mut self, a: &BdAddr) -> Result<bool, String> {
        let name = self.known_name(a).unwrap_or_default();
        self.disconnect(a);
        let r = self.store.block(a, &name)?;
        for s in self.seen.iter_mut().filter(|s| &s.addr == a) {
            s.blocked = true;
        }
        self.out.push(Output::StoreChanged);
        self.update_scan();
        Ok(r)
    }

    pub fn unblock(&mut self, a: &BdAddr) -> bool {
        let r = self.store.unblock(a);
        if r {
            let st = self.store.clone();
            for s in self.seen.iter_mut().filter(|s| &s.addr == a) {
                s.blocked = st.is_blocked(&s.addr, s.name.as_deref());
            }
            self.out.push(Output::StoreChanged);
        }
        r
    }

    pub fn block_name(&mut self, pattern: &str) -> bool {
        if !self.store.block_name(pattern) {
            return false;
        }
        let st = self.store.clone();
        let mut drop = Vec::new();
        for s in self.seen.iter_mut() {
            s.blocked = st.is_blocked(&s.addr, s.name.as_deref());
        }
        for c in &self.conns {
            if st.is_blocked(&c.addr, self.known_name(&c.addr).as_deref()) {
                drop.push(c.addr);
            }
        }
        for a in drop {
            self.disconnect(&a);
        }
        self.out.push(Output::StoreChanged);
        true
    }

    pub fn unblock_name(&mut self, pattern: &str) -> bool {
        let r = self.store.unblock_name(pattern);
        if r {
            let st = self.store.clone();
            for s in self.seen.iter_mut() {
                s.blocked = st.is_blocked(&s.addr, s.name.as_deref());
            }
            self.out.push(Output::StoreChanged);
        }
        r
    }

    /// The user's answer to a numeric comparison.
    pub fn confirm(&mut self, yes: bool) -> bool {
        let Some(Prompt::Confirm { addr, .. }) = self.prompt.clone() else { return false };
        self.prompt = None;
        if self.le_confirm(&addr, yes).is_none() {
            self.cmd(hci::user_confirm_reply(&addr, yes));
        }
        true
    }

    /// Approve input from a bonded device (keyboard needs an authenticated bond).
    pub fn allow_input(&mut self, a: &BdAddr, kbd: bool, ptr: bool) -> Result<(), String> {
        let cod_now = self.conns.iter().find(|c| &c.addr == a).map(|c| c.cod);
        let Some(b) = self.store.bond_mut(a) else { return Err(String::from("not paired")) };
        if kbd && !b.authenticated() {
            return Err(String::from("keyboard input needs a pairing confirmed with a code: Remove the device and pair it again"));
        }
        b.allow_kbd |= kbd;
        b.allow_ptr |= ptr;
        if let Some(c) = cod_now.filter(|c| *c != 0) {
            b.cod = c;
        }
        self.le_approve(a);
        for c in self.conns.iter_mut().filter(|c| &c.addr == a) {
            c.cod_changed = false;
            c.blocked_kbd_noted = false;
            c.blocked_ptr_noted = false;
        }
        self.out.push(Output::StoreChanged);
        Ok(())
    }

    // ── the clock ───────────────────────────────────────────────────────────

    pub fn tick(&mut self, now: u64) {
        self.now = now;
        if self.state == State::Off {
            return;
        }
        if let Some((op_, t)) = self.inflight {
            if now.saturating_sub(t) > CMD_TIMEOUT_MS {
                self.inflight = None;
                self.notice(format!("Bluetooth: command {op_:#06x} got no answer"));
                if self.state == State::Init {
                    self.state = State::Failed;
                    self.notice(String::from("Bluetooth: the controller stopped answering during start-up"));
                }
                self.pump();
            }
        }
        if self.scanning && now > self.scan_until {
            self.scan_done();
        }
        if self.pairable_until != 0 && now >= self.pairable_until {
            self.pairable_until = 0;
            self.update_scan();
        }
        if let Some(Prompt::Confirm { addr, until, .. }) = self.prompt.clone() {
            if now >= until {
                self.prompt = None;
                if self.le_confirm(&addr, false).is_none() {
                    self.cmd(hci::user_confirm_reply(&addr, false));
                }
                self.notice(format!("pairing with {} timed out (the number was not confirmed)", self.name_of(&addr)));
            }
        }
        if let Some((a, _, t)) = self.connecting {
            if now.saturating_sub(t) > 30_000 {
                self.connecting = None;
                self.notice(format!("{}: no answer", self.name_of(&a)));
            }
        }
        self.attempts.retain(|x| now.saturating_sub(x.1) < 60_000);
        self.le_tick();
        self.accepted.retain(|x| now.saturating_sub(x.1) < 30_000);
    }

    // ── events ──────────────────────────────────────────────────────────────

    pub fn on_event(&mut self, pkt: &[u8]) {
        if self.state == State::Off {
            return; // nothing is acted on while off
        }
        let Some(e) = hci::parse_event(pkt) else { return };
        match e {
            Event::CommandComplete { opcode, ret, .. } => self.on_complete(opcode, &ret),
            Event::CommandStatus { status, opcode, .. } => {
                if self.inflight.is_some_and(|(o, _)| o == opcode) {
                    self.inflight = None;
                }
                if status != 0 {
                    match opcode {
                        op::CREATE_CONNECTION => {
                            if let Some((a, _, _)) = self.connecting.take() {
                                self.notice(format!("could not connect to {} (error {status:#04x})", self.name_of(&a)));
                            }
                        },
                        op::REMOTE_NAME_REQUEST => self.next_name(),
                        op::INQUIRY => self.scan_done(),
                        op::LE_CREATE_CONNECTION => self.le_create_failed(status),
                        _ => {},
                    }
                }
                self.pump();
            },
            Event::InquiryResult(v) => {
                for f in v {
                    self.saw(f.addr, f.name, f.cod, f.rssi, false, f.pscan_rep_mode, Some(f.clock_offset));
                }
            },
            Event::LeAdvertisingReport(v) => {
                for r in v {
                    self.le_adv(r);
                }
            },
            Event::LeConnectionComplete { status, handle, role, peer_type, peer, .. } => self.le_connected(status, handle, role, peer_type, peer),
            Event::LeRemoteParamRequest { handle, min, max, latency, timeout } => self.le_param_request(handle, min, max, latency, timeout),
            Event::InquiryComplete { .. } => self.scan_done(),
            Event::RemoteNameComplete { status, addr, name } => {
                if status == 0 && !name.is_empty() {
                    let st = self.store.clone();
                    if let Some(s) = self.seen.iter_mut().find(|s| s.addr == addr) {
                        s.blocked = st.is_blocked(&addr, Some(&name));
                        s.name = Some(name);
                    }
                }
                self.next_name();
            },
            Event::ConnectionRequest { addr, cod, link_type } => self.on_conn_request(addr, cod, link_type),
            Event::ConnectionComplete { status, handle, addr, link_type, .. } => self.on_conn_complete(status, handle, addr, link_type),
            Event::DisconnectionComplete { status, handle, reason } => {
                if status == 0 {
                    self.link_gone(handle, reason);
                }
            },
            Event::PinCodeRequest(a) => {
                self.cmd(hci::pin_code_neg_reply(&a));
                self.notice(format!("{} only supports insecure (legacy PIN) pairing: refused", self.name_of(&a)));
                self.drop_addr(&a, 0x05);
            },
            Event::LinkKeyRequest(a) => {
                let pairing = self.conns.iter().any(|c| c.addr == a && c.pairing);
                match self.store.bond(&a).filter(|_| !pairing && !self.blocked(&a)).cloned() {
                    Some(b) => {
                        for c in self.conns.iter_mut().filter(|c| c.addr == a) {
                            c.key_type = Some(b.key_type);
                        }
                        self.cmd(hci::link_key_reply(&a, &b.key));
                    },
                    // no bond (or a re-pair the user asked for): a fresh key
                    None => self.cmd(hci::link_key_neg_reply(&a)),
                }
            },
            Event::IoCapabilityRequest(a) => self.on_io_cap_request(a),
            Event::IoCapabilityResponse { addr, io_cap, .. } => {
                for c in self.conns.iter_mut().filter(|c| c.addr == addr) {
                    c.remote_io = Some(io_cap);
                }
            },
            Event::UserConfirmationRequest { addr, value } => self.on_confirm_request(addr, value),
            Event::UserPasskeyRequest(a) => {
                // we never asked to type a passkey (DisplayYesNo)
                self.cmd(hci::command(0x042F, &a));
            },
            Event::UserPasskeyNotification { addr, passkey } => {
                self.prompt = Some(Prompt::Passkey { addr, passkey });
                self.notice(format!("type {passkey:06} on {} and press Enter", self.name_of(&addr)));
            },
            Event::SimplePairingComplete { status, addr } => {
                if matches!(self.prompt, Some(Prompt::Passkey { addr: a, .. }) | Some(Prompt::Confirm { addr: a, .. }) if a == addr) {
                    self.prompt = None;
                }
                if status != 0 {
                    self.notice(format!("pairing with {} failed (error {status:#04x})", self.name_of(&addr)));
                }
            },
            Event::LinkKeyNotification { addr, key, key_type } => self.on_link_key(addr, key, key_type),
            Event::AuthenticationComplete { status, handle } => {
                let Some(i) = self.conns.iter().position(|c| c.handle == handle) else { return };
                if status != 0 {
                    let a = self.conns[i].addr;
                    self.notice(format!("{}: authentication failed (error {status:#04x})", self.name_of(&a)));
                    self.drop_handle(handle, 0x05);
                    return;
                }
                if !self.conns[i].auth_sent || self.conns[i].key_type.is_none() {
                    // authentication we did not ask for, or with no key we know: not trusted
                    self.drop_handle(handle, 0x05);
                    return;
                }
                self.conns[i].authenticated = true;
                if self.conns[i].enc_mode == 2 {
                    // the link encrypted (AES-CCM) before our authentication finished: check its key now
                    self.cmd(hci::read_enc_key_size(handle));
                } else {
                    self.cmd(hci::set_conn_encryption(handle, true));
                }
            },
            Event::EncryptionChange { status, handle, mode } => {
                if self.is_le(handle) {
                    self.le_encryption(status, handle, mode);
                } else {
                    self.on_encryption(status, handle, mode);
                }
            },
            Event::EncryptionKeyRefresh { status, handle } => {
                if status != 0 {
                    self.drop_handle(handle, 0x05);
                }
            },
            Event::NumCompletedPackets(v) => {
                for (_, n) in v {
                    self.acl_credits = self.acl_credits.saturating_add(n);
                }
                self.flush_acl();
            },
            Event::HardwareError(code) => {
                self.notice(format!("Bluetooth controller hardware error {code:#04x}"));
                self.state = State::Failed;
            },
            _ => {},
        }
    }

    fn on_complete(&mut self, opcode: u16, ret: &[u8]) {
        if self.inflight.is_some_and(|(o, _)| o == opcode) {
            self.inflight = None;
        }
        match opcode {
            op::RESET | op::WRITE_SSP_MODE | op::WRITE_SC_HOST_SUPPORT if self.state == State::Init && !ok(ret) => {
                self.state = State::Failed;
                self.cmd_q.clear();
                self.notice(format!(
                    "Bluetooth stays off: the controller refused {} (Secure Connections is required)",
                    match opcode {
                        op::RESET => "reset",
                        op::WRITE_SSP_MODE => "Secure Simple Pairing",
                        _ => "Secure Connections",
                    }
                ));
            },
            op::READ_BD_ADDR => {
                if let Some(a) = crate::addr_at(ret, 1) {
                    self.local_addr = a;
                }
            },
            op::READ_BUFFER_SIZE => {
                if ok(ret) {
                    self.acl_mtu = crate::le16(ret, 1).unwrap_or(310).clamp(27, 1021) as usize;
                    self.acl_credits = crate::le16(ret, 4).unwrap_or(4).clamp(1, 64);
                }
            },
            op::LE_READ_BUFFER_SIZE => {
                // LE buffers of their own: LE data is fragmented to their size and the shared
                // credit count never exceeds them
                if ok(ret) {
                    let (mtu, n) = (crate::le16(ret, 1).unwrap_or(0) as usize, ret.get(3).copied().unwrap_or(0) as u16);
                    if mtu >= 27 && n > 0 {
                        self.le_mtu = mtu.min(251);
                        self.acl_credits = self.acl_credits.min(n);
                    } else {
                        self.le_mtu = self.acl_mtu.min(251);
                    }
                }
            },
            op::LE_SET_RANDOM_ADDRESS if self.state == State::Init => {
                self.state = State::Ready;
                self.update_scan();
            },
            op::READ_ENC_KEY_SIZE => self.on_key_size(ret),
            _ => {},
        }
        self.pump();
    }

    #[allow(clippy::too_many_arguments)]
    fn saw(&mut self, addr: BdAddr, name: Option<String>, cod: u32, rssi: Option<i8>, le: bool, rep: u8, clock: Option<u16>) {
        if !self.scanning {
            return;
        }
        let st = &self.store;
        if let Some(s) = self.seen.iter_mut().find(|s| s.addr == addr) {
            if name.is_some() {
                s.name = name;
            }
            if cod != 0 {
                s.cod = cod;
            }
            s.rssi = rssi.or(s.rssi);
            s.blocked = st.is_blocked(&addr, s.name.as_deref());
            return;
        }
        if self.seen.len() >= MAX_SEEN {
            return;
        }
        let blocked = st.is_blocked(&addr, name.as_deref());
        self.seen.push(Seen { addr, name, cod, rssi, le, pscan_rep_mode: rep, clock_offset: clock, blocked, ..Default::default() });
    }

    /// The scan list as the user sees it (blocked devices only with "show blocked").
    pub fn visible(&self) -> Vec<&Seen> {
        self.seen.iter().filter(|s| !s.blocked || self.store.show_blocked).collect()
    }

    fn rate_limited(&mut self, a: &BdAddr) -> bool {
        let now = self.now;
        if now.saturating_sub(self.attempts_all.0) >= 60_000 {
            self.attempts_all = (now, 0);
        }
        self.attempts_all.1 += 1;
        let n = match self.attempts.iter_mut().find(|x| &x.0 == a) {
            Some(x) => {
                x.2 += 1;
                x.2
            },
            None => {
                self.attempts.push((*a, now, 1));
                if self.attempts.len() > 64 {
                    self.attempts.remove(0);
                }
                1
            },
        };
        if n == MAX_ATTEMPTS_PER_DEV + 1 {
            self.dev_notice(a, format!("{} keeps trying to connect: refused for a minute (Block it in Settings > Bluetooth to stop it for good)", self.name_of(a)));
        }
        n > MAX_ATTEMPTS_PER_DEV || self.attempts_all.1 > MAX_ATTEMPTS_ALL
    }

    fn on_conn_request(&mut self, addr: BdAddr, cod: u32, link_type: u8) {
        // the earliest point: blocked devices never get a link
        if self.blocked(&addr) {
            self.cmd(hci::reject_connection(&addr, 0x0F));
            return;
        }
        if self.rate_limited(&addr) {
            self.cmd(hci::reject_connection(&addr, 0x0D));
            return;
        }
        if link_type != 1 {
            // SCO / eSCO (calls): not supported
            self.cmd(hci::reject_connection(&addr, 0x0D));
            return;
        }
        let bonded = self.store.bond(&addr).is_some();
        let pairable = self.now < self.pairable_until;
        if (!bonded && !pairable) || self.conns.len() >= MAX_CONNS {
            self.cmd(hci::reject_connection(&addr, if bonded { 0x0D } else { 0x0F }));
            return;
        }
        self.cmd(hci::accept_connection(&addr));
        self.accepted.retain(|x| x.0 != addr);
        self.accepted.push((addr, self.now));
        // remember the class it shows now (compared with the bond's at completion)
        if let Some(s) = self.seen.iter_mut().find(|s| s.addr == addr) {
            s.cod = cod;
        } else if self.seen.len() < MAX_SEEN {
            self.seen.push(Seen { addr, name: None, cod, rssi: None, le: false, pscan_rep_mode: 1, clock_offset: None, blocked: false, ..Default::default() });
        }
    }

    fn on_conn_complete(&mut self, status: u8, handle: u16, addr: BdAddr, link_type: u8) {
        let mine = self.connecting.filter(|c| c.0 == addr);
        if mine.is_some() {
            self.connecting = None;
        }
        let was_accepted = self.accepted.iter().any(|x| x.0 == addr);
        self.accepted.retain(|x| x.0 != addr);
        if mine.is_none() && !was_accepted {
            // a link nobody asked for: never trusted
            if status == 0 {
                self.cmd(hci::disconnect(handle, 0x13));
            }
            return;
        }
        if status != 0 {
            if mine.is_some() {
                self.notice(format!("could not connect to {} (error {status:#04x})", self.name_of(&addr)));
            }
            return;
        }
        if link_type != 1 || self.blocked(&addr) || self.conns.len() >= MAX_CONNS {
            self.cmd(hci::disconnect(handle, 0x13));
            return;
        }
        let cod = self.seen.iter().find(|s| s.addr == addr).map(|s| s.cod).unwrap_or(0);
        let bond_cod = self.store.bond(&addr).map(|b| b.cod);
        let cod_changed = matches!(bond_cod, Some(b) if cod != 0 && b != 0 && b != cod);
        if cod_changed {
            self.dev_notice(&addr, format!("{} now reports a different device class than when it was paired: its input is held until you approve it in Settings > Bluetooth", self.name_of(&addr)));
        }
        self.conns.push(Conn {
            handle,
            addr,
            outgoing: mine.is_some(),
            pairing: mine.is_some_and(|m| m.1),
            cod: if cod != 0 { cod } else { bond_cod.unwrap_or(0) },
            remote_io: None,
            key_type: None,
            auth_sent: true,
            authenticated: false,
            enc_mode: 0,
            secure: false,
            cod_changed,
            hid_ctrl: None,
            hid_intr: None,
            hid_ready: false,
            blocked_kbd_noted: false,
            blocked_ptr_noted: false,
            waiting: Vec::new(),
            avdtp_sig: None,
            avdtp_media: None,
            av: None,
            closing: false,
            sdp: None,
            pad_attached: false,
        });
        // we always authenticate the link ourselves, whoever connected
        self.cmd(hci::auth_requested(handle));
    }

    fn on_io_cap_request(&mut self, a: BdAddr) {
        let Some(c) = self.conns.iter().find(|c| c.addr == a) else {
            self.cmd(hci::io_cap_neg_reply(&a, 0x18));
            return;
        };
        let allowed = (c.pairing || self.now < self.pairable_until) && !self.blocked(&a);
        if !allowed {
            self.cmd(hci::io_cap_neg_reply(&a, 0x18));
            self.dev_notice(&a, format!("{} tried to pair: refused (open Settings > Bluetooth and press Pair to pair it)", self.name_of(&a)));
            return;
        }
        // MITM protection required, dedicated bonding when we started it
        let auth = if c.outgoing { 0x03 } else { 0x05 };
        self.cmd(hci::io_cap_reply(&a, IO_DISPLAY_YES_NO, auth));
    }

    fn on_confirm_request(&mut self, a: BdAddr, value: u32) {
        let Some(c) = self.conns.iter().find(|c| c.addr == a).cloned() else {
            self.cmd(hci::user_confirm_reply(&a, false));
            return;
        };
        let (kbd, _) = hci::cod_hid(c.cod);
        match c.remote_io {
            Some(IO_DISPLAY_YES_NO) | Some(IO_KEYBOARD_ONLY) => {
                self.prompt = Some(Prompt::Confirm { addr: a, value, until: self.now + CONFIRM_MS });
                self.notice(format!("does {} show {value:06}? Confirm in Settings > Bluetooth (or `bt confirm yes`)", self.name_of(&a)));
            },
            Some(IO_NO_IO) | Some(IO_DISPLAY_ONLY) if !kbd && c.pairing => {
                // just works: only for devices that cannot type, and only the one the user chose
                self.cmd(hci::user_confirm_reply(&a, true));
            },
            _ => {
                self.cmd(hci::user_confirm_reply(&a, false));
                self.notice(format!("{} cannot confirm a pairing code; refused{}", self.name_of(&a), if kbd { " (keyboards must pair with a code)" } else { "" }));
            },
        }
    }

    fn on_link_key(&mut self, a: BdAddr, key: [u8; 16], key_type: u8) {
        let Some(c) = self.conns.iter().find(|c| c.addr == a).cloned() else { return };
        let name = self.name_of(&a);
        if !store::key_type_ok(key_type) {
            self.notice(format!("{name} only supports insecure pairing (no Secure Connections): refused"));
            self.drop_addr(&a, 0x05);
            return;
        }
        let (kbd, ptr) = hci::cod_hid(c.cod);
        if kbd && key_type != store::KEY_AUTH_P256 {
            self.notice(format!("{name}: a keyboard must pair with a confirmed code: refused"));
            self.drop_addr(&a, 0x05);
            return;
        }
        if self.store.bond(&a).is_some_and(|b| b.authenticated()) && key_type != store::KEY_AUTH_P256 {
            self.notice(format!("{name}: refused a weaker pairing than the one it was bonded with"));
            self.drop_addr(&a, 0x05);
            return;
        }
        if !c.pairing && self.now >= self.pairable_until {
            self.drop_addr(&a, 0x05);
            return;
        }
        let bond = store::Bond {
            addr: a,
            key,
            key_type,
            cod: c.cod,
            allow_kbd: kbd && key_type == store::KEY_AUTH_P256,
            allow_ptr: ptr,
            audio: hci::cod_is_audio(c.cod),
            name: self.known_name(&a).unwrap_or_default(),
            ..Default::default()
        };
        self.store.remove_bond(&a);
        if self.store.bonds.len() >= store::MAX_BONDS {
            self.notice(String::from("too many paired devices: remove one first"));
            self.drop_addr(&a, 0x13);
            return;
        }
        self.store.bonds.push(bond);
        for c in self.conns.iter_mut().filter(|c| c.addr == a) {
            c.key_type = Some(key_type);
        }
        self.out.push(Output::StoreChanged);
        self.notice(format!("paired with {name} ({})", store::level_name(key_type)));
        self.update_scan();
    }

    fn on_encryption(&mut self, status: u8, handle: u16, mode: u8) {
        let Some(i) = self.conns.iter().position(|c| c.handle == handle) else { return };
        let a = self.conns[i].addr;
        if status != 0 || mode == 0 {
            if !self.conns[i].closing {
                self.notice(format!("{}: encryption failed or was turned off", self.name_of(&a)));
            }
            self.drop_handle(handle, 0x05);
            return;
        }
        if mode != 2 {
            self.notice(format!("{}: not Secure Connections encryption (AES-CCM): disconnected", self.name_of(&a)));
            self.drop_handle(handle, 0x05);
            return;
        }
        self.conns[i].enc_mode = mode;
        if !self.conns[i].authenticated {
            // encrypted before our own authentication completed: nothing opens until it does
            return;
        }
        self.cmd(hci::read_enc_key_size(handle));
    }

    fn on_key_size(&mut self, ret: &[u8]) {
        let (Some(handle), Some(&size)) = (crate::le16(ret, 1), ret.get(3)) else { return };
        let handle = handle & 0x0FFF;
        let Some(i) = self.conns.iter().position(|c| c.handle == handle) else { return };
        let a = self.conns[i].addr;
        if !ok(ret) || size < 16 {
            self.notice(format!("{}: encryption key of {size} bytes is too short (KNOB): disconnected", self.name_of(&a)));
            self.drop_handle(handle, 0x05);
            return;
        }
        let bonded = self.store.bond(&a).is_some_and(|b| Some(b.key_type) == self.conns[i].key_type);
        if !bonded {
            self.drop_handle(handle, 0x05);
            return;
        }
        let c = &mut self.conns[i];
        c.secure = true;
        // requests that waited for security
        let waiting = core::mem::take(&mut c.waiting);
        let outgoing = c.outgoing;
        for (id, psm, scid) in waiting {
            self.answer_incoming(handle, id, psm, scid);
        }
        if outgoing {
            let b = self.store.bond(&a).cloned();
            let mut out = Vec::new();
            if let Some(b) = b {
                if hid_capable(&b) && b.hid_desc.is_empty() && b.desc_hash == 0 {
                    // first: what is it? (report descriptor + Device ID over SDP)
                    if let Some(cid) = self.l2.connect(handle, l2cap::PSM_SDP, &mut out) {
                        self.conns[i].sdp = Some((cid, crate::sdp::Client::default()));
                    }
                } else if hid_capable(&b) {
                    if let Some(cid) = self.l2.connect(handle, l2cap::PSM_HID_CONTROL, &mut out) {
                        self.conns[i].hid_ctrl = Some(cid);
                    }
                } else if b.audio {
                    if let Some(cid) = self.l2.connect(handle, l2cap::PSM_AVDTP, &mut out) {
                        self.conns[i].avdtp_sig = Some(cid);
                    }
                }
            }
            for f in out {
                self.acl(f);
            }
        }
    }

    fn drop_addr(&mut self, a: &BdAddr, reason: u8) {
        let hs: Vec<u16> = self.conns.iter().filter(|c| &c.addr == a).map(|c| c.handle).collect();
        for h in hs {
            self.drop_handle(h, reason);
        }
    }

    fn drop_handle(&mut self, handle: u16, reason: u8) {
        if let Some(c) = self.conns.iter_mut().find(|c| c.handle == handle) {
            if c.closing {
                return;
            }
            c.closing = true;
            c.secure = false;
        }
        self.cmd(hci::disconnect(handle, reason));
    }

    fn link_gone(&mut self, handle: u16, _reason: u8) {
        self.le_gone(handle);
        let mut notes = Vec::new();
        self.l2.link_down(handle, &mut notes);
        self.asm.link_down(handle);
        if let Some(i) = self.conns.iter().position(|c| c.handle == handle) {
            let c = self.conns.remove(i);
            if c.avdtp_media.is_some() {
                self.out.push(Output::AudioStop(c.addr));
            }
            if c.pad_attached {
                self.out.push(Output::PadDetach(c.addr));
            }
            if c.hid_ready {
                self.notice(format!("{} disconnected", self.name_of(&c.addr)));
            }
            if matches!(self.prompt, Some(Prompt::Passkey { addr, .. }) | Some(Prompt::Confirm { addr, .. }) if addr == c.addr) {
                self.prompt = None;
            }
        }
    }

    // ── ACL / L2CAP ─────────────────────────────────────────────────────────

    fn acl(&mut self, (handle, frame): l2cap::Out) {
        let mtu = if self.is_le(handle) { self.le_mtu } else { self.acl_mtu };
        for p in hci::fragment(handle, &frame, mtu) {
            if self.acl_q.len() >= 256 {
                self.acl_q.pop_front();
            }
            self.acl_q.push_back(p);
        }
        self.flush_acl();
    }

    fn flush_acl(&mut self) {
        while self.acl_credits > 0 {
            let Some(p) = self.acl_q.pop_front() else { break };
            self.acl_credits -= 1;
            self.out.push(Output::Acl(p));
        }
    }

    /// ACL packets waiting for controller buffers.
    pub fn acl_backlog(&self) -> usize {
        self.acl_q.len()
    }

    pub fn on_acl(&mut self, pkt: &[u8]) {
        if self.state != State::Ready {
            return;
        }
        let Some(a) = hci::parse_acl(pkt) else { return };
        // data on a link we do not have is dropped
        if !self.conns.iter().any(|c| c.handle == a.handle) {
            return;
        }
        let Some((cid, payload)) = self.asm.push(a.handle, a.pb, a.data) else { return };
        if self.is_le(a.handle) {
            self.le_frame(a.handle, cid, &payload);
            return;
        }
        let mut out = Vec::new();
        let mut notes = Vec::new();
        self.l2.input(a.handle, cid, &payload, &mut out, &mut notes);
        for f in out {
            self.acl(f);
        }
        for n in notes {
            self.on_note(n);
        }
    }

    fn answer_incoming(&mut self, handle: u16, id: u8, psm: u16, scid: u16) {
        let Some(i) = self.conns.iter().position(|c| c.handle == handle) else { return };
        let a = self.conns[i].addr;
        let bond = self.store.bond(&a).cloned();
        let mut out = Vec::new();
        let hid_ok = bond.as_ref().is_some_and(hid_capable);
        let audio_ok = bond.as_ref().is_some_and(|b| b.audio);
        match psm {
            l2cap::PSM_HID_CONTROL | l2cap::PSM_HID_INTERRUPT if hid_ok => self.l2.accept(handle, id, psm, scid, &mut out),
            l2cap::PSM_AVDTP if audio_ok => self.l2.accept(handle, id, psm, scid, &mut out),
            l2cap::PSM_HID_CONTROL | l2cap::PSM_HID_INTERRUPT | l2cap::PSM_AVDTP => self.l2.refuse(handle, id, scid, l2cap::RES_SECURITY_BLOCK, &mut out),
            // SDP, RFCOMM, BNEP, OBEX...: we offer nothing else
            _ => self.l2.refuse(handle, id, scid, l2cap::RES_PSM_NOT_SUPPORTED, &mut out),
        }
        for f in out {
            self.acl(f);
        }
    }

    fn on_note(&mut self, n: Note) {
        match n {
            Note::Incoming { handle, psm, id, scid } => {
                let Some(c) = self.conns.iter_mut().find(|c| c.handle == handle) else { return };
                if c.secure {
                    self.answer_incoming(handle, id, psm, scid);
                } else if matches!(psm, l2cap::PSM_HID_CONTROL | l2cap::PSM_HID_INTERRUPT | l2cap::PSM_AVDTP) && c.waiting.len() < 4 {
                    // answered once the link is authenticated and encrypted
                    c.waiting.push((id, psm, scid));
                    let mut out = Vec::new();
                    out.push((handle, l2cap::frame(l2cap::CID_SIGNALING, &{
                        let mut v = l2cap::sig_conn_rsp(id, 0, scid, l2cap::RES_PENDING);
                        // status: authentication pending
                        let n = v.len();
                        v[n - 2] = 1;
                        v
                    })));
                    for f in out {
                        self.acl(f);
                    }
                } else {
                    let mut out = Vec::new();
                    self.l2.refuse(handle, id, scid, l2cap::RES_PSM_NOT_SUPPORTED, &mut out);
                    for f in out {
                        self.acl(f);
                    }
                }
            },
            Note::Opened { handle, psm, cid } => self.on_open(handle, psm, cid),
            Note::Data { handle, psm, cid, data } => self.on_data(handle, psm, cid, &data),
            Note::Closed { handle, psm, cid } => {
                if let Some(c) = self.conns.iter_mut().find(|c| c.handle == handle) {
                    if c.hid_ctrl == Some(cid) {
                        c.hid_ctrl = None;
                    }
                    if c.hid_intr == Some(cid) {
                        c.hid_intr = None;
                    }
                    c.hid_ready = false;
                    if psm == l2cap::PSM_AVDTP && (c.avdtp_media == Some(cid) || c.avdtp_sig == Some(cid)) {
                        let a = c.addr;
                        c.avdtp_media = None;
                        c.avdtp_sig = None;
                        c.av = None;
                        self.out.push(Output::AudioStop(a));
                    }
                }
            },
            Note::Refused { handle, psm, result } => {
                if let Some(c) = self.conns.iter().find(|c| c.handle == handle) {
                    let a = c.addr;
                    self.notice(format!("{} refused channel {psm:#06x} (result {result})", self.name_of(&a)));
                }
            },
        }
    }

    fn on_open(&mut self, handle: u16, psm: u16, cid: u16) {
        let Some(i) = self.conns.iter().position(|c| c.handle == handle) else { return };
        if !self.conns[i].secure {
            // cannot happen (requests wait for security), but never run a profile on an insecure link
            let mut out = Vec::new();
            self.l2.disconnect(cid, &mut out);
            for f in out {
                self.acl(f);
            }
            return;
        }
        let mut out = Vec::new();
        match psm {
            l2cap::PSM_SDP if self.conns[i].sdp.as_ref().is_some_and(|s| s.0 == cid) => {
                let ev = self.conns[i].sdp.as_mut().map(|s| s.1.start()).unwrap_or_default();
                self.sdp_events(i, ev);
            },
            l2cap::PSM_HID_CONTROL => {
                self.conns[i].hid_ctrl = Some(cid);
                if self.conns[i].outgoing && self.conns[i].hid_intr.is_none() {
                    if let Some(c2) = self.l2.connect(handle, l2cap::PSM_HID_INTERRUPT, &mut out) {
                        self.conns[i].hid_intr = Some(c2);
                    }
                }
            },
            l2cap::PSM_HID_INTERRUPT => self.conns[i].hid_intr = Some(cid),
            l2cap::PSM_AVDTP => {
                let c = &mut self.conns[i];
                if c.avdtp_sig.is_none() || c.avdtp_sig == Some(cid) {
                    c.avdtp_sig = Some(cid);
                    if c.av.is_none() && c.outgoing {
                        let mut s = avdtp::Session::new(self.audio_rate);
                        let ev = s.start();
                        c.av = Some(s);
                        self.av_events(i, ev);
                    }
                } else {
                    c.avdtp_media = Some(cid);
                    let ev = c.av.as_mut().map(|s| s.media_opened()).unwrap_or_default();
                    self.av_events(i, ev);
                }
            },
            _ => {},
        }
        for f in out {
            self.acl(f);
        }
        let c = &mut self.conns[i];
        if !c.hid_ready && c.hid_ctrl.is_some() && c.hid_intr.is_some() && self.l2.is_open(handle, l2cap::PSM_HID_INTERRUPT) && self.l2.is_open(handle, l2cap::PSM_HID_CONTROL) {
            c.hid_ready = true;
            let a = c.addr;
            let ctrl = c.hid_ctrl.unwrap_or(0);
            let bond = self.store.bond(&a).cloned();
            let pad = bond.as_ref().is_some_and(|b| b.gamepad);
            // a gamepad stays in the report protocol (its descriptor says how to read it);
            // keyboards and mice go to the boot protocol
            let proto = if pad { hidp::SET_PROTOCOL_REPORT } else { hidp::SET_PROTOCOL_BOOT };
            if let Some(f) = self.l2.send(ctrl, &proto) {
                self.acl(f);
            }
            if let Some(b) = bond.filter(|b| b.gamepad) {
                self.conns[i].pad_attached = true;
                self.out.push(Output::PadAttach { addr: a, map: b.hid_desc.clone(), vid: b.vid, pid: b.pid, name: self.name_of(&a), le: false });
            }
            let kind = if pad { "gamepad" } else { self.store.bond(&a).map(|b| hci::cod_kind(b.cod)).unwrap_or("input device") };
            self.dev_notice(&a, format!("Bluetooth {kind} connected: {}", self.name_of(&a)));
        }
    }

    fn av_events(&mut self, i: usize, ev: Vec<avdtp::Event>) {
        for e in ev {
            let c = &self.conns[i];
            let (a, handle, sig) = (c.addr, c.handle, c.avdtp_sig);
            match e {
                avdtp::Event::Send(m) => {
                    if let Some(f) = sig.and_then(|s| self.l2.send(s, &m)) {
                        self.acl(f);
                    }
                },
                avdtp::Event::OpenMedia => {
                    let mut out = Vec::new();
                    // the second channel on the same PSM: the media transport
                    let _ = self.l2.connect(handle, l2cap::PSM_AVDTP, &mut out);
                    for f in out {
                        self.acl(f);
                    }
                },
                avdtp::Event::Streaming(cfg) => {
                    let mtu = c.avdtp_media.and_then(|m| self.l2.chans.iter().find(|ch| ch.local == m)).map(|ch| ch.remote_mtu).unwrap_or(672);
                    self.notice(format!("streaming audio to {} (SBC {} Hz, bitpool {})", self.name_of(&a), cfg.rate, cfg.bitpool));
                    self.out.push(Output::AudioStart(a, cfg, mtu));
                },
                avdtp::Event::Suspended | avdtp::Event::Closed => self.out.push(Output::AudioStop(a)),
                avdtp::Event::Failed(s) => self.notice(format!("{}: {s}", self.name_of(&a))),
            }
        }
    }

    fn on_data(&mut self, handle: u16, psm: u16, cid: u16, data: &[u8]) {
        let Some(i) = self.conns.iter().position(|c| c.handle == handle) else { return };
        if !self.conns[i].secure {
            return;
        }
        match psm {
            l2cap::PSM_SDP => {
                if self.conns[i].sdp.as_ref().is_some_and(|s| s.0 == cid) {
                    let ev = self.conns[i].sdp.as_mut().map(|s| s.1.input(data)).unwrap_or_default();
                    self.sdp_events(i, ev);
                }
            },
            l2cap::PSM_HID_INTERRUPT if self.store.bond(&self.conns[i].addr).is_some_and(|b| b.gamepad) => {
                // a gamepad: its input reports (DATA | Input + report) to drivers::gamepad, never to the keyboard
                if data.first() == Some(&0xA1) && data.len() >= 2 && !self.conns[i].cod_changed {
                    let a = self.conns[i].addr;
                    self.out.push(Output::PadReport { addr: a, data: data[1..data.len().min(80)].to_vec() });
                }
            },
            l2cap::PSM_HID_INTERRUPT => {
                let Some(r) = hidp::parse_input(data) else { return };
                let a = self.conns[i].addr;
                let b = self.store.bond(&a).cloned();
                let held = self.conns[i].cod_changed;
                match r {
                    hidp::Report::Keyboard(_) => {
                        let ok = b.as_ref().is_some_and(|b| b.allow_kbd && b.authenticated()) && !held;
                        if ok {
                            self.out.push(Output::Input(a, r));
                        } else if !self.conns[i].blocked_kbd_noted {
                            self.conns[i].blocked_kbd_noted = true;
                            self.dev_notice(&a, format!("{} sent keyboard input; blocked until you allow it in Settings > Bluetooth", self.name_of(&a)));
                        }
                    },
                    hidp::Report::Mouse { .. } => {
                        let ok = b.as_ref().is_some_and(|b| b.allow_ptr) && !held;
                        if ok {
                            self.out.push(Output::Input(a, r));
                        } else if !self.conns[i].blocked_ptr_noted {
                            self.conns[i].blocked_ptr_noted = true;
                            self.dev_notice(&a, format!("{} sent pointer input; blocked until you allow it in Settings > Bluetooth", self.name_of(&a)));
                        }
                    },
                }
            },
            l2cap::PSM_AVDTP if self.conns[i].avdtp_sig == Some(cid) => {
                let ev = self.conns[i].av.as_mut().map(|s| s.input(data)).unwrap_or_default();
                self.av_events(i, ev);
            },
            _ => {},
        }
    }

    fn sdp_events(&mut self, i: usize, ev: Vec<crate::sdp::Event>) {
        for e in ev {
            let Some(c) = self.conns.get(i) else { return };
            let (handle, addr, sdp_cid) = (c.handle, c.addr, c.sdp.as_ref().map(|s| s.0).unwrap_or(0));
            match e {
                crate::sdp::Event::Send(p) => {
                    if let Some(f) = self.l2.send(sdp_cid, &p) {
                        self.acl(f);
                    }
                },
                crate::sdp::Event::Done { desc, vid, pid } => {
                    let (kbd, mouse, mut pad) = crate::gatt::hid_kinds(&desc);
                    // pads with a fixed vendor format (Sony, Nintendo) are pads whatever their descriptor says
                    let name = self.known_name(&addr).unwrap_or_default();
                    pad |= crate::gatt::personality(vid, pid, &name).0 != crate::gatt::Personality::Generic && !kbd;
                    let mut changed = false;
                    if let Some(b) = self.store.bond_mut(&addr) {
                        b.hid_desc = desc;
                        b.vid = vid;
                        b.pid = pid;
                        b.desc_hash = crate::gatt::fnv(&b.hid_desc);
                        b.gamepad = pad;
                        b.allow_ptr |= mouse && !pad;
                        // keyboard rights only with a MITM-protected bond, and never for a pad
                        b.allow_kbd = kbd && !pad && b.authenticated();
                        changed = true;
                    }
                    if changed {
                        self.out.push(Output::StoreChanged);
                    }
                    let mut out = Vec::new();
                    self.l2.disconnect(sdp_cid, &mut out);
                    if let Some(cid) = self.l2.connect(handle, l2cap::PSM_HID_CONTROL, &mut out) {
                        self.conns[i].hid_ctrl = Some(cid);
                    }
                    self.conns[i].sdp = None;
                    for f in out {
                        self.acl(f);
                    }
                },
                crate::sdp::Event::Failed(why) => {
                    self.notice(format!("{}: could not read its HID description ({why})", self.name_of(&addr)));
                    self.conns[i].sdp = None;
                    self.drop_handle(handle, 0x13);
                },
            }
        }
    }

    /// An output report for a Bluetooth gamepad -- classic: DATA | Output on the
    /// interrupt channel; LE: a GATT write to the output report -- its init /
    /// LED / rumble packets from drivers::gamepad.
    pub fn pad_output(&mut self, a: &BdAddr, report: &[u8]) -> bool {
        if let Some(r) = self.le_pad_output(a, report) {
            return r;
        }
        let Some(c) = self.conns.iter().find(|c| &c.addr == a && c.secure && c.hid_ready && c.pad_attached) else { return false };
        let Some(intr) = c.hid_intr else { return false };
        let mut m = alloc::vec![0xA2];
        m.extend_from_slice(&report[..report.len().min(78)]);
        match self.l2.send(intr, &m) {
            Some(f) => {
                self.acl(f);
                true
            },
            None => false,
        }
    }

    /// Send one A2DP media packet (RTP + SBC frames) to `a`; false when there
    /// is no stream or the controller's queue is backed up.
    pub fn send_media(&mut self, a: &BdAddr, sbc_frames: &[u8], n_frames: u8, timestamp: u32) -> bool {
        let Some(c) = self.conns.iter().find(|c| &c.addr == a && c.secure) else { return false };
        let Some(m) = c.avdtp_media else { return false };
        if self.acl_q.len() > 32 {
            return false;
        }
        self.media_seq = self.media_seq.wrapping_add(1);
        let pkt = avdtp::media(self.media_seq, timestamp, sbc_frames, n_frames);
        match self.l2.send(m, &pkt) {
            Some(f) => {
                self.acl(f);
                true
            },
            None => false,
        }
    }

    /// Ask a bonded audio device to stream (A2DP): connect if needed.
    pub fn audio_connect(&mut self, a: &BdAddr) -> Result<(), String> {
        if !self.store.bond(a).is_some_and(|b| b.audio) {
            return Err(String::from("not a paired audio device"));
        }
        self.pair(a)
    }

    /// One-line state for each connection (the `bt` command / Settings).
    pub fn conn_state(&self, a: &BdAddr) -> Option<&'static str> {
        if let Some(s) = self.le_state(a) {
            return Some(s);
        }
        let c = self.conns.iter().find(|c| &c.addr == a)?;
        Some(if c.hid_ready {
            "connected (input)"
        } else if c.avdtp_media.is_some() {
            "connected (audio)"
        } else if c.secure {
            "connected"
        } else {
            "securing the link"
        })
    }
}
