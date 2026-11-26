// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// Previous Author: Kostya Shishkov <kostya.shiskov@gmail.com>
//
// This source file includes code originally written for the NihAV
// project. With the author's permission, it has been relicensed for,
// and ported to the Symphonia project.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Ported to LingOS (no_std + alloc + libm) from symphonia-codec-aac 0.5.5
// (aac/ics/{mod,pulse,tns,ltp}.rs merged). Changes: crate-local bit reader and
// Huffman tables, max_sfb validation (garbage input could index past the band
// tables), the pulse re-quantisation fix (symphonia ignored the scale factor),
// and PNS noise generated exactly like FFmpeg (full 32-bit LCG values).

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::bits::BitReader;
use crate::common::*;
use crate::dsp;
use crate::huffman::Vlc;
use crate::tables::*;
use crate::AacError;

const ZERO_HCB: u8 = 0;
const RESERVED_HCB: u8 = 12;
const NOISE_HCB: u8 = 13;
const INTENSITY_HCB2: u8 = 14;
const INTENSITY_HCB: u8 = 15;

const INTENSITY_SCALE_MIN: i16 = -155;
const NORMAL_SCALE_MIN: i16 = -100;

macro_rules! validate {
    ($a:expr) => {
        if !$a {
            return Err(AacError::InvalidData(concat!("check failed: ", stringify!($a))));
        }
    };
}


/// Codebooks and dequantisation tables shared by every channel.
pub struct Books {
    spec: [Vlc; 11],
    scf: Vlc,
    /// y = x^(4/3)
    pow43: Box<[f32]>,
    /// 2^(0.25 * (i - 156))
    normal_scf: [f32; 256],
    /// 0.5^(0.25 * (i - 155))
    intensity_scf: [f32; 256],
}

impl Books {
    pub fn new() -> Self {
        let spec = [
            Vlc::new(&SPECTRUM_CODEBOOK1_CODES, &SPECTRUM_CODEBOOK1_LENS),
            Vlc::new(&SPECTRUM_CODEBOOK2_CODES, &SPECTRUM_CODEBOOK2_LENS),
            Vlc::new(&SPECTRUM_CODEBOOK3_CODES, &SPECTRUM_CODEBOOK3_LENS),
            Vlc::new(&SPECTRUM_CODEBOOK4_CODES, &SPECTRUM_CODEBOOK4_LENS),
            Vlc::new(&SPECTRUM_CODEBOOK5_CODES, &SPECTRUM_CODEBOOK5_LENS),
            Vlc::new(&SPECTRUM_CODEBOOK6_CODES, &SPECTRUM_CODEBOOK6_LENS),
            Vlc::new(&SPECTRUM_CODEBOOK7_CODES, &SPECTRUM_CODEBOOK7_LENS),
            Vlc::new(&SPECTRUM_CODEBOOK8_CODES, &SPECTRUM_CODEBOOK8_LENS),
            Vlc::new(&SPECTRUM_CODEBOOK9_CODES, &SPECTRUM_CODEBOOK9_LENS),
            Vlc::new(&SPECTRUM_CODEBOOK10_CODES, &SPECTRUM_CODEBOOK10_LENS),
            Vlc::new(&SPECTRUM_CODEBOOK11_CODES, &SPECTRUM_CODEBOOK11_LENS),
        ];
        let scf = Vlc::new(&SCF_CODEBOOK_CODES, &SCF_CODEBOOK_LENS);
        let pow43: Vec<f32> =
            (0..8192).map(|i| libm::pow(i as f64, 4.0 / 3.0) as f32).collect();
        let mut normal_scf = [0f32; 256];
        let mut intensity_scf = [0f32; 256];
        for i in 0..256 {
            normal_scf[i] = libm::pow(2.0, 0.25 * f64::from(i as i16 - 56 + NORMAL_SCALE_MIN)) as f32;
            intensity_scf[i] = libm::pow(0.5, 0.25 * f64::from(i as i16 + INTENSITY_SCALE_MIN)) as f32;
        }
        Self { spec, scf, pow43: pow43.into_boxed_slice(), normal_scf, intensity_scf }
    }

    #[inline(always)]
    fn iq(&self, v: u32) -> f32 {
        self.pow43[v as usize]
    }
}

