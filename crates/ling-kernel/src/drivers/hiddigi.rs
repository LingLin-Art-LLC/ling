//! USB pens, pen displays and absolute pointers, decoded from their HID report descriptor or, for the older
//! Wacom models, from Wacom's own packet formats.
//!
//! A Wacom Cintiq / Intuos (HID mode), a Huion / XP-Pen display or a Surface-style pen is a HID digitizer: an
//! application collection Digitizer (usage page 0x0D) / Pen (0x02) whose input report carries absolute X / Y,
//! Tip Pressure, Tip Switch, In Range, Barrel Switch (+ Secondary Barrel), Invert / Eraser and X / Y Tilt.
//! Newer Wacoms (Cintiq 16 / 22, Cintiq Pro 13 / 16 / 17 / 22 / 24 / 27 / 32, Intuos Pro PTH-x60...) also
//! describe a second, higher-resolution pen report on Wacom's vendor page 0xFF0D, whose usages map onto the
//! digitizer ones the way Linux's `wacom_equivalent_usage` does ([`equiv`]); both reports are kept
//! ([`Device`]), so the pen works whichever one the device sends. QEMU's `usb-tablet` (and VM "seamless
//! mouse" devices) is the simple case: a Generic Desktop Mouse with absolute X / Y and buttons.
//!
//! The Wacom mode switch (linux drivers/hid/wacom_sys.c, `wacom_query_tablet_data`): a HID-generic Wacom
//! gets SET_REPORT(feature) of its Wacom "data mode" report (usage 0xFF0D1002, value 2) and of a HID
//! "Input Mode" field (0x0D0052, value 2) when it has them ([`parse_full`] finds both); a model on the
//! product table ([`MODELS`], from linux drivers/hid/wacom_wac.c: 21UX / 21UX2 / 22HD / 24HD / 27QHD /
//! 13HD / 12WX / 20WSX / Companion / DTK / DTU / PL / Cintiq Partner, the Intuos 1-5 / Pro) gets feature
//! report 2 = 2, after which it sends Wacom's own packets, decoded here (`wacom_intuos_irq` & co.).
//!
//! [`Clicker`] turns pen samples into the system pointer's clicks: tip contact = left (with a pressure
//! threshold and hysteresis), lower barrel = right, upper barrel = middle (dragged with it held: scroll),
//! a short "tap freeze" so a click is not a tiny drag, and a double-tap snap so two quick taps a few pixels
//! apart land on the same pixel. `arch::xhci` moves the pointer, and queues pen samples ([`push`] /
//! [`take`]) for LingAnime's `art_pen_pump`; `drivers::pen` keeps the live state for `os_pen_*`.
//!
//! Only `core` + `alloc` + `libm` are used, so the host test crate (tools/hiddigi-test) builds this file too.

use alloc::vec::Vec;

/// Wacom's USB vendor id.
pub const WACOM_VID: u16 = 0x056a;
/// The system double-click time: two clicks closer than this are a double-click (kernel and Ling side).
pub const DBL_CLICK_MS: u64 = 500;

/// One input field: where it sits in the report and its logical / physical range.
#[derive(Clone, Copy, Default, Debug)]
pub struct Field {
    pub ok: bool,
    pub bit: u32,
    pub size: u32,
    pub lmin: i64,
    pub lmax: i64,
    /// physical range with the unit exponent applied (0..0 when the descriptor gives none)
    pub pmin: f32,
    pub pmax: f32,
}

impl Field {
    fn raw(&self, data: &[u8]) -> Option<i64> {
        if !self.ok || self.size == 0 || self.size > 32 || (self.bit + self.size) as usize > data.len() * 8 {
            return None;
        }
        let mut v: u64 = 0;
        for k in 0..self.size {
            let b = self.bit + k;
            if data[(b / 8) as usize] >> (b % 8) & 1 != 0 {
                v |= 1 << k;
            }
        }
        let mut v = v as i64;
        if self.lmin < 0 && v & (1 << (self.size - 1)) != 0 {
            v -= 1 << self.size;
        }
        Some(v)
    }
    /// 0..1 across the logical range
    fn norm(&self, data: &[u8]) -> Option<f32> {
        let v = self.raw(data)?;
        let span = (self.lmax - self.lmin) as f32;
        if span <= 0.0 {
            return None;
        }
        Some(((v - self.lmin) as f32 / span).clamp(0.0, 1.0))
    }
    fn on(&self, data: &[u8]) -> Option<bool> {
        self.raw(data).map(|v| v != 0)
    }
    /// a tilt in degrees: through the physical range when there is one, else the logical value as degrees
    /// (a logical range wider than +-90 is taken as +-60 degrees, the usual pen limit)
    fn degrees(&self, data: &[u8]) -> Option<f32> {
        let v = self.raw(data)? as f32;
        if self.pmax > self.pmin && self.lmax > self.lmin {
            let d = self.pmin + (v - self.lmin as f32) * (self.pmax - self.pmin) / (self.lmax - self.lmin) as f32;
            // a range of a few units is radians
            return Some(if self.pmax <= 3.2 { d.to_degrees() } else { d });
        }
        if self.lmax <= 90 {
            Some(v)
        } else {
            Some(v * 60.0 / self.lmax as f32)
        }
    }
}

/// Where a pen's / absolute pointer's values sit in one input report.
#[derive(Clone, Copy, Default, Debug)]
pub struct Layout {
    /// the report's id (its first byte) when the descriptor numbers reports
    pub report_id: Option<u8>,
    /// a digitizer (Digitizer usage page), not just an absolute mouse
    pub pen: bool,
    /// described on Wacom's vendor page (0xFF0D...): the high-resolution report of a HID-mode Wacom
    pub vendor: bool,
    pub x: Field,
    pub y: Field,
    pub pressure: Field,
    pub tip: Field,
    pub in_range: Field,
    /// Wacom's "In Proximity" (0xFF0D0036): set only while the pen is close enough to point
    pub sense: Field,
    pub barrel: Field,
    pub barrel2: Field,
    pub barrel3: Field,
    pub invert: Field,
    pub eraser: Field,
    pub tilt_x: Field,
    pub tilt_y: Field,
    pub twist: Field,
    /// hover distance: Wacom's 0xFF0D0132 (grows with height) or Generic Desktop Z (shrinks with height)
    pub distance: Field,
    pub z: Field,
    pub b1: Field,
    pub b2: Field,
    pub b3: Field,
    pub wheel: Field,
}

impl Layout {
    fn slot(&mut self, page: u32, usage: u32) -> Option<&mut Field> {
        Some(match (page, usage) {
            (0x01, 0x30) => &mut self.x,
            (0x01, 0x31) => &mut self.y,
            (0x01, 0x32) => &mut self.z,
            (0x01, 0x38) => &mut self.wheel,
            (0x0D, 0x30) => &mut self.pressure,
            (0x0D, 0x42) => &mut self.tip,
            (0x0D, 0x32) => &mut self.in_range,
            (0x0D, 0x44) => &mut self.barrel,
            (0x0D, 0x5A) => &mut self.barrel2,
            (0x0D, 0x3C) => &mut self.invert,
            (0x0D, 0x45) => &mut self.eraser,
            (0x0D, 0x3D) => &mut self.tilt_x,
            (0x0D, 0x3E) => &mut self.tilt_y,
            (0x0D, 0x41) => &mut self.twist,
            (0xFF0D, 0x36) => &mut self.sense,
            (0xFF0D, 0x5D) => &mut self.barrel3,
            (0xFF0D, 0x132) => &mut self.distance,
            (0x09, 1) => &mut self.b1,
            (0x09, 2) => &mut self.b2,
            (0x09, 3) => &mut self.b3,
            _ => return None,
        })
    }
}

/// A Wacom vendor usage as the digitizer usage it stands for (linux `wacom_equivalent_usage`): on page 0xFF0D
/// the high byte of the usage id is the "sub-page" (0 = Digitizer, 1 = Generic Desktop), so 0xFF0D0130 is X,
/// 0xFF0D0030 Tip Pressure; pad / button / info sub-pages and a few Wacom-only usages stay as they are.
pub fn equiv(full: u32) -> u32 {
    if full >> 16 != 0xFF0D {
        return full;
    }
    let id = full & 0xFFFF;
    let sub = id >> 8;
    let keep = matches!(sub, 0x04 | 0x09 | 0x0D | 0x10)
        || matches!(id, 0x36 | 0x5C | 0x5D | 0x77 | 0x132 | 0x136 | 0x137 | 0x138 | 0x139 | 0x1D0 | 0x220);
    if keep {
        return full;
    }
    let page = if sub == 0 { 0x0D } else { sub };
    (page << 16) | (id & 0xFF)
}

