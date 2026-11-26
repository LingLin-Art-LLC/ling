//! "Burn all trash": the Trash app's window catches fire, burns to ash, a fire hydrant pops up,
//! water rises over it all, and the window fades back in -- empty -- while the real delete runs
//! in the background (fsx.rs's delete job). Plus the procedural sound effects for it and for
//! Files (drop / trash / restore), mixed through the sound-effect voices (drivers/sfx.rs).
//!
//! The picture is a screenshot of the window taken the frame the burn starts. The fire is a burn
//! map (each cell's ignition time: its distance from the lit corner, climbing faster than it
//! spreads sideways, frayed by fractal noise) driving a flame field that rises cell by cell
//! (the classic "Doom fire" propagation, with flicker and cooling), embers that float up, the
//! paper browning ahead of the front, a glowing rim, then char and ash with glowing spots. The
//! water is a rising surface of three travelling sine waves with foam, caustics and bubbles.
//! Everything runs on the wall clock, so the sound -- rendered once at the start as one stereo
//! track whose crackle rate and roar follow how much is burning at each moment -- stays in step
//! with the picture at any frame rate. Pixels are shaded on every core (`par`).
//!
//! Builtins: fsx_burn_start(x, y, w, h) -> 1/0; fsx_burn_frame() draws the frame over the window
//! (after the desktop drew it) and returns the phase: 0 fire, 1 hydrant, 2 water, 3 fade-in,
//! 4 finished; fsx_burn_active(); fsx_sound(name); fsx_mute(on); fsx_muted().

use alloc::vec;
use alloc::vec::Vec;

use crate::lingrt::{boolean, num, UNIT};

const T_HYD: f32 = 3.3;
const T_WATER: f32 = 4.3;
const T_FADE: f32 = 6.3;
const T_END: f32 = 7.3;
const BURN_TAIL: f32 = 1.1;
const CS: usize = 4;
const MARGIN: usize = 56;
const SR: f32 = 48_000.0;

#[derive(Clone, Copy)]
struct P {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    life: f32,
    max: f32,
    kind: u8,
    size: f32,
}

struct Burn {
    x: i32,
    y: i32,
    w: usize,
    h: usize,
    shot: Vec<u32>,
    gw: usize,
    gh: usize,
    tign: Vec<f32>,
    heat: Vec<f32>,
    tmp: Vec<f32>,
    parts: Vec<P>,
    t0: u64,
    simt: f32,
    rng: u32,
    sound: Option<usize>,
    sound_len: f32,
    last_t: f32,
    /// the voice the soundtrack plays on: the animation's clock while it sounds
    voice: Option<usize>,
    /// wall clock minus sound clock, kept for when the voice is gone
    clock_off: f64,
    last_audio: f64,
    last_audio_wall: f64,
}

static mut BURN: Option<Burn> = None;
static mut MUTED: bool = false;
/// A finished burn's sound sample, released once it has played out.
static mut RELEASE: Option<(usize, u64)> = None;

fn burn() -> Option<&'static mut Burn> {
    unsafe { (*(&raw mut BURN)).as_mut() }
}

// ── noise ────────────────────────────────────────────────────────────────────

#[inline]
fn hash(x: i32, y: i32, s: u32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x8DA6_B343) ^ (y as u32).wrapping_mul(0xD816_3841) ^ s.wrapping_mul(0xCB1A_B31F);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^= h >> 15;
    (h & 0xFFFF) as f32 / 65535.0
}

#[inline]
fn vnoise(x: f32, y: f32, s: u32) -> f32 {
    let (xf, yf) = (libm::floorf(x), libm::floorf(y));
    let (ix, iy) = (xf as i32, yf as i32);
    let (fx, fy) = (x - xf, y - yf);
    let (ux, uy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let a = hash(ix, iy, s);
    let b = hash(ix + 1, iy, s);
    let c = hash(ix, iy + 1, s);
    let d = hash(ix + 1, iy + 1, s);
    a + (b - a) * ux + (c - a) * uy + (a - b - c + d) * ux * uy
}

fn fbm(x: f32, y: f32) -> f32 {
    vnoise(x, y, 1) * 0.55 + vnoise(x * 2.1, y * 2.1, 2) * 0.3 + vnoise(x * 4.3, y * 4.3, 3) * 0.15
}

fn rnd(r: &mut u32) -> f32 {
    *r ^= *r << 13;
    *r ^= *r >> 17;
    *r ^= *r << 5;
    (*r >> 8) as f32 / 16_777_216.0
}

#[inline]
fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

#[inline]
fn smooth(e0: f32, e1: f32, v: f32) -> f32 {
    let t = clamp01((v - e0) / (e1 - e0));
    t * t * (3.0 - 2.0 * t)
}

#[inline]
fn rgb(c: u32) -> (f32, f32, f32) {
    (((c >> 16) & 255) as f32, ((c >> 8) & 255) as f32, (c & 255) as f32)
}

#[inline]
fn pack(r: f32, g: f32, b: f32) -> u32 {
    ((r.clamp(0.0, 255.0) as u32) << 16) | ((g.clamp(0.0, 255.0) as u32) << 8) | b.clamp(0.0, 255.0) as u32
}

#[inline]
fn mix3(a: (f32, f32, f32), b: (f32, f32, f32), t: f32) -> (f32, f32, f32) {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t)
}

/// Inside the window's rounded outline (the desktop's frames have radius 7)?
#[inline]
fn in_round(px: i32, py: i32, w: i32, h: i32) -> bool {
    const R: i32 = 7;
    let cx = if px < R { R - px } else if px >= w - R { px - (w - R - 1) } else { 0 };
    let cy = if py < R { R - py } else if py >= h - R { py - (h - R - 1) } else { 0 };
    cx * cx + cy * cy <= R * R
}

// ── the burn map and the flame field ─────────────────────────────────────────

impl Burn {
    fn new(x: i32, y: i32, w: usize, h: usize, shot: Vec<u32>) -> Burn {
        let gw = w.div_ceil(CS) + 1;
        let gh = (h + MARGIN).div_ceil(CS) + 1;
        let mut tign = vec![f32::INFINITY; gw * gh];
        let mg = MARGIN / CS;
        for gy in mg..gh {
            for gx in 0..gw {
                // window coords of the cell centre; the fire starts at the bottom-left corner
                let px = (gx * CS) as f32;
                let py = ((gy - mg) * CS) as f32;
                let dx = px / w as f32;
                let dy = (h as f32 - py) / h as f32;
                let d = libm::sqrtf(dx * dx * 1.0 + dy * dy * 0.55);
                let frayed = d + 0.30 * (fbm(px * 0.011, py * 0.011) - 0.5) + 0.05 * (hash(gx as i32, gy as i32, 7) - 0.5);
                tign[gy * gw + gx] = 0.12 + frayed.max(0.0) * 2.05;
            }
        }
        Burn {
            x,
            y,
            w,
            h,
            shot,
            gw,
            gh,
            tign,
            heat: vec![0.0; gw * gh],
            tmp: vec![0.0; gw * gh],
            parts: Vec::new(),
            t0: crate::arch::timer::now_us(),
            simt: 0.0,
            rng: 0x2545_F491,
            sound: None,
            sound_len: 0.0,
            last_t: 0.0,
            voice: None,
            clock_off: 0.0,
            last_audio: -1.0,
            last_audio_wall: 0.0,
        }
    }

