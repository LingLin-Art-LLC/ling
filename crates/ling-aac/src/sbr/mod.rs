//! Spectral Band Replication (HE-AAC), ISO/IEC 14496-3 subclause 4.6.18.
//!
//! Written for LingOS from the specification. The QMF banks use the spec's
//! complex-exponential definitions directly (precomputed matrices); signal
//! levels follow the reference decoders (core samples scaled to +-32768 inside
//! the filterbanks), so the output matches FFmpeg's float decoder closely.
//!
//! Parametric stereo (HE-AACv2) data is recognised and skipped; the caller
//! duplicates the mono SBR output.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crate::bits::BitReader;
use crate::huffman::Vlc;
use crate::AacError;

mod tables;
use tables::*;

const NUM_TIME_SLOTS: usize = 16; // 1024-sample frames
const RATE: usize = 2;
const SLOTS: usize = NUM_TIME_SLOTS * RATE; // 32 QMF slots per frame
const T_HFGEN: usize = 8;
const T_HFADJ: usize = 2;
const NOISE_FLOOR_OFFSET: i32 = 6;

const FIXFIX: u32 = 0;
const FIXVAR: u32 = 1;
const VARFIX: u32 = 2;

#[derive(Clone, Copy, Default, Debug)]
struct C {
    re: f32,
    im: f32,
}

impl C {
    #[inline(always)]
    fn new(re: f32, im: f32) -> Self {
        C { re, im }
    }
    #[inline(always)]
    fn mul(self, o: C) -> C {
        C::new(self.re * o.re - self.im * o.im, self.re * o.im + self.im * o.re)
    }
    #[inline(always)]
    fn conj(self) -> C {
        C::new(self.re, -self.im)
    }
    #[inline(always)]
    fn add(self, o: C) -> C {
        C::new(self.re + o.re, self.im + o.im)
    }
    #[inline(always)]
    fn scale(self, s: f32) -> C {
        C::new(self.re * s, self.im * s)
    }
    #[inline(always)]
    fn norm(self) -> f32 {
        self.re * self.re + self.im * self.im
    }
}

/// 2^x for integer x.
#[inline(always)]
fn exp2i(x: i32) -> f32 {
    if x > 127 {
        f32::INFINITY
    } else if x < -149 {
        0.0
    } else {
        libm::exp2f(x as f32)
    }
}

/// A Huffman book plus the offset added to decoded symbols.
type Book = (Vlc, i32);

struct Books {
    t_env_15: Book,
    f_env_15: Book,
    t_env_bal_15: Book,
    f_env_bal_15: Book,
    t_env_30: Book,
    f_env_30: Book,
    t_env_bal_30: Book,
    f_env_bal_30: Book,
    t_noise_30: Book,
    t_noise_bal_30: Book,
}

fn book<const N: usize>(t: &([(u8, u8); N], i8)) -> Book {
    // Canonical codes in listed order: next = (prev + 1) << (len - prev_len).
    let mut c2 = [0u32; N];
    let mut l2 = [0u8; N];
    let mut code: u32 = 0;
    let mut prev_len = 0u8;
    for (i, &(sym, len)) in t.0.iter().enumerate() {
        if i > 0 {
            code = (code + 1) << (len - prev_len);
        }
        prev_len = len;
        // Vlc returns the table index; store so that index == symbol.
        if (sym as usize) < N {
            c2[sym as usize] = code;
            l2[sym as usize] = len;
        }
    }
    (Vlc::new(&c2, &l2), i32::from(t.1))
}

impl Books {
    fn new() -> Self {
        Self {
            t_env_15: book(&T_ENV_1_5DB),
            f_env_15: book(&F_ENV_1_5DB),
            t_env_bal_15: book(&T_ENV_BAL_1_5DB),
            f_env_bal_15: book(&F_ENV_BAL_1_5DB),
            t_env_30: book(&T_ENV_3_0DB),
            f_env_30: book(&F_ENV_3_0DB),
            t_env_bal_30: book(&T_ENV_BAL_3_0DB),
            f_env_bal_30: book(&F_ENV_BAL_3_0DB),
            t_noise_30: book(&T_NOISE_3_0DB),
            t_noise_bal_30: book(&T_NOISE_BAL_3_0DB),
        }
    }
}

#[inline]
fn read_sym(bs: &mut BitReader<'_>, b: &Book) -> Result<i32, AacError> {
    Ok(i32::from(b.0.read(bs)?) + b.1)
}

/// QMF matrices (spec 4.6.18.4).
struct Qmf {
    /// Analysis: 2*exp(i*pi*(k+0.5)*(2n-0.5)/64), [k][n], 32 x 64.
    ana: Vec<C>,
    /// Synthesis: (1/64)*exp(i*pi*(k+0.5)*(2n-255)/128), stored [n][k], 128 x 64.
    syn: Vec<C>,
}

impl Qmf {
    fn new() -> Self {
        let pi = core::f64::consts::PI;
        let mut ana = Vec::with_capacity(32 * 64);
        for k in 0..32 {
            for n in 0..64 {
                let a = pi * (k as f64 + 0.5) * (2.0 * n as f64 - 0.5) / 64.0;
                ana.push(C::new((2.0 * libm::cos(a)) as f32, (2.0 * libm::sin(a)) as f32));
            }
        }
        let mut syn = Vec::with_capacity(128 * 64);
        for n in 0..128 {
            for k in 0..64 {
                let a = pi * (k as f64 + 0.5) * (2.0 * n as f64 - 255.0) / 128.0;
                syn.push(C::new((libm::cos(a) / 64.0) as f32, (libm::sin(a) / 64.0) as f32));
            }
        }
        Self { ana, syn }
    }
}

/// Per-channel SBR state.
#[derive(Clone)]
struct ChannelData {
    frame_class: u32,
    num_env: usize,
    num_noise: usize,
    freq_res: [usize; 9],
    amp_res: bool,
    t_env: [usize; 9],
    t_env_num_env_old: usize,
    t_q: [usize; 3],
    df_env: [bool; 8],
    df_noise: [bool; 2],
    invf_mode: [[u8; 5]; 2],
    env_facs_q: [[i32; 64]; 9],
    env_facs: [[f32; 64]; 9],
    noise_facs_q: [[i32; 5]; 3],
    noise_facs: [[f32; 5]; 3],
    add_harmonic_flag: bool,
    add_harmonic: [bool; 64],
    e_a: [i32; 2],
    s_indexmapped: [[u8; 64]; 9],
    g_temp: Vec<[f32; 64]>, // 42 slots
    q_temp: Vec<[f32; 64]>, // 42 slots
    f_indexnoise: usize,
    f_indexsine: usize,
    bw_array: [f32; 5],

    // Filterbank / signal history.
    /// Analysis input, 320 history + 1024 new samples (chronological).
    x: Vec<f32>,
    /// Synthesis buffer, 1280 samples, newest first.
    v: Vec<f32>,
    /// Analysis output of this and the previous frame, [slot][32].
    w: [Vec<C>; 2],
    w_cur: usize,
    /// HF-adjusted signal of this and the previous frame, [38][64].
    y: [Vec<C>; 2],
    y_cur: usize,
}

