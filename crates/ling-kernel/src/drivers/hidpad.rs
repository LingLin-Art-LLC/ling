//! USB / Bluetooth gamepads: a HID report-descriptor parser that finds the
//! sticks, triggers, hat and buttons of a generic HID gamepad or joystick,
//! plus hand-written decoders for the pads with fixed formats -- Xbox 360
//! (wired + wireless receiver, XInput vendor class), Xbox One / Series (GIP),
//! DualShock 4, DualSense, Switch Pro -- and the packets the host sends them
//! (GIP power-on and acks, the Switch USB handshake, LEDs, rumble). Reports
//! decode into one [`PadState`]; [`super::gamepad`] keeps one per pad.
//!
//! Pure data code: no bus access, so it is unit-testable on the host.

/// One field of an input report: `bits` wide at bit offset `off`, logical
/// range `min..=max`, what it is (`usage`: Generic Desktop usage id).
#[derive(Clone, Copy, Default)]
pub struct Field {
    pub off: u16,
    pub bits: u8,
    pub min: i32,
    pub max: i32,
    pub usage: u8,
}

pub const MAX_AXES: usize = 8;
pub const MAX_BUTTONS: usize = 32;

/// Where the interesting fields sit in a pad's input report.
#[derive(Clone, Copy)]
pub struct Layout {
    /// Report ID the fields belong to (0 = the device uses none).
    pub report_id: u8,
    pub axes: [Field; MAX_AXES],
    pub n_axes: u8,
    pub hat: Option<Field>,
    /// First button's bit offset and how many 1-bit buttons follow it.
    pub buttons_off: u16,
    pub n_buttons: u8,
    pub xbox360: bool,
    /// Which decoder reads the reports: a fixed vendor format, or the descriptor fields above.
    pub kind: PadKind,
}

/// The pad families with a fixed report format (decoded by hand, buttons in the
/// Xbox order A B X Y LB RB Back Start LS RS Guide), or `Generic` (the HID
/// report descriptor's fields, buttons in descriptor order).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PadKind {
    Generic,
    /// Wired Xbox 360 controller (vendor class FF/5D/01, XInput).
    Xbox360,
    /// Xbox 360 wireless receiver interface (FF/5D/81): one per pad, 360 reports behind a 4-byte header.
    Xbox360Wireless,
    /// Xbox One / Series controller over USB (vendor class FF/47/D0, GIP). Silent until it gets the power-on packet.
    XboxOne,
    /// Sony DualShock 4 (HID report 0x01 over USB, 0x11 over Bluetooth).
    DualShock4,
    /// Sony DualSense / DualSense Edge (HID report 0x01 over USB, 0x31 over Bluetooth).
    DualSense,
    /// Nintendo Switch Pro Controller (full report 0x30 after the USB handshake, simple 0x3F otherwise).
    SwitchPro,
    /// An Xbox Wireless Controller over Bluetooth -- BLE HID over GATT (Series pads, recent One
    /// firmware) or classic HID (One S) -- and the pads that pose as one. Its reports are read
    /// through the HID descriptor's fields like a generic pad (the Bluetooth transport puts the BLE
    /// button order right first); rumble is HID output report 3.
    XboxBle,
}

impl Layout {
    pub const EMPTY: Layout = Layout { report_id: 0, axes: [Field { off: 0, bits: 0, min: 0, max: 0, usage: 0 }; MAX_AXES], n_axes: 0, hat: None, buttons_off: 0, n_buttons: 0, xbox360: false, kind: PadKind::Generic };

    /// The wired Xbox 360 controller's 20-byte report (not HID).
    pub const XBOX360: Layout = Layout { xbox360: true, kind: PadKind::Xbox360, ..Layout::EMPTY };
    pub const XBOX360_WIRELESS: Layout = Layout { kind: PadKind::Xbox360Wireless, ..Layout::EMPTY };
    pub const XBOXONE: Layout = Layout { kind: PadKind::XboxOne, ..Layout::EMPTY };
    pub const DUALSHOCK4: Layout = Layout { kind: PadKind::DualShock4, ..Layout::EMPTY };
    pub const DUALSENSE: Layout = Layout { kind: PadKind::DualSense, ..Layout::EMPTY };
    pub const SWITCHPRO: Layout = Layout { kind: PadKind::SwitchPro, ..Layout::EMPTY };

    pub fn usable(&self) -> bool {
        self.kind != PadKind::Generic || self.xbox360 || self.n_axes > 0 || self.n_buttons > 0
    }

    /// Human name of the pad family (Settings / `os_info("gamepad")`).
    pub fn name(&self) -> &'static str {
        match self.kind {
            PadKind::Generic => "USB gamepad",
            PadKind::Xbox360 => "Xbox 360 controller",
            PadKind::Xbox360Wireless => "Xbox 360 wireless controller",
            PadKind::XboxOne => "Xbox One / Series controller",
            PadKind::DualShock4 => "DualShock 4",
            PadKind::DualSense => "DualSense",
            PadKind::SwitchPro => "Switch Pro Controller",
            PadKind::XboxBle => "Xbox Wireless Controller (Bluetooth)",
        }
    }
}

/// Sony DualShock 4 product ids (v1, v2, the USB wireless adaptor).
const DS4_PIDS: [u16; 4] = [0x05C4, 0x09CC, 0x0BA0, 0x0BA1];
/// Sony DualSense, DualSense Edge.
const DUALSENSE_PIDS: [u16; 2] = [0x0CE6, 0x0DF2];

/// Which decoder a pad interface needs. `class/sub/proto` are the USB
/// interface's (a Bluetooth HID device passes 3/0/0), `desc` its HID report
/// descriptor (may be empty for the vendor-class pads). Transport-agnostic:
/// the xHCI driver calls it at enumeration, Bluetooth HID can call it with the
/// SDP descriptor.
pub fn identify(vid: u16, pid: u16, class: u8, sub: u8, proto: u8, desc: &[u8]) -> Option<Layout> {
    if class == 0xFF && sub == 0x5D {
        return match proto {
            0x01 => Some(Layout::XBOX360),
            0x81 => Some(Layout::XBOX360_WIRELESS),
            _ => None,
        };
    }
    if class == 0xFF && sub == 0x47 && proto == 0xD0 {
        return Some(Layout::XBOXONE);
    }
    if class != 3 {
        return None;
    }
    if vid == 0x054C && DS4_PIDS.contains(&pid) {
        return Some(Layout::DUALSHOCK4);
    }
    if vid == 0x054C && DUALSENSE_PIDS.contains(&pid) {
        return Some(Layout::DUALSENSE);
    }
    if vid == 0x057E && pid == 0x2009 {
        return Some(Layout::SWITCHPRO);
    }
    parse_descriptor(desc)
}