    /// Ignition time at a window pixel (bilinear over the cells, so the front is smooth).
    #[inline]
    fn tign_at(&self, px: f32, py: f32) -> f32 {
        let gx = px / CS as f32;
        let gy = (py + MARGIN as f32) / CS as f32;
        let (x0, y0) = (gx as usize, gy as usize);
        let (x1, y1) = ((x0 + 1).min(self.gw - 1), (y0 + 1).min(self.gh - 1));
        let (fx, fy) = (gx - x0 as f32, gy - y0 as f32);
        let g = |x: usize, y: usize| {
            let v = self.tign[y * self.gw + x];
            if v.is_finite() {
                v
            } else {
                99.0
            }
        };
        let a = g(x0, y0) + (g(x1, y0) - g(x0, y0)) * fx;
        let b = g(x0, y1) + (g(x1, y1) - g(x0, y1)) * fx;
        a + (b - a) * fy
    }

    /// Flame heat at a pixel of the render area (window + margin above).
    #[inline]
    fn heat_at(&self, px: f32, my: f32) -> f32 {
        let gx = px / CS as f32;
        let gy = my / CS as f32;
        let (x0, y0) = ((gx as usize).min(self.gw - 1), (gy as usize).min(self.gh - 1));
        let (x1, y1) = ((x0 + 1).min(self.gw - 1), (y0 + 1).min(self.gh - 1));
        let (fx, fy) = (gx - x0 as f32, gy - y0 as f32);
        let g = |x: usize, y: usize| self.heat[y * self.gw + x];
        let a = g(x0, y0) + (g(x1, y0) - g(x0, y0)) * fx;
        let b = g(x0, y1) + (g(x1, y1) - g(x0, y1)) * fx;
        a + (b - a) * fy
    }

    /// One 1/60 s step of the flames: burning cells feed heat, heat rises and cools.
    fn sim_step(&mut self) {
        let t = self.simt;
        let (gw, gh) = (self.gw, self.gh);
        for gy in 0..gh {
            for gx in 0..gw {
                let i = gy * gw + gx;
                let age = t - self.tign[i];
                // a cell flares as the front reaches it, then smoulders down: the flames ride the
                // front instead of filling everything behind it
                let src = if !(age > 0.0) {
                    0.0
                } else if age < 0.12 {
                    age / 0.12
                } else if age < 0.4 {
                    1.0
                } else {
                    (1.0 - (age - 0.4) / 0.7).max(0.0) * 0.8
                };
                let r = rnd(&mut self.rng);
                let below = if gy + 1 < gh {
                    let k = (r * 3.0) as usize;
                    let sx = (gx + k).saturating_sub(1).min(gw - 1);
                    self.heat[(gy + 1) * gw + sx]
                } else {
                    0.0
                };
                let rise = below * 0.962 - rnd(&mut self.rng) * 0.05;
                // flicker: a cell's own fire comes and goes, so tongues break up as they rise
                let flick = if r < 0.25 { 0.35 } else { 0.7 + 0.3 * r };
                self.tmp[i] = (src * flick).max(rise).max(0.0);
            }
        }
        core::mem::swap(&mut self.heat, &mut self.tmp);
        // embers from what is burning now
        for _ in 0..10 {
            let gx = (rnd(&mut self.rng) * gw as f32) as usize % gw;
            let gy = (rnd(&mut self.rng) * gh as f32) as usize % gh;
            let age = t - self.tign[gy * gw + gx];
            if age > 0.05 && age < 1.1 && self.parts.len() < 700 && rnd(&mut self.rng) < 0.55 {
                let max = 0.7 + rnd(&mut self.rng) * 1.3;
                let p = P {
                    x: (gx * CS) as f32,
                    y: (gy * CS) as f32 - MARGIN as f32,
                    vx: (rnd(&mut self.rng) - 0.5) * 50.0,
                    vy: -60.0 - rnd(&mut self.rng) * 120.0,
                    life: max,
                    max,
                    kind: 0,
                    size: 1.0 + rnd(&mut self.rng) * 1.6,
                };
                self.parts.push(p);
            }
        }
        self.simt += 1.0 / 60.0;
    }

    fn step_parts(&mut self, t: f32, dt: f32) {
        let surface = water_y(t, self.w as f32, self.h as f32, 0.0);
        let h = self.h as f32;
        let (w, nx, ny) = (self.w as f32, self.hyd_nozzle(t).0, self.hyd_nozzle(t).1);
        for p in self.parts.iter_mut() {
            match p.kind {
                0 => {
                    p.vy -= 25.0 * dt;
                    p.vx += libm::sinf(p.y * 0.05 + p.life * 6.0) * 40.0 * dt;
                },
                1 => p.vy += 620.0 * dt,
                _ => {
                    p.vx = libm::sinf(p.y * 0.08 + p.max * 9.0) * 18.0;
                },
            }
            p.x += p.vx * dt;
            p.y += p.vy * dt;
            p.life -= dt;
            if p.kind == 1 && (p.y > h || p.y > surface) {
                p.life = 0.0;
            }
            if p.kind == 2 && p.y < surface + 4.0 {
                p.life = 0.0;
            }
        }
        self.parts.retain(|p| p.life > 0.0 && p.x > -20.0 && p.x < w + 20.0);
        let r = &mut self.rng;
        // the hydrant's jet, once the valve is open and until the water covers the nozzle
        if t > 3.95 && t < T_FADE && ny < surface - 2.0 {
            for _ in 0..(dt * 700.0) as usize + 1 {
                if self.parts.len() < 1400 {
                    let max = 1.4;
                    self.parts.push(P {
                        x: nx,
                        y: ny + (rnd(r) - 0.5) * 6.0,
                        vx: 230.0 + rnd(r) * 140.0,
                        vy: -170.0 + rnd(r) * 120.0,
                        life: max,
                        max,
                        kind: 1,
                        size: 1.5 + rnd(r) * 2.0,
                    });
                }
            }
        }
        // bubbles from the bottom while it fills
        if t > T_WATER && t < T_END {
            for _ in 0..(dt * 70.0) as usize + 1 {
                if rnd(r) < 0.6 && self.parts.len() < 1400 {
                    let max = 3.0 + rnd(r) * 2.0;
                    self.parts.push(P { x: rnd(r) * w, y: h - 2.0, vx: 0.0, vy: -50.0 - rnd(r) * 110.0, life: max, max, kind: 2, size: 1.5 + rnd(r) * 3.5 });
                }
            }
        }
    }

