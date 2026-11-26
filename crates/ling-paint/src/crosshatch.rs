//! Keyed crosshatching: tonal hatching whose every irregularity (line wobble, breaks, watermark bits) comes from a
//! cryptographic seed, so the same key + settings always draw the same hatching and nobody else can reproduce it.
//!
//! Seeds (both start from the passphrase through Argon2id, cached per passphrase):
//! - SHAKE-256: seed = SHAKE-256(domain || key || M).
//! - ML-DSA-87 (FIPS 204): the key seeds an ML-DSA-87 key pair, M is signed with the deterministic variant, and
//!   seed = SHAKE-256(domain || signature). The public key + signature + M make a record anyone can verify.
//!
//! M = domain || canvas size || the geometry settings (spacing, angle, directions, wobble) || watermark text, so the
//! key holder can always regenerate the hatching later; it does not cover the pixels.
//!
//! Watermark (optional): a 128-bit tag (SHAKE-256 of the signer's identity and the watermark text) is spread over
//! the hatch segments. Each segment carries one bit, chosen with a keyed Blake3 PRF, as a small sideways shift of its
//! line (spread-spectrum, every bit repeated many times). `verify` re-derives the geometry and correlates the ink on
//! the layer against both shift positions to read the bits back.

#[allow(unused_imports)]
use crate::prelude::*;
use crate::px::*;
use ling_crypto::{Argon2idParams, Blake3, MlDsa87Keypair, Shake256};

pub const SEED_NAMES: [&str; 2] = ["SHAKE-256 hash seed", "ML-DSA-87 signature seed"];
pub const INK_NAMES: [&str; 3] = ["Ink on transparent", "Ink on white paper", "Ink over the picture"];
const DIR_ANGLES: [f32; 4] = [0.0, 90.0, 45.0, -45.0];
const PAYLOAD_BITS: usize = 128;

/// Parameter order (matches `filters::FILTERS`): spacing, angle, directions, line weight, wobble, contrast,
/// seed source, watermark, ink.
pub struct Geo {
    pub spacing: f32,
    pub angle: f32,
    pub dirs: usize,
    pub weight: f32,
    pub wobble: f32,
    pub contrast: f32,
    pub seed_src: u8,
    pub watermark: bool,
    pub ink: u8,
}
impl Geo {
    pub fn from(p: &[f32]) -> Geo {
        let pa = |i: usize, d: f32| p.get(i).copied().unwrap_or(d);
        Geo {
            spacing: pa(0, 6.0).max(2.0),
            angle: pa(1, 45.0),
            dirs: (pa(2, 3.0).round() as usize).clamp(1, 4),
            weight: pa(3, 0.35).clamp(0.02, 1.0),
            wobble: pa(4, 0.3).clamp(0.0, 1.0),
            contrast: pa(5, 1.0),
            seed_src: pa(6, 1.0) as u8,
            watermark: pa(7, 1.0) > 0.5,
            ink: pa(8, 0.0) as u8,
        }
    }
    fn seg(&self) -> f32 {
        (self.spacing * 4.0).max(16.0)
    }
    /// watermark shift, in px
    fn amp(&self) -> f32 {
        self.spacing * 0.2
    }
}

/// Everything derived from the key for one set of settings.
#[derive(Clone)]
pub struct Keyed {
    pub seed: [u8; 32],
    pub payload: [u8; 16],
    /// short public id of the signer (ML-DSA: of the public key; hash mode: of the derived key)
    pub fingerprint: [u8; 8],
    /// ML-DSA-87 only: public key, signature, signed message
    pub pk: Vec<u8>,
    pub sig: Vec<u8>,
    pub msg: Vec<u8>,
}

/// Passphrase / watermark text and caches. The passphrase stays in memory only; it is never saved.
#[derive(Default)]
pub struct HatchKeys {
    pass: String,
    text: String,
    master: Option<[u8; 32]>,
    cache: Option<(Vec<u8>, Keyed)>,
}

fn shake(parts: &[&[u8]], n: usize) -> Vec<u8> {
    let mut v = Vec::new();
    for p in parts {
        // length-prefixed so different splits of the same bytes never collide
        v.extend_from_slice(&(p.len() as u32).to_le_bytes());
        v.extend_from_slice(p);
    }
    Shake256::hash(&v, n)
}
fn arr<const N: usize>(v: &[u8]) -> [u8; N] {
    let mut a = [0u8; N];
    a.copy_from_slice(&v[..N]);
    a
}
pub fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(char::from_digit((x >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((x & 15) as u32, 16).unwrap_or('0'));
    }
    s
}

