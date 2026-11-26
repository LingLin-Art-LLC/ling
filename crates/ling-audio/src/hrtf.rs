// crates/ling-audio/src/hrtf.rs — parametric binaural (HRTF) spatializer
//
// A structural head-related transfer function after Brown & Duda (1998,
// "A structural model for binaural sound synthesis"). No measured HRIR data
// set is needed — every cue is computed from the source direction:
//
//   - ITD  — interaural time difference (Woodworth's spherical-head formula),
//            realised as a fractional delay per ear (up to ≈0.66 ms).
//   - ILD  — head shadow: a one-pole/one-zero shelf per ear whose high-
//            frequency gain α(θ) falls as the ear turns away from the source.
//   - Pinna — two short echoes whose delays depend on elevation, carving the
//            spectral notches the brain reads as "up" / "down".
//   - Rear — a gentle low-pass when the source is behind the head, so
//            front/back confusion is reduced.
//   - Air  — distance low-pass (far sources lose their top end) plus the
//            engine's usual 1/(1 + 0.18·d) attenuation.
//
// Coordinates are listener (camera) space as produced by the engine:
// +x right, +y DOWN (Ling's screen convention), +z forward.
//
// Geometry → filter targets is recomputed at control rate (every CTL
// samples); the per-sample path only glides the targets (no zipper noise —
// a moving source's ITD glide even gives a natural hint of Doppler).

use std::f32::consts::PI;

const HLEN: usize = 256; // input history (power of two): ITD + pinna at ≤192 kHz
const HMASK: usize = HLEN - 1;
const CTL: u32 = 16; // samples between geometry updates

const HEAD_A: f32 = 0.0875; // head radius (m)
const SOUND_C: f32 = 343.0; // speed of sound (m/s)
const ALPHA_MIN: f32 = 0.1; // head-shadow floor (Brown & Duda)
const THETA_MIN: f32 = 150.0 * PI / 180.0;

/// Pinna echoes: (reflection coefficient, A, B, D) — delays in samples at
/// 44.1 kHz: τ = A·cos(θ/2)·sin(D·(90° − φ)) + B.
const PINNA: [(f32, f32, f32, f32); 2] = [(0.45, 1.0, 2.0, 1.0), (-0.4, 5.0, 4.0, 0.5)];

/// Per-voice binaural state. Cheap (≈1 KB) and allocation-free.
#[derive(Clone)]
pub(crate) struct Binaural {
    hist: [f32; HLEN],
    idx: usize,
    ctl: u32,
    primed: bool,
    // current (smoothed) parameters
    d_l: f32, // left-ear delay, samples
    d_r: f32,
    a_l: f32, // head-shadow α per ear
    a_r: f32,
    p: [f32; 2], // pinna echo delays, samples
    rear: f32,   // 0..1 rear darkening
    gain: f32,
    air: f32, // air-absorption one-pole coefficient
    // targets
    t_d_l: f32,
    t_d_r: f32,
    t_a_l: f32,
    t_a_r: f32,
    t_p: [f32; 2],
    t_rear: f32,
    t_gain: f32,
    t_air: f32,
    // filter memories
    air_s: f32,
    hs_l: (f32, f32), // (x1, y1)
    hs_r: (f32, f32),
    rl: f32,
    rr: f32,
}

impl Default for Binaural {
    fn default() -> Self {
        Self {
            hist: [0.0; HLEN],
            idx: 0,
            ctl: 0,
            primed: false,
            d_l: 0.0,
            d_r: 0.0,
            a_l: 1.0,
            a_r: 1.0,
            p: [2.0, 4.0],
            rear: 0.0,
            gain: 0.0,
            air: 1.0,
            t_d_l: 0.0,
            t_d_r: 0.0,
            t_a_l: 1.0,
            t_a_r: 1.0,
            t_p: [2.0, 4.0],
            t_rear: 0.0,
            t_gain: 0.0,
            t_air: 1.0,
            air_s: 0.0,
            hs_l: (0.0, 0.0),
            hs_r: (0.0, 0.0),
            rl: 0.0,
            rr: 0.0,
        }
    }
}

/// Head-shadow high-frequency gain for an ear at angle `theta` (radians)
/// from the source direction.
#[inline]
fn shadow_alpha(theta: f32) -> f32 {
    (1.0 + ALPHA_MIN * 0.5) + (1.0 - ALPHA_MIN * 0.5) * (theta / THETA_MIN * PI).cos()
}