/// Parse a HID report descriptor. `Some` only for a device whose application
/// collection is a Game Pad (0x05), Joystick (0x04) or Multi-axis Controller
/// (0x08) -- a keyboard's consumer-control interface is not one.
pub fn parse_descriptor(d: &[u8]) -> Option<Layout> {
    let mut lay = Layout::EMPTY;
    // Global items.
    let (mut usage_page, mut lmin, mut lmax, mut rsize, mut rcount, mut rid) = (0u32, 0i32, 0i32, 0u32, 0u32, 0u32);
    // Local items: the usages named before the next main item.
    let mut usages: [u32; 32] = [0; 32];
    let mut n_usages = 0usize;
    let (mut umin, mut umax): (Option<u32>, Option<u32>) = (None, None);
    let mut is_pad = false;
    let mut depth = 0u32;
    // Input bit offset per report id (the one we keep); reset when the id changes.
    let mut bit = 0u32;
    let mut cur_rid = 0u32;
    let mut kept_rid: Option<u32> = None;
    let mut i = 0usize;
    while i < d.len() {
        let b = d[i];
        i += 1;
        if b == 0xFE {
            // Long item: skip it.
            if i < d.len() {
                i += 2 + d[i] as usize;
            }
            continue;
        }
        let size = match b & 3 {
            3 => 4,
            n => n as usize,
        };
        if i + size > d.len() {
            break;
        }
        let mut val: u32 = 0;
        for k in 0..size {
            val |= (d[i + k] as u32) << (8 * k);
        }
        let sval: i32 = match size {
            1 => d[i] as i8 as i32,
            2 => i16::from_le_bytes([d[i], d[i + 1]]) as i32,
            4 => val as i32,
            _ => 0,
        };
        i += size;
        let (typ, tag) = ((b >> 2) & 3, b >> 4);
        match typ {
            1 => match tag {
                0 => usage_page = val,
                1 => lmin = sval,
                2 => lmax = if size == 4 { sval } else if lmin < 0 { sval } else { val as i32 },
                7 => rsize = val,
                8 => {
                    rid = val;
                    if cur_rid != rid {
                        cur_rid = rid;
                        if kept_rid.is_none() || kept_rid != Some(rid) {
                            bit = 0;
                        }
                    }
                },
                9 => rcount = val,
                _ => {},
            },
            2 => match tag {
                0 => {
                    if n_usages < usages.len() {
                        // A 32-bit usage carries its own page in the top half.
                        usages[n_usages] = if size == 4 { val } else { (usage_page << 16) | val };
                        n_usages += 1;
                    }
                },
                1 => umin = Some(if size == 4 { val } else { (usage_page << 16) | val }),
                2 => umax = Some(if size == 4 { val } else { (usage_page << 16) | val }),
                _ => {},
            },
            0 => {
                match tag {
                    10 => {
                        // Collection: the application one says what this is.
                        if depth == 0 && val == 1 && n_usages > 0 {
                            let u = usages[0];
                            if u >> 16 == 1 && matches!(u & 0xFFFF, 4 | 5 | 8) {
                                is_pad = true;
                            }
                        }
                        depth += 1;
                    },
                    12 => depth = depth.saturating_sub(1),
                    8 => {
                        // Input item.
                        let constant = val & 1 != 0;
                        let total = rsize * rcount;
                        // Fields of a report other than the one we keep are skipped
                        // (their offsets are counted in their own report).
                        let keep = is_pad && (kept_rid.is_none() || kept_rid == Some(cur_rid));
                        if !constant && keep {
                            for n in 0..rcount {
                                let usage = if n_usages > 0 {
                                    usages[(n as usize).min(n_usages - 1)]
                                } else if let (Some(a), Some(z)) = (umin, umax) {
                                    (a + n).min(z)
                                } else {
                                    0
                                };
                                let page = usage >> 16;
                                let id = usage & 0xFFFF;
                                let off = (bit + n * rsize) as u16;
                                if page == 1 && (0x30..=0x37).contains(&id) && (lay.n_axes as usize) < MAX_AXES && rsize <= 32 {
                                    lay.axes[lay.n_axes as usize] = Field { off, bits: rsize as u8, min: lmin, max: lmax, usage: id as u8 };
                                    lay.n_axes += 1;
                                    kept_rid = Some(cur_rid);
                                } else if page == 1 && id == 0x39 && lay.hat.is_none() {
                                    lay.hat = Some(Field { off, bits: rsize as u8, min: lmin, max: lmax, usage: 0x39 });
                                    kept_rid = Some(cur_rid);
                                } else if page == 9 && rsize == 1 && lay.n_buttons == 0 {
                                    lay.buttons_off = off;
                                    lay.n_buttons = rcount.min(MAX_BUTTONS as u32) as u8;
                                    kept_rid = Some(cur_rid);
                                    break;
                                }
                            }
                        }
                        if kept_rid.is_none() || kept_rid == Some(cur_rid) {
                            bit += total;
                        }
                    },
                    _ => {},
                }
                // Local items reset after every main item.
                n_usages = 0;
                umin = None;
                umax = None;
            },
            _ => {},
        }
    }
    if !is_pad || !lay.usable() {
        return None;
    }
    lay.report_id = kept_rid.unwrap_or(0) as u8;
    Some(lay)
}

/// What apps see.
#[derive(Clone, Copy, Default)]
pub struct PadState {
    pub connected: bool,
    /// LX, LY, RX, RY, LT, RT (then any further axes), -1..1 (triggers 0..1).
    pub axes: [f32; MAX_AXES],
    pub buttons: u32,
    /// 0..7 clockwise from up, -1 centred.
    pub hat: i8,
    pub xbox: bool,
}

fn bits_at(r: &[u8], off: u16, bits: u8) -> u32 {
    let mut v: u32 = 0;
    for k in 0..bits as u16 {
        let b = off + k;
        let byte = (b / 8) as usize;
        if byte < r.len() && r[byte] & (1 << (b % 8)) != 0 {
            v |= 1 << k;
        }
    }
    v
}

fn norm(f: &Field, raw: u32) -> f32 {
    if f.max <= f.min {
        return 0.0;
    }
    let v: i32 = if f.min < 0 {
        // Signed field: sign-extend from its width.
        let shift = 32 - f.bits as u32;
        ((raw << shift) as i32) >> shift
    } else {
        raw as i32
    };
    let span = (f.max - f.min) as f32;
    let centred = (v - f.min) as f32 / span * 2.0 - 1.0;
    centred.clamp(-1.0, 1.0)
}

// Canonical button bits (every fixed-format decoder uses them; the host's
// names map onto them in hostcompat::pad_bit).
pub const B_A: u32 = 0;
pub const B_B: u32 = 1;
pub const B_X: u32 = 2;
pub const B_Y: u32 = 3;
pub const B_LB: u32 = 4;
pub const B_RB: u32 = 5;
pub const B_BACK: u32 = 6;
pub const B_START: u32 = 7;
pub const B_LS: u32 = 8;
pub const B_RS: u32 = 9;
pub const B_GUIDE: u32 = 10;
/// The extra button: touchpad click (Sony), Capture (Switch).
pub const B_MISC: u32 = 11;
pub const B_UP: u32 = 12;
pub const B_DOWN: u32 = 13;
pub const B_LEFT: u32 = 14;
pub const B_RIGHT: u32 = 15;

fn bit(m: &mut u32, cond: bool, b: u32) {
    if cond {
        *m |= 1 << b;
    }
}

