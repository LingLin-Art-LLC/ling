//! Every gamepad, whatever carries it: up to [`MAX_PADS`] pads, each with its
//! decoder ([`super::hidpad::Layout`]), its last state, and a small queue of
//! packets for it (init / LEDs / acks / rumble).
//!
//! Transport-agnostic. A transport (the xHCI driver today, Bluetooth HID
//! later) does four things:
//!   * [`attach`] once the pad is identified (`hidpad::identify`) -> a handle,
//!   * [`report`] with every input report it receives,
//!   * [`next_out`] from its poll loop, sending whatever comes back to the pad
//!     (interrupt OUT on USB, an output report on Bluetooth),
//!   * [`detach`] when the pad goes away.
//!
//! Apps reach the pads through the `pad_*` / `gamepad_*` builtins
//! (hostcompat.rs / lingapp.rs), which only answer while a user is signed in
//! and the session is unlocked. Nothing here turns pad input into keyboard
//! or pointer input.
//!
//! Pure data code (no bus access, no allocation): unit-tested on the host.

use super::hidpad::{self, Layout, OutPkt, PadKind, PadState, Seq};

pub const MAX_PADS: usize = 8;
const QLEN: usize = 12;
/// How long a packet that expects an answer (the Switch handshake) holds the
/// queue before the next one goes anyway.
const REPLY_TIMEOUT_MS: u64 = 300;

const NO_PAD: PadState = PadState { connected: false, axes: [0.0; hidpad::MAX_AXES], buttons: 0, hat: -1, xbox: false };

#[derive(Clone, Copy)]
struct Slot {
    used: bool,
    gen: u8,
    lay: Layout,
    st: PadState,
    vid: u16,
    pid: u16,
    q: [OutPkt; QLEN],
    qh: u8,
    qn: u8,
    /// First byte of the report the last packet waits for (0 = none), and since when.
    wait_reply: u8,
    wait_since: u64,
    /// The GIP / Switch running counter.
    seq: u8,
    /// Rumble asked for (strong, weak), whether it still has to go out, when it stops (0 = never).
    rumble: (f32, f32),
    rumble_dirty: bool,
    rumble_until: u64,
}

impl Slot {
    const EMPTY: Slot = Slot {
        used: false,
        gen: 0,
        lay: Layout::EMPTY,
        st: NO_PAD,
        vid: 0,
        pid: 0,
        q: [OutPkt::EMPTY; QLEN],
        qh: 0,
        qn: 0,
        wait_reply: 0,
        wait_since: 0,
        seq: 0,
        rumble: (0.0, 0.0),
        rumble_dirty: false,
        rumble_until: 0,
    };

    fn push(&mut self, p: OutPkt) {
        if self.qn as usize == QLEN {
            // full (a pad that never takes its packets): drop the oldest
            self.qh = (self.qh + 1) % QLEN as u8;
            self.qn -= 1;
        }
        let i = (self.qh as usize + self.qn as usize) % QLEN;
        self.q[i] = p;
        self.qn += 1;
    }

    fn pop(&mut self) -> Option<OutPkt> {
        if self.qn == 0 {
            return None;
        }
        let p = self.q[self.qh as usize];
        self.qh = (self.qh + 1) % QLEN as u8;
        self.qn -= 1;
        Some(p)
    }

    /// Stamp the running counter into a packet that carries one.
    fn stamp(&mut self, mut p: OutPkt) -> OutPkt {
        match p.seq {
            Seq::Gip if p.len > 2 => {
                self.seq = if self.seq == 255 { 1 } else { self.seq + 1 };
                p.buf[2] = self.seq;
            },
            Seq::Switch if p.len > 1 => {
                p.buf[1] = self.seq & 0x0F;
                self.seq = self.seq.wrapping_add(1);
            },
            _ => {},
        }
        p
    }
}

static mut PADS: [Slot; MAX_PADS] = [Slot::EMPTY; MAX_PADS];

fn pads() -> &'static mut [Slot; MAX_PADS] {
    unsafe { &mut *(&raw mut PADS) }
}

fn slot_of(h: u32) -> Option<&'static mut Slot> {
    let s = pads().get_mut((h & 0xFF) as usize)?;
    (s.used && s.gen == (h >> 8) as u8).then_some(s)
}