#[derive(Clone)]
pub struct IcsInfo {
    pub window_sequence: u8,
    pub prev_window_sequence: u8,
    pub window_shape: bool,
    pub prev_window_shape: bool,
    pub scale_factor_grouping: [bool; MAX_WINDOWS],
    pub group_start: [usize; MAX_WINDOWS],
    pub window_groups: usize,
    pub num_windows: usize,
    pub max_sfb: usize,
    pub long_win: bool,
}

impl IcsInfo {
    fn new() -> Self {
        Self {
            window_sequence: 0,
            prev_window_sequence: 0,
            window_shape: false,
            prev_window_shape: false,
            scale_factor_grouping: [false; MAX_WINDOWS],
            group_start: [0; MAX_WINDOWS],
            num_windows: 1,
            window_groups: 1,
            max_sfb: 0,
            long_win: true,
        }
    }

    pub fn decode(&mut self, bs: &mut BitReader<'_>) -> Result<(), AacError> {
        self.prev_window_sequence = self.window_sequence;
        self.prev_window_shape = self.window_shape;

        if bs.read_bool()? {
            return Err(AacError::InvalidData("ics reserved bit set"));
        }

        self.window_sequence = bs.read(2)? as u8;
        self.window_shape = bs.read_bool()?;
        self.window_groups = 1;
        self.group_start = [0; MAX_WINDOWS];

        if self.window_sequence == EIGHT_SHORT_SEQUENCE {
            self.long_win = false;
            self.num_windows = 8;
            self.max_sfb = bs.read(4)? as usize;

            for i in 0..MAX_WINDOWS - 1 {
                self.scale_factor_grouping[i] = bs.read_bool()?;
                if !self.scale_factor_grouping[i] {
                    self.group_start[self.window_groups] = i + 1;
                    self.window_groups += 1;
                }
            }
        } else {
            self.long_win = true;
            self.num_windows = 1;
            self.max_sfb = bs.read(6)? as usize;
            // predictor_data_present: AAC Main prediction / LTP are not in AAC-LC.
            if bs.read_bool()? {
                return Err(AacError::Unsupported("aac: predictor data"));
            }
        }
        Ok(())
    }

    pub fn copy_from_common(&mut self, other: &IcsInfo) {
        let prev_window_sequence = self.window_sequence;
        let prev_window_shape = self.window_shape;
        *self = other.clone();
        self.prev_window_sequence = prev_window_sequence;
        self.prev_window_shape = prev_window_shape;
    }

    fn get_group_start(&self, g: usize) -> usize {
        if g == 0 {
            0
        } else if g >= self.window_groups {
            if self.long_win {
                1
            } else {
                8
            }
        } else {
            self.group_start[g]
        }
    }
}

#[derive(Clone)]
pub struct Ics {
    global_gain: u8,
    pub info: IcsInfo,
    pulse: Option<Pulse>,
    tns: Option<Tns>,
    sfb_cb: [[u8; MAX_SFBS]; MAX_WINDOWS],
    pub scales: [[f32; MAX_SFBS]; MAX_WINDOWS],
    sbinfo: GASubbandInfo,
    pub coeffs: [f32; 1024],
    delay: [f32; 1024],
}

impl Ics {
    pub fn new(sbinfo: GASubbandInfo) -> Self {
        Self {
            global_gain: 0,
            info: IcsInfo::new(),
            pulse: None,
            tns: None,
            sfb_cb: [[0; MAX_SFBS]; MAX_WINDOWS],
            scales: [[0.0; MAX_SFBS]; MAX_WINDOWS],
            sbinfo,
            coeffs: [0.0; 1024],
            delay: [0.0; 1024],
        }
    }

    pub fn reset(&mut self) {
        self.info = IcsInfo::new();
        self.delay = [0.0; 1024];
    }

    fn decode_section_data(&mut self, bs: &mut BitReader<'_>) -> Result<(), AacError> {
        let sect_bits = if self.info.long_win { 5 } else { 3 };
        let sect_esc_val = (1 << sect_bits) - 1;

        for g in 0..self.info.window_groups {
            let mut k = 0;
            while k < self.info.max_sfb {
                let cb = bs.read(4)? as u8;
                if cb == RESERVED_HCB {
                    return Err(AacError::InvalidData("aac: invalid band type"));
                }
                let mut len = 0usize;
                loop {
                    let incr = bs.read(sect_bits)? as usize;
                    len += incr;
                    if incr < sect_esc_val {
                        break;
                    }
                    validate!(len <= MAX_SFBS);
                }
                validate!(k + len <= self.info.max_sfb);
                for sfb in k..k + len {
                    self.sfb_cb[g][sfb] = cb;
                }
                k += len;
            }
        }
        Ok(())
    }