/// The hat (0..7 clockwise from up, -1 centred) from four d-pad flags.
fn hat_of(up: bool, down: bool, left: bool, right: bool) -> i8 {
    let v = (up && !down) as i32 - (down && !up) as i32; // +1 up
    let h = (right && !left) as i32 - (left && !right) as i32; // +1 right
    match (v, h) {
        (1, 0) => 0,
        (1, 1) => 1,
        (0, 1) => 2,
        (-1, 1) => 3,
        (-1, 0) => 4,
        (-1, -1) => 5,
        (0, -1) => 6,
        (1, -1) => 7,
        _ => -1,
    }
}

/// D-pad bits 12..15 from a hat value.
pub fn hat_bits(h: i8) -> u32 {
    let mut m = 0;
    if h >= 0 {
        bit(&mut m, matches!(h, 7 | 0 | 1), B_UP);
        bit(&mut m, matches!(h, 3 | 4 | 5), B_DOWN);
        bit(&mut m, matches!(h, 5 | 6 | 7), B_LEFT);
        bit(&mut m, matches!(h, 1 | 2 | 3), B_RIGHT);
    }
    m
}

/// A 0..255 stick byte centred at 128 -> -1..1.
fn u8_axis(v: u8) -> f32 {
    ((v as f32 - 127.5) / 127.5).clamp(-1.0, 1.0)
}

fn i16_axis(r: &[u8], i: usize) -> f32 {
    (i16::from_le_bytes([r[i], r[i + 1]]) as f32 / 32767.0).clamp(-1.0, 1.0)
}

/// Wired Xbox 360 report (`r[0]` = 0x00, `r[1]` = 0x14). Sticks are signed
/// 16-bit with Y UP = positive (the old decoder negated Y, flipping it).
fn decode_360(r: &[u8], st: &mut PadState) -> bool {
    if r.len() < 14 || r[0] != 0x00 {
        return false;
    }
    st.xbox = true;
    st.axes[0] = i16_axis(r, 6);
    st.axes[1] = i16_axis(r, 8);
    st.axes[2] = i16_axis(r, 10);
    st.axes[3] = i16_axis(r, 12);
    st.axes[4] = r[4] as f32 / 255.0;
    st.axes[5] = r[5] as f32 / 255.0;
    let (b2, b3) = (r[2], r[3]);
    let mut m = 0u32;
    bit(&mut m, b3 & 0x10 != 0, B_A);
    bit(&mut m, b3 & 0x20 != 0, B_B);
    bit(&mut m, b3 & 0x40 != 0, B_X);
    bit(&mut m, b3 & 0x80 != 0, B_Y);
    bit(&mut m, b3 & 0x01 != 0, B_LB);
    bit(&mut m, b3 & 0x02 != 0, B_RB);
    bit(&mut m, b2 & 0x20 != 0, B_BACK);
    bit(&mut m, b2 & 0x10 != 0, B_START);
    bit(&mut m, b2 & 0x40 != 0, B_LS);
    bit(&mut m, b2 & 0x80 != 0, B_RS);
    bit(&mut m, b3 & 0x04 != 0, B_GUIDE);
    st.hat = hat_of(b2 & 1 != 0, b2 & 2 != 0, b2 & 4 != 0, b2 & 8 != 0);
    st.buttons = m | hat_bits(st.hat);
    true
}

/// Xbox 360 wireless receiver: presence packets (`r[0] & 0x08`) and pad
/// reports behind a 4-byte header (`r[1]` = 0x01).
fn decode_360w(r: &[u8], st: &mut PadState) -> bool {
    if r.len() >= 2 && r[0] & 0x08 != 0 {
        let on = r[1] & 0x80 != 0;
        *st = PadState { connected: on, hat: -1, xbox: true, ..PadState::default() };
        return true;
    }
    if r.len() < 18 || r[1] != 0x01 {
        return false;
    }
    st.connected = true;
    decode_360(&r[4..], st)
}

/// Xbox One / Series GIP: input (0x20) and the Guide button (0x07, "virtual key").
fn decode_gip(r: &[u8], st: &mut PadState) -> bool {
    if r.len() < 5 {
        return false;
    }
    st.xbox = true;
    match r[0] {
        0x07 => {
            st.buttons = (st.buttons & !(1 << B_GUIDE)) | (((r[4] & 0x01) as u32) << B_GUIDE);
            true
        },
        0x20 if r.len() >= 18 => {
            let (b4, b5) = (r[4], r[5]);
            let mut m = st.buttons & (1 << B_GUIDE); // the Guide button comes in its own packets
            bit(&mut m, b4 & 0x04 != 0, B_START); // Menu
            bit(&mut m, b4 & 0x08 != 0, B_BACK); // View
            bit(&mut m, b4 & 0x10 != 0, B_A);
            bit(&mut m, b4 & 0x20 != 0, B_B);
            bit(&mut m, b4 & 0x40 != 0, B_X);
            bit(&mut m, b4 & 0x80 != 0, B_Y);
            bit(&mut m, b5 & 0x10 != 0, B_LB);
            bit(&mut m, b5 & 0x20 != 0, B_RB);
            bit(&mut m, b5 & 0x40 != 0, B_LS);
            bit(&mut m, b5 & 0x80 != 0, B_RS);
            st.hat = hat_of(b5 & 1 != 0, b5 & 2 != 0, b5 & 4 != 0, b5 & 8 != 0);
            st.buttons = m | hat_bits(st.hat);
            st.axes[4] = (u16::from_le_bytes([r[6], r[7]]) as f32 / 1023.0).clamp(0.0, 1.0);
            st.axes[5] = (u16::from_le_bytes([r[8], r[9]]) as f32 / 1023.0).clamp(0.0, 1.0);
            st.axes[0] = i16_axis(r, 10);
            st.axes[1] = i16_axis(r, 12);
            st.axes[2] = i16_axis(r, 14);
            st.axes[3] = i16_axis(r, 16);
            true
        },
        _ => false,
    }
}

/// The Sony face / shoulder bytes shared by DS4 and DualSense: `hb` = hat in
/// the low nibble + Square Cross Circle Triangle, `sb` = L1 R1 L2 R2 Share
/// Options L3 R3, `pb` = PS, touchpad click.
fn sony_buttons(hb: u8, sb: u8, pb: u8, st: &mut PadState) {
    let mut m = 0u32;
    bit(&mut m, hb & 0x20 != 0, B_A); // Cross
    bit(&mut m, hb & 0x40 != 0, B_B); // Circle
    bit(&mut m, hb & 0x10 != 0, B_X); // Square
    bit(&mut m, hb & 0x80 != 0, B_Y); // Triangle
    bit(&mut m, sb & 0x01 != 0, B_LB);
    bit(&mut m, sb & 0x02 != 0, B_RB);
    bit(&mut m, sb & 0x10 != 0, B_BACK); // Share / Create
    bit(&mut m, sb & 0x20 != 0, B_START); // Options
    bit(&mut m, sb & 0x40 != 0, B_LS);
    bit(&mut m, sb & 0x80 != 0, B_RS);
    bit(&mut m, pb & 0x01 != 0, B_GUIDE); // PS
    bit(&mut m, pb & 0x02 != 0, B_MISC); // touchpad click
    let h = hb & 0x0F;
    st.hat = if h <= 7 { h as i8 } else { -1 };
    st.buttons = m | hat_bits(st.hat);
}