/// Feature reports that switch a HID-mode Wacom into its full pen mode, found in the report descriptor.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Modes {
    /// (report id, value): Wacom's data mode (0xFF0D1002 -> 2), or 0x0B on a G9 / G11 pen (-> 0)
    pub mode: Option<(u8, u8)>,
    /// (report id, bit, size): a HID "Input Mode" field (0x0D0052), set to 2
    pub input_mode: Option<(u8, u32, u32)>,
    /// (report id, bit): the display's sensor margins (0xFF0D0D30..0D33, 4 x 16 bits: left, top, right, bottom)
    pub offsets: Option<(u8, u32)>,
    /// feature report lengths in bytes (with the id byte): (id, len)
    pub feat_len: [(u8, u16); 32],
    pub n_feat: usize,
}

impl Modes {
    /// A feature report's length in bytes, its id byte included (`None` when the descriptor lacks it).
    pub fn len(&self, id: u8) -> Option<u16> {
        self.feat_len[..self.n_feat].iter().find(|f| f.0 == id).map(|f| f.1)
    }
}

const MAX_REPORTS: usize = 16;

/// Parse a HID report descriptor: `Some` for a pen / pen display (preferred) or an absolute pointer.
/// A relative mouse, keyboard, gamepad, touch screen or touchpad gives `None`.
pub fn parse(d: &[u8]) -> Option<Layout> {
    parse_full(d).0.first().copied()
}

/// Every pen report (a Wacom describes two) or else the one absolute pointer report, best first, plus the
/// feature reports that switch modes.
pub fn parse_full(d: &[u8]) -> (Vec<Layout>, Modes) {
    // candidate reports by id: (id, bits so far, layout, relative X/Y seen)
    let mut reps: [(u32, u32, Layout, bool); MAX_REPORTS] = [(0, 0, Layout::default(), false); MAX_REPORTS];
    let mut n_reps = 0usize;
    // feature reports by id: (id, bits so far)
    let mut feats: [(u32, u32); 32] = [(0, 0); 32];
    let mut n_feats = 0usize;
    let mut modes = Modes::default();
    // global items (+ a small push / pop stack)
    #[derive(Clone, Copy, Default)]
    struct G {
        page: u32,
        lmin: i64,
        lmax: i64,
        pmin: i64,
        pmax: i64,
        exp: i32,
        size: u32,
        count: u32,
        id: u32,
    }
    let mut g = G::default();
    let mut stack: [G; 4] = [G::default(); 4];
    let mut sp = 0usize;
    // local items (raw full usages: page << 16 | id)
    let mut usages: [u32; 32] = [0; 32];
    let mut n_usages = 0usize;
    let (mut umin, mut umax): (Option<u32>, Option<u32>) = (None, None);
    // the application collection we are in: its usage as written, and as the digitizer usage it stands for
    let mut app_raw: u32 = 0;
    let mut app: u32 = 0;
    let mut depth = 0u32;
    let mut i = 0usize;
    while i < d.len() {
        let b = d[i];
        i += 1;
        if b == 0xFE {
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
        let mut u: u32 = 0;
        for k in 0..size {
            u |= (d[i + k] as u32) << (8 * k);
        }
        let s: i64 = match size {
            1 => d[i] as i8 as i64,
            2 => (u as u16) as i16 as i64,
            4 => u as i32 as i64,
            _ => 0,
        };
        i += size;
        let (typ, tag) = ((b >> 2) & 3, b >> 4);
        match typ {
            // global
            1 => match tag {
                0 => g.page = u,
                1 => g.lmin = s,
                2 => g.lmax = s,
                3 => g.pmin = s,
                4 => g.pmax = s,
                5 => g.exp = if u & 0x8 != 0 { (u & 0xF) as i32 - 16 } else { (u & 0xF) as i32 },
                7 => g.size = u,
                8 => g.id = u,
                9 => g.count = u,
                10 => {
                    if sp < stack.len() {
                        stack[sp] = g;
                        sp += 1;
                    }
                },
                11 => {
                    if sp > 0 {
                        sp -= 1;
                        g = stack[sp];
                    }
                },
                _ => {},
            },
            // local
            2 => match tag {
                0 => {
                    if n_usages < usages.len() {
                        usages[n_usages] = if size == 4 { u } else { (g.page << 16) | u };
                        n_usages += 1;
                    }
                },
                1 => umin = Some(if size == 4 { u } else { (g.page << 16) | u }),
                2 => umax = Some(if size == 4 { u } else { (g.page << 16) | u }),
                _ => {},
            },
            // main
            0 => {
                // the usage of the k-th value of this main item, as written
                let usage_at = |k: u32| -> Option<u32> {
                    if n_usages > 0 {
                        Some(usages[(k as usize).min(n_usages - 1)])
                    } else if let (Some(a), Some(z)) = (umin, umax) {
                        if a + k > z { None } else { Some(a + k) }
                    } else {
                        None
                    }
                };
                match tag {
                    0xA => {
                        if u == 1 && depth == 0 {
                            app_raw = usages.first().copied().filter(|_| n_usages > 0).unwrap_or(0);
                            // a Wacom G9 / G11 pen collection is a pen too
                            app = match app_raw {
                                0xFF09_0002 | 0xFF11_0002 => 0x000D_0002,
                                a => equiv(a),
                            };
                        }
                        depth += 1;
                    },
                    0xC => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            app = 0;
                            app_raw = 0;
                        }
                    },
                    0xB => {
                        // Feature: the mode-switch reports and every feature report's length
                        let fi = match feats[..n_feats].iter().position(|f| f.0 == g.id) {
                            Some(k) => Some(k),
                            None if n_feats < feats.len() => {
                                feats[n_feats] = (g.id, 0);
                                n_feats += 1;
                                Some(n_feats - 1)
                            },
                            None => None,
                        };
                        if let Some(fi) = fi {
                            let bit0 = feats[fi].1;
                            for k in 0..g.count {
                                let Some(full) = usage_at(k) else { continue };
                                let e = equiv(full);
                                let id = g.id as u8;
                                if e == 0xFF0D_1002 && modes.mode.is_none() {
                                    modes.mode = Some((id, 2));
                                } else if e == 0x000D_0052 && modes.input_mode.is_none() {
                                    modes.input_mode = Some((id, bit0 + k * g.size, g.size));
                                } else if e == 0xFF0D_0D30 && modes.offsets.is_none() {
                                    modes.offsets = Some((id, bit0 + k * g.size));
                                } else if id == 0x0B && matches!(app_raw, 0xFF09_0002 | 0xFF11_0002) && e >> 16 == 0x0D && modes.mode.is_none() {
                                    modes.mode = Some((id, 0));
                                }
                            }
                            feats[fi].1 = bit0 + g.size * g.count;
                        }
                    },
                    0x8 => {
                        // Input
                        let ri = match reps[..n_reps].iter().position(|r| r.0 == g.id) {
                            Some(k) => k,
                            None if n_reps < MAX_REPORTS => {
                                reps[n_reps] = (g.id, 0, Layout::default(), false);
                                n_reps += 1;
                                n_reps - 1
                            },
                            None => {
                                n_usages = 0;
                                umin = None;
                                umax = None;
                                continue;
                            },
                        };
                        let bit0 = reps[ri].1;
                        let wanted = matches!(app, 0x000D_0001 | 0x000D_0002 | 0x0001_0001 | 0x0001_0002);
                        let constant = u & 1 != 0;
                        let variable = u & 2 != 0;
                        let relative = u & 4 != 0;
                        if wanted && !constant && variable {
                            let scale = pow10(g.exp);
                            let (lmin, lmax) = if g.lmin >= 0 && g.lmax < 0 { (g.lmin, g.lmax & 0xFFFF_FFFF) } else { (g.lmin, g.lmax) };
                            for k in 0..g.count {
                                let Some(full) = usage_at(k) else { continue };
                                let e = equiv(full);
                                let (page, usage) = (e >> 16, e & 0xFFFF);
                                let r = &mut reps[ri];
                                if (page, usage) == (1, 0x30) || (page, usage) == (1, 0x31) {
                                    if relative {
                                        r.3 = true;
                                        continue;
                                    }
                                }
                                if page == 0x0D && app >> 16 == 0x0D {
                                    r.2.pen = true;
                                    r.2.vendor |= app_raw >> 24 == 0xFF;
                                }
                                if let Some(f) = r.2.slot(page, usage) {
                                    if !f.ok {
                                        *f = Field {
                                            ok: true,
                                            bit: bit0 + k * g.size,
                                            size: g.size,
                                            lmin,
                                            lmax,
                                            pmin: g.pmin as f32 * scale,
                                            pmax: g.pmax as f32 * scale,
                                        };
                                    }
                                }
                            }
                        }
                        reps[ri].1 = bit0 + g.size * g.count;
                    },
                    _ => {},
                }
                n_usages = 0;
                umin = None;
                umax = None;
            },
            _ => {},
        }
    }
    for f in &feats[..n_feats] {
        if modes.n_feat < modes.feat_len.len() {
            let bytes = f.1.div_ceil(8) + (f.0 != 0) as u32;
            modes.feat_len[modes.n_feat] = (f.0 as u8, bytes.min(0xFFFF) as u16);
            modes.n_feat += 1;
        }
    }
    let mut out: Vec<Layout> = Vec::new();
    for r in &reps[..n_reps] {
        let mut l = r.2;
        if r.3 || !l.x.ok || !l.y.ok {
            continue;
        }
        l.report_id = if r.0 != 0 { Some(r.0 as u8) } else { None };
        out.push(l);
    }
    // pens first (the vendor report, finer, before the standard one); a plain absolute pointer only alone
    out.sort_by_key(|l| (!l.pen, !l.vendor));
    if out.first().is_some_and(|l| l.pen) {
        out.retain(|l| l.pen);
    } else {
        out.truncate(1);
    }
    (out, modes)
}