    /// The hydrant's right nozzle tip in window coords.
    fn hyd_nozzle(&self, t: f32) -> (f32, f32) {
        let u = hyd_unit(self.h as f32) * hyd_scale(t);
        (self.w as f32 / 2.0 + 34.0 * u, self.h as f32 - 12.0 - 40.0 * u)
    }
}

fn hyd_unit(h: f32) -> f32 {
    (h * 0.42).min(190.0) / 100.0
}

/// The hydrant's pop-in: an overshooting ease.
fn hyd_scale(t: f32) -> f32 {
    let tau = clamp01((t - T_HYD) / 0.32);
    let c = 2.2;
    1.0 + (c + 1.0) * libm::powf(tau - 1.0, 3.0) + c * libm::powf(tau - 1.0, 2.0)
}

/// The water surface's mean height (window y) at time t; `x` adds the waves.
fn water_y(t: f32, _w: f32, h: f32, x: f32) -> f32 {
    if t < T_WATER {
        return h + 100.0;
    }
    let u = clamp01((t - T_WATER) / (T_FADE - T_WATER - 0.15));
    let e = u * u * (3.0 - 2.0 * u);
    let base = h + 14.0 - e * (h + 60.0);
    if x == 0.0 {
        return base;
    }
    base + 6.0 * libm::sinf(x * 0.034 + t * 3.1) + 3.5 * libm::sinf(x * 0.087 - t * 4.6) + 1.8 * libm::sinf(x * 0.21 + t * 7.3)
}

// ── shading ──────────────────────────────────────────────────────────────────

/// The burnt look of a window pixel `age` seconds after the front reached it.
#[inline]
fn burnt(base: u32, age: f32, px: f32, py: f32, t: f32) -> (f32, f32, f32) {
    let c = rgb(base);
    if age < -0.3 {
        return c;
    }
    if age < 0.0 {
        // the paper browns ahead of the flames
        let k = (age + 0.3) / 0.3;
        return mix3(c, (125.0, 72.0, 28.0), k * k * 0.6);
    }
    let n = vnoise(px * 0.16, py * 0.16, 11);
    let scorched = mix3(c, (92.0, 46.0, 16.0), 0.7);
    let ash = (20.0 + n * 30.0, 17.0 + n * 25.0, 15.0 + n * 21.0);
    let mut col = mix3(scorched, ash, smooth(0.0, 0.5, age));
    if age < 0.16 {
        let k = (1.0 - age / 0.16) * (0.75 + 0.5 * vnoise(px * 0.3, py * 0.3 + t * 4.0, 5));
        col = (col.0 + 255.0 * k, col.1 + 175.0 * k, col.2 + 70.0 * k);
    }
    if age > 0.08 && age < 2.6 {
        let g = 1.0 - (age - 0.08) / 2.52;
        let n2 = vnoise(px * 0.07 + 3.0, py * 0.07 - t * 0.35, 9);
        let e = (n2 - 0.6).max(0.0) * 3.2 * g * (0.65 + 0.35 * libm::sinf(t * 9.0 + n2 * 23.0));
        col = (col.0 + 255.0 * e, col.1 + 92.0 * e, col.2 + 14.0 * e);
    }
    col
}

#[inline]
fn flame(h: f32) -> ((f32, f32, f32), f32) {
    let stops: [(f32, (f32, f32, f32)); 6] = [(0.0, (60.0, 0.0, 0.0)), (0.2, (140.0, 14.0, 0.0)), (0.42, (230.0, 62.0, 6.0)), (0.66, (255.0, 150.0, 22.0)), (0.86, (255.0, 222.0, 96.0)), (1.0, (255.0, 250.0, 215.0))];
    let h = clamp01(h);
    let mut k = 0;
    while k + 2 < stops.len() && h > stops[k + 1].0 {
        k += 1;
    }
    let (a, b) = (stops[k], stops[k + 1]);
    let col = mix3(a.1, b.1, clamp01((h - a.0) / (b.0 - a.0)));
    (col, clamp01(h * 1.9) * 0.96)
}

/// Water over `under` at window pixel (px, py); `fade` scales its opacity.
#[inline]
fn water(under: (f32, f32, f32), px: f32, py: f32, t: f32, w: f32, h: f32, fade: f32) -> (f32, f32, f32) {
    let surf = water_y(t, w, h, px + 0.5);
    let d = py - surf;
    if d < 0.0 {
        return under;
    }
    let deep = clamp01(d / (h * 0.7));
    let mut wc = mix3((78.0, 176.0, 236.0), (12.0, 52.0, 124.0), deep);
    let c1 = vnoise(px * 0.045 + t * 0.55, py * 0.045 - t * 0.3, 21);
    let c2 = vnoise(px * 0.04 - t * 0.42, py * 0.05 + t * 0.22, 22);
    let line = (1.0 - libm::fabsf(c1 - c2) * 7.0).max(0.0);
    let ca = line * line * line * 0.5 * (1.0 - deep * 0.6);
    wc = (wc.0 + 120.0 * ca, wc.1 + 190.0 * ca, wc.2 + 255.0 * ca * 0.8);
    let u = clamp01((t - T_WATER) / (T_FADE - T_WATER));
    let mut a = 0.58 + 0.34 * deep;
    a += (1.0 - a) * smooth(0.55, 1.0, u);
    if d < 2.5 {
        wc = (236.0, 248.0, 255.0);
        a = 0.95;
    } else if d < 7.0 {
        let k = 1.0 - (d - 2.5) / 4.5;
        wc = mix3(wc, (200.0, 235.0, 255.0), k * 0.6);
    }
    mix3(under, wc, a * fade)
}

// ── drawing helpers on the canvas (clipped to the window rect) ──────────────

struct Canvas<'a> {
    buf: &'a mut [u32],
    cw: i32,
    ch: i32,
    ox: i32,
    oy: i32,
    w: i32,
    h: i32,
}