impl HatchKeys {
    pub fn set(&mut self, pass: &str, text: &str) {
        if pass != self.pass {
            self.master = None;
        }
        if pass != self.pass || text != self.text {
            self.cache = None;
        }
        self.pass = pass.to_string();
        self.text = text.to_string();
    }
    pub fn has_pass(&self) -> bool {
        !self.pass.is_empty()
    }
    /// Argon2id(passphrase) - slow on purpose (about a third of a second), so it runs once per passphrase.
    fn master(&mut self) -> Result<[u8; 32], String> {
        if let Some(m) = self.master {
            return Ok(m);
        }
        let kdf = Argon2idParams { m_cost: 19456, t_cost: 2, p_cost: 1 };
        let k = kdf.derive_key(self.pass.as_bytes(), b"LingAnime crosshatch key v1", 32).map_err(|e| e.to_string())?;
        let m = arr::<32>(&k);
        self.master = Some(m);
        Ok(m)
    }
    /// Derive (or reuse) the seed, watermark payload and, for ML-DSA-87, the signature for these settings.
    pub fn derive(&mut self, g: &Geo, w: u32, h: u32) -> Result<Keyed, String> {
        let mut msg = Vec::new();
        msg.extend_from_slice(b"LingAnime crosshatch v1\0");
        msg.extend_from_slice(&w.to_le_bytes());
        msg.extend_from_slice(&h.to_le_bytes());
        for v in [g.spacing, g.angle, g.dirs as f32, g.wobble] {
            msg.extend_from_slice(&v.to_le_bytes());
        }
        msg.extend_from_slice(&(self.text.len() as u32).to_le_bytes());
        msg.extend_from_slice(self.text.as_bytes());
        let mut ck = msg.clone();
        ck.push(g.seed_src);
        if let Some((k, v)) = &self.cache {
            if *k == ck {
                return Ok(v.clone());
            }
        }
        let master = self.master()?;
        let text = self.text.clone();
        let keyed = if g.seed_src == 1 {
            let xi = arr::<32>(&shake(&[b"LA-xh mldsa87 keygen", &master], 32));
            let kp = MlDsa87Keypair::from_seed(xi);
            let pk = kp.public_key();
            let sig = kp.sign(&msg);
            Keyed {
                seed: arr(&shake(&[b"LA-xh sig seed", &sig], 32)),
                payload: arr(&shake(&[b"LA-xh watermark", &pk, text.as_bytes()], 16)),
                fingerprint: arr(&shake(&[b"LA-xh fingerprint", &pk], 8)),
                pk,
                sig,
                msg,
            }
        } else {
            let id = shake(&[b"LA-xh hash id", &master], 32);
            Keyed {
                seed: arr(&shake(&[b"LA-xh hash seed", &master, &msg], 32)),
                payload: arr(&shake(&[b"LA-xh watermark", &id, text.as_bytes()], 16)),
                fingerprint: arr(&shake(&[b"LA-xh fingerprint", &id], 8)),
                pk: vec![],
                sig: vec![],
                msg,
            }
        };
        self.cache = Some((ck, keyed.clone()));
        Ok(keyed)
    }
}

/// Check an exported record: does `sig` verify for `msg` under `pk` (ML-DSA-87)?
pub fn verify_signature(pk: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    MlDsa87Keypair::verify(pk, msg, sig).is_ok()
}