/// The DS4-style 9 bytes: LX LY RX RY, hat+face, shoulders, PS/pad (+counter), L2 R2.
fn sony_ds4_body(b: &[u8], st: &mut PadState) {
    st.axes[0] = u8_axis(b[0]);
    st.axes[1] = -u8_axis(b[1]);
    st.axes[2] = u8_axis(b[2]);
    st.axes[3] = -u8_axis(b[3]);
    sony_buttons(b[4], b[5], b[6], st);
    st.axes[4] = b[7] as f32 / 255.0;
    st.axes[5] = b[8] as f32 / 255.0;
}

/// DualShock 4: USB report 0x01, Bluetooth 0x11 (2 more header bytes).
fn decode_ds4(r: &[u8], st: &mut PadState) -> bool {
    let b = match r.first() {
        Some(0x01) if r.len() >= 10 => &r[1..],
        Some(0x11) if r.len() >= 12 => &r[3..],
        _ => return false,
    };
    sony_ds4_body(b, st);
    true
}

/// DualSense: USB report 0x01 (64 bytes), Bluetooth 0x31; a short 0x01 is
/// the Bluetooth "simple" report, laid out like a DS4's.
fn decode_dualsense(r: &[u8], st: &mut PadState) -> bool {
    let b = match r.first() {
        Some(0x01) if r.len() >= 30 => &r[1..],
        Some(0x01) if r.len() >= 10 => {
            sony_ds4_body(&r[1..], st);
            return true;
        },
        Some(0x31) if r.len() >= 13 => &r[2..],
        _ => return false,
    };
    st.axes[0] = u8_axis(b[0]);
    st.axes[1] = -u8_axis(b[1]);
    st.axes[2] = u8_axis(b[2]);
    st.axes[3] = -u8_axis(b[3]);
    st.axes[4] = b[4] as f32 / 255.0;
    st.axes[5] = b[5] as f32 / 255.0;
    sony_buttons(b[7], b[8], b[9], st);
    true
}

/// A Switch Pro 12-bit stick value (centre ~2048, uncalibrated reach ~±1600).
fn switch_axis(v: u16) -> f32 {
    ((v as f32 - 2048.0) / 1600.0).clamp(-1.0, 1.0)
}

/// Switch Pro Controller. Buttons go by POSITION (south = A, as on an Xbox
/// pad: Nintendo's B), as SDL and Steam do. Full report 0x30 (and the 0x21
/// subcommand replies, which carry the same input bytes) or the simple HID
/// report 0x3F.
fn decode_switch(r: &[u8], st: &mut PadState) -> bool {
    match r.first() {
        Some(0x30 | 0x21) if r.len() >= 12 => {
            let (rb, sb, lb) = (r[3], r[4], r[5]);
            let mut m = 0u32;
            bit(&mut m, rb & 0x04 != 0, B_A); // B (south)
            bit(&mut m, rb & 0x08 != 0, B_B); // A (east)
            bit(&mut m, rb & 0x01 != 0, B_X); // Y (west)
            bit(&mut m, rb & 0x02 != 0, B_Y); // X (north)
            bit(&mut m, lb & 0x40 != 0, B_LB); // L
            bit(&mut m, rb & 0x40 != 0, B_RB); // R
            bit(&mut m, sb & 0x01 != 0, B_BACK); // Minus
            bit(&mut m, sb & 0x02 != 0, B_START); // Plus
            bit(&mut m, sb & 0x08 != 0, B_LS);
            bit(&mut m, sb & 0x04 != 0, B_RS);
            bit(&mut m, sb & 0x10 != 0, B_GUIDE); // Home
            bit(&mut m, sb & 0x20 != 0, B_MISC); // Capture
            st.hat = hat_of(lb & 0x02 != 0, lb & 0x01 != 0, lb & 0x08 != 0, lb & 0x04 != 0);
            st.buttons = m | hat_bits(st.hat);
            st.axes[4] = if lb & 0x80 != 0 { 1.0 } else { 0.0 }; // ZL
            st.axes[5] = if rb & 0x80 != 0 { 1.0 } else { 0.0 }; // ZR
            let lx = r[6] as u16 | ((r[7] as u16 & 0x0F) << 8);
            let ly = (r[7] as u16 >> 4) | ((r[8] as u16) << 4);
            let rx = r[9] as u16 | ((r[10] as u16 & 0x0F) << 8);
            let ry = (r[10] as u16 >> 4) | ((r[11] as u16) << 4);
            st.axes[0] = switch_axis(lx);
            st.axes[1] = switch_axis(ly);
            st.axes[2] = switch_axis(rx);
            st.axes[3] = switch_axis(ry);
            true
        },
        Some(0x3F) if r.len() >= 12 => {
            let (b1, b2) = (r[1], r[2]);
            let mut m = 0u32;
            bit(&mut m, b1 & 0x01 != 0, B_A); // B
            bit(&mut m, b1 & 0x02 != 0, B_B); // A
            bit(&mut m, b1 & 0x04 != 0, B_X); // Y
            bit(&mut m, b1 & 0x08 != 0, B_Y); // X
            bit(&mut m, b1 & 0x10 != 0, B_LB);
            bit(&mut m, b1 & 0x20 != 0, B_RB);
            bit(&mut m, b2 & 0x01 != 0, B_BACK);
            bit(&mut m, b2 & 0x02 != 0, B_START);
            bit(&mut m, b2 & 0x04 != 0, B_LS);
            bit(&mut m, b2 & 0x08 != 0, B_RS);
            bit(&mut m, b2 & 0x10 != 0, B_GUIDE);
            bit(&mut m, b2 & 0x20 != 0, B_MISC);
            st.hat = if r[3] <= 7 { r[3] as i8 } else { -1 };
            st.buttons = m | hat_bits(st.hat);
            st.axes[4] = if b1 & 0x40 != 0 { 1.0 } else { 0.0 };
            st.axes[5] = if b1 & 0x80 != 0 { 1.0 } else { 0.0 };
            let u = |i: usize| ((u16::from_le_bytes([r[i], r[i + 1]]) as f32 - 32768.0) / 32768.0).clamp(-1.0, 1.0);
            st.axes[0] = u(4);
            st.axes[1] = -u(6);
            st.axes[2] = u(8);
            st.axes[3] = -u(10);
            true
        },
        _ => false,
    }
}

