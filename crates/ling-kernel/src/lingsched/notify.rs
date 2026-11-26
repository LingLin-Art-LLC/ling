//! Desktop toasts for LingCal reminders and LingScheduler notices, drawn by
//! the kernel over whatever has the screen (the desktop or any app), like the
//! screenshot notice: `lingapp::present` swaps the patch into the frame,
//! blits, and swaps it back out, so an app's own buffer is never touched.
//!
//! A reminder has Open / Snooze 5 min / Dismiss; a notice has Open LingCal /
//! Dismiss and goes away by itself. Clicks on a toast are the toast's: the
//! app under it sees the button up (`covers`, checked by `app_mouse_state`).
//! A toast belongs to the user it is for and only shows while that user is
//! signed in (a reminder's text is private); it waits otherwise.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub struct Toast {
    pub id: u32,
    pub owner: String,
    pub header: String,
    pub right: String,
    pub title: String,
    pub detail: String,
    /// Hatch colours of the header (two shades) and the border.
    pub accent: [u32; 2],
    pub reminder: bool,
    /// For Open: the event and occurrence start (local minutes), or 0.
    pub event: u32,
    pub occ: i64,
    pub created_ms: u64,
    /// Hide by itself after this long (0 = until dismissed).
    pub ttl_ms: u64,
}

pub struct Patch {
    pub px: Vec<u32>,
    pub w: usize,
    pub h: usize,
    pub x: usize,
    pub y: usize,
}

static mut TOASTS: Vec<Toast> = Vec::new();
static mut NEXT: u32 = 1;
static mut GEN: u64 = 1;
/// The patch of the last frame and the key it was drawn for.
static mut CACHE: Option<(u64, Patch)> = None;
/// Button rectangles of the drawn patch: (toast id, button 0..2, x, y, w, h), frame coordinates.
static mut BUTTONS: Vec<(u32, u8, i64, i64, i64, i64)> = Vec::new();
static mut AREA: Option<(i64, i64, i64, i64)> = None;
static mut HOVER: Option<(u32, u8)> = None;
static mut WAS_DOWN: bool = false;
/// LingCal should show this event (Open): (event id, occurrence start).
static mut FOCUS: Option<(u32, i64)> = None;
/// Snoozed reminders come back at this time (ms).
static mut SNOOZED: Vec<(u64, Toast)> = Vec::new();

fn toasts() -> &'static mut Vec<Toast> {
    unsafe { &mut *(&raw mut TOASTS) }
}

fn bump() {
    unsafe { GEN += 1 };
}

const PURPLE: [u32; 2] = [0x6d45b0, 0x4f3281];
const GREEN: [u32; 2] = [0x2f8a55, 0x22663e];
const RED: [u32; 2] = [0xa8323a, 0x7d242b];

pub fn push(t: Toast) -> u32 {
    let l = toasts();
    // the same reminder twice (a reload) stays one toast
    if t.reminder && l.iter().any(|x| x.reminder && x.event == t.event && x.occ == t.occ && x.owner == t.owner) {
        return 0;
    }
    if l.len() >= 6 {
        l.remove(0);
    }
    let id = unsafe {
        let i = NEXT;
        NEXT += 1;
        i
    };
    let mut t = t;
    t.id = id;
    l.push(t);
    bump();
    id
}

pub fn reminder(owner: &str, title: &str, detail: &str, right: &str, event: u32, occ: i64, accent: Option<[u32; 2]>) -> u32 {
    push(Toast {
        id: 0,
        owner: String::from(owner),
        header: String::from("LingCal reminder"),
        right: String::from(right),
        title: String::from(title),
        detail: String::from(detail),
        accent: accent.unwrap_or(PURPLE),
        reminder: true,
        event,
        occ,
        created_ms: crate::arch::timer::now_ms(),
        ttl_ms: 0,
    })
}

