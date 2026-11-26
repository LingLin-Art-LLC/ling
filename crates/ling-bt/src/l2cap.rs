//! L2CAP (Core v5.3 Vol 3 Part A), basic mode only: the ACL reassembly,
//! connection-oriented channels and the signalling channel (CID 1).
//!
//! Incoming connection requests are NOT accepted here: they come out as
//! [`Note::Incoming`] and the host's security policy answers them with
//! [`L2cap::accept`] or [`L2cap::refuse`].

use crate::le16;
use alloc::vec::Vec;

pub const CID_SIGNALING: u16 = 0x0001;
pub const PSM_SDP: u16 = 0x0001;
pub const PSM_HID_CONTROL: u16 = 0x0011;
pub const PSM_HID_INTERRUPT: u16 = 0x0013;
pub const PSM_AVDTP: u16 = 0x0019;

/// Connection response results.
pub const RES_SUCCESS: u16 = 0x0000;
pub const RES_PENDING: u16 = 0x0001;
pub const RES_PSM_NOT_SUPPORTED: u16 = 0x0002;
pub const RES_SECURITY_BLOCK: u16 = 0x0003;
pub const RES_NO_RESOURCES: u16 = 0x0004;

/// Largest L2CAP frame we reassemble (HID reports and AVDTP signalling are small).
pub const MAX_SDU: usize = 2048;
/// Channels per ACL link / in all.
pub const MAX_CHANNELS_PER_LINK: usize = 8;
pub const MAX_CHANNELS: usize = 24;
/// The MTU we announce (what we accept).
pub const OUR_MTU: u16 = 1024;

const CODE_REJECT: u8 = 0x01;
const CODE_CONN_REQ: u8 = 0x02;
const CODE_CONN_RSP: u8 = 0x03;
const CODE_CONF_REQ: u8 = 0x04;
const CODE_CONF_RSP: u8 = 0x05;
const CODE_DISC_REQ: u8 = 0x06;
const CODE_DISC_RSP: u8 = 0x07;
const CODE_ECHO_REQ: u8 = 0x08;
const CODE_ECHO_RSP: u8 = 0x09;
const CODE_INFO_REQ: u8 = 0x0A;
const CODE_INFO_RSP: u8 = 0x0B;

/// A basic-mode frame: length, channel id, payload.
pub fn frame(cid: u16, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + payload.len());
    v.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    v.extend_from_slice(&cid.to_le_bytes());
    v.extend_from_slice(payload);
    v
}

/// Rebuilds L2CAP frames from ACL packets, per connection handle. A
/// continuation without a start, an oversize frame or a new start in the
/// middle of a frame drops what was collected.
#[derive(Default)]
pub struct Reassembler {
    partial: Vec<(u16, Vec<u8>, usize)>,
    pub dropped: u32,
}

impl Reassembler {
    /// Feed one ACL packet's data; a whole frame comes out as (cid, payload).
    pub fn push(&mut self, handle: u16, pb: u8, data: &[u8]) -> Option<(u16, Vec<u8>)> {
        let idx = self.partial.iter().position(|p| p.0 == handle);
        if pb == crate::hci::PB_CONT {
            let Some(i) = idx else {
                self.dropped += 1;
                return None;
            };
            let p = &mut self.partial[i];
            if p.1.len() + data.len() > p.2 {
                self.partial.remove(i);
                self.dropped += 1;
                return None;
            }
            p.1.extend_from_slice(data);
            if p.1.len() == p.2 {
                let (_, buf, _) = self.partial.remove(i);
                return split_frame(&buf);
            }
            return None;
        }
        // a start fragment
        if let Some(i) = idx {
            self.partial.remove(i);
            self.dropped += 1;
        }
        let len = le16(data, 0)? as usize;
        if len > MAX_SDU {
            self.dropped += 1;
            return None;
        }
        let total = 4 + len;
        if data.len() > total {
            self.dropped += 1;
            return None;
        }
        if data.len() == total {
            return split_frame(data);
        }
        if self.partial.len() >= 8 {
            self.partial.remove(0);
        }
        self.partial.push((handle, data.to_vec(), total));
        None
    }

