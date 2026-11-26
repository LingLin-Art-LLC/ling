//! ATT / GATT client (Core v5.3 Vol 3 Parts F, G) for HID over GATT (HOGP):
//! MTU exchange, primary services, characteristics, descriptors, the HID
//! service's Report Map (long read), Report References, the Device
//! Information service's PnP ID, then CCCD writes for the input reports and
//! notifications.
//!
//! One request at a time; every response is bounds-checked; lists are capped
//! (services, characteristics, descriptors, reports, the Report Map's size).
//! As a server we offer nothing: MTU requests are answered, every other
//! request gets an error.

use crate::le16;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

pub const CID_ATT: u16 = 0x0004;
pub const OUR_MTU: u16 = 247;

pub const ERROR_RSP: u8 = 0x01;
pub const MTU_REQ: u8 = 0x02;
pub const MTU_RSP: u8 = 0x03;
pub const FIND_INFO_REQ: u8 = 0x04;
pub const FIND_INFO_RSP: u8 = 0x05;
pub const READ_BY_TYPE_REQ: u8 = 0x08;
pub const READ_BY_TYPE_RSP: u8 = 0x09;
pub const READ_REQ: u8 = 0x0A;
pub const READ_RSP: u8 = 0x0B;
pub const READ_BLOB_REQ: u8 = 0x0C;
pub const READ_BLOB_RSP: u8 = 0x0D;
pub const READ_BY_GROUP_REQ: u8 = 0x10;
pub const READ_BY_GROUP_RSP: u8 = 0x11;
pub const WRITE_REQ: u8 = 0x12;
pub const WRITE_RSP: u8 = 0x13;
pub const NOTIFY: u8 = 0x1B;
pub const INDICATE: u8 = 0x1D;
pub const CONFIRM: u8 = 0x1E;
pub const WRITE_CMD: u8 = 0x52;

pub const ERR_NOT_FOUND: u8 = 0x0A;
pub const ERR_NOT_SUPPORTED: u8 = 0x06;

pub const UUID_PRIMARY: u16 = 0x2800;
pub const UUID_CHARACTERISTIC: u16 = 0x2803;
pub const UUID_CCCD: u16 = 0x2902;
pub const UUID_REPORT_REF: u16 = 0x2908;
pub const SVC_HID: u16 = 0x1812;
pub const SVC_DIS: u16 = 0x180A;
pub const CHR_REPORT_MAP: u16 = 0x2A4B;
pub const CHR_REPORT: u16 = 0x2A4D;
pub const CHR_PROTOCOL_MODE: u16 = 0x2A4E;
pub const CHR_BOOT_KBD_IN: u16 = 0x2A22;
pub const CHR_BOOT_MOUSE_IN: u16 = 0x2A33;
pub const CHR_PNP_ID: u16 = 0x2A50;

pub const MAX_SERVICES: usize = 32;
pub const MAX_CHARS: usize = 64;
pub const MAX_REPORTS: usize = 16;
pub const MAX_MAP: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Char {
    pub decl: u16,
    pub value: u16,
    pub props: u8,
    pub uuid: u16,
    /// Last handle of the characteristic (its descriptors end here).
    pub end: u16,
}

/// One HID report characteristic: its value handle, CCCD, report id and type (1 input, 2 output, 3 feature).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Report {
    pub value: u16,
    pub cccd: u16,
    pub id: u8,
    pub kind: u8,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Job {
    Mtu,
    Services(u16),
    Chars { start: u16, end: u16 },
    Descs { start: u16, end: u16 },
    ReadRef { handle: u16, value: u16 },
    ReadMap { handle: u16, offset: u16 },
    ReadPnp(u16),
    WriteCccd(u16),
}

