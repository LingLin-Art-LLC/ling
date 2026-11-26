//! Intel High Definition Audio driver (ICH6/ICH8-style HDA controllers and
//! their codecs: PCH + Realtek, GPU HDMI functions) -- the audio device new VirtualBox VMs get by default ("Intel HD
//! Audio") and QEMU's `-device intel-hda -device hda-output` /
//! `-device ich9-intel-hda`. 48 kHz 16-bit stereo, polled: no interrupts,
//! the mixer's per-frame [`pump`] keeps the ring fed (the same disclosed
//! pull model as `ac97.rs`). Capture from the analog input jacks (mic /
//! line in) runs on an input stream the same way -- see "inputs / capture"
//! below. Settings > Sound picks the output and input by the keys
//! [`output_key`] / [`input_key`] (controller PCI address, codec, pin).
//!
//! Bring-up:
//! 1. Every PCI class 0x04/0x03 function (any prog-if: an Intel PCH with
//!    its DSP enabled reports 0x040380); memory BAR0 (64-bit capable -- a
//!    BAR above 4 GiB is mapped uncached with `paging::map_mmio`); PCI D0,
//!    bus master, TCSEL / DMA snoop set up the way Linux's hda_intel does.
//! 2. Controller reset (GCTL.CRST 0 -> 1), codecs self-enumerate into
//!    STATESTS; every SDIN bit is a codec and every codec is probed.
//! 3. Codec commands go through the CORB/RIRB rings (polled); if the rings
//!    don't answer, the Immediate Command interface (ICOI/IRII/ICIS) is
//!    used instead. One controller owns the (shared static) rings at a time.
//! 4. Probe (`scan_all`): each codec's audio function group is walked;
//!    every output-capable pin with a path to a converter is a candidate,
//!    scored from its default config (jack / fixed / none; HP / line out /
//!    speaker / HDMI; rear / front) and presence detect -- a plugged analog
//!    jack wins, HDMI is the fallback (`pin_score`). `audio pins` lists
//!    them, `audio use <n|auto>` re-routes.
//! 5. The winner's controller is reset again and its codec programmed:
//!    AFG + widgets to D0, Realtek COEF init where Linux does one, every
//!    converter of the pin's kind on stream tag 1 / channel 0 at 48 kHz
//!    16-bit stereo (format 0x0011, so whichever DAC an emulator treats as
//!    "front" plays it), the path pin -> mixers/selectors -> DAC with
//!    connections selected, the path input unmuted at 0 dB (other mixer
//!    inputs muted), OUT_EN (+HP_EN for headphones), EAPD. In auto mode
//!    the other analog output jacks of that codec are routed too, so a
//!    jack plugged later plays without polling.
//! 6. One output stream descriptor (index = number of input streams) runs a
//!    cyclic BDL of NUM_BUFFERS buffers (FRAMES_PER_BUFFER stereo frames
//!    each, 128-byte aligned). The ring never stops on its own, so
//!    [`pump`] reads LPIB to find the playing buffer, zeroes buffers the
//!    hardware has finished with (an underrun then plays silence, never a
//!    stale repeat) and keeps QUEUE_DEPTH buffers rendered ahead.
//! 7. Because the ring never stops by itself, [`watchdog`] (called from the
//!    100 Hz timer IRQ) stops the stream once it has run STOP_AFTER_SILENT
//!    buffers into silence; the next pump resets the stream (SRST) and
//!    re-primes it. Stopping only after silence matters: emulated codecs
//!    buffer audio past LPIB and a stop discards it (QEMU: 2 buffers).

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use crate::arch::{pci, timer};
use crate::drivers::serial;

/// Capture buffers (the input ring below) stay 1024 frames.
pub const FRAMES_PER_BUFFER: usize = 1024;
const SAMPLES_PER_BUFFER: usize = FRAMES_PER_BUFFER * 2; // stereo
const BUFFER_BYTES: usize = SAMPLES_PER_BUFFER * 2;
/// Output: a ring of short buffers (256 frames = 5.3 ms) so the queue ahead of
/// the speaker can be as short as the app's frame pacing allows -- the mixer
/// sets how many are kept queued ([`set_target`]) from how often it is pumped.
pub const NUM_BUFFERS: usize = 128;
pub const OUT_FRAMES: usize = 256;
const OUT_SAMPLES: usize = OUT_FRAMES * 2;
const OUT_BYTES: usize = OUT_SAMPLES * 2;
/// The most buffers ever kept rendered ahead of the playing one (~340 ms).
pub const QUEUE_DEPTH: usize = 64;
/// The fewest (~21 ms): below that a busy frame would starve the codec.
pub const MIN_QUEUE: usize = 4;
static mut TARGET: usize = 32;
/// The watchdog stops a starved stream only after it has fetched this many
/// silent buffers (~170ms) past the last rendered one. Must stay below
/// NUM_BUFFERS - QUEUE_DEPTH: that's how much silence lies ahead of the
/// queue before the ring would wrap into already-played audio.
const STOP_AFTER_SILENT: usize = 32;

/// How many output buffers the pump keeps queued (MIN_QUEUE..=QUEUE_DEPTH).
pub fn set_target(n: usize) {
    unsafe { TARGET = n.clamp(MIN_QUEUE, QUEUE_DEPTH) };
}
pub fn target() -> usize {
    unsafe { TARGET }
}

/// Stereo frames rendered but not yet played (from the DMA position), 0 while stopped.
pub fn queued_frames() -> usize {
    unsafe {
        if !present() || !read_volatile(&raw const STARTED) {
            return 0;
        }
        let base = OUT_BASE;
        let sd = SD;
        if br32(base, sd + SD_CTL) & SD_CTL_RUN == 0 {
            return 0;
        }
        let ring = OUT_BYTES * NUM_BUFFERS;
        let lpib = br32(base, sd + SD_LPIB) as usize % ring;
        let write = read_volatile(&raw const WRITE) * OUT_BYTES;
        ((write + ring - lpib) % ring) / 4
    }
}

/// 48 kHz base, x1, /1, 16 bits/sample, 2 channels.
const STREAM_FORMAT: u16 = 0x0011;
const STREAM_TAG: u32 = 1;

// Controller registers (byte offsets from BAR0).
const GCAP: usize = 0x00; // 16
const GCTL: usize = 0x08; // 32
const STATESTS: usize = 0x0E; // 16
const INTCTL: usize = 0x20; // 32
const CORBLBASE: usize = 0x40;
const CORBUBASE: usize = 0x44;
const CORBWP: usize = 0x48; // 16
const CORBRP: usize = 0x4A; // 16
const CORBCTL: usize = 0x4C; // 8
const CORBSIZE: usize = 0x4E; // 8
const RIRBLBASE: usize = 0x50;
const RIRBUBASE: usize = 0x54;
const RIRBWP: usize = 0x58; // 16
const RINTCNT: usize = 0x5A; // 16
const RIRBCTL: usize = 0x5C; // 8
const RIRBSTS: usize = 0x5D; // 8
const RIRBSIZE: usize = 0x5E; // 8
const ICOI: usize = 0x60; // 32
const IRII: usize = 0x64; // 32
const ICIS: usize = 0x68; // 16
const DPLBASE: usize = 0x70;
const DPUBASE: usize = 0x74;

// Stream-descriptor registers (offsets within a descriptor).
const SD_CTL: usize = 0x00; // 24 bits + STS byte at 0x03
const SD_STS: usize = 0x03; // 8
const SD_LPIB: usize = 0x04;
const SD_CBL: usize = 0x08;
const SD_LVI: usize = 0x0C; // 16
const SD_FMT: usize = 0x12; // 16
const SD_BDPL: usize = 0x18;
const SD_BDPU: usize = 0x1C;
const SD_CTL_SRST: u32 = 1 << 0;
const SD_CTL_RUN: u32 = 1 << 1;

// Codec verbs / parameters.
const VERB_GET_PARAM: u32 = 0xF00;
const VERB_GET_CONN_LIST: u32 = 0xF02;
const VERB_SET_CONN_SELECT: u32 = 0x701;
const VERB_SET_POWER: u32 = 0x705;
const VERB_SET_STREAM: u32 = 0x706;
const VERB_SET_PIN_CTL: u32 = 0x707;
const VERB_SET_EAPD: u32 = 0x70C;
const VERB_GET_CONFIG_DEFAULT: u32 = 0xF1C;
const VERB_GET_POWER: u32 = 0xF05;
const VERB_EXEC_PIN_SENSE: u32 = 0x709;
const VERB_GET_PIN_SENSE: u32 = 0xF09;
const VERB_SET_DIGI_CONVERT_1: u32 = 0x70D;
const VERB_SET_CVT_CHAN_COUNT: u32 = 0x72D;
const VERB_SET_HDMI_DIP_INDEX: u32 = 0x730;
const VERB_SET_HDMI_DIP_DATA: u32 = 0x731;
const VERB_SET_HDMI_DIP_XMIT: u32 = 0x732;
const VERB_SET_HDMI_CHAN_SLOT: u32 = 0x734;
const VERB_GET_HDMI_DIP_SIZE: u32 = 0xF2E;
const VERB_GET_HDMI_ELDD: u32 = 0xF2F;
const VERB_SET_COEF_INDEX: u32 = 0x500;
const VERB_GET_PROC_COEF: u32 = 0xC00;
const VERB4_SET_FORMAT: u32 = 0x2;
const VERB4_SET_AMP: u32 = 0x3;
const VERB4_SET_PROC_COEF: u32 = 0x4;

const PARAM_VENDOR_ID: u32 = 0x00;
const PARAM_NODE_COUNT: u32 = 0x04;
const PARAM_FG_TYPE: u32 = 0x05;
const PARAM_WIDGET_CAPS: u32 = 0x09;
const PARAM_PIN_CAPS: u32 = 0x0C;
const PARAM_IN_AMP_CAPS: u32 = 0x0D;
const PARAM_CONN_LEN: u32 = 0x0E;
const PARAM_OUT_AMP_CAPS: u32 = 0x12;

const WT_OUTPUT: u32 = 0;
const WT_INPUT: u32 = 1;
const WT_MIXER: u32 = 2;
const WT_SELECTOR: u32 = 3;
const WT_PIN: u32 = 4;

const CORB_ENTRIES: usize = 256;

#[derive(Clone, Copy)]
#[repr(C)]
struct BdlEntry {
    addr: u64,
    len: u32,
    ioc: u32,
}

#[repr(C, align(128))]
struct Bdl([BdlEntry; NUM_BUFFERS]);
#[repr(C, align(4096))]
struct DmaBuffers([[i16; OUT_SAMPLES]; NUM_BUFFERS]);
#[repr(C, align(1024))]
struct Corb([u32; CORB_ENTRIES]);
#[repr(C, align(2048))]
struct Rirb([u64; CORB_ENTRIES]);

static mut BDL: Bdl = Bdl([BdlEntry { addr: 0, len: 0, ioc: 0 }; NUM_BUFFERS]);
static mut DMA: DmaBuffers = DmaBuffers([[0; OUT_SAMPLES]; NUM_BUFFERS]);
static mut CORB: Corb = Corb([0; CORB_ENTRIES]);
static mut RIRB: Rirb = Rirb([0; CORB_ENTRIES]);

static mut BASE: usize = 0;
static mut PRESENT: bool = false;
static mut CODEC: u32 = 0;
/// true = CORB/RIRB rings, false = immediate command interface.
static mut USE_CORB: bool = true;
static mut CORB_WP: usize = 0;
static mut RIRB_RP: usize = 0;
/// Byte offset of our output stream descriptor.
static mut SD: usize = 0;
static mut STARTED: bool = false;
/// Next buffer to render into.
static mut WRITE: usize = 0;
/// Buffer the hardware was playing at the previous pump.
static mut LAST_PLAY: usize = 0;

