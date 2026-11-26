//! AVDTP (v1.3) signalling for an A2DP source: discover the headset's sink
//! endpoint, read its SBC capabilities, configure SBC, open the stream (the
//! host then opens the second L2CAP channel, the media transport) and start
//! it. Media packets are RTP + an SBC payload header + SBC frames ([`media`]).

use crate::sbc::{Alloc, Config, Mode};
use alloc::string::String;
use alloc::vec::Vec;

pub const SIG_DISCOVER: u8 = 0x01;
pub const SIG_GET_CAPS: u8 = 0x02;
pub const SIG_SET_CONFIG: u8 = 0x03;
pub const SIG_OPEN: u8 = 0x06;
pub const SIG_START: u8 = 0x07;
pub const SIG_CLOSE: u8 = 0x08;
pub const SIG_SUSPEND: u8 = 0x09;
pub const SIG_ABORT: u8 = 0x0A;
pub const SIG_GET_ALL_CAPS: u8 = 0x0C;
pub const SIG_DELAY_REPORT: u8 = 0x0D;

const MSG_CMD: u8 = 0;
const MSG_GENERAL_REJECT: u8 = 1;
const MSG_ACCEPT: u8 = 2;
const MSG_REJECT: u8 = 3;

/// Our one stream endpoint: an SBC audio source.
pub const OUR_SEID: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    Idle,
    Discovering,
    GettingCaps,
    Configuring,
    Opening,
    /// Open accepted: waiting for the media channel.
    Open,
    Starting,
    Streaming,
    Closed,
}

#[derive(Debug, PartialEq)]
pub enum Event {
    /// A signalling message to send.
    Send(Vec<u8>),
    /// Open the media transport channel now (L2CAP PSM 0x19 again).
    OpenMedia,
    Streaming(Config),
    Suspended,
    Closed,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct Session {
    pub state: State,
    label: u8,
    /// Sink SEIDs not yet tried.
    seps: Vec<u8>,
    remote: u8,
    pub config: Option<Config>,
    /// The rate the mixer runs at: 48000 or 44100 preferred.
    want_rate: u32,
}

fn hdr(label: u8, msg: u8, sig: u8) -> [u8; 2] {
    [(label << 4) | msg, sig & 0x3F]
}

impl Session {
    pub fn new(want_rate: u32) -> Self {
        Session { state: State::Idle, label: 0, seps: Vec::new(), remote: 0, config: None, want_rate }
    }

    fn cmd(&mut self, sig: u8, body: &[u8]) -> Event {
        self.label = (self.label + 1) & 0xF;
        let mut v = hdr(self.label, MSG_CMD, sig).to_vec();
        v.extend_from_slice(body);
        Event::Send(v)
    }

    pub fn start(&mut self) -> Vec<Event> {
        self.state = State::Discovering;
        alloc::vec![self.cmd(SIG_DISCOVER, &[])]
    }

    /// The media channel is open: start streaming.
    pub fn media_opened(&mut self) -> Vec<Event> {
        if self.state != State::Open {
            return Vec::new();
        }
        self.state = State::Starting;
        let r = self.remote << 2;
        alloc::vec![self.cmd(SIG_START, &[r])]
    }

    fn next_sep(&mut self) -> Vec<Event> {
        match self.seps.pop() {
            Some(s) => {
                self.remote = s;
                self.state = State::GettingCaps;
                alloc::vec![self.cmd(SIG_GET_CAPS, &[s << 2])]
            },
            None => {
                self.state = State::Closed;
                alloc::vec![Event::Failed(String::from("the headset has no SBC sink this source can use"))]
            },
        }
    }

    /// A signalling message from the headset.
    pub fn input(&mut self, msg: &[u8]) -> Vec<Event> {
        let (Some(&b0), Some(&b1)) = (msg.first(), msg.get(1)) else { return Vec::new() };
        if (b0 >> 2) & 3 != 0 {
            return Vec::new(); // fragmented signalling: not used by our messages
        }
        let label = b0 >> 4;
        let mtype = b0 & 3;
        let sig = b1 & 0x3F;
        let body = &msg[2..];
        if mtype == MSG_CMD {
            return self.remote_cmd(label, sig, body);
        }
        if label != self.label {
            return Vec::new();
        }
        if mtype == MSG_REJECT || mtype == MSG_GENERAL_REJECT {
            return match self.state {
                State::GettingCaps | State::Configuring => self.next_sep(),
                _ => {
                    self.state = State::Closed;
                    alloc::vec![Event::Failed(alloc::format!("the headset refused AVDTP signal {sig:#04x}"))]
                },
            };
        }
        match (self.state, sig) {
            (State::Discovering, SIG_DISCOVER) => {
                self.seps.clear();
                for e in body.chunks_exact(2).take(16) {
                    let seid = e[0] >> 2;
                    let in_use = e[0] & 0x02 != 0;
                    let media = e[1] >> 4;
                    let sink = e[1] & 0x08 != 0;
                    if !in_use && media == 0 && sink && seid != 0 {
                        self.seps.push(seid);
                    }
                }
                self.seps.reverse();
                self.next_sep()
            },
            (State::GettingCaps, SIG_GET_CAPS) => match sbc_caps(body).and_then(|c| choose(c, self.want_rate)) {
                Some(cfg) => {
                    self.config = Some(cfg);
                    self.state = State::Configuring;
                    let mut b = alloc::vec![self.remote << 2, OUR_SEID << 2, 0x01, 0x00, 0x07, 0x06, 0x00, 0x00];
                    b.extend_from_slice(&cfg.a2dp_bytes());
                    alloc::vec![self.cmd(SIG_SET_CONFIG, &b)]
                },
                None => self.next_sep(),
            },
            (State::Configuring, SIG_SET_CONFIG) => {
                self.state = State::Opening;
                let r = self.remote << 2;
                alloc::vec![self.cmd(SIG_OPEN, &[r])]
            },
            (State::Opening, SIG_OPEN) => {
                self.state = State::Open;
                alloc::vec![Event::OpenMedia]
            },
            (State::Starting, SIG_START) => {
                self.state = State::Streaming;
                alloc::vec![Event::Streaming(self.config.unwrap_or_default())]
            },
            _ => Vec::new(),
        }
    }

