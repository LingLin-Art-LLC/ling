//! A small streaming FLAC encoder (16-bit, mono or stereo): fixed predictors
//! (orders 0-4) with partitioned Rice coding, constant / verbatim subframes
//! when they are smaller, and the best of independent, left/side, right/side
//! and mid/side stereo per frame. No LPC (a few percent bigger than `flac -5`),
//! no MD5 (STREAMINFO's all-zero signature means "not computed").
//!
//! Streaming: [`FlacEncoder::header`] gives the "fLaC" marker and STREAMINFO
//! (the total sample count has to be known up front, as for a WAV header),
//! [`FlacEncoder::push`] returns the bytes of every finished frame, and
//! [`FlacEncoder::finish`] the last, shorter one.

use alloc::vec::Vec;

/// Samples per channel in a frame.
pub const BLOCK: usize = 4096;

pub struct FlacEncoder {
    rate: u32,
    channels: usize,
    /// interleaved samples waiting for a whole block
    pending: Vec<i16>,
    frame_no: u64,
}

struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn new(out: Vec<u8>) -> Bits {
        Bits { out, acc: 0, n: 0 }
    }
    /// The low `bits` bits of `v` (bits <= 32).
    fn put(&mut self, v: u64, bits: u32) {
        if bits == 0 {
            return;
        }
        self.acc = (self.acc << bits) | (v & ((1u64 << bits) - 1));
        self.n += bits;
        while self.n >= 8 {
            self.n -= 8;
            self.out.push((self.acc >> self.n) as u8);
        }
        self.acc &= (1u64 << self.n) - 1;
    }
    fn signed(&mut self, v: i64, bits: u32) {
        self.put(v as u64, bits);
    }
    /// `q` zero bits then a one.
    fn unary(&mut self, mut q: u64) {
        while q >= 32 {
            self.put(0, 32);
            q -= 32;
        }
        self.put(1, q as u32 + 1);
    }
    fn align(&mut self) {
        if self.n > 0 {
            let pad = 8 - self.n;
            self.put(0, pad);
        }
    }
    fn done(mut self) -> Vec<u8> {
        self.align();
        self.out
    }
}

fn crc8(d: &[u8]) -> u8 {
    let mut c = 0u8;
    for &b in d {
        c ^= b;
        for _ in 0..8 {
            c = if c & 0x80 != 0 { (c << 1) ^ 0x07 } else { c << 1 };
        }
    }
    c
}

fn crc16(d: &[u8]) -> u16 {
    let mut c = 0u16;
    for &b in d {
        c ^= (b as u16) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 { (c << 1) ^ 0x8005 } else { c << 1 };
        }
    }
    c
}

/// FLAC's UTF-8-style frame number.
fn utf8_num(o: &mut Vec<u8>, v: u64) {
    if v < 0x80 {
        o.push(v as u8);
        return;
    }
    let mut n = 2u32;
    while v >= (1u64 << (5 * n + 1)) {
        n += 1;
    }
    let lead: u8 = !(0xFFu8 >> n);
    o.push(lead | (v >> (6 * (n - 1))) as u8);
    for k in (0..n - 1).rev() {
        o.push(0x80 | ((v >> (6 * k)) & 0x3F) as u8);
    }
}

fn rate_code(rate: u32) -> u8 {
    match rate {
        88200 => 0b0001,
        176400 => 0b0010,
        192000 => 0b0011,
        8000 => 0b0100,
        16000 => 0b0101,
        22050 => 0b0110,
        24000 => 0b0111,
        32000 => 0b1000,
        44100 => 0b1001,
        48000 => 0b1010,
        96000 => 0b1011,
        _ => 0, // from STREAMINFO
    }
}

/// Residuals of the fixed predictor of `order` (the first `order` samples are warm-up, not included).
fn fixed_residual(x: &[i64], order: usize, r: &mut Vec<i64>) {
    r.clear();
    for i in order..x.len() {
        let e = match order {
            0 => x[i],
            1 => x[i] - x[i - 1],
            2 => x[i] - 2 * x[i - 1] + x[i - 2],
            3 => x[i] - 3 * x[i - 1] + 3 * x[i - 2] - x[i - 3],
            _ => x[i] - 4 * x[i - 1] + 6 * x[i - 2] - 4 * x[i - 3] + x[i - 4],
        };
        r.push(e);
    }
}

fn zz(e: i64) -> u64 {
    ((e << 1) ^ (e >> 63)) as u64
}

/// Estimated bits of a partition of `cnt` residuals whose zigzag values sum to `sum`, coded with
/// Rice parameter `k` (the quotients' total is about `sum >> k`).
fn est_bits(cnt: u64, sum: u64, k: u32) -> u64 {
    cnt * (k as u64 + 1) + (sum >> k)
}

