//! A compact AAC-LC encoder (no_std + alloc + libm) for LingOS's screen
//! recorder: 48 / 44.1 kHz, mono or stereo, long blocks with the sine
//! window, a flat-noise quantiser whose step (the global gain) is chosen per
//! channel per frame to fill the bit budget, best-fit Huffman codebooks per
//! scale-factor band and run-length sections. No TNS, M/S, PNS or short
//! blocks -- a straightforward stream every AAC decoder plays.
//!
//! Output: raw access units (the MP4 `mp4a` sample format) with the
//! AudioSpecificConfig from [`AacEncoder::asc`]. Like every AAC encoder the
//! output lags the input by one frame (1024 samples): the first decoded frame
//! is the priming silence (an MP4 edit list skips it).
//!
//! [`AacEncoder::encode_frame`] never allocates (all state is made in `new`),
//! so it can run on kernel worker cores.

use crate::common::{AAC_SAMPLE_RATES, SWB_OFFSET_48K_LONG};
use crate::tables::*;
use alloc::vec;
use alloc::vec::Vec;

/// Samples per channel per frame.
pub const FRAME: usize = 1024;
/// Input scale: PCM in -1..1 -> the decoder's spectral domain.
const SCALE: f32 = 65536.0;
/// Highest frequency coded (Hz): the bands above it stay empty.
const BANDWIDTH_HZ: u32 = 19_000;
/// AAC's per-channel limit for one frame.
const MAX_BITS_PER_CH: u32 = 6144;

struct Bw<'a> {
    buf: &'a mut [u8],
    pos: usize,
    acc: u64,
    n: u32,
    over: bool,
}

impl<'a> Bw<'a> {
    fn put(&mut self, v: u32, len: u32) {
        if len == 0 {
            return;
        }
        self.acc = (self.acc << len) | (v as u64 & ((1u64 << len) - 1));
        self.n += len;
        while self.n >= 8 {
            self.n -= 8;
            let b = (self.acc >> self.n) as u8;
            match self.buf.get_mut(self.pos) {
                Some(d) => *d = b,
                None => self.over = true,
            }
            self.pos += 1;
        }
        self.acc &= (1u64 << self.n) - 1;
    }

    fn align(&mut self) {
        if self.n > 0 {
            self.put(0, 8 - self.n);
        }
    }
}

/// Largest absolute value each codebook (1..=11) represents without escapes.
const LAV: [u32; 12] = [0, 1, 1, 2, 2, 4, 4, 7, 7, 12, 12, 16];

fn book(cb: usize) -> (&'static [u32], &'static [u8]) {
    match cb {
        1 => (&SPECTRUM_CODEBOOK1_CODES, &SPECTRUM_CODEBOOK1_LENS),
        2 => (&SPECTRUM_CODEBOOK2_CODES, &SPECTRUM_CODEBOOK2_LENS),
        3 => (&SPECTRUM_CODEBOOK3_CODES, &SPECTRUM_CODEBOOK3_LENS),
        4 => (&SPECTRUM_CODEBOOK4_CODES, &SPECTRUM_CODEBOOK4_LENS),
        5 => (&SPECTRUM_CODEBOOK5_CODES, &SPECTRUM_CODEBOOK5_LENS),
        6 => (&SPECTRUM_CODEBOOK6_CODES, &SPECTRUM_CODEBOOK6_LENS),
        7 => (&SPECTRUM_CODEBOOK7_CODES, &SPECTRUM_CODEBOOK7_LENS),
        8 => (&SPECTRUM_CODEBOOK8_CODES, &SPECTRUM_CODEBOOK8_LENS),
        9 => (&SPECTRUM_CODEBOOK9_CODES, &SPECTRUM_CODEBOOK9_LENS),
        10 => (&SPECTRUM_CODEBOOK10_CODES, &SPECTRUM_CODEBOOK10_LENS),
        _ => (&SPECTRUM_CODEBOOK11_CODES, &SPECTRUM_CODEBOOK11_LENS),
    }
}

/// Bits of the escape word for |v| >= 16.
fn esc_bits(v: u32) -> u32 {
    let n = 31 - v.leading_zeros();
    (n - 4) + 1 + n
}