/// Decode one report into `st` (`r` starts with the report ID byte when the
/// device uses IDs). Returns false when the report is for another ID (or is
/// not an input report: GIP acks / announces, Switch handshake replies).
/// Every decoder leaves sticks at -1..1 with Y UP = positive, triggers 0..1,
/// and buttons in the canonical bits `B_*` (generic pads: descriptor order).
pub fn decode(lay: &Layout, r: &[u8], st: &mut PadState) -> bool {
    let fixed = match lay.kind {
        PadKind::Xbox360Wireless => return decode_360w(r, st),
        PadKind::Xbox360 => Some(decode_360 as fn(&[u8], &mut PadState) -> bool),
        PadKind::XboxOne => Some(decode_gip as fn(&[u8], &mut PadState) -> bool),
        PadKind::DualShock4 => Some(decode_ds4 as fn(&[u8], &mut PadState) -> bool),
        PadKind::DualSense => Some(decode_dualsense as fn(&[u8], &mut PadState) -> bool),
        PadKind::SwitchPro => Some(decode_switch as fn(&[u8], &mut PadState) -> bool),
        PadKind::Generic if lay.xbox360 => Some(decode_360 as fn(&[u8], &mut PadState) -> bool),
        PadKind::Generic | PadKind::XboxBle => None,
    };
    st.connected = true;
    if let Some(f) = fixed {
        return f(r, st);
    }
    let body = if lay.report_id != 0 {
        if r.is_empty() || r[0] != lay.report_id {
            return false;
        }
        &r[1..]
    } else {
        r
    };
    // Axes in a stable order: X Y Z Rz Rx Ry -> LX LY RX RY LT RT on a DualShock,
    // anything else after.
    let mut slot_of = |usage: u8| -> usize {
        match usage {
            0x30 => 0,
            0x31 => 1,
            0x32 => 2,
            0x35 => 3,
            0x33 => 4,
            0x34 => 5,
            0x36 => 6,
            _ => 7,
        }
    };
    for a in lay.axes.iter().take(lay.n_axes as usize) {
        let v = norm(a, bits_at(body, a.off, a.bits));
        let slot = slot_of(a.usage);
        // Y axes point down in HID; apps expect up = positive.
        st.axes[slot] = if slot == 1 || slot == 3 { -v } else { v };
        if slot == 4 || slot == 5 {
            st.axes[slot] = (v + 1.0) / 2.0; // triggers 0..1
        }
    }
    if let Some(h) = lay.hat {
        let raw = bits_at(body, h.off, h.bits) as i32;
        let v = raw - h.min;
        st.hat = if v >= 0 && v <= h.max - h.min && h.max - h.min >= 7 { v as i8 } else { -1 };
    }
    let mut m = 0u32;
    for k in 0..lay.n_buttons as u16 {
        if bits_at(body, lay.buttons_off + k, 1) != 0 {
            m |= 1 << k;
        }
    }
    // The hat doubles as d-pad buttons 12..15 when the pad has 12 or fewer buttons.
    if lay.n_buttons <= 12 && st.hat >= 0 {
        let h = st.hat;
        if matches!(h, 7 | 0 | 1) {
            m |= 1 << 12;
        }
        if matches!(h, 3 | 4 | 5) {
            m |= 1 << 13;
        }
        if matches!(h, 5 | 6 | 7) {
            m |= 1 << 14;
        }
        if matches!(h, 1 | 2 | 3) {
            m |= 1 << 15;
        }
    }
    st.buttons = m;
    true
}

// ── Host -> pad packets (interrupt OUT, or a Bluetooth HID output report) ───

/// One packet for the pad. `reply` is the first byte of the input report that
/// answers it (the Switch handshake waits for it), 0 = none. `seq` says where
/// the sender patches in its running counter: GIP sequence byte 2, the Switch
/// subcommand counter byte 1.
#[derive(Clone, Copy)]
pub struct OutPkt {
    pub buf: [u8; 64],
    pub len: u8,
    pub reply: u8,
    pub seq: Seq,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Seq {
    None,
    /// GIP: byte 2 = a counter that wraps 1..=255.
    Gip,
    /// Switch: byte 1 = a counter 0..15.
    Switch,
}

impl OutPkt {
    pub const EMPTY: OutPkt = OutPkt { buf: [0; 64], len: 0, reply: 0, seq: Seq::None };