// ── geometry ─────────────────────────────────────────────────────────────────
/// Per-direction knot table: one entry per (line, segment) covering a rectangle.
struct Knots {
    ca: f32,
    sa: f32,
    l0: i32,
    s0: i32,
    nl: usize,
    ns: usize,
    /// (line offset in px including the watermark shift, gap flag, bit index, chip)
    k: Vec<(f32, bool, u8, i8)>,
}
impl Knots {
    fn build(g: &Geo, key: &Keyed, dir: usize, rect: [i32; 4]) -> Knots {
        let a = (g.angle + DIR_ANGLES[dir]).to_radians();
        let (sa, ca) = a.sin_cos();
        let (s, seg) = (g.spacing, g.seg());
        let (mut umin, mut umax, mut vmin, mut vmax) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for (x, y) in [(rect[0], rect[1]), (rect[2], rect[1]), (rect[0], rect[3]), (rect[2], rect[3])] {
            let (x, y) = (x as f32, y as f32);
            let u = -x * sa + y * ca;
            let v = x * ca + y * sa;
            umin = umin.min(u);
            umax = umax.max(u);
            vmin = vmin.min(v);
            vmax = vmax.max(v);
        }
        let l0 = (umin / s).floor() as i32 - 2;
        let l1 = (umax / s).ceil() as i32 + 2;
        let s0 = (vmin / seg).floor() as i32 - 1;
        let s1 = (vmax / seg).ceil() as i32 + 2;
        let nl = (l1 - l0 + 1).max(1) as usize;
        let ns = (s1 - s0 + 1).max(1) as usize;
        let prf_key = arr::<32>(&shake(&[b"LA-xh prf", &key.seed], 32));
        let mut k = Vec::with_capacity(nl * ns);
        let gap_p = g.wobble * 0.15;
        let amp = g.amp();
        let mut buf = [0u8; 9];
        buf[0] = dir as u8;
        for li in 0..nl {
            let l = l0 + li as i32;
            buf[1..5].copy_from_slice(&l.to_le_bytes());
            for si in 0..ns {
                buf[5..9].copy_from_slice(&(s0 + si as i32).to_le_bytes());
                let r = Blake3::keyed_hash(&prf_key, &buf);
                let u = |i: usize| u16::from_le_bytes([r[i], r[i + 1]]) as f32 / 65535.0;
                let wob = (u(0) * 2.0 - 1.0) * g.wobble * 0.3 * s;
                let gap = u(2) < gap_p;
                let bit = r[4] % PAYLOAD_BITS as u8;
                let chip: i8 = if r[5] & 1 == 0 { 1 } else { -1 };
                let mut off = wob;
                if g.watermark {
                    let b = (key.payload[bit as usize / 8] >> (bit % 8)) & 1;
                    off += amp * chip as f32 * if b == 1 { 1.0 } else { -1.0 };
                }
                k.push((off, gap, bit, chip));
            }
        }
        Knots { ca, sa, l0, s0, nl, ns, k }
    }
    #[inline]
    fn at(&self, l: i32, s: i32) -> (f32, bool, u8, i8) {
        let li = (l - self.l0).clamp(0, self.nl as i32 - 1) as usize;
        let si = (s - self.s0).clamp(0, self.ns as i32 - 1) as usize;
        self.k[li * self.ns + si]
    }
    /// line-local coordinates of a canvas point
    #[inline]
    fn uv(&self, x: f32, y: f32) -> (f32, f32) {
        (-x * self.sa + y * self.ca, x * self.ca + y * self.sa)
    }
    /// offset of line `l` at position `v` along it, and whether `v` falls in a break
    #[inline]
    fn line(&self, l: i32, v: f32, seg: f32) -> (f32, bool) {
        let t = v / seg;
        let s = t.floor();
        let f = t - s;
        let a = self.at(l, s as i32);
        let b = self.at(l, s as i32 + 1);
        let e = f * f * (3.0 - 2.0 * f);
        (a.0 + (b.0 - a.0) * e, a.1 && f > 0.3 && f < 0.55)
    }
}

fn darkness(c: [f32; 4]) -> f32 {
    // premultiplied pixel composited over white paper
    1.0 - lum(1.0 - c[3] + c[0], 1.0 - c[3] + c[1], 1.0 - c[3] + c[2])
}

/// Render the hatching into `reg` (premultiplied), `dark` = tone (0 white .. 1 black) per pixel of `reg`.
pub fn render(g: &Geo, key: &Keyed, reg: &mut crate::filters::Region, dark: &[f32], fg: [f32; 3]) {
    let (w, h) = (reg.w, reg.h);
    let rect = [reg.x0, reg.y0, reg.x0 + w as i32, reg.y0 + h as i32];
    let knots: Vec<Knots> = (0..g.dirs).map(|d| Knots::build(g, key, d, rect)).collect();
    let (s, seg) = (g.spacing, g.seg());
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let dk = dark[i];
            let gx = (reg.x0 + x as i32) as f32 + 0.5;
            let gy = (reg.y0 + y as i32) as f32 + 0.5;
            let mut cov = 0.0f32;
            for (d, kn) in knots.iter().enumerate() {
                // each extra direction comes in at a darker tone
                let t = d as f32 / g.dirs as f32 * 0.8 + 0.04;
                let wk = smoothstep(t, t + 0.22, dk);
                if wk <= 0.0 {
                    continue;
                }
                let hw = 0.5 * g.weight * s * (0.35 + 0.65 * wk);
                let (u, v) = kn.uv(gx, gy);
                let l = (u / s).round() as i32;
                for ll in l - 1..=l + 1 {
                    let (off, gap) = kn.line(ll, v, seg);
                    if gap {
                        continue;
                    }
                    let dist = (u - (ll as f32 * s + off)).abs();
                    let c = (hw + 0.5 - dist).clamp(0.0, 1.0) * wk.min(1.0).max(0.6);
                    cov = cov + c - cov * c;
                }
            }
            let o = reg.px[i];
            let ink = [fg[0] * cov, fg[1] * cov, fg[2] * cov, cov];
            reg.px[i] = match g.ink {
                1 => [1.0 + (fg[0] - 1.0) * cov, 1.0 + (fg[1] - 1.0) * cov, 1.0 + (fg[2] - 1.0) * cov, 1.0],
                2 => over(o, ink),
                _ => ink,
            };
        }
    }
}