#[derive(Debug, PartialEq)]
pub enum Event {
    Send(Vec<u8>),
    /// Discovery finished: the HID service's report map, reports, boot characteristics, PnP ID.
    Discovered,
    /// Every requested CCCD is written: input flows.
    Subscribed,
    /// A notification (or indication) from `handle`.
    Value(u16, Vec<u8>),
    Failed(String),
}

#[derive(Default)]
pub struct Client {
    pub mtu: u16,
    pub services: Vec<(u16, u16, u16)>,
    pub chars: Vec<Char>,
    /// (descriptor handle, uuid, the characteristic value handle it belongs to)
    pub descs: Vec<(u16, u16, u16)>,
    pub map: Vec<u8>,
    /// PnP ID: (vendor id source, vendor, product, version)
    pub pnp: Option<(u8, u16, u16, u16)>,
    pub reports: Vec<Report>,
    pub boot_kbd: Option<Report>,
    pub boot_mouse: Option<Report>,
    pub protocol_mode: Option<u16>,
    jobs: VecDeque<Job>,
    pending: Option<(u8, Job)>,
    discovering: bool,
    subscribing: bool,
    pub done: bool,
}

fn req(op: u8, args: &[u16]) -> Vec<u8> {
    let mut v = alloc::vec![op];
    for a in args {
        v.extend_from_slice(&a.to_le_bytes());
    }
    v
}

pub fn error_rsp(req_op: u8, handle: u16, code: u8) -> Vec<u8> {
    let mut v = alloc::vec![ERROR_RSP, req_op];
    v.extend_from_slice(&handle.to_le_bytes());
    v.push(code);
    v
}

/// A Write Command (no response), e.g. an output report or the protocol mode.
pub fn write_cmd(handle: u16, value: &[u8]) -> Vec<u8> {
    let mut v = alloc::vec![WRITE_CMD];
    v.extend_from_slice(&handle.to_le_bytes());
    v.extend_from_slice(value);
    v
}

impl Client {
    pub fn new() -> Client {
        Client { mtu: 23, ..Default::default() }
    }

    /// Begin discovery (the link is encrypted by now).
    pub fn start(&mut self) -> Vec<Event> {
        self.discovering = true;
        self.jobs.push_back(Job::Mtu);
        self.jobs.push_back(Job::Services(1));
        self.next()
    }

    /// Write the CCCDs of these characteristics (value handles) to enable notifications.
    pub fn subscribe(&mut self, value_handles: &[u16]) -> Vec<Event> {
        self.subscribing = true;
        for &h in value_handles.iter().take(MAX_REPORTS + 2) {
            if let Some(c) = self.cccd_of(h) {
                self.jobs.push_back(Job::WriteCccd(c));
            }
        }
        self.next()
    }

    pub fn cccd_of(&self, value: u16) -> Option<u16> {
        self.descs.iter().find(|d| d.2 == value && d.1 == UUID_CCCD).map(|d| d.0)
    }

    pub fn char_by_uuid(&self, uuid: u16) -> Option<Char> {
        self.chars.iter().copied().find(|c| c.uuid == uuid)
    }

    fn in_service(&self, svc: u16, handle: u16) -> bool {
        self.services.iter().any(|s| s.2 == svc && (s.0..=s.1).contains(&handle))
    }

