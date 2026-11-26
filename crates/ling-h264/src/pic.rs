//! Decoded picture buffers and the per-picture macroblock state.

use alloc::vec;
use alloc::vec::Vec;
use crate::Frame;

/// Luma padding (samples) around each plane so motion compensation never needs bounds checks.
pub const PAD: usize = 32;
/// Chroma padding.
pub const PADC: usize = 16;

#[derive(Default)]
pub struct Planes {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    /// luma width/height (macroblock aligned)
    pub w: usize,
    pub h: usize,
    pub ys: usize,
    pub cs: usize,
}

impl Planes {
    pub fn alloc(&mut self, w: usize, h: usize) {
        if self.w == w && self.h == h && !self.y.is_empty() {
            return;
        }
        self.w = w;
        self.h = h;
        self.ys = w + 2 * PAD;
        self.cs = w / 2 + 2 * PADC;
        self.y = vec![0; self.ys * (h + 2 * PAD)];
        self.u = vec![128; self.cs * (h / 2 + 2 * PADC)];
        self.v = vec![128; self.cs * (h / 2 + 2 * PADC)];
    }
    /// Offset of luma sample (x, y).
    #[inline(always)]
    pub fn yoff(&self, x: usize, y: usize) -> usize {
        (y + PAD) * self.ys + x + PAD
    }
    #[inline(always)]
    pub fn coff(&self, x: usize, y: usize) -> usize {
        (y + PADC) * self.cs + x + PADC
    }

    /// Replicate the picture edges into the padding.
    pub fn extend_edges(&mut self) {
        fn ext(buf: &mut [u8], stride: usize, w: usize, h: usize, pad: usize) {
            for y in 0..h {
                let row = (y + pad) * stride;
                let l = buf[row + pad];
                let r = buf[row + pad + w - 1];
                buf[row..row + pad].fill(l);
                buf[row + pad + w..row + 2 * pad + w].fill(r);
            }
            let (top, rest) = buf.split_at_mut(pad * stride);
            let first = &rest[..stride];
            for y in 0..pad {
                top[y * stride..(y + 1) * stride].copy_from_slice(first);
            }
            let last_row = (h + pad - 1) * stride;
            let (head, tail) = buf.split_at_mut((h + pad) * stride);
            let last = &head[last_row..last_row + stride];
            for y in 0..pad {
                tail[y * stride..(y + 1) * stride].copy_from_slice(last);
            }
        }
        let (w, h) = (self.w, self.h);
        ext(&mut self.y, self.ys, w, h, PAD);
        ext(&mut self.u, self.cs, w / 2, h / 2, PADC);
        ext(&mut self.v, self.cs, w / 2, h / 2, PADC);
    }
}

pub const REF_NONE: u8 = 0;
pub const REF_SHORT: u8 = 1;
pub const REF_LONG: u8 = 2;

#[derive(Default)]
pub struct Pic {
    pub planes: Planes,
    pub poc: i32,
    pub frame_num: u32,
    pub frame_num_wrap: i32,
    pub long_term_idx: u32,
    pub reference: u8,
    pub output_needed: bool,
    pub id: u32,
    pub non_existing: bool,
    /// slot is holding a picture (current, reference or waiting for output)
    pub busy: bool,
    // motion data, kept for direct prediction from this picture
    pub mv: [Vec<[i16; 2]>; 2],
    pub refi: [Vec<i8>; 2],
    pub mb_intra: Vec<bool>,
    pub mb_slice: Vec<u32>,
    /// per slice of this picture: picture ids referenced by list0 / list1
    pub slice_refs: Vec<[Vec<u32>; 2]>,
    /// colour description from the SPS that produced the picture
    pub full_range: bool,
    pub bt709: bool,
    pub crop: [u32; 4],
    /// output frame being filled as rows are finished (see `copy_out_rows`)
    pub pending: Option<Frame>,
}

impl Pic {
    pub fn alloc(&mut self, mbw: usize, mbh: usize) {
        self.planes.alloc(mbw * 16, mbh * 16);
        let n4 = mbw * mbh * 16;
        for l in 0..2 {
            if self.mv[l].len() != n4 {
                self.mv[l] = vec![[0; 2]; n4];
                self.refi[l] = vec![-1; n4];
            }
        }
        if self.mb_intra.len() != mbw * mbh {
            self.mb_intra = vec![true; mbw * mbh];
            self.mb_slice = vec![0; mbw * mbh];
        }
        self.slice_refs.clear();
        self.pending = None;
    }