fn r8(off: usize) -> u8 {
    unsafe { read_volatile((BASE + off) as *const u8) }
}
fn r16(off: usize) -> u16 {
    unsafe { read_volatile((BASE + off) as *const u16) }
}
fn r32(off: usize) -> u32 {
    unsafe { read_volatile((BASE + off) as *const u32) }
}
fn w8(off: usize, v: u8) {
    unsafe { write_volatile((BASE + off) as *mut u8, v) }
}
fn w16(off: usize, v: u16) {
    unsafe { write_volatile((BASE + off) as *mut u16, v) }
}
fn w32(off: usize, v: u32) {
    unsafe { write_volatile((BASE + off) as *mut u32, v) }
}

// Stream-descriptor access with an explicit controller base: the output and
// the capture stream may live on different controllers (NVIDIA HDMI out, PCH
// mic in), and `BASE` follows whichever controller the codec commands go to.
fn br32(base: usize, off: usize) -> u32 {
    unsafe { read_volatile((base + off) as *const u32) }
}
fn bw32(base: usize, off: usize, v: u32) {
    unsafe { write_volatile((base + off) as *mut u32, v) }
}
fn bw16(base: usize, off: usize, v: u16) {
    unsafe { write_volatile((base + off) as *mut u16, v) }
}
fn bw8(base: usize, off: usize, v: u8) {
    unsafe { write_volatile((base + off) as *mut u8, v) }
}

/// Base of the controller the output stream runs on.
static mut OUT_BASE: usize = 0;

/// Bounded spin without a cooperative yield (no re-entry during bring-up).
fn wait_us(timeout_us: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = timer::now_us() + timeout_us;
    loop {
        if cond() {
            return true;
        }
        if timer::now_us() >= deadline {
            return false;
        }
        core::hint::spin_loop();
    }
}

fn log_hex(label: &str, v: u32) {
    serial::write_str(label);
    let mut buf = [0u8; 10];
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..8 {
        let nib = (v >> ((7 - i) * 4)) & 0xF;
        buf[2 + i] = if nib < 10 { b'0' + nib as u8 } else { b'a' + nib as u8 - 10 };
    }
    serial::write(&buf);
}

// -- codec command transport -------------------------------------------------

fn corb_setup() -> bool {
    // Stop both ring DMAs before touching their registers.
    w8(CORBCTL, 0);
    w8(RIRBCTL, 0);
    wait_us(10_000, || r8(CORBCTL) & 0x02 == 0 && r8(RIRBCTL) & 0x02 == 0);
    let corb = &raw const CORB as u64;
    let rirb = &raw const RIRB as u64;
    w32(CORBLBASE, corb as u32);
    w32(CORBUBASE, (corb >> 32) as u32);
    w32(RIRBLBASE, rirb as u32);
    w32(RIRBUBASE, (rirb >> 32) as u32);
    // 256 entries if offered (size bits 1:0 = 2); emulators often fix it.
    if r8(CORBSIZE) & 0x40 != 0 {
        w8(CORBSIZE, (r8(CORBSIZE) & 0xFC) | 0x02);
    }
    if r8(RIRBSIZE) & 0x40 != 0 {
        w8(RIRBSIZE, (r8(RIRBSIZE) & 0xFC) | 0x02);
    }
    // CORB read pointer reset handshake (set RST, see it, clear it, see
    // it clear) -- bounded, since emulators don't all echo the bit.
    w16(CORBRP, 0x8000);
    wait_us(1_000, || r16(CORBRP) & 0x8000 != 0);
    w16(CORBRP, 0);
    wait_us(1_000, || r16(CORBRP) & 0x8000 == 0);
    w16(CORBWP, 0);
    w16(RIRBWP, 0x8000); // RIRB write pointer reset
    w16(RINTCNT, 0xFF);
    w8(RIRBSTS, 0x05);
    unsafe {
        CORB_WP = 0;
        RIRB_RP = 0;
    }
    w8(CORBCTL, 0x02); // DMA run
    w8(RIRBCTL, 0x02); // DMA run, no interrupts
    wait_us(10_000, || r8(CORBCTL) & 0x02 != 0 && r8(RIRBCTL) & 0x02 != 0)
}

fn corb_command(verb: u32) -> Option<u32> {
    unsafe {
        let wp = (CORB_WP + 1) % CORB_ENTRIES;
        write_volatile(&raw mut CORB.0[wp], verb);
        fence(Ordering::SeqCst);
        CORB_WP = wp;
        w16(CORBWP, wp as u16);
        loop {
            let ok = wait_us(20_000, || (r16(RIRBWP) & 0xFF) as usize != RIRB_RP);
            if !ok {
                return None;
            }
            fence(Ordering::SeqCst);
            RIRB_RP = (RIRB_RP + 1) % CORB_ENTRIES;
            let entry = read_volatile(&raw const RIRB.0[RIRB_RP]);
            w8(RIRBSTS, 0x05);
            // bit 36 (resp_ex bit 4) = unsolicited response: skip it.
            if entry & (1 << 36) == 0 {
                return Some(entry as u32);
            }
        }
    }
}

fn immediate_command(verb: u32) -> Option<u32> {
    if !wait_us(20_000, || r16(ICIS) & 0x01 == 0) {
        return None;
    }
    w16(ICIS, 0x02); // clear IRV
    w32(ICOI, verb);
    w16(ICIS, 0x01); // ICB: send
    if !wait_us(20_000, || r16(ICIS) & 0x03 == 0x02) {
        return None;
    }
    let resp = r32(IRII);
    w16(ICIS, 0x02);
    Some(resp)
}

fn command(verb: u32) -> Option<u32> {
    if unsafe { USE_CORB } {
        corb_command(verb)
    } else {
        immediate_command(verb)
    }
}

/// 12-bit verb with an 8-bit payload.
fn verb12(nid: u32, verb: u32, payload: u32) -> u32 {
    let cad = unsafe { CODEC };
    (cad << 28) | ((nid & 0xFF) << 20) | ((verb & 0xFFF) << 8) | (payload & 0xFF)
}

/// 4-bit verb with a 16-bit payload.
fn verb4(nid: u32, verb: u32, payload: u32) -> u32 {
    let cad = unsafe { CODEC };
    (cad << 28) | ((nid & 0xFF) << 20) | ((verb & 0xF) << 16) | (payload & 0xFFFF)
}

fn param(nid: u32, p: u32) -> u32 {
    command(verb12(nid, VERB_GET_PARAM, p)).unwrap_or(0)
}

fn send(v: u32) {
    let _ = command(v);
}

// -- widget graph --------------------------------------------------------------

static mut CAPS: [u32; 256] = [0; 256];
static mut AFG: u32 = 0;

fn wtype(nid: u32) -> u32 {
    unsafe { (CAPS[nid as usize & 0xFF] >> 20) & 0xF }
}

/// Widget caps bit 9: a digital (S/PDIF / HDMI) converter or pin.
fn is_digital(nid: u32) -> bool {
    (unsafe { CAPS[nid as usize & 0xFF] } & (1 << 9)) != 0
}

/// Read up to `out.len()` connection-list entries of `nid`; returns count.
fn conn_list(nid: u32, out: &mut [u32; 32]) -> usize {
    if unsafe { CAPS[nid as usize & 0xFF] } & (1 << 8) == 0 {
        return 0;
    }
    let lenp = param(nid, PARAM_CONN_LEN);
    let long = lenp & 0x80 != 0;
    let len = (lenp & 0x7F) as usize;
    let per = if long { 2 } else { 4 };
    let bits = if long { 16 } else { 8 };
    let mask = (1u32 << bits) - 1;
    let range_bit = 1u32 << (bits - 1);
    let mut n = 0;
    let mut prev = 0u32;
    let mut i = 0;
    while i < len {
        let resp = command(verb12(nid, VERB_GET_CONN_LIST, i as u32)).unwrap_or(0);
        for j in 0..per {
            if i + j >= len {
                break;
            }
            let e = (resp >> (j * bits)) & mask;
            let id = e & (range_bit - 1);
            if e & range_bit != 0 && prev != 0 && id > prev {
                // range: prev+1 ..= id
                let mut k = prev + 1;
                while k <= id && n < out.len() {
                    out[n] = k;
                    n += 1;
                    k += 1;
                }
            } else if n < out.len() {
                out[n] = id;
                n += 1;
            }
            prev = id;
        }
        i += per;
    }
    n
}

/// Depth-first search from `nid` down to an output converter of the wanted
/// kind (analog DAC for analog pins, digital converter for HDMI / S/PDIF).
/// Records (widget, chosen connection index) into `path`.
fn find_dac(nid: u32, digital: bool, path: &mut [(u32, usize); 8], depth: usize) -> Option<usize> {
    if depth + 1 >= path.len() {
        return None;
    }
    let mut conns = [0u32; 32];
    let n = conn_list(nid, &mut conns);
    // Prefer a directly connected DAC, then go through mixers/selectors.
    for pass in 0..2 {
        for (idx, &c) in conns[..n].iter().enumerate() {
            if c == 0 || c > 255 {
                continue;
            }
            let t = wtype(c);
            if pass == 0 && t == WT_OUTPUT && is_digital(c) == digital {
                path[depth] = (nid, idx);
                path[depth + 1] = (c, 0);
                return Some(depth + 2);
            }
            if pass == 1 && (t == WT_MIXER || t == WT_SELECTOR) {
                path[depth] = (nid, idx);
                if let Some(len) = find_dac(c, digital, path, depth + 1) {
                    return Some(len);
                }
            }
        }
    }
    None
}

fn amp_gain(nid: u32, p: u32) -> u32 {
    // Per-widget amp caps when "amp param override" is set, else the AFG's.
    let caps = if unsafe { CAPS[nid as usize] } & (1 << 3) != 0 {
        param(nid, p)
    } else {
        param(unsafe { AFG }, p)
    };
    caps & 0x7F // offset = the step that is 0 dB
}

fn unmute_out(nid: u32) {
    if unsafe { CAPS[nid as usize] } & (1 << 2) != 0 {
        let g = amp_gain(nid, PARAM_OUT_AMP_CAPS);
        // output amp, left + right, unmuted, 0 dB
        send(verb4(nid, VERB4_SET_AMP, 0xB000 | g));
    }
}

fn unmute_in(nid: u32, index: usize) {
    if unsafe { CAPS[nid as usize] } & (1 << 1) != 0 {
        let g = amp_gain(nid, PARAM_IN_AMP_CAPS);
        send(verb4(nid, VERB4_SET_AMP, 0x7000 | ((index as u32 & 0xF) << 8) | g));
    }
}

/// Mute one input of a mixer: a Realtek front mixer (0x0c..) also sums the
/// analog loopback mixer 0x0b (mic / line-in pass-through), which only
/// adds hiss and feedback when left open.
fn mute_in(nid: u32, index: usize) {
    if unsafe { CAPS[nid as usize] } & (1 << 1) != 0 {
        send(verb4(nid, VERB4_SET_AMP, 0x7000 | ((index as u32 & 0xF) << 8) | 0x80));
    }
}

fn power_on(nid: u32) {
    send(verb12(nid, VERB_SET_POWER, 0));
}

/// Wait (bounded) until a node reports actual power state D0 -- real codecs
/// take a few ms to leave D3 after reset; widgets programmed before that
/// can drop the verbs.
fn wait_d0(nid: u32) {
    wait_us(100_000, || match command(verb12(nid, VERB_GET_POWER, 0)) {
        Some(r) => (r >> 4) & 0xF == 0,
        None => true,
    });
}