impl Canvas<'_> {
    #[inline]
    fn blend(&mut self, px: i32, py: i32, c: (f32, f32, f32), a: f32, add: bool) {
        if px < 0 || py < 0 || px >= self.w || py >= self.h || !in_round(px, py, self.w, self.h) {
            return;
        }
        let (sx, sy) = (self.ox + px, self.oy + py);
        if sx < 0 || sy < 0 || sx >= self.cw || sy >= self.ch {
            return;
        }
        let i = (sy * self.cw + sx) as usize;
        let d = rgb(self.buf[i]);
        let o = if add { (d.0 + c.0 * a, d.1 + c.1 * a, d.2 + c.2 * a) } else { mix3(d, c, clamp01(a)) };
        self.buf[i] = pack(o.0, o.1, o.2);
    }

    fn disc(&mut self, cx: f32, cy: f32, r: f32, c: (f32, f32, f32), a: f32, add: bool) {
        let r2 = r * r;
        for yy in libm::floorf(cy - r) as i32..=libm::ceilf(cy + r) as i32 {
            for xx in libm::floorf(cx - r) as i32..=libm::ceilf(cx + r) as i32 {
                let (dx, dy) = (xx as f32 + 0.5 - cx, yy as f32 + 0.5 - cy);
                let q = dx * dx + dy * dy;
                if q <= r2 {
                    let edge = clamp01((r2 - q) / (r * 1.6).max(1.0));
                    self.blend(xx, yy, c, a * edge, add);
                }
            }
        }
    }

    fn ring(&mut self, cx: f32, cy: f32, r: f32, c: (f32, f32, f32), a: f32) {
        for yy in libm::floorf(cy - r - 1.0) as i32..=libm::ceilf(cy + r + 1.0) as i32 {
            for xx in libm::floorf(cx - r - 1.0) as i32..=libm::ceilf(cx + r + 1.0) as i32 {
                let (dx, dy) = (xx as f32 + 0.5 - cx, yy as f32 + 0.5 - cy);
                let d = libm::fabsf(libm::sqrtf(dx * dx + dy * dy) - r);
                if d < 1.1 {
                    self.blend(xx, yy, c, a * (1.1 - d), false);
                }
            }
        }
    }

    /// A shaded upright cylinder piece (the hydrant's metal): bright left of centre, dark edges.
    fn cyl(&mut self, x0: f32, y0: f32, w: f32, h: f32, base: (f32, f32, f32), round_top: f32) {
        // a dark outline first
        let ol = (70.0, 8.0, 8.0);
        for yy in libm::floorf(y0 - 1.5) as i32..libm::ceilf(y0 + h + 1.5) as i32 {
            for xx in libm::floorf(x0 - 1.5) as i32..libm::ceilf(x0 + w + 1.5) as i32 {
                let fx = (xx as f32 + 0.5 - x0) / w;
                let fy = yy as f32 + 0.5 - y0;
                let inside = |fx: f32, fy: f32| {
                    if !(0.0..=1.0).contains(&fx) || fy < 0.0 || fy > h {
                        return false;
                    }
                    if round_top > 0.0 && fy < round_top {
                        // a half ellipse on top
                        let ex = (fx - 0.5) * 2.0;
                        let ey = (round_top - fy) / round_top;
                        return ex * ex + ey * ey <= 1.0;
                    }
                    true
                };
                if inside(fx, fy) {
                    let s = 0.42 + 0.78 * libm::sinf(core::f32::consts::PI * clamp01(fx * 0.92 + 0.04));
                    let hl = 0.55 * libm::expf(-((fx - 0.3) / 0.07) * ((fx - 0.3) / 0.07));
                    let c = (base.0 * s + 255.0 * hl, base.1 * s + 230.0 * hl, base.2 * s + 220.0 * hl);
                    self.blend(xx, yy, c, 1.0, false);
                } else if inside((xx as f32 + 0.5 - x0 + 1.5) / w, fy) || inside((xx as f32 + 0.5 - x0 - 1.5) / w, fy) || inside(fx, fy + 1.5) || inside(fx, fy - 1.5) {
                    self.blend(xx, yy, ol, 0.9, false);
                }
            }
        }
    }
}

/// The fire hydrant, standing on the window's bottom edge, `u` px per unit.
fn hydrant(cv: &mut Canvas, cx: f32, yb: f32, u: f32, valve: f32, open: bool) {
    let red = (206.0, 34.0, 30.0);
    let dark = (150.0, 20.0, 18.0);
    let metal = (150.0, 152.0, 160.0);
    // side nozzles (behind the body)
    cv.cyl(cx - 34.0 * u, yb - 47.0 * u, 13.0 * u, 14.0 * u, red, 0.0);
    cv.cyl(cx + 21.0 * u, yb - 47.0 * u, 13.0 * u, 14.0 * u, red, 0.0);
    cv.cyl(cx - 37.0 * u, yb - 48.5 * u, 4.5 * u, 17.0 * u, dark, 0.0);
    if !open {
        cv.cyl(cx + 32.5 * u, yb - 48.5 * u, 4.5 * u, 17.0 * u, dark, 0.0);
    }
    // base flange, body, the ring under the dome, the dome, the operating nut
    cv.cyl(cx - 30.0 * u, yb - 8.0 * u, 60.0 * u, 8.0 * u, dark, 0.0);
    cv.cyl(cx - 21.0 * u, yb - 60.0 * u, 42.0 * u, 52.0 * u, red, 0.0);
    cv.cyl(cx - 25.0 * u, yb - 66.0 * u, 50.0 * u, 7.0 * u, dark, 0.0);
    cv.cyl(cx - 22.0 * u, yb - 84.0 * u, 44.0 * u, 19.0 * u, red, 18.0 * u);
    let nw = 14.0 * u * (0.55 + 0.45 * libm::fabsf(libm::cosf(valve)));
    cv.cyl(cx - nw / 2.0, yb - 93.0 * u, nw, 10.0 * u, metal, 0.0);
    // the big pumper cap in front, with its nut
    cv.disc(cx, yb - 36.0 * u, 10.0 * u, (120.0, 16.0, 14.0), 1.0, false);
    cv.disc(cx, yb - 36.0 * u, 8.0 * u, (180.0, 28.0, 24.0), 1.0, false);
    cv.disc(cx - 2.0 * u, yb - 38.0 * u, 3.0 * u, (255.0, 120.0, 110.0), 0.7, false);
    cv.disc(cx, yb - 36.0 * u, 3.0 * u, metal, 1.0, false);
    // the cap chain
    for k in 0..6 {
        let f = k as f32 / 5.0;
        cv.ring(cx - 33.0 * u + f * 24.0 * u, yb - 33.0 * u + libm::sinf(f * 3.1) * 6.0 * u, 1.4 * u, (90.0, 90.0, 96.0), 0.9);
    }
    // a white stripe band like the real ones
    for yy in 0..(3.0 * u) as i32 {
        for xx in 0..(42.0 * u) as i32 {
            let fx = xx as f32 / (42.0 * u);
            let s = 0.6 + 0.5 * libm::sinf(core::f32::consts::PI * fx);
            cv.blend((cx - 21.0 * u) as i32 + xx, (yb - 20.0 * u) as i32 + yy, (235.0 * s, 235.0 * s, 230.0 * s), 0.92, false);
        }
    }
}

// ── one frame ────────────────────────────────────────────────────────────────