    #[inline(always)]
    pub fn is_zero(&self, g: usize, sfb: usize) -> bool {
        self.sfb_cb[g][sfb] == ZERO_HCB
    }

    #[inline(always)]
    pub fn is_intensity(&self, g: usize, sfb: usize) -> bool {
        (self.sfb_cb[g][sfb] == INTENSITY_HCB) || (self.sfb_cb[g][sfb] == INTENSITY_HCB2)
    }

    #[inline(always)]
    pub fn is_noise(&self, g: usize, sfb: usize) -> bool {
        self.sfb_cb[g][sfb] == NOISE_HCB
    }

    #[inline(always)]
    pub fn get_intensity_dir(&self, g: usize, sfb: usize) -> bool {
        self.sfb_cb[g][sfb] == INTENSITY_HCB
    }

    fn decode_scale_factor_data(
        &mut self,
        bs: &mut BitReader<'_>,
        books: &Books,
    ) -> Result<(), AacError> {
        let mut noise_pcm_flag = true;
        let mut scf_intensity = -INTENSITY_SCALE_MIN;
        let mut scf_noise = i16::from(self.global_gain) - 90 - NORMAL_SCALE_MIN;
        let mut scf_normal = i16::from(self.global_gain);

        for g in 0..self.info.window_groups {
            for sfb in 0..self.info.max_sfb {
                self.scales[g][sfb] = if self.is_zero(g, sfb) {
                    0.0
                } else if self.is_intensity(g, sfb) {
                    scf_intensity += books.scf.read(bs)? as i16 - 60;
                    validate!((scf_intensity >= 0) && (scf_intensity < 256));
                    books.intensity_scf[scf_intensity as usize]
                } else if self.is_noise(g, sfb) {
                    if noise_pcm_flag {
                        noise_pcm_flag = false;
                        scf_noise += (bs.read(9)? as i16) - 256;
                    } else {
                        scf_noise += books.scf.read(bs)? as i16 - 60;
                    }
                    validate!((scf_noise >= 0) && (scf_noise < 256));
                    books.normal_scf[scf_noise as usize]
                } else {
                    scf_normal += books.scf.read(bs)? as i16 - 60;
                    validate!((scf_normal >= 0) && (scf_normal < 256));
                    books.normal_scf[scf_normal as usize]
                }
            }
        }
        Ok(())
    }