fn setup_dac(nid: u32, digital: bool) {
    power_on(nid);
    send(verb4(nid, VERB4_SET_FORMAT, STREAM_FORMAT as u32));
    send(verb12(nid, VERB_SET_STREAM, (STREAM_TAG << 4) | 0));
    if digital {
        // DIGEN, 2 channels, ASP slot 0 -> ch 0, slot 1 -> ch 1 (HDMI).
        send(verb12(nid, VERB_SET_DIGI_CONVERT_1, 0x01));
        send(verb12(nid, VERB_SET_CVT_CHAN_COUNT, 1));
        send(verb12(nid, VERB_SET_HDMI_CHAN_SLOT, 0x00));
        send(verb12(nid, VERB_SET_HDMI_CHAN_SLOT, 0x11));
    }
    unmute_out(nid);
}

/// Open codec `CODEC`: find its audio function group, power it to D0 and
/// cache every widget's caps. Returns (afg, first nid, end nid, vendor id).
fn open_codec() -> Option<(u32, u32, u32, u32)> {
    let vendor = param(0, PARAM_VENDOR_ID);
    if vendor == 0 || vendor == 0xFFFF_FFFF {
        return None;
    }
    let root = param(0, PARAM_NODE_COUNT);
    let (fg_start, fg_count) = ((root >> 16) & 0xFF, root & 0xFF);
    let mut afg = 0;
    for nid in fg_start..fg_start + fg_count {
        if param(nid, PARAM_FG_TYPE) & 0xFF == 0x01 {
            afg = nid;
            break;
        }
    }
    if afg == 0 {
        return None;
    }
    unsafe { AFG = afg };
    power_on(afg);
    wait_d0(afg);

    let sub = param(afg, PARAM_NODE_COUNT);
    let (start, count) = ((sub >> 16) & 0xFF, sub & 0xFF);
    let end = (start + count).min(256);
    unsafe {
        let caps = &mut *&raw mut CAPS;
        caps.fill(0);
        for nid in start..end {
            caps[nid as usize] = param(nid, PARAM_WIDGET_CAPS);
        }
    }
    Some((afg, start, end, vendor))
}

// -- pin default config / presence ---------------------------------------------

fn pin_conn(cfg: u32) -> u32 {
    cfg >> 30 // 0 jack, 1 none, 2 fixed (built in), 3 both
}
fn pin_dev(cfg: u32) -> u32 {
    (cfg >> 20) & 0xF // 0 line out, 1 speaker, 2 HP out, 4 S/PDIF, 5 digital other, 8 line in, 0xA mic
}
fn pin_loc(cfg: u32) -> u32 {
    (cfg >> 24) & 0x3F // bits 5:4 chassis, 3:0 geometry (1 rear, 2 front)
}
fn pin_hdmi(pcaps: u32) -> bool {
    pcaps & ((1 << 7) | (1 << 24)) != 0 // HDMI / DisplayPort pin
}
fn pin_digital(pcaps: u32, cfg: u32) -> bool {
    pin_hdmi(pcaps) || pin_dev(cfg) == 4 || pin_dev(cfg) == 5
}
/// An analog output jack the user can plug into: line out / speaker / HP,
/// physically connected.
fn analog_out(pcaps: u32, cfg: u32) -> bool {
    !pin_digital(pcaps, cfg) && pin_conn(cfg) != 1 && pin_dev(cfg) <= 2
}

const SENSE_NO: u8 = 0;
const SENSE_YES: u8 = 1;
const SENSE_UNKNOWN: u8 = 2;

/// Presence detect, when the pin can report it (pin caps bit 2) and the
/// board did not set "jack detect override" (default config misc bit 0).
fn pin_sense(nid: u32, pcaps: u32, cfg: u32) -> u8 {
    if pcaps & (1 << 2) == 0 || cfg & (1 << 8) != 0 {
        return SENSE_UNKNOWN;
    }
    if pcaps & (1 << 1) != 0 {
        // "trigger required": execute a sense cycle first.
        send(verb12(nid, VERB_EXEC_PIN_SENSE, 0));
        timer::delay_us(2_000);
    }
    match command(verb12(nid, VERB_GET_PIN_SENSE, 0)) {
        Some(r) if r & (1 << 31) != 0 => SENSE_YES,
        Some(_) => SENSE_NO,
        None => SENSE_UNKNOWN,
    }
}

/// Auto-selection rank. A plugged analog jack wins (headphones first), an
/// analog jack that cannot report presence still beats HDMI (it may be in
/// use -- e.g. an AC'97-mode front panel), HDMI with a sink attached is the
/// fallback, then analog jacks that say "unplugged" in the usual order
/// (rear line out, other line out, speaker, headphone).
fn pin_score(pcaps: u32, cfg: u32, sense: u8) -> u8 {
    let conn = pin_conn(cfg);
    let dev = pin_dev(cfg);
    if conn == 1 {
        return 1;
    }
    if pin_hdmi(pcaps) {
        return match sense {
            SENSE_YES => 60,
            SENSE_UNKNOWN => 20,
            _ => 15,
        };
    }
    if pin_digital(pcaps, cfg) {
        return 10; // S/PDIF
    }
    if dev > 2 {
        return 2; // a retaskable input jack (mic / line in): manual only
    }
    if conn == 2 {
        return if dev == 1 { 65 } else { 64 }; // built-in speaker
    }
    let rear = pin_loc(cfg) & 0xF == 1;
    match sense {
        SENSE_YES => match dev {
            2 => 100,
            0 if rear => 99,
            0 => 98,
            _ => 97,
        },
        s => {
            let rank = match dev {
                0 if rear => 5,
                0 => 4,
                1 => 3,
                _ => 2,
            };
            if s == SENSE_UNKNOWN { 70 + rank } else { 30 + rank }
        }
    }
}

// -- Realtek -------------------------------------------------------------------

/// Realtek processing coefficients live behind vendor widget 0x20.
fn rtk_coef_update(idx: u32, mask: u32, set: u32) {
    if wtype(0x20) != 0xF {
        return; // no vendor-defined widget there: not the layout we know
    }
    send(verb12(0x20, VERB_SET_COEF_INDEX, idx));
    let v = command(verb12(0x20, VERB_GET_PROC_COEF, 0)).unwrap_or(0) & 0xFFFF;
    let nv = (v & !mask) | set;
    if nv != v {
        send(verb12(0x20, VERB_SET_COEF_INDEX, idx));
        send(verb4(0x20, VERB4_SET_PROC_COEF, nv));
    }
}

fn realtek_init(vendor: u32) {
    match vendor {
        // ALC898 / ALC1150 / ALCS1200A / ALC1220: Linux
        // sound/pci/hda/patch_realtek.c alc_fill_eapd_coef() clears COEF
        // 0x07 bit 1 at every init (alc_pre_init / resume) for exactly these
        // ids; it is the codec-internal EAPD/amp control, and with it left
        // at the power-on value the line-out / headphone amps can stay off
        // even though the pin's EAPD verb is set.
        0x10ec_0899 | 0x10ec_0900 | 0x10ec_0b00 | 0x10ec_1168 | 0x10ec_1220 => {
            rtk_coef_update(0x07, 1 << 1, 0);
        }
        _ => {}
    }
}

// -- HDMI ------------------------------------------------------------------------