    pub fn link_down(&mut self, handle: u16) {
        self.partial.retain(|p| p.0 != handle);
    }
}

fn split_frame(b: &[u8]) -> Option<(u16, Vec<u8>)> {
    let len = le16(b, 0)? as usize;
    let cid = le16(b, 2)?;
    Some((cid, b.get(4..4 + len)?.to_vec()))
}

// ── signalling ──────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Clone)]
pub enum Sig {
    Reject { id: u8, reason: u16 },
    ConnReq { id: u8, psm: u16, scid: u16 },
    ConnRsp { id: u8, dcid: u16, scid: u16, result: u16 },
    ConfReq { id: u8, dcid: u16, flags: u16, mtu: Option<u16>, mode: Option<u8> },
    ConfRsp { id: u8, scid: u16, flags: u16, result: u16 },
    DiscReq { id: u8, dcid: u16, scid: u16 },
    DiscRsp { id: u8, dcid: u16, scid: u16 },
    EchoReq { id: u8 },
    EchoRsp { id: u8 },
    InfoReq { id: u8, info_type: u16 },
    InfoRsp { id: u8 },
    Unknown { code: u8, id: u8 },
}

/// Every command in a signalling frame (at most 16; a truncated one ends the list).
pub fn parse_sig(p: &[u8]) -> Vec<Sig> {
    let mut out = Vec::new();
    let mut o = 0usize;
    while o + 4 <= p.len() && out.len() < 16 {
        let code = p[o];
        let id = p[o + 1];
        let Some(len) = le16(p, o + 2) else { break };
        let Some(d) = p.get(o + 4..o + 4 + len as usize) else { break };
        let s = match code {
            CODE_REJECT => le16(d, 0).map(|reason| Sig::Reject { id, reason }),
            CODE_CONN_REQ => (|| Some(Sig::ConnReq { id, psm: le16(d, 0)?, scid: le16(d, 2)? }))(),
            CODE_CONN_RSP => (|| Some(Sig::ConnRsp { id, dcid: le16(d, 0)?, scid: le16(d, 2)?, result: le16(d, 4)? }))(),
            CODE_CONF_REQ => (|| {
                let (mut mtu, mut mode) = (None, None);
                let mut q = 4usize;
                while q + 2 <= d.len() {
                    let ty = d[q] & 0x7F;
                    let l = d[q + 1] as usize;
                    let v = d.get(q + 2..q + 2 + l)?;
                    match ty {
                        0x01 => mtu = le16(v, 0),
                        0x04 => mode = v.first().copied(),
                        _ => {},
                    }
                    q += 2 + l;
                }
                Some(Sig::ConfReq { id, dcid: le16(d, 0)?, flags: le16(d, 2)?, mtu, mode })
            })(),
            CODE_CONF_RSP => (|| Some(Sig::ConfRsp { id, scid: le16(d, 0)?, flags: le16(d, 2)?, result: le16(d, 4)? }))(),
            CODE_DISC_REQ => (|| Some(Sig::DiscReq { id, dcid: le16(d, 0)?, scid: le16(d, 2)? }))(),
            CODE_DISC_RSP => (|| Some(Sig::DiscRsp { id, dcid: le16(d, 0)?, scid: le16(d, 2)? }))(),
            CODE_ECHO_REQ => Some(Sig::EchoReq { id }),
            CODE_ECHO_RSP => Some(Sig::EchoRsp { id }),
            CODE_INFO_REQ => le16(d, 0).map(|info_type| Sig::InfoReq { id, info_type }),
            CODE_INFO_RSP => Some(Sig::InfoRsp { id }),
            _ => Some(Sig::Unknown { code, id }),
        };
        // a command too short for its kind is rejected (command not understood)
        out.push(s.unwrap_or(Sig::Unknown { code: 0, id }));
        o += 4 + len as usize;
    }
    out
}