/// A pad arrived: take a slot, queue its init packets. `None` when all
/// [`MAX_PADS`] are taken.
pub fn attach(lay: Layout, vid: u16, pid: u16) -> Option<u32> {
    let ps = pads();
    let idx = ps.iter().position(|s| !s.used)?;
    let gen = ps[idx].gen.wrapping_add(1);
    let mut s = Slot::EMPTY;
    s.used = true;
    s.gen = gen;
    s.lay = lay;
    s.vid = vid;
    s.pid = pid;
    // a wireless receiver's interface has a pad only once one links up
    s.st.connected = lay.kind != PadKind::Xbox360Wireless;
    s.st.xbox = matches!(lay.kind, PadKind::Xbox360 | PadKind::XboxOne | PadKind::Xbox360Wireless | PadKind::XboxBle) || lay.xbox360;
    // its LED / lightbar shows the player number it will most likely get
    let player = ps.iter().filter(|o| o.used && o.st.connected).count() as u8;
    hidpad::init_packets(&lay, vid, pid, player, |p| s.push(p));
    ps[idx] = s;
    Some(((gen as u32) << 8) | idx as u32)
}

/// The pad went away (unplugged, link lost).
pub fn detach(h: u32) {
    if let Some(s) = slot_of(h) {
        let gen = s.gen;
        *s = Slot::EMPTY;
        s.gen = gen;
    }
}

/// One input report from the pad. True when it changed the pad's state.
pub fn report(h: u32, r: &[u8]) -> bool {
    let Some(s) = slot_of(h) else { return false };
    if s.wait_reply != 0 && r.first() == Some(&s.wait_reply) {
        s.wait_reply = 0;
    }
    let lay = s.lay;
    hidpad::reply_to(&lay, r, |p| s.push(p));
    let (pb, ph) = (s.st.buttons, s.st.hat);
    let changed = hidpad::decode(&lay, r, &mut s.st);
    // Playing with only a pad is activity: without this the idle lock (Settings > Startup,
    // up to 30 min) parked a game mid-play and switched Bluetooth off under the pad.
    // A button / d-pad edge, or a stick or trigger pushed well past drift, counts.
    if s.st.buttons != pb || s.st.hat != ph || s.st.axes.iter().take(6).any(|a| a.abs() > 0.5) {
        crate::startup::touch();
    }
    changed
}

/// The next packet the transport should send to pad `h`, if any. `now_ms`
/// is any millisecond clock (handshake timeouts, rumble durations).
pub fn next_out(h: u32, now_ms: u64) -> Option<OutPkt> {
    let player = player_of(h).unwrap_or(0) as u8;
    let s = slot_of(h)?;
    if s.wait_reply != 0 {
        if now_ms.saturating_sub(s.wait_since) < REPLY_TIMEOUT_MS {
            return None;
        }
        s.wait_reply = 0;
    }
    if let Some(p) = s.pop() {
        let p = s.stamp(p);
        if p.reply != 0 {
            s.wait_reply = p.reply;
            s.wait_since = now_ms;
        }
        return Some(p);
    }
    if s.rumble_until != 0 && now_ms >= s.rumble_until {
        s.rumble_until = 0;
        if s.rumble != (0.0, 0.0) {
            s.rumble = (0.0, 0.0);
            s.rumble_dirty = true;
        }
    }
    if s.rumble_dirty {
        s.rumble_dirty = false;
        if let Some(p) = hidpad::rumble_packet(&s.lay, s.rumble.0, s.rumble.1, player) {
            return Some(s.stamp(p));
        }
    }
    None
}

/// Slot index of player `i`: the i-th CONNECTED pad in slot order (a
/// wireless receiver's idle interfaces do not take player numbers).
fn slot_of_player(i: usize) -> Option<usize> {
    pads().iter().enumerate().filter(|(_, s)| s.used && s.st.connected).map(|(k, _)| k).nth(i)
}

fn player_of(h: u32) -> Option<usize> {
    let idx = (h & 0xFF) as usize;
    pads().iter().enumerate().filter(|(_, s)| s.used && s.st.connected).position(|(k, _)| k == idx)
}

/// Player `i`'s pad as its last report left it (disconnected when there is none).
pub fn state(i: usize) -> PadState {
    slot_of_player(i).map(|k| pads()[k].st).unwrap_or(NO_PAD)
}

/// Pads connected now.
pub fn count() -> usize {
    pads().iter().filter(|s| s.used && s.st.connected).count()
}

/// Player `i`'s pad family ("DualSense", "Xbox One / Series controller", ...) and USB ids.
pub fn info(i: usize) -> Option<(&'static str, PadKind, u16, u16)> {
    let s = &pads()[slot_of_player(i)?];
    Some((s.lay.name(), s.lay.kind, s.vid, s.pid))
}