/// Minimal CEA-861 audio InfoFrame (2-channel PCM, "refer to stream
/// header"): HDMI sinks may stay silent without one.
fn hdmi_infoframe(pin: u32) {
    let mut f = [0x84u8, 0x01, 0x0A, 0, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let sum = f.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    f[3] = 0u8.wrapping_sub(sum);
    send(verb12(pin, VERB_SET_HDMI_DIP_XMIT, 0x00));
    send(verb12(pin, VERB_SET_HDMI_DIP_INDEX, 0)); // packet 0, byte 0
    for b in f {
        send(verb12(pin, VERB_SET_HDMI_DIP_DATA, b as u32));
    }
    send(verb12(pin, VERB_SET_HDMI_DIP_XMIT, 0xC0)); // best effort
}

// -- controllers / outputs ---------------------------------------------------------

#[derive(Clone, Copy)]
struct Ctrl {
    base: usize,
    bus: u8,
    dev: u8,
    func: u8,
    vendor: u16,
}

const MAX_CTRL: usize = 4;
static mut CTRLS: [Ctrl; MAX_CTRL] = [Ctrl { base: 0, bus: 0, dev: 0, func: 0, vendor: 0 }; MAX_CTRL];
static mut NCTRL: usize = 0;
/// Controller whose registers BASE points at (usize::MAX = none).
static mut OPEN_CTRL: usize = usize::MAX;

/// One output-capable pin with a route to a converter, on any codec of any
/// controller.
#[derive(Clone, Copy)]
struct OutPin {
    ctrl: u8,
    codec: u8,
    nid: u8,
    dac: u8,
    digital: bool,
    sense: u8,
    score: u8,
    cfg: u32,
    pcaps: u32,
    vendor: u32,
    /// HDMI / DP: the sink's monitor name from its ELD (`eld_len` 0 = none).
    eld: [u8; 16],
    eld_len: u8,
}

const MAX_PINS: usize = 32;
static mut PINS: [OutPin; MAX_PINS] = [OutPin {
    ctrl: 0,
    codec: 0,
    nid: 0,
    dac: 0,
    digital: false,
    sense: 0,
    score: 0,
    cfg: 0,
    pcaps: 0,
    vendor: 0,
    eld: [0; 16],
    eld_len: 0,
}; MAX_PINS];
static mut NPINS: usize = 0;
/// Index into PINS of the output being played (usize::MAX = none).
static mut ACTIVE: usize = usize::MAX;
/// false = auto (best pin + every other analog jack of that codec plays);
/// true = only the pin chosen with `audio use <n>`.
static mut MANUAL: bool = false;
/// Other analog jacks enabled alongside the primary one (auto mode).
static mut EXTRA_JACKS: u32 = 0;

fn cfg_addr(c: &Ctrl, off: u8) -> u32 {
    (1 << 31) | ((c.bus as u32) << 16) | ((c.dev as u32) << 11) | ((c.func as u32) << 8) | (off as u32 & 0xFC)
}
fn cfg_r8(c: &Ctrl, off: u8) -> u8 {
    (pci::read32(c.bus, c.dev, c.func, off & 0xFC) >> ((off & 3) * 8)) as u8
}
fn cfg_w8(c: &Ctrl, off: u8, v: u8) {
    use crate::arch::io::{outb, outl};
    unsafe {
        outl(0xCF8, cfg_addr(c, off));
        outb(0xCFC + (off & 3) as u16, v);
    }
}
fn cfg_r16(c: &Ctrl, off: u8) -> u16 {
    (pci::read32(c.bus, c.dev, c.func, off & 0xFC) >> ((off & 2) * 8)) as u16
}
fn cfg_w16(c: &Ctrl, off: u8, v: u16) {
    use crate::arch::io::{outl, outw};
    unsafe {
        outl(0xCF8, cfg_addr(c, off));
        outw(0xCFC + (off & 2) as u16, v);
    }
}

/// PCI-side preparation Linux's hda_intel.c does in azx_init_pci() /
/// azx_probe(): power state D0, memory + bus master, TCSEL cleared (Intel:
/// "clears playback static on some HD Audio codecs") and DMA snooping on
/// -- our BDL and sample buffers are ordinary write-back memory, so a
/// controller left in no-snoop mode by the firmware would play stale RAM
/// (noise / silence) instead of what the CPU wrote.
fn pci_prepare(c: &Ctrl) {
    // PCI power management capability -> D0.
    if pci::read32(c.bus, c.dev, c.func, 0x04) & (1 << 20) != 0 {
        let mut p = cfg_r8(c, 0x34) & 0xFC;
        let mut guard = 0;
        while p != 0 && guard < 48 {
            if cfg_r8(c, p) == 0x01 {
                let pmcsr = cfg_r16(c, p + 4);
                if pmcsr & 3 != 0 {
                    cfg_w16(c, p + 4, pmcsr & !3);
                    timer::delay_us(10_000); // D3hot -> D0 recovery
                }
                break;
            }
            p = cfg_r8(c, p + 1) & 0xFC;
            guard += 1;
        }
    }
    let cmd = cfg_r16(c, 0x04);
    cfg_w16(c, 0x04, cmd | 0x0006); // memory space + bus master
    match c.vendor {
        0x8086 => {
            cfg_w8(c, 0x44, cfg_r8(c, 0x44) & !0x07); // TCSEL = TC0
            // INTEL_SCH_HDA_DEVC (0x78) bit 11 = NOSNOOP: clear it.
            let devc = cfg_r16(c, 0x78);
            if devc & (1 << 11) != 0 {
                cfg_w16(c, 0x78, devc & !(1 << 11));
            }
        }
        0x10DE => {
            // NVIDIA_HDA_TRANSREG_ADDR / OSTRM_COH / ISTRM_COH coherency bits.
            cfg_w8(c, 0x4E, (cfg_r8(c, 0x4E) & !0x0F) | 0x0F);
            cfg_w8(c, 0x4C, cfg_r8(c, 0x4C) | 0x01);
            cfg_w8(c, 0x4D, cfg_r8(c, 0x4D) | 0x01);
        }
        _ => {}
    }
}

/// Find every HD Audio function (class 04/03, any prog-if: Intel PCHs with
/// the DSP enabled in firmware report 0x040380 but still run the legacy
/// HDA interface) and map its registers.
fn discover() {
    unsafe {
        if NCTRL != 0 {
            return;
        }
    }
    for loc in pci::find_all_class(0x04, 0x03) {
        if unsafe { NCTRL } >= MAX_CTRL {
            break;
        }
        let bar0 = loc.read32(0x10);
        if bar0 & 1 != 0 {
            continue; // must be a memory BAR
        }
        let mut base = (bar0 & 0xFFFF_FFF0) as u64;
        if (bar0 >> 1) & 3 == 2 {
            base |= (loc.read32(0x14) as u64) << 32;
        }
        // UEFI boards often put this BAR above 4 GiB (the Z590's is at
        // 0x40_1211_0000): map it there, uncached.
        if base == 0 || crate::arch::paging::map_mmio(base, 0x4000).is_none() {
            log_hex("hda: BAR0 cannot be mapped: hi=", (base >> 32) as u32);
            serial::write(b"\n");
            continue;
        }
        let c = Ctrl {
            base: base as usize,
            bus: loc.bus,
            dev: loc.device,
            func: loc.function,
            vendor: (loc.read32(0x00) & 0xFFFF) as u16,
        };
        pci_prepare(&c);
        unsafe {
            CTRLS[NCTRL] = c;
            NCTRL += 1;
        }
    }
}

fn bdf(i: usize) -> alloc::string::String {
    let c = unsafe { CTRLS[i] };
    alloc::format!("{:02x}:{:02x}.{}", c.bus, c.dev, c.func)
}

/// Reset controller `i`, enumerate its codecs (STATESTS SDIN bits) and bring
/// up a command transport. Returns the codec bitmask (0 = unusable).
fn ctrl_open(i: usize) -> u16 {
    ctrl_close();
    let c = unsafe { CTRLS[i] };
    unsafe {
        BASE = c.base;
        OPEN_CTRL = i;
        CTRL_READY[i] = false;
        if IN_RUNNING && IN_CTRL == i {
            // the reset below stops the capture stream; `capture_resume`
            // brings it back once the caller is done
            IN_RUNNING = false;
        }
    }
    // Controller reset: CRST 0, wait, CRST 1, wait, then >=521us for the
    // codecs to request enumeration (STATESTS).
    w8(CORBCTL, 0);
    w8(RIRBCTL, 0);
    w32(INTCTL, 0);
    w32(DPLBASE, 0);
    w32(DPUBASE, 0);
    w16(STATESTS, 0x7FFF);
    w32(GCTL, r32(GCTL) & !1);
    wait_us(100_000, || r32(GCTL) & 1 == 0);
    timer::delay_us(200);
    w32(GCTL, r32(GCTL) | 1);
    if !wait_us(100_000, || r32(GCTL) & 1 != 0) {
        crate::console_write(alloc::format!("hda: {} stuck in reset\n", bdf(i)).as_bytes());
        unsafe { OPEN_CTRL = usize::MAX };
        return 0;
    }
    wait_us(100_000, || r16(STATESTS) & 0x7FFF != 0);
    timer::delay_us(2_000); // let slower codecs (HDMI) raise their bit too
    let codecs = r16(STATESTS) & 0x7FFF;
    w16(STATESTS, codecs);
    if codecs == 0 {
        crate::console_write(alloc::format!("hda: {} has no codec\n", bdf(i)).as_bytes());
        unsafe { OPEN_CTRL = usize::MAX };
        return 0;
    }
    unsafe { CODEC = codecs.trailing_zeros() };

    // Command transport: CORB/RIRB, else the immediate interface.
    let corb_ok = corb_setup();
    unsafe { USE_CORB = corb_ok };
    if !corb_ok || corb_command(verb12(0, VERB_GET_PARAM, PARAM_VENDOR_ID)).is_none() {
        w8(CORBCTL, 0);
        w8(RIRBCTL, 0);
        unsafe { USE_CORB = false };
        if immediate_command(verb12(0, VERB_GET_PARAM, PARAM_VENDOR_ID)).is_none() {
            crate::console_write(alloc::format!("hda: {} codec does not answer\n", bdf(i)).as_bytes());
            unsafe { OPEN_CTRL = usize::MAX };
            return 0;
        }
        serial::write_str("hda: using immediate command interface\n");
    }
    unsafe {
        CTRL_READY[i] = true;
        CTRL_CORB[i] = USE_CORB;
    }
    codecs
}

/// Out of reset since `ctrl_open` (codecs enumerated, streams programmable).
static mut CTRL_READY: [bool; MAX_CTRL] = [false; MAX_CTRL];
/// Which command transport worked on that controller.
static mut CTRL_CORB: [bool; MAX_CTRL] = [true; MAX_CTRL];

/// Point the codec command transport at controller `i` WITHOUT resetting it
/// (a reset would stop a stream running there -- the output, or the capture
/// stream). Falls back to `ctrl_open` for a controller never brought up.
fn ctrl_use(i: usize) -> bool {
    if i >= unsafe { NCTRL } {
        return false;
    }
    if unsafe { OPEN_CTRL } == i {
        return true;
    }
    if !unsafe { CTRL_READY[i] } {
        return ctrl_open(i) != 0;
    }
    ctrl_close();
    unsafe {
        BASE = CTRLS[i].base;
        OPEN_CTRL = i;
        USE_CORB = CTRL_CORB[i];
    }
    if unsafe { USE_CORB } && corb_setup() {
        return true;
    }
    unsafe {
        USE_CORB = false;
        CTRL_CORB[i] = false;
    }
    w8(CORBCTL, 0);
    w8(RIRBCTL, 0);
    true
}

/// Stop the CORB/RIRB DMA of the open controller (the rings are shared
/// statics, so only one controller may run them).
fn ctrl_close() {
    if unsafe { OPEN_CTRL } == usize::MAX {
        return;
    }
    w8(CORBCTL, 0);
    w8(RIRBCTL, 0);
    unsafe { OPEN_CTRL = usize::MAX };
}

fn vendor_name(v: u32) -> &'static str {
    match v >> 16 {
        0x10EC => "Realtek",
        0x8086 => "Intel",
        0x10DE => "NVIDIA",
        0x1002 => "AMD",
        0x1AF4 => "QEMU",
        0x8384 => "SigmaTel",
        0x11D4 => "Analog Devices",
        0x14F1 => "Conexant",
        0x111D => "IDT",
        _ => "codec",
    }
}

fn kind_name(pcaps: u32, cfg: u32) -> &'static str {
    if pin_hdmi(pcaps) {
        return "HDMI/DP";
    }
    match pin_dev(cfg) {
        0 => "line out",
        1 => "speaker",
        2 => "headphone",
        4 => "S/PDIF",
        5 => "digital",
        8 => "line in",
        0xA => "mic",
        _ => "pin",
    }
}

fn where_name(cfg: u32) -> &'static str {
    if pin_conn(cfg) == 1 {
        return "not connected";
    }
    if pin_conn(cfg) == 2 {
        return "built in";
    }
    match pin_loc(cfg) & 0xF {
        1 => "rear",
        2 => "front",
        3 => "left",
        4 => "right",
        5 => "top",
        6 => "bottom",
        _ => "jack",
    }
}

fn color_name(cfg: u32) -> &'static str {
    match (cfg >> 12) & 0xF {
        1 => " black",
        2 => " grey",
        3 => " blue",
        4 => " green",
        5 => " red",
        6 => " orange",
        7 => " yellow",
        8 => " purple",
        9 => " pink",
        0xE => " white",
        _ => "",
    }
}

fn sense_name(s: u8) -> &'static str {
    match s {
        SENSE_YES => "plugged",
        SENSE_NO => "unplugged",
        _ => "no detect",
    }
}

fn describe(i: usize) -> alloc::string::String {
    let p = unsafe { PINS[i] };
    alloc::format!(
        "{} codec {} {} {:08x} pin 0x{:02x} {} {}{}, {} -> dac 0x{:02x}",
        bdf(p.ctrl as usize),
        p.codec,
        vendor_name(p.vendor),
        p.vendor,
        p.nid,
        kind_name(p.pcaps, p.cfg),
        where_name(p.cfg),
        color_name(p.cfg),
        sense_name(p.sense),
        p.dac
    )
}

/// Walk every output pin of codec `CODEC` on controller `ctrl` into PINS.
fn scan_codec(ctrl: usize) {
    let codec = unsafe { CODEC };
    let Some((_afg, start, end, vendor)) = open_codec() else {
        crate::console_write(alloc::format!("hda: {} codec {codec}: no audio function group\n", bdf(ctrl)).as_bytes());
        return;
    };
    for nid in start..end {
        if wtype(nid) != WT_PIN {
            continue;
        }
        let pcaps = param(nid, PARAM_PIN_CAPS);
        if pcaps & (1 << 4) == 0 {
            continue; // not output capable
        }
        let cfg = command(verb12(nid, VERB_GET_CONFIG_DEFAULT, 0)).unwrap_or(0);
        let digital = pin_digital(pcaps, cfg);
        let mut path = [(0u32, 0usize); 8];
        let Some(len) = find_dac(nid, digital, &mut path, 0) else {
            continue;
        };
        if unsafe { CAPS[nid as usize] } & (1 << 10) != 0 {
            power_on(nid); // pin with its own power control: sense needs D0
        }
        let sense = if pin_conn(cfg) == 1 { SENSE_UNKNOWN } else { pin_sense(nid, pcaps, cfg) };
        unsafe {
            if NPINS >= MAX_PINS {
                return;
            }
            PINS[NPINS] = OutPin {
                ctrl: ctrl as u8,
                codec: codec as u8,
                nid: nid as u8,
                dac: path[len - 1].0 as u8,
                digital,
                sense,
                score: pin_score(pcaps, cfg, sense),
                cfg,
                pcaps,
                vendor,
                eld: [0; 16],
                eld_len: 0,
            };
            if pin_hdmi(pcaps) && sense == SENSE_YES {
                let (name, n) = read_eld_name(nid);
                PINS[NPINS].eld = name;
                PINS[NPINS].eld_len = n as u8;
            }
            NPINS += 1;
        }
    }
    scan_inputs(ctrl, start, end);
}

