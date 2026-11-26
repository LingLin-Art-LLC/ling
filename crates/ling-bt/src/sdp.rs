//! SDP client (Core v5.3 Vol 3 Part B), outbound only: a classic HID
//! device's report descriptor (HID service 0x1124, attribute 0x0206
//! HIDDescriptorList) and its Device ID record (0x1200: vendor 0x0201,
//! product 0x0202). Our own SDP server answers nothing (the host refuses
//! incoming PSM 1).
//!
//! Data elements are parsed with a depth limit and length checks; responses
//! are reassembled through continuation states up to 16 KiB.

use alloc::string::String;
use alloc::vec::Vec;

pub const PSM_SDP: u16 = 0x0001;
const SSA_REQ: u8 = 0x06;
const SSA_RSP: u8 = 0x07;
const MAX_TOTAL: usize = 16 * 1024;

/// A parsed data element.
#[derive(Debug, PartialEq, Clone)]
pub enum De {
    Nil,
    Uint(u64),
    Int(i64),
    Uuid(Vec<u8>),
    Str(Vec<u8>),
    Bool(bool),
    Seq(Vec<De>),
    Other,
}

/// Parse one data element at the start of `b`: (element, bytes used).
pub fn parse_de(b: &[u8], depth: u8) -> Option<(De, usize)> {
    if depth > 8 {
        return None;
    }
    let h = *b.first()?;
    let (ty, si) = (h >> 3, h & 7);
    let (len, hdr): (usize, usize) = match si {
        0 if ty == 0 => (0, 1),
        0 => (1, 1),
        1 => (2, 1),
        2 => (4, 1),
        3 => (8, 1),
        4 => (16, 1),
        5 => (*b.get(1)? as usize, 2),
        6 => (le16_be(b, 1)? as usize, 3),
        _ => (u32::from_be_bytes([*b.get(1)?, *b.get(2)?, *b.get(3)?, *b.get(4)?]) as usize, 5),
    };
    let d = b.get(hdr..hdr.checked_add(len)?)?;
    let be = |d: &[u8]| d.iter().fold(0u64, |a, &x| (a << 8) | x as u64);
    let e = match ty {
        0 => De::Nil,
        1 if len <= 8 => De::Uint(be(d)),
        2 if len <= 8 => De::Int(be(d) as i64),
        3 => De::Uuid(d.to_vec()),
        4 | 8 => De::Str(d.to_vec()),
        5 => De::Bool(d.first().is_some_and(|&x| x != 0)),
        6 | 7 => {
            let mut v = Vec::new();
            let mut o = 0;
            while o < d.len() && v.len() < 256 {
                let (e, n) = parse_de(&d[o..], depth + 1)?;
                v.push(e);
                o += n;
            }
            De::Seq(v)
        },
        _ => De::Other,
    };
    Some((e, hdr + len))
}

fn le16_be(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(o)?, *b.get(o + 1)?]))
}