fn pow10(e: i32) -> f32 {
    let mut v = 1.0f32;
    for _ in 0..e.unsigned_abs().min(9) {
        v *= 10.0;
    }
    if e < 0 { 1.0 / v } else { v }
}

/// One decoded report.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct PenState {
    /// position, 0..1 across the device's range
    pub nx: f32,
    pub ny: f32,
    /// the pen is near enough to point (always true for an absolute mouse)
    pub in_range: bool,
    /// the tip or the eraser touches
    pub contact: bool,
    /// pressure 0..1 while in contact (0 otherwise)
    pub p: f32,
    /// pressure 0..1 whatever the tip switch says (for the click threshold), and whether the device has one
    pub pr: f32,
    pub has_pressure: bool,
    /// 0 upright .. 1 flat, azimuth radians clockwise from up, barrel rotation radians
    pub tilt: f32,
    pub az: f32,
    pub twist: f32,
    /// tilt along X / Y in degrees (+ = toward the right / the user), 0 when the pen has no tilt sensor
    pub tx: f32,
    pub ty: f32,
    /// hover height 0 (touching) .. 1 (edge of range), 0 when not reported
    pub dist: f32,
    pub eraser: bool,
    pub barrel: bool,
    pub barrel2: bool,
    /// the mouse buttons this means: bit 0 left (tip), bit 1 right (barrel), bit 2 middle (2nd barrel)
    pub buttons: u8,
    pub wheel: i32,
}

impl PenState {
    fn set_tilt(&mut self, tx: f32, ty: f32) {
        let (tx, ty) = (tx.clamp(-89.0, 89.0), ty.clamp(-89.0, 89.0));
        self.tx = tx;
        self.ty = ty;
        let (gx, gy) = (libm::tanf(tx.to_radians()), libm::tanf(ty.to_radians()));
        let lean = libm::atanf(libm::sqrtf(gx * gx + gy * gy));
        self.tilt = (lean / core::f32::consts::FRAC_PI_2).clamp(0.0, 1.0);
        self.az = if lean > 0.001 { libm::atan2f(gx, -gy) } else { 0.0 };
    }
    /// the Wacom driver's default: the lower side button right-clicks, the upper one middle-clicks (pans);
    /// otherwise the tip (or the eraser) is the left button
    fn pen_buttons(&mut self) {
        self.buttons = if self.barrel {
            2
        } else if self.barrel2 {
            4
        } else {
            self.contact as u8
        };
    }
}

/// Decode one input report; `None` for another report id (or a short report).
pub fn decode(l: &Layout, rep: &[u8]) -> Option<PenState> {
    let data = match l.report_id {
        Some(id) => {
            if rep.first() != Some(&id) {
                return None;
            }
            &rep[1..]
        },
        None => rep,
    };
    let nx = l.x.norm(data)?;
    let ny = l.y.norm(data)?;
    let mut s = PenState { nx, ny, in_range: l.in_range.on(data).unwrap_or(true) && l.sense.on(data).unwrap_or(true), ..Default::default() };
    let eraser_sw = l.eraser.on(data).unwrap_or(false);
    s.eraser = l.invert.on(data).unwrap_or(false) || eraser_sw;
    s.barrel = l.barrel.on(data).unwrap_or(false);
    s.barrel2 = l.barrel2.on(data).unwrap_or(false) || l.barrel3.on(data).unwrap_or(false);
    let b1 = l.b1.on(data).unwrap_or(false);
    let b2 = l.b2.on(data).unwrap_or(false);
    let b3 = l.b3.on(data).unwrap_or(false);
    s.contact = match l.tip.on(data) {
        Some(t) => t || eraser_sw,
        None if l.b1.ok => b1,
        None => l.pressure.raw(data).unwrap_or(0) > 0,
    };
    s.has_pressure = l.pressure.ok;
    s.pr = if l.pressure.ok { l.pressure.norm(data).unwrap_or(0.0) } else if s.contact { 1.0 } else { 0.0 };
    s.p = if s.contact { s.pr } else { 0.0 };
    if l.tilt_x.ok || l.tilt_y.ok {
        s.set_tilt(l.tilt_x.degrees(data).unwrap_or(0.0), l.tilt_y.degrees(data).unwrap_or(0.0));
    }
    if l.twist.ok {
        s.twist = l.twist.degrees(data).unwrap_or(0.0).to_radians();
    }
    if let Some(d) = l.distance.norm(data) {
        s.dist = d;
    } else if let Some(z) = l.z.norm(data) {
        s.dist = 1.0 - z;
    }
    if s.contact {
        s.dist = 0.0;
    }
    if l.pen {
        s.pen_buttons();
        // a pen pad's own buttons, if it reports any here
        s.buttons |= (b2 as u8) << 1 | (b3 as u8) << 2;
    } else {
        s.buttons = (b1 as u8) | (b2 as u8) << 1 | (b3 as u8) << 2;
    }
    if let Some(w) = l.wheel.raw(data) {
        s.wheel = w as i32;
    }
    Some(s)
}

// ── Wacom models (linux drivers/hid/wacom_wac.c) ────────────────────────────

/// How a Wacom model on the product table talks once switched (the `wacom_*_irq` it goes through in Linux).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Family {
    /// HID digitizer reports, decoded from the report descriptor (every model not on the table)
    #[default]
    Hid,
    /// Intuos (1st gen): 16-bit X / Y (`type < INTUOS3S`)
    Intuos1,
    /// Intuos 3 / 4 / 5 / Pro and the Cintiqs that share their packets (12WX, 20WSX, 21UX2, 22HD, 24HD,
    /// 27QHD, 13HD, Companion, DTK / DTH): 17-bit X / Y, pressure, tilt, distance
    Intuos,
    /// Cintiq 21UX (1st gen): Intuos packets, data only with the RDY bit
    Cintiq,
    /// PL-series pen displays and DTU-710 / DTU-1931 / DTF
    Pl,
    /// Cintiq Partner
    Ptu,
    /// DTU-2231 / DTU-1631
    Dtu,
    /// DTU-1031 / 1031X / 1141, DTK-1651
    Dtus,
}

/// One product from the table.
#[derive(Clone, Copy, Debug)]
pub struct Model {
    pub pid: u16,
    pub name: &'static str,
    pub x_max: u32,
    pub y_max: u32,
    pub p_max: u32,
    pub d_max: u32,
    pub family: Family,
    /// sensor margins outside the display: left, right, top, bottom (in X / Y units)
    pub off: [u32; 4],
}

const fn m(pid: u16, name: &'static str, x_max: u32, y_max: u32, p_max: u32, d_max: u32, family: Family, off: [u32; 4]) -> Model {
    Model { pid, name, x_max, y_max, p_max, d_max, family, off }
}

/// The product on the table, if `pid` (a Wacom product id) is one of the models with their own packets.
pub fn lookup(pid: u16) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.pid == pid)
}

/// A touch-only Wacom (the touch half of a 24HD touch, 27QHD touch, 22HDT, 13HD touch, DTH-2242...): its
/// finger reports are not a pointer here, so it is left idle.
pub fn touch_only(pid: u16) -> bool {
    TOUCH_ONLY.contains(&pid)
}

/// HID-mode Wacom pen displays by product id, for the log line (names as libwacom gives them).
pub fn hid_name(pid: u16) -> Option<&'static str> {
    Some(match pid {
        0x034f => "Wacom Cintiq Pro 13",
        0x0350 => "Wacom Cintiq Pro 16",
        0x0351 => "Wacom Cintiq Pro 24",
        0x0352 => "Wacom Cintiq Pro 32",
        0x037c => "Wacom Cintiq Pro 24 (pen)",
        0x0390 | 0x03ae => "Wacom Cintiq 16",
        0x0391 => "Wacom Cintiq 22",
        0x0396 => "Wacom Cintiq 16 (DTK-1660E)",
        0x037d => "Wacom Cintiq Pro 24 (DTH-2452)",
        0x0382 => "Wacom Cintiq Pro 24 (DTK-2451)",
        0x034d | 0x0398 => "Wacom MobileStudio Pro 13",
        0x034e | 0x0399 | 0x03aa => "Wacom MobileStudio Pro 16",
        0x03b2 => "Wacom Cintiq Pro 16 (DTH167)",
        0x03c0 => "Wacom Cintiq Pro 27",
        0x03c4 => "Wacom Cintiq Pro 17",
        0x03d0 => "Wacom Cintiq Pro 22",
        0x0357 => "Wacom Intuos Pro M (PTH-660)",
        0x0358 => "Wacom Intuos Pro L (PTH-860)",
        0x0392 => "Wacom Intuos Pro S (PTH-460)",
        _ => return None,
    })
}