fn frame() -> u8 {
    let Some(b) = burn() else { return 4 };
    // the clock: how far the soundtrack has played (a frame held up by a slow disk delays picture
    // and sound alike, so they never drift apart); the wall clock without sound
    let wall = crate::arch::timer::now_us().saturating_sub(b.t0) as f64 / 1e6;
    let audio = match (b.voice, b.sound) {
        (Some(v), Some(id)) => crate::drivers::sfx::voice_pos(v, id),
        _ => None,
    };
    // a voice that does not advance (no sound device pulling the mix) is no clock
    let audio = match audio {
        Some(p) if p > b.last_audio + 1e-6 => {
            b.last_audio = p;
            b.last_audio_wall = wall;
            Some(p)
        },
        Some(p) if wall - b.last_audio_wall < 0.35 => Some(p),
        Some(_) => {
            b.voice = None;
            b.clock_off = wall - b.last_t as f64;
            None
        },
        None => None,
    };
    let t = match audio {
        Some(p) => {
            b.clock_off = wall - p;
            p as f32
        },
        None => (wall - b.clock_off) as f32,
    }
    .max(b.last_t);
    if t >= T_END {
        if let Some(id) = b.sound {
            unsafe { RELEASE = Some((id, crate::arch::timer::now_us() + 1_500_000)) };
        }
        unsafe { BURN = None };
        return 4;
    }
    let dt = (t - b.last_t).clamp(0.0, 0.1);
    b.last_t = t;
    // the flames step at 60 Hz whatever the frame rate (a few steps at most per frame)
    let mut steps = 0;
    while b.simt < t && steps < 5 && t < T_WATER + 0.5 {
        b.sim_step();
        steps += 1;
    }
    if b.simt < t - 0.2 {
        b.simt = t;
    }
    b.step_parts(t, dt);
    let Some((buf, cw, ch)) = crate::lingapp::canvas() else { return phase(t) };
    let (w, h) = (b.w as i32, b.h as i32);
    let top = if t < T_WATER { MARGIN as i32 } else { 0 };
    let rows = (h + top) as usize;
    let (chunks, per) = crate::par::row_chunks(rows, 16);
    let bp = crate::par::Shared(buf.as_mut_ptr());
    let bref: &Burn = b;
    let (cw_i, ch_i) = (cw as i32, ch as i32);
    let fade = if t >= T_FADE { 1.0 - clamp01((t - T_FADE) / (T_END - T_FADE)) } else { 1.0 };
    // pass 1: fire / ash / water, every pixel of the window (and the flames above it)
    crate::par::for_each(chunks, &|c: usize| {
        let p = bp;
        for r in c * per..((c + 1) * per).min(rows) {
            let py = r as i32 - top;
            let sy = bref.y + py;
            if sy < 0 || sy >= ch_i {
                continue;
            }
            for px in 0..w {
                let sx = bref.x + px;
                if sx < 0 || sx >= cw_i {
                    continue;
                }
                let inwin = py >= 0 && in_round(px, py, w, h);
                if py >= 0 && !inwin && t >= T_HYD {
                    continue;
                }
                let idx = (sy * cw_i + sx) as usize;
                let cur = unsafe { *p.0.add(idx) };
                let (fx, fy) = (px as f32, py as f32);
                let mut col;
                if t >= T_FADE {
                    // the window as the desktop drew it now (the empty Trash), the water fading off it
                    if !inwin {
                        continue;
                    }
                    col = water(rgb(cur), fx, fy, t, w as f32, h as f32, fade);
                } else {
                    col = if inwin {
                        let jitter = (hash(px, py, 3) - 0.5) * 0.05;
                        burnt(bref.shot[(py * w + px) as usize], t - bref.tign_at(fx, fy) + jitter, fx, fy, t)
                    } else {
                        rgb(cur)
                    };
                    if t < T_WATER + 0.5 {
                        let hh = bref.heat_at(fx, (py + MARGIN as i32) as f32);
                        if hh > 0.015 {
                            // turbulence that scrolls upward: licking tongues, not a flat sheet
                            let wob = vnoise(fx * 0.045, fy * 0.03 + t * 3.2, 31) * 0.65 + vnoise(fx * 0.11, fy * 0.08 + t * 5.0, 32) * 0.35;
                            let (fc, a) = flame(hh * (0.45 + 0.95 * wob));
                            let a = if inwin { a } else { a * 0.85 };
                            col = mix3(col, fc, a);
                            col = (col.0 + fc.0 * 0.18 * hh, col.1 + fc.1 * 0.12 * hh, col.2);
                        }
                    }
                    if !inwin {
                        unsafe { *p.0.add(idx) = pack(col.0, col.1, col.2) };
                        continue;
                    }
                }
                unsafe { *p.0.add(idx) = pack(col.0, col.1, col.2) };
            }
        }
    });
    let mut cv = Canvas { buf, cw: cw_i, ch: ch_i, ox: b.x, oy: b.y, w, h };
    // pass 2: the hydrant (under the water), its flash, then particles
    if (T_HYD..T_FADE).contains(&t) {
        let u = hyd_unit(h as f32) * hyd_scale(t);
        let cx = w as f32 / 2.0;
        let yb = h as f32 - 12.0;
        let tau = t - T_HYD;
        if tau < 0.6 {
            // the flash: a white burst behind it
            let k = 1.0 - tau / 0.6;
            let rad = (h as f32 * 0.42).min(190.0) * (0.9 + tau * 2.2);
            let (cyf, r2) = (yb - 50.0 * u, rad * rad);
            for yy in (cyf - rad) as i32..(cyf + rad) as i32 {
                for xx in (cx - rad) as i32..(cx + rad) as i32 {
                    let (dx, dy) = (xx as f32 - cx, yy as f32 - cyf);
                    let q = (dx * dx + dy * dy) / r2;
                    if q < 1.0 {
                        cv.blend(xx, yy, (255.0, 250.0, 235.0), (1.0 - q) * (1.0 - q) * k * 0.9, true);
                    }
                }
            }
        }
        // a strobe as it lands: two quick bright flashes of the hydrant itself
        let valve = if t > 3.85 { (t - 3.85) * 9.0 } else { 0.0 };
        hydrant(&mut cv, cx, yb, u, valve, t > 3.95);
        if tau < 0.5 && libm::fmodf(tau, 0.25) < 0.08 {
            for yy in (yb - 95.0 * u) as i32..yb as i32 {
                for xx in (cx - 40.0 * u) as i32..(cx + 40.0 * u) as i32 {
                    cv.blend(xx, yy, (255.0, 255.0, 255.0), 0.35, true);
                }
            }
        }
    }
    let surface = water_y(t, w as f32, h as f32, 0.0);
    for p in b.parts.iter() {
        let lf = clamp01(p.life / p.max);
        match p.kind {
            0 => {
                let c = mix3((255.0, 70.0, 10.0), (255.0, 230.0, 150.0), lf * lf);
                if p.y < surface {
                    cv.disc(p.x, p.y, p.size, c, lf * 0.95, true);
                }
            },
            1 => cv.disc(p.x, p.y, p.size, (200.0, 232.0, 255.0), 0.85 * lf.max(0.4), false),
            _ => {},
        }
    }
    // pass 3: the water rises over it all (the hydrant goes under)
    if (T_WATER..T_FADE).contains(&t) {
        let rows = h as usize;
        let (chunks, per) = crate::par::row_chunks(rows, 16);
        let bp = crate::par::Shared(cv.buf.as_mut_ptr());
        let (bx, by) = (b.x, b.y);
        crate::par::for_each(chunks, &|c: usize| {
            let p = bp;
            for r in c * per..((c + 1) * per).min(rows) {
                let py = r as i32;
                let sy = by + py;
                if sy < 0 || sy >= ch_i || (py as f32) < surface - 16.0 {
                    continue;
                }
                for px in 0..w {
                    let sx = bx + px;
                    if sx < 0 || sx >= cw_i || !in_round(px, py, w, h) {
                        continue;
                    }
                    let idx = (sy * cw_i + sx) as usize;
                    let col = water(rgb(unsafe { *p.0.add(idx) }), px as f32, py as f32, t, w as f32, h as f32, 1.0);
                    unsafe { *p.0.add(idx) = pack(col.0, col.1, col.2) };
                }
            }
        });
    }
    if t >= T_WATER {
        for p in b.parts.iter() {
            if p.kind == 2 && p.y > water_y(t, w as f32, h as f32, p.x + 0.5) + 3.0 {
                cv.ring(p.x, p.y, p.size, (215.0, 240.0, 255.0), 0.75 * fade);
                cv.disc(p.x - p.size * 0.35, p.y - p.size * 0.35, (p.size * 0.3).max(0.6), (255.0, 255.0, 255.0), 0.7 * fade, false);
            }
        }
    }
    phase(t)
}

