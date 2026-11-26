//! SBC, the A2DP mandatory codec (A2DP v1.3 Appendix B), 8 subbands.
//!
//! [`Encoder`] turns 16-bit PCM into SBC frames: the 80-tap polyphase
//! analysis filterbank, scale factors, the normative bit allocation (the
//! decoder recomputes it from the scale factors, so it must match bit for
//! bit), quantisation and the frame with its CRC-8. [`decode`] is the
//! matching synthesis (used by the tests to measure the round trip).
//! Joint stereo is sent with every join bit clear (valid, just no M/S gain).

use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Mono,
    Dual,
    Stereo,
    Joint,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Alloc {
    Loudness,
    Snr,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Config {
    pub rate: u32,
    pub mode: Mode,
    pub alloc: Alloc,
    /// 4, 8, 12 or 16
    pub blocks: u8,
    pub bitpool: u8,
}

impl Default for Config {
    fn default() -> Self {
        Config { rate: 48000, mode: Mode::Stereo, alloc: Alloc::Loudness, blocks: 16, bitpool: 53 }
    }
}

const SB: usize = 8;

impl Config {
    pub fn channels(&self) -> usize {
        if self.mode == Mode::Mono {
            1
        } else {
            2
        }
    }

    fn rate_code(&self) -> u8 {
        match self.rate {
            16000 => 0,
            32000 => 1,
            44100 => 2,
            _ => 3,
        }
    }

    /// PCM frames (per channel) one SBC frame carries.
    pub fn samples(&self) -> usize {
        self.blocks as usize * SB
    }

    /// Bytes in one SBC frame.
    pub fn frame_len(&self) -> usize {
        let ch = self.channels();
        let nb = self.blocks as usize;
        let bp = self.bitpool as usize;
        let mut bits = 4 * SB * ch;
        bits += match self.mode {
            Mode::Mono | Mode::Dual => nb * ch * bp,
            Mode::Stereo => nb * bp,
            Mode::Joint => SB + nb * bp,
        };
        4 + bits.div_ceil(8)
    }

    /// The A2DP codec information element for Set Configuration.
    pub fn a2dp_bytes(&self) -> [u8; 4] {
        let rate = match self.rate {
            16000 => 0x80,
            32000 => 0x40,
            44100 => 0x20,
            _ => 0x10,
        };
        let mode = match self.mode {
            Mode::Mono => 0x08,
            Mode::Dual => 0x04,
            Mode::Stereo => 0x02,
            Mode::Joint => 0x01,
        };
        let blocks = match self.blocks {
            4 => 0x80,
            8 => 0x40,
            12 => 0x20,
            _ => 0x10,
        };
        let alloc = if self.alloc == Alloc::Loudness { 0x01 } else { 0x02 };
        [rate | mode, blocks | 0x04 | alloc, self.bitpool, self.bitpool]
    }
}

/// The prototype filter for 8 subbands, h[0..=40] (h[80-n] = h[n]), from the
/// A2DP specification's table (where the 2nd and 4th groups of 16 carry a
/// negated sign; [`c_table`] applies that).
const PROTO8_HALF: [f32; 41] = [
    0.0, 1.565_754e-4, 3.432_564_3e-4, 5.546_202e-4, 8.239_195e-4, 1.139_925_1e-3, 1.476_401_7e-3, 1.783_717_3e-3, 2.011_825_4e-3, 2.103_72e-3, 1.994_545_5e-3,
    1.616_562_8e-3, 9.021_545e-4, -1.788_053_6e-4, -1.649_731e-3, -3.497_174_5e-3, -5.659_494_7e-3, -8.029_412e-3, -1.045_844_4e-2, -1.274_723_3e-2, -1.465_252_6e-2,
    -1.590_456e-2, -1.622_084_7e-2, -1.531_841_1e-2, -1.293_718e-2, -8.857_575e-3, -2.924_084_4e-3, 4.915_780_2e-3, 1.464_040_8e-2, 2.610_987_5e-2, 3.907_514e-2,
    5.318_730_3e-2, 6.799_894e-2, 8.298_476e-2, 9.757_539e-2, 1.111_966_9e-1, 1.232_645_5e-1, 1.332_644_2e-1, 1.407_535e-1, 1.453_898_5e-1, 1.469_550_7e-1,
];

/// The analysis window C[0..80]: h with the sign of groups 1 and 3 flipped.
fn c_table() -> [f32; 80] {
    let mut c = [0f32; 80];
    for (i, v) in c.iter_mut().enumerate() {
        let n = if i <= 40 { i } else { 80 - i };
        let s = if (i / 16) % 2 == 1 { -1.0 } else { 1.0 };
        *v = PROTO8_HALF[n] * s;
    }
    c
}

fn cos_matrix() -> [[f32; 16]; 8] {
    let mut m = [[0f32; 16]; 8];
    for (k, row) in m.iter_mut().enumerate() {
        for (i, v) in row.iter_mut().enumerate() {
            *v = libm::cos((k as f64 + 0.5) * (i as f64 - 4.0) * core::f64::consts::PI / 8.0) as f32;
        }
    }
    m
}

/// Loudness offsets for 8 subbands per sampling rate (16, 32, 44.1, 48 kHz).
const OFFSET8: [[i32; 8]; 4] = [[-2, 0, 0, 0, 0, 0, 0, 1], [-3, 0, 0, 0, 0, 0, 1, 2], [-4, 0, 0, 0, 0, 0, 1, 2], [-4, 0, 0, 0, 0, 0, 1, 2]];

/// The bit allocation (A2DP 12.6.3): bits per channel and subband.
pub fn allocate(cfg: &Config, sf: &[[u8; SB]; 2]) -> [[u8; SB]; 2] {
    let ch = cfg.channels();
    let mut bits = [[0u8; SB]; 2];
    let off = &OFFSET8[cfg.rate_code() as usize];
    let need = |c: usize, sb: usize| -> i32 {
        let s = sf[c][sb] as i32;
        match cfg.alloc {
            Alloc::Snr => s,
            Alloc::Loudness => {
                if s == 0 {
                    -5
                } else {
                    let l = s - off[sb];
                    if l > 0 {
                        l / 2
                    } else {
                        l
                    }
                }
            },
        }
    };
    let bitpool = cfg.bitpool as i32;
    // the channel groups the bitpool is shared over
    let groups: Vec<Vec<usize>> = match cfg.mode {
        Mode::Mono | Mode::Dual => (0..ch).map(|c| alloc::vec![c]).collect(),
        Mode::Stereo | Mode::Joint => alloc::vec![alloc::vec![0, 1]],
    };
    for g in groups {
        let mut bitneed = [[0i32; SB]; 2];
        let mut max_need = i32::MIN;
        for &c in &g {
            for sb in 0..SB {
                bitneed[c][sb] = need(c, sb);
                max_need = max_need.max(bitneed[c][sb]);
            }
        }
        let mut bitcount = 0i32;
        let mut slicecount = 0i32;
        let mut bitslice = max_need + 1;
        loop {
            bitslice -= 1;
            bitcount += slicecount;
            slicecount = 0;
            for &c in &g {
                for sb in 0..SB {
                    let n = bitneed[c][sb];
                    if n > bitslice + 1 && n < bitslice + 16 {
                        slicecount += 1;
                    } else if n == bitslice + 1 {
                        slicecount += 2;
                    }
                }
            }
            if bitcount + slicecount >= bitpool {
                break;
            }
        }
        if bitcount + slicecount == bitpool {
            bitcount += slicecount;
            bitslice -= 1;
        }
        for &c in &g {
            for sb in 0..SB {
                let n = bitneed[c][sb];
                bits[c][sb] = if n < bitslice + 2 { 0 } else { (n - bitslice).min(16) as u8 };
            }
        }
        // leftovers: subband-major, channel-minor within the group
        let order: Vec<(usize, usize)> = (0..SB).flat_map(|sb| g.iter().map(move |&c| (c, sb))).collect();
        let mut i = 0;
        while bitcount < bitpool && i < order.len() {
            let (c, sb) = order[i];
            if bits[c][sb] >= 2 && bits[c][sb] < 16 {
                bits[c][sb] += 1;
                bitcount += 1;
            } else if bitneed[c][sb] == bitslice + 1 && bitpool > bitcount + 1 {
                bits[c][sb] = 2;
                bitcount += 2;
            }
            i += 1;
        }
        let mut i = 0;
        while bitcount < bitpool && i < order.len() {
            let (c, sb) = order[i];
            if bits[c][sb] < 16 {
                bits[c][sb] += 1;
                bitcount += 1;
            }
            i += 1;
        }
    }
    bits
}

struct BitW {
    v: Vec<u8>,
    n: usize,
}

impl BitW {
    fn put(&mut self, val: u32, bits: u32) {
        for i in (0..bits).rev() {
            let b = ((val >> i) & 1) as u8;
            if self.n % 8 == 0 {
                self.v.push(0);
            }
            let last = self.v.len() - 1;
            self.v[last] |= b << (7 - self.n % 8);
            self.n += 1;
        }
    }
}

fn crc8(bytes: &[u8], bits: usize) -> u8 {
    let mut crc = 0x0Fu8;
    for i in 0..bits {
        let b = (bytes[i / 8] >> (7 - i % 8)) & 1;
        let x = ((crc >> 7) ^ b) & 1;
        crc <<= 1;
        if x != 0 {
            crc ^= 0x1D;
        }
    }
    crc
}

pub struct Encoder {
    pub cfg: Config,
    x: [[f32; 80]; 2],
    c: [f32; 80],
    m: [[f32; 16]; 8],
}

impl Encoder {
    pub fn new(cfg: Config) -> Self {
        Encoder { cfg, x: [[0.0; 80]; 2], c: c_table(), m: cos_matrix() }
    }

    /// One SBC frame from `cfg.samples()` interleaved stereo frames (L, R, L, R...;
    /// mono configurations take the average). Short input is zero-padded.
    pub fn encode(&mut self, pcm: &[i16]) -> Vec<u8> {
        let cfg = self.cfg;
        let ch = cfg.channels();
        let nb = cfg.blocks as usize;
        let mut s = [[[0f32; SB]; 2]; 16];
        for (blk, sblk) in s.iter_mut().enumerate().take(nb) {
            for (c, sc) in sblk.iter_mut().enumerate().take(ch) {
                let x = &mut self.x[c];
                x.copy_within(0..72, 8);
                for i in 0..SB {
                    let f = blk * SB + i;
                    let l = *pcm.get(f * 2).unwrap_or(&0) as f32;
                    let r = *pcm.get(f * 2 + 1).unwrap_or(&0) as f32;
                    let v = if ch == 1 { (l + r) * 0.5 } else if c == 0 { l } else { r };
                    x[7 - i] = v;
                }
                let mut y = [0f32; 16];
                for (i, yi) in y.iter_mut().enumerate() {
                    for j in 0..5 {
                        *yi += self.c[i + 16 * j] * x[i + 16 * j];
                    }
                }
                for (k, out) in sc.iter_mut().enumerate() {
                    *out = (0..16).map(|i| self.m[k][i] * y[i]).sum();
                }
            }
        }
        // scale factors: the smallest sf with max |s| < 2^(sf+1)
        let mut sf = [[0u8; SB]; 2];
        for c in 0..ch {
            for sb in 0..SB {
                let mx = (0..nb).map(|b| libm::fabsf(s[b][c][sb])).fold(0f32, f32::max);
                let mut f = 0u8;
                while f < 15 && mx >= (2u32 << f) as f32 {
                    f += 1;
                }
                sf[c][sb] = f;
            }
        }
        let bits = allocate(&cfg, &sf);
        let mut w = BitW { v: Vec::with_capacity(cfg.frame_len()), n: 0 };
        let mode = match cfg.mode {
            Mode::Mono => 0,
            Mode::Dual => 1,
            Mode::Stereo => 2,
            Mode::Joint => 3,
        };
        let blocks = match nb {
            4 => 0,
            8 => 1,
            12 => 2,
            _ => 3,
        };
        w.put(0x9C, 8);
        w.put(((cfg.rate_code() as u32) << 6) | (blocks << 4) | (mode << 2) | (((cfg.alloc == Alloc::Snr) as u32) << 1) | 1, 8);
        w.put(cfg.bitpool as u32, 8);
        w.put(0, 8); // CRC, filled in below
        if cfg.mode == Mode::Joint {
            w.put(0, SB as u32);
        }
        for c in 0..ch {
            for sb in 0..SB {
                w.put(sf[c][sb] as u32, 4);
            }
        }
        // the CRC covers bytes 1 and 2 and the bits from the join field to the scale factors' end
        let crc_bits = w.n - 32;
        let mut crc_in = alloc::vec![w.v[1], w.v[2]];
        crc_in.extend_from_slice(&w.v[4..]);
        w.v[3] = crc8(&crc_in, 16 + crc_bits);
        for sblk in s.iter().take(nb) {
            for c in 0..ch {
                for sb in 0..SB {
                    let b = bits[c][sb] as u32;
                    if b == 0 {
                        continue;
                    }
                    let levels = ((1u32 << b) - 1) as f32;
                    let scale = (2u32 << sf[c][sb]) as f32;
                    let q = libm::floorf((sblk[c][sb] / scale + 1.0) * levels / 2.0);
                    w.put(q.clamp(0.0, levels) as u32, b);
                }
            }
        }
        w.v
    }
}

/// Decode one frame (8 subbands) into interleaved i16 (two channels unless
/// mono). `state` carries the synthesis history between frames. None when the
/// frame is malformed or its CRC is wrong.
pub fn decode(frame: &[u8], state: &mut [[f32; 160]; 2]) -> Option<(Config, Vec<i16>)> {
    if *frame.first()? != 0x9C {
        return None;
    }
    let b1 = *frame.get(1)?;
    let rate = [16000, 32000, 44100, 48000][(b1 >> 6) as usize];
    let blocks = [4u8, 8, 12, 16][((b1 >> 4) & 3) as usize];
    let mode = [Mode::Mono, Mode::Dual, Mode::Stereo, Mode::Joint][((b1 >> 2) & 3) as usize];
    let alloc = if b1 & 2 != 0 { Alloc::Snr } else { Alloc::Loudness };
    if b1 & 1 == 0 {
        return None; // 4 subbands: not handled here
    }
    let cfg = Config { rate, mode, alloc, blocks, bitpool: *frame.get(2)? };
    if frame.len() < cfg.frame_len() {
        return None;
    }
    let ch = cfg.channels();
    let mut pos = 32usize;
    let rd = |pos: &mut usize, n: usize| -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            let b = (frame[*pos / 8] >> (7 - *pos % 8)) & 1;
            v = (v << 1) | b as u32;
            *pos += 1;
        }
        v
    };
    let mut join = 0u32;
    if mode == Mode::Joint {
        join = rd(&mut pos, SB);
    }
    let mut sf = [[0u8; SB]; 2];
    for c in sf.iter_mut().take(ch) {
        for v in c.iter_mut() {
            *v = rd(&mut pos, 4) as u8;
        }
    }
    let mut crc_in = alloc::vec![frame[1], frame[2]];
    crc_in.extend_from_slice(&frame[4..pos.div_ceil(8)]);
    if crc8(&crc_in, 16 + pos - 32) != frame[3] {
        return None;
    }
    let bits = allocate(&cfg, &sf);
    let n = cfg.blocks as usize;
    let mut s = alloc::vec![[[0f32; SB]; 2]; n];
    for sblk in s.iter_mut() {
        for c in 0..ch {
            for sb in 0..SB {
                let b = bits[c][sb] as usize;
                if b == 0 {
                    continue;
                }
                let q = rd(&mut pos, b) as f32;
                let levels = ((1u32 << b) - 1) as f32;
                let scale = (2u32 << sf[c][sb]) as f32;
                sblk[c][sb] = scale * ((q * 2.0 + 1.0) / levels - 1.0);
            }
        }
        if mode == Mode::Joint {
            for sb in 0..SB {
                if join & (1 << (SB - 1 - sb)) != 0 {
                    let (m, sd) = (sblk[0][sb], sblk[1][sb]);
                    sblk[0][sb] = m + sd;
                    sblk[1][sb] = m - sd;
                }
            }
        }
    }
    let c = c_table();
    let mut nmat = [[0f32; SB]; 16];
    for (k, row) in nmat.iter_mut().enumerate() {
        for (i, v) in row.iter_mut().enumerate() {
            *v = libm::cos((i as f64 + 0.5) * (k as f64 + 4.0) * core::f64::consts::PI / 8.0) as f32;
        }
    }
    let mut out = alloc::vec![0i16; n * SB * ch];
    for (blk, sblk) in s.iter().enumerate() {
        for ci in 0..ch {
            let v = &mut state[ci];
            v.copy_within(0..144, 16);
            for k in 0..16 {
                v[k] = (0..SB).map(|i| nmat[k][i] * sblk[ci][i]).sum();
            }
            let mut u = [0f32; 80];
            for i in 0..5 {
                for j in 0..8 {
                    u[i * 16 + j] = v[i * 32 + j];
                    u[i * 16 + 8 + j] = v[i * 32 + 24 + j];
                }
            }
            for j in 0..SB {
                let mut acc = 0f32;
                for i in 0..10 {
                    acc += c[j + 8 * i] * -8.0 * u[j + 8 * i];
                }
                out[(blk * SB + j) * ch + ci] = acc.clamp(-32768.0, 32767.0) as i16;
            }
        }
    }
    Some((cfg, out))
}