/// Set player `i`'s rumble: `strong` (low-frequency motor) and `weak`
/// (high-frequency), 0..1; `ms` > 0 stops it after that long. Sent on the
/// transport's next [`next_out`]. False when the pad cannot rumble.
pub fn set_rumble(i: usize, strong: f32, weak: f32, ms: u32, now_ms: u64) -> bool {
    let Some(k) = slot_of_player(i) else { return false };
    let s = &mut pads()[k];
    if hidpad::rumble_packet(&s.lay, 0.0, 0.0, 0).is_none() {
        return false;
    }
    let v = (strong.clamp(0.0, 1.0), weak.clamp(0.0, 1.0));
    s.rumble_until = if ms > 0 && v != (0.0, 0.0) { now_ms + ms as u64 } else { 0 };
    if v != s.rumble {
        s.rumble = v;
        s.rumble_dirty = true;
    }
    true
}

/// Forget every pad (a controller reset).
#[allow(dead_code)]
pub fn reset() {
    for s in pads().iter_mut() {
        let gen = s.gen;
        *s = Slot::EMPTY;
        s.gen = gen;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The registry is one global: the tests share it, so they run in one function.
    #[test]
    fn registry() {
        reset();
        // Xbox One: power-on first, its GIP sequence stamped (1).
        let h = attach(Layout::XBOXONE, 0x045E, 0x0B12).unwrap();
        let p = next_out(h, 0).unwrap();
        assert_eq!(p.bytes(), &[0x05, 0x20, 0x01, 0x01, 0x00]);
        assert!(next_out(h, 0).is_none());
        assert_eq!(count(), 1);
        assert_eq!(info(0).unwrap().1, PadKind::XboxOne);
        // The Guide button (captured 0x07 with the ACK option): state + an ack echoing seq 0x0E.
        assert!(report(h, &[0x07, 0x30, 0x0E, 0x02, 0x01, 0x5B]));
        assert_eq!(state(0).buttons & (1 << hidpad::B_GUIDE), 1 << hidpad::B_GUIDE);
        let ack = next_out(h, 0).unwrap();
        assert_eq!(ack.bytes(), &[0x01, 0x20, 0x0E, 0x09, 0x00, 0x07, 0x20, 0x02, 0, 0, 0, 0, 0]);
        // Rumble: one packet, then nothing until it changes; stops by itself after 100 ms.
        assert!(set_rumble(0, 1.0, 0.5, 100, 1000));
        let r = next_out(h, 1000).unwrap();
        assert_eq!(r.bytes(), &[0x09, 0x00, 0x02, 0x09, 0x00, 0x0F, 0x00, 0x00, 100, 50, 0xFF, 0x00, 0xFF]);
        assert!(next_out(h, 1050).is_none());
        let stop = next_out(h, 1100).unwrap();
        assert_eq!(&stop.bytes()[8..10], &[0, 0]);
        assert!(next_out(h, 1200).is_none());

        // A wireless receiver interface takes no player number until a pad links.
        let w = attach(Layout::XBOX360_WIRELESS, 0x045E, 0x0719).unwrap();
        assert_eq!(count(), 1);
        let _ = next_out(w, 0);
        // Switch Pro: the handshake waits for each 0x81 reply (or 300 ms).
        let sw = attach(Layout::SWITCHPRO, 0x057E, 0x2009).unwrap();
        assert_eq!(count(), 2);
        assert_eq!(info(1).unwrap().1, PadKind::SwitchPro);
        assert_eq!(next_out(sw, 0).unwrap().bytes(), &[0x80, 0x02]);
        assert!(next_out(sw, 10).is_none()); // waiting for 81 02
        report(sw, &[0x81, 0x02]);
        assert_eq!(next_out(sw, 20).unwrap().bytes(), &[0x80, 0x03]);
        assert!(next_out(sw, 100).is_none());
        assert_eq!(next_out(sw, 400).unwrap().bytes(), &[0x80, 0x02]); // timed out: goes on
        report(sw, &[0x81, 0x02]);
        assert_eq!(next_out(sw, 401).unwrap().bytes(), &[0x80, 0x04]);
        let sub = next_out(sw, 402).unwrap();
        assert_eq!((sub.buf[0], sub.buf[1], sub.buf[10], sub.buf[11]), (0x01, 0x00, 0x03, 0x30));
        report(sw, &[0x21; 20]);
        let lights = next_out(sw, 403).unwrap();
        assert_eq!((sub.len, lights.buf[1], lights.buf[10]), (12, 0x01, 0x30));
        // the receiver's pad links: it becomes a player (slot order: pad 1, between the two)
        assert!(report(w, &[0x08, 0x80]));
        assert_eq!(count(), 3);
        assert_eq!(info(1).unwrap().1, PadKind::Xbox360Wireless);
        detach(h);
        assert_eq!(count(), 2);
        assert_eq!(info(0).unwrap().1, PadKind::Xbox360Wireless);
        assert!(!report(h, &[0x20; 18])); // a stale handle does nothing
        detach(w);
        detach(sw);
        assert_eq!(count(), 0);
        assert!(!state(0).connected);
    }
}