fn phase(t: f32) -> u8 {
    if t < T_HYD {
        0
    } else if t < T_WATER {
        1
    } else if t < T_FADE {
        2
    } else if t < T_END {
        3
    } else {
        4
    }
}

fn start(x: i32, y: i32, w: usize, h: usize) -> bool {
    if w < 40 || h < 40 {
        return false;
    }
    let Some((buf, cw, ch)) = crate::lingapp::canvas() else { return false };
    let mut shot = vec![0u32; w * h];
    for yy in 0..h as i32 {
        for xx in 0..w as i32 {
            let (sx, sy) = (x + xx, y + yy);
            if sx >= 0 && sy >= 0 && (sx as usize) < cw && (sy as usize) < ch {
                shot[(yy as usize) * w + xx as usize] = buf[sy as usize * cw + sx as usize];
            }
        }
    }
    let mut b = Burn::new(x, y, w, h, shot);
    // how much is burning at each moment, for the roar and the crackle
    let bins = ((T_END + 1.0) * 100.0) as usize;
    let mut delta = vec![0i32; bins + 2];
    let mut cells = 0;
    for &ti in &b.tign {
        if ti.is_finite() {
            cells += 1;
            let a = ((ti * 100.0) as usize).min(bins);
            let e = (((ti + BURN_TAIL) * 100.0) as usize).min(bins);
            delta[a] += 1;
            delta[e] -= 1;
        }
    }
    let mut frac = vec![0f32; bins];
    let mut run = 0i32;
    let mut peak = 1i32;
    let mut counts = vec![0i32; bins];
    for i in 0..bins {
        run += delta[i];
        counts[i] = run;
        peak = peak.max(run);
    }
    for i in 0..bins {
        frac[i] = counts[i] as f32 / peak as f32;
    }
    let _ = cells;
    if !muted() {
        let pcm = burn_track(&frac);
        b.sound_len = pcm.len() as f32 / 2.0 / SR;
        let id = crate::drivers::sfx::stream_open(SR as u32, true);
        crate::drivers::sfx::stream_push(id, &pcm);
        crate::drivers::sfx::stream_close(id);
        b.sound = Some(id);
    }
    // the clock starts with the sound
    b.t0 = crate::arch::timer::now_us();
    if let Some(id) = b.sound {
        b.voice = crate::drivers::sfx::play(id, 0.9, 0.0, false, 1.0, false);
    }
    unsafe { BURN = Some(b) };
    true
}

pub fn tick() {
    unsafe {
        if let Some((id, at)) = RELEASE {
            if crate::arch::timer::now_us() >= at {
                crate::drivers::sfx::release(id);
                RELEASE = None;
            }
        }
    }
}

// ── sound ────────────────────────────────────────────────────────────────────

struct Rng(u32);
impl Rng {
    fn f(&mut self) -> f32 {
        rnd(&mut self.0) * 2.0 - 1.0
    }
    fn u(&mut self) -> f32 {
        rnd(&mut self.0)
    }
}