/// Code (or count, with `bw` None) the values `q` of one band with codebook `cb`; returns the bits.
fn code_band(q: &[i32], cb: usize, mut bw: Option<&mut Bw>) -> u32 {
    let (codes, lens) = book(cb);
    let mut bits = 0;
    if cb <= 4 {
        for w in q.chunks_exact(4) {
            let idx = if cb <= 2 {
                ((w[0] + 1) * 27 + (w[1] + 1) * 9 + (w[2] + 1) * 3 + (w[3] + 1)) as usize
            } else {
                (w[0].unsigned_abs() * 27 + w[1].unsigned_abs() * 9 + w[2].unsigned_abs() * 3 + w[3].unsigned_abs()) as usize
            };
            bits += lens[idx] as u32;
            if let Some(b) = bw.as_deref_mut() {
                b.put(codes[idx], lens[idx] as u32);
            }
            if cb > 2 {
                for &v in w {
                    if v != 0 {
                        bits += 1;
                        if let Some(b) = bw.as_deref_mut() {
                            b.put((v < 0) as u32, 1);
                        }
                    }
                }
            }
        }
    } else {
        for p in q.chunks_exact(2) {
            let (y, z) = (p[0], p[1]);
            let idx = match cb {
                5 | 6 => ((y + 4) * 9 + (z + 4)) as usize,
                7 | 8 => (y.unsigned_abs() * 8 + z.unsigned_abs()) as usize,
                9 | 10 => (y.unsigned_abs() * 13 + z.unsigned_abs()) as usize,
                _ => (y.unsigned_abs().min(16) * 17 + z.unsigned_abs().min(16)) as usize,
            };
            bits += lens[idx] as u32;
            if let Some(b) = bw.as_deref_mut() {
                b.put(codes[idx], lens[idx] as u32);
            }
            if cb >= 7 {
                for v in [y, z] {
                    if v != 0 {
                        bits += 1;
                        if let Some(b) = bw.as_deref_mut() {
                            b.put((v < 0) as u32, 1);
                        }
                    }
                }
                if cb == 11 {
                    for v in [y.unsigned_abs(), z.unsigned_abs()] {
                        if v >= 16 {
                            bits += esc_bits(v);
                            if let Some(b) = bw.as_deref_mut() {
                                let n = 31 - v.leading_zeros();
                                b.put((1 << (n - 4)) - 1, n - 4);
                                b.put(0, 1);
                                b.put(v - (1 << n), n);
                            }
                        }
                    }
                }
            }
        }
    }
    bits
}

/// The cheapest codebook for a band (0 = all zero) and its bits.
fn best_book(q: &[i32]) -> (usize, u32) {
    let m = q.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0);
    if m == 0 {
        return (0, 0);
    }
    let first = (1..=11).find(|&cb| LAV[cb] >= m).unwrap_or(11);
    let mut best = (11, u32::MAX);
    let last = if first >= 11 { 11 } else { (first + 3).min(11) };
    for cb in first..=last {
        if LAV[cb] < m && cb != 11 {
            continue;
        }
        let b = code_band(q, cb, None);
        if b < best.1 {
            best = (cb, b);
        }
    }
    best
}

/// One channel's quantised frame.
struct Chan {
    gg: u32,
    max_sfb: usize,
    cb: [u8; 64],
}

pub struct AacEncoder {
    channels: usize,
    rate: u32,
    rate_idx: usize,
    bits_per_frame: u32,
    bands: &'static [usize],
    nbands: usize,
    win: Vec<f32>,
    prev: Vec<f32>,
    buf: Vec<f32>,
    spec: Vec<f32>,
    q: Vec<i32>,
    /// pre / post twiddles and FFT twiddles (cos, sin)
    pre: Vec<(f32, f32)>,
    post: Vec<(f32, f32)>,
    tw: Vec<(f32, f32)>,
    rev: Vec<u16>,
    z: Vec<(f32, f32)>,
    /// 2^(-(sf - 100) / 4) per scale factor (|x| times it, then to the 3/4 power)
    step: Vec<f32>,
}