impl Binaural {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Recompute the filter targets from a listener-space position.
    fn retarget(&mut self, x: f32, y: f32, z: f32, sr: f32) {
        let dist = (x * x + y * y + z * z).sqrt();
        let de = dist.max(0.5); // sources inside the head collapse toward centre
        let (ux, uy, uz) = (x / de, -y / de, z / de); // uy: +up
        // ── ITD (Woodworth): lateral angle from the median plane ──
        let lat = ux.clamp(-1.0, 1.0).asin();
        let itd = HEAD_A / SOUND_C * (lat.abs() + lat.abs().sin()) * sr;
        // the far ear hears it later
        if lat >= 0.0 {
            self.t_d_l = itd;
            self.t_d_r = 0.0;
        } else {
            self.t_d_l = 0.0;
            self.t_d_r = itd;
        }
        // ── head shadow: angle between source and each ear axis (±x) ──
        self.t_a_r = shadow_alpha(ux.clamp(-1.0, 1.0).acos());
        self.t_a_l = shadow_alpha((-ux).clamp(-1.0, 1.0).acos());
        // ── pinna: elevation notches ──
        let az = ux.atan2(uz.abs().max(1e-4)); // front-mirrored azimuth, ±90°
        let el = uy.clamp(-1.0, 1.0).asin(); // +up
        let k = sr / 44_100.0;
        for (i, &(_, a, b, d)) in PINNA.iter().enumerate() {
            let tau = a * (az * 0.5).cos() * (d * (PI * 0.5 - el)).sin() + b;
            self.t_p[i] = tau.max(0.5) * k;
        }
        // ── behind the head: darker ──
        self.t_rear = (-uz).clamp(0.0, 1.0) * 0.55;
        // ── distance: level + air absorption ──
        self.t_gain = (1.0 / (1.0 + dist.max(0.5) * 0.18)).clamp(0.0, 1.0) * 0.8;
        let fc = 18_000.0 / (1.0 + dist * 0.06);
        self.t_air = 1.0 - (-2.0 * PI * fc.min(sr * 0.45) / sr).exp();
        if !self.primed {
            self.primed = true;
            self.d_l = self.t_d_l;
            self.d_r = self.t_d_r;
            self.a_l = self.t_a_l;
            self.a_r = self.t_a_r;
            self.p = self.t_p;
            self.rear = self.t_rear;
            self.gain = self.t_gain;
            self.air = self.t_air;
        }
    }

    /// Fractional-delay read from the input history (`d` samples ago).
    #[inline]
    fn tap(&self, d: f32) -> f32 {
        let d = d.clamp(0.0, (HLEN - 2) as f32);
        let di = d as usize;
        let fr = d - di as f32;
        let a = self.hist[(self.idx + HLEN - di) & HMASK];
        let b = self.hist[(self.idx + HLEN - di - 1) & HMASK];
        a + (b - a) * fr
    }