    fn next(&mut self) -> Vec<Event> {
        if self.pending.is_some() {
            return Vec::new();
        }
        let Some(j) = self.jobs.pop_front() else {
            let mut out = Vec::new();
            if self.discovering {
                self.discovering = false;
                self.done = true;
                out.push(Event::Discovered);
            } else if self.subscribing {
                self.subscribing = false;
                out.push(Event::Subscribed);
            }
            return out;
        };
        let (op, pdu) = match j {
            Job::Mtu => (MTU_REQ, req(MTU_REQ, &[OUR_MTU])),
            Job::Services(s) => (READ_BY_GROUP_REQ, req(READ_BY_GROUP_REQ, &[s, 0xFFFF, UUID_PRIMARY])),
            Job::Chars { start, end } => (READ_BY_TYPE_REQ, req(READ_BY_TYPE_REQ, &[start, end, UUID_CHARACTERISTIC])),
            Job::Descs { start, end } => (FIND_INFO_REQ, req(FIND_INFO_REQ, &[start, end])),
            Job::ReadRef { handle, .. } => (READ_REQ, req(READ_REQ, &[handle])),
            Job::ReadMap { handle, offset } => {
                if offset == 0 {
                    (READ_REQ, req(READ_REQ, &[handle]))
                } else {
                    (READ_BLOB_REQ, req(READ_BLOB_REQ, &[handle, offset]))
                }
            },
            Job::ReadPnp(h) => (READ_REQ, req(READ_REQ, &[h])),
            Job::WriteCccd(h) => {
                let mut v = req(WRITE_REQ, &[h]);
                v.extend_from_slice(&1u16.to_le_bytes());
                (WRITE_REQ, v)
            },
        };
        self.pending = Some((op, j));
        alloc::vec![Event::Send(pdu)]
    }

    /// After the characteristics are known: descriptors, references, the map, PnP.
    fn plan_after_chars(&mut self) {
        let mut jobs = Vec::new();
        // descriptor ranges of the HID characteristics that have them (reports + boot inputs)
        for c in self.chars.iter() {
            if self.in_service(SVC_HID, c.value) && matches!(c.uuid, CHR_REPORT | CHR_BOOT_KBD_IN | CHR_BOOT_MOUSE_IN) && c.end > c.value {
                jobs.push(Job::Descs { start: c.value + 1, end: c.end });
            }
        }
        if let Some(m) = self.chars.iter().find(|c| c.uuid == CHR_REPORT_MAP && self.in_service(SVC_HID, c.value)) {
            jobs.push(Job::ReadMap { handle: m.value, offset: 0 });
        }
        if let Some(p) = self.chars.iter().find(|c| c.uuid == CHR_PNP_ID && self.in_service(SVC_DIS, c.value)) {
            jobs.push(Job::ReadPnp(p.value));
        }
        self.protocol_mode = self.chars.iter().find(|c| c.uuid == CHR_PROTOCOL_MODE && self.in_service(SVC_HID, c.value)).map(|c| c.value);
        self.jobs.extend(jobs);
    }