impl AacEncoder {
    /// `rate` 48000 or 44100 (the 48 kHz band table serves both), 1 or 2 channels, bit rate in bit/s.
    pub fn new(rate: u32, channels: usize, bitrate: u32) -> AacEncoder {
        let channels = channels.clamp(1, 2);
        let rate_idx = AAC_SAMPLE_RATES.iter().position(|&r| r == rate).unwrap_or(3);
        let bands: &'static [usize] = &SWB_OFFSET_48K_LONG;
        let lim = (BANDWIDTH_HZ as usize * 2 * FRAME / rate.max(1) as usize).min(FRAME);
        let nbands = (0..bands.len() - 1).take_while(|&b| bands[b] < lim).count();
        let n = 2 * FRAME;
        let win = (0..n).map(|i| libm::sin(core::f64::consts::PI / n as f64 * (i as f64 + 0.5)) as f32).collect();
        let nn = FRAME as f64;
        let pre = (0..FRAME / 2)
            .map(|i| {
                let a = -core::f64::consts::PI * (4.0 * i as f64 + 1.0) / (4.0 * nn);
                (libm::cos(a) as f32, libm::sin(a) as f32)
            })
            .collect();
        let post = (0..FRAME / 2)
            .map(|k| {
                let a = -core::f64::consts::PI * k as f64 / nn;
                (libm::cos(a) as f32, libm::sin(a) as f32)
            })
            .collect();
        let m = FRAME / 2;
        let tw = (0..m / 2)
            .map(|k| {
                let a = -2.0 * core::f64::consts::PI * k as f64 / m as f64;
                (libm::cos(a) as f32, libm::sin(a) as f32)
            })
            .collect();
        let bitsn = m.trailing_zeros();
        let rev = (0..m).map(|i| ((i as u32).reverse_bits() >> (32 - bitsn)) as u16).collect();
        let step = (0..256).map(|sf| libm::pow(2.0, -(sf as f64 - 100.0) / 4.0) as f32).collect();
        let bits_per_frame = (bitrate as u64 * FRAME as u64 / rate.max(1) as u64) as u32;
        AacEncoder {
            channels,
            rate,
            rate_idx,
            bits_per_frame: bits_per_frame.min(MAX_BITS_PER_CH * channels as u32),
            bands,
            nbands,
            win,
            prev: vec![0.0; FRAME * channels],
            buf: vec![0.0; 2 * FRAME],
            spec: vec![0.0; FRAME * channels],
            q: vec![0; FRAME * channels],
            pre,
            post,
            tw,
            rev,
            z: vec![(0.0, 0.0); m],
            step,
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// AudioSpecificConfig: AAC-LC, the rate index, the channel count.
    pub fn asc(&self) -> [u8; 2] {
        let v: u16 = (2 << 11) | ((self.rate_idx as u16) << 7) | ((self.channels as u16) << 3);
        v.to_be_bytes()
    }

    /// Largest access unit `encode_frame` can produce.
    pub fn max_frame_bytes(&self) -> usize {
        (MAX_BITS_PER_CH as usize * self.channels) / 8 + 16
    }

    /// MDCT of `self.buf` (2048 windowed samples) into `out` (1024 coefficients).
    fn mdct(&mut self, out_off: usize) {
        let x = &self.buf;
        let n = FRAME; // outputs
        let h = n / 2;
        // DCT-IV input: (-c_r - d, a - b_r) of the four quarters a b c d
        let q = n / 2; // quarter length (512)
        let v = |i: usize| -> f32 {
            if i < q {
                -x[n + q - 1 - i] - x[n + q + i]
            } else {
                let j = i - q;
                x[j] - x[n - 1 - j]
            }
        };
        for i in 0..h {
            let (re, im) = (v(2 * i), v(n - 1 - 2 * i));
            let (c, s) = self.pre[i];
            self.z[self.rev[i] as usize] = (re * c - im * s, re * s + im * c);
        }
        // radix-2 FFT of h points (bit-reversed input)
        let mut len = 2;
        while len <= h {
            let half = len / 2;
            let stride = h / len;
            for start in (0..h).step_by(len) {
                for k in 0..half {
                    let (c, s) = self.tw[k * stride];
                    let (ar, ai) = self.z[start + k];
                    let (br, bi) = self.z[start + k + half];
                    let (tr, ti) = (br * c - bi * s, br * s + bi * c);
                    self.z[start + k] = (ar + tr, ai + ti);
                    self.z[start + k + half] = (ar - tr, ai - ti);
                }
            }
            len *= 2;
        }
        let out = &mut self.spec[out_off..out_off + n];
        for k in 0..h {
            let (re, im) = self.z[k];
            let (c, s) = self.post[k];
            let (ur, ui) = (re * c - im * s, re * s + im * c);
            out[2 * k] = ur;
            out[n - 1 - 2 * k] = -ui;
        }
    }

    /// Quantise channel `ch` with scale factor `sf`; returns (max_sfb, bits incl. side info).
    fn quantise(&mut self, ch: usize, sf: u32, cbs: &mut [u8; 64]) -> (usize, u32) {
        let step = self.step[sf as usize];
        let off = ch * FRAME;
        let mut max_sfb = 0;
        let mut clipped = false;
        for b in 0..self.nbands {
            let (s, e) = (self.bands[b], self.bands[b + 1]);
            let mut any = false;
            for i in s..e {
                let a = self.spec[off + i].abs() * step;
                // |x|^(3/4) via two square roots
                let r = libm::sqrtf(a * libm::sqrtf(a));
                let v = (r + 0.4054) as i32;
                clipped |= v > 8191;
                let v = v.min(8191);
                self.q[off + i] = if self.spec[off + i] < 0.0 { -v } else { v };
                any |= v != 0;
            }
            if any {
                max_sfb = b + 1;
            }
        }
        for i in self.bands[self.nbands]..FRAME {
            self.q[off + i] = 0;
        }
        if clipped {
            // too fine a step: values past the escape range never fit
            return (max_sfb, u32::MAX);
        }
        let mut bits = 8 + 3; // global gain, pulse / tns / gain control flags
        let mut last_cb = 255u8;
        for b in 0..max_sfb {
            let (s, e) = (self.bands[b], self.bands[b + 1]);
            let (cb, bb) = best_book(&self.q[off + s..off + e]);
            cbs[b] = cb as u8;
            bits += bb;
            if cb != 0 {
                bits += 1; // scale factor delta 0
            }
            if cb as u8 != last_cb {
                bits += 4 + 5;
                last_cb = cb as u8;
            }
        }
        (max_sfb, bits)
    }

    /// The lowest (finest) scale factor whose frame fits `budget` bits.
    fn fit(&mut self, ch: usize, budget: u32) -> Chan {
        let mut cbs = [0u8; 64];
        let (mut lo, mut hi) = (40u32, 255u32);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let (_, bits) = self.quantise(ch, mid, &mut cbs);
            if bits <= budget {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        let (max_sfb, _) = self.quantise(ch, lo, &mut cbs);
        Chan { gg: lo, max_sfb, cb: cbs }
    }

    fn write_ics(&self, bw: &mut Bw, ch: usize, c: &Chan, max_sfb: usize, info: bool) {
        let off = ch * FRAME;
        bw.put(c.gg, 8);
        if info {
            self.write_info(bw, max_sfb);
        }
        // section data
        let mut b = 0;
        while b < max_sfb {
            let cb = if b < c.max_sfb { c.cb[b] } else { 0 };
            let mut e = b + 1;
            while e < max_sfb && (if e < c.max_sfb { c.cb[e] } else { 0 }) == cb {
                e += 1;
            }
            bw.put(cb as u32, 4);
            let mut len = e - b;
            while len >= 31 {
                bw.put(31, 5);
                len -= 31;
            }
            bw.put(len as u32, 5);
            b = e;
        }
        // scale factors: every coded band at the global gain (delta 0)
        for b in 0..c.max_sfb.min(max_sfb) {
            if c.cb[b] != 0 {
                bw.put(SCF_CODEBOOK_CODES[60], SCF_CODEBOOK_LENS[60] as u32);
            }
        }
        bw.put(0, 1); // pulse_data_present
        bw.put(0, 1); // tns_data_present
        bw.put(0, 1); // gain_control_data_present
        for b in 0..c.max_sfb.min(max_sfb) {
            if c.cb[b] != 0 {
                let (s, e) = (self.bands[b], self.bands[b + 1]);
                code_band(&self.q[off + s..off + e], c.cb[b] as usize, Some(bw));
            }
        }
    }

    fn write_info(&self, bw: &mut Bw, max_sfb: usize) {
        bw.put(0, 1); // ics_reserved_bit
        bw.put(0, 2); // ONLY_LONG_SEQUENCE
        bw.put(0, 1); // sine window
        bw.put(max_sfb as u32, 6);
        bw.put(0, 1); // predictor_data_present
    }

    /// Encode one frame: `pcm` = FRAME samples per channel, interleaved,
    /// -1..1. Writes one raw access unit into `out` (at least
    /// `max_frame_bytes`) and returns its length (0 if `out` was too small).
    pub fn encode_frame(&mut self, pcm: &[f32], out: &mut [u8]) -> usize {
        let chn = self.channels;
        for ch in 0..chn {
            for i in 0..FRAME {
                self.buf[i] = self.prev[ch * FRAME + i] * self.win[i];
                let s = pcm.get(i * chn + ch).copied().unwrap_or(0.0) * SCALE;
                self.prev[ch * FRAME + i] = s;
                self.buf[FRAME + i] = s * self.win[FRAME + i];
            }
            self.mdct(ch * FRAME);
        }
        // side info: element header + ics_info + M/S flag + END + alignment slack
        let overhead = if chn == 2 { 3 + 4 + 1 + 11 + 2 } else { 3 + 4 + 11 } + 3 + 8;
        let budget = self.bits_per_frame.saturating_sub(overhead) / chn as u32;
        let budget = budget.min(MAX_BITS_PER_CH - 64);
        let mut bw = Bw { buf: out, pos: 0, acc: 0, n: 0, over: false };
        if chn == 2 {
            let c0 = self.fit(0, budget);
            // (each channel quantises into its own half of `q`)
            let c1 = self.fit(1, budget);
            let max_sfb = c0.max_sfb.max(c1.max_sfb);
            bw.put(1, 3); // ID_CPE
            bw.put(0, 4); // element_instance_tag
            bw.put(1, 1); // common_window
            self.write_info(&mut bw, max_sfb);
            bw.put(0, 2); // ms_mask_present
            self.write_ics(&mut bw, 0, &c0, max_sfb, false);
            self.write_ics(&mut bw, 1, &c1, max_sfb, false);
        } else {
            let c0 = self.fit(0, budget);
            bw.put(0, 3); // ID_SCE
            bw.put(0, 4);
            self.write_ics(&mut bw, 0, &c0, c0.max_sfb, true);
        }
        bw.put(7, 3); // ID_END
        bw.align();
        if bw.over {
            0
        } else {
            bw.pos
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AacDecoder;

    fn naive_mdct(x: &[f32]) -> Vec<f64> {
        let n = x.len() / 2;
        (0..n)
            .map(|k| {
                (0..2 * n)
                    .map(|i| {
                        x[i] as f64 * libm::cos(core::f64::consts::PI / n as f64 * (i as f64 + 0.5 + n as f64 / 2.0) * (k as f64 + 0.5))
                    })
                    .sum()
            })
            .collect()
    }

    #[test]
    fn mdct_matches_definition() {
        let mut e = AacEncoder::new(48000, 1, 128_000);
        for i in 0..2048 {
            e.buf[i] = libm::sin(i as f64 * 0.37) as f32 * 1000.0 + ((i * 7919) % 101) as f32;
        }
        let want = naive_mdct(&e.buf.clone());
        e.mdct(0);
        let err = want.iter().zip(&e.spec).map(|(a, b)| (a - *b as f64).abs()).fold(0.0, f64::max);
        let peak = want.iter().map(|a| a.abs()).fold(0.0, f64::max);
        assert!(err < peak * 1e-4, "err {err} peak {peak}");
    }

    /// Encode a stereo tone pair, decode with the crate's decoder, check level and SNR.
    #[test]
    fn round_trip_through_decoder() {
        let rate = 48000;
        let mut e = AacEncoder::new(rate, 2, 160_000);
        let mut d = AacDecoder::new(&e.asc()).unwrap();
        let frames = 40;
        let mut input = vec![0f32; frames * FRAME * 2];
        for i in 0..frames * FRAME {
            let t = i as f64 / rate as f64;
            input[i * 2] = (0.5 * libm::sin(2.0 * core::f64::consts::PI * 1000.0 * t)) as f32;
            input[i * 2 + 1] = (0.25 * libm::sin(2.0 * core::f64::consts::PI * 440.0 * t)) as f32;
        }
        let mut out = vec![0u8; e.max_frame_bytes()];
        let mut decoded: Vec<f32> = Vec::new();
        let mut total = 0;
        for f in 0..frames {
            let n = e.encode_frame(&input[f * FRAME * 2..(f + 1) * FRAME * 2], &mut out);
            assert!(n > 0);
            total += n;
            let mut pcm = Vec::new();
            d.decode(&out[..n], &mut pcm).unwrap();
            decoded.extend_from_slice(&pcm);
        }
        // one frame of delay
        let (mut sig, mut noise) = (0f64, 0f64);
        for i in (4 * FRAME)..((frames - 1) * FRAME) {
            for c in 0..2 {
                let want = input[(i - FRAME) * 2 + c] as f64;
                let got = decoded[i * 2 + c] as f64;
                sig += want * want;
                noise += (want - got) * (want - got);
            }
        }
        let mut best = (0usize, 0f64);
        for lag in 0..2 * FRAME {
            let mut c = 0f64;
            for i in (4 * FRAME)..((frames - 3) * FRAME) {
                c += input[(i - lag.min(i)) * 2] as f64 * decoded[i * 2] as f64;
            }
            if c > best.1 { best = (lag, c); }
        }
        let (mut xy, mut xx) = (0f64, 0f64);
        for i in (4 * FRAME)..((frames - 1) * FRAME) {
            xy += input[(i - FRAME) * 2] as f64 * decoded[i * 2] as f64;
            xx += (input[(i - FRAME) * 2] as f64).powi(2);
        }
        std::println!("best lag {} gain {}", best.0, xy / xx);
        for lag in [1022usize, 1023, 1024, 1025, 1026] {
            let (mut xy, mut xx, mut e) = (0f64, 0f64, 0f64);
            for i in (4 * FRAME)..((frames - 3) * FRAME) {
                let a = input[(i - lag) * 2 + 1] as f64; let b = decoded[i * 2 + 1] as f64;
                xy += a * b; xx += a * a;
            }
            let g = xy / xx;
            for i in (4 * FRAME)..((frames - 3) * FRAME) {
                let a = input[(i - lag) * 2 + 1] as f64 * g; let b = decoded[i * 2 + 1] as f64;
                e += (a - b).powi(2);
            }
            std::println!("lag {lag} gain {g:.4} snr {:.1}", 10.0 * libm::log10(xx * g * g / e));
        }
        let snr = 10.0 * libm::log10(sig / noise);
        let kbps = total as f64 * 8.0 / (frames as f64 * FRAME as f64 / rate as f64) / 1000.0;
        std::println!("AAC round trip: SNR {snr:.1} dB at {kbps:.0} kbit/s");
        assert!(snr > 30.0, "SNR {snr:.1} dB");
        assert!(kbps < 170.0);
    }
}

#[cfg(test)]
mod adts {
    use super::*;
    #[test]
    fn write_adts() {
        let stereo = std::env::var("ADTS_STEREO").is_ok();
        let ch = if stereo { 2 } else { 1 };
        let mut e = AacEncoder::new(48000, ch, if stereo { 160_000 } else { 256_000 });
        let frames = 200;
        let mut seed = 1u32;
        let input: Vec<f32> = (0..frames * FRAME * ch)
            .map(|i| {
                seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                let noise = ((seed >> 16) as f32 / 32768.0 - 1.0) * if stereo { 0.15 } else { 0.0 };
                let t = (i / ch) as f64;
                noise + (0.3 * libm::sin(t * 0.0731) + 0.2 * libm::sin(t * 0.39 * (1.0 + (i % 2) as f64))) as f32 * if stereo { 1.0 } else { 1.6 }
            })
            .collect();
        let mut out = vec![0u8; e.max_frame_bytes()];
        let mut file = Vec::new();
        for f in 0..frames {
            let n = e.encode_frame(&input[f * FRAME * ch..(f + 1) * FRAME * ch], &mut out);
            let len = n + 7;
            file.extend_from_slice(&[0xFF, 0xF1, (1 << 6) | (3 << 2), ((ch as u8) << 6) | ((len >> 11) as u8 & 3), (len >> 3) as u8, ((len & 7) << 5) as u8 | 0x1F, 0xFC]);
            file.extend_from_slice(&out[..n]);
        }
        std::fs::write(std::env::var("ADTS_OUT").unwrap_or("t.aac".into()), &file).unwrap();
    }
}