/// A notice from an automation: `failed` draws it red and keeps it up.
pub fn notice(owner: &str, title: &str, detail: &str, failed: bool, sticky: bool) -> u32 {
    // the same job failing again updates its notice instead of stacking another
    if let Some(t) = toasts().iter_mut().find(|t| t.event == 0 && t.owner == owner && t.title == title) {
        t.detail = String::from(detail);
        t.right = crate::lingsched::now_hm();
        t.created_ms = crate::arch::timer::now_ms();
        t.header = String::from(if failed { "LingScheduler - failed" } else { "LingScheduler" });
        t.accent = if failed { RED } else { GREEN };
        t.ttl_ms = if sticky || failed { 0 } else { 12_000 };
        let id = t.id;
        bump();
        return id;
    }
    push(Toast {
        id: 0,
        owner: String::from(owner),
        header: String::from(if failed { "LingScheduler - failed" } else { "LingScheduler" }),
        right: crate::lingsched::now_hm(),
        title: String::from(title),
        detail: String::from(detail),
        accent: if failed { RED } else { GREEN },
        reminder: sticky,
        event: 0,
        occ: 0,
        created_ms: crate::arch::timer::now_ms(),
        ttl_ms: if sticky || failed { 0 } else { 12_000 },
    })
}

pub fn take_focus() -> Option<(u32, i64)> {
    unsafe { (*(&raw mut FOCUS)).take() }
}

pub fn count_for(user: &str) -> usize {
    toasts().iter().filter(|t| t.owner == user).count()
}

fn visible_user() -> Option<String> {
    crate::keyring::user()
}

/// Expire old toasts and bring back snoozed ones (from the scheduler's tick).
pub fn tick() {
    let now = crate::arch::timer::now_ms();
    let l = toasts();
    let n = l.len();
    l.retain(|t| t.ttl_ms == 0 || now.saturating_sub(t.created_ms) < t.ttl_ms || unsafe { HOVER.map(|h| h.0 == t.id).unwrap_or(false) });
    // a reminder nobody saw for 6 hours is stale
    l.retain(|t| now.saturating_sub(t.created_ms) < 6 * 3600 * 1000);
    if l.len() != n {
        bump();
    }
    let sn = unsafe { &mut *(&raw mut SNOOZED) };
    let mut back = Vec::new();
    sn.retain_mut(|(due, _)| {
        if *due <= now {
            back.push(*due);
            false
        } else {
            true
        }
    });
    // (retain_mut cannot move the toast out: do it in two passes)
    if !back.is_empty() {
        let mut keep = Vec::new();
        let all = core::mem::take(sn);
        for (due, t) in all {
            if back.contains(&due) {
                let mut t = t;
                t.created_ms = now;
                t.right = String::from("snoozed");
                push(t);
                crate::lingsched::chime("soft");
            } else {
                keep.push((due, t));
            }
        }
        *sn = keep;
    }
}

fn act(id: u32, button: u8) {
    let l = toasts();
    let Some(i) = l.iter().position(|t| t.id == id) else { return };
    let t = l.remove(i);
    bump();
    match (t.reminder && t.event != 0, button) {
        // Open
        (_, 0) => {
            if t.event != 0 {
                unsafe { FOCUS = Some((t.event, t.occ)) };
            } else {
                unsafe { FOCUS = Some((0, 0)) };
            }
            if crate::lingapp::active_app_name() != "lingcal" {
                crate::remote::push("open:lingcal");
            }
        },
        // Snooze 5 min (a reminder), Dismiss (a notice)
        (true, 1) => {
            let due = crate::arch::timer::now_ms() + 5 * 60_000;
            unsafe { (*(&raw mut SNOOZED)).push((due, t)) };
        },
        _ => {},
    }
}

