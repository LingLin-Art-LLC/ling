//! Bluetooth LE for the host: passive scanning (we never advertise), central
//! connections to a device the user picked (or a bonded one coming back),
//! SMP with LE Secure Connections only, GATT discovery and HID over GATT
//! (gamepads to drivers::gamepad, boot keyboards / mice to the input path).
//!
//! Policy, as for classic Bluetooth:
//! - pairing only after the user pressed Pair on that device; just works only
//!   for devices that do not announce themselves as keyboards, and a
//!   just-works bond never gets keyboard rights;
//! - SC only: a device without LE Secure Connections is refused ("only
//!   supports insecure pairing"); 16-byte keys; bonds never downgrade;
//! - a bonded device comes back through a passive background scan that
//!   resolves its private addresses with its IRK; nothing else connects;
//! - the Report Map seen at pairing is remembered: a device that later
//!   describes itself differently is held until the user allows it;
//! - blocked identities (IRKs kept with the block entry) never connect.

use super::*;
use crate::gatt::{self, Client};
use crate::smp::{self, Pairing};

pub const LE_CONNECT_TIMEOUT_MS: u64 = 10_000;
pub const LE_RECONNECT_GAP_MS: u64 = 5_000;
const CID_LE_SIGNALING: u16 = 0x0005;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    None,
    /// A gamepad / joystick: input reports to drivers::gamepad.
    Pad,
    /// A keyboard / mouse in the HID boot protocol.
    Boot,
}

pub struct LeLink {
    pub handle: u16,
    /// The identity address (the bond's), or the connection address before pairing.
    pub addr: BdAddr,
    pub peer_type: u8,
    pub peer: BdAddr,
    pub smp: Option<Pairing>,
    pub gatt: Option<Client>,
    pub mode: Mode,
    pub appearance: u16,
    /// Reconnecting with the bond's LTK.
    pub bond_encrypting: bool,
    pub held: bool,
    pub noted: bool,
    pub attached: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Connecting {
    pub addr: BdAddr,
    #[allow(dead_code)]
    pub peer_type: u8,
    pub peer: BdAddr,
    pub pairing: bool,
    pub t: u64,
}

use crate::gatt::fnv;

impl Host {
    pub(super) fn is_le(&self, handle: u16) -> bool {
        self.le_links.iter().any(|l| l.handle == handle)
    }

    fn link(&mut self, handle: u16) -> Option<&mut LeLink> {
        self.le_links.iter_mut().find(|l| l.handle == handle)
    }

    fn le_send(&mut self, handle: u16, cid: u16, pdu: &[u8]) {
        self.acl((handle, l2cap::frame(cid, pdu)));
    }

    /// An advertising report: the scan list while the user scans; a bonded
    /// device's return while the background scan runs.
    pub(super) fn le_adv(&mut self, r: hci::LeAdv) {
        let id = self.store.resolve(&r.addr);
        let connectable = matches!(r.evt_type, 0 | 1);
        if self.bg_scan && connectable && self.le_connecting.is_none() && self.conns.len() < MAX_CONNS {
            let bonded = self.store.bond(&id).is_some_and(|b| b.le);
            let recent = self.reconnect_at.iter().any(|x| x.0 == id && self.now.saturating_sub(x.1) < LE_RECONNECT_GAP_MS);
            if bonded && !recent && !self.conns.iter().any(|c| c.addr == id) && !self.store.is_blocked(&r.addr, None) {
                self.reconnect_at.retain(|x| x.0 != id);
                self.reconnect_at.push((id, self.now));
                if self.reconnect_at.len() > 16 {
                    self.reconnect_at.remove(0);
                }
                self.cmd(hci::le_set_scan_enable(false));
                self.bg_scan = false;
                self.le_connecting = Some(Connecting { addr: id, peer_type: r.addr_type, peer: r.addr, pairing: false, t: self.now });
                self.cmd(hci::le_create_connection(r.addr_type, &r.addr));
                return;
            }
        }
        if !self.scanning {
            return;
        }
        let blocked = self.store.is_blocked(&r.addr, r.name.as_deref());
        if let Some(s) = self.seen.iter_mut().find(|s| s.addr == id) {
            if r.name.is_some() {
                s.name = r.name;
            }
            if r.appearance != 0 {
                s.appearance = r.appearance;
            }
            s.rssi = Some(r.rssi);
            s.conn_addr = r.addr;
            s.conn_type = r.addr_type;
            s.blocked = blocked || self.store.is_blocked(&id, s.name.as_deref());
            if connectable {
                s.connectable = true;
            }
            return;
        }
        if self.seen.len() >= MAX_SEEN {
            return;
        }
        self.seen.push(Seen {
            addr: id,
            name: r.name,
            rssi: Some(r.rssi),
            le: true,
            pscan_rep_mode: 0,
            blocked,
            appearance: r.appearance,
            conn_addr: r.addr,
            conn_type: r.addr_type,
            connectable,
            ..Default::default()
        });
    }