    pub fn get_bands(&self) -> &'static [usize] {
        if self.info.long_win {
            self.sbinfo.long_bands
        } else {
            self.sbinfo.short_bands
        }
    }

    fn decode_spectrum(
        &mut self,
        bs: &mut BitReader<'_>,
        books: &Books,
        lcg: &mut Lcg,
    ) -> Result<(), AacError> {
        self.coeffs.fill(0.0);
        let bands = self.get_bands();

        for g in 0..self.info.window_groups {
            let cur_w = self.info.get_group_start(g);
            let next_w = self.info.get_group_start(g + 1);
            for sfb in 0..self.info.max_sfb {
                let start = bands[sfb];
                let end = bands[sfb + 1];
                let cb_idx = self.sfb_cb[g][sfb];
                let scale = self.scales[g][sfb];

                for w in cur_w..next_w {
                    let dst = &mut self.coeffs[start + w * 128..end + w * 128];
                    match cb_idx {
                        ZERO_HCB | RESERVED_HCB | INTENSITY_HCB2 | INTENSITY_HCB => (),
                        NOISE_HCB => decode_noise(lcg, scale, dst),
                        1 | 2 => decode_quads(bs, books, cb_idx, true, scale, dst)?,
                        3 | 4 => decode_quads(bs, books, cb_idx, false, scale, dst)?,
                        5 | 6 => decode_pairs(bs, books, cb_idx, 9, true, scale, dst)?,
                        7 | 8 => decode_pairs(bs, books, cb_idx, 8, false, scale, dst)?,
                        9 | 10 => decode_pairs(bs, books, cb_idx, 13, false, scale, dst)?,
                        _ => decode_pairs_escape(bs, books, scale, dst)?,
                    }
                }
            }
        }
        Ok(())
    }

    pub fn decode(
        &mut self,
        bs: &mut BitReader<'_>,
        books: &Books,
        lcg: &mut Lcg,
        common_window: bool,
    ) -> Result<(), AacError> {
        self.global_gain = bs.read(8)? as u8;

        if !common_window {
            self.info.decode(bs)?;
        }
        validate!(self.info.max_sfb < self.get_bands().len());

        self.decode_section_data(bs)?;
        self.decode_scale_factor_data(bs, books)?;

        self.pulse = Pulse::read(bs)?;
        validate!(self.pulse.is_none() || self.info.long_win);

        self.tns = Tns::read(bs, &self.info, true)?;

        // gain_control_data_present (AAC SSR only).
        if bs.read_bool()? {
            return Err(AacError::Unsupported("aac: gain control data"));
        }

        self.decode_spectrum(bs, books, lcg)?;
        Ok(())
    }

    pub fn synth_channel(&mut self, dsp: &mut dsp::Dsp, rate_idx: usize, dst: &mut [f32]) {
        let bands = self.get_bands();

        if let Some(pulse) = &self.pulse {
            pulse.synth(bands, &self.scales, &mut self.coeffs);
        }

        if let Some(tns) = &self.tns {
            tns.synth(&self.info, bands, rate_idx, &mut self.coeffs);
        }

        dsp.synth(
            &self.coeffs,
            &mut self.delay,
            self.info.window_sequence,
            self.info.window_shape,
            self.info.prev_window_shape,
            dst,
        );
    }
}

/// Perceptual noise substitution decode step. Section 4.6.13.3.
/// Same random sequence and normalisation as FFmpeg's float decoder.
fn decode_noise(lcg: &mut Lcg, sf: f32, dst: &mut [f32]) {
    let mut energy = 0.0f32;
    for spec in dst.iter_mut() {
        *spec = lcg.next() as f32;
        energy += *spec * *spec;
    }
    let scale = sf / libm::sqrtf(energy);
    for spec in dst.iter_mut() {
        *spec *= scale;
    }
}

#[inline(always)]
fn decode_sign(val: u32) -> f32 {
    1.0 - 2.0 * val as f32
}

fn decode_quads(
    bs: &mut BitReader<'_>,
    books: &Books,
    cb_idx: u8,
    signed: bool,
    scale: f32,
    dst: &mut [f32],
) -> Result<(), AacError> {
    let vlc = &books.spec[cb_idx as usize - 1];
    for out in dst.chunks_exact_mut(4) {
        let cw = vlc.read(bs)? as u32;
        let q = [cw / 27, (cw / 9) % 3, (cw / 3) % 3, cw % 3];
        if signed {
            for i in 0..4 {
                out[i] = (q[i] as f32 - 1.0) * scale;
            }
        } else {
            for i in 0..4 {
                if q[i] != 0 {
                    out[i] = decode_sign(bs.read(1)?) * books.iq(q[i]) * scale;
                }
            }
        }
    }
    Ok(())
}

fn decode_pairs(
    bs: &mut BitReader<'_>,
    books: &Books,
    cb_idx: u8,
    modulo: u32,
    signed: bool,
    scale: f32,
    dst: &mut [f32],
) -> Result<(), AacError> {
    let vlc = &books.spec[cb_idx as usize - 1];
    for out in dst.chunks_exact_mut(2) {
        let cw = vlc.read(bs)? as u32;
        let (a, b) = (cw / modulo, cw % modulo);
        if signed {
            let half = modulo >> 1;
            for (o, v) in out.iter_mut().zip([a, b]) {
                *o = if v < half {
                    -books.iq(half - v)
                } else {
                    books.iq(v - half)
                } * scale;
            }
        } else {
            let sx = if a != 0 { decode_sign(bs.read(1)?) } else { 1.0 };
            let sy = if b != 0 { decode_sign(bs.read(1)?) } else { 1.0 };
            out[0] = sx * books.iq(a) * scale;
            out[1] = sy * books.iq(b) * scale;
        }
    }
    Ok(())
}