/// The internal / external USB hubs of Wacom pen displays (usb.ids). The pen itself is a separate HID
/// device behind the hub, HID-generic (any product id with a pen in its report descriptor works).
/// 0x039e is not in usb.ids; it sits next to the MobileStudio Pro hubs 0x039c / 0x039d.
pub fn hub_name(pid: u16) -> Option<&'static str> {
    Some(match pid {
        0x0347 | 0x0348 => "MobileStudio Pro 16",
        0x038a | 0x038d | 0x038e | 0x038f => "Cintiq Pro 32",
        0x039c | 0x039d => "MobileStudio Pro",
        0x039e => "id not in usb.ids; Cintiq Pro / MobileStudio family",
        _ => return None,
    })
}

/// Wacom models with their own packet formats (linux drivers/hid/wacom_wac.c `wacom_features_0x*`: name, X / Y
/// max, pressure max, distance max, packet family, display margins). Every other Wacom is HID-generic.
#[rustfmt::skip]
pub static MODELS: &[Model] = &[
    m(0x0003, "Wacom Cintiq Partner", 20480, 15360, 511, 0, Family::Ptu, [0, 0, 0, 0]),
    m(0x0020, "Wacom Intuos 4x5", 12700, 10600, 1023, 31, Family::Intuos1, [0, 0, 0, 0]),
    m(0x0021, "Wacom Intuos 6x8", 20320, 16240, 1023, 31, Family::Intuos1, [0, 0, 0, 0]),
    m(0x0022, "Wacom Intuos 9x12", 30480, 24060, 1023, 31, Family::Intuos1, [0, 0, 0, 0]),
    m(0x0023, "Wacom Intuos 12x12", 30480, 31680, 1023, 31, Family::Intuos1, [0, 0, 0, 0]),
    m(0x0024, "Wacom Intuos 12x18", 45720, 31680, 1023, 31, Family::Intuos1, [0, 0, 0, 0]),
    m(0x0026, "Wacom Intuos5 touch S", 31496, 19685, 2047, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x0027, "Wacom Intuos5 touch M", 44704, 27940, 2047, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x0028, "Wacom Intuos5 touch L", 65024, 40640, 2047, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x0029, "Wacom Intuos5 S", 31496, 19685, 2047, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x002a, "Wacom Intuos5 M", 44704, 27940, 2047, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x0030, "Wacom PL400", 5408, 4056, 255, 0, Family::Pl, [0, 0, 0, 0]),
    m(0x0031, "Wacom PL500", 6144, 4608, 255, 0, Family::Pl, [0, 0, 0, 0]),
    m(0x0032, "Wacom PL600", 6126, 4604, 255, 0, Family::Pl, [0, 0, 0, 0]),
    m(0x0033, "Wacom PL600SX", 6260, 5016, 255, 0, Family::Pl, [0, 0, 0, 0]),
    m(0x0034, "Wacom PL550", 6144, 4608, 511, 0, Family::Pl, [0, 0, 0, 0]),
    m(0x0035, "Wacom PL800", 7220, 5780, 511, 0, Family::Pl, [0, 0, 0, 0]),
    m(0x0037, "Wacom PL700", 6758, 5406, 511, 0, Family::Pl, [0, 0, 0, 0]),
    m(0x0038, "Wacom PL510", 6282, 4762, 511, 0, Family::Pl, [0, 0, 0, 0]),
    m(0x0039, "Wacom DTU710", 34080, 27660, 511, 0, Family::Pl, [0, 0, 0, 0]),
    m(0x003f, "Wacom Cintiq 21UX", 87200, 65600, 1023, 63, Family::Cintiq, [0, 0, 0, 0]),
    m(0x0041, "Wacom Intuos2 4x5", 12700, 10600, 1023, 31, Family::Intuos1, [0, 0, 0, 0]),
    m(0x0042, "Wacom Intuos2 6x8", 20320, 16240, 1023, 31, Family::Intuos1, [0, 0, 0, 0]),
    m(0x0043, "Wacom Intuos2 9x12", 30480, 24060, 1023, 31, Family::Intuos1, [0, 0, 0, 0]),
    m(0x0044, "Wacom Intuos2 12x12", 30480, 31680, 1023, 31, Family::Intuos1, [0, 0, 0, 0]),
    m(0x0045, "Wacom Intuos2 12x18", 45720, 31680, 1023, 31, Family::Intuos1, [0, 0, 0, 0]),
    m(0x0047, "Wacom Intuos2 6x8", 20320, 16240, 1023, 31, Family::Intuos1, [0, 0, 0, 0]),
    m(0x0057, "Wacom DTK2241", 95840, 54260, 2047, 63, Family::Intuos, [400, 400, 400, 400]),
    m(0x0059, "Wacom DTH2242", 95840, 54260, 2047, 63, Family::Intuos, [400, 400, 400, 400]),
    m(0x005b, "Wacom Cintiq 22HDT", 95840, 54260, 2047, 63, Family::Intuos, [400, 400, 400, 400]),
    m(0x00b0, "Wacom Intuos3 4x5", 25400, 20320, 1023, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x00b1, "Wacom Intuos3 6x8", 40640, 30480, 1023, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x00b2, "Wacom Intuos3 9x12", 60960, 45720, 1023, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x00b3, "Wacom Intuos3 12x12", 60960, 60960, 1023, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x00b4, "Wacom Intuos3 12x19", 97536, 60960, 1023, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x00b5, "Wacom Intuos3 6x11", 54204, 31750, 1023, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x00b7, "Wacom Intuos3 4x6", 31496, 19685, 1023, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x00b8, "Wacom Intuos4 4x6", 31496, 19685, 2047, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x00b9, "Wacom Intuos4 6x9", 44704, 27940, 2047, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x00ba, "Wacom Intuos4 8x13", 65024, 40640, 2047, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x00bb, "Wacom Intuos4 12x19", 97536, 60960, 2047, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x00bc, "Wacom Intuos4 WL", 40640, 25400, 2047, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x00c0, "Wacom DTF720", 6858, 5506, 511, 0, Family::Pl, [0, 0, 0, 0]),
    m(0x00c2, "Wacom DTF720a", 6858, 5506, 511, 0, Family::Pl, [0, 0, 0, 0]),
    m(0x00c4, "Wacom DTF521", 6282, 4762, 511, 0, Family::Pl, [0, 0, 0, 0]),
    m(0x00c5, "Wacom Cintiq 20WSX", 86680, 54180, 1023, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x00c6, "Wacom Cintiq 12WX", 53020, 33440, 1023, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x00c7, "Wacom DTU1931", 37832, 30305, 511, 0, Family::Pl, [0, 0, 0, 0]),
    m(0x00cc, "Wacom Cintiq 21UX2", 87200, 65600, 2047, 63, Family::Intuos, [400, 400, 400, 400]),
    m(0x00ce, "Wacom DTU2231", 47864, 27011, 511, 0, Family::Dtu, [0, 0, 0, 0]),
    m(0x00f0, "Wacom DTU1631", 34623, 19553, 511, 0, Family::Dtu, [0, 0, 0, 0]),
    m(0x00f4, "Wacom Cintiq 24HD", 104480, 65600, 2047, 63, Family::Intuos, [400, 400, 400, 400]),
    m(0x00f8, "Wacom Cintiq 24HD touch", 104480, 65600, 2047, 63, Family::Intuos, [400, 400, 400, 400]),
    m(0x00fa, "Wacom Cintiq 22HD", 95840, 54260, 2047, 63, Family::Intuos, [400, 400, 400, 400]),
    m(0x00fb, "Wacom DTU1031", 22096, 13960, 511, 0, Family::Dtus, [200, 200, 200, 200]),
    m(0x0304, "Wacom Cintiq 13HD", 59552, 33848, 1023, 63, Family::Intuos, [400, 400, 400, 400]),
    m(0x0307, "Wacom ISDv5 307", 59552, 33848, 2047, 63, Family::Intuos, [400, 400, 400, 400]),
    m(0x030a, "Wacom ISDv5 30A", 59552, 33848, 2047, 63, Family::Intuos, [400, 400, 400, 400]),
    m(0x0314, "Wacom Intuos Pro S", 31496, 19685, 2047, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x0315, "Wacom Intuos Pro M", 44704, 27940, 2047, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x0317, "Wacom Intuos Pro L", 65024, 40640, 2047, 63, Family::Intuos, [0, 0, 0, 0]),
    m(0x0325, "Wacom ISDv5 325", 59552, 33848, 2047, 63, Family::Intuos, [400, 400, 400, 400]),
    m(0x032a, "Wacom Cintiq 27QHD", 120140, 67920, 2047, 63, Family::Intuos, [400, 400, 400, 400]),
    m(0x032b, "Wacom Cintiq 27QHD touch", 120140, 67920, 2047, 63, Family::Intuos, [400, 400, 400, 400]),
    m(0x032f, "Wacom DTU1031X", 22672, 12928, 511, 0, Family::Dtus, [200, 200, 200, 200]),
    m(0x0333, "Wacom Cintiq 13HD touch", 59552, 33848, 2047, 63, Family::Intuos, [400, 400, 400, 400]),
    m(0x0336, "Wacom DTU1141", 23672, 13403, 1023, 0, Family::Dtus, [200, 200, 200, 200]),
    m(0x0343, "Wacom DTK1651", 34816, 19759, 1023, 0, Family::Dtus, [200, 200, 200, 200]),
];
/// Touch-only Wacom product ids (the touch halves of the pen displays above): left idle.
pub static TOUCH_ONLY: &[u16] = &[0x005d, 0x005e, 0x00f6, 0x0309, 0x030c, 0x0326, 0x032c, 0x0335];