fn cmd(code: u8, id: u8, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + data.len());
    v.push(code);
    v.push(id);
    v.extend_from_slice(&(data.len() as u16).to_le_bytes());
    v.extend_from_slice(data);
    v
}

fn w16s(v: &[u16]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub fn sig_conn_req(id: u8, psm: u16, scid: u16) -> Vec<u8> {
    cmd(CODE_CONN_REQ, id, &w16s(&[psm, scid]))
}
pub fn sig_conn_rsp(id: u8, dcid: u16, scid: u16, result: u16) -> Vec<u8> {
    cmd(CODE_CONN_RSP, id, &w16s(&[dcid, scid, result, 0]))
}
/// Configuration request with our MTU.
pub fn sig_conf_req(id: u8, dcid: u16) -> Vec<u8> {
    let mut d = w16s(&[dcid, 0]);
    d.extend_from_slice(&[0x01, 2]);
    d.extend_from_slice(&OUR_MTU.to_le_bytes());
    cmd(CODE_CONF_REQ, id, &d)
}
pub fn sig_conf_rsp(id: u8, scid: u16, result: u16, opts: &[u8]) -> Vec<u8> {
    let mut d = w16s(&[scid, 0, result]);
    d.extend_from_slice(opts);
    cmd(CODE_CONF_RSP, id, &d)
}
pub fn sig_disc_req(id: u8, dcid: u16, scid: u16) -> Vec<u8> {
    cmd(CODE_DISC_REQ, id, &w16s(&[dcid, scid]))
}
pub fn sig_disc_rsp(id: u8, dcid: u16, scid: u16) -> Vec<u8> {
    cmd(CODE_DISC_RSP, id, &w16s(&[dcid, scid]))
}
/// Command not understood (reason 0).
pub fn sig_reject(id: u8) -> Vec<u8> {
    cmd(CODE_REJECT, id, &[0, 0])
}

// ── channels ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    /// Our Connection Request is out.
    WaitConnRsp,
    /// Connected; configuring both directions.
    Config,
    Open,
    /// Our Disconnection Request is out.
    WaitDisc,
}

#[derive(Clone, Debug)]
pub struct Chan {
    pub handle: u16,
    pub psm: u16,
    pub local: u16,
    pub remote: u16,
    pub state: State,
    /// Our configuration was accepted / theirs was answered.
    out_done: bool,
    in_done: bool,
    /// What the peer accepts (its MTU), for our sends.
    pub remote_mtu: u16,
    req_id: u8,
}

#[derive(Debug, PartialEq)]
pub enum Note {
    /// A device asks to open `psm`; answer with accept / refuse.
    Incoming { handle: u16, psm: u16, id: u8, scid: u16 },
    Opened { handle: u16, psm: u16, cid: u16 },
    Data { handle: u16, psm: u16, cid: u16, data: Vec<u8> },
    Closed { handle: u16, psm: u16, cid: u16 },
    /// Our connection request was refused (or the peer had no resources).
    Refused { handle: u16, psm: u16, result: u16 },
}

/// A complete L2CAP frame to send on a link.
pub type Out = (u16, Vec<u8>);

pub struct L2cap {
    pub chans: Vec<Chan>,
    next_cid: u16,
    next_id: u8,
}

impl Default for L2cap {
    fn default() -> Self {
        L2cap { chans: Vec::new(), next_cid: 0x0040, next_id: 1 }
    }
}

impl L2cap {
    fn id(&mut self) -> u8 {
        let i = self.next_id;
        self.next_id = if self.next_id == 255 { 1 } else { self.next_id + 1 };
        i
    }

    fn alloc_cid(&mut self) -> u16 {
        loop {
            let c = self.next_cid;
            self.next_cid = if self.next_cid >= 0x0FFF { 0x0040 } else { self.next_cid + 1 };
            if !self.chans.iter().any(|ch| ch.local == c) {
                return c;
            }
        }
    }

    fn room(&self, handle: u16) -> bool {
        self.chans.len() < MAX_CHANNELS && self.chans.iter().filter(|c| c.handle == handle).count() < MAX_CHANNELS_PER_LINK
    }