    /// Pair (or reconnect) an LE device from the scan list.
    pub(super) fn le_pair(&mut self, a: &BdAddr) -> Result<(), String> {
        let s = self.seen.iter().find(|s| &s.addr == a).cloned();
        let bond = self.store.bond(a).cloned();
        let (peer_type, peer) = match (&s, &bond) {
            (Some(s), _) => (s.conn_type, s.conn_addr),
            (None, Some(b)) => (b.addr_type, b.addr),
            _ => return Err(String::from("scan first: the device's current address is not known")),
        };
        if s.as_ref().is_some_and(|s| !s.connectable) && bond.is_none() {
            return Err(String::from("this device is not accepting connections (put it in pairing mode)"));
        }
        if self.scanning {
            self.cmd(hci::command(op::INQUIRY_CANCEL, &[]));
            self.scan_done();
        }
        if self.bg_scan {
            self.cmd(hci::le_set_scan_enable(false));
            self.bg_scan = false;
        }
        self.le_connecting = Some(Connecting { addr: *a, peer_type, peer, pairing: bond.is_none(), t: self.now });
        self.cmd(hci::le_create_connection(peer_type, &peer));
        Ok(())
    }

    /// The background scan for bonded LE devices: on while one is away (and
    /// nothing else needs the scanner). Passive: we transmit nothing.
    pub(super) fn le_update_scan(&mut self) {
        let away = self.store.bonds.iter().any(|b| b.le && !self.conns.iter().any(|c| c.addr == b.addr));
        let want = self.state == State::Ready && !self.scanning && self.le_connecting.is_none() && away && self.conns.len() < MAX_CONNS;
        if want != self.bg_scan {
            self.bg_scan = want;
            if want {
                self.cmd(hci::le_set_scan_params());
            }
            self.cmd(hci::le_set_scan_enable(want));
        }
    }

    pub(super) fn le_tick(&mut self) {
        if let Some(c) = self.le_connecting {
            if self.now.saturating_sub(c.t) > LE_CONNECT_TIMEOUT_MS {
                self.le_connecting = None;
                self.cmd(hci::le_create_connection_cancel());
                if c.pairing {
                    self.notice(format!("{}: no answer (is it in pairing mode?)", self.name_of(&c.addr)));
                }
                self.le_update_scan();
            }
        }
    }

    pub(super) fn le_create_failed(&mut self, status: u8) {
        if let Some(c) = self.le_connecting.take() {
            self.notice(format!("could not connect to {} (error {status:#04x})", self.name_of(&c.addr)));
        }
        self.le_update_scan();
    }