    pub fn new(bytes: &[u8], reply: u8, seq: Seq) -> OutPkt {
        let mut p = OutPkt::EMPTY;
        let n = bytes.len().min(64);
        p.buf[..n].copy_from_slice(&bytes[..n]);
        p.len = n as u8;
        p.reply = reply;
        p.seq = seq;
        p
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }
}

/// Player lightbar colours for the Sony pads (player 1 blue, 2 red, 3 green, 4 pink).
fn sony_colour(player: u8) -> [u8; 3] {
    match player % 4 {
        0 => [0x00, 0x00, 0x40],
        1 => [0x40, 0x00, 0x00],
        2 => [0x00, 0x40, 0x00],
        _ => [0x20, 0x00, 0x20],
    }
}

/// DS4 USB output report 0x05 (32 bytes): rumble (right = weak, left = strong) + lightbar.
fn ds4_out(weak: u8, strong: u8, player: u8) -> OutPkt {
    let c = sony_colour(player);
    let mut b = [0u8; 32];
    b[0] = 0x05;
    b[1] = 0x07; // motors + lightbar + flash fields valid
    b[2] = 0x04;
    b[4] = weak;
    b[5] = strong;
    b[6] = c[0];
    b[7] = c[1];
    b[8] = c[2];
    OutPkt::new(&b, 0, Seq::None)
}

/// DualSense USB output report 0x02 (48 bytes): rumble (+ at init the lightbar and player dots).
fn dualsense_out(weak: u8, strong: u8, player: u8, init: bool) -> OutPkt {
    let mut b = [0u8; 48];
    b[0] = 0x02;
    let c = &mut b[1..];
    c[0] = 0x03; // compatible vibration + haptics select
    c[2] = weak; // right motor
    c[3] = strong; // left motor
    if init {
        let col = sony_colour(player);
        c[1] = 0x04 | 0x10; // lightbar + player indicator
        c[38] = 0x02; // lightbar setup valid
        c[41] = 0x02; // lightbar setup: light out (hand the bar over to us)
        c[43] = [0x04, 0x0A, 0x15, 0x1B][player as usize % 4];
        c[44] = col[0];
        c[45] = col[1];
        c[46] = col[2];
    }
    OutPkt::new(&b, 0, Seq::None)
}

/// A Switch Pro subcommand (output report 0x01): neutral rumble data + `cmd` `arg`.
fn switch_sub(cmd: u8, args: &[u8]) -> OutPkt {
    let mut b = [0u8; 16];
    b[0] = 0x01;
    b[2..10].copy_from_slice(&[0x00, 0x01, 0x40, 0x40, 0x00, 0x01, 0x40, 0x40]);
    b[10] = cmd;
    let n = args.len().min(5);
    b[11..11 + n].copy_from_slice(&args[..n]);
    OutPkt::new(&b[..11 + n], 0x21, Seq::Switch)
}

/// What a pad needs once it is attached (`player` 0-based: its LED / colour).
/// The Xbox One pad stays silent until it gets the power-on packet; the
/// Switch Pro needs its USB handshake before it sends full reports.
pub fn init_packets(lay: &Layout, vid: u16, pid: u16, player: u8, mut push: impl FnMut(OutPkt)) {
    let p = player % 4;
    match lay.kind {
        PadKind::Xbox360 => push(OutPkt::new(&[0x01, 0x03, 0x06 + p], 0, Seq::None)),
        PadKind::Xbox360Wireless => push(OutPkt::new(&[0x00, 0x00, 0x08, 0x40 + 0x06 + p, 0, 0, 0, 0, 0, 0, 0, 0], 0, Seq::None)),
        PadKind::XboxOne => {
            // GIP power on (every pad), then the per-model extras Linux xpad sends.
            push(OutPkt::new(&[0x05, 0x20, 0x00, 0x01, 0x00], 0, Seq::Gip));
            if vid == 0x045E && matches!(pid, 0x02EA | 0x0B00) {
                push(OutPkt::new(&[0x05, 0x20, 0x00, 0x0F, 0x06], 0, Seq::Gip));
            }
            if vid == 0x045E && pid == 0x0B00 {
                push(OutPkt::new(&[0x4D, 0x10, 0x01, 0x02, 0x07, 0x00], 0, Seq::None));
            }
            if vid == 0x0E6F || (vid == 0x20D6 && pid == 0xA01A) {
                // PDP: LED on + "authentication done"
                push(OutPkt::new(&[0x0A, 0x20, 0x00, 0x03, 0x00, 0x01, 0x14], 0, Seq::Gip));
                push(OutPkt::new(&[0x06, 0x20, 0x00, 0x02, 0x01, 0x00], 0, Seq::Gip));
            }
        },
        PadKind::DualShock4 => push(ds4_out(0, 0, p)),
        PadKind::DualSense => push(dualsense_out(0, 0, p, true)),
        PadKind::SwitchPro => {
            // USB: handshake, 3 Mbit, handshake, then "USB only, no timeout"
            push(OutPkt::new(&[0x80, 0x02], 0x81, Seq::None));
            push(OutPkt::new(&[0x80, 0x03], 0x81, Seq::None));
            push(OutPkt::new(&[0x80, 0x02], 0x81, Seq::None));
            push(OutPkt::new(&[0x80, 0x04], 0, Seq::None));
            push(switch_sub(0x03, &[0x30])); // input report mode: full (0x30)
            push(switch_sub(0x30, &[1 << p])); // player lights
        },
        PadKind::Generic | PadKind::XboxBle => {},
    }
}

/// Answer a pad report that asks for one: GIP packets with the ACK option
/// (the Guide button's 0x07 does; unanswered, the pad repeats it), and a GIP
/// announce (0x02), which means the pad (re)started and wants powering on.
pub fn reply_to(lay: &Layout, r: &[u8], mut push: impl FnMut(OutPkt)) {
    if lay.kind != PadKind::XboxOne || r.len() < 4 {
        return;
    }
    if r[1] & 0x10 != 0 {
        // ACK: 01 20 <their seq> 09 00 <cmd> 20 <len> 00 00 00 00 00
        push(OutPkt::new(&[0x01, 0x20, r[2], 0x09, 0x00, r[0], 0x20, r[3], 0, 0, 0, 0, 0], 0, Seq::None));
    }
    if r[0] == 0x02 {
        push(OutPkt::new(&[0x05, 0x20, 0x00, 0x01, 0x00], 0, Seq::Gip));
    }
}

/// Rumble: `strong` = the low-frequency (left) motor, `weak` = the
/// high-frequency (right) one, 0..1 each. None for pads without a simple
/// rumble: generic HID, and the Switch Pro -- its "HD rumble" takes encoded
/// frequency / amplitude pairs inside subcommand packets, not done (a
/// documented no-op, over USB and Bluetooth alike).
pub fn rumble_packet(lay: &Layout, strong: f32, weak: f32, player: u8) -> Option<OutPkt> {
    let s = (strong.clamp(0.0, 1.0) * 255.0) as u8;
    let w = (weak.clamp(0.0, 1.0) * 255.0) as u8;
    Some(match lay.kind {
        PadKind::Xbox360 => OutPkt::new(&[0x00, 0x08, 0x00, s, w, 0x00, 0x00, 0x00], 0, Seq::None),
        PadKind::Xbox360Wireless => OutPkt::new(&[0x00, 0x01, 0x0F, 0xC0, 0x00, s, w, 0, 0, 0, 0, 0], 0, Seq::None),
        PadKind::XboxOne => {
            // GIP rumble: all motors, magnitudes 0..100, "play once, long"
            let (s, w) = ((strong.clamp(0.0, 1.0) * 100.0) as u8, (weak.clamp(0.0, 1.0) * 100.0) as u8);
            OutPkt::new(&[0x09, 0x00, 0x00, 0x09, 0x00, 0x0F, 0x00, 0x00, s, w, 0xFF, 0x00, 0xFF], 0, Seq::Gip)
        },
        PadKind::DualShock4 => ds4_out(w, s, player % 4),
        PadKind::DualSense => dualsense_out(w, s, player % 4, false),
        // Xbox over Bluetooth: output report 3 -- enable (weak | strong), magnitudes 0..100 for the
        // left trigger, right trigger, strong and weak motors, duration (10 ms units, 0xFF = 2.55 s;
        // drivers::gamepad sends zeros to stop earlier), start delay, loop count. As Linux hid-microsoft.
        PadKind::XboxBle => {
            let (s, w) = ((strong.clamp(0.0, 1.0) * 100.0) as u8, (weak.clamp(0.0, 1.0) * 100.0) as u8);
            OutPkt::new(&[0x03, 0x03, 0x00, 0x00, s, w, 0xFF, 0x00, 0x00], 0, Seq::None)
        },
        PadKind::SwitchPro | PadKind::Generic => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.02
    }

    /// DragonRise "Generic USB Joystick" (0079:0006), the report descriptor
    /// most cheap PC / retro pads ship (linux-hardware.org / usbhid-dump).
    const DRAGONRISE: &[u8] = &[
        0x05, 0x01, 0x09, 0x04, 0xA1, 0x01, 0xA1, 0x02, 0x75, 0x08, 0x95, 0x05, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x35, 0x00, 0x46, 0xFF, 0x00, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x32, 0x09, 0x35, 0x81, 0x02, 0x75, 0x04, 0x95, 0x01, 0x25, 0x07, 0x46, 0x3B, 0x01, 0x65, 0x14, 0x09, 0x39, 0x81, 0x42, 0x65, 0x00, 0x75, 0x01, 0x95, 0x0C, 0x25, 0x01, 0x45, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x0C, 0x81, 0x02, 0x06, 0x00, 0xFF, 0x75, 0x01, 0x95, 0x08, 0x25, 0x01, 0x45, 0x01, 0x09, 0x01, 0x81, 0x02, 0xC0, 0xA1, 0x02, 0x75, 0x08, 0x95, 0x07, 0x46, 0xFF, 0x00, 0x26, 0xFF, 0x00, 0x09, 0x02, 0x91, 0x02, 0xC0, 0xC0,
    ];

    /// The input half of the DualShock 4 (054C:05C4) report descriptor, report 0x01.
    const DS4_INPUT: &[u8] = &[
        0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x04, 0x81, 0x02, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07, 0x35, 0x00, 0x46, 0x3B, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x0E, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0E, 0x81, 0x02, 0x06, 0x00, 0xFF, 0x09, 0x20, 0x75, 0x06, 0x95, 0x01, 0x15, 0x00, 0x25, 0x7F, 0x81, 0x02, 0x05, 0x01, 0x09, 0x33, 0x09, 0x34, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02, 0xC0,
    ];

    /// A keyboard's consumer-control interface is not a pad.
    const CONSUMER: &[u8] = &[0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x02, 0x19, 0x00, 0x2A, 0x3C, 0x02, 0x15, 0x00, 0x26, 0x3C, 0x02, 0x95, 0x01, 0x75, 0x10, 0x81, 0x00, 0xC0];

    #[test]
    fn generic_dragonrise() {
        let lay = parse_descriptor(DRAGONRISE).expect("a joystick");
        assert_eq!(lay.kind, PadKind::Generic);
        assert_eq!((lay.report_id, lay.n_axes, lay.n_buttons), (0, 5, 12));
        assert!(lay.hat.is_some());
        assert_eq!(lay.buttons_off, 44);
        // idle: sticks 0x7F / 0x80, hat 0x0F (null), no buttons
        let mut st = PadState::default();
        assert!(decode(&lay, &[0x7F, 0x7F, 0x80, 0x80, 0x80, 0x0F, 0x00, 0x00], &mut st));
        assert!(st.connected && st.hat == -1 && st.buttons == 0);
        assert!(near(st.axes[0], 0.0) && near(st.axes[1], 0.0));
        // stick fully up (Y = 0) + hat right (2) + button 1 (bit 4 of byte 5)
        assert!(decode(&lay, &[0x7F, 0x00, 0x80, 0x80, 0x80, 0x12, 0x00, 0x00], &mut st));
        assert!(near(st.axes[1], 1.0), "HID Y=0 is up: {}", st.axes[1]);
        assert_eq!(st.hat, 2);
        assert_eq!(st.buttons, 1 | (1 << B_RIGHT));
        // identify() leaves unknown vendors to the descriptor
        assert_eq!(identify(0x0079, 0x0006, 3, 0, 0, DRAGONRISE).unwrap().kind, PadKind::Generic);
        assert!(identify(0x046D, 0xC52B, 3, 0, 0, CONSUMER).is_none());
    }

    #[test]
    fn ds4() {
        // the descriptor alone already finds its fields (report 1, X Y Z Rz Rx Ry, hat, 14 buttons) ...
        let g = parse_descriptor(DS4_INPUT).unwrap();
        assert_eq!((g.report_id, g.n_axes, g.n_buttons, g.hat.is_some()), (1, 6, 14, true));
        // ... but identify() picks the fixed decoder, which puts Cross on A, not on button 1
        let lay = identify(0x054C, 0x09CC, 3, 0, 0, DS4_INPUT).unwrap();
        assert_eq!(lay.kind, PadKind::DualShock4);
        let mut st = PadState::default();
        // captured idle USB report (sticks at rest, hat 8 = released, counter in byte 7)
        let mut r = [0u8; 64];
        r[..10].copy_from_slice(&[0x01, 0x7F, 0x81, 0x82, 0x7D, 0x08, 0x00, 0xB4, 0x00, 0x00]);
        assert!(decode(&lay, &r, &mut st));
        assert_eq!((st.buttons, st.hat), (0, -1));
        assert!(st.axes[..4].iter().all(|a| a.abs() < 0.03));
        // Cross + Triangle + hat up-left, L1 + Options, PS, L2 half
        r[5] = 0x20 | 0x80 | 0x07;
        r[6] = 0x01 | 0x20;
        r[7] = 0x01 | 0xB4;
        r[8] = 0x80;
        r[2] = 0x00; // left stick up
        decode(&lay, &r, &mut st);
        let want = (1 << B_A) | (1 << B_Y) | (1 << B_LB) | (1 << B_START) | (1 << B_GUIDE) | (1 << B_UP) | (1 << B_LEFT);
        assert_eq!(st.buttons, want);
        assert_eq!(st.hat, 7);
        assert!(near(st.axes[1], 1.0) && near(st.axes[4], 0.5));
        // Bluetooth report 0x11: the same bytes two further in
        let mut bt = [0u8; 78];
        bt[0] = 0x11;
        bt[3..12].copy_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x48, 0x00, 0x00, 0x00, 0xFF]); // Circle, R2 full
        decode(&lay, &bt, &mut st);
        assert_eq!(st.buttons, 1 << B_B);
        assert!(near(st.axes[5], 1.0));
    }