    /// One ATT PDU from the peer.
    pub fn input(&mut self, p: &[u8]) -> Vec<Event> {
        let Some(&op) = p.first() else { return Vec::new() };
        let body = &p[1..];
        match op {
            NOTIFY | INDICATE => {
                let mut out = Vec::new();
                if op == INDICATE {
                    out.push(Event::Send(alloc::vec![CONFIRM]));
                }
                if let Some(h) = le16(body, 0) {
                    let v = &body[2..];
                    out.push(Event::Value(h, v[..v.len().min(512)].to_vec()));
                }
                return out;
            },
            MTU_REQ => {
                let mut v = alloc::vec![MTU_RSP];
                v.extend_from_slice(&OUR_MTU.to_le_bytes());
                if let Some(m) = le16(body, 0) {
                    self.mtu = m.clamp(23, OUR_MTU);
                }
                return alloc::vec![Event::Send(v)];
            },
            // the peer acting as a client: we have no attributes
            FIND_INFO_REQ | READ_BY_TYPE_REQ | READ_REQ | READ_BLOB_REQ | READ_BY_GROUP_REQ | 0x06 => {
                return alloc::vec![Event::Send(error_rsp(op, le16(body, 0).unwrap_or(0), ERR_NOT_FOUND))];
            },
            WRITE_REQ | 0x16 | 0x18 | 0x0E | 0x20 => {
                return alloc::vec![Event::Send(error_rsp(op, le16(body, 0).unwrap_or(0), ERR_NOT_SUPPORTED))];
            },
            _ => {},
        }
        // a response: it must answer the request in flight
        let Some((req_op, job)) = self.pending else { return Vec::new() };
        let ok_rsp = op == req_op + 1;
        if !ok_rsp && op != ERROR_RSP {
            return Vec::new();
        }
        self.pending = None;
        let err = if op == ERROR_RSP { body.get(3).copied() } else { None };
        match job {
            Job::Mtu => {
                if let Some(m) = le16(body, 0).filter(|_| ok_rsp) {
                    self.mtu = m.clamp(23, OUR_MTU);
                }
            },
            Job::Services(_) => {
                if ok_rsp {
                    let len = *body.first().unwrap_or(&0) as usize;
                    let mut last = 0u16;
                    if len == 6 || len == 20 {
                        for e in body[1..].chunks_exact(len) {
                            let (Some(s), Some(en)) = (le16(e, 0), le16(e, 2)) else { break };
                            if en < s || s <= last {
                                return self.bad("service list out of order");
                            }
                            last = en;
                            // 128-bit services are kept with uuid 0 (not ours)
                            let uuid = if len == 6 { le16(e, 4).unwrap_or(0) } else { 0 };
                            if self.services.len() < MAX_SERVICES {
                                self.services.push((s, en, uuid));
                            }
                        }
                    }
                    if last != 0 && last < 0xFFFF && self.services.len() < MAX_SERVICES {
                        self.jobs.push_front(Job::Services(last + 1));
                    } else {
                        self.queue_chars();
                    }
                } else {
                    // attribute not found: the list is complete
                    self.queue_chars();
                }
            },
            Job::Chars { start: _, end } => {
                if ok_rsp {
                    let len = *body.first().unwrap_or(&0) as usize;
                    let mut last = 0u16;
                    if len == 7 || len == 21 {
                        for e in body[1..].chunks_exact(len) {
                            let (Some(decl), Some(&props), Some(value)) = (le16(e, 0), e.get(2), le16(e, 3)) else { break };
                            if value <= decl || decl <= last {
                                return self.bad("characteristic list out of order");
                            }
                            last = decl;
                            let uuid = if len == 7 { le16(e, 5).unwrap_or(0) } else { 0 };
                            if self.chars.len() < MAX_CHARS {
                                // its end: before the next declaration, or the service end (fixed below)
                                if let Some(prev) = self.chars.last_mut() {
                                    if prev.end > decl - 1 || prev.end == 0 {
                                        prev.end = prev.end.min(decl - 1);
                                    }
                                }
                                self.chars.push(Char { decl, value, props, uuid, end: end });
                            }
                        }
                    }
                    if last != 0 && last < end {
                        self.jobs.push_front(Job::Chars { start: last + 1, end });
                    }
                }
                if !self.jobs.iter().any(|j| matches!(j, Job::Chars { .. })) {
                    self.plan_after_chars();
                }
            },
            Job::Descs { start: _, end } => {
                if ok_rsp {
                    let fmt = *body.first().unwrap_or(&0);
                    let size = if fmt == 1 { 4 } else if fmt == 2 { 18 } else { 0 };
                    let mut last = 0u16;
                    if size != 0 {
                        for e in body[1..].chunks_exact(size) {
                            let Some(h) = le16(e, 0) else { break };
                            if h <= last || h > end {
                                return self.bad("descriptor list out of order");
                            }
                            last = h;
                            let uuid = if size == 4 { le16(e, 2).unwrap_or(0) } else { 0 };
                            // which characteristic: the last one declared before it
                            let owner = self.chars.iter().filter(|c| c.value < h).map(|c| c.value).max().unwrap_or(0);
                            if self.descs.len() < 4 * MAX_CHARS {
                                self.descs.push((h, uuid, owner));
                                if uuid == UUID_REPORT_REF {
                                    self.jobs.push_back(Job::ReadRef { handle: h, value: owner });
                                }
                            }
                        }
                    }
                    if last != 0 && last < end {
                        self.jobs.push_front(Job::Descs { start: last + 1, end });
                    }
                }
                // boot characteristics are known once their descriptors are
                self.fill_boot();
            },
            Job::ReadRef { handle: _, value } => {
                if ok_rsp && body.len() >= 2 && self.reports.len() < MAX_REPORTS {
                    let cccd = self.cccd_of(value).unwrap_or(0);
                    self.reports.push(Report { value, cccd, id: body[0], kind: body[1] });
                }
            },
            Job::ReadMap { handle, offset } => {
                if ok_rsp {
                    let room = MAX_MAP.saturating_sub(self.map.len());
                    self.map.extend_from_slice(&body[..body.len().min(room)]);
                    // a full-MTU chunk means there may be more (Read Blob)
                    if body.len() >= self.mtu as usize - 1 && self.map.len() < MAX_MAP {
                        self.jobs.push_front(Job::ReadMap { handle, offset: offset.saturating_add(body.len() as u16) });
                    }
                } else if err == Some(0x07) || err == Some(0x0B) {
                    // invalid offset / attribute not long: the map is complete
                }
            },
            Job::ReadPnp(_) => {
                if ok_rsp && body.len() >= 7 {
                    self.pnp = Some((body[0], le16(body, 1).unwrap_or(0), le16(body, 3).unwrap_or(0), le16(body, 5).unwrap_or(0)));
                }
            },
            Job::WriteCccd(_) => {
                if !ok_rsp {
                    return alloc::vec![Event::Failed(alloc::format!("the device refused to send reports (ATT error {:#04x})", err.unwrap_or(0)))];
                }
            },
        }
        self.next()
    }