    fn reply(&self, label: u8, msg: u8, sig: u8, body: &[u8]) -> Event {
        let mut v = hdr(label, msg, sig).to_vec();
        v.extend_from_slice(body);
        Event::Send(v)
    }

    /// A command from the headset (it may suspend, close, ask about us).
    fn remote_cmd(&mut self, label: u8, sig: u8, body: &[u8]) -> Vec<Event> {
        match sig {
            SIG_DISCOVER => alloc::vec![self.reply(label, MSG_ACCEPT, sig, &[OUR_SEID << 2 | if self.config.is_some() { 2 } else { 0 }, 0x00])],
            SIG_GET_CAPS | SIG_GET_ALL_CAPS => {
                if body.first().map(|b| b >> 2) != Some(OUR_SEID) {
                    return alloc::vec![self.reply(label, MSG_REJECT, sig, &[0x12])]; // bad ACP SEID
                }
                alloc::vec![self.reply(label, MSG_ACCEPT, sig, &[0x01, 0x00, 0x07, 0x06, 0x00, 0x00, 0x3F, 0xF7, 2, 53])]
            },
            SIG_START => {
                let ok = self.state == State::Open || self.state == State::Starting;
                if ok {
                    self.state = State::Streaming;
                    alloc::vec![self.reply(label, MSG_ACCEPT, sig, &[]), Event::Streaming(self.config.unwrap_or_default())]
                } else {
                    alloc::vec![self.reply(label, MSG_REJECT, sig, &[OUR_SEID << 2, 0x31])] // bad state
                }
            },
            SIG_SUSPEND => {
                if self.state == State::Streaming {
                    self.state = State::Open;
                }
                alloc::vec![self.reply(label, MSG_ACCEPT, sig, &[]), Event::Suspended]
            },
            SIG_CLOSE | SIG_ABORT => {
                self.state = State::Closed;
                alloc::vec![self.reply(label, MSG_ACCEPT, sig, &[]), Event::Closed]
            },
            SIG_DELAY_REPORT => alloc::vec![self.reply(label, MSG_ACCEPT, sig, &[])],
            // a headset configuring us (SetConfiguration as initiator) is not supported
            SIG_SET_CONFIG | SIG_OPEN => alloc::vec![self.reply(label, MSG_REJECT, sig, &[0x00, 0x29])],
            _ => alloc::vec![self.reply(label, MSG_GENERAL_REJECT, sig, &[])],
        }
    }
}

/// The SBC codec information element (4 bytes) in a capabilities list.
pub fn sbc_caps(body: &[u8]) -> Option<[u8; 4]> {
    let mut o = 0usize;
    while o + 2 <= body.len() {
        let cat = body[o];
        let len = body[o + 1] as usize;
        let d = body.get(o + 2..o + 2 + len)?;
        if cat == 0x07 && len >= 6 && d[0] >> 4 == 0 && d[1] == 0x00 {
            return d[2..6].try_into().ok();
        }
        o += 2 + len;
    }
    None
}

/// Pick a configuration the headset supports: 48 / 44.1 kHz (the mixer's
/// rate first), stereo or joint stereo, 16 blocks, 8 subbands, loudness,
/// bitpool up to 53 ("high quality").
pub fn choose(c: [u8; 4], want_rate: u32) -> Option<Config> {
    let rates: [(u8, u32); 2] = if want_rate == 44100 { [(0x20, 44100), (0x10, 48000)] } else { [(0x10, 48000), (0x20, 44100)] };
    let (_, rate) = rates.iter().copied().find(|(bit, _)| c[0] & bit != 0)?;
    let mode = if c[0] & 0x02 != 0 {
        Mode::Stereo
    } else if c[0] & 0x01 != 0 {
        Mode::Joint
    } else if c[0] & 0x08 != 0 {
        Mode::Mono
    } else {
        Mode::Dual
    };
    if c[1] & 0x10 == 0 || c[1] & 0x04 == 0 {
        return None; // 16 blocks and 8 subbands are mandatory for sinks; refuse oddities
    }
    let alloc = if c[1] & 0x01 != 0 { Alloc::Loudness } else { Alloc::Snr };
    let (min, max) = (c[2].max(2), c[3]);
    if min > max {
        return None;
    }
    let bitpool = max.min(53).max(min);
    Some(Config { rate, mode, alloc, blocks: 16, bitpool })
}

/// One media packet: RTP header (version 2, payload type 96), the SBC payload
/// header (frame count) and the frames.
pub fn media(seq: u16, timestamp: u32, frames: &[u8], n_frames: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity(13 + frames.len());
    v.extend_from_slice(&[0x80, 0x60]);
    v.extend_from_slice(&seq.to_be_bytes());
    v.extend_from_slice(&timestamp.to_be_bytes());
    v.extend_from_slice(&1u32.to_be_bytes()); // SSRC
    v.push(n_frames & 0x0F);
    v.extend_from_slice(frames);
    v
}