fn decode_pairs_escape(
    bs: &mut BitReader<'_>,
    books: &Books,
    scale: f32,
    dst: &mut [f32],
) -> Result<(), AacError> {
    let vlc = &books.spec[10];
    for out in dst.chunks_exact_mut(2) {
        let cw = vlc.read(bs)? as u32;
        let (a, b) = (cw / 17, cw % 17);
        let sx = if a != 0 { decode_sign(bs.read(1)?) } else { 1.0 };
        let sy = if b != 0 { decode_sign(bs.read(1)?) } else { 1.0 };
        let x = if a == 16 { read_escape(bs)? } else { a };
        let y = if b == 16 { read_escape(bs)? } else { b };
        out[0] = sx * books.iq(x) * scale;
        out[1] = sy * books.iq(y) * scale;
    }
    Ok(())
}

fn read_escape(bs: &mut BitReader<'_>) -> Result<u32, AacError> {
    let n = bs.read_unary_ones(8)?;
    // The escape word is added to 2^(n + 4) to yield the unsigned value (< 8192).
    Ok((1 << (n + 4)) + bs.read(n + 4)?)
}

// ---------------------------------------------------------------------------
// Pulse data (ics/pulse.rs)

#[derive(Clone, Copy)]
pub struct Pulse {
    number_pulse: usize,
    pulse_start_sfb: usize,
    pulse_offset: [u8; 4],
    pulse_amp: [u8; 4],
}

impl Pulse {
    pub fn read(bs: &mut BitReader<'_>) -> Result<Option<Self>, AacError> {
        if !bs.read_bool()? {
            return Ok(None);
        }
        let number_pulse = bs.read(2)? as usize + 1;
        let pulse_start_sfb = bs.read(6)? as usize;
        let mut pulse_offset = [0u8; 4];
        let mut pulse_amp = [0u8; 4];
        for i in 0..number_pulse {
            pulse_offset[i] = bs.read(5)? as u8;
            pulse_amp[i] = bs.read(4)? as u8;
        }
        Ok(Some(Self { number_pulse, pulse_start_sfb, pulse_offset, pulse_amp }))
    }

    pub fn synth(
        &self,
        bands: &[usize],
        scales: &[[f32; MAX_SFBS]; MAX_WINDOWS],
        coeffs: &mut [f32; 1024],
    ) {
        if self.pulse_start_sfb >= bands.len() - 1 {
            return;
        }
        let mut k = bands[self.pulse_start_sfb];
        let mut band = self.pulse_start_sfb;

        for pno in 0..self.number_pulse {
            k += self.pulse_offset[pno] as usize;
            if k >= 1024 {
                return;
            }
            while bands[band + 1] <= k {
                band += 1;
            }
            let scale = scales[0][band];
            if scale == 0.0 {
                continue;
            }
            let amp = f32::from(self.pulse_amp[pno]);
            let co = coeffs[k];
            // Re-quantise (|x| = q^(4/3) * scale), add the pulse amplitude away from zero.
            let ico = if co != 0.0 {
                let q = co / scale;
                let q = q / libm::sqrtf(libm::sqrtf(libm::fabsf(q)));
                if q > 0.0 {
                    q + amp
                } else {
                    q - amp
                }
            } else {
                -amp
            };
            coeffs[k] = libm::cbrtf(libm::fabsf(ico)) * ico * scale;
        }
    }
}

// ---------------------------------------------------------------------------
// Temporal noise shaping (ics/tns.rs)

const TNS_MAX_ORDER: usize = 20;
const TNS_MAX_LONG_BANDS: [usize; 12] = [31, 31, 34, 40, 42, 51, 46, 46, 42, 42, 42, 39];
const TNS_MAX_SHORT_BANDS: [usize; 12] = [9, 9, 10, 14, 14, 14, 14, 14, 14, 14, 14, 14];

#[derive(Copy, Clone)]
struct TnsCoeffs {
    length: usize,
    order: usize,
    direction: bool,
    coef: [f32; TNS_MAX_ORDER + 1],
}

impl TnsCoeffs {
    fn new() -> Self {
        Self { length: 0, order: 0, direction: false, coef: [0.0; TNS_MAX_ORDER + 1] }
    }