/// The monitor name a display put in its ELD (EDID-Like Data, which the GPU
/// copies from the monitor's EDID into the HDMI codec): baseline block byte
/// 4 bits 4:0 = name length, the name at byte 20 (Linux hda_eld.c).
fn read_eld_name(nid: u32) -> ([u8; 16], usize) {
    let mut out = [0u8; 16];
    // pin sense bit 30 = ELD valid
    let valid = command(verb12(nid, VERB_GET_PIN_SENSE, 0)).map(|r| r & (1 << 30) != 0).unwrap_or(false);
    if !valid {
        return (out, 0);
    }
    let size = command(verb12(nid, VERB_GET_HDMI_DIP_SIZE, 0x08)).unwrap_or(0) & 0xFF;
    if size != 0 && size < 21 {
        return (out, 0);
    }
    let byte = |i: u32| -> Option<u8> {
        let r = command(verb12(nid, VERB_GET_HDMI_ELDD, i))?;
        if r & (1 << 31) == 0 { None } else { Some(r as u8) }
    };
    let Some(b4) = byte(4) else { return (out, 0) };
    let mnl = ((b4 & 0x1F) as usize).min(out.len());
    let mut n = 0;
    for k in 0..mnl {
        match byte(20 + k as u32) {
            Some(c) if (0x20..0x7F).contains(&c) => {
                out[n] = c;
                n += 1;
            },
            Some(0) | None => break,
            Some(_) => {},
        }
    }
    while n > 0 && out[n - 1] == b' ' {
        n -= 1;
    }
    (out, n)
}

/// Probe every controller / codec / output pin. Leaves no controller open.
fn scan_all() {
    unsafe {
        NPINS = 0;
        NIN = 0;
    }
    for i in 0..unsafe { NCTRL } {
        let codecs = ctrl_open(i);
        if codecs == 0 {
            continue;
        }
        for c in 0..15u32 {
            if codecs & (1 << c) != 0 {
                unsafe { CODEC = c };
                scan_codec(i);
            }
        }
        ctrl_close();
    }
    for i in 0..unsafe { NPINS } {
        let p = unsafe { PINS[i] };
        if p.score > 1 {
            crate::console_write(alloc::format!("hda: [{i}] {} (score {})\n", describe(i), p.score).as_bytes());
        }
    }
}

/// Route one pin to a converter: connection selects, amps (the path's mixer
/// input unmuted at 0 dB, its other inputs muted), power, pin control,
/// EAPD, HDMI InfoFrame. Returns the widget path (pin first, DAC last).
fn route_pin(nid: u32, pcaps: u32, cfg: u32) -> Option<([u8; 8], usize)> {
    let digital = pin_digital(pcaps, cfg);
    let mut path = [(0u32, 0usize); 8];
    let len = find_dac(nid, digital, &mut path, 0)?;
    let mut nids = [0u8; 8];
    for (k, &(w, idx)) in path[..len].iter().enumerate() {
        nids[k] = w as u8;
        power_on(w);
        let t = wtype(w);
        if t == WT_PIN || t == WT_SELECTOR {
            send(verb12(w, VERB_SET_CONN_SELECT, idx as u32));
        }
        if t == WT_MIXER {
            let mut conns = [0u32; 32];
            let n = conn_list(w, &mut conns);
            for i in 0..n {
                if i == idx {
                    unmute_in(w, i);
                } else {
                    mute_in(w, i);
                }
            }
        } else if t == WT_SELECTOR {
            unmute_in(w, idx);
        }
        unmute_out(w);
    }
    // Pin widget control: OUT_EN, plus HP_EN on headphone-capable pins.
    let hp = !digital && pin_dev(cfg) == 2 && pcaps & (1 << 3) != 0;
    send(verb12(nid, VERB_SET_PIN_CTL, if hp { 0xC0 } else { 0x40 }));
    if pcaps & (1 << 16) != 0 {
        send(verb12(nid, VERB_SET_EAPD, 0x02));
    }
    if pin_hdmi(pcaps) {
        hdmi_infoframe(nid);
    }
    Some((nids, len))
}

/// Program the codec of PINS[idx] (its controller already open). Auto mode
/// also routes every other analog output jack of that codec to the stream
/// -- Linux's default without line-out auto-mute -- so plugging into
/// another jack later just works, no polling needed; manual mode plays
/// only the chosen pin and turns the other analog outputs off.
fn configure(idx: usize, manual: bool) -> Option<([u8; 8], usize)> {
    let p = unsafe { PINS[idx] };
    unsafe { CODEC = p.codec as u32 };
    let (_afg, start, end, vendor) = open_codec()?;
    for nid in start..end {
        if unsafe { CAPS[nid as usize] } & (1 << 10) != 0 {
            power_on(nid);
        }
    }
    if vendor >> 16 == 0x10EC {
        realtek_init(vendor);
    }
    // Every converter of the pin's kind listens to our stream (harmless
    // duplicates on real hardware; covers emulators that tie "front" to a
    // fixed DAC, e.g. VirtualBox ignores routing).
    for nid in start..end {
        if wtype(nid) == WT_OUTPUT && is_digital(nid) == p.digital {
            setup_dac(nid, p.digital);
        }
    }
    let mut main = None;
    let mut extra = 0;
    for nid in start..end {
        if wtype(nid) != WT_PIN {
            continue;
        }
        let pcaps = param(nid, PARAM_PIN_CAPS);
        if pcaps & (1 << 4) == 0 {
            continue;
        }
        let cfg = command(verb12(nid, VERB_GET_CONFIG_DEFAULT, 0)).unwrap_or(0);
        let analog = analog_out(pcaps, cfg);
        if nid == p.nid as u32 {
            main = route_pin(nid, pcaps, cfg);
        } else if !manual && !p.digital && analog {
            if route_pin(nid, pcaps, cfg).is_some() {
                extra += 1;
            }
        } else if analog {
            send(verb12(nid, VERB_SET_PIN_CTL, 0));
        }
    }
    unsafe { EXTRA_JACKS = extra };
    main
}

/// Stop playback and release the open controller (before a re-route).
fn stop_active() {
    unsafe {
        write_volatile(&raw mut PRESENT, false);
        write_volatile(&raw mut STARTED, false);
        if OUT_BASE != 0 && SD != 0 {
            bw32(OUT_BASE, SD + SD_CTL, 0);
        }
    }
    ctrl_close();
}

/// Make PINS[idx] the output: reset its controller, program the codec path
/// and the output stream.
fn activate(idx: usize, manual: bool) -> bool {
    let p = unsafe { PINS[idx] };
    let ctrl = p.ctrl as usize;
    if ctrl_open(ctrl) == 0 {
        return false;
    }
    let Some((nids, len)) = configure(idx, manual) else {
        ctrl_close();
        return false;
    };

    // Output stream descriptor: the first one after the input streams.
    let gcap = r16(GCAP) as usize;
    let iss = (gcap >> 8) & 0xF;
    let oss = (gcap >> 12) & 0xF;
    if oss == 0 {
        crate::console_write(alloc::format!("hda: {} has no output streams\n", bdf(ctrl)).as_bytes());
        ctrl_close();
        return false;
    }
    let sd = 0x80 + iss * 0x20;
    unsafe {
        SD = sd;
        OUT_BASE = BASE;
    }
    stream_program(sd);

    let mut route = alloc::string::String::new();
    for (k, n) in nids[..len].iter().enumerate() {
        if k > 0 {
            route.push('>');
        }
        route.push_str(&alloc::format!("{:02x}", n));
    }
    let extra = unsafe { EXTRA_JACKS };
    crate::console_write(
        alloc::format!(
            "hda: output [{idx}] {} via {route} ({}{})\n",
            describe(idx),
            if manual { "manual" } else { "auto" },
            if extra > 0 { alloc::format!(", +{extra} more jacks") } else { alloc::string::String::new() }
        )
        .as_bytes(),
    );

    unsafe {
        WRITE = 0;
        LAST_PLAY = 0;
        ACTIVE = idx;
        MANUAL = manual;
        write_volatile(&raw mut STARTED, false);
        write_volatile(&raw mut PRESENT, true);
    }
    true
}

/// Activate the best-scoring output that comes up (auto mode).
fn activate_best() -> bool {
    let n = unsafe { NPINS };
    let mut tried = [false; MAX_PINS];
    loop {
        let mut best = usize::MAX;
        for i in 0..n {
            let s = unsafe { PINS[i].score };
            if !tried[i] && (best == usize::MAX || s > unsafe { PINS[best].score }) {
                best = i;
            }
        }
        if best == usize::MAX {
            return false;
        }
        tried[best] = true;
        if activate(best, false) {
            return true;
        }
    }
}

/// Reset our output stream descriptor and (re)program it, stopped: cyclic
/// BDL over all DMA buffers (zeroed), CBL, LVI, format, stream tag. After
/// SRST the DMA position and LPIB are 0 on every implementation -- which
/// is why a restart goes through here rather than a bare RUN 0->1 (QEMU
/// restarts that at BDL entry 0, real hardware resumes in place).
fn stream_program(sd: usize) {
    let base = unsafe { OUT_BASE };
    // Stream reset handshake.
    bw32(base, sd + SD_CTL, 0);
    wait_us(10_000, || br32(base, sd + SD_CTL) & SD_CTL_RUN == 0);
    bw32(base, sd + SD_CTL, SD_CTL_SRST);
    wait_us(10_000, || br32(base, sd + SD_CTL) & SD_CTL_SRST != 0);
    bw32(base, sd + SD_CTL, 0);
    wait_us(10_000, || br32(base, sd + SD_CTL) & SD_CTL_SRST == 0);

    unsafe {
        let dma = &mut *&raw mut DMA;
        let bdl = &mut *&raw mut BDL;
        for i in 0..NUM_BUFFERS {
            dma.0[i].fill(0);
            bdl.0[i] = BdlEntry {
                addr: dma.0[i].as_ptr() as u64,
                len: OUT_BYTES as u32,
                ioc: 0,
            };
        }
        fence(Ordering::SeqCst);
        let bdl_addr = &raw const BDL as u64;
        bw32(base, sd + SD_BDPL, bdl_addr as u32);
        bw32(base, sd + SD_BDPU, (bdl_addr >> 32) as u32);
    }
    bw32(base, sd + SD_CBL, (OUT_BYTES * NUM_BUFFERS) as u32);
    bw16(base, sd + SD_LVI, (NUM_BUFFERS - 1) as u16);
    bw16(base, sd + SD_FMT, STREAM_FORMAT);
    // Stream tag in CTL bits 23:20; STS byte written as 0 (nothing cleared).
    bw32(base, sd + SD_CTL, STREAM_TAG << 20);
    bw8(base, sd + SD_STS, 0x1C);

}

// -- friendly names (Settings > Sound) -----------------------------------------------

use alloc::string::String;

/// "rear green", "front", "built in" -- where a jack is and its colour.
fn place(cfg: u32) -> String {
    alloc::format!("{}{}", where_name(cfg), color_name(cfg))
}

/// The output as Settings lists it: "Headphones (front)", "Speakers / Line
/// out (rear green)", "HDMI - DELL U2720Q".
fn out_name(i: usize) -> String {
    use alloc::format;
    let p = unsafe { PINS[i] };
    if pin_hdmi(p.pcaps) {
        if p.eld_len > 0 {
            let n = core::str::from_utf8(&p.eld[..p.eld_len as usize]).unwrap_or("display");
            return format!("HDMI - {n}");
        }
        // which HDMI/DP pin of its codec (GPUs expose one per connector)
        let mut k = 1;
        for j in 0..i {
            let q = unsafe { PINS[j] };
            if q.ctrl == p.ctrl && q.codec == p.codec && pin_hdmi(q.pcaps) {
                k += 1;
            }
        }
        return format!("HDMI / DisplayPort {k} ({})", vendor_name(p.vendor));
    }
    if p.digital {
        return format!("S/PDIF digital out ({})", place(p.cfg));
    }
    let color = (p.cfg >> 12) & 0xF;
    let rear = pin_loc(p.cfg) & 0xF == 1;
    match pin_dev(p.cfg) {
        2 => format!("Headphones ({})", place(p.cfg)),
        1 if pin_conn(p.cfg) == 2 => String::from("Speakers (built in)"),
        1 => format!("Speakers ({})", place(p.cfg)),
        0 if color == 4 || (rear && color == 0) || pin_conn(p.cfg) == 2 => format!("Speakers / Line out ({})", place(p.cfg)),
        0 => format!("Line out ({})", place(p.cfg)),
        8 => format!("Line-in jack as output ({})", place(p.cfg)),
        0xA => format!("Mic jack as output ({})", place(p.cfg)),
        _ => format!("Output ({})", place(p.cfg)),
    }
}