/// The pointer, once a frame, in frame coordinates (`buttons` bit 0 = left).
pub fn pointer(x: f64, y: f64, buttons: u8) {
    let (xi, yi) = (x as i64, y as i64);
    let down = buttons & 1 != 0;
    let was = unsafe { WAS_DOWN };
    unsafe { WAS_DOWN = down };
    let btns = unsafe { &*(&raw const BUTTONS) };
    let hit = btns.iter().find(|b| xi >= b.2 && yi >= b.3 && xi < b.2 + b.4 && yi < b.3 + b.5).map(|b| (b.0, b.1));
    if hit != unsafe { HOVER } {
        unsafe { HOVER = hit };
        bump();
    }
    if down && !was {
        if let Some((id, b)) = hit {
            act(id, b);
        }
    }
}

/// Is this frame point on a toast (its clicks are not the app's)?
pub fn covers(x: f64, y: f64) -> bool {
    match unsafe { AREA } {
        Some((ax, ay, aw, ah)) => {
            let (xi, yi) = (x as i64, y as i64);
            xi >= ax && yi >= ay && xi < ax + aw && yi < ay + ah
        },
        None => false,
    }
}

// ── drawing ──────────────────────────────────────────────────────────────────

struct Cv<'a> {
    px: &'a mut [u32],
    w: i64,
    h: i64,
}

impl Cv<'_> {
    fn fill(&mut self, x: i64, y: i64, w: i64, h: i64, c: u32) {
        let (x0, y0, x1, y1) = (x.max(0), y.max(0), (x + w).min(self.w), (y + h).min(self.h));
        for yy in y0..y1 {
            for xx in x0..x1 {
                self.px[(yy * self.w + xx) as usize] = c;
            }
        }
    }
    fn frame(&mut self, x: i64, y: i64, w: i64, h: i64, c: u32) {
        self.fill(x, y, w, 1, c);
        self.fill(x, y + h - 1, w, 1, c);
        self.fill(x, y, 1, h, c);
        self.fill(x + w - 1, y, 1, h, c);
    }
    /// 135-degree stripes, 4 px each, two shades (LingStationX's clip headers).
    fn hatch(&mut self, x: i64, y: i64, w: i64, h: i64, s: i64, a: u32, b: u32) {
        let band = (4 * s).max(2);
        for yy in y.max(0)..(y + h).min(self.h) {
            for xx in x.max(0)..(x + w).min(self.w) {
                let k = ((xx - x) + (yy - y)).div_euclid(band);
                self.px[(yy * self.w + xx) as usize] = if k % 2 == 0 { a } else { b };
            }
        }
    }
    fn blend(&mut self, i: usize, c: u32, a: u32) {
        let d = self.px[i];
        let mix = |s: u32, d: u32| (s * a + d * (255 - a)) / 255;
        self.px[i] = mix(c >> 16 & 255, d >> 16 & 255) << 16 | mix(c >> 8 & 255, d >> 8 & 255) << 8 | mix(c & 255, d & 255);
    }
    /// Text at (x, top y) in `size` px; returns its width.
    fn text(&mut self, x: i64, y: i64, size: f32, c: u32, s: &str, maxw: i64) -> i64 {
        let Some(fd) = font() else {
            return self.text8(x, y, (size / 10.0).max(1.0) as i64, c, s, maxw);
        };
        let asc = fd.horizontal_line_metrics(size).map(|m| m.ascent).unwrap_or(size * 0.8);
        let base = y as f32 + asc;
        let mut pen = 0.0f32;
        let ell = fd.metrics('.', size).advance_width * 3.0;
        let total: f32 = s.chars().map(|ch| fd.metrics(ch, size).advance_width).sum();
        let cut = total > maxw as f32;
        for ch in s.chars() {
            let m = fd.metrics(ch, size);
            if cut && pen + m.advance_width + ell > maxw as f32 {
                for _ in 0..3 {
                    self.glyph(fd, '.', x as f32 + pen, base, size, c);
                    pen += fd.metrics('.', size).advance_width;
                }
                break;
            }
            self.glyph(fd, ch, x as f32 + pen, base, size, c);
            pen += m.advance_width;
        }
        pen as i64
    }
    fn glyph(&mut self, fd: &fontdue::Font, ch: char, x: f32, base: f32, size: f32, c: u32) {
        let (m, cov) = fd.rasterize(ch, size);
        let ox = libm::floorf(x) as i64 + m.xmin as i64;
        let oy = libm::roundf(base) as i64 - m.ymin as i64 - m.height as i64;
        for gy in 0..m.height {
            for gx in 0..m.width {
                let a = cov[gy * m.width + gx] as u32;
                let (xx, yy) = (ox + gx as i64, oy + gy as i64);
                if a == 0 || xx < 0 || yy < 0 || xx >= self.w || yy >= self.h {
                    continue;
                }
                self.blend((yy * self.w + xx) as usize, c, a);
            }
        }
    }
    fn text8(&mut self, x: i64, y: i64, sc: i64, c: u32, s: &str, maxw: i64) -> i64 {
        let mut cx = x;
        for ch in s.bytes() {
            if cx + 8 * sc > x + maxw {
                break;
            }
            for (r, bits) in crate::drivers::font8x8::glyph(ch).iter().enumerate() {
                for b in 0..8i64 {
                    if (bits >> b) & 1 != 0 {
                        self.fill(cx + b * sc, y + r as i64 * sc, sc, sc, c);
                    }
                }
            }
            cx += 8 * sc;
        }
        cx - x
    }
    fn width(&self, size: f32, s: &str) -> i64 {
        match font() {
            Some(fd) => s.chars().map(|ch| fd.metrics(ch, size).advance_width).sum::<f32>() as i64,
            None => (s.len() as i64) * 8 * (size / 10.0).max(1.0) as i64,
        }
    }
}