    pub fn find(&self, handle: u16, psm: u16) -> Option<&Chan> {
        self.chans.iter().find(|c| c.handle == handle && c.psm == psm)
    }

    pub fn is_open(&self, handle: u16, psm: u16) -> bool {
        self.find(handle, psm).is_some_and(|c| c.state == State::Open)
    }

    /// Open a channel to `psm` on the link; None when out of channels.
    pub fn connect(&mut self, handle: u16, psm: u16, out: &mut Vec<Out>) -> Option<u16> {
        if !self.room(handle) {
            return None;
        }
        let local = self.alloc_cid();
        let id = self.id();
        self.chans.push(Chan { handle, psm, local, remote: 0, state: State::WaitConnRsp, out_done: false, in_done: false, remote_mtu: 672, req_id: id });
        out.push((handle, frame(CID_SIGNALING, &sig_conn_req(id, psm, local))));
        Some(local)
    }

    /// Accept an [`Note::Incoming`] request (and start configuring).
    pub fn accept(&mut self, handle: u16, id: u8, psm: u16, scid: u16, out: &mut Vec<Out>) {
        if !self.room(handle) || self.chans.iter().any(|c| c.handle == handle && c.remote == scid) {
            out.push((handle, frame(CID_SIGNALING, &sig_conn_rsp(id, 0, scid, RES_NO_RESOURCES))));
            return;
        }
        let local = self.alloc_cid();
        let cid = self.id();
        self.chans.push(Chan { handle, psm, local, remote: scid, state: State::Config, out_done: false, in_done: false, remote_mtu: 672, req_id: cid });
        out.push((handle, frame(CID_SIGNALING, &sig_conn_rsp(id, local, scid, RES_SUCCESS))));
        out.push((handle, frame(CID_SIGNALING, &sig_conf_req(cid, scid))));
    }

    pub fn refuse(&mut self, handle: u16, id: u8, scid: u16, result: u16, out: &mut Vec<Out>) {
        out.push((handle, frame(CID_SIGNALING, &sig_conn_rsp(id, 0, scid, result))));
    }

    /// The frame that sends `data` on our channel `cid`, if it is open.
    pub fn send(&self, cid: u16, data: &[u8]) -> Option<Out> {
        let c = self.chans.iter().find(|c| c.local == cid && c.state == State::Open)?;
        Some((c.handle, frame(c.remote, data)))
    }

    pub fn disconnect(&mut self, cid: u16, out: &mut Vec<Out>) {
        let id = self.id();
        if let Some(c) = self.chans.iter_mut().find(|c| c.local == cid) {
            if c.remote != 0 {
                out.push((c.handle, frame(CID_SIGNALING, &sig_disc_req(id, c.remote, c.local))));
                c.state = State::WaitDisc;
            }
        }
    }

    /// The ACL link went down: every channel on it is gone.
    pub fn link_down(&mut self, handle: u16, notes: &mut Vec<Note>) {
        for c in self.chans.iter().filter(|c| c.handle == handle) {
            if c.state == State::Open {
                notes.push(Note::Closed { handle, psm: c.psm, cid: c.local });
            }
        }
        self.chans.retain(|c| c.handle != handle);
    }

    /// A reassembled frame from the link.
    pub fn input(&mut self, handle: u16, cid: u16, payload: &[u8], out: &mut Vec<Out>, notes: &mut Vec<Note>) {
        if cid == CID_SIGNALING {
            for s in parse_sig(payload) {
                self.signal(handle, s, out, notes);
            }
            return;
        }
        if let Some(c) = self.chans.iter().find(|c| c.handle == handle && c.local == cid && c.state == State::Open) {
            notes.push(Note::Data { handle, psm: c.psm, cid, data: payload.to_vec() });
        }
        // data on a channel that is not open (or not ours) is dropped
    }