    fn bad(&mut self, why: &str) -> Vec<Event> {
        self.jobs.clear();
        self.discovering = false;
        self.subscribing = false;
        alloc::vec![Event::Failed(alloc::format!("GATT: {why}"))]
    }

    fn queue_chars(&mut self) {
        let wanted: Vec<(u16, u16)> = self.services.iter().filter(|s| s.2 == SVC_HID || s.2 == SVC_DIS).map(|s| (s.0, s.1)).collect();
        for (s, e) in wanted.iter().rev() {
            self.jobs.push_front(Job::Chars { start: *s, end: *e });
        }
        if wanted.is_empty() {
            self.plan_after_chars();
        }
        // close each characteristic's range at its service's end
        let svcs = self.services.clone();
        for c in self.chars.iter_mut() {
            if let Some(s) = svcs.iter().find(|s| (s.0..=s.1).contains(&c.decl)) {
                c.end = c.end.min(s.1);
            }
        }
    }

    fn fill_boot(&mut self) {
        let bk = self.char_by_uuid(CHR_BOOT_KBD_IN).map(|c| Report { value: c.value, cccd: self.cccd_of(c.value).unwrap_or(0), id: 0, kind: 1 });
        let bm = self.char_by_uuid(CHR_BOOT_MOUSE_IN).map(|c| Report { value: c.value, cccd: self.cccd_of(c.value).unwrap_or(0), id: 0, kind: 1 });
        self.boot_kbd = bk;
        self.boot_mouse = bm;
    }
}

// ── HOGP: what the Report Map describes ─────────────────────────────────────