/// A biquad (RBJ cookbook) for the band-passes.
#[derive(Clone, Copy, Default)]
struct Bq {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Bq {
    fn bandpass(&mut self, fc: f32, q: f32) {
        let w = 2.0 * core::f32::consts::PI * fc.clamp(20.0, SR * 0.45) / SR;
        let al = libm::sinf(w) / (2.0 * q);
        let a0 = 1.0 + al;
        self.b0 = al / a0;
        self.b1 = 0.0;
        self.b2 = -al / a0;
        self.a1 = -2.0 * libm::cosf(w) / a0;
        self.a2 = (1.0 - al) / a0;
    }
    fn lowpass(&mut self, fc: f32, q: f32) {
        let w = 2.0 * core::f32::consts::PI * fc.clamp(20.0, SR * 0.45) / SR;
        let al = libm::sinf(w) / (2.0 * q);
        let cw = libm::cosf(w);
        let a0 = 1.0 + al;
        self.b0 = (1.0 - cw) / 2.0 / a0;
        self.b1 = (1.0 - cw) / a0;
        self.b2 = self.b0;
        self.a1 = -2.0 * cw / a0;
        self.a2 = (1.0 - al) / a0;
    }
    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

#[inline]
fn sin2pi(ph: f32) -> f32 {
    libm::sinf(ph * 2.0 * core::f32::consts::PI)
}

struct Track {
    l: Vec<f32>,
    r: Vec<f32>,
}

impl Track {
    fn new(secs: f32) -> Track {
        let n = (secs * SR) as usize;
        Track { l: vec![0.0; n], r: vec![0.0; n] }
    }
    #[inline]
    fn add(&mut self, i: usize, v: f32, pan: f32) {
        if i < self.l.len() {
            self.l[i] += v * (1.0 - pan.max(0.0));
            self.r[i] += v * (1.0 + pan.min(0.0));
        }
    }
    fn len(&self) -> usize {
        self.l.len()
    }
    /// A struck metal: inharmonic partials with their own decays.
    fn bell(&mut self, at: f32, parts: &[(f32, f32, f32)], pan: f32, amp: f32) {
        let s = (at * SR) as usize;
        let longest = parts.iter().map(|p| p.2).fold(0.0f32, f32::max) * 5.0;
        for k in 0..(longest * SR) as usize {
            let t = k as f32 / SR;
            let mut v = 0.0;
            for &(f, a, d) in parts {
                v += sin2pi(f * t) * a * libm::expf(-t / d);
            }
            let att = clamp01(t / 0.002);
            self.add(s + k, v * amp * att, pan);
        }
    }
    fn mixdown(&self, gain: f32) -> Vec<i16> {
        let mut out = Vec::with_capacity(self.len() * 2);
        for i in 0..self.len() {
            for v in [self.l[i], self.r[i]] {
                let x = v * gain;
                let y = x / (1.0 + libm::fabsf(x) * 0.6);
                out.push((y.clamp(-1.0, 1.0) * 30000.0) as i16);
            }
        }
        out
    }
}

/// The whole burn soundtrack, in step with the picture's timeline.
fn burn_track(frac: &[f32]) -> Vec<i16> {
    let mut tr = Track::new(T_END + 0.9);
    let n = tr.len();
    let mut rng = Rng(0x1357_9BDF);
    let fr = |t: f32| -> f32 {
        let i = (t * 100.0) as usize;
        if i + 1 < frac.len() {
            let f = t * 100.0 - i as f32;
            frac[i] + (frac[i + 1] - frac[i]) * f
        } else {
            0.0
        }
    };
    // 1. ignition: a rising whoosh and a low thump
    {
        let mut bp = Bq::default();
        let mut bp2 = Bq::default();
        let mut ph = 0.0f32;
        for i in 0..(1.0 * SR) as usize {
            let t = i as f32 / SR;
            if i % 32 == 0 {
                let fc = 220.0 * libm::powf(10.0, clamp01(t / 0.75));
                bp.bandpass(fc, 1.3);
                bp2.bandpass(fc * 1.6, 2.0);
            }
            let env = clamp01(t / 0.07) * libm::expf(-(t - 0.07).max(0.0) * 3.4);
            let x = rng.f();
            let v = (bp.run(x) * 1.4 + bp2.run(x) * 0.6) * env;
            ph += (78.0 - 40.0 * clamp01(t / 0.3)) / SR;
            let thump = sin2pi(ph) * libm::expf(-t * 10.0) * 0.6;
            tr.add(i, v * 0.9 + thump, -0.15 + t * 0.3);
        }
    }
    // 2. the fire: a roar that follows how much is burning, and crackles at a rate that does too
    {
        let mut brown = 0.0f32;
        let mut lp = Bq::default();
        lp.lowpass(650.0, 0.7);
        let mut lp2 = Bq::default();
        lp2.lowpass(180.0, 0.9);
        // (envelope now, decay per sample, pan, pop?, filter state)
        let mut clicks: Vec<(f32, f32, f32, bool, f32)> = Vec::new();
        let start = (0.1 * SR) as usize;
        let end = ((T_HYD + 0.9) * SR) as usize;
        for i in start..end.min(n) {
            let t = i as f32 / SR;
            let f = fr(t);
            let tail = if t > T_HYD { clamp01(1.0 - (t - T_HYD) / 0.9) } else { 1.0 };
            let w = rng.f();
            brown = (brown + w * 0.08) * 0.996;
            let roar = lp.run(brown) * 1.6 + lp2.run(w) * 0.5;
            let flick = 0.75 + 0.25 * vnoise(t * 7.0, 0.0, 41);
            let amp = (f * 0.85 + 0.05) * tail * flick;
            tr.add(i, roar * amp * 0.9, 0.12 * libm::sinf(t * 1.3));
            // new crackles
            let rate = (6.0 + 110.0 * f) * tail;
            if rng.u() < rate / SR && clicks.len() < 48 {
                let pop = rng.u() < 0.14;
                let a = (0.12 + 0.75 * rng.u() * rng.u()) * if pop { 1.4 } else { 1.0 };
                let tau = if pop { 0.012 } else { 0.0008 + 0.004 * rng.u() };
                clicks.push((a, libm::expf(-1.0 / (tau * SR)), rng.f() * 0.8, pop, 0.0));
            }
            let mut v_sum = (0.0f32, 0.0f32);
            for c in clicks.iter_mut() {
                let x = rng.f();
                let v = if c.3 {
                    c.4 += (x - c.4) * 0.08;
                    c.4 * 3.0
                } else {
                    let hp = x - c.4;
                    c.4 += (x - c.4) * 0.3;
                    hp
                };
                let e = c.0;
                c.0 *= c.1;
                v_sum.0 += v * e * (1.0 - c.2.max(0.0));
                v_sum.1 += v * e * (1.0 + c.2.min(0.0));
            }
            if i % 128 == 0 {
                clicks.retain(|c| c.0 > 0.002);
            }
            if i < n {
                tr.l[i] += v_sum.0 * 0.8;
                tr.r[i] += v_sum.1 * 0.8;
            }
        }
    }
    // 3. the hydrant: a metal clank as it lands, the valve creaking open, then a hiss
    tr.bell(T_HYD + 0.04, &[(612.0, 0.6, 0.22), (1423.0, 0.32, 0.12), (2210.0, 0.22, 0.07), (3170.0, 0.14, 0.04), (190.0, 0.4, 0.06)], 0.0, 1.6);
    {
        let s = (3.86 * SR) as usize;
        let mut ph = 0.0f32;
        let mut lp = Bq::default();
        lp.lowpass(1400.0, 3.0);
        for k in 0..(0.34 * SR) as usize {
            let t = k as f32 / SR;
            ph += (170.0 + 110.0 * t / 0.34 + 14.0 * libm::sinf(t * 80.0)) / SR;
            let saw = (ph - libm::floorf(ph)) * 2.0 - 1.0;
            let env = libm::sinf(core::f32::consts::PI * t / 0.34);
            tr.add(s + k, lp.run(saw) * env * 0.22, 0.2);
        }
        let s = (3.96 * SR) as usize;
        let mut lo = 0.0f32;
        for k in 0..(0.75 * SR) as usize {
            let t = k as f32 / SR;
            let x = rng.f();
            lo += (x - lo) * 0.35;
            let env = clamp01(t / 0.05) * (1.0 - 0.7 * clamp01(t / 0.75));
            tr.add(s + k, (x - lo) * env * 0.42, 0.35);
        }
    }
    // 4. the water: a splash as it starts, a gush that sloshes, bubbles rising in pitch as it fills
    {
        let s = (T_WATER * SR) as usize;
        let mut bp = Bq::default();
        bp.bandpass(1400.0, 0.7);
        let mut ph = 0.0f32;
        for k in 0..(0.9 * SR) as usize {
            let t = k as f32 / SR;
            let env = clamp01(t / 0.004) * libm::expf(-t * 6.5);
            ph += (115.0 - 55.0 * clamp01(t / 0.25)) / SR;
            tr.add(s + k, bp.run(rng.f()) * env * 1.5 + sin2pi(ph) * libm::expf(-t * 9.0) * 0.45, -0.1);
        }
        let mut lp = Bq::default();
        lp.lowpass(950.0, 0.6);
        let mut lp2 = Bq::default();
        lp2.lowpass(140.0, 0.8);
        // it quietens as the window fills, leaving the chime clear
        let dur = T_FADE - T_WATER;
        for k in 0..(dur * SR) as usize {
            let t = k as f32 / SR;
            let env = clamp01(t / 0.12) * clamp01((dur - t) / 0.35);
            let slosh = 0.72 + 0.28 * libm::sinf(2.0 * core::f32::consts::PI * 3.3 * t + 0.7 * libm::sinf(2.0 * core::f32::consts::PI * 0.7 * t));
            let x = rng.f();
            let v = lp.run(x) * 0.9 + lp2.run(x) * 1.2;
            tr.add(s + k, v * env * slosh * 0.75, 0.15 * libm::sinf(t * 2.1));
        }
        // bubbles
        let mut at = T_WATER + 0.08;
        while at < T_FADE {
            let fill = clamp01((at - T_WATER) / (T_FADE - T_WATER));
            let f0 = (260.0 + 760.0 * fill) * (0.8 + 0.45 * rng.u());
            let d = 0.045 + 0.07 * rng.u();
            let pan = rng.f() * 0.7;
            let amp = 0.12 + 0.16 * rng.u();
            let s = (at * SR) as usize;
            let mut ph = 0.0f32;
            for k in 0..(d * SR) as usize {
                let tau = k as f32 / SR;
                ph += f0 * (1.0 + 1.9 * tau / d) / SR;
                let e = libm::sinf(core::f32::consts::PI * tau / d);
                tr.add(s + k, sin2pi(ph) * e * e * amp, pan);
            }
            at += 0.025 + 0.075 * rng.u();
        }
    }
    // 5. a soft chime as the window comes back
    tr.bell(T_FADE + 0.05, &[(1318.5, 0.5, 0.5), (2637.0, 0.16, 0.25), (3955.5, 0.07, 0.12)], -0.2, 0.8);
    tr.bell(T_FADE + 0.24, &[(1760.0, 0.45, 0.55), (3520.0, 0.12, 0.25), (5280.0, 0.05, 0.1)], 0.2, 0.7);
    tr.mixdown(0.85)
}

// ── Files' small sounds ──────────────────────────────────────────────────────

static mut CACHED: [Option<usize>; 6] = [None; 6];
const SOUNDS: [&str; 6] = ["drop", "trash", "restore", "delete", "copy", "error"];

fn ui_sound(kind: usize) -> Vec<i16> {
    let mut rng = Rng(0xACE1_2345 ^ kind as u32);
    let mut tr = Track::new(0.4);
    match kind {
        // drop: a soft thud
        0 => {
            let mut ph = 0.0f32;
            for k in 0..(0.14 * SR) as usize {
                let t = k as f32 / SR;
                ph += (170.0 - 80.0 * clamp01(t / 0.09)) / SR;
                tr.add(k, sin2pi(ph) * libm::expf(-t * 30.0) * 0.8 + rng.f() * libm::expf(-t * 300.0) * 0.2, 0.0);
            }
        },
        // trash: a paper crumple and a short whoosh down
        1 => {
            let mut bp = Bq::default();
            bp.bandpass(3000.0, 1.2);
            let mut at = 0.0;
            while at < 0.16 {
                let s = (at * SR) as usize;
                let a = 0.3 + 0.5 * rng.u();
                for k in 0..(0.012 * SR) as usize {
                    let t = k as f32 / SR;
                    tr.add(s + k, bp.run(rng.f()) * libm::expf(-t * 400.0) * a * 2.0, rng.f() * 0.3);
                }
                at += 0.012 + 0.025 * rng.u();
            }
            let mut bw = Bq::default();
            for k in 0..(0.22 * SR) as usize {
                let t = k as f32 / SR;
                if k % 32 == 0 {
                    bw.bandpass(1800.0 - 1300.0 * t / 0.22, 1.5);
                }
                tr.add(k + (0.05 * SR) as usize, bw.run(rng.f()) * libm::sinf(core::f32::consts::PI * t / 0.22) * 0.6, 0.0);
            }
        },
        // restore: two rising notes
        2 => {
            for (j, f) in [784.0f32, 1174.7].iter().enumerate() {
                let s = (j as f32 * 0.075 * SR) as usize;
                for k in 0..(0.2 * SR) as usize {
                    let t = k as f32 / SR;
                    tr.add(s + k, (sin2pi(f * t) + 0.25 * sin2pi(2.0 * f * t)) * clamp01(t / 0.005) * libm::expf(-t * 14.0) * 0.4, 0.0);
                }
            }
        },
        // delete: a falling "fft"
        3 => {
            let mut bp = Bq::default();
            for k in 0..(0.18 * SR) as usize {
                let t = k as f32 / SR;
                if k % 32 == 0 {
                    bp.bandpass(2400.0 - 1800.0 * t / 0.18, 1.4);
                }
                tr.add(k, bp.run(rng.f()) * clamp01(t / 0.01) * libm::expf(-t * 14.0) * 1.2, 0.0);
            }
        },
        // copy: two light ticks
        4 => {
            for j in 0..2 {
                let s = (j as f32 * 0.06 * SR) as usize;
                for k in 0..(0.05 * SR) as usize {
                    let t = k as f32 / SR;
                    tr.add(s + k, sin2pi(1568.0 * t) * libm::expf(-t * 70.0) * 0.4, 0.0);
                }
            }
        },
        // error: a low double buzz
        _ => {
            for j in 0..2 {
                let s = (j as f32 * 0.11 * SR) as usize;
                for k in 0..(0.08 * SR) as usize {
                    let t = k as f32 / SR;
                    let sq = if sin2pi(220.0 * t) > 0.0 { 1.0 } else { -1.0 };
                    tr.add(s + k, sq * libm::sinf(core::f32::consts::PI * t / 0.08) * 0.18, 0.0);
                }
            }
        },
    }
    tr.mixdown(1.0)
}

fn play_ui(name: &str) -> bool {
    if muted() {
        return false;
    }
    let Some(k) = SOUNDS.iter().position(|s| *s == name) else { return false };
    let cache = unsafe { &mut *(&raw mut CACHED) };
    let id = match cache[k] {
        Some(id) => id,
        None => {
            let pcm = ui_sound(k);
            let id = crate::drivers::sfx::stream_open(SR as u32, true);
            crate::drivers::sfx::stream_push(id, &pcm);
            crate::drivers::sfx::stream_close(id);
            cache[k] = Some(id);
            id
        },
    };
    crate::drivers::sfx::play(id, 0.8, 0.0, false, 1.0, false).is_some()
}

fn muted() -> bool {
    unsafe { MUTED }
}

pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    let n = |i: usize| crate::fsx::narg(args, i, 0.0);
    Some(match name {
        b"fsx_burn_start" => boolean(start(n(0) as i32, n(1) as i32, n(2).max(0.0) as usize, n(3).max(0.0) as usize)),
        b"fsx_burn_frame" => num(frame() as f64),
        b"fsx_burn_active" => boolean(burn().is_some()),
        b"fsx_sound" => boolean(play_ui(&crate::fsx::sarg(args, 0))),
        b"fsx_mute" => {
            unsafe { MUTED = args.first().map(|&v| crate::lingrt::truthy(v)).unwrap_or(true) };
            UNIT
        },
        b"fsx_muted" => boolean(muted()),
        _ => return None,
    })
}