    fn read(
        &mut self,
        bs: &mut BitReader<'_>,
        long_win: bool,
        coef_res: bool,
        max_order: usize,
    ) -> Result<(), AacError> {
        self.length = bs.read(if long_win { 6 } else { 4 })? as usize;
        self.order = bs.read(if long_win { 5 } else { 3 })? as usize;
        validate!(self.order <= max_order);

        if self.order > 0 {
            self.direction = bs.read_bool()?;
            let coef_compress = bs.read_bool()?;
            let mut coef_res_bits = if coef_res { 4 } else { 3 };
            if coef_compress {
                coef_res_bits -= 1;
            }
            let sign_mask = 1u8 << (coef_res_bits - 1);
            let neg_mask = !((1u8 << coef_res_bits) - 1);
            let fac_base = if coef_res { 8.0 } else { 4.0 };
            let iqfac = (fac_base - 0.5) / core::f32::consts::FRAC_PI_2;
            let iqfac_m = (fac_base + 0.5) / core::f32::consts::FRAC_PI_2;

            let mut tmp = [0f32; TNS_MAX_ORDER];
            for el in tmp[..self.order].iter_mut() {
                let val = bs.read(coef_res_bits)? as u8;
                let c = f32::from(if (val & sign_mask) != 0 {
                    (val | neg_mask) as i8
                } else {
                    val as i8
                });
                *el = libm::sinf(if c >= 0.0 { c / iqfac } else { c / iqfac_m });
            }

            // Convert reflection coefficients to LPC coefficients.
            let mut b = [0f32; TNS_MAX_ORDER + 1];
            for m in 1..=self.order {
                for i in 1..m {
                    b[i] = self.coef[i - 1] + tmp[m - 1] * self.coef[m - i - 1];
                }
                self.coef[..(m - 1)].copy_from_slice(&b[1..m]);
                self.coef[m - 1] = tmp[m - 1];
            }
        }
        Ok(())
    }
}

#[derive(Copy, Clone)]
pub struct Tns {
    n_filt: [usize; MAX_WINDOWS],
    coeffs: [[TnsCoeffs; 4]; MAX_WINDOWS],
}

impl Tns {
    pub fn read(
        bs: &mut BitReader<'_>,
        info: &IcsInfo,
        is_lc: bool,
    ) -> Result<Option<Self>, AacError> {
        if !bs.read_bool()? {
            return Ok(None);
        }
        let max_order = if !info.long_win {
            7
        } else if is_lc {
            12
        } else {
            TNS_MAX_ORDER
        };

        let mut n_filt = [0usize; MAX_WINDOWS];
        let mut coeffs = [[TnsCoeffs::new(); 4]; MAX_WINDOWS];

        for w in 0..info.num_windows {
            n_filt[w] = bs.read(if info.long_win { 2 } else { 1 })? as usize;
            let coef_res = if n_filt[w] != 0 { bs.read_bool()? } else { false };
            for filt in 0..n_filt[w] {
                coeffs[w][filt].read(bs, info.long_win, coef_res, max_order)?;
            }
        }
        Ok(Some(Self { n_filt, coeffs }))
    }

    pub fn synth(&self, info: &IcsInfo, bands: &[usize], rate_idx: usize, coeffs: &mut [f32; 1024]) {
        let tns_max_bands = (if info.long_win {
            TNS_MAX_LONG_BANDS[rate_idx]
        } else {
            TNS_MAX_SHORT_BANDS[rate_idx]
        })
        .min(info.max_sfb)
        .min(bands.len() - 1);

        for w in 0..info.num_windows {
            let mut bottom = bands.len() - 1;
            for f in 0..self.n_filt[w] {
                let top = bottom;
                bottom = top.saturating_sub(self.coeffs[w][f].length);
                let order = self.coeffs[w][f].order;
                if order == 0 {
                    continue;
                }
                let start = w * 128 + bands[bottom.min(tns_max_bands)];
                let end = w * 128 + bands[top.min(tns_max_bands)];
                let lpc = &self.coeffs[w][f].coef;

                if !self.coeffs[w][f].direction {
                    for (m, i) in (start..end).enumerate() {
                        for j in 0..order.min(m) {
                            coeffs[i] -= coeffs[i - j - 1] * lpc[j];
                        }
                    }
                } else {
                    for (m, i) in (start..end).rev().enumerate() {
                        for j in 0..order.min(m) {
                            coeffs[i] -= coeffs[i + j + 1] * lpc[j];
                        }
                    }
                }
            }
        }
    }
}