    /// Append the final (deblocked) macroblock rows y0..y1 to the pending output frame (cropped),
    /// while they are still in cache. Rows must be appended in order starting at row 0.
    pub fn copy_out_rows(&mut self, y0: usize, y1: usize) {
        let pl = &self.planes;
        let [cl, cr, ct, cb] = self.crop;
        let w = (pl.w as u32).saturating_sub(cl + cr) as usize;
        let h = (pl.h as u32).saturating_sub(ct + cb) as usize;
        let (cl, ct) = (cl as usize, ct as usize);
        let cw = w.div_ceil(2);
        let chh = h.div_ceil(2);
        if y0 == 0 {
            self.pending = Some(Frame {
                width: w as u32,
                height: h as u32,
                y: Vec::with_capacity(w * h),
                u: Vec::with_capacity(cw * chh),
                v: Vec::with_capacity(cw * chh),
                y_stride: w,
                c_stride: cw,
                poc: self.poc,
                bt709: self.bt709,
                full_range: self.full_range,
            });
        }
        let f = match self.pending.as_mut() {
            Some(f) => f,
            None => return,
        };
        let y1 = y1.min(pl.h / 16);
        for r in (16 * y0).max(ct)..(16 * y1).min(ct + h) {
            let o = pl.yoff(cl, r);
            f.y.extend_from_slice(&pl.y[o..o + w]);
        }
        for r in (8 * y0).max(ct / 2)..(8 * y1).min(ct / 2 + chh) {
            let o = pl.coff(cl / 2, r);
            f.u.extend_from_slice(&pl.u[o..o + cw]);
            f.v.extend_from_slice(&pl.v[o..o + cw]);
        }
    }
}

// macroblock type flags
pub const MB_INTRA: u32 = 1;
pub const MB_I4: u32 = 2;
pub const MB_I8: u32 = 4;
pub const MB_I16: u32 = 8;
pub const MB_PCM: u32 = 16;
pub const MB_SKIP: u32 = 32;
/// B_Skip or B_Direct_16x16
pub const MB_DIRECT: u32 = 64;
pub const MB_8X8DCT: u32 = 128;
pub const MB_INTER: u32 = 256;

pub const NOT_DECODED: u32 = u32::MAX;

#[derive(Clone, Copy)]
pub struct SliceParams {
    pub disable_deblock: u32,
    pub alpha_offset: i32,
    pub beta_offset: i32,
}

/// Macroblock-level side information of the picture being decoded.
#[derive(Default)]
pub struct FrameState {
    pub mbw: usize,
    pub mbh: usize,
    pub mbt: Vec<u32>,
    pub slice: Vec<u32>,
    pub qp: Vec<u8>,
    pub qpc: Vec<[u8; 2]>,
    /// luma cbp bits 0..3, chroma cbp bits 4..5, DC coded_block_flags bit 8 (Y), 9 (Cb), 10 (Cr)
    pub cbp: Vec<u16>,
    pub chroma_pred: Vec<u8>,
    /// direct-predicted 8x8 blocks (bit per 8x8)
    pub direct8: Vec<u8>,
    /// total_coeff (CAVLC) / coded flag (CABAC) per luma 4x4 block, picture-wide 4x4 grid
    pub nnz: Vec<u8>,
    /// chroma: two planes of (mbw*2) x (mbh*2)
    pub nnzc: Vec<u8>,
    /// non-zero coefficients per luma 4x4 for the deblocking filter
    pub nzd: Vec<u8>,
    pub ipm: Vec<i8>,
    pub mvd: [Vec<[u8; 2]>; 2],
    pub slice_params: Vec<SliceParams>,
    /// deblocking scratch: reference picture id per luma 4x4 block and list
    pub dbk_ref: [Vec<u32>; 2],
    /// macroblock rows of the current picture already deblocked
    pub dbk_next_row: usize,
    /// macroblock rows of the current picture already copied to its output frame
    pub out_next_row: usize,
}

impl FrameState {
    pub fn reset(&mut self, mbw: usize, mbh: usize) {
        let n = mbw * mbh;
        if self.mbw != mbw || self.mbh != mbh || self.mbt.len() != n {
            self.mbw = mbw;
            self.mbh = mbh;
            self.mbt = vec![0; n];
            self.slice = vec![NOT_DECODED; n];
            self.qp = vec![0; n];
            self.qpc = vec![[0; 2]; n];
            self.cbp = vec![0; n];
            self.chroma_pred = vec![0; n];
            self.direct8 = vec![0; n];
            self.nnz = vec![0; n * 16];
            self.nnzc = vec![0; n * 8];
            self.nzd = vec![0; n * 16];
            self.ipm = vec![2; n * 16];
            self.mvd = [vec![[0; 2]; n * 16], vec![[0; 2]; n * 16]];
        } else {
            self.slice.fill(NOT_DECODED);
        }
        self.slice_params.clear();
        self.dbk_next_row = 0;
        self.out_next_row = 0;
    }
}