// ── one plugged-in pen device ───────────────────────────────────────────────

/// How the device ended up after set-up (for the log line and `pen`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// HID reports as the device sends them after reset (nothing to switch, or not a Wacom)
    #[default]
    Hid,
    /// a HID-mode Wacom switched to its full pen mode (data mode / input mode feature reports)
    HidSwitched,
    /// a table model switched to Wacom's own packets (feature report 2 = 2)
    Wacom,
    /// the switch request failed: decoding whatever comes
    SwitchFailed,
}

/// A pen / absolute pointer interface: its report layouts (or its Wacom model) and the decoder's state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Device {
    pub vid: u16,
    pub pid: u16,
    /// HID report layouts, best first (`n` of them)
    pub reps: [Layout; 3],
    pub n: usize,
    /// a Wacom model with its own packets (`family` != Hid)
    pub model: Option<&'static Model>,
    pub family: Family,
    pub mode: Mode,
    /// Intuos tool id from the last "in proximity" packet (0 = not known yet)
    pub tool: u32,
    /// the PL / DTU / DTUS tool in proximity is the eraser
    pub rubber: bool,
    /// the last position (packets without one keep it)
    pub last: (f32, f32),
}

impl Device {
    /// A HID device from its report layouts (best first).
    pub fn hid(vid: u16, pid: u16, layouts: &[Layout]) -> Device {
        let mut d = Device { vid, pid, ..Default::default() };
        for l in layouts.iter().take(3) {
            d.reps[d.n] = *l;
            d.n += 1;
        }
        d
    }
    /// A table model speaking Wacom's own packets.
    pub fn wacom(m: &'static Model) -> Device {
        Device { vid: WACOM_VID, pid: m.pid, model: Some(m), family: m.family, mode: Mode::Wacom, ..Default::default() }
    }
    /// A pen (pressure, eraser, side buttons), not a plain absolute mouse.
    pub fn pen(&self) -> bool {
        self.family != Family::Hid || self.reps[..self.n].iter().any(|l| l.pen)
    }
    /// The sensor margins (left, top, right, bottom) a Wacom display reports: its vendor-page reports' X / Y
    /// range shrinks to the visible screen.
    pub fn set_offsets(&mut self, left: u32, top: u32, right: u32, bottom: u32) {
        for l in self.reps[..self.n].iter_mut().filter(|l| l.vendor) {
            let (ox, oy) = ((left + right) as i64, (top + bottom) as i64);
            if l.x.lmax - l.x.lmin > 4 * ox && l.y.lmax - l.y.lmin > 4 * oy {
                l.x.lmin += left as i64;
                l.x.lmax -= right as i64;
                l.y.lmin += top as i64;
                l.y.lmax -= bottom as i64;
            }
        }
    }
    /// The pressure range's top (raw units) and whether the pen reports tilt.
    pub fn pressure_max(&self) -> u32 {
        match self.model {
            Some(m) => m.p_max,
            None => self.reps[..self.n].iter().find(|l| l.pressure.ok).map(|l| (l.pressure.lmax - l.pressure.lmin).max(0) as u32).unwrap_or(0),
        }
    }
    pub fn has_tilt(&self) -> bool {
        match self.model {
            Some(m) => matches!(m.family, Family::Intuos | Family::Intuos1 | Family::Cintiq),
            None => self.reps[..self.n].iter().any(|l| l.tilt_x.ok || l.tilt_y.ok),
        }
    }
    /// The product's name.
    pub fn name(&self) -> &'static str {
        if let Some(m) = self.model {
            return m.name;
        }
        if self.vid == WACOM_VID {
            return hid_name(self.pid).unwrap_or("Wacom (HID mode)");
        }
        if self.pen() { "USB pen / pen display" } else { "absolute pointer (tablet)" }
    }
    /// One line for the log / a photo of the screen: model, ids, mode, pressure range, tilt.
    pub fn describe(&self) -> alloc::string::String {
        let mode = match self.mode {
            Mode::Hid => "HID reports",
            Mode::HidSwitched => "HID, switched to full pen mode",
            Mode::Wacom => "Wacom packets (mode switched)",
            Mode::SwitchFailed => "mode switch FAILED, HID reports",
        };
        let ids: alloc::vec::Vec<alloc::string::String> = self.reps[..self.n].iter().map(|l| alloc::format!("{}", l.report_id.unwrap_or(0))).collect();
        let reps = if self.family == Family::Hid { alloc::format!(" reports [{}]", ids.join(",")) } else { alloc::string::String::new() };
        alloc::format!(
            "{} {:04x}:{:04x}: {}{}, pressure 0..{}, tilt {}",
            self.name(),
            self.vid,
            self.pid,
            mode,
            reps,
            self.pressure_max(),
            if self.has_tilt() { "yes" } else { "no" }
        )
    }
}

/// Decode one input report of `d` (any of its layouts, or its Wacom packet format).
pub fn decode_dev(d: &mut Device, rep: &[u8]) -> Option<PenState> {
    let s = match d.family {
        Family::Hid => d.reps[..d.n].iter().find_map(|l| decode(l, rep)),
        Family::Intuos | Family::Intuos1 | Family::Cintiq => wac_intuos(d, rep),
        Family::Pl => wac_pl(d, rep),
        Family::Ptu => wac_ptu(d, rep),
        Family::Dtu => wac_dtu(d, rep),
        Family::Dtus => wac_dtus(d, rep),
    }?;
    if s.in_range || s.contact {
        d.last = (s.nx, s.ny);
    }
    Some(s)
}

/// raw X / Y into 0..1 across the display (the margins outside it cut off)
fn wac_norm(m: &Model, x: u32, y: u32) -> (f32, f32) {
    let (l, r, t, b) = (m.off[0] as f32, m.off[1] as f32, m.off[2] as f32, m.off[3] as f32);
    let nx = (x as f32 - l) / (m.x_max as f32 - l - r).max(1.0);
    let ny = (y as f32 - t) / (m.y_max as f32 - t - b).max(1.0);
    (nx.clamp(0.0, 1.0), ny.clamp(0.0, 1.0))
}

fn be16(r: &[u8], i: usize) -> u32 {
    (r[i] as u32) << 8 | r[i + 1] as u32
}
fn le16(r: &[u8], i: usize) -> u32 {
    r[i] as u32 | (r[i + 1] as u32) << 8
}

/// An Intuos tool id that is the eraser end (linux `wacom_intuos_get_tool_type`: bit 3, apart from the
/// pencil / brush / mouse / lens / airbrush ids listed there).
pub fn intuos_eraser(tool: u32) -> bool {
    !matches!(
        tool,
        0x812 | 0x801 | 0x12802 | 0x012 | 0x832 | 0x032 | 0x007 | 0x09c | 0x094 | 0x017 | 0x806 | 0x096 | 0x097 | 0x006 | 0xd12 | 0x912 | 0x112 | 0x913 | 0x902 | 0x10902
    ) && tool & 0x0008 != 0
}