    fn opened_check(&mut self, i: usize, notes: &mut Vec<Note>) {
        let c = &mut self.chans[i];
        if c.state == State::Config && c.in_done && c.out_done {
            c.state = State::Open;
            notes.push(Note::Opened { handle: c.handle, psm: c.psm, cid: c.local });
        }
    }

    fn signal(&mut self, handle: u16, s: Sig, out: &mut Vec<Out>, notes: &mut Vec<Note>) {
        let sig = |out: &mut Vec<Out>, p: Vec<u8>| out.push((handle, frame(CID_SIGNALING, &p)));
        match s {
            Sig::ConnReq { id, psm, scid } => notes.push(Note::Incoming { handle, psm, id, scid }),
            Sig::ConnRsp { id, dcid, scid, result } => {
                let Some(i) = self.chans.iter().position(|c| c.handle == handle && c.local == scid && c.state == State::WaitConnRsp && c.req_id == id) else {
                    return;
                };
                match result {
                    RES_SUCCESS => {
                        let cid = self.id();
                        let c = &mut self.chans[i];
                        c.remote = dcid;
                        c.state = State::Config;
                        c.req_id = cid;
                        sig(out, sig_conf_req(cid, dcid));
                    },
                    RES_PENDING => {},
                    r => {
                        let c = self.chans.remove(i);
                        notes.push(Note::Refused { handle, psm: c.psm, result: r });
                    },
                }
            },
            Sig::ConfReq { id, dcid, flags: _, mtu, mode } => {
                let Some(i) = self.chans.iter().position(|c| c.handle == handle && c.local == dcid && matches!(c.state, State::Config | State::Open)) else {
                    sig(out, cmd(CODE_REJECT, id, &w16s(&[0x0002, dcid, 0])));
                    return;
                };
                if mode.is_some_and(|m| m != 0) {
                    // only basic mode: answer "unacceptable parameters" proposing basic
                    sig(out, sig_conf_rsp(id, self.chans[i].remote, 0x0001, &[0x04, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
                    return;
                }
                let c = &mut self.chans[i];
                c.remote_mtu = mtu.unwrap_or(672).max(48);
                c.in_done = true;
                sig(out, sig_conf_rsp(id, c.remote, 0, &[]));
                self.opened_check(i, notes);
            },
            Sig::ConfRsp { id, scid, flags: _, result } => {
                let Some(i) = self.chans.iter().position(|c| c.handle == handle && c.local == scid && c.state == State::Config && c.req_id == id) else {
                    return;
                };
                if result == 0 {
                    self.chans[i].out_done = true;
                    self.opened_check(i, notes);
                } else {
                    // they refused our MTU: give up on the channel
                    let c = self.chans.remove(i);
                    sig(out, sig_disc_req(0x7F, c.remote, c.local));
                    notes.push(Note::Refused { handle, psm: c.psm, result });
                }
            },
            Sig::DiscReq { id, dcid, scid } => {
                if let Some(i) = self.chans.iter().position(|c| c.handle == handle && c.local == dcid && c.remote == scid) {
                    let c = self.chans.remove(i);
                    if c.state == State::Open {
                        notes.push(Note::Closed { handle, psm: c.psm, cid: c.local });
                    }
                }
                sig(out, sig_disc_rsp(id, dcid, scid));
            },
            Sig::DiscRsp { dcid: _, scid, .. } => {
                if let Some(i) = self.chans.iter().position(|c| c.handle == handle && c.local == scid) {
                    let c = self.chans.remove(i);
                    notes.push(Note::Closed { handle, psm: c.psm, cid: c.local });
                }
            },
            Sig::EchoReq { id } => sig(out, cmd(CODE_ECHO_RSP, id, &[])),
            // no extended features, no fixed channels: "not supported"
            Sig::InfoReq { id, info_type } => sig(out, cmd(CODE_INFO_RSP, id, &w16s(&[info_type, 0x0001]))),
            Sig::Unknown { id, .. } => sig(out, sig_reject(id)),
            Sig::Reject { .. } | Sig::EchoRsp { .. } | Sig::InfoRsp { .. } => {},
        }
    }
}