fn state_name(s: u8) -> &'static str {
    match s {
        SENSE_YES => "plugged",
        SENSE_NO => "unplugged",
        _ => "",
    }
}

/// Stable identity of a pin across boots: controller PCI address, codec
/// address, pin node id -- the key Settings saves.
fn pin_key(ctrl: u8, codec: u8, nid: u8) -> String {
    alloc::format!("hda:{}:{}:{:02x}", bdf(ctrl as usize), codec, nid)
}

/// `hwinfo`: every HDA controller found ("bb:dd.f", PCI vendor, PCI device).
/// Cached state plus a config-space read; no codec traffic.
pub fn hw_controllers() -> alloc::vec::Vec<(String, u16, u16)> {
    let n = unsafe { NCTRL }.min(MAX_CTRL);
    (0..n)
        .map(|i| {
            let c = unsafe { CTRLS[i] };
            let id = crate::arch::pci::read32(c.bus, c.dev, c.func, 0);
            (bdf(i), c.vendor, (id >> 16) as u16)
        })
        .collect()
}

/// `hwinfo`: the codecs that have output pins ("bb:dd.f", codec address,
/// vendor/device id, vendor name), from the cached pin table.
pub fn hw_codecs() -> alloc::vec::Vec<(String, u8, u32, &'static str)> {
    let mut out: alloc::vec::Vec<(String, u8, u32, &'static str)> = alloc::vec::Vec::new();
    let mut seen: alloc::vec::Vec<(u8, u8)> = alloc::vec::Vec::new();
    for i in 0..output_count() {
        let p = unsafe { PINS[i] };
        if seen.contains(&(p.ctrl, p.codec)) {
            continue;
        }
        seen.push((p.ctrl, p.codec));
        out.push((bdf(p.ctrl as usize), p.codec, p.vendor, vendor_name(p.vendor)));
    }
    out
}

/// `hwinfo`: one output pin in words (codec, jack kind, place, colour, sense, converter).
pub fn output_describe(i: usize) -> String {
    if i < output_count() { describe(i) } else { String::new() }
}

pub fn output_count() -> usize {
    unsafe { NPINS }
}
/// Worth offering in Settings: connected, not a retaskable input jack.
pub fn output_listed(i: usize) -> bool {
    i < output_count() && unsafe { PINS[i].score } > 2
}
pub fn output_is_hdmi(i: usize) -> bool {
    i < output_count() && pin_hdmi(unsafe { PINS[i].pcaps })
}
pub fn output_key(i: usize) -> String {
    let p = unsafe { PINS[i] };
    pin_key(p.ctrl, p.codec, p.nid)
}
pub fn output_name(i: usize) -> String {
    out_name(i)
}
/// "plugged" / "unplugged" / "" (the jack cannot tell); HDMI: "plugged"
/// means a display with audio is attached.
pub fn output_state(i: usize) -> &'static str {
    state_name(unsafe { PINS[i].sense })
}
/// The pin playing now.
pub fn active_output() -> Option<usize> {
    unsafe {
        if PRESENT && ACTIVE < NPINS { Some(ACTIVE) } else { None }
    }
}
/// true = a pin chosen by hand (`audio use <n>` / Settings), false = auto.
pub fn manual() -> bool {
    unsafe { MANUAL }
}
pub fn find_output(key: &str) -> Option<usize> {
    (0..output_count()).find(|&i| output_key(i) == key)
}
/// Present enough to restore a saved choice at boot: not a jack that says
/// "unplugged".
pub fn output_present(i: usize) -> bool {
    i < output_count() && unsafe { PINS[i].sense } != SENSE_NO
}

/// Re-read jack presence on every controller (Settings shows it live).
/// Rate-limited: a sense is a few codec commands per pin.
pub fn refresh_sense() {
    static mut LAST_MS: u64 = 0;
    let now = timer::now_ms();
    unsafe {
        if LAST_MS != 0 && now.saturating_sub(LAST_MS) < 1500 {
            return;
        }
        LAST_MS = now;
    }
    let saved_codec = unsafe { CODEC };
    for c in 0..unsafe { NCTRL } {
        let used = (0..unsafe { NPINS }).any(|i| unsafe { PINS[i].ctrl } as usize == c)
            || (0..unsafe { NIN }).any(|i| unsafe { INS[i].ctrl } as usize == c);
        if !used || !unsafe { CTRL_READY[c] } || !ctrl_use(c) {
            continue;
        }
        for i in 0..unsafe { NPINS } {
            let p = unsafe { PINS[i] };
            if p.ctrl as usize != c || pin_conn(p.cfg) == 1 || p.score <= 2 {
                continue;
            }
            unsafe { CODEC = p.codec as u32 };
            let s = pin_sense(p.nid as u32, p.pcaps, p.cfg);
            unsafe { PINS[i].sense = s };
            if pin_hdmi(p.pcaps) {
                if s == SENSE_YES && p.eld_len == 0 {
                    let (name, n) = read_eld_name(p.nid as u32);
                    unsafe {
                        PINS[i].eld = name;
                        PINS[i].eld_len = n as u8;
                    }
                } else if s != SENSE_YES {
                    unsafe { PINS[i].eld_len = 0 };
                }
            }
        }
        for i in 0..unsafe { NIN } {
            let p = unsafe { INS[i] };
            if p.ctrl as usize != c {
                continue;
            }
            unsafe { CODEC = p.codec as u32 };
            let s = pin_sense(p.nid as u32, p.pcaps, p.cfg);
            unsafe { INS[i].sense = s };
        }
    }
    unsafe { CODEC = saved_codec };
}

// -- inputs / capture ------------------------------------------------------------
//
// Every analog input jack (mic / line in / aux) with a path from an ADC is a
// capture source. Capture runs on the controller's first input stream
// descriptor (0x80), 48 kHz 16-bit stereo like the output, through a cyclic
// BDL of IN_BUFFERS buffers that never stops while capturing; [`capture_poll`]
// reads LPIB and hands the frames recorded since the last poll to the mixer
// (which keeps a software ring for apps, see `mixer::audio_in_*`).
//
// Routing (`capture_open`): pin control IN_EN plus a mic bias (VREF 80%, else
// 50% / 100%) for mic jacks, the pin's boost amp (about +20 dB for a mic,
// 0 dB for line in), the path ADC <- selector/mixer <- pin with connections
// selected, the chosen mixer input unmuted and the others (e.g. Realtek's
// analog loopback 0x0b) muted, and the capture amp (the ADC's input amp, or
// the path mixer's) driven by the Settings gain slider. With no hardware amp
// the gain is applied in software.

/// One analog input jack and its route from an ADC.
#[derive(Clone, Copy)]
struct InPin {
    ctrl: u8,
    codec: u8,
    nid: u8,
    adc: u8,
    /// ADC first, pin last: (widget, connection index used at that widget).
    path: [(u8, u8); 6],
    plen: u8,
    sense: u8,
    cfg: u32,
    pcaps: u32,
}

const MAX_IN: usize = 16;
static mut INS: [InPin; MAX_IN] =
    [InPin { ctrl: 0, codec: 0, nid: 0, adc: 0, path: [(0, 0); 6], plen: 0, sense: 0, cfg: 0, pcaps: 0 }; MAX_IN];
static mut NIN: usize = 0;

const IN_BUFFERS: usize = 16; // 16 x 1024 frames = ~341 ms of ring
const IN_STREAM_TAG: u32 = 2;
const IN_RING_BYTES: usize = IN_BUFFERS * BUFFER_BYTES;

#[repr(C, align(128))]
struct InBdl([BdlEntry; IN_BUFFERS]);
#[repr(C, align(4096))]
struct InDma([[i16; SAMPLES_PER_BUFFER]; IN_BUFFERS]);
static mut IN_BDL: InBdl = InBdl([BdlEntry { addr: 0, len: 0, ioc: 0 }; IN_BUFFERS]);
static mut IN_DMA: InDma = InDma([[0; SAMPLES_PER_BUFFER]; IN_BUFFERS]);

static mut IN_RUNNING: bool = false;
/// The capture source wanted: (ctrl, codec, nid); kept across controller
/// resets so `capture_resume` can bring the stream back.
static mut IN_WANT: Option<(u8, u8, u8)> = None;
static mut IN_CTRL: usize = usize::MAX;
static mut IN_BASE: usize = 0;
static mut IN_SD: usize = 0;
/// Byte offset in the ring up to which frames were handed out.
static mut IN_READ: usize = 0;
static mut IN_LAST_POLL_US: u64 = 0;
static mut IN_FRAMES: u64 = 0;
static mut IN_OVERRUNS: u32 = 0;
/// 0..100, 50 = 0 dB.
static mut IN_GAIN: u32 = 50;
/// Capture amp the gain drives: (codec, nid, input index, amp caps); nid 0 = none.
static mut IN_AMP: (u8, u8, u8, u32) = (0, 0, 0, 0);
static mut IN_LAST_ERR: &str = "";

fn input_jack(pcaps: u32, cfg: u32) -> bool {
    pcaps & (1 << 5) != 0 && pin_conn(cfg) != 1 && !pin_hdmi(pcaps) && matches!(pin_dev(cfg), 8 | 9 | 0xA)
}

/// Depth-first search from an ADC towards pin `target` through selectors and
/// mixers, direct connections first (so a capture mux input wins over the
/// same pin reached through the analog loopback mixer).
fn find_pin_from(nid: u32, target: u32, path: &mut [(u8, u8); 6], depth: usize) -> Option<usize> {
    if depth + 1 >= path.len() {
        return None;
    }
    let mut conns = [0u32; 32];
    let n = conn_list(nid, &mut conns);
    for pass in 0..2 {
        for (idx, &c) in conns[..n].iter().enumerate() {
            if c == 0 || c > 255 {
                continue;
            }
            if pass == 0 && c == target {
                path[depth] = (nid as u8, idx as u8);
                path[depth + 1] = (c as u8, 0);
                return Some(depth + 2);
            }
            let t = wtype(c);
            if pass == 1 && (t == WT_MIXER || t == WT_SELECTOR) {
                path[depth] = (nid as u8, idx as u8);
                if let Some(len) = find_pin_from(c, target, path, depth + 1) {
                    return Some(len);
                }
            }
        }
    }
    None
}

/// Walk the input jacks of codec `CODEC` (caps already cached by
/// `open_codec`) into INS.
fn scan_inputs(ctrl: usize, start: u32, end: u32) {
    let codec = unsafe { CODEC };
    for nid in start..end {
        if wtype(nid) != WT_PIN {
            continue;
        }
        let pcaps = param(nid, PARAM_PIN_CAPS);
        let cfg = command(verb12(nid, VERB_GET_CONFIG_DEFAULT, 0)).unwrap_or(0);
        if !input_jack(pcaps, cfg) {
            continue;
        }
        let mut found = None;
        for adc in start..end {
            if wtype(adc) != WT_INPUT || is_digital(adc) {
                continue;
            }
            let mut path = [(0u8, 0u8); 6];
            if let Some(len) = find_pin_from(adc, nid, &mut path, 0) {
                found = Some((adc, path, len));
                break;
            }
        }
        let Some((adc, path, len)) = found else { continue };
        if unsafe { CAPS[nid as usize] } & (1 << 10) != 0 {
            power_on(nid);
        }
        let sense = pin_sense(nid, pcaps, cfg);
        unsafe {
            if NIN >= MAX_IN {
                return;
            }
            INS[NIN] = InPin {
                ctrl: ctrl as u8,
                codec: codec as u8,
                nid: nid as u8,
                adc: adc as u8,
                path,
                plen: len as u8,
                sense,
                cfg,
                pcaps,
            };
            NIN += 1;
        }
    }
}