/// FNV-1a of a report descriptor (never 0): remembered at pairing to notice a device that changes.
pub fn fnv(d: &[u8]) -> u32 {
    let mut h = 0x811C_9DC5u32;
    for &b in d {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h.max(1)
}

/// The application collections of a report descriptor: (usage page, usage).
/// Bounded and tolerant: a malformed descriptor yields what was parsed so far.
pub fn app_usages(d: &[u8]) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    let (mut page, mut usage) = (0u16, 0u16);
    let mut i = 0usize;
    while i < d.len() && out.len() < 16 {
        let b = d[i];
        if b == 0xFE {
            // long item: skip
            let n = *d.get(i + 1).unwrap_or(&0) as usize;
            i += 3 + n;
            continue;
        }
        let size = match b & 3 {
            3 => 4,
            s => s as usize,
        };
        let Some(data) = d.get(i + 1..i + 1 + size) else { break };
        let mut v = 0u32;
        for (k, &x) in data.iter().enumerate() {
            v |= (x as u32) << (8 * k);
        }
        match b & 0xFC {
            0x04 => page = v as u16,
            0x08 => {
                if size == 4 {
                    page = (v >> 16) as u16;
                }
                usage = v as u16;
            },
            0xA0 => {
                if v == 1 {
                    out.push((page, usage));
                }
            },
            _ => {},
        }
        i += 1 + size;
    }
    out
}

/// Does the descriptor use report IDs (then every report starts with one)?
pub fn uses_report_ids(d: &[u8]) -> bool {
    let mut i = 0usize;
    while i < d.len() {
        let b = d[i];
        if b == 0xFE {
            i += 3 + *d.get(i + 1).unwrap_or(&0) as usize;
            continue;
        }
        if b & 0xFC == 0x84 {
            return true;
        }
        i += 1 + match b & 3 {
            3 => 4,
            s => s as usize,
        };
    }
    false
}

/// What kinds of input device the map describes: (keyboard, mouse, gamepad / joystick).
pub fn hid_kinds(map: &[u8]) -> (bool, bool, bool) {
    let a = app_usages(map);
    let has = |u: u16| a.iter().any(|&(p, x)| p == 1 && x == u);
    (has(0x06), has(0x02), has(0x05) || has(0x04) || has(0x08))
}

/// Xbox Wireless controllers (BLE firmware) send 15 buttons in their own order
/// (A B - X Y - LB RB - - View Menu Guide LS RS, as Linux's xpadneo maps
/// them); rewrite them in place to the canonical order LingOS uses (A B X Y
/// LB RB Back Start LS RS Guide) so the generic descriptor decoder reads them
/// right. `bit_off` is where the first button sits in `report`.
pub fn xbox_ble_remap(report: &mut [u8], bit_off: usize) {
    const DST: [(usize, usize); 11] = [(0, 0), (1, 1), (3, 2), (4, 3), (6, 4), (7, 5), (10, 6), (11, 7), (13, 8), (14, 9), (12, 10)];
    let get = |r: &[u8], k: usize| -> bool {
        let b = bit_off + k;
        r.get(b / 8).is_some_and(|x| x >> (b % 8) & 1 != 0)
    };
    if report.len() * 8 < bit_off + 15 {
        return;
    }
    let src: Vec<bool> = (0..15).map(|k| get(report, k)).collect();
    for k in 0..15 {
        let b = bit_off + k;
        report[b / 8] &= !(1 << (b % 8));
    }
    for (s, d) in DST {
        if src[s] {
            let b = bit_off + d;
            report[b / 8] |= 1 << (b % 8);
        }
    }
}

/// What a gamepad presents itself as. Multi-mode pads (the AceGamer Aurora
/// and its kind) pick one at power-on: an Xbox Wireless Controller (BLE HOGP
/// like the Series pads, or classic HID like the One S), a Switch Pro
/// Controller (classic, 057E:2009), a DualShock 4, or generic HID. The PnP /
/// Device ID decides first; without one, the advertised name.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Personality {
    Xbox,
    SwitchPro,
    DualShock4,
    DualSense,
    Generic,
}

pub const XBOX_PIDS: [u16; 9] = [0x02E0, 0x02FD, 0x0B05, 0x0B13, 0x0B20, 0x0B21, 0x0B22, 0x0B12, 0x0B0C];