/// Intuos-protocol packets (`wacom_intuos_irq`: `wacom_intuos_inout` + `wacom_intuos_general`), 10 bytes:
/// report id 2 / 16, then a status byte, X / Y big-endian (+ a low bit each in byte 9), pressure, tilt.
fn wac_intuos(d: &mut Device, r: &[u8]) -> Option<PenState> {
    let m = d.model?;
    if r.len() < 10 || !matches!(r[0], 2 | 5 | 6 | 16) {
        return None; // pad (12 / 17 / 3) and status reports
    }
    let b1 = r[1];
    let (lx, ly) = d.last;
    if b1 & 0xfc == 0xc0 {
        // entering proximity: which tool (pen, eraser end, airbrush...)
        d.tool = (r[2] as u32) << 4 | (r[3] as u32) >> 4 | ((r[7] & 0x0f) as u32) << 16 | ((r[8] & 0xf0) as u32) << 8;
        return Some(PenState { nx: lx, ny: ly, in_range: true, has_pressure: true, eraser: intuos_eraser(d.tool), dist: 1.0, ..Default::default() });
    }
    if b1 & 0xfe == 0x20 {
        // in range, too high to point
        return Some(PenState { nx: lx, ny: ly, in_range: true, has_pressure: true, eraser: intuos_eraser(d.tool), dist: 1.0, ..Default::default() });
    }
    if b1 & 0xfe == 0x80 {
        // out of proximity
        d.tool = 0;
        return Some(PenState { nx: lx, ny: ly, has_pressure: true, ..Default::default() });
    }
    if !matches!(r[0], 2 | 16) {
        return None;
    }
    // the 1st-gen Cintiq sends nothing valid without the RDY bit
    if m.family == Family::Cintiq && b1 & 0x40 == 0 {
        return None;
    }
    let mut x = be16(r, 2) << 1 | ((r[9] >> 1) & 1) as u32;
    let mut y = be16(r, 4) << 1 | (r[9] & 1) as u32;
    let mut dist = (r[9] >> 2) as u32;
    if m.family == Family::Intuos1 {
        x >>= 1;
        y >>= 1;
        dist >>= 1;
    }
    let (nx, ny) = wac_norm(m, x, y);
    let mut s = PenState { nx, ny, in_range: true, has_pressure: true, eraser: intuos_eraser(d.tool), ..Default::default() };
    if m.d_max > 0 {
        s.dist = (dist as f32 / m.d_max as f32).clamp(0.0, 1.0);
    }
    let tilt = |r: &[u8]| ((((r[7] as i32) << 1) & 0x7e) | (r[8] as i32 >> 7), (r[8] & 0x7f) as i32);
    match (b1 >> 1) & 0x0f {
        0..=3 => {
            // a pen packet
            let mut t = (r[6] as u32) << 3 | ((r[7] & 0xC0) >> 5) as u32 | (b1 & 1) as u32;
            if m.p_max < 2047 {
                t >>= 1;
            }
            s.pr = (t as f32 / m.p_max.max(1) as f32).clamp(0.0, 1.0);
            s.contact = t > 10;
            s.p = if s.contact { s.pr } else { 0.0 };
            let (tx, ty) = tilt(r);
            // -64..63 units of about a degree (Linux: resolution 57 per radian)
            s.set_tilt((tx - 64) as f32 * 1.005, (ty - 64) as f32 * 1.005);
            s.barrel = b1 & 2 != 0;
            s.barrel2 = b1 & 4 != 0;
            if s.contact {
                s.dist = 0.0;
            }
        },
        0x0a => {
            // airbrush second packet: tilt only
            let (tx, ty) = tilt(r);
            s.set_tilt((tx - 64) as f32 * 1.005, (ty - 64) as f32 * 1.005);
        },
        _ => {}, // a puck / lens cursor: position only
    }
    s.pen_buttons();
    Some(s)
}

/// PL-series pen displays (`wacom_pl_irq`): 8 bytes, report id 2, 7-bit split X / Y, signed pressure.
fn wac_pl(d: &mut Device, r: &[u8]) -> Option<PenState> {
    let m = d.model?;
    if r.len() < 8 || r[0] != 2 {
        return None;
    }
    let prox = r[1] & 0x40 != 0;
    if d.tool == 0 {
        d.rubber = r[0] & 0x10 != 0 || r[4] & 0x20 != 0;
        d.tool = 1;
    }
    // the eraser always has "stylus 2" set; without it this was the pen after all
    if d.rubber && r[4] & 0x20 == 0 {
        d.rubber = false;
    }
    let (lx, ly) = d.last;
    let mut s = PenState { nx: lx, ny: ly, in_range: prox, has_pressure: true, eraser: d.rubber, ..Default::default() };
    if prox {
        let mut p = ((((r[7] as u32) << 1) | ((r[4] >> 2) & 1) as u32) as u8 as i8) as i32;
        if m.p_max > 255 {
            p = (p << 1) | ((r[4] >> 6) & 1) as i32;
        }
        p += (m.p_max as i32 + 1) / 2;
        let x = r[3] as u32 | (r[2] as u32) << 7 | ((r[1] & 0x03) as u32) << 14;
        let y = r[6] as u32 | (r[5] as u32) << 7 | ((r[4] & 0x03) as u32) << 14;
        (s.nx, s.ny) = wac_norm(m, x, y);
        s.pr = (p.max(0) as f32 / m.p_max.max(1) as f32).clamp(0.0, 1.0);
        s.contact = r[4] & 0x08 != 0;
        s.p = if s.contact { s.pr } else { 0.0 };
        s.barrel = r[4] & 0x10 != 0;
        s.barrel2 = !d.rubber && r[4] & 0x20 != 0;
    } else {
        d.tool = 0;
    }
    s.pen_buttons();
    Some(s)
}

/// Cintiq Partner (`wacom_ptu_irq`): report id 2, little-endian X / Y / pressure.
fn wac_ptu(d: &mut Device, r: &[u8]) -> Option<PenState> {
    let m = d.model?;
    if r.len() < 8 || r[0] != 2 {
        return None;
    }
    let rubber = r[1] & 0x04 != 0;
    let (nx, ny) = wac_norm(m, le16(r, 2), le16(r, 4));
    let mut s = PenState { nx, ny, in_range: r[1] & 0x20 != 0, has_pressure: true, eraser: rubber, ..Default::default() };
    s.contact = if rubber { r[1] & 0x08 != 0 } else { r[1] & 0x01 != 0 };
    s.pr = (le16(r, 6) as f32 / m.p_max.max(1) as f32).clamp(0.0, 1.0);
    s.p = if s.contact { s.pr } else { 0.0 };
    s.barrel = r[1] & 0x02 != 0;
    s.barrel2 = r[1] & 0x10 != 0;
    s.pen_buttons();
    Some(s)
}

/// DTU-2231 / 1631 (`wacom_dtu_irq`).
fn wac_dtu(d: &mut Device, r: &[u8]) -> Option<PenState> {
    let m = d.model?;
    if r.len() < 8 {
        return None;
    }
    let prox = r[1] & 0x20 != 0;
    if prox {
        d.rubber = r[1] & 0x0c != 0;
    }
    let (nx, ny) = wac_norm(m, le16(r, 2), le16(r, 4));
    let mut s = PenState { nx, ny, in_range: prox, has_pressure: true, eraser: d.rubber, ..Default::default() };
    s.pr = ((((r[7] & 0x01) as u32) << 8 | r[6] as u32) as f32 / m.p_max.max(1) as f32).clamp(0.0, 1.0);
    s.contact = r[1] & 0x05 != 0;
    s.p = if s.contact { s.pr } else { 0.0 };
    s.barrel = r[1] & 0x02 != 0;
    s.barrel2 = r[1] & 0x10 != 0;
    s.pen_buttons();
    Some(s)
}

/// DTU-1031 / 1031X / 1141, DTK-1651 (`wacom_dtus_irq`): report id 17, big-endian X / Y.
fn wac_dtus(d: &mut Device, r: &[u8]) -> Option<PenState> {
    let m = d.model?;
    if r.len() < 7 || r[0] != 17 {
        return None; // 21 = the pad's buttons
    }
    let prox = r[1] & 0x80 != 0;
    if prox {
        match (r[1] >> 3) & 3 {
            1 => d.rubber = true,
            2 => d.rubber = false,
            _ => {},
        }
    }
    let (nx, ny) = wac_norm(m, be16(r, 3), be16(r, 5));
    let p = ((r[1] & 0x03) as u32) << 8 | r[2] as u32;
    let mut s = PenState { nx, ny, in_range: prox, has_pressure: true, eraser: d.rubber, ..Default::default() };
    s.pr = (p as f32 / m.p_max.max(1) as f32).clamp(0.0, 1.0);
    s.contact = p > 10;
    s.p = if s.contact { s.pr } else { 0.0 };
    s.barrel = r[1] & 0x20 != 0;
    s.barrel2 = r[1] & 0x40 != 0;
    s.pen_buttons();
    Some(s)
}

// ── pen -> system pointer: clicks, tap freeze, double-tap snap, scroll ───────

/// Pressure (0..1) that presses the tip, and the lower one that lets it go again: a pen resting lightly
/// on the glass does not flicker between clicked and not.
pub const P_ON: f32 = 0.012;
pub const P_OFF: f32 = 0.005;
/// While a button is held the pointer stays where it went down until the pen moves this far (px at 1080p)
/// -- generously for the first `FREEZE_MS`, then just past the pen's jitter -- so a tap is a click, not a
/// tiny drag.
pub const FREEZE_PX: f32 = 6.0;
pub const FREEZE_MS: u64 = 80;
pub const FREEZE_PX_AFTER: f32 = 2.5;
/// A tap this close (px at 1080p) to the previous one, within the double-click time, lands on its pixel.
pub const DBL_SLOP_PX: f32 = 8.0;
/// Upper barrel button held and the pen moved this far (px at 1080p): scrolling, one wheel click per
/// `SCROLL_STEP_PX`, instead of a middle click.
pub const SCROLL_START_PX: f32 = 12.0;
pub const SCROLL_STEP_PX: f32 = 24.0;