    #[test]
    fn dualsense() {
        let lay = identify(0x054C, 0x0CE6, 3, 0, 0, &[]).unwrap();
        assert_eq!(lay.kind, PadKind::DualSense);
        let mut st = PadState::default();
        // USB report 0x01: LX LY RX RY L2 R2 counter, hat/face, shoulders, PS/pad/mute
        let mut r = [0u8; 64];
        r[..11].copy_from_slice(&[0x01, 0x80, 0x80, 0x80, 0x80, 0x00, 0x00, 0x2A, 0x08, 0x00, 0x00]);
        decode(&lay, &r, &mut st);
        assert_eq!((st.buttons, st.hat), (0, -1));
        r[8] = 0x10 | 0x04; // Square, hat down
        r[9] = 0x02 | 0x10; // R1, Create
        r[10] = 0x02; // touchpad click
        r[5] = 0xFF; // L2 full
        r[3] = 0xFF; // right stick right
        decode(&lay, &r, &mut st);
        assert_eq!(st.buttons, (1 << B_X) | (1 << B_RB) | (1 << B_BACK) | (1 << B_MISC) | (1 << B_DOWN));
        assert!(near(st.axes[4], 1.0) && near(st.axes[2], 1.0));
        // Bluetooth 0x31 (one more header byte)
        let mut bt = [0u8; 78];
        bt[0] = 0x31;
        bt[2..12].copy_from_slice(&[0x80, 0x80, 0x80, 0x80, 0, 0, 0, 0x28, 0x00, 0x01]); // Cross, PS
        decode(&lay, &bt, &mut st);
        assert_eq!(st.buttons, (1 << B_A) | (1 << B_GUIDE));
    }

    #[test]
    fn xbox360() {
        let lay = identify(0x045E, 0x028E, 0xFF, 0x5D, 0x01, &[]).unwrap();
        assert!(lay.xbox360 && lay.kind == PadKind::Xbox360);
        let mut st = PadState::default();
        // 20-byte report: 00 14, buttons, LT RT, LX LY RX RY (i16 LE, Y up = +)
        let mut r = [0u8; 20];
        r[1] = 0x14;
        r[2] = 0x01 | 0x08 | 0x10; // up + right, Start
        r[3] = 0x10 | 0x04; // A, Guide
        r[4] = 0xFF;
        r[8..10].copy_from_slice(&32767i16.to_le_bytes()); // LY full up
        r[10..12].copy_from_slice(&(-32768i16).to_le_bytes()); // RX full left
        assert!(decode(&lay, &r, &mut st));
        assert_eq!(st.buttons, (1 << B_A) | (1 << B_START) | (1 << B_GUIDE) | (1 << B_UP) | (1 << B_RIGHT));
        assert_eq!(st.hat, 1);
        assert!(near(st.axes[1], 1.0), "360 Y up must be positive like every other pad");
        assert!(near(st.axes[2], -1.0) && near(st.axes[4], 1.0));
        // the wireless receiver: presence, then the same report behind 4 bytes
        let w = identify(0x045E, 0x0719, 0xFF, 0x5D, 0x81, &[]).unwrap();
        let mut ws = PadState::default();
        decode(&w, &[0x08, 0x80], &mut ws);
        assert!(ws.connected);
        let mut wr = [0u8; 29];
        wr[1] = 0x01;
        wr[4..24].copy_from_slice(&r);
        decode(&w, &wr, &mut ws);
        assert_eq!(ws.buttons, st.buttons);
        decode(&w, &[0x08, 0x00], &mut ws);
        assert!(!ws.connected);
        // packets: LED for player 2, rumble
        let mut out = [OutPkt::EMPTY; 4];
        let mut n = 0;
        init_packets(&lay, 0x045E, 0x028E, 1, |p| {
            out[n] = p;
            n += 1;
        });
        assert_eq!((n, out[0].bytes()), (1, &[0x01u8, 0x03, 0x07][..]));
        assert_eq!(rumble_packet(&lay, 1.0, 0.0, 0).unwrap().bytes(), &[0x00, 0x08, 0x00, 0xFF, 0x00, 0, 0, 0]);
    }