/// Attribute `id` of the first record in an AttributeLists element.
fn attr(lists: &De, id: u16) -> Option<&De> {
    let De::Seq(records) = lists else { return None };
    for r in records {
        let De::Seq(kv) = r else { continue };
        for pair in kv.chunks(2) {
            if let [De::Uint(k), v] = pair {
                if *k == id as u64 {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// The report descriptor in a HIDDescriptorList (the first of type 0x22).
pub fn hid_descriptor(lists: &De) -> Option<Vec<u8>> {
    let De::Seq(list) = attr(lists, 0x0206)? else { return None };
    for d in list {
        if let De::Seq(x) = d {
            if let [De::Uint(0x22), De::Str(s)] = x.as_slice() {
                return Some(s[..s.len().min(4096)].to_vec());
            }
        }
    }
    None
}

/// (vendor, product) from a Device ID record.
pub fn device_id(lists: &De) -> Option<(u16, u16)> {
    let v = match attr(lists, 0x0201)? {
        De::Uint(v) => *v as u16,
        _ => return None,
    };
    let p = match attr(lists, 0x0202)? {
        De::Uint(v) => *v as u16,
        _ => return None,
    };
    Some((v, p))
}

fn ssa_request(tid: u16, uuid: u16, attrs: (u16, u16), cont: &[u8]) -> Vec<u8> {
    let mut p = alloc::vec![0x35, 3, 0x19];
    p.extend_from_slice(&uuid.to_be_bytes());
    p.extend_from_slice(&0x0400u16.to_be_bytes()); // up to 1 KiB per response
    p.extend_from_slice(&[0x35, 5, 0x0A]);
    p.extend_from_slice(&attrs.0.to_be_bytes());
    p.extend_from_slice(&attrs.1.to_be_bytes());
    p.push(cont.len().min(16) as u8);
    p.extend_from_slice(&cont[..cont.len().min(16)]);
    let mut v = alloc::vec![SSA_REQ];
    v.extend_from_slice(&tid.to_be_bytes());
    v.extend_from_slice(&(p.len() as u16).to_be_bytes());
    v.extend(p);
    v
}

#[derive(Debug, PartialEq)]
pub enum Event {
    Send(Vec<u8>),
    /// The HID report descriptor (empty when the device has none) and the Device ID.
    Done { desc: Vec<u8>, vid: u16, pid: u16 },
    Failed(String),
}

#[derive(Default, Clone, Debug)]
pub struct Client {
    step: u8,
    tid: u16,
    buf: Vec<u8>,
    desc: Vec<u8>,
    ids: (u16, u16),
}

impl Client {
    pub fn start(&mut self) -> Vec<Event> {
        self.step = 0;
        self.buf.clear();
        alloc::vec![self.request(&[])]
    }

    fn request(&mut self, cont: &[u8]) -> Event {
        self.tid = self.tid.wrapping_add(1);
        Event::Send(if self.step == 0 { ssa_request(self.tid, 0x1124, (0x0206, 0x0206), cont) } else { ssa_request(self.tid, 0x1200, (0x0201, 0x0202), cont) })
    }

    pub fn input(&mut self, p: &[u8]) -> Vec<Event> {
        let (Some(&id), Some(tid)) = (p.first(), le16_be(p, 1)) else { return Vec::new() };
        if tid != self.tid || self.step > 1 {
            return Vec::new();
        }
        if id != SSA_RSP {
            // an error response: this record is missing; go on without it
            return self.finish_step();
        }
        let Some(cnt) = le16_be(p, 5) else { return self.fail("short SDP response") };
        let Some(lists) = p.get(7..7 + cnt as usize) else { return self.fail("truncated SDP response") };
        if self.buf.len() + lists.len() > MAX_TOTAL {
            return self.fail("SDP response too large");
        }
        self.buf.extend_from_slice(lists);
        let cl = p.get(7 + cnt as usize).copied().unwrap_or(0) as usize;
        let Some(cont) = p.get(8 + cnt as usize..8 + cnt as usize + cl) else { return self.fail("bad continuation state") };
        if cl > 16 {
            return self.fail("bad continuation state");
        }
        if cl > 0 {
            let cont = cont.to_vec();
            return alloc::vec![self.request(&cont)];
        }
        let parsed = parse_de(&self.buf, 0).map(|x| x.0);
        if let Some(l) = parsed {
            if self.step == 0 {
                self.desc = hid_descriptor(&l).unwrap_or_default();
            } else {
                self.ids = device_id(&l).unwrap_or((0, 0));
            }
        }
        self.finish_step()
    }

    fn finish_step(&mut self) -> Vec<Event> {
        self.buf.clear();
        self.step += 1;
        if self.step == 1 {
            return alloc::vec![self.request(&[])];
        }
        alloc::vec![Event::Done { desc: core::mem::take(&mut self.desc), vid: self.ids.0, pid: self.ids.1 }]
    }

    fn fail(&mut self, why: &str) -> Vec<Event> {
        self.step = 9;
        alloc::vec![Event::Failed(String::from(why))]
    }
}