/// The best parameter for a partition (estimated) and its cost.
fn best_k(cnt: u64, sum: u64) -> (u32, u64) {
    if cnt == 0 {
        return (0, 0);
    }
    let mean = sum / cnt;
    let mut k0 = 0u32;
    while k0 < 14 && (1u64 << (k0 + 1)) <= mean {
        k0 += 1;
    }
    let mut best = (k0, est_bits(cnt, sum, k0));
    for k in [k0.saturating_sub(1), (k0 + 1).min(14)] {
        let b = est_bits(cnt, sum, k);
        if b < best.1 {
            best = (k, b);
        }
    }
    best
}

/// The partition order and parameters that code a block's residual smallest:
/// (order, params, estimated bits including the 4-bit parameters).
fn plan_residual(res: &[i64], n: usize, pred_order: usize) -> (u32, Vec<u32>, u64) {
    // prefix sums of the zigzag values: any partition's sum in one step
    let mut pre: Vec<u64> = Vec::with_capacity(res.len() + 1);
    pre.push(0);
    let mut acc = 0u64;
    for &e in res {
        acc += zz(e);
        pre.push(acc);
    }
    let mut best: Option<(u32, Vec<u32>, u64)> = None;
    for p in 0..=8u32 {
        let parts = 1usize << p;
        if n % parts != 0 || n / parts <= pred_order {
            break;
        }
        let len = n / parts;
        let mut ks = Vec::with_capacity(parts);
        let mut bits = 0u64;
        let mut at = 0usize;
        for i in 0..parts {
            let cnt = if i == 0 { len - pred_order } else { len };
            let (k, b) = best_k(cnt as u64, pre[at + cnt] - pre[at]);
            at += cnt;
            ks.push(k);
            bits += b + 4;
        }
        if best.as_ref().is_none_or(|b| bits < b.2) {
            best = Some((p, ks, bits));
        }
    }
    best.unwrap_or((0, alloc::vec![0], u64::MAX / 4))
}

enum Sub {
    Constant,
    Verbatim,
    Fixed { order: usize, res: Vec<i64>, porder: u32, ks: Vec<u32> },
}

/// The smallest subframe for samples `x` of `bps` bits, and its size in bits.
fn plan_subframe(x: &[i64], bps: u32) -> (Sub, u64) {
    let n = x.len();
    if x.iter().all(|&v| v == x[0]) {
        return (Sub::Constant, 8 + bps as u64);
    }
    let mut best = (Sub::Verbatim, 8 + bps as u64 * n as u64);
    let mut r = Vec::with_capacity(n);
    for order in 0..=4usize.min(n.saturating_sub(1)) {
        fixed_residual(x, order, &mut r);
        let (porder, ks, rbits) = plan_residual(&r, n, order);
        let bits = 8 + order as u64 * bps as u64 + 6 + rbits;
        if bits < best.1 {
            best = (Sub::Fixed { order, res: core::mem::take(&mut r), porder, ks }, bits);
            r = Vec::with_capacity(n);
        }
    }
    best
}

fn write_subframe(w: &mut Bits, x: &[i64], bps: u32, sub: &Sub) {
    match sub {
        Sub::Constant => {
            w.put(0, 8);
            w.signed(x[0], bps);
        },
        Sub::Verbatim => {
            w.put(0b0000_0010, 8);
            for &v in x {
                w.signed(v, bps);
            }
        },
        Sub::Fixed { order, res, porder, ks } => {
            w.put((0x10 | (*order as u64) << 1) & 0xFF, 8);
            for &v in &x[..*order] {
                w.signed(v, bps);
            }
            w.put(0, 2); // Rice, 4-bit parameters
            w.put(*porder as u64, 4);
            let parts = 1usize << porder;
            let len = x.len() / parts;
            let mut at = 0usize;
            for (i, &k) in ks.iter().enumerate() {
                let cnt = if i == 0 { len - order } else { len };
                w.put(k as u64, 4);
                for &e in &res[at..at + cnt] {
                    let u = zz(e);
                    w.unary(u >> k);
                    w.put(u, k);
                }
                at += cnt;
            }
        },
    }
}

impl FlacEncoder {
    pub fn new(rate: u32, channels: usize) -> FlacEncoder {
        FlacEncoder { rate, channels: channels.clamp(1, 2), pending: Vec::new(), frame_no: 0 }
    }

    /// "fLaC" and the STREAMINFO block, for `total` samples per channel.
    pub fn header(&self, total: u64) -> Vec<u8> {
        let mut o = Vec::with_capacity(42);
        o.extend_from_slice(b"fLaC");
        o.extend_from_slice(&[0x80, 0, 0, 34]); // the last metadata block: STREAMINFO, 34 bytes
        o.extend_from_slice(&(BLOCK as u16).to_be_bytes());
        o.extend_from_slice(&(BLOCK as u16).to_be_bytes());
        o.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // min/max frame size: unknown
        let packed: u64 = ((self.rate as u64 & 0xF_FFFF) << 44) | (((self.channels - 1) as u64) << 41) | (15u64 << 36) | (total & 0xF_FFFF_FFFF);
        o.extend_from_slice(&packed.to_be_bytes());
        o.extend_from_slice(&[0u8; 16]);
        o
    }