/// What the system pointer does after one pen sample.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PointerOut {
    pub x: f32,
    pub y: f32,
    /// bit 0 left, 1 right, 2 middle
    pub buttons: u8,
    /// wheel clicks (positive = away from the user / scroll up)
    pub wheel: i32,
    /// the pen is near: the pointer follows it (hover moves the pointer without clicking)
    pub near: bool,
}

/// The pen-as-mouse state machine (one per system: one pen draws at a time).
#[derive(Clone, Copy, Debug, Default)]
pub struct Clicker {
    /// buttons reported last time
    pub down: u8,
    /// the tip is pressed (after the hysteresis)
    tip: bool,
    /// where the pointer froze when a button went down
    anchor: (f32, f32),
    frozen: bool,
    t_down: u64,
    /// the last left tap that stayed put: where and when it went down
    tap: Option<(f32, f32, u64)>,
    /// upper barrel held: where it went down, scrolling yet, the y of the last wheel click
    b2: Option<(f32, f32)>,
    scrolling: bool,
    sc_y: f32,
    /// the pointer's last position
    pos: (f32, f32),
}

impl Clicker {
    /// Nothing held, no tap yet (for a `static`).
    pub const NEW: Clicker = Clicker {
        down: 0,
        tip: false,
        anchor: (0.0, 0.0),
        frozen: false,
        t_down: 0,
        tap: None,
        b2: None,
        scrolling: false,
        sc_y: 0.0,
        pos: (0.0, 0.0),
    };

    /// One sample at screen position (`fx`, `fy`) at `now_ms`; `scale` = screen height / 1080.
    pub fn step(&mut self, s: &PenState, fx: f32, fy: f32, now_ms: u64, scale: f32) -> PointerOut {
        let scale = scale.max(0.5);
        let near = s.in_range || s.contact;
        if !near {
            // left the screen: everything lets go where the pointer is
            self.tip = false;
            self.frozen = false;
            self.b2 = None;
            self.scrolling = false;
            self.down = 0;
            return PointerOut { x: self.pos.0, y: self.pos.1, buttons: 0, wheel: 0, near: false };
        }
        // the tip, with a pressure threshold + hysteresis when the pen reports pressure
        self.tip = if s.has_pressure {
            if self.tip { s.contact && s.pr >= P_OFF } else { s.contact && s.pr >= P_ON }
        } else {
            s.contact
        };
        let want: u8 = if s.barrel {
            2
        } else if s.barrel2 {
            4
        } else {
            self.tip as u8
        };
        let mut wheel = 0;
        // upper barrel: a click, or dragged, a scroll
        if s.barrel2 && !s.barrel {
            let (bx, by) = *self.b2.get_or_insert((fx, fy));
            if !self.scrolling && libm::fabsf(fy - by) > SCROLL_START_PX * scale {
                self.scrolling = true;
                self.sc_y = fy;
            }
            if self.scrolling {
                // the middle button lets go: a scroll, not a click
                let step = SCROLL_STEP_PX * scale;
                let t = ((fy - self.sc_y) / step) as i32;
                if t != 0 {
                    // pen up = the page follows it up = wheel toward the user
                    wheel = t;
                    self.sc_y += t as f32 * step;
                }
                self.pos = (bx, by);
                self.down = 0;
                self.frozen = false;
                return PointerOut { x: bx, y: by, buttons: 0, wheel, near: true };
            }
        } else {
            self.b2 = None;
            self.scrolling = false;
        }
        let d = |a: (f32, f32), b: (f32, f32)| libm::sqrtf((a.0 - b.0) * (a.0 - b.0) + (a.1 - b.1) * (a.1 - b.1));
        if want != 0 && self.down == 0 {
            // a button went down: freeze here, or on the previous tap's pixel for a double-tap
            let mut a = (fx, fy);
            if want == 1 {
                if let Some((tx, ty, tt)) = self.tap {
                    if now_ms.saturating_sub(tt) <= DBL_CLICK_MS && d(a, (tx, ty)) <= DBL_SLOP_PX * scale {
                        a = (tx, ty);
                    }
                }
            }
            self.anchor = a;
            self.frozen = true;
            self.t_down = now_ms;
        }
        let mut pos = (fx, fy);
        if want == 0 {
            if self.down != 0 {
                // released: at the pixel it went down on when it never moved off it
                if self.frozen {
                    pos = self.anchor;
                }
                self.tap = if self.down & 1 != 0 && self.frozen { Some((self.anchor.0, self.anchor.1, self.t_down)) } else { None };
            }
            self.frozen = false;
        } else if self.frozen {
            let r = if now_ms.saturating_sub(self.t_down) < FREEZE_MS { FREEZE_PX } else { FREEZE_PX_AFTER } * scale;
            if d((fx, fy), self.anchor) > r {
                self.frozen = false;
            } else {
                pos = self.anchor;
            }
        }
        self.down = want;
        self.pos = pos;
        PointerOut { x: pos.0, y: pos.1, buttons: want, wheel, near: true }
    }
}

// ── pen samples for LingAnime (art_pen_pump) ────────────────────────────────

/// One pen sample in screen (framebuffer) pixels.
#[derive(Clone, Copy, Default, Debug)]
pub struct Sample {
    pub sx: f32,
    pub sy: f32,
    pub p: f32,
    pub tilt: f32,
    pub az: f32,
    pub twist: f32,
    pub eraser: bool,
    pub contact: bool,
    pub buttons: u32,
}

const RING: usize = 1024;
static mut SAMPLES: [Sample; RING] = [Sample { sx: 0.0, sy: 0.0, p: 0.0, tilt: 0.0, az: 0.0, twist: 0.0, eraser: false, contact: false, buttons: 0 }; RING];
static mut HEAD: usize = 0;
static mut TAIL: usize = 0;
/// pens plugged in right now (bumped by the xHCI driver)
static mut PENS: u32 = 0;

/// Queue a sample (the oldest goes when nobody reads them).
pub fn push(s: Sample) {
    unsafe {
        let h = HEAD;
        (*(&raw mut SAMPLES))[h % RING] = s;
        HEAD = h.wrapping_add(1);
        if HEAD.wrapping_sub(TAIL) > RING {
            TAIL = HEAD.wrapping_sub(RING);
        }
    }
}

/// Every sample since the last call, oldest first.
pub fn take() -> Vec<Sample> {
    unsafe {
        let (h, t) = (HEAD, TAIL);
        let mut out = Vec::with_capacity(h.wrapping_sub(t));
        let mut k = t;
        while k != h {
            out.push((*(&raw const SAMPLES))[k % RING]);
            k = k.wrapping_add(1);
        }
        TAIL = h;
        out
    }
}