/// Which personality, and the ids drivers::hidpad should identify it by.
pub fn personality(vid: u16, pid: u16, name: &str) -> (Personality, u16, u16) {
    match (vid, pid) {
        (0x057E, 0x2009) => return (Personality::SwitchPro, vid, pid),
        (0x054C, 0x05C4 | 0x09CC | 0x0BA0 | 0x0BA1) => return (Personality::DualShock4, vid, pid),
        (0x054C, 0x0CE6 | 0x0DF2) => return (Personality::DualSense, vid, pid),
        (0x045E, _) => return (Personality::Xbox, vid, pid),
        _ => {},
    }
    let n = name.to_ascii_lowercase();
    if n.contains("xbox wireless controller") || n.starts_with("xbox ") {
        return (Personality::Xbox, 0x045E, if pid != 0 && vid == 0x045E { pid } else { 0x0B13 });
    }
    if n == "pro controller" || n.contains("switch pro") {
        return (Personality::SwitchPro, 0x057E, 0x2009);
    }
    if n.contains("wireless controller") && vid == 0 {
        // what Sony pads call themselves
        return (Personality::DualShock4, 0x054C, 0x09CC);
    }
    (Personality::Generic, vid, pid)
}

impl Personality {
    pub fn label(&self, le: bool) -> &'static str {
        match (self, le) {
            (Personality::Xbox, true) => "Xbox layout, Bluetooth LE",
            (Personality::Xbox, false) => "Xbox layout, classic Bluetooth",
            (Personality::SwitchPro, _) => "Switch Pro layout",
            (Personality::DualShock4, _) => "DualShock 4 layout",
            (Personality::DualSense, _) => "DualSense layout",
            (Personality::Generic, _) => "generic HID layout",
        }
    }
}

/// The Xbox Wireless Controller's report map puts its triggers on the
/// Simulation page (Brake = LT, Accelerator = RT, 10 bits each), which the
/// generic pad parser does not read; rename them Generic Desktop Rx / Ry, the
/// parser's trigger slots. Everything else is copied as is.
pub fn xbox_fix_map(d: &[u8]) -> Vec<u8> {
    let mut out = d.to_vec();
    let mut page = 0u32;
    let mut i = 0usize;
    while i < out.len() {
        let b = out[i];
        if b == 0xFE {
            i += 3 + *out.get(i + 1).unwrap_or(&0) as usize;
            continue;
        }
        let size = match b & 3 {
            3 => 4,
            s => s as usize,
        };
        if i + 1 + size > out.len() {
            break;
        }
        if b == 0x05 && out[i + 1] == 0x02 {
            // Usage Page (Simulation) -> Generic Desktop, if a brake / accelerator follows
            let next_usage = out.get(i + 2..i + 4);
            if matches!(next_usage, Some([0x09, 0xC4]) | Some([0x09, 0xC5])) {
                out[i + 1] = 0x01;
                page = 1;
                i += 2;
                continue;
            }
            page = 2;
        } else if b & 0xFC == 0x04 {
            page = out[i + 1] as u32;
        } else if b == 0x09 && page == 1 && (out[i + 1] == 0xC5 || out[i + 1] == 0xC4) {
            out[i + 1] = if out[i + 1] == 0xC5 { 0x33 } else { 0x34 };
        }
        i += 1 + size;
    }
    out
}

/// The Xbox Bluetooth rumble output report (id 3, without the id): enable the
/// strong and weak motors, magnitudes 0..100 (triggers off), duration in 10 ms
/// units (0 = 0xFF, 2.55 s), no delay, no loop -- as Linux hid-microsoft.
pub fn xbox_ble_rumble(strong: f32, weak: f32, ms: u32) -> [u8; 8] {
    let s = (strong.clamp(0.0, 1.0) * 100.0) as u8;
    let w = (weak.clamp(0.0, 1.0) * 100.0) as u8;
    let dur = (ms / 10).clamp(1, 255) as u8;
    [0x03, 0, 0, s, w, if ms == 0 { 0xFF } else { dur }, 0, 0]
}