    pub(super) fn le_connected(&mut self, status: u8, handle: u16, role: u8, peer_type: u8, peer: BdAddr) {
        let mine = self.le_connecting.filter(|c| c.peer == peer);
        if mine.is_some() {
            self.le_connecting = None;
        }
        if status != 0 {
            if let Some(c) = mine {
                if status != 0x02 {
                    self.notice(format!("could not connect to {} (error {status:#04x})", self.name_of(&c.addr)));
                }
            }
            self.le_update_scan();
            return;
        }
        // we never advertise, so we are never a peripheral; and only connections we started count
        let Some(c) = mine.filter(|_| role == 0) else {
            self.cmd(hci::disconnect(handle, 0x13));
            return;
        };
        if self.store.is_blocked(&peer, None) || self.store.is_blocked(&c.addr, self.known_name(&c.addr).as_deref()) || self.conns.len() >= MAX_CONNS {
            self.cmd(hci::disconnect(handle, 0x13));
            return;
        }
        let appearance = self.seen.iter().find(|s| s.addr == c.addr).map(|s| s.appearance).unwrap_or(0);
        self.conns.push(Conn {
            handle,
            addr: c.addr,
            outgoing: true,
            pairing: c.pairing,
            cod: 0,
            remote_io: None,
            key_type: None,
            auth_sent: true,
            authenticated: false,
            enc_mode: 0,
            secure: false,
            cod_changed: false,
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
        let mut link = LeLink { handle, addr: c.addr, peer_type, peer, smp: None, gatt: None, mode: Mode::None, appearance, bond_encrypting: false, held: false, noted: false, attached: false };
        let bond = self.store.bond(&c.addr).filter(|b| b.le).cloned();
        match (bond, c.pairing) {
            (Some(b), false) => {
                // a bonded device: encrypt with its LTK (mutual proof of the key)
                link.bond_encrypting = true;
                if let Some(cn) = self.conns.last_mut() {
                    cn.key_type = Some(b.key_type);
                }
                self.le_links.push(link);
                self.cmd(hci::le_start_encryption(handle, &b.key));
            },
            _ => {
                let mut seed = [0u8; 32];
                self.drbg.fill(&mut seed);
                match Pairing::start((0, self.local_addr), (peer_type, peer), &seed) {
                    Some((p, ev)) => {
                        link.smp = Some(p);
                        self.le_links.push(link);
                        self.smp_events(handle, ev);
                    },
                    None => {
                        self.le_links.push(link);
                        self.drop_handle(handle, 0x13);
                    },
                }
            },
        }
        self.le_update_scan();
    }

    pub(super) fn le_gone(&mut self, handle: u16) {
        if let Some(i) = self.le_links.iter().position(|l| l.handle == handle) {
            let l = self.le_links.remove(i);
            if l.attached {
                self.out.push(Output::PadDetach(l.addr));
            }
            if l.mode != Mode::None && l.gatt.as_ref().is_some_and(|g| g.done) {
                self.notice(format!("{} disconnected", self.name_of(&l.addr)));
            }
        }
        self.le_update_scan();
    }

    /// Encryption on an LE link: the end of pairing, or a bonded device back.
    pub(super) fn le_encryption(&mut self, status: u8, handle: u16, mode: u8) {
        let Some(l) = self.le_links.iter().find(|l| l.handle == handle) else { return };
        let (addr, bond_enc) = (l.addr, l.bond_encrypting);
        if status != 0 || mode == 0 {
            if bond_enc {
                self.dev_notice(&addr, format!("{} no longer has the pairing (remove it and pair again)", self.name_of(&addr)));
            }
            self.drop_handle(handle, 0x05);
            return;
        }
        if let Some(l) = self.link(handle) {
            if let Some(p) = l.smp.as_mut() {
                let ev = p.encrypted();
                self.smp_events(handle, ev);
                return;
            }
            l.bond_encrypting = false;
        }
        if bond_enc {
            self.le_secure(handle);
        }
    }

    /// Encrypted with a bond's LTK: run GATT.
    fn le_secure(&mut self, handle: u16) {
        if let Some(c) = self.conns.iter_mut().find(|c| c.handle == handle) {
            c.secure = true;
            c.authenticated = true;
            c.enc_mode = 1;
        }
        let mut g = Client::new();
        let ev = g.start();
        if let Some(l) = self.link(handle) {
            l.gatt = Some(g);
        }
        self.gatt_events(handle, ev);
    }

    fn smp_events(&mut self, handle: u16, ev: Vec<smp::Event>) {
        for e in ev {
            let Some(l) = self.le_links.iter().find(|l| l.handle == handle) else { return };
            let (addr, appearance) = (l.addr, l.appearance);
            let pairing = self.conns.iter().any(|c| c.handle == handle && c.pairing);
            match e {
                smp::Event::Send(p) => self.le_send(handle, smp::CID_SMP, &p),
                smp::Event::Method(m) => {
                    let keyboard = appearance == 0x03C1;
                    let was_auth = self.store.bond(&addr).is_some_and(|b| b.authenticated());
                    let why = if m == smp::Method::JustWorks && (keyboard || !pairing) {
                        Some("a keyboard must pair with a confirmed code")
                    } else if m == smp::Method::JustWorks && was_auth {
                        Some("refused a weaker pairing than the one it was bonded with")
                    } else {
                        None
                    };
                    if let Some(why) = why {
                        let ev = self.link(handle).and_then(|l| l.smp.as_mut()).map(|p| p.refuse(why)).unwrap_or_default();
                        self.smp_events(handle, ev);
                    }
                },
                smp::Event::Compare(value) => {
                    self.prompt = Some(Prompt::Confirm { addr, value, until: self.now + CONFIRM_MS });
                    self.notice(format!("does {} show {value:06}? Confirm in Settings > Bluetooth (or `bt confirm yes`)", self.name_of(&addr)));
                },
                smp::Event::ShowPasskey(passkey) => {
                    self.prompt = Some(Prompt::Passkey { addr, passkey });
                    self.notice(format!("type {passkey:06} on {} and press Enter", self.name_of(&addr)));
                },
                smp::Event::Encrypt(ltk) => self.cmd(hci::le_start_encryption(handle, &ltk)),
                smp::Event::Done { ltk, authenticated, irk, identity } => {
                    if matches!(self.prompt, Some(Prompt::Passkey { addr: a, .. }) | Some(Prompt::Confirm { addr: a, .. }) if a == addr) {
                        self.prompt = None;
                    }
                    self.le_bonded(handle, ltk, authenticated, irk, identity);
                },
                smp::Event::Failed { why, insecure } => {
                    if matches!(self.prompt, Some(Prompt::Passkey { addr: a, .. }) | Some(Prompt::Confirm { addr: a, .. }) if a == addr) {
                        self.prompt = None;
                    }
                    let name = self.name_of(&addr);
                    self.notice(if insecure { format!("{name} only supports insecure pairing (no LE Secure Connections): refused") } else { format!("pairing with {name} failed: {why}") });
                    self.drop_handle(handle, 0x05);
                },
            }
        }
    }

    fn le_bonded(&mut self, handle: u16, ltk: [u8; 16], authenticated: bool, irk: Option<[u8; 16]>, identity: Option<(u8, BdAddr)>) {
        let Some(l) = self.le_links.iter().find(|l| l.handle == handle) else { return };
        let (old, peer_type, peer) = (l.addr, l.peer_type, l.peer);
        // the identity the device gave, else the address it connected with (if that is stable)
        let (id_type, id) = identity.unwrap_or((peer_type, peer));
        let name = self.known_name(&old).unwrap_or_default();
        if self.store.is_blocked(&id, Some(&name)) {
            self.drop_handle(handle, 0x13);
            return;
        }
        let key_type = if authenticated { store::KEY_AUTH_P256 } else { store::KEY_UNAUTH_P256 };
        if self.store.bond(&id).is_some_and(|b| b.authenticated()) && !authenticated {
            self.notice(format!("{name}: refused a weaker pairing than the one it was bonded with"));
            self.drop_handle(handle, 0x05);
            return;
        }
        self.store.remove_bond(&id);
        if self.store.bonds.len() >= store::MAX_BONDS {
            self.notice(String::from("too many paired devices: remove one first"));
            self.drop_handle(handle, 0x13);
            return;
        }
        self.store.bonds.push(store::Bond { addr: id, key: ltk, key_type, le: true, addr_type: id_type & 1, irk, name: name.clone(), ..Default::default() });
        // the link and the scan entry now go by the identity address
        for c in self.conns.iter_mut().filter(|c| c.handle == handle) {
            c.addr = id;
            c.key_type = Some(key_type);
        }
        if let Some(l) = self.link(handle) {
            l.addr = id;
            l.smp = None;
        }
        for s in self.seen.iter_mut().filter(|s| s.addr == old) {
            s.addr = id;
        }
        self.out.push(Output::StoreChanged);
        self.notice(format!("paired with {} ({})", if name.is_empty() { addr_str(&id) } else { name }, store::level_name(key_type)));
        self.le_secure(handle);
    }

    fn gatt_events(&mut self, handle: u16, ev: Vec<gatt::Event>) {
        for e in ev {
            match e {
                gatt::Event::Send(p) => self.le_send(handle, gatt::CID_ATT, &p),
                gatt::Event::Discovered => self.hogp_discovered(handle),
                gatt::Event::Subscribed => {
                    let Some(l) = self.le_links.iter().find(|l| l.handle == handle) else { continue };
                    let (addr, mode) = (l.addr, l.mode);
                    let kind = if mode == Mode::Pad { "gamepad" } else { "input device" };
                    self.dev_notice(&addr, format!("Bluetooth {kind} connected: {}", self.name_of(&addr)));
                },
                gatt::Event::Value(h, v) => self.hogp_value(handle, h, &v),
                gatt::Event::Failed(why) => {
                    let addr = self.le_links.iter().find(|l| l.handle == handle).map(|l| l.addr).unwrap_or_default();
                    self.notice(format!("{}: {why}", self.name_of(&addr)));
                    self.drop_handle(handle, 0x13);
                },
            }
        }
    }

    fn hogp_discovered(&mut self, handle: u16) {
        let Some(l) = self.le_links.iter().find(|l| l.handle == handle) else { return };
        let addr = l.addr;
        let Some(g) = l.gatt.as_ref() else { return };
        let map = g.map.clone();
        let (kbd, mouse, pad) = gatt::hid_kinds(&map);
        let pnp = g.pnp;
        let inputs: Vec<u16> = g.reports.iter().filter(|r| r.kind == 1).map(|r| r.value).collect();
        let boots: Vec<u16> = [g.boot_kbd.filter(|_| kbd), g.boot_mouse.filter(|_| mouse)].iter().flatten().map(|r| r.value).collect();
        let proto = g.protocol_mode;
        if map.is_empty() {
            self.notice(format!("{} is not an input device this computer can use (no HID service)", self.name_of(&addr)));
            self.drop_handle(handle, 0x13);
            return;
        }
        let hash = fnv(&map);
        let mut held = false;
        let mut changed = false;
        if let Some(b) = self.store.bond_mut(&addr) {
            if b.desc_hash == 0 {
                // first connection after pairing: what it is now is what it was paired as
                b.desc_hash = hash;
                b.gamepad = pad;
                b.allow_ptr = mouse;
                b.allow_kbd = kbd && b.authenticated();
                changed = true;
            } else if b.desc_hash != hash {
                held = true;
            }
        }
        if changed {
            self.out.push(Output::StoreChanged);
        }
        if held {
            self.dev_notice(&addr, format!("{} now describes itself differently than when it was paired: its input is held until you allow it", self.name_of(&addr)));
        }
        if kbd && !self.store.bond(&addr).is_some_and(|b| b.authenticated()) {
            self.dev_notice(&addr, format!("{} has keyboard keys, but was paired without a code: keyboard input stays blocked", self.name_of(&addr)));
        }
        let mode = if pad { Mode::Pad } else if kbd || mouse { Mode::Boot } else { Mode::None };
        let mut ev = Vec::new();
        if let Some(l) = self.link(handle) {
            l.mode = mode;
            l.held = held;
            if let Some(g) = l.gatt.as_mut() {
                match mode {
                    Mode::Pad => ev = g.subscribe(&inputs),
                    Mode::Boot => {
                        if let Some(pm) = proto {
                            // boot protocol: fixed keyboard / mouse reports
                            ev.push(gatt::Event::Send(gatt::write_cmd(pm, &[0x00])));
                        }
                        ev.extend(g.subscribe(&boots));
                    },
                    Mode::None => {},
                }
            }
        }
        if mode == Mode::None {
            self.notice(format!("{}: no gamepad, keyboard or mouse in its report map", self.name_of(&addr)));
        }
        if mode == Mode::Pad {
            let (vid, pid) = pnp.map(|p| (p.1, p.2)).unwrap_or((0, 0));
            if let Some(l) = self.link(handle) {
                l.attached = true;
            }
            let name = self.name_of(&addr);
            self.out.push(Output::PadAttach { addr, map, vid, pid, name, le: true });
        }
        self.gatt_events(handle, ev);
    }

    fn hogp_value(&mut self, handle: u16, h: u16, v: &[u8]) {
        let Some(l) = self.le_links.iter().find(|l| l.handle == handle) else { return };
        let (addr, mode, held) = (l.addr, l.mode, l.held);
        let Some(g) = l.gatt.as_ref() else { return };
        let Some(b) = self.store.bond(&addr).cloned() else { return };
        if held {
            return;
        }
        match mode {
            Mode::Pad => {
                let Some(r) = g.reports.iter().find(|r| r.value == h && r.kind == 1) else { return };
                if !b.gamepad {
                    return;
                }
                let mut data = Vec::with_capacity(v.len() + 1);
                if gatt::uses_report_ids(&g.map) {
                    data.push(r.id);
                }
                data.extend_from_slice(&v[..v.len().min(64)]);
                self.out.push(Output::PadReport { addr, data });
            },
            Mode::Boot => {
                if g.boot_kbd.is_some_and(|r| r.value == h) {
                    if v.len() < 8 {
                        return;
                    }
                    if b.allow_kbd && b.authenticated() {
                        let mut k = [0u8; 8];
                        k.copy_from_slice(&v[..8]);
                        self.out.push(Output::Input(addr, hidp::Report::Keyboard(k)));
                    } else if !self.le_links.iter().any(|l| l.handle == handle && l.noted) {
                        if let Some(l) = self.link(handle) {
                            l.noted = true;
                        }
                        self.dev_notice(&addr, format!("{} sent keyboard input; blocked until you allow it in Settings > Bluetooth", self.name_of(&addr)));
                    }
                } else if g.boot_mouse.is_some_and(|r| r.value == h) && v.len() >= 3 && b.allow_ptr {
                    self.out.push(Output::Input(addr, hidp::Report::Mouse { buttons: v[0] & 7, dx: v[1] as i8, dy: v[2] as i8, wheel: v.get(3).map(|&w| w as i8).unwrap_or(0) }));
                }
            },
            Mode::None => {},
        }
    }

    /// A frame on an LE link's fixed channels.
    pub(super) fn le_frame(&mut self, handle: u16, cid: u16, p: &[u8]) {
        match cid {
            smp::CID_SMP => {
                let has = self.le_links.iter().any(|l| l.handle == handle && l.smp.is_some());
                if has {
                    let ev = self.link(handle).and_then(|l| l.smp.as_mut()).map(|s| s.input(p)).unwrap_or_default();
                    self.smp_events(handle, ev);
                } else if p.first() == Some(&smp::SECURITY_REQUEST) {
                    // a bonded device asking for security gets it from the LTK we already start
                } else if !p.is_empty() && p[0] != smp::PAIRING_FAILED {
                    // a pairing nobody asked for
                    self.le_send(handle, smp::CID_SMP, &[smp::PAIRING_FAILED, smp::ERR_NOT_SUPPORTED]);
                }
            },
            gatt::CID_ATT => {
                let secure = self.conns.iter().any(|c| c.handle == handle && c.secure);
                let ev = match self.link(handle).and_then(|l| l.gatt.as_mut()) {
                    Some(g) if secure => g.input(p),
                    // before encryption: we serve nothing
                    _ => match p.first() {
                        Some(&op) if op & 0x40 == 0 && op != gatt::NOTIFY && op != gatt::INDICATE && op % 2 == 0 => alloc::vec![gatt::Event::Send(gatt::error_rsp(op, crate::le16(p, 1).unwrap_or(0), 0x05))],
                        _ => Vec::new(),
                    },
                };
                self.gatt_events(handle, ev);
            },
            CID_LE_SIGNALING => self.le_signal(handle, p),
            _ => {},
        }
    }

    fn le_signal(&mut self, handle: u16, p: &[u8]) {
        let (Some(&code), Some(&id)) = (p.first(), p.get(1)) else { return };
        match code {
            // Connection Parameter Update Request
            0x12 => {
                let (Some(min), Some(max), Some(lat), Some(to)) = (crate::le16(p, 4), crate::le16(p, 6), crate::le16(p, 8), crate::le16(p, 10)) else { return };
                let ok = hci::le_params_ok(min, max, lat, to);
                self.le_send(handle, CID_LE_SIGNALING, &[0x13, id, 2, 0, if ok { 0 } else { 1 }, 0]);
                if ok {
                    self.cmd(hci::le_conn_params(op::LE_CONNECTION_UPDATE, handle, min, max, lat, to));
                }
            },
            0x01 | 0x13 => {},
            // command not understood
            _ => self.le_send(handle, CID_LE_SIGNALING, &[0x01, id, 2, 0, 0, 0]),
        }
    }

    pub(super) fn le_param_request(&mut self, handle: u16, min: u16, max: u16, latency: u16, timeout: u16) {
        if hci::le_params_ok(min, max, latency, timeout) {
            self.cmd(hci::le_conn_params(op::LE_REMOTE_PARAM_REPLY, handle, min, max, latency, timeout));
        } else {
            self.cmd(hci::le_remote_param_neg(handle));
        }
    }

    /// The user's answer to a numeric comparison on an LE link; None when the prompt is not LE.
    pub(super) fn le_confirm(&mut self, addr: &BdAddr, yes: bool) -> Option<bool> {
        let handle = self.le_links.iter().find(|l| &l.addr == addr && l.smp.is_some())?.handle;
        let ev = self.link(handle).and_then(|l| l.smp.as_mut()).map(|p| p.user_confirm(yes)).unwrap_or_default();
        self.smp_events(handle, ev);
        Some(true)
    }

    /// Approve the current report map of a bonded LE device (after "describes itself differently").
    pub(super) fn le_approve(&mut self, a: &BdAddr) {
        let Some(l) = self.le_links.iter_mut().find(|l| &l.addr == a) else { return };
        l.held = false;
        let Some(g) = l.gatt.as_ref() else { return };
        let hash = fnv(&g.map);
        let (_, mouse, pad) = gatt::hid_kinds(&g.map);
        if let Some(b) = self.store.bond_mut(a) {
            b.desc_hash = hash;
            b.gamepad |= pad;
            b.allow_ptr |= mouse;
        }
    }

    pub(super) fn le_state(&self, a: &BdAddr) -> Option<&'static str> {
        let l = self.le_links.iter().find(|l| &l.addr == a)?;
        Some(if l.smp.is_some() {
            "pairing"
        } else if l.held {
            "connected (input held)"
        } else if l.mode == Mode::Pad {
            "connected (gamepad)"
        } else if l.mode == Mode::Boot {
            "connected (input)"
        } else {
            "securing the link"
        })
    }

    /// An output report for an LE pad (drivers::gamepad's init / rumble
    /// packets): a Write Command to the Report characteristic of that output
    /// report id (the first byte when the map uses ids). None when `a` is not an LE pad.
    pub(super) fn le_pad_output(&mut self, a: &BdAddr, report: &[u8]) -> Option<bool> {
        let l = self.le_links.iter().find(|l| &l.addr == a && l.mode == Mode::Pad)?;
        let handle = l.handle;
        let Some(g) = l.gatt.as_ref() else { return Some(false) };
        let ids = gatt::uses_report_ids(&g.map);
        let (id, body) = if ids { (report.first().copied().unwrap_or(0), report.get(1..).unwrap_or(&[])) } else { (0, report) };
        let Some(r) = g.reports.iter().find(|r| r.kind == 2 && r.id == id) else { return Some(false) };
        let pdu = gatt::write_cmd(r.value, &body[..body.len().min(64)]);
        self.le_send(handle, gatt::CID_ATT, &pdu);
        Some(true)
    }

    /// Rumble on an LE pad that has one (Xbox: output report 3).
    pub fn pad_rumble(&mut self, a: &BdAddr, strong: f32, weak: f32, ms: u32) -> Result<(), String> {
        let l = self.le_links.iter().find(|l| &l.addr == a && l.mode == Mode::Pad).ok_or("not a connected Bluetooth gamepad")?;
        let g = l.gatt.as_ref().ok_or("not ready")?;
        if g.pnp.map(|p| p.1) != Some(0x045E) {
            return Err(String::from("rumble over Bluetooth LE is only known for Xbox controllers"));
        }
        let r = g.reports.iter().find(|r| r.kind == 2 && r.id == 3).ok_or("no rumble output report")?;
        let pdu = gatt::write_cmd(r.value, &gatt::xbox_ble_rumble(strong, weak, ms));
        let h = l.handle;
        self.le_send(h, gatt::CID_ATT, &pdu);
        Ok(())
    }
}