    /// Interleaved samples in; the bytes of every whole frame they complete out.
    pub fn push(&mut self, pcm: &[i16]) -> Vec<u8> {
        self.pending.extend_from_slice(pcm);
        let mut out = Vec::new();
        let per = BLOCK * self.channels;
        let mut at = 0usize;
        while self.pending.len() - at >= per {
            let f = self.frame(&self.pending[at..at + per].to_vec());
            out.extend_from_slice(&f);
            at += per;
        }
        self.pending.drain(..at);
        out
    }

    /// The last (shorter) frame.
    pub fn finish(&mut self) -> Vec<u8> {
        let whole = self.pending.len() / self.channels * self.channels;
        if whole == 0 {
            return Vec::new();
        }
        let rest: Vec<i16> = self.pending[..whole].to_vec();
        self.pending.clear();
        self.frame(&rest)
    }

    fn frame(&mut self, pcm: &[i16]) -> Vec<u8> {
        let ch = self.channels;
        let n = pcm.len() / ch;
        let chans: Vec<Vec<i64>> = (0..ch).map(|c| (0..n).map(|i| pcm[i * ch + c] as i64).collect()).collect();
        // the channel layout: (assignment, [(samples, bps)])
        let mut subs: Vec<(Vec<i64>, u32, Sub)> = Vec::new();
        let assign: u8;
        if ch == 2 {
            let (l, r) = (&chans[0], &chans[1]);
            let side: Vec<i64> = l.iter().zip(r).map(|(a, b)| a - b).collect();
            let mid: Vec<i64> = l.iter().zip(r).map(|(a, b)| (a + b) >> 1).collect();
            let pl = plan_subframe(l, 16);
            let pr = plan_subframe(r, 16);
            let ps = plan_subframe(&side, 17);
            let pm = plan_subframe(&mid, 16);
            let costs = [pl.1 + pr.1, pl.1 + ps.1, ps.1 + pr.1, pm.1 + ps.1];
            let best = (0..4).min_by_key(|&i| costs[i]).unwrap_or(0);
            match best {
                1 => {
                    assign = 0b1000;
                    subs.push((l.clone(), 16, pl.0));
                    subs.push((side, 17, ps.0));
                },
                2 => {
                    assign = 0b1001;
                    subs.push((side, 17, ps.0));
                    subs.push((r.clone(), 16, pr.0));
                },
                3 => {
                    assign = 0b1010;
                    subs.push((mid, 16, pm.0));
                    subs.push((side, 17, ps.0));
                },
                _ => {
                    assign = 0b0001;
                    subs.push((l.clone(), 16, pl.0));
                    subs.push((r.clone(), 16, pr.0));
                },
            }
        } else {
            assign = 0;
            let p = plan_subframe(&chans[0], 16);
            subs.push((chans[0].clone(), 16, p.0));
        }
        // the header
        let mut o: Vec<u8> = Vec::with_capacity(n * ch * 2 / 2 + 32);
        o.extend_from_slice(&[0xFF, 0xF8]);
        let bs_code: u8 = if n == BLOCK { 0b1100 } else { 0b0111 };
        o.push((bs_code << 4) | rate_code(self.rate));
        o.push((assign << 4) | (0b100 << 1));
        utf8_num(&mut o, self.frame_no);
        if bs_code == 0b0111 {
            o.extend_from_slice(&((n - 1) as u16).to_be_bytes());
        }
        let c8 = crc8(&o);
        o.push(c8);
        let mut w = Bits::new(o);
        for (x, bps, sub) in &subs {
            write_subframe(&mut w, x, *bps, sub);
        }
        let mut o = w.done();
        let c16 = crc16(&o);
        o.extend_from_slice(&c16.to_be_bytes());
        self.frame_no += 1;
        o
    }
}

/// A 16-bit PCM WAV header for `frames` samples per channel.
pub fn wav_header(rate: u32, channels: u16, frames: u64) -> Vec<u8> {
    let len = (frames * channels as u64 * 2).min(u32::MAX as u64 - 36) as u32;
    let mut o = Vec::with_capacity(44);
    o.extend_from_slice(b"RIFF");
    o.extend_from_slice(&(36 + len).to_le_bytes());
    o.extend_from_slice(b"WAVEfmt ");
    o.extend_from_slice(&16u32.to_le_bytes());
    o.extend_from_slice(&1u16.to_le_bytes());
    o.extend_from_slice(&channels.to_le_bytes());
    o.extend_from_slice(&rate.to_le_bytes());
    o.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    o.extend_from_slice(&(channels * 2).to_le_bytes());
    o.extend_from_slice(&16u16.to_le_bytes());
    o.extend_from_slice(b"data");
    o.extend_from_slice(&len.to_le_bytes());
    o
}