pub fn input_count() -> usize {
    unsafe { NIN }
}
pub fn input_key(i: usize) -> String {
    let p = unsafe { INS[i] };
    pin_key(p.ctrl, p.codec, p.nid)
}
pub fn find_input(key: &str) -> Option<usize> {
    (0..input_count()).find(|&i| input_key(i) == key)
}
/// "Microphone (rear pink)", "Line in (rear blue)", "Microphone (front pink)".
pub fn input_name(i: usize) -> String {
    use alloc::format;
    let p = unsafe { INS[i] };
    match pin_dev(p.cfg) {
        0xA if pin_conn(p.cfg) == 2 => String::from("Microphone (built in)"),
        0xA => format!("Microphone ({})", place(p.cfg)),
        8 => format!("Line in ({})", place(p.cfg)),
        _ => format!("Aux in ({})", place(p.cfg)),
    }
}
pub fn input_state(i: usize) -> &'static str {
    state_name(unsafe { INS[i].sense })
}

/// Automatic input: a plugged mic, then a plugged line in, then a built-in
/// mic, then jacks that cannot report presence; never an unplugged jack.
pub fn input_best() -> Option<usize> {
    let mut best = None;
    let mut best_score = 0;
    for i in 0..input_count() {
        let p = unsafe { INS[i] };
        let mic = pin_dev(p.cfg) == 0xA;
        let score = match p.sense {
            SENSE_YES if mic => 100,
            SENSE_YES => 90,
            _ if pin_conn(p.cfg) == 2 && mic => 80,
            SENSE_UNKNOWN if mic => 50,
            SENSE_UNKNOWN => 40,
            _ => 0,
        };
        if score > best_score {
            best = Some(i);
            best_score = score;
        }
    }
    best
}

/// Capture amp step for gain 0..100 (50 = the 0 dB step, 100 = the top).
fn gain_step(caps: u32, gain: u32) -> u32 {
    let offset = caps & 0x7F;
    let steps = (caps >> 8) & 0x7F;
    let g = gain.min(100);
    if g <= 50 {
        offset * g / 50
    } else {
        offset + steps.saturating_sub(offset) * (g - 50) / 50
    }
}

/// Amp caps of `nid` (`PARAM_IN_AMP_CAPS`): its own when it overrides, else
/// the function group's. Needs the codec's CAPS cache (open_codec).
fn in_amp_caps(nid: u32) -> u32 {
    if unsafe { CAPS[nid as usize] } & (1 << 3) != 0 {
        param(nid, PARAM_IN_AMP_CAPS)
    } else {
        param(unsafe { AFG }, PARAM_IN_AMP_CAPS)
    }
}

/// Program the capture amp from IN_GAIN (codec transport must be on IN_CTRL).
fn apply_gain_hw() {
    let (codec, nid, idx, caps) = unsafe { IN_AMP };
    if nid == 0 {
        return;
    }
    unsafe { CODEC = codec as u32 };
    let g = unsafe { IN_GAIN };
    let mute = if g == 0 { 0x80 } else { 0 };
    send(verb4(nid as u32, VERB4_SET_AMP, 0x7000 | ((idx as u32 & 0xF) << 8) | mute | gain_step(caps, g)));
}

/// Software gain applied to captured samples: 1.0 when a hardware amp does
/// the job, else gain/50 (0..2x).
pub fn capture_soft_gain() -> f32 {
    if unsafe { IN_AMP.1 } != 0 { 1.0 } else { (unsafe { IN_GAIN }) as f32 / 50.0 }
}

/// Settings' input gain slider (0..100, 50 = 0 dB).
pub fn capture_set_gain(g: u32) {
    unsafe { IN_GAIN = g.min(100) };
    if unsafe { IN_RUNNING } && ctrl_use(unsafe { IN_CTRL }) {
        apply_gain_hw();
    }
}
pub fn capture_gain() -> u32 {
    unsafe { IN_GAIN }
}

/// Route INS[i] to its ADC and start the capture stream.
fn capture_open(i: usize) -> Result<(), &'static str> {
    if i >= input_count() {
        return Err("no such input");
    }
    capture_halt();
    let p = unsafe { INS[i] };
    let ctrl = p.ctrl as usize;
    if !ctrl_use(ctrl) {
        return Err("the controller does not answer");
    }
    unsafe { CODEC = p.codec as u32 };
    if open_codec().is_none() {
        return Err("the codec does not answer");
    }
    let gcap = r16(GCAP) as usize;
    if (gcap >> 8) & 0xF == 0 {
        return Err("this controller has no input streams");
    }
    let path = &p.path[..p.plen as usize];
    let mut amp = (0u8, 0u8, 0u8, 0u32);
    for (k, &(w, idx)) in path.iter().enumerate() {
        let w = w as u32;
        power_on(w);
        if k + 1 == path.len() {
            break; // the pin, below
        }
        let t = wtype(w);
        let has_in_amp = unsafe { CAPS[w as usize] } & (1 << 1) != 0;
        match t {
            WT_INPUT | WT_SELECTOR => {
                send(verb12(w, VERB_SET_CONN_SELECT, idx as u32));
            },
            WT_MIXER => {
                let mut conns = [0u32; 32];
                let n = conn_list(w, &mut conns);
                for j in 0..n {
                    if j != idx as usize {
                        mute_in(w, j);
                    }
                }
            },
            _ => {},
        }
        if has_in_amp {
            // the first amp on the way (ADC, else the capture mixer) takes
            // the gain slider; others sit at 0 dB
            if amp.1 == 0 {
                amp = (p.codec, w as u8, idx, in_amp_caps(w));
            } else {
                unmute_in(w, idx as usize);
            }
        }
        if t != WT_INPUT {
            unmute_out(w);
        }
    }
    // the pin: input enable + mic bias, boost
    let nid = p.nid as u32;
    let mic = pin_dev(p.cfg) == 0xA;
    let vcaps = (p.pcaps >> 8) & 0xFF;
    let vref = if !mic {
        0
    } else if vcaps & (1 << 4) != 0 {
        4 // 80%
    } else if vcaps & (1 << 1) != 0 {
        1 // 50%
    } else if vcaps & (1 << 5) != 0 {
        5 // 100%
    } else {
        0
    };
    send(verb12(nid, VERB_SET_PIN_CTL, 0x20 | vref));
    if unsafe { CAPS[nid as usize] } & (1 << 1) != 0 {
        let caps = in_amp_caps(nid);
        let offset = caps & 0x7F;
        let steps = (caps >> 8) & 0x7F;
        let quarter_db = ((caps >> 16) & 0x7F) + 1;
        // a mic gets about +20 dB of boost, line level none
        let boost = if mic { (80 / quarter_db).min(steps.saturating_sub(offset)) } else { 0 };
        send(verb4(nid, VERB4_SET_AMP, 0x7000 | (offset + boost)));
    }
    // the converter: our format, the input stream's tag
    let adc = p.adc as u32;
    send(verb4(adc, VERB4_SET_FORMAT, STREAM_FORMAT as u32));
    send(verb12(adc, VERB_SET_STREAM, IN_STREAM_TAG << 4));
    unsafe {
        IN_AMP = amp;
        IN_CTRL = ctrl;
        IN_BASE = CTRLS[ctrl].base;
        IN_SD = 0x80; // input stream descriptor 0
    }
    apply_gain_hw();
    in_stream_start();
    unsafe {
        IN_WANT = Some((p.ctrl, p.codec, p.nid));
        IN_RUNNING = true;
        IN_LAST_ERR = "";
    }
    crate::console_write(
        alloc::format!(
            "hda: capture from [{i}] {} (pin 0x{:02x} -> adc 0x{:02x}, {} gain {})\n",
            input_name(i),
            p.nid,
            p.adc,
            if amp.1 != 0 { "hardware amp," } else { "software" },
            unsafe { IN_GAIN }
        )
        .as_bytes(),
    );
    Ok(())
}

/// Reset + program + run the capture stream descriptor.
fn in_stream_start() {
    let (base, sd) = unsafe { (IN_BASE, IN_SD) };
    bw32(base, sd + SD_CTL, 0);
    wait_us(10_000, || br32(base, sd + SD_CTL) & SD_CTL_RUN == 0);
    bw32(base, sd + SD_CTL, SD_CTL_SRST);
    wait_us(10_000, || br32(base, sd + SD_CTL) & SD_CTL_SRST != 0);
    bw32(base, sd + SD_CTL, 0);
    wait_us(10_000, || br32(base, sd + SD_CTL) & SD_CTL_SRST == 0);
    unsafe {
        let dma = &mut *&raw mut IN_DMA;
        let bdl = &mut *&raw mut IN_BDL;
        for i in 0..IN_BUFFERS {
            dma.0[i].fill(0);
            bdl.0[i] = BdlEntry { addr: dma.0[i].as_ptr() as u64, len: BUFFER_BYTES as u32, ioc: 0 };
        }
        fence(Ordering::SeqCst);
        let bdl_addr = &raw const IN_BDL as u64;
        bw32(base, sd + SD_BDPL, bdl_addr as u32);
        bw32(base, sd + SD_BDPU, (bdl_addr >> 32) as u32);
    }
    bw32(base, sd + SD_CBL, IN_RING_BYTES as u32);
    bw16(base, sd + SD_LVI, (IN_BUFFERS - 1) as u16);
    bw16(base, sd + SD_FMT, STREAM_FORMAT);
    bw32(base, sd + SD_CTL, IN_STREAM_TAG << 20);
    bw8(base, sd + SD_STS, 0x1C);
    unsafe {
        IN_READ = 0;
        IN_LAST_POLL_US = timer::now_us();
    }
    bw32(base, sd + SD_CTL, (IN_STREAM_TAG << 20) | SD_CTL_RUN);
}

/// Stop the capture DMA (the route stays programmed).
fn capture_halt() {
    unsafe {
        if IN_RUNNING && IN_BASE != 0 {
            bw32(IN_BASE, IN_SD + SD_CTL, 0);
        }
        IN_RUNNING = false;
    }
}

/// Start capturing from INS[i].
pub fn capture_start(i: usize) -> Result<(), &'static str> {
    let r = capture_open(i);
    if let Err(e) = r {
        unsafe { IN_LAST_ERR = e };
    }
    r
}

/// Stop capturing (an app's `audio_in_stop`, or nobody listening).
pub fn capture_stop() {
    capture_halt();
    unsafe { IN_WANT = None };
}

pub fn capture_running() -> bool {
    unsafe { IN_RUNNING }
}

/// INS index being captured.
pub fn capture_current() -> Option<usize> {
    let (c, k, n) = unsafe { IN_WANT }?;
    (0..input_count()).find(|&i| {
        let p = unsafe { INS[i] };
        p.ctrl == c && p.codec == k && p.nid == n
    })
}

pub fn capture_error() -> &'static str {
    unsafe { IN_LAST_ERR }
}

/// After a controller reset / re-scan (an output switch): restart the
/// capture stream that was running.
fn capture_resume() {
    if unsafe { IN_RUNNING } || unsafe { IN_WANT }.is_none() {
        return;
    }
    if let Some(i) = capture_current() {
        let _ = capture_start(i);
    } else {
        unsafe { IN_WANT = None };
    }
}