fn font() -> Option<&'static fontdue::Font> {
    let id = crate::lingapp::ui_font()?;
    crate::lingapp::font_fd(id as f64)
}

fn scale_for(sw: usize, sh: usize) -> f32 {
    #[cfg(target_arch = "x86_64")]
    let dpi = crate::drivers::ui_scale::dpi_pct() as f32 / 100.0;
    #[cfg(not(target_arch = "x86_64"))]
    let dpi = 1.0f32;
    let base = if sh >= 1000 && sw >= 1600 { 1.0 } else if sh >= 700 { 0.9 } else { 0.75 };
    (base * dpi.max(1.0)).clamp(0.75, 2.5)
}

/// The toasts for a `sw` x `sh` frame (top right, under the desktop's bar), while any is due.
pub fn take_patch(sw: usize, sh: usize) -> Option<Patch> {
    let me = visible_user();
    let shown: Vec<&Toast> = toasts().iter().filter(|t| Some(&t.owner) == me.as_ref()).rev().take(3).collect();
    if shown.is_empty() || sw < 300 || sh < 200 {
        unsafe {
            AREA = None;
            (*(&raw mut BUTTONS)).clear();
        }
        return None;
    }
    let minute = crate::arch::timer::now_ms() / 30_000;
    let key = unsafe { GEN }.wrapping_mul(1_000_003) ^ (sw as u64) << 40 ^ (sh as u64) << 20 ^ minute;
    if let Some((k, p)) = unsafe { (*(&raw mut CACHE)).take() } {
        if k == key {
            return Some(p);
        }
    }
    let s = scale_for(sw, sh);
    let si = libm::roundf(s) as i64;
    let z = |v: f32| -> i64 { libm::roundf(v * s) as i64 };
    let w = z(420.0).min(sw as i64 - 16);
    let head = z(26.0);
    let one_h = head + z(64.0) + z(46.0);
    let gap = z(10.0);
    let h = shown.len() as i64 * (one_h + gap);
    let x0 = sw as i64 - w - z(16.0);
    let y0 = z(40.0);
    let mut px = alloc::vec![0x06080bu32; (w * h) as usize];
    let mut cv = Cv { px: &mut px, w, h };
    let mut buttons = Vec::new();
    let hover = unsafe { HOVER };
    for (k, t) in shown.iter().enumerate() {
        let ty = k as i64 * (one_h + gap);
        cv.fill(0, ty, w, one_h, 0x0e0c14);
        cv.frame(0, ty, w, one_h, t.accent[0]);
        cv.hatch(1, ty + 1, w - 2, head - 1, si.max(1), t.accent[0], t.accent[1]);
        // the bell
        let bx = z(10.0);
        let by = ty + head / 2;
        let r = z(5.0);
        cv.fill(bx, by - r, z(10.0), r + z(3.0), 0xffffff);
        cv.fill(bx - z(1.0), by + z(3.0), z(12.0), z(2.0), 0xffffff);
        cv.fill(bx + z(3.5), by + z(6.0), z(3.0), z(2.0), 0xffffff);
        let fs = 13.0 * s;
        let rw = cv.width(11.0 * s, &t.right);
        cv.text(bx + z(18.0), by - z(8.0), fs, 0xffffff, &t.header, w - z(60.0) - rw);
        cv.text(w - z(10.0) - rw, by - z(7.0), 11.0 * s, 0xffffff, &t.right, rw + 4);
        cv.text(z(12.0), ty + head + z(10.0), 17.0 * s, 0xf0f2f6, &t.title, w - z(24.0));
        cv.text(z(12.0), ty + head + z(36.0), 13.0 * s, 0x9aa3b5, &t.detail, w - z(24.0));
        let labels: &[&str] = if t.reminder && t.event != 0 { &["Open", "Snooze 5 min", "Dismiss"] } else { &["Open LingCal", "Dismiss"] };
        let mut bxx = z(12.0);
        let byy = ty + one_h - z(12.0) - z(30.0);
        for (i, lab) in labels.iter().enumerate() {
            let bw = cv.width(13.0 * s, lab) + z(28.0);
            // button numbers: 0 Open, 1 Snooze, 2 Dismiss (a notice's second button is Dismiss)
            let num = if labels.len() == 2 && i == 1 { 2 } else { i as u8 };
            let hot = hover == Some((t.id, num));
            let (bg, bd, fg) = if i == 0 { (if hot { 0x2a80e6 } else { 0x1b6fd1 }, 0x3a8ef0, 0xffffff) } else { (if hot { 0x1c2230 } else { 0x10141b }, if hot { 0x4a5163 } else { 0x2a2f3a }, 0xc9d1e0) };
            cv.fill(bxx, byy, bw, z(30.0), bg);
            cv.frame(bxx, byy, bw, z(30.0), bd);
            cv.text(bxx + z(14.0), byy + z(7.0), 13.0 * s, fg, lab, bw);
            buttons.push((t.id, num, x0 + bxx, y0 + byy, bw, z(30.0)));
            bxx += bw + z(8.0);
        }
    }
    unsafe {
        *(&raw mut BUTTONS) = buttons;
        AREA = Some((x0, y0, w, h));
    }
    if x0 < 0 || y0 + h > sh as i64 {
        return None;
    }
    Some(Patch { px, w: w as usize, h: h as usize, x: x0 as usize, y: y0 as usize })
}

/// Put the drawn patch back for the next frame (it holds the toast pixels again after the second swap).
pub fn give_back(p: Patch, sw: usize, sh: usize) {
    let minute = crate::arch::timer::now_ms() / 30_000;
    let key = unsafe { GEN }.wrapping_mul(1_000_003) ^ (sw as u64) << 40 ^ (sh as u64) << 20 ^ minute;
    unsafe { *(&raw mut CACHE) = Some((key, p)) };
}

/// The patch's pixels and the frame's trade places (twice = no change).
pub fn swap_into(buf: &mut [u32], bw: usize, t: &mut Patch) {
    for row in 0..t.h {
        let o = (t.y + row) * bw + t.x;
        if o + t.w > buf.len() || t.x + t.w > bw {
            return;
        }
        buf[o..o + t.w].swap_with_slice(&mut t.px[row * t.w..(row + 1) * t.w]);
    }
}

#[allow(dead_code)]
pub fn debug() -> String {
    format!("{} toast(s)", toasts().len())
}