    /// Spatialize one mono sample at listener-space (x, y, z) → (L, R).
    #[inline]
    pub(crate) fn process(&mut self, s: f32, x: f32, y: f32, z: f32, sr: f32) -> (f32, f32) {
        if self.ctl == 0 || !self.primed {
            self.retarget(x, y, z, sr);
        }
        self.ctl = (self.ctl + 1) % CTL;
        // ~8 ms parameter glide
        let g = 1.0 - (-1.0 / (0.008 * sr)).exp();
        self.d_l += (self.t_d_l - self.d_l) * g;
        self.d_r += (self.t_d_r - self.d_r) * g;
        self.a_l += (self.t_a_l - self.a_l) * g;
        self.a_r += (self.t_a_r - self.a_r) * g;
        self.p[0] += (self.t_p[0] - self.p[0]) * g;
        self.p[1] += (self.t_p[1] - self.p[1]) * g;
        self.rear += (self.t_rear - self.rear) * g;
        self.gain += (self.t_gain - self.gain) * g;
        self.air += (self.t_air - self.air) * g;

        // air absorption, then into the history
        self.air_s += (s - self.air_s) * self.air;
        self.idx = (self.idx + 1) & HMASK;
        self.hist[self.idx] = self.air_s;

        // each ear: delayed direct + pinna echoes
        let ear = |me: &Self, d: f32| {
            let mut v = me.tap(d);
            for (i, &(rho, ..)) in PINNA.iter().enumerate() {
                v += rho * me.tap(d + me.p[i]);
            }
            v
        };
        let xl = ear(self, self.d_l);
        let xr = ear(self, self.d_r);

        // head shadow: H(s) = (α s + β)/(s + β), β = 2c/a, bilinear transform
        let beta = 2.0 * SOUND_C / HEAD_A;
        let kk = 2.0 * sr;
        let inv = 1.0 / (kk + beta);
        let a1 = (beta - kk) * inv;
        let shelf = |st: &mut (f32, f32), x: f32, alpha: f32| {
            let y = (alpha * kk + beta) * inv * x + (beta - alpha * kk) * inv * st.0 - a1 * st.1;
            *st = (x, y);
            y
        };
        let yl = shelf(&mut self.hs_l, xl, self.a_l);
        let yr = shelf(&mut self.hs_r, xr, self.a_r);

        // rear darkening (~3.5 kHz one-pole, blended)
        let rc = 1.0 - (-2.0 * PI * 3_500.0 / sr).exp();
        self.rl += (yl - self.rl) * rc;
        self.rr += (yr - self.rr) * rc;
        let ol = yl + (self.rl - yl) * self.rear;
        let or = yr + (self.rr - yr) * self.rear;
        (ol * self.gain, or * self.gain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    /// Render `n` samples of a sine at (x, y, z); returns (L energy, R energy,
    /// first-arrival sample index per ear).
    fn render(freq: f32, x: f32, y: f32, z: f32, n: usize) -> (f32, f32, usize, usize) {
        let mut b = Binaural::new();
        let (mut el, mut er) = (0.0, 0.0);
        let (mut fl, mut fr) = (usize::MAX, usize::MAX);
        for i in 0..n {
            let s = if freq > 0.0 {
                (i as f32 * freq / SR * 2.0 * PI).sin()
            } else if i == 0 {
                1.0 // impulse
            } else {
                0.0
            };
            let (l, r) = b.process(s, x, y, z, SR);
            if l.abs() > 1e-4 && fl == usize::MAX {
                fl = i;
            }
            if r.abs() > 1e-4 && fr == usize::MAX {
                fr = i;
            }
            if i > n / 4 {
                el += l * l;
                er += r * r;
            }
        }
        (el, er, fl, fr)
    }

    #[test]
    fn source_on_right_is_louder_and_earlier_on_right() {
        let (_, _, fl, fr) = render(0.0, 3.0, 0.0, 0.0, 400);
        assert!(fr < fl, "right ear must hear it first (fl={fl}, fr={fr})");
        assert!(fl - fr >= 20, "ITD at 90° ≈ 0.66 ms ≈ 31 samples, got {}", fl - fr);
        let (el, er, ..) = render(4_000.0, 3.0, 0.0, 0.0, 9_600);
        assert!(er > el * 4.0, "high frequencies are shadowed at the far ear: L={el} R={er}");
    }

    #[test]
    fn source_in_front_is_balanced() {
        let (el, er, fl, fr) = render(1_000.0, 0.0, 0.0, 3.0, 9_600);
        assert_eq!(fl, fr);
        assert!((el / er - 1.0).abs() < 0.01, "L={el} R={er}");
    }

    #[test]
    fn behind_is_darker_than_front() {
        let (fl, fr, ..) = render(6_000.0, 0.0, 0.0, 3.0, 9_600);
        let (bl, br, ..) = render(6_000.0, 0.0, 0.0, -3.0, 9_600);
        assert!(bl + br < (fl + fr) * 0.8, "front={} back={}", fl + fr, bl + br);
    }

    #[test]
    fn elevation_changes_the_spectrum() {
        // the pinna notch moves with elevation → different high-band energy
        let (ul, ur, ..) = render(8_000.0, 0.0, -3.0, 0.5, 9_600); // up (y is down)
        let (dl, dr, ..) = render(8_000.0, 0.0, 3.0, 0.5, 9_600);
        let (u, d) = (ul + ur, dl + dr);
        assert!((u / d - 1.0).abs() > 0.05, "up={u} down={d}");
    }
}