impl ChannelData {
    fn new() -> Self {
        Self {
            frame_class: 0,
            num_env: 0,
            num_noise: 0,
            freq_res: [0; 9],
            amp_res: false,
            t_env: [0; 9],
            t_env_num_env_old: 0,
            t_q: [0; 3],
            df_env: [false; 8],
            df_noise: [false; 2],
            invf_mode: [[0; 5]; 2],
            env_facs_q: [[0; 64]; 9],
            env_facs: [[0.0; 64]; 9],
            noise_facs_q: [[0; 5]; 3],
            noise_facs: [[0.0; 5]; 3],
            add_harmonic_flag: false,
            add_harmonic: [false; 64],
            e_a: [-1, -1],
            s_indexmapped: [[0; 64]; 9],
            g_temp: vec![[0.0; 64]; 42],
            q_temp: vec![[0.0; 64]; 42],
            f_indexnoise: 0,
            f_indexsine: 0,
            bw_array: [0.0; 5],
            x: vec![0.0; 320 + 1024],
            v: vec![0.0; 1280],
            w: [vec![C::default(); SLOTS * 32], vec![C::default(); SLOTS * 32]],
            w_cur: 0,
            y: [vec![C::default(); 38 * 64], vec![C::default(); 38 * 64]],
            y_cur: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SpectrumParams {
    start_freq: i32,
    stop_freq: i32,
    xover_band: i32,
    freq_scale: i32,
    alter_scale: i32,
    noise_bands: i32,
}

const PARAMS_UNSET: SpectrumParams = SpectrumParams {
    start_freq: -1,
    stop_freq: -1,
    xover_band: -1,
    freq_scale: -1,
    alter_scale: -1,
    noise_bands: -1,
};

/// SBR state of one channel element (SCE or CPE).
struct Element {
    sample_rate: u32,
    start: bool,
    reset: bool,
    ready: bool,
    is_pair: bool,
    coupling: bool,
    ps_seen: bool,

    amp_res_header: bool,
    params: SpectrumParams,
    limiter_bands: u32,
    limiter_gains: u32,
    interpol_freq: bool,
    smoothing_mode: bool,

    k: [usize; 3],
    kx: [usize; 2],
    m: [usize; 2],
    n: [usize; 2],
    n_master: usize,
    n_q: usize,
    n_lim: usize,
    f_master: [usize; 72],
    f_tablehigh: [usize; 72],
    f_tablelow: [usize; 40],
    f_tablenoise: [usize; 6],
    f_tablelim: [usize; 72],
    num_patches: usize,
    patch_num_subbands: [usize; 7],
    patch_start_subband: [usize; 7],

    ch: [ChannelData; 2],
}

impl Element {
    fn new(sample_rate: u32, is_pair: bool) -> Self {
        let mut e = Self {
            sample_rate,
            start: false,
            reset: false,
            ready: false,
            is_pair,
            coupling: false,
            ps_seen: false,
            amp_res_header: false,
            params: PARAMS_UNSET,
            limiter_bands: 0,
            limiter_gains: 0,
            interpol_freq: false,
            smoothing_mode: false,
            k: [0; 3],
            kx: [32, 32],
            m: [0, 0],
            n: [0; 2],
            n_master: 0,
            n_q: 0,
            n_lim: 0,
            f_master: [0; 72],
            f_tablehigh: [0; 72],
            f_tablelow: [0; 40],
            f_tablenoise: [0; 6],
            f_tablelim: [0; 72],
            num_patches: 0,
            patch_num_subbands: [0; 7],
            patch_start_subband: [0; 7],
            ch: [ChannelData::new(), ChannelData::new()],
        };
        e.turn_off();
        e
    }

    /// Pure upsampling mode (no usable SBR data).
    fn turn_off(&mut self) {
        self.start = false;
        self.ready = false;
        self.kx[1] = 32;
        self.m[1] = 0;
        self.ch[0].e_a[1] = -1;
        self.ch[1].e_a[1] = -1;
        self.params = PARAMS_UNSET;
    }

    // -------------------------------------------------------------------
    // Frequency band tables (4.6.18.3)

    fn make_f_master(&mut self) -> Result<(), ()> {
        let sr = self.sample_rate;
        let offs = match sr {
            16000 => &START_OFFSET[0],
            22050 => &START_OFFSET[1],
            24000 => &START_OFFSET[2],
            32000 => &START_OFFSET[3],
            44100 | 48000 | 64000 => &START_OFFSET[4],
            88200 | 96000 | 128000 | 176400 | 192000 => &START_OFFSET[5],
            _ => return Err(()),
        };
        let temp: u32 = if sr < 32000 {
            3000
        } else if sr < 64000 {
            4000
        } else {
            5000
        };
        let start_min = ((temp << 7) + (sr >> 1)) / sr;
        let stop_min = ((temp << 8) + (sr >> 1)) / sr;
        let p = self.params;
        let k0 = start_min as i32 + i32::from(offs[(p.start_freq & 15) as usize]);
        if k0 <= 0 {
            return Err(());
        }
        let k0 = k0 as usize;
        self.k[0] = k0;

        let k2 = if p.stop_freq < 14 {
            let mut stop_dk = [0i32; 13];
            make_bands(&mut stop_dk, stop_min as i32, 64, 13);
            stop_dk.sort_unstable();
            let mut k2 = stop_min as i32;
            for d in stop_dk.iter().take(p.stop_freq.max(0) as usize) {
                k2 += d;
            }
            k2.max(0) as usize
        } else if p.stop_freq == 14 {
            2 * k0
        } else {
            3 * k0
        }
        .min(64);
        self.k[2] = k2;

        let max_qmf_subbands = if sr <= 32000 {
            48
        } else if sr == 44100 {
            35
        } else {
            32
        };
        if k2 <= k0 || k2 - k0 > max_qmf_subbands {
            return Err(());
        }

        if p.freq_scale == 0 {
            let dk = (p.alter_scale + 1) as usize;
            let n_master = ((k2 - k0 + (dk & 2)) >> dk) << 1;
            if n_master == 0 || n_master > 64 || p.xover_band as usize >= n_master {
                return Err(());
            }
            self.n_master = n_master;
            let mut fm = [0i32; 72];
            for v in fm.iter_mut().take(n_master + 1).skip(1) {
                *v = dk as i32;
            }
            let k2diff = k2 as i32 - k0 as i32 - (n_master * dk) as i32;
            if k2diff < 0 {
                fm[1] -= 1;
                if k2diff < -1 {
                    fm[2] -= 1;
                }
            } else if k2diff > 0 {
                fm[n_master] += 1;
            }
            fm[0] = k0 as i32;
            for k in 1..=n_master {
                fm[k] += fm[k - 1];
            }
            for k in 0..=n_master {
                if fm[k] < 0 || (k > 0 && fm[k] < fm[k - 1]) {
                    return Err(());
                }
                self.f_master[k] = fm[k] as usize;
            }
        } else {
            let half_bands = 7 - p.freq_scale;
            let two_regions = 49 * k2 > 110 * k0;
            let k1 = if two_regions { 2 * k0 } else { k2 };
            self.k[1] = k1;
            let nb0 = (libm::rintf(half_bands as f32 * libm::log2f(k1 as f32 / k0 as f32)) as i32) * 2;
            if nb0 <= 0 || nb0 > 48 {
                return Err(());
            }
            let nb0 = nb0 as usize;
            let mut vk0 = [0i32; 50];
            make_bands(&mut vk0[1..=nb0], k0 as i32, k1 as i32, nb0);
            vk0[1..=nb0].sort_unstable();
            let vdk0_max = vk0[nb0];
            vk0[0] = k0 as i32;
            for k in 1..=nb0 {
                if vk0[k] <= 0 {
                    return Err(());
                }
                vk0[k] += vk0[k - 1];
            }
            if two_regions {
                let invwarp = if p.alter_scale != 0 { 0.769_230_77_f32 } else { 1.0 };
                let nb1 = (libm::rintf(half_bands as f32 * invwarp * libm::log2f(k2 as f32 / k1 as f32)) as i32) * 2;
                if nb1 <= 0 || nb1 > 48 {
                    return Err(());
                }
                let nb1 = nb1 as usize;
                let mut vk1 = [0i32; 50];
                make_bands(&mut vk1[1..=nb1], k1 as i32, k2 as i32, nb1);
                let vdk1_min = vk1[1..=nb1].iter().copied().min().unwrap_or(0);
                if vdk1_min < vdk0_max {
                    vk1[1..=nb1].sort_unstable();
                    let change = (vdk0_max - vk1[1]).min((vk1[nb1] - vk1[1]) >> 1);
                    vk1[1] += change;
                    vk1[nb1] -= change;
                }
                vk1[1..=nb1].sort_unstable();
                vk1[0] = k1 as i32;
                for k in 1..=nb1 {
                    if vk1[k] <= 0 {
                        return Err(());
                    }
                    vk1[k] += vk1[k - 1];
                }
                let n_master = nb0 + nb1;
                if n_master > 64 || p.xover_band as usize >= n_master {
                    return Err(());
                }
                self.n_master = n_master;
                for k in 0..=nb0 {
                    self.f_master[k] = vk0[k] as usize;
                }
                for k in 1..=nb1 {
                    self.f_master[nb0 + k] = vk1[k] as usize;
                }
            } else {
                if p.xover_band as usize >= nb0 {
                    return Err(());
                }
                self.n_master = nb0;
                for k in 0..=nb0 {
                    self.f_master[k] = vk0[k] as usize;
                }
            }
        }
        Ok(())
    }

    /// Patch construction (4.6.18.6.3, figure 4.48).
    fn calc_patches(&mut self) -> Result<(), ()> {
        let k0 = self.k[0] as i32;
        let mut msb = k0;
        let mut usb = self.kx[1] as i32;
        let goal_sb = (((1000u32 << 11) + (self.sample_rate >> 1)) / self.sample_rate) as i32;
        let end = (self.kx[1] + self.m[1]) as i32;
        self.num_patches = 0;

        let mut k = if goal_sb < end {
            let mut k = 0;
            while k < self.n_master && (self.f_master[k] as i32) < goal_sb {
                k += 1;
            }
            k
        } else {
            self.n_master
        };

        let mut last_k: i64 = -1;
        let mut last_msb = -1;
        for _ in 0..64 {
            let mut odd = 0;
            if k as i64 == last_k && msb == last_msb {
                return Err(());
            }
            last_k = k as i64;
            last_msb = msb;
            let mut i = k as i64;
            let mut sb = self.f_master[k] as i32;
            while i >= 0 {
                sb = self.f_master[i as usize] as i32;
                odd = (sb + k0) & 1;
                if sb <= k0 - 1 + msb - odd {
                    break;
                }
                i -= 1;
            }
            if self.num_patches > 5 {
                return Err(());
            }
            let nsub = (sb - usb).max(0);
            let start = k0 - odd - nsub;
            if start < 0 {
                return Err(());
            }
            self.patch_num_subbands[self.num_patches] = nsub as usize;
            self.patch_start_subband[self.num_patches] = start as usize;
            if nsub > 0 {
                usb = sb;
                msb = sb;
                self.num_patches += 1;
            } else {
                msb = self.kx[1] as i32;
            }
            if (self.f_master[k] as i32) - sb < 3 {
                k = self.n_master;
            }
            if sb == end {
                if self.num_patches > 1 && self.patch_num_subbands[self.num_patches - 1] < 3 {
                    self.num_patches -= 1;
                }
                return Ok(());
            }
        }
        Err(())
    }

    fn make_f_derived(&mut self) -> Result<(), ()> {
        let xover = self.params.xover_band as usize;
        let n1 = self.n_master - xover;
        let n0 = (n1 + 1) >> 1;
        self.n = [n0, n1];
        for i in 0..=n1 {
            self.f_tablehigh[i] = self.f_master[xover + i];
        }
        if self.f_tablehigh[n1] < self.f_tablehigh[0] {
            return Err(());
        }
        self.m[1] = self.f_tablehigh[n1] - self.f_tablehigh[0];
        self.kx[1] = self.f_tablehigh[0];
        if self.kx[1] + self.m[1] > 64 || self.kx[1] > 32 || n0 >= self.f_tablelow.len() {
            return Err(());
        }
        self.f_tablelow[0] = self.f_tablehigh[0];
        let temp = n1 & 1;
        for k in 1..=n0 {
            self.f_tablelow[k] = self.f_tablehigh[2 * k - temp];
        }
        let nq = libm::rintf(self.params.noise_bands as f32 * libm::log2f(self.k[2] as f32 / self.kx[1] as f32));
        let nq = if nq.is_finite() { (nq as i32).max(1) as usize } else { 1 };
        if nq > 5 {
            self.n_q = 1;
            return Err(());
        }
        self.n_q = nq;
        self.f_tablenoise[0] = self.f_tablelow[0];
        let mut temp = 0usize;
        for k in 1..=nq {
            temp += (n0 - temp) / (nq + 1 - k);
            self.f_tablenoise[k] = self.f_tablelow[temp];
        }
        self.calc_patches()?;
        self.make_f_tablelim();
        self.ch[0].f_indexnoise = 0;
        self.ch[1].f_indexnoise = 0;
        Ok(())
    }

    /// Limiter frequency band table (4.6.18.3.2.4).
    fn make_f_tablelim(&mut self) {
        let n0 = self.n[0];
        if self.limiter_bands > 0 {
            const WARPED: [f32; 3] = [1.327_151_8, 1.185_092_8, 1.119_871_6];
            let lim = WARPED[(self.limiter_bands - 1) as usize % 3];
            let np = self.num_patches;
            let mut patch_borders = [0usize; 7];
            patch_borders[0] = self.kx[1];
            for k in 1..=np {
                patch_borders[k] = patch_borders[k - 1] + self.patch_num_subbands[k - 1];
            }
            let mut t: Vec<usize> = Vec::with_capacity(n0 + np + 1);
            t.extend_from_slice(&self.f_tablelow[..=n0]);
            if np > 1 {
                t.extend_from_slice(&patch_borders[1..np]);
            }
            t.sort_unstable();
            let in_pb = |v: usize| patch_borders[..=np].contains(&v);
            let mut n_lim = (n0 + np).saturating_sub(1);
            let mut out = 0usize;
            let mut inp = 1usize;
            while out < n_lim && inp < t.len() {
                if t[inp] as f32 >= t[out] as f32 * lim {
                    out += 1;
                    t[out] = t[inp];
                    inp += 1;
                } else if t[inp] == t[out] || !in_pb(t[inp]) {
                    inp += 1;
                    n_lim -= 1;
                } else if !in_pb(t[out]) {
                    t[out] = t[inp];
                    inp += 1;
                    n_lim -= 1;
                } else {
                    out += 1;
                    t[out] = t[inp];
                    inp += 1;
                }
            }
            self.n_lim = n_lim.min(t.len().saturating_sub(1));
            for (i, v) in t.iter().enumerate().take(self.f_tablelim.len()) {
                self.f_tablelim[i] = *v;
            }
        } else {
            self.f_tablelim[0] = self.f_tablelow[0];
            self.f_tablelim[1] = self.f_tablelow[n0];
            self.n_lim = 1;
        }
    }

    fn do_reset(&mut self) {
        if self.make_f_master().is_err() || self.make_f_derived().is_err() {
            self.turn_off();
        }
    }

    // -------------------------------------------------------------------
    // Bitstream (4.4.2.8)

    fn read_header(&mut self, bs: &mut BitReader<'_>) -> Result<(), AacError> {
        self.start = true;
        self.ready = false;
        let old = self.params;
        let old_limiter = self.limiter_bands;
        self.amp_res_header = bs.read_bool()?;
        let mut p = SpectrumParams {
            start_freq: bs.read(4)? as i32,
            stop_freq: bs.read(4)? as i32,
            xover_band: bs.read(3)? as i32,
            freq_scale: 2,
            alter_scale: 1,
            noise_bands: 2,
        };
        bs.skip(2)?;
        let extra1 = bs.read_bool()?;
        let extra2 = bs.read_bool()?;
        if extra1 {
            p.freq_scale = bs.read(2)? as i32;
            p.alter_scale = bs.read(1)? as i32;
            p.noise_bands = bs.read(2)? as i32;
        }
        self.params = p;
        if p != old {
            self.reset = true;
        }
        if extra2 {
            self.limiter_bands = bs.read(2)?;
            self.limiter_gains = bs.read(2)?;
            self.interpol_freq = bs.read_bool()?;
            self.smoothing_mode = bs.read_bool()?;
        } else {
            self.limiter_bands = 2;
            self.limiter_gains = 2;
            self.interpol_freq = true;
            self.smoothing_mode = true;
        }
        if self.limiter_bands != old_limiter && !self.reset && self.n_master > 0 {
            self.make_f_tablelim();
        }
        Ok(())
    }

    fn read_grid(&mut self, bs: &mut BitReader<'_>, c: usize) -> Result<(), AacError> {
        let amp_res_header = self.amp_res_header;
        let d = &mut self.ch[c];
        let num_env_old = d.num_env;
        let mut abs_bord_trail = NUM_TIME_SLOTS;
        let mut bs_pointer = 0usize;
        const CEIL_LOG2: [u32; 6] = [0, 1, 2, 2, 3, 3];

        d.freq_res[0] = d.freq_res[d.num_env];
        d.amp_res = amp_res_header;
        d.t_env_num_env_old = d.t_env[num_env_old];

        let class = bs.read(2)?;
        match class {
            FIXFIX => {
                let n = 1usize << bs.read(2)?;
                if n > 5 {
                    return Err(AacError::InvalidData("sbr: too many envelopes"));
                }
                d.num_env = n;
                if n == 1 {
                    d.amp_res = false;
                }
                d.t_env[0] = 0;
                d.t_env[n] = abs_bord_trail;
                let step = (abs_bord_trail + (n >> 1)) / n;
                for i in 0..n - 1 {
                    d.t_env[i + 1] = d.t_env[i] + step;
                }
                let r = bs.read(1)? as usize;
                for i in 1..=n {
                    d.freq_res[i] = r;
                }
            }
            FIXVAR => {
                abs_bord_trail += bs.read(2)? as usize;
                let num_rel_trail = bs.read(2)? as usize;
                let n = num_rel_trail + 1;
                d.num_env = n;
                d.t_env[0] = 0;
                d.t_env[n] = abs_bord_trail;
                for i in (1..=num_rel_trail).rev() {
                    let dlt = 2 * bs.read(2)? as usize + 2;
                    d.t_env[i] = d.t_env[i + 1].checked_sub(dlt).ok_or(AacError::InvalidData("sbr: borders"))?;
                }
                bs_pointer = bs.read(CEIL_LOG2[n])? as usize;
                for i in 0..n {
                    d.freq_res[n - i] = bs.read(1)? as usize;
                }
            }
            VARFIX => {
                d.t_env[0] = bs.read(2)? as usize;
                let num_rel_lead = bs.read(2)? as usize;
                let n = num_rel_lead + 1;
                d.num_env = n;
                d.t_env[n] = abs_bord_trail;
                for i in 0..num_rel_lead {
                    d.t_env[i + 1] = d.t_env[i] + 2 * bs.read(2)? as usize + 2;
                }
                bs_pointer = bs.read(CEIL_LOG2[n])? as usize;
                for i in 1..=n {
                    d.freq_res[i] = bs.read(1)? as usize;
                }
            }
            _ => {
                // VARVAR
                d.t_env[0] = bs.read(2)? as usize;
                abs_bord_trail += bs.read(2)? as usize;
                let num_rel_lead = bs.read(2)? as usize;
                let num_rel_trail = bs.read(2)? as usize;
                let n = num_rel_lead + num_rel_trail + 1;
                if n > 5 {
                    return Err(AacError::InvalidData("sbr: too many envelopes"));
                }
                d.num_env = n;
                d.t_env[n] = abs_bord_trail;
                for i in 0..num_rel_lead {
                    d.t_env[i + 1] = d.t_env[i] + 2 * bs.read(2)? as usize + 2;
                }
                for i in (n - num_rel_trail..n).rev() {
                    let dlt = 2 * bs.read(2)? as usize + 2;
                    d.t_env[i] = d.t_env[i + 1].checked_sub(dlt).ok_or(AacError::InvalidData("sbr: borders"))?;
                }
                bs_pointer = bs.read(CEIL_LOG2[n])? as usize;
                for i in 1..=n {
                    d.freq_res[i] = bs.read(1)? as usize;
                }
            }
        }
        d.frame_class = class;

        if bs_pointer > d.num_env + 1 {
            return Err(AacError::InvalidData("sbr: bs_pointer"));
        }
        for i in 1..=d.num_env {
            if d.t_env[i - 1] >= d.t_env[i] {
                return Err(AacError::InvalidData("sbr: borders not monotone"));
            }
        }
        d.num_noise = if d.num_env > 1 { 2 } else { 1 };
        d.t_q[0] = d.t_env[0];
        d.t_q[d.num_noise] = d.t_env[d.num_env];
        if d.num_noise > 1 {
            let idx = if class == FIXFIX {
                d.num_env >> 1
            } else if class & 1 == 1 {
                d.num_env - (bs_pointer as isize - 1).max(1) as usize
            } else if bs_pointer == 0 {
                1
            } else if bs_pointer == 1 {
                d.num_env - 1
            } else {
                bs_pointer - 1
            };
            d.t_q[1] = d.t_env[idx.min(d.num_env)];
        }
        d.e_a[0] = if d.e_a[1] != num_env_old as i32 { -1 } else { 0 };
        d.e_a[1] = -1;
        if class & 1 == 1 && bs_pointer != 0 {
            d.e_a[1] = (d.num_env + 1 - bs_pointer) as i32;
        } else if class == VARFIX && bs_pointer > 1 {
            d.e_a[1] = bs_pointer as i32 - 1;
        }
        Ok(())
    }

    /// Coupled stereo: channel 1 shares channel 0's time grid.
    fn copy_grid(&mut self) {
        let (a, b) = self.ch.split_at_mut(1);
        let src = &a[0];
        let dst = &mut b[0];
        dst.freq_res[0] = dst.freq_res[dst.num_env];
        dst.t_env_num_env_old = dst.t_env[dst.num_env];
        dst.e_a[0] = if dst.e_a[1] != dst.num_env as i32 { -1 } else { 0 };
        dst.freq_res[1..].copy_from_slice(&src.freq_res[1..]);
        dst.t_env = src.t_env;
        dst.t_q = src.t_q;
        dst.num_env = src.num_env;
        dst.amp_res = src.amp_res;
        dst.num_noise = src.num_noise;
        dst.frame_class = src.frame_class;
        dst.e_a[1] = src.e_a[1];
    }

    fn read_dtdf(&mut self, bs: &mut BitReader<'_>, c: usize) -> Result<(), AacError> {
        let d = &mut self.ch[c];
        for i in 0..d.num_env {
            d.df_env[i] = bs.read_bool()?;
        }
        for i in 0..d.num_noise {
            d.df_noise[i] = bs.read_bool()?;
        }
        Ok(())
    }

    fn read_invf(&mut self, bs: &mut BitReader<'_>, c: usize) -> Result<(), AacError> {
        let nq = self.n_q;
        let d = &mut self.ch[c];
        d.invf_mode[1] = d.invf_mode[0];
        for i in 0..nq {
            d.invf_mode[0][i] = bs.read(2)? as u8;
        }
        Ok(())
    }

    fn read_envelope(&mut self, bs: &mut BitReader<'_>, books: &Books, c: usize) -> Result<(), AacError> {
        let coupled = self.coupling && c == 1;
        let delta: i32 = if coupled { 2 } else { 1 };
        let odd = self.n[1] & 1;
        let n = self.n;
        let d = &mut self.ch[c];
        let (bits, t_huff, f_huff) = match (coupled, d.amp_res) {
            (true, true) => (5, &books.t_env_bal_30, &books.f_env_bal_30),
            (true, false) => (6, &books.t_env_bal_15, &books.f_env_bal_15),
            (false, true) => (6, &books.t_env_30, &books.f_env_30),
            (false, false) => (7, &books.t_env_15, &books.f_env_15),
        };
        for i in 0..d.num_env {
            let res = d.freq_res[i + 1];
            let nb = n[res];
            if d.df_env[i] {
                for j in 0..nb {
                    let k = if res == d.freq_res[i] {
                        j
                    } else if res == 1 {
                        (j + odd) >> 1
                    } else if j == 0 {
                        0
                    } else {
                        2 * j - odd
                    };
                    let v = d.env_facs_q[i][k.min(63)] + delta * read_sym(bs, t_huff)?;
                    if !(0..=127).contains(&v) {
                        return Err(AacError::InvalidData("sbr: envelope out of range"));
                    }
                    d.env_facs_q[i + 1][j] = v;
                }
            } else {
                d.env_facs_q[i + 1][0] = delta * bs.read(bits)? as i32;
                for j in 1..nb {
                    let v = d.env_facs_q[i + 1][j - 1] + delta * read_sym(bs, f_huff)?;
                    if !(0..=127).contains(&v) {
                        return Err(AacError::InvalidData("sbr: envelope out of range"));
                    }
                    d.env_facs_q[i + 1][j] = v;
                }
            }
        }
        d.env_facs_q[0] = d.env_facs_q[d.num_env];
        Ok(())
    }

    fn read_noise(&mut self, bs: &mut BitReader<'_>, books: &Books, c: usize) -> Result<(), AacError> {
        let coupled = self.coupling && c == 1;
        let delta: i32 = if coupled { 2 } else { 1 };
        let nq = self.n_q;
        let d = &mut self.ch[c];
        let (t_huff, f_huff) = if coupled {
            (&books.t_noise_bal_30, &books.f_env_bal_30)
        } else {
            (&books.t_noise_30, &books.f_env_30)
        };
        for i in 0..d.num_noise {
            if d.df_noise[i] {
                for j in 0..nq {
                    let v = d.noise_facs_q[i][j] + delta * read_sym(bs, t_huff)?;
                    if !(0..=30).contains(&v) {
                        return Err(AacError::InvalidData("sbr: noise out of range"));
                    }
                    d.noise_facs_q[i + 1][j] = v;
                }
            } else {
                d.noise_facs_q[i + 1][0] = delta * bs.read(5)? as i32;
                for j in 1..nq {
                    let v = d.noise_facs_q[i + 1][j - 1] + delta * read_sym(bs, f_huff)?;
                    if !(0..=30).contains(&v) {
                        return Err(AacError::InvalidData("sbr: noise out of range"));
                    }
                    d.noise_facs_q[i + 1][j] = v;
                }
            }
        }
        d.noise_facs_q[0] = d.noise_facs_q[d.num_noise];
        Ok(())
    }

    fn read_harmonics(&mut self, bs: &mut BitReader<'_>, c: usize) -> Result<(), AacError> {
        let n1 = self.n[1];
        let d = &mut self.ch[c];
        d.add_harmonic_flag = bs.read_bool()?;
        if d.add_harmonic_flag {
            for i in 0..n1 {
                d.add_harmonic[i] = bs.read_bool()?;
            }
        }
        Ok(())
    }

    fn read_data(&mut self, bs: &mut BitReader<'_>, books: &Books) -> Result<(), AacError> {
        if !self.is_pair {
            if bs.read_bool()? {
                bs.skip(4)?;
            }
            self.read_grid(bs, 0)?;
            self.read_dtdf(bs, 0)?;
            self.read_invf(bs, 0)?;
            self.read_envelope(bs, books, 0)?;
            self.read_noise(bs, books, 0)?;
            self.read_harmonics(bs, 0)?;
        } else {
            if bs.read_bool()? {
                bs.skip(8)?;
            }
            self.coupling = bs.read_bool()?;
            if self.coupling {
                self.read_grid(bs, 0)?;
                self.copy_grid();
                self.read_dtdf(bs, 0)?;
                self.read_dtdf(bs, 1)?;
                self.read_invf(bs, 0)?;
                self.ch[1].invf_mode[1] = self.ch[1].invf_mode[0];
                self.ch[1].invf_mode[0] = self.ch[0].invf_mode[0];
                self.read_envelope(bs, books, 0)?;
                self.read_noise(bs, books, 0)?;
                self.read_envelope(bs, books, 1)?;
                self.read_noise(bs, books, 1)?;
            } else {
                self.read_grid(bs, 0)?;
                self.read_grid(bs, 1)?;
                self.read_dtdf(bs, 0)?;
                self.read_dtdf(bs, 1)?;
                self.read_invf(bs, 0)?;
                self.read_invf(bs, 1)?;
                self.read_envelope(bs, books, 0)?;
                self.read_envelope(bs, books, 1)?;
                self.read_noise(bs, books, 0)?;
                self.read_noise(bs, books, 1)?;
            }
            self.read_harmonics(bs, 0)?;
            self.read_harmonics(bs, 1)?;
        }
        if bs.read_bool()? {
            // bs_extended_data
            let mut cnt = bs.read(4)? as usize;
            if cnt == 15 {
                cnt += bs.read(8)? as usize;
            }
            let bits_left = cnt * 8;
            if bits_left > 7 {
                let id = bs.read(2)?;
                if id == 2 {
                    // EXTENSION_ID_PS: parametric stereo, not reconstructed.
                    self.ps_seen = true;
                }
                bs.skip(bits_left - 2)?;
            } else {
                bs.skip(bits_left)?;
            }
        }
        Ok(())
    }

    /// Parse one SBR extension payload (`bs` positioned at the 4-bit extension type).
    fn parse_extension(&mut self, bs: &mut BitReader<'_>, books: &Books) {
        self.reset = false;
        let ext_type = match bs.read(4) {
            Ok(t) => t,
            Err(_) => {
                self.turn_off();
                return;
            }
        };
        if ext_type == 14 && bs.skip(10).is_err() {
            self.turn_off();
            return;
        }
        let header = match bs.read_bool() {
            Ok(h) => h,
            Err(_) => {
                self.turn_off();
                return;
            }
        };
        if header && self.read_header(bs).is_err() {
            self.turn_off();
            return;
        }
        if self.reset {
            self.do_reset();
        }
        if self.start {
            self.ready = true;
            if self.read_data(bs, books).is_err() {
                self.turn_off();
            }
        }
    }

    // -------------------------------------------------------------------
    // Dequantisation and stereo decoding (4.6.18.3.5)

    fn dequant(&mut self) {
        let n = self.n;
        let nq = self.n_q;
        let sqrt2 = |q: i32| if q & 1 != 0 { core::f32::consts::SQRT_2 } else { 1.0 };
        if self.is_pair && self.coupling {
            let amp = self.ch[0].amp_res;
            let pan_offset = if amp { 12 } else { 24 };
            let (a, b) = self.ch.split_at_mut(1);
            let (d0, d1) = (&mut a[0], &mut b[0]);
            for e in 1..=d0.num_env {
                for k in 0..n[d0.freq_res[e]] {
                    let q0 = d0.env_facs_q[e][k];
                    let q1 = pan_offset - d1.env_facs_q[e][k];
                    let (mut t1, t2) = if amp {
                        (exp2i(q0 + 7), exp2i(q1))
                    } else {
                        (exp2i((q0 >> 1) + 7) * sqrt2(q0), exp2i(q1 >> 1) * sqrt2(q1))
                    };
                    if t1 > 1e20 {
                        t1 = 1.0;
                    }
                    let fac = t1 / (1.0 + t2);
                    d0.env_facs[e][k] = fac;
                    d1.env_facs[e][k] = fac * t2;
                }
            }
            for e in 1..=d0.num_noise {
                for k in 0..nq {
                    let t1 = exp2i(NOISE_FLOOR_OFFSET - d0.noise_facs_q[e][k] + 1);
                    let t2 = exp2i(12 - d1.noise_facs_q[e][k]);
                    let fac = t1 / (1.0 + t2);
                    d0.noise_facs[e][k] = fac;
                    d1.noise_facs[e][k] = fac * t2;
                }
            }
        } else {
            let nch = if self.is_pair { 2 } else { 1 };
            for d in self.ch.iter_mut().take(nch) {
                for e in 1..=d.num_env {
                    for k in 0..n[d.freq_res[e]] {
                        let q = d.env_facs_q[e][k];
                        let mut v = if d.amp_res { exp2i(q + 6) } else { exp2i((q >> 1) + 6) * sqrt2(q) };
                        if v > 1e20 {
                            v = 1.0;
                        }
                        d.env_facs[e][k] = v;
                    }
                }
                for e in 1..=d.num_noise {
                    for k in 0..nq {
                        d.noise_facs[e][k] = exp2i(NOISE_FLOOR_OFFSET - d.noise_facs_q[e][k]);
                    }
                }
            }
        }
    }
}

/// Scratch buffers for one channel-frame.
struct Scratch {
    x_low: Vec<C>,  // [32][40]
    x_high: Vec<C>, // [64][40]
    x_out: Vec<C>,  // [38][64]
    alpha0: [C; 64],
    alpha1: [C; 64],
    e_origmapped: [[f32; 64]; 8],
    q_mapped: [[f32; 64]; 8],
    s_mapped: [[u8; 64]; 8],
    e_curr: [[f32; 64]; 8],
    q_m: [[f32; 64]; 8],
    s_m: [[f32; 64]; 8],
    gain: [[f32; 64]; 8],
}

impl Scratch {
    fn new() -> Self {
        Self {
            x_low: vec![C::default(); 32 * 40],
            x_high: vec![C::default(); 64 * 40],
            x_out: vec![C::default(); 38 * 64],
            alpha0: [C::default(); 64],
            alpha1: [C::default(); 64],
            e_origmapped: [[0.0; 64]; 8],
            q_mapped: [[0.0; 64]; 8],
            s_mapped: [[0; 64]; 8],
            e_curr: [[0.0; 64]; 8],
            q_m: [[0.0; 64]; 8],
            s_m: [[0.0; 64]; 8],
            gain: [[0.0; 64]; 8],
        }
    }
}

/// SBR decoder for all channel elements of a stream.
pub struct SbrDecoder {
    sample_rate: u32,
    books: Books,
    qmf: Qmf,
    elements: Vec<Option<Box<Element>>>,
    scratch: Box<Scratch>,
    ps_seen: bool,
}

impl SbrDecoder {
    /// `core_rate`: AAC core sampling rate; `max_elements`: channel elements.
    pub fn new(core_rate: u32, max_elements: usize) -> Self {
        let mut elements = Vec::new();
        elements.resize_with(max_elements, || None);
        Self {
            sample_rate: core_rate * 2,
            books: Books::new(),
            qmf: Qmf::new(),
            elements,
            scratch: Box::new(Scratch::new()),
            ps_seen: false,
        }
    }

    /// Drop all inter-frame state.
    pub fn reset(&mut self) {
        for e in self.elements.iter_mut() {
            *e = None;
        }
    }

    /// Parametric stereo data was seen (and ignored).
    pub fn ps_seen(&self) -> bool {
        self.ps_seen
    }

    /// Process the SBR payload of element `idx` for this frame (`data`: the
    /// access unit plus the bit position / length of the FIL payload,
    /// starting at the 4-bit extension type), or `None` when there is none.
    pub fn element(&mut self, idx: usize, is_pair: bool, data: Option<(&[u8], usize, usize)>) -> Result<(), AacError> {
        if idx >= self.elements.len() {
            return Err(AacError::InvalidData("sbr: element index"));
        }
        let sr = self.sample_rate;
        let el = self.elements[idx].get_or_insert_with(|| Box::new(Element::new(sr, is_pair)));
        if el.is_pair != is_pair {
            **el = Element::new(sr, is_pair);
        }
        el.kx[0] = el.kx[1];
        el.m[0] = el.m[1];
        match data {
            Some((au, start, len)) => {
                let mut bs = BitReader::new_range(au, start, len);
                el.parse_extension(&mut bs, &self.books);
                if el.ps_seen {
                    self.ps_seen = true;
                }
            }
            None => {
                if el.start {
                    el.turn_off();
                }
            }
        }
        if el.start && el.ready {
            el.dequant();
            el.ready = false;
        } else if el.start {
            el.turn_off();
        }
        Ok(())
    }

    /// Run channel `c` (0/1) of element `idx`: `input` = 1024 core samples
    /// (-1..1), `out` = 2048 output samples.
    pub fn synthesize(&mut self, idx: usize, c: usize, input: &[f32], out: &mut [f32]) {
        let el = match self.elements.get_mut(idx).and_then(|e| e.as_mut()) {
            Some(e) => e,
            None => {
                out.fill(0.0);
                return;
            }
        };
        process_channel(el, c.min(1), &self.qmf, &mut self.scratch, input, out);
    }
}

fn make_bands(bands: &mut [i32], start: i32, stop: i32, num_bands: usize) {
    let base = libm::powf(stop as f32 / start as f32, 1.0 / num_bands as f32);
    let mut prod = start as f32;
    let mut previous = start;
    for b in bands.iter_mut().take(num_bands - 1) {
        prod *= base;
        let present = libm::rintf(prod) as i32;
        *b = present - previous;
        previous = present;
    }
    bands[num_bands - 1] = stop - previous;
}

// ---------------------------------------------------------------------------
// Signal processing for one channel of one element.

fn process_channel(el: &mut Element, c: usize, qmf: &Qmf, s: &mut Scratch, input: &[f32], out: &mut [f32]) {
    // QMF analysis (32 bands) of the core output, scaled to +-32768.
    {
        let d = &mut el.ch[c];
        d.x.copy_within(1024.., 0);
        for (dst, &v) in d.x[320..].iter_mut().zip(input.iter()) {
            *dst = v * 32768.0;
        }
        let wc = d.w_cur;
        let mut u = [0f32; 64];
        for l in 0..SLOTS {
            // x(n), n = 0..319, x(0) = newest sample of this slot.
            let newest = 320 + 32 * l + 31;
            for (n, un) in u.iter_mut().enumerate() {
                let mut acc = 0.0f32;
                for j in 0..5 {
                    let m = n + 64 * j;
                    acc += d.x[newest - m] * QMF_WINDOW[2 * m];
                }
                *un = acc;
            }
            for k in 0..32 {
                let row = &qmf.ana[k * 64..k * 64 + 64];
                let (mut re, mut im) = (0.0f32, 0.0f32);
                for n in 0..64 {
                    re += u[n] * row[n].re;
                    im += u[n] * row[n].im;
                }
                d.w[wc][l * 32 + k] = C::new(re, im);
            }
        }
    }

    // Low band X_low[k][i], i = 0..40.
    let kx0 = el.kx[0].min(32);
    let kx1 = el.kx[1].min(32);
    {
        let d = &el.ch[c];
        let (wc, wp) = (d.w_cur, d.w_cur ^ 1);
        s.x_low.fill(C::default());
        for k in 0..kx1 {
            for i in T_HFGEN..SLOTS + T_HFGEN {
                s.x_low[k * 40 + i] = d.w[wc][(i - T_HFGEN) * 32 + k];
            }
        }
        for k in 0..kx0 {
            for i in 0..T_HFGEN {
                s.x_low[k * 40 + i] = d.w[wp][(i + SLOTS - T_HFGEN) * 32 + k];
            }
        }
    }
    el.ch[c].w_cur ^= 1;
    el.ch[c].y_cur ^= 1;

    if el.start && el.ch[c].num_env > 0 {
        hf_inverse_filter(el, s);
        chirp(el, c);
        hf_gen(el, c, s);
        if mapping(el, c, s) {
            env_estimate(el, c, s);
            gain_calc(el, c, s);
            hf_assemble(el, c, s);
        }
    }

    // Assemble the 64-band signal for synthesis.
    {
        let d = &el.ch[c];
        let y1 = &d.y[d.y_cur];
        let y0 = &d.y[d.y_cur ^ 1];
        let i_temp = (2 * d.t_env_num_env_old).saturating_sub(SLOTS).min(6);
        let m0 = el.m[0];
        let m1 = el.m[1];
        s.x_out.fill(C::default());
        for k in 0..kx0 {
            for i in 0..i_temp {
                s.x_out[i * 64 + k] = s.x_low[k * 40 + i + T_HFADJ];
            }
        }
        for k in kx0..(kx0 + m0).min(64) {
            for i in 0..i_temp {
                s.x_out[i * 64 + k] = y0[(i + SLOTS) * 64 + k];
            }
        }
        for k in 0..kx1 {
            for i in i_temp..38 {
                s.x_out[i * 64 + k] = s.x_low[k * 40 + i + T_HFADJ];
            }
        }
        for k in kx1..(kx1 + m1).min(64) {
            for i in i_temp..SLOTS {
                s.x_out[i * 64 + k] = y1[i * 64 + k];
            }
        }
    }

    // QMF synthesis (64 bands).
    let d = &mut el.ch[c];
    for l in 0..SLOTS {
        d.v.copy_within(0..1280 - 128, 128);
        let xs = &s.x_out[l * 64..l * 64 + 64];
        let kmax = xs.iter().rposition(|x| x.re != 0.0 || x.im != 0.0).map_or(0, |k| k + 1);
        for n in 0..128 {
            let row = &qmf.syn[n * 64..n * 64 + kmax];
            let mut acc = 0.0f32;
            for (x, r) in xs[..kmax].iter().zip(row) {
                acc += x.re * r.re - x.im * r.im;
            }
            d.v[n] = acc;
        }
        let o = &mut out[l * 64..l * 64 + 64];
        for (j, oj) in o.iter_mut().enumerate() {
            let mut acc = 0.0f32;
            for i in 0..5 {
                acc += d.v[256 * i + j] * QMF_WINDOW[128 * i + j];
                acc += d.v[256 * i + 192 + j] * QMF_WINDOW[128 * i + 64 + j];
            }
            *oj = acc * (1.0 / 32768.0);
        }
    }
}

/// Inverse filtering: LPC predictor coefficients of each low subband (4.6.18.6.2).
fn hf_inverse_filter(el: &Element, s: &mut Scratch) {
    let k0 = el.k[0].min(32);
    for k in 0..k0 {
        let x = &s.x_low[k * 40..k * 40 + 40];
        // phi(i, j) = sum_{n=0}^{37} x(n - i + 2) conj(x(n - j + 2))
        let (mut p11, mut p22) = (0.0f32, 0.0f32);
        let (mut p01, mut p02, mut p12) = (C::default(), C::default(), C::default());
        for n in 0..38 {
            let a = x[n + 2];
            let b = x[n + 1];
            let cc = x[n];
            p11 += b.norm();
            p22 += cc.norm();
            p01 = p01.add(a.mul(b.conj()));
            p02 = p02.add(a.mul(cc.conj()));
            p12 = p12.add(b.mul(cc.conj()));
        }
        let dk = p22 * p11 - p12.norm() / 1.000001;
        let a1 = if dk == 0.0 {
            C::default()
        } else {
            let t = p01.mul(p12).add(p02.scale(-p11));
            C::new(t.re / dk, t.im / dk)
        };
        let a0 = if p11 == 0.0 {
            C::default()
        } else {
            let t = p01.add(a1.mul(p12.conj()));
            C::new(-t.re / p11, -t.im / p11)
        };
        if !(a1.norm() < 16.0 && a0.norm() < 16.0) {
            s.alpha0[k] = C::default();
            s.alpha1[k] = C::default();
        } else {
            s.alpha0[k] = a0;
            s.alpha1[k] = a1;
        }
    }
}

/// Chirp factors (4.6.18.6.2).
fn chirp(el: &mut Element, c: usize) {
    const BW: [f32; 4] = [0.0, 0.75, 0.9, 0.98];
    let nq = el.n_q;
    let d = &mut el.ch[c];
    for i in 0..nq {
        let mut new_bw = if d.invf_mode[0][i] + d.invf_mode[1][i] == 1 {
            0.6
        } else {
            BW[d.invf_mode[0][i] as usize & 3]
        };
        if new_bw < d.bw_array[i] {
            new_bw = 0.75 * new_bw + 0.25 * d.bw_array[i];
        } else {
            new_bw = 0.90625 * new_bw + 0.09375 * d.bw_array[i];
        }
        d.bw_array[i] = if new_bw < 0.015625 { 0.0 } else { new_bw };
    }
}

/// HF generator: patch low subbands up with the LPC predictor (4.6.18.6.3).
fn hf_gen(el: &Element, c: usize, s: &mut Scratch) {
    let d = &el.ch[c];
    let start = (2 * d.t_env[0] + T_HFADJ).max(2);
    let end = (2 * d.t_env[d.num_env] + T_HFADJ).min(40);
    let mut k = el.kx[1];
    s.x_high.fill(C::default());
    for j in 0..el.num_patches {
        for x in 0..el.patch_num_subbands[j] {
            let p = el.patch_start_subband[j] + x;
            if k >= 64 || p >= 32 {
                return;
            }
            let mut g = 0usize;
            while g <= el.n_q && k >= el.f_tablenoise[g] {
                g += 1;
            }
            let g = g.saturating_sub(1).min(4);
            let bw = d.bw_array[g];
            let a0 = s.alpha0[p].scale(bw);
            let a1 = s.alpha1[p].scale(bw * bw);
            let xl = &s.x_low[p * 40..p * 40 + 40];
            for i in start..end {
                s.x_high[k * 40 + i] = xl[i].add(a0.mul(xl[i - 1])).add(a1.mul(xl[i - 2]));
            }
            k += 1;
        }
    }
}

/// Map envelope / noise / sinusoid data onto QMF bands (4.6.18.7.2).
fn mapping(el: &mut Element, c: usize, s: &mut Scratch) -> bool {
    let kx = el.kx[1];
    let n = el.n;
    let d = &mut el.ch[c];
    for row in d.s_indexmapped[1..].iter_mut() {
        *row = [0; 64];
    }
    for e in 0..d.num_env.min(8) {
        let res = d.freq_res[e + 1];
        let ilim = n[res];
        let table: &[usize] = if res == 1 { &el.f_tablehigh } else { &el.f_tablelow };
        if table[0] != kx {
            return false;
        }
        for i in 0..ilim {
            for m in table[i]..table[i + 1] {
                if m >= kx && m - kx < 64 {
                    s.e_origmapped[e][m - kx] = d.env_facs[e + 1][i];
                }
            }
        }
        let k = (d.num_noise > 1 && d.t_env[e] >= d.t_q[1]) as usize;
        for i in 0..el.n_q {
            for m in el.f_tablenoise[i]..el.f_tablenoise[i + 1] {
                if m >= kx && m - kx < 64 {
                    s.q_mapped[e][m - kx] = d.noise_facs[k + 1][i];
                }
            }
        }
        if d.add_harmonic_flag {
            for i in 0..n[1] {
                let mid = (el.f_tablehigh[i] + el.f_tablehigh[i + 1]) >> 1;
                if mid < kx || mid - kx >= 64 {
                    continue;
                }
                let mm = mid - kx;
                let on = d.add_harmonic[i] && (e as i32 >= d.e_a[1] || d.s_indexmapped[0][mm] == 1);
                d.s_indexmapped[e + 1][mm] = on as u8;
            }
        }
        for i in 0..ilim {
            let lo = table[i].max(kx);
            let hi = table[i + 1].min(kx + 64);
            let present = (lo..hi).any(|m| d.s_indexmapped[e + 1][m - kx] != 0) as u8;
            for m in lo..hi {
                s.s_mapped[e][m - kx] = present;
            }
        }
    }
    d.s_indexmapped[0] = d.s_indexmapped[d.num_env];
    true
}

/// Estimate the energy of the generated HF signal (4.6.18.7.3).
fn env_estimate(el: &Element, c: usize, s: &mut Scratch) {
    let d = &el.ch[c];
    let kx = el.kx[1];
    for e in 0..d.num_env {
        let ilb = d.t_env[e] * 2 + T_HFADJ;
        let iub = (d.t_env[e + 1] * 2 + T_HFADJ).min(40);
        if ilb >= 40 {
            return;
        }
        if el.interpol_freq {
            let recip = 0.5 / (d.t_env[e + 1] - d.t_env[e]) as f32;
            for m in 0..el.m[1] {
                let row = &s.x_high[(m + kx) * 40..(m + kx) * 40 + 40];
                let sum: f32 = row[ilb..iub].iter().map(|v| v.norm()).sum();
                s.e_curr[e][m] = sum * recip;
            }
        } else {
            let env_size = 2 * (d.t_env[e + 1] - d.t_env[e]);
            let res = d.freq_res[e + 1];
            let table: &[usize] = if res == 1 { &el.f_tablehigh } else { &el.f_tablelow };
            for p in 0..el.n[res] {
                let den = (env_size * (table[p + 1] - table[p])) as f32;
                let mut sum = 0.0f32;
                for k in table[p]..table[p + 1].min(64) {
                    let row = &s.x_high[k * 40..k * 40 + 40];
                    sum += row[ilb..iub].iter().map(|v| v.norm()).sum::<f32>();
                }
                let sum = if den > 0.0 { sum / den } else { 0.0 };
                for k in table[p]..table[p + 1] {
                    if k >= kx && k - kx < 64 {
                        s.e_curr[e][k - kx] = sum;
                    }
                }
            }
        }
    }
}

/// Gains for the HF signal, noise floor and sinusoids, with the limiter (4.6.18.7.4/5).
fn gain_calc(el: &Element, c: usize, s: &mut Scratch) {
    const LIMGAIN: [f32; 4] = [0.70795, 1.0, 1.41254, 10000000000.0];
    let d = &el.ch[c];
    let kx = el.kx[1];
    for e in 0..d.num_env {
        let delta = !(e as i32 == d.e_a[1] || e as i32 == d.e_a[0]);
        for k in 0..el.n_lim {
            let lo = el.f_tablelim[k].saturating_sub(kx).min(64);
            let hi = el.f_tablelim[k + 1].saturating_sub(kx).min(64);
            for m in lo..hi {
                let q = s.q_mapped[e][m];
                let eo = s.e_origmapped[e][m];
                let temp = eo / (1.0 + q);
                s.q_m[e][m] = libm::sqrtf(temp * q);
                s.s_m[e][m] = libm::sqrtf(temp * f32::from(d.s_indexmapped[e + 1][m]));
                let g = if s.s_mapped[e][m] == 0 {
                    eo / ((1.0 + s.e_curr[e][m]) * (1.0 + if delta { q } else { 0.0 }))
                } else {
                    eo * q / ((1.0 + s.e_curr[e][m]) * (1.0 + q))
                };
                s.gain[e][m] = libm::sqrtf(g) + f32::MIN_POSITIVE;
            }
            let (mut s0, mut s1) = (0.0f32, 0.0f32);
            for m in lo..hi {
                s0 += s.e_origmapped[e][m];
                s1 += s.e_curr[e][m];
            }
            let gain_max = (LIMGAIN[el.limiter_gains as usize & 3]
                * libm::sqrtf((f32::EPSILON + s0) / (f32::EPSILON + s1)))
            .min(100000.0);
            for m in lo..hi {
                let q_m_max = s.q_m[e][m] * gain_max / s.gain[e][m];
                s.q_m[e][m] = s.q_m[e][m].min(q_m_max);
                s.gain[e][m] = s.gain[e][m].min(gain_max);
            }
            let (mut s0, mut s1) = (0.0f32, 0.0f32);
            for m in lo..hi {
                s0 += s.e_origmapped[e][m];
                s1 += s.e_curr[e][m] * s.gain[e][m] * s.gain[e][m]
                    + s.s_m[e][m] * s.s_m[e][m]
                    + if delta && s.s_m[e][m] == 0.0 { s.q_m[e][m] * s.q_m[e][m] } else { 0.0 };
            }
            let gain_boost = libm::sqrtf((f32::EPSILON + s0) / (f32::EPSILON + s1)).min(1.584893192);
            for m in lo..hi {
                s.gain[e][m] *= gain_boost;
                s.q_m[e][m] *= gain_boost;
                s.s_m[e][m] *= gain_boost;
            }
        }
    }
}

/// Assemble the adjusted HF signal: gain smoothing, noise floor, sinusoids (4.6.18.7.5).
fn hf_assemble(el: &mut Element, c: usize, s: &mut Scratch) {
    const H_SMOOTH: [f32; 5] = [0.33333333333333, 0.30150283239582, 0.21816949906249, 0.11516383427084, 0.03183050093751];
    let h_sl = if el.smoothing_mode { 0 } else { 4 };
    let kx = el.kx[1];
    let m_max = el.m[1].min(64 - kx.min(64));
    let reset = el.reset;
    let d = &mut el.ch[c];
    let mut indexnoise = d.f_indexnoise;
    let mut indexsine = d.f_indexsine;
    let t0 = 2 * d.t_env[0];

    if reset {
        for i in 0..h_sl {
            if i + t0 < 42 {
                d.g_temp[i + t0][..m_max].copy_from_slice(&s.gain[0][..m_max]);
                d.q_temp[i + t0][..m_max].copy_from_slice(&s.q_m[0][..m_max]);
            }
        }
    } else if h_sl > 0 {
        for i in 0..4 {
            let src = i + 2 * d.t_env_num_env_old;
            if i + t0 < 42 && src < 42 {
                d.g_temp[i + t0] = d.g_temp[src];
                d.q_temp[i + t0] = d.q_temp[src];
            }
        }
    }
    for e in 0..d.num_env {
        for i in 2 * d.t_env[e]..2 * d.t_env[e + 1] {
            if h_sl + i < 42 {
                d.g_temp[h_sl + i][..m_max].copy_from_slice(&s.gain[e][..m_max]);
                d.q_temp[h_sl + i][..m_max].copy_from_slice(&s.q_m[e][..m_max]);
            }
        }
    }

    let yc = d.y_cur;
    let mut g_filt = [0f32; 64];
    let mut q_filt = [0f32; 64];
    for e in 0..d.num_env {
        let transient = e as i32 == d.e_a[0] || e as i32 == d.e_a[1];
        for i in 2 * d.t_env[e]..2 * d.t_env[e + 1] {
            if i >= 38 || i + h_sl >= 42 {
                break;
            }
            if h_sl > 0 && !transient {
                let idx1 = i + h_sl;
                for m in 0..m_max {
                    let (mut g, mut q) = (0.0f32, 0.0f32);
                    for j in 0..=h_sl {
                        g += d.g_temp[idx1 - j][m] * H_SMOOTH[j];
                        q += d.q_temp[idx1 - j][m] * H_SMOOTH[j];
                    }
                    g_filt[m] = g;
                    q_filt[m] = q;
                }
            } else {
                g_filt[..m_max].copy_from_slice(&d.g_temp[i + h_sl][..m_max]);
                q_filt[..m_max].copy_from_slice(&d.q_temp[i][..m_max]);
            }

            let y = &mut d.y[yc][i * 64..i * 64 + 64];
            for m in 0..m_max {
                y[kx + m] = s.x_high[(kx + m) * 40 + i + T_HFADJ].scale(g_filt[m]);
            }
            // Sinusoid phase phi = [1, i, -1, -i][indexsine]; the imaginary
            // part alternates sign with the band index.
            let (sre, sim) = match indexsine {
                0 => (1.0f32, 0.0f32),
                1 => (0.0, 1.0),
                2 => (-1.0, 0.0),
                _ => (0.0, -1.0),
            };
            let mut noise = indexnoise;
            for m in 0..m_max {
                noise = (noise + 1) & 0x1ff;
                let alt = if (kx + m) & 1 == 1 { -1.0 } else { 1.0 };
                let sm = s.s_m[e][m];
                if sm != 0.0 || transient {
                    y[kx + m].re += sm * sre;
                    y[kx + m].im += sm * sim * alt;
                } else {
                    y[kx + m].re += q_filt[m] * NOISE_TABLE[noise][0];
                    y[kx + m].im += q_filt[m] * NOISE_TABLE[noise][1];
                }
            }
            indexnoise = (indexnoise + m_max) & 0x1ff;
            indexsine = (indexsine + 1) & 3;
        }
    }
    d.f_indexnoise = indexnoise;
    d.f_indexsine = indexsine;
}