/// Hand the frames captured since the last poll to `sink` (interleaved
/// stereo i16, possibly in two pieces at the ring wrap). Returns frames.
/// Called every desktop frame by the mixer's pump while capturing.
pub fn capture_poll(sink: &mut dyn FnMut(&[i16])) -> usize {
    unsafe {
        if !IN_RUNNING {
            return 0;
        }
        let (base, sd) = (IN_BASE, IN_SD);
        if br32(base, sd + SD_CTL) & SD_CTL_RUN == 0 {
            IN_RUNNING = false; // reset under us
            return 0;
        }
        let lpib = br32(base, sd + SD_LPIB) as usize % IN_RING_BYTES;
        let now = timer::now_us();
        let ring_us = (IN_BUFFERS * FRAMES_PER_BUFFER) as u64 * 1_000_000 / 48_000;
        if now.saturating_sub(IN_LAST_POLL_US) + 30_000 > ring_us {
            // nobody polled for most of a ring: what lies between is a mix
            // of laps -- skip to the present
            IN_READ = lpib & !3;
            IN_LAST_POLL_US = now;
            IN_OVERRUNS += 1;
            return 0;
        }
        IN_LAST_POLL_US = now;
        // keep 32 frames behind LPIB: some controllers count a sample as
        // transferred before its posted write has reached memory
        let avail = ((lpib + IN_RING_BYTES - IN_READ) % IN_RING_BYTES).saturating_sub(128) & !3;
        if avail == 0 {
            return 0;
        }
        fence(Ordering::SeqCst);
        let ring = core::slice::from_raw_parts((&raw const IN_DMA) as *const i16, IN_RING_BYTES / 2);
        let start = IN_READ / 2;
        let n = avail / 2;
        let first = n.min(ring.len() - start);
        sink(&ring[start..start + first]);
        if n > first {
            sink(&ring[..n - first]);
        }
        IN_READ = (IN_READ + avail) % IN_RING_BYTES;
        IN_FRAMES += (avail / 4) as u64;
        avail / 4
    }
}

/// `audio in`: the HDA input jacks, * = capturing.
pub fn input_status() -> String {
    use alloc::format;
    let mut t = String::new();
    let cur = capture_current();
    for i in 0..input_count() {
        let p = unsafe { INS[i] };
        let st = input_state(i);
        t.push_str(&format!(
            "{} {i:2}  {}{}  -- {} codec {} pin 0x{:02x} <- adc 0x{:02x}\n",
            if cur == Some(i) { '*' } else { ' ' },
            input_name(i),
            if st.is_empty() { String::new() } else { format!(", {st}") },
            bdf(p.ctrl as usize),
            p.codec,
            p.nid,
            p.adc
        ));
    }
    if input_count() == 0 {
        t.push_str("  (no HD Audio input jacks)\n");
    }
    unsafe {
        if IN_RUNNING {
            t.push_str(&format!(
                "capture: running, stream 0x{:x} LPIB {} bytes, {} frames captured ({} ms), {} overrun(s), gain {} ({})\n",
                IN_SD,
                br32(IN_BASE, IN_SD + SD_LPIB),
                IN_FRAMES,
                IN_FRAMES * 1000 / 48_000,
                IN_OVERRUNS,
                IN_GAIN,
                if IN_AMP.1 != 0 { "hardware amp" } else { "software" }
            ));
        } else if !IN_LAST_ERR.is_empty() {
            t.push_str(&format!("capture: stopped ({})\n", IN_LAST_ERR));
        } else {
            t.push_str("capture: stopped\n");
        }
    }
    t
}

// -- init / pump -----------------------------------------------------------------

/// Find every HDA controller + codec, pick the output (see [`pin_score`])
/// and bring it up. Idempotent; false when absent or when nothing usable
/// answered.
/// The output pin key saved in Settings (`output_key`), tried first by
/// [`init`]; the mixer sets it from its saved settings before init.
static mut PREFERRED: Option<String> = None;

pub fn set_preferred(key: Option<&str>) {
    unsafe { *(&raw mut PREFERRED) = key.map(String::from) };
}

pub fn init() -> bool {
    if unsafe { PRESENT } {
        return true;
    }
    discover();
    if unsafe { NCTRL } == 0 {
        return false;
    }
    scan_all();
    // the output chosen in Settings, when it is there (not an unplugged jack)
    let pref = unsafe { (*(&raw const PREFERRED)).clone() };
    if let Some(i) = pref.and_then(|k| find_output(&k)).filter(|&i| output_present(i)) {
        if activate(i, true) {
            log_hex("hda: ready (saved choice), output stream @", unsafe { SD } as u32);
            serial::write(b"\n");
            return true;
        }
    }
    if !activate_best() {
        serial::write_str("hda: no usable output path\n");
        return false;
    }
    log_hex("hda: ready, output stream @", unsafe { SD } as u32);
    serial::write(b"\n");
    true
}

pub fn present() -> bool {
    unsafe { PRESENT }
}

/// Short name of the playing output, for Settings > Audio / `audio`.
pub fn label() -> &'static str {
    unsafe {
        if !PRESENT || ACTIVE >= NPINS {
            return "Intel HD Audio";
        }
        let p = PINS[ACTIVE];
        if pin_hdmi(p.pcaps) {
            return "HD Audio - HDMI/DP";
        }
        if p.digital {
            return "HD Audio - S/PDIF";
        }
        match pin_dev(p.cfg) {
            2 => "HD Audio - headphones",
            1 => "HD Audio - speaker",
            0 if pin_loc(p.cfg) & 0xF == 1 => "HD Audio - line out (rear)",
            0 => "HD Audio - line out",
            _ => "HD Audio",
        }
    }
}

/// `audio pins`: every HDA output pin (live presence for the open
/// controller), * = playing.
pub fn status() -> alloc::string::String {
    let mut t = String::new();
    let n = unsafe { NPINS };
    if n == 0 {
        t.push_str("hda: no output pins found\n");
        return t;
    }
    refresh_sense();
    t.push_str("HD Audio outputs (audio use <n> | audio use auto):\n");
    for i in 0..n {
        let mark = if unsafe { PRESENT && ACTIVE == i } { '*' } else { ' ' };
        t.push_str(&alloc::format!("{mark} {i:2}  {}\n       \"{}\"\n", describe(i), out_name(i)));
    }
    t.push_str(&alloc::format!(
        "hda mode: {}\n",
        if unsafe { MANUAL } { "manual (only the chosen pin)" } else { "auto (best jack + other analog jacks of its codec)" }
    ));
    t
}

/// `audio use <n>` (Some) / `audio use auto` (None): re-route playback.
/// Auto re-probes every controller, so a jack plugged after boot counts.
pub fn use_output(n: Option<usize>) -> Result<&'static str, &'static str> {
    let r = use_output_inner(n);
    // a re-route resets the controller(s): bring a running capture back
    capture_resume();
    r
}

fn use_output_inner(n: Option<usize>) -> Result<&'static str, &'static str> {
    discover();
    if unsafe { NCTRL } == 0 {
        return Err("no HD Audio controller");
    }
    match n {
        Some(i) => {
            if i >= unsafe { NPINS } {
                return Err("no such output (see `audio pins`)");
            }
            stop_active();
            if activate(i, true) {
                Ok(label())
            } else {
                activate_best();
                Err("that pin did not come up; back on auto")
            }
        }
        None => {
            stop_active();
            scan_all();
            if activate_best() {
                Ok(label())
            } else {
                Err("no usable output path")
            }
        }
    }
}

/// Keep QUEUE_DEPTH buffers rendered ahead of the playing one; starts the
/// stream on the first call. Returns how many buffers were rendered.
///
/// `WRITE` / `STARTED` are shared with [`watchdog`] (timer IRQ), hence the
/// volatile accesses; the IRQ only reads them and may clear RUN, which the
/// next pump answers with a stream reset + re-prime, so no interrupt
/// masking is needed.
pub fn pump(render: &mut dyn FnMut(&mut [i16])) -> usize {
    if !present() {
        return 0;
    }
    let sd = unsafe { SD };
    let base = unsafe { OUT_BASE };
    let dma = unsafe { &mut *&raw mut DMA };
    let wr = &raw mut WRITE;
    let mut filled = 0;
    unsafe {
        if read_volatile(&raw const STARTED) && br32(base, sd + SD_CTL) & SD_CTL_RUN == 0 {
            // The watchdog paused us (pump stalled past the queue), or the
            // emulator reset the stream: everything rendered has played,
            // so re-prime from a freshly reset stream.
            write_volatile(&raw mut STARTED, false);
            stream_program(sd);
        }
        if !read_volatile(&raw const STARTED) {
            let target = TARGET.clamp(MIN_QUEUE, QUEUE_DEPTH);
            for i in 0..target {
                render(&mut dma.0[i]);
            }
            fence(Ordering::SeqCst);
            write_volatile(wr, target % NUM_BUFFERS);
            LAST_PLAY = 0;
            write_volatile(&raw mut STARTED, true);
            bw32(base, sd + SD_CTL, (STREAM_TAG << 20) | SD_CTL_RUN);
            return target;
        }
        let mut write = read_volatile(wr);
        let lpib = br32(base, sd + SD_LPIB) as usize;
        let play = (lpib / OUT_BYTES) % NUM_BUFFERS;
        // Silence what the hardware has finished with; if that sweep
        // crosses our write cursor, the pump fell behind (underrun).
        let mut underrun = false;
        while LAST_PLAY != play {
            if LAST_PLAY == write {
                underrun = true;
            }
            dma.0[LAST_PLAY].fill(0);
            LAST_PLAY = (LAST_PLAY + 1) % NUM_BUFFERS;
        }
        if play == write {
            underrun = true;
        }
        if underrun {
            // The playing buffer is silence (the landing pad below / a
            // swept one); restart the queue right after it.
            write = (play + 1) % NUM_BUFFERS;
            write_volatile(wr, write);
        }
        // Buffers play..write-1 belong to the hardware (play included).
        let mut queued = (write + NUM_BUFFERS - play) % NUM_BUFFERS;
        let target = TARGET.clamp(MIN_QUEUE, QUEUE_DEPTH);
        while queued < target {
            render(&mut dma.0[write]);
            fence(Ordering::SeqCst);
            write = (write + 1) % NUM_BUFFERS;
            write_volatile(wr, write);
            queued += 1;
            filled += 1;
        }
        // Landing pad: everything outside [play, write) is silence (swept
        // above / zeroed at stream reset; this re-zeroes the first one as
        // cheap insurance), so if pumping stops the hardware runs into
        // silence and the watchdog halts it there -- never into a stale lap.
        if filled > 0 {
            dma.0[write].fill(0);
        }
        fence(Ordering::SeqCst);
        bw8(base, sd + SD_STS, 0x1C);
    }
    filled
}

/// Timer-IRQ hook (100 Hz): an HDA ring never stops by itself, so when
/// nobody pumps (a long blocking load, a screen that doesn't run the audio
/// pump) it would replay the last lap forever. Once playback reaches the
/// silent buffers past the queue, stop the stream (see STOP_AFTER_SILENT);
/// the next [`pump`] resets, refills and restarts it. Reads two registers, no rendering.
pub fn watchdog() {
    unsafe {
        if !read_volatile(&raw const PRESENT) || !read_volatile(&raw const STARTED) {
            return;
        }
        let sd = SD;
        let base = OUT_BASE;
        let ctl = br32(base, sd + SD_CTL);
        if ctl & SD_CTL_RUN == 0 {
            return;
        }
        let play = (br32(base, sd + SD_LPIB) as usize / OUT_BYTES) % NUM_BUFFERS;
        let write = read_volatile(&raw const WRITE);
        let queued = (write + NUM_BUFFERS - play) % NUM_BUFFERS;
        if queued == 0 || queued > QUEUE_DEPTH {
            // `past` buffers of silence fetched since the queue ran dry.
            // Wait until the codec-side FIFO (emulators buffer tens of ms
            // past LPIB, and a stop discards it) can only hold silence.
            let past = (play + NUM_BUFFERS - write) % NUM_BUFFERS;
            if past >= STOP_AFTER_SILENT {
                bw32(base, sd + SD_CTL, ctl & !SD_CTL_RUN & 0x00FF_FFFF);
            }
        }
    }
}