    #[test]
    fn xbox_one_gip() {
        let lay = identify(0x045E, 0x0B12, 0xFF, 0x47, 0xD0, &[]).unwrap();
        assert_eq!(lay.kind, PadKind::XboxOne);
        let mut st = PadState::default();
        // announce / status packets are not input
        assert!(!decode(&lay, &[0x02, 0x20, 0x01, 0x1C, 0x7E, 0xED], &mut st));
        assert!(!decode(&lay, &[0x03, 0x20, 0x02, 0x04, 0x80, 0x00, 0x00, 0x00], &mut st));
        // input 0x20: A + Menu, d-pad left + LB, LT full, RT 0, LX right, LY down
        let mut r = [0u8; 18];
        r[..4].copy_from_slice(&[0x20, 0x00, 0x0D, 0x0E]);
        r[4] = 0x10 | 0x04;
        r[5] = 0x04 | 0x10;
        r[6..8].copy_from_slice(&1023u16.to_le_bytes());
        r[10..12].copy_from_slice(&32767i16.to_le_bytes());
        r[12..14].copy_from_slice(&(-32767i16).to_le_bytes());
        assert!(decode(&lay, &r, &mut st));
        assert_eq!(st.buttons, (1 << B_A) | (1 << B_START) | (1 << B_LEFT) | (1 << B_LB));
        assert!(near(st.axes[4], 1.0) && near(st.axes[0], 1.0) && near(st.axes[1], -1.0));
        // the Guide button lives in 0x07 packets and survives the next input packet
        assert!(decode(&lay, &[0x07, 0x30, 0x0E, 0x02, 0x01, 0x5B], &mut st));
        assert!(decode(&lay, &r, &mut st));
        assert!(st.buttons & (1 << B_GUIDE) != 0);
        decode(&lay, &[0x07, 0x20, 0x0F, 0x02, 0x00, 0x5B], &mut st);
        assert!(st.buttons & (1 << B_GUIDE) == 0);
        // init: power on (+ the S / Elite extras)
        let mut n = 0;
        init_packets(&lay, 0x045E, 0x0B12, 0, |p| {
            assert_eq!(p.bytes(), &[0x05, 0x20, 0x00, 0x01, 0x00]);
            assert_eq!(p.seq, Seq::Gip);
            n += 1;
        });
        assert_eq!(n, 1);
        let mut n = 0;
        init_packets(&lay, 0x045E, 0x02EA, 0, |_| n += 1);
        assert_eq!(n, 2);
        // an announce asks to be powered on again; a packet with the ACK flag gets an ack
        let mut got = 0;
        reply_to(&lay, &[0x02, 0x20, 0x01, 0x1C], |p| {
            assert_eq!(p.buf[0], 0x05);
            got += 1;
        });
        reply_to(&lay, &[0x07, 0x30, 0x0E, 0x02, 0x01, 0x5B], |p| {
            assert_eq!(&p.bytes()[..8], &[0x01, 0x20, 0x0E, 0x09, 0x00, 0x07, 0x20, 0x02]);
            got += 1;
        });
        assert_eq!(got, 2);
    }

    #[test]
    fn switch_pro() {
        let lay = identify(0x057E, 0x2009, 3, 0, 0, &[]).unwrap();
        assert_eq!(lay.kind, PadKind::SwitchPro);
        let mut st = PadState::default();
        // full report 0x30: timer, battery, right / shared / left buttons, 3+3 stick bytes (12-bit pairs)
        let pack = |x: u16, y: u16| [(x & 0xFF) as u8, ((x >> 8) as u8 & 0x0F) | (((y & 0x0F) as u8) << 4), (y >> 4) as u8];
        let mut r = [0u8; 64];
        r[0] = 0x30;
        r[2] = 0x91;
        r[3] = 0x04 | 0x80; // B (south), ZR
        r[4] = 0x02 | 0x10; // Plus, Home
        r[5] = 0x02 | 0x40; // up, L
        r[6..9].copy_from_slice(&pack(2048, 2048 + 1600)); // left stick full up
        r[9..12].copy_from_slice(&pack(2048 - 1600, 2048)); // right stick full left
        assert!(decode(&lay, &r, &mut st));
        assert_eq!(st.buttons, (1 << B_A) | (1 << B_START) | (1 << B_GUIDE) | (1 << B_LB) | (1 << B_UP));
        assert!(near(st.axes[1], 1.0) && near(st.axes[0], 0.0) && near(st.axes[2], -1.0) && near(st.axes[5], 1.0));
        // simple HID report 0x3F (before the handshake): A (east) + hat down-right
        let mut s = [0u8; 12];
        s[0] = 0x3F;
        s[1] = 0x02;
        s[3] = 3;
        s[4..6].copy_from_slice(&0x8000u16.to_le_bytes());
        s[6..8].copy_from_slice(&0xFFFFu16.to_le_bytes()); // Y max = down
        decode(&lay, &s, &mut st);
        assert_eq!(st.buttons, (1 << B_B) | (1 << B_DOWN) | (1 << B_RIGHT));
        assert!(near(st.axes[1], -1.0));
        // handshake replies decode to nothing
        assert!(!decode(&lay, &[0x81, 0x02], &mut st));
        assert!(rumble_packet(&lay, 1.0, 1.0, 0).is_none());
    }

    #[test]
    fn sony_out_reports() {
        let ds4 = rumble_packet(&Layout::DUALSHOCK4, 1.0, 0.5, 0).unwrap();
        assert_eq!((ds4.len, ds4.buf[0], ds4.buf[4], ds4.buf[5], ds4.buf[8]), (32, 0x05, 127, 255, 0x40));
        let ds = rumble_packet(&Layout::DUALSENSE, 1.0, 0.5, 0).unwrap();
        assert_eq!((ds.len, ds.buf[0], ds.buf[1], ds.buf[3], ds.buf[4]), (48, 0x02, 0x03, 127, 255));
    }

    #[test]
    fn hat_directions() {
        let all = [(true, false, false, false, 0), (true, false, false, true, 1), (false, false, false, true, 2), (false, true, false, true, 3), (false, true, false, false, 4), (false, true, true, false, 5), (false, false, true, false, 6), (true, false, true, false, 7), (false, false, false, false, -1), (true, true, false, false, -1)];
        for (u, d, l, r, h) in all {
            assert_eq!(hat_of(u, d, l, r), h);
        }
        assert_eq!(hat_bits(1), (1 << B_UP) | (1 << B_RIGHT));
        assert_eq!(hat_bits(-1), 0);
    }
}