pub fn pen_added() {
    unsafe { PENS += 1 };
}
pub fn pen_removed() {
    unsafe { PENS = PENS.saturating_sub(1) };
}
/// Is a USB pen (digitizer) plugged in?
pub fn present() -> bool {
    unsafe { PENS > 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QEMU's usb-tablet (hw/input/hid.c / dev-hid.c): buttons, 15-bit absolute X / Y, wheel.
    const QEMU_TABLET: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01,
        0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xff, 0x7f, 0x35, 0x00, 0x46, 0xff, 0x7f,
        0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0x05, 0x01, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x35, 0x00, 0x45, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06,
        0xc0, 0xc0,
    ];

    /// QEMU's usb-mouse: relative X / Y -> not ours.
    const QEMU_MOUSE: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01,
        0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x03,
        0x81, 0x06, 0xc0, 0xc0,
    ];

    /// A Windows-Ink-style pen (the layout of Microsoft's sample pen descriptor): report id 2, tip / barrel /
    /// invert / eraser / in-range bits, X / Y, pressure, tilt.
    const PEN: &[u8] = &[
        0x05, 0x0d, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x02, 0x09, 0x20, 0xa1, 0x00, // Digitizer / Pen, id 2, Stylus
        0x09, 0x42, 0x09, 0x44, 0x09, 0x3c, 0x09, 0x45, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x04, 0x81, 0x02, // tip barrel invert eraser
        0x95, 0x01, 0x81, 0x03, // pad
        0x09, 0x32, 0x81, 0x02, // in range
        0x95, 0x02, 0x81, 0x03, // pad
        0x05, 0x01, 0x09, 0x30, 0x75, 0x10, 0x95, 0x01, 0xa4, 0x55, 0x0d, 0x65, 0x13, 0x35, 0x00, 0x46, 0x3a, 0x20, 0x26, 0xf8, 0x52, 0x81, 0x02, // X 0..21240
        0x09, 0x31, 0x46, 0x2c, 0x18, 0x26, 0x6c, 0x3e, 0x81, 0x02, 0xb4, // Y 0..15980
        0x05, 0x0d, 0x09, 0x30, 0x26, 0xff, 0x1f, 0x81, 0x02, // pressure 0..8191
        0x09, 0x3d, 0x09, 0x3e, 0x16, 0xd8, 0xdc, 0x26, 0x28, 0x23, 0x36, 0xd8, 0xdc, 0x46, 0x28, 0x23, 0x65, 0x14, 0x55, 0x0e, 0x75, 0x10, 0x95, 0x02,
        0x81, 0x02, // tilt -9000..9000 = -90..90 degrees (exp -2)
        0xc0, 0xc0,
    ];

    #[test]
    fn qemu_tablet_is_absolute() {
        let l = parse(QEMU_TABLET).expect("tablet");
        assert!(!l.pen);
        assert_eq!(l.report_id, None);
        let s = decode(&l, &[0x01, 0xff, 0x3f, 0x00, 0x20, 0x00]).unwrap();
        assert!((s.nx - 0.5).abs() < 0.01 && (s.ny - 0.25).abs() < 0.01, "{s:?}");
        assert_eq!(s.buttons, 1);
        assert!(s.contact && s.in_range);
        let d = Device::hid(0x0627, 0x0001, &parse_full(QEMU_TABLET).0);
        assert!(!d.pen() && d.n == 1);
    }

    #[test]
    fn relative_mouse_is_not() {
        assert!(parse(QEMU_MOUSE).is_none());
    }

    #[test]
    fn pen_report() {
        let l = parse(PEN).expect("pen");
        assert!(l.pen);
        assert_eq!(l.report_id, Some(2));
        assert!(l.pressure.ok && l.tip.ok && l.barrel.ok && l.invert.ok && l.in_range.ok && l.tilt_x.ok);
        // tip down, in range, x = half, y = quarter, pressure half, tilt x +45 degrees
        let x = 10620u16.to_le_bytes();
        let y = 3995u16.to_le_bytes();
        let p = 4095u16.to_le_bytes();
        let tx = 4500i16.to_le_bytes();
        let rep = [2, 0b0010_0001, x[0], x[1], y[0], y[1], p[0], p[1], tx[0], tx[1], 0, 0];
        let s = decode(&l, &rep).unwrap();
        assert!(s.contact && s.in_range && !s.eraser && !s.barrel, "{s:?}");
        assert!((s.nx - 0.5).abs() < 0.001 && (s.ny - 0.25).abs() < 0.001);
        assert!((s.p - 0.5).abs() < 0.01);
        assert!((s.tilt - 0.5).abs() < 0.02, "tilt {}", s.tilt);
        assert!((s.tx - 45.0).abs() < 0.1, "tx {}", s.tx);
        assert_eq!(s.buttons, 1);
        // hovering with the eraser end (invert) and the barrel button held
        let rep = [2, 0b0010_0110, x[0], x[1], y[0], y[1], 0, 0, 0, 0, 0, 0];
        let s = decode(&l, &rep).unwrap();
        assert!(!s.contact && s.eraser && s.barrel && s.p == 0.0);
        assert_eq!(s.buttons, 2);
        // another report id is skipped
        assert!(decode(&l, &[3, 0, 0, 0]).is_none());
    }

    #[test]
    fn wacom_vendor_usages() {
        assert_eq!(equiv(0xFF0D_0130), 0x0001_0030); // X
        assert_eq!(equiv(0xFF0D_0131), 0x0001_0031); // Y
        assert_eq!(equiv(0xFF0D_0030), 0x000D_0030); // tip pressure
        assert_eq!(equiv(0xFF0D_003D), 0x000D_003D); // tilt X
        assert_eq!(equiv(0xFF0D_0001), 0x000D_0001); // the Digitizer collection
        assert_eq!(equiv(0xFF0D_0132), 0xFF0D_0132); // distance stays
        assert_eq!(equiv(0xFF0D_1002), 0xFF0D_1002); // data mode stays
        assert_eq!(equiv(0xFF0D_0D30), 0xFF0D_0D30); // offset left stays
        assert_eq!(equiv(0x000D_0030), 0x000D_0030);
    }

    fn at(scale: f32, s: &PenState, c: &mut Clicker, x: f32, y: f32, t: u64) -> PointerOut {
        c.step(s, x, y, t, scale)
    }

    #[test]
    fn clicks_from_the_pen() {
        let mut c = Clicker::default();
        let hover = PenState { in_range: true, has_pressure: true, ..Default::default() };
        let touch = |p: f32| PenState { in_range: true, contact: true, has_pressure: true, pr: p, p, ..Default::default() };
        // hover moves the pointer, no buttons
        let o = at(1.0, &hover, &mut c, 100.0, 100.0, 0);
        assert!(o.near && o.buttons == 0 && o.x == 100.0);
        // a feather touch under the threshold is not a click
        let o = at(1.0, &touch(0.006), &mut c, 100.0, 100.0, 10);
        assert_eq!(o.buttons, 0);
        // pressed
        let o = at(1.0, &touch(0.2), &mut c, 101.0, 100.0, 20);
        assert_eq!(o.buttons, 1);
        assert_eq!((o.x, o.y), (101.0, 100.0));
        // jitter of 3 px while down: frozen
        let o = at(1.0, &touch(0.2), &mut c, 104.0, 101.0, 40);
        assert_eq!((o.x, o.y, o.buttons), (101.0, 100.0, 1));
        // pressure sags to between the thresholds: still down (hysteresis)
        let o = at(1.0, &touch(0.008), &mut c, 103.0, 100.0, 50);
        assert_eq!(o.buttons, 1);
        // lifted: released on the pixel it went down on
        let o = at(1.0, &hover, &mut c, 104.0, 102.0, 70);
        assert_eq!((o.x, o.y, o.buttons), (101.0, 100.0, 0));
        // hover moves again
        let o = at(1.0, &hover, &mut c, 106.0, 103.0, 120);
        assert_eq!((o.x, o.y), (106.0, 103.0));
        // second tap 6 px away within the double-click time: lands on the first tap's pixel
        let o = at(1.0, &touch(0.3), &mut c, 106.0, 103.0, 200);
        assert_eq!((o.x, o.y, o.buttons), (101.0, 100.0, 1));
        let o = at(1.0, &hover, &mut c, 106.0, 103.0, 260);
        assert_eq!((o.x, o.y, o.buttons), (101.0, 100.0, 0));
        // a third tap too late is not snapped
        let o = at(1.0, &touch(0.3), &mut c, 106.0, 103.0, 900);
        assert_eq!((o.x, o.y), (106.0, 103.0));
        // dragging away unfreezes and follows
        let o = at(1.0, &touch(0.3), &mut c, 130.0, 103.0, 920);
        assert_eq!((o.x, o.buttons), (130.0, 1));
        let o = at(1.0, &hover, &mut c, 140.0, 103.0, 950);
        assert_eq!((o.x, o.buttons), (140.0, 0));
        // a tap far from the dragged one is not snapped
        let o = at(1.0, &touch(0.3), &mut c, 300.0, 300.0, 1000);
        assert_eq!((o.x, o.y), (300.0, 300.0));
        let _ = at(1.0, &hover, &mut c, 300.0, 300.0, 1040);
        // lower barrel = right click (hovering or touching), upper = middle
        let o = at(1.0, &PenState { barrel: true, ..hover }, &mut c, 300.0, 300.0, 2000);
        assert_eq!(o.buttons, 2);
        let o = at(1.0, &PenState { barrel: true, ..touch(0.3) }, &mut c, 300.0, 300.0, 2010);
        assert_eq!(o.buttons, 2);
        let _ = at(1.0, &hover, &mut c, 300.0, 300.0, 2020);
        let o = at(1.0, &PenState { barrel2: true, ..hover }, &mut c, 300.0, 300.0, 3000);
        assert_eq!(o.buttons, 4);
        // eraser end touching = left click too
        let _ = at(1.0, &hover, &mut c, 300.0, 300.0, 3010);
        let o = at(1.0, &PenState { eraser: true, ..touch(0.3) }, &mut c, 300.0, 300.0, 4000);
        assert_eq!(o.buttons, 1);
        // out of range lets everything go
        let o = at(1.0, &PenState::default(), &mut c, 0.0, 0.0, 4100);
        assert!(!o.near && o.buttons == 0);
    }

    #[test]
    fn upper_barrel_drag_scrolls() {
        let mut c = Clicker::default();
        let b2 = PenState { in_range: true, barrel2: true, has_pressure: true, ..Default::default() };
        assert_eq!(c.step(&b2, 500.0, 500.0, 0, 1.0).buttons, 4);
        let o = c.step(&b2, 500.0, 490.0, 10, 1.0);
        assert_eq!(o.buttons, 4); // not far enough yet
        let o = c.step(&b2, 500.0, 480.0, 20, 1.0); // 20 px: scrolling, the middle button lets go
        assert_eq!((o.buttons, o.wheel, o.x, o.y), (0, 0, 500.0, 500.0));
        let o = c.step(&b2, 500.0, 430.0, 30, 1.0); // 50 px up from the start of the scroll: two clicks down
        assert_eq!(o.wheel, -2);
        let o = c.step(&b2, 500.0, 480.0, 40, 1.0);
        assert_eq!(o.wheel, 2);
    }
}