/// Tone map for a region: darkness with contrast, smoothed over about one hatch spacing.
pub fn tone(g: &Geo, reg: &crate::filters::Region) -> Vec<f32> {
    let mut d: Vec<f32> = reg.px.iter().map(|&c| ((darkness(c) - 0.5) * g.contrast + 0.5).clamp(0.0, 1.0)).collect();
    let r = (g.spacing * 0.5) as usize;
    if r > 0 {
        crate::filters::box1(&mut d, reg.w, reg.h, r, 1, 0);
    }
    d
}

/// Read the watermark back from `t` inside `rect`.
/// Returns (bits that agree with the expected payload, bits with any evidence, mean |correlation|).
pub fn verify(g: &Geo, key: &Keyed, t: &Tiles, rect: [i32; 4]) -> (usize, usize, f32) {
    let mut acc = [0.0f32; PAYLOAD_BITS];
    let mut hits = [0u32; PAYLOAD_BITS];
    let (s, seg, amp) = (g.spacing, g.seg(), g.amp());
    let probe = |x: f32, y: f32| -> f32 {
        let (xi, yi) = (x.floor() as i32, y.floor() as i32);
        if xi < rect[0] || yi < rect[1] || xi >= rect[2] || yi >= rect[3] {
            return f32::NAN;
        }
        darkness(t.getf(xi, yi))
    };
    for d in 0..g.dirs {
        let kn = Knots::build(g, key, d, rect);
        for li in 0..kn.nl {
            let l = kn.l0 + li as i32;
            for si in 0..kn.ns {
                let sk = kn.s0 + si as i32;
                let (off, _, bit, chip) = kn.at(l, sk);
                // the knot's own watermark shift is unknown to the reader: take it back out
                let base = if g.watermark {
                    let b = (key.payload[bit as usize / 8] >> (bit % 8)) & 1;
                    off - amp * chip as f32 * if b == 1 { 1.0 } else { -1.0 }
                } else {
                    off
                };
                let mut vote = 0.0f32;
                let mut n = 0;
                for j in -2..=2 {
                    let v = sk as f32 * seg + j as f32 * seg * 0.07;
                    let (lo, _) = kn.line(l, v, seg);
                    // keep only the wobble part of the interpolated offset
                    let wob = lo - (off - base);
                    let u = l as f32 * s + wob;
                    let p = |du: f32| {
                        let uu = u + du;
                        probe(uu * -kn.sa + v * kn.ca, uu * kn.ca + v * kn.sa)
                    };
                    let (a, b) = (p(amp), p(-amp));
                    if a.is_nan() || b.is_nan() {
                        continue;
                    }
                    vote += a - b;
                    n += 1;
                }
                if n >= 3 && vote.abs() > 0.05 {
                    acc[bit as usize] += vote * chip as f32;
                    hits[bit as usize] += 1;
                }
            }
        }
    }
    let (mut agree, mut seen, mut mag) = (0, 0, 0.0f32);
    for b in 0..PAYLOAD_BITS {
        if hits[b] == 0 || acc[b] == 0.0 {
            continue;
        }
        seen += 1;
        mag += acc[b].abs() / hits[b] as f32;
        let want = (key.payload[b / 8] >> (b % 8)) & 1 == 1;
        if (acc[b] > 0.0) == want {
            agree += 1;
        }
    }
    (agree, seen, if seen > 0 { mag / seen as f32 } else { 0.0 })
}
