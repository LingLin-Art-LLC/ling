//! Macroblock layer: syntax driving (shared by CAVLC and CABAC), motion vector prediction,
//! direct modes, and reconstruction (intra prediction, motion compensation, residual).

use crate::idct;
use crate::inter::{self, Wp};
use crate::intra;
use crate::pic::*;
use crate::ps::{Pps, Sps};
use crate::slice::{SliceHeader, SliceType};
use crate::tables::*;
use crate::Error;
use alloc::boxed::Box;
use alloc::vec::Vec;

/// Dequantisation multipliers: LevelScale(m, pos) << (qp / 6), per list and qp.
pub struct Dequant {
    /// [list: intraY, intraCb, intraCr, interY, interCb, interCr][qp][raster pos]
    pub d4: [[[i32; 16]; 52]; 6],
    /// [intraY, interY][qp][raster pos]
    pub d8: [[[i32; 64]; 52]; 2],
}

impl Dequant {
    pub fn new(pps: &Pps) -> Box<Dequant> {
        let mut d = Box::new(Dequant { d4: [[[0; 16]; 52]; 6], d8: [[[0; 64]; 52]; 2] });
        for l in 0..6 {
            for qp in 0..52 {
                for p in 0..16 {
                    d.d4[l][qp][p] = (pps.sl4[l][p] as i32 * norm4(qp % 6, p) as i32) << (qp / 6);
                }
            }
        }
        for l in 0..2 {
            for qp in 0..52 {
                for p in 0..64 {
                    d.d8[l][qp][p] = (pps.sl8[l][p] as i32 * norm8(qp % 6, p) as i32) << (qp / 6);
                }
            }
        }
        d
    }
}

/// Non-zero coefficients of one residual block: level `lv[k]` at scan position `pos[k]`.
pub struct Coefs {
    pub lv: [i32; 64],
    pub pos: [u8; 64],
}

/// Entropy decoder interface: the syntax elements of the macroblock layer.
pub trait Entropy {
    fn mb_type(&mut self, c: &SliceCtx) -> Result<u32, Error>;
    fn transform8(&mut self, c: &SliceCtx) -> bool;
    /// prev_intra_pred_mode_flag / rem_intra_pred_mode: None when the predicted mode is used.
    fn intra_mode(&mut self) -> Option<u32>;
    fn chroma_mode(&mut self, c: &SliceCtx) -> u32;
    fn sub_mb_type(&mut self, c: &SliceCtx) -> u32;
    fn ref_idx(&mut self, c: &SliceCtx, list: usize, x4: usize, y4: usize, num: u32) -> u32;
    fn mvd(&mut self, c: &mut SliceCtx, list: usize, x4: usize, y4: usize, w4: usize, h4: usize) -> [i32; 2];
    fn cbp(&mut self, c: &SliceCtx, intra: bool) -> Result<u32, Error>;
    fn qp_delta(&mut self, c: &SliceCtx) -> i32;
    /// Decode a residual block: stores the non-zero coefficient levels sparsely
    /// (`out.lv[k]` at scan position `out.pos[k]`, positions counted from startIdx, `k < n`)
    /// and returns their number n.
    fn residual(&mut self, c: &SliceCtx, cat: u32, blk: usize, max: usize, out: &mut Coefs) -> Result<usize, Error>;
    fn pcm(&mut self, out: &mut [u8; 384]) -> Result<(), Error>;
    fn overrun(&self) -> bool;
}

// partition prediction usage bits
const L0: u8 = 1;
const L1: u8 = 2;
const BI: u8 = 3;

/// B macroblock types 1..=21: (shape 0=16x16 1=16x8 2=8x16, pred part0, pred part1)
const B_TYPES: [(u8, u8, u8); 22] = [
    (0, 0, 0),
    (0, L0, 0),
    (0, L1, 0),
    (0, BI, 0),
    (1, L0, L0),
    (2, L0, L0),
    (1, L1, L1),
    (2, L1, L1),
    (1, L0, L1),
    (2, L0, L1),
    (1, L1, L0),
    (2, L1, L0),
    (1, L0, BI),
    (2, L0, BI),
    (1, L1, BI),
    (2, L1, BI),
    (1, BI, L0),
    (2, BI, L0),
    (1, BI, L1),
    (2, BI, L1),
    (1, BI, BI),
    (2, BI, BI),
];
/// B sub-macroblock types: (shape 0=8x8 1=8x4 2=4x8 3=4x4, pred) ; 0 = direct
const B_SUB: [(u8, u8); 13] = [
    (0, 0),
    (0, L0),
    (0, L1),
    (0, BI),
    (1, L0),
    (2, L0),
    (1, L1),
    (2, L1),
    (1, BI),
    (2, BI),
    (3, L0),
    (3, L1),
    (3, BI),
];

pub struct SliceCtx<'a> {
    pub sps: &'a Sps,
    pub pps: &'a Pps,
    pub sh: &'a SliceHeader,
    pub cur: &'a mut Pic,
    pub pics: &'a [Pic],
    pub fs: &'a mut FrameState,
    pub lists: [Vec<usize>; 2],
    pub slice_num: u32,
    pub dq: &'a Dequant,
    pub mbw: usize,
    pub mbh: usize,
    pub stype: SliceType,
    pub qp: i32,
    pub mbx: usize,
    pub mby: usize,
    pub addr: usize,
    pub prev_qp_delta_nz: bool,
    pub cabac: bool,
    coef: Box<[i32; 384]>,
    pcm: Box<[u8; 384]>,
    /// luma blocks with coefficients (bit per 4x4 blkIdx, or per 8x8 when 8x8 transform)
    nz_luma: u32,
    nz_chroma: u32,
    // per-picture constants for B slices
    col: Option<usize>,
    col_short: bool,
    pub cur_poc: i32,
    tmp: Box<[[u8; 256]; 2]>,
    tmpc: Box<[[[u8; 64]; 2]; 2]>,
    mct: Box<inter::McTmp>,
}

#[inline(always)]
fn median(a: i32, b: i32, c: i32) -> i32 {
    a.max(b).min(a.min(b).max(c))
}

impl<'a> SliceCtx<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sps: &'a Sps,
        pps: &'a Pps,
        sh: &'a SliceHeader,
        cur: &'a mut Pic,
        pics: &'a [Pic],
        fs: &'a mut FrameState,
        lists: [Vec<usize>; 2],
        slice_num: u32,
        dq: &'a Dequant,
    ) -> Result<SliceCtx<'a>, Error> {
        let mbw = sps.mb_width as usize;
        let mbh = sps.mb_height as usize;
        let mut col = None;
        let mut col_short = false;
        if sh.slice_type == SliceType::B {
            let c = *lists[1].first().ok_or(Error::Invalid("empty list1"))?;
            col_short = pics[c].reference == REF_SHORT || pics[c].reference == REF_NONE;
            col = Some(c);
        }
        Ok(SliceCtx {
            sps,
            pps,
            sh,
            cur_poc: cur.poc,
            cur,
            pics,
            fs,
            lists,
            slice_num,
            dq,
            mbw,
            mbh,
            stype: sh.slice_type,
            qp: sh.qp,
            mbx: 0,
            mby: 0,
            addr: 0,
            prev_qp_delta_nz: false,
            cabac: pps.cabac,
            coef: Box::new([0; 384]),
            pcm: Box::new([0; 384]),
            nz_luma: 0,
            nz_chroma: 0,
            col,
            col_short,
            tmp: Box::new([[0; 256]; 2]),
            tmpc: Box::new([[[0; 64]; 2]; 2]),
            mct: Box::new(inter::McTmp { b: [0; 256], t: [0; 16 * 21] }),
        })
    }

    // ------------------------------------------------------------------ neighbours

    #[inline(always)]
    pub fn s4(&self) -> usize {
        self.mbw * 4
    }

    /// Is macroblock (x, y) available (decoded, same slice)?
    #[inline(always)]
    pub fn avail(&self, x: isize, y: isize) -> bool {
        x >= 0 && y >= 0 && (x as usize) < self.mbw && (y as usize) < self.mbh && self.fs.slice[y as usize * self.mbw + x as usize] == self.slice_num
    }

    #[inline(always)]
    pub fn mb_a(&self) -> Option<usize> {
        if self.avail(self.mbx as isize - 1, self.mby as isize) {
            Some(self.addr - 1)
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn mb_b(&self) -> Option<usize> {
        if self.avail(self.mbx as isize, self.mby as isize - 1) {
            Some(self.addr - self.mbw)
        } else {
            None
        }
    }

    /// Luma 4x4 neighbour (global 4x4 coords) -> index if available.
    #[inline(always)]
    pub fn nb4(&self, gx: isize, gy: isize) -> Option<usize> {
        if gx < 0 || gy < 0 {
            return None;
        }
        if self.avail(gx >> 2, gy >> 2) {
            Some(gy as usize * self.s4() + gx as usize)
        } else {
            None
        }
    }

    /// Global 4x4 index of local block (x4, y4) in the current MB.
    #[inline(always)]
    pub fn idx4(&self, x4: usize, y4: usize) -> usize {
        (self.mby * 4 + y4) * self.s4() + self.mbx * 4 + x4
    }

    #[inline(always)]
    fn nb_motion(&self, list: usize, gx: isize, gy: isize) -> (i8, [i16; 2]) {
        match self.nb4(gx, gy) {
            Some(i) => (self.cur.refi[list][i], self.cur.mv[list][i]),
            None => (-2, [0, 0]),
        }
    }

    /// Neighbour C (or D when C is unavailable) of a partition at local (x4, y4) of width w4.
    fn nb_c(&self, list: usize, x4: usize, y4: usize, w4: usize) -> (i8, [i16; 2]) {
        let gx = (self.mbx * 4 + x4) as isize;
        let gy = (self.mby * 4 + y4) as isize;
        let cx = x4 + w4;
        let c_ok = if y4 == 0 {
            true
        } else if cx >= 4 {
            false
        } else {
            BLK_IDX[(y4 - 1) * 4 + cx] < BLK_IDX[y4 * 4 + x4]
        };
        if c_ok {
            let r = self.nb_motion(list, gx + w4 as isize, gy - 1);
            if r.0 != -2 {
                return r;
            }
        }
        self.nb_motion(list, gx - 1, gy - 1)
    }

    /// Motion vector predictor (8.4.1.3). shape: 0 median, 1 16x8 upper, 2 16x8 lower, 3 8x16 left, 4 8x16 right.
    fn pred_mv(&self, list: usize, x4: usize, y4: usize, w4: usize, r: i8, shape: u8) -> [i16; 2] {
        let gx = (self.mbx * 4 + x4) as isize;
        let gy = (self.mby * 4 + y4) as isize;
        let a = self.nb_motion(list, gx - 1, gy);
        let b = self.nb_motion(list, gx, gy - 1);
        let c = self.nb_c(list, x4, y4, w4);
        match shape {
            1 if b.0 == r => return b.1,
            2 if a.0 == r => return a.1,
            3 if a.0 == r => return a.1,
            4 if c.0 == r => return c.1,
            _ => {}
        }
        let (a, b, c) = if b.0 == -2 && c.0 == -2 && a.0 != -2 { (a, a, a) } else { (a, b, c) };
        let ma = (a.0 == r) as u32;
        let mb = (b.0 == r) as u32;
        let mc = (c.0 == r) as u32;
        if ma + mb + mc == 1 {
            if ma == 1 {
                return a.1;
            }
            if mb == 1 {
                return b.1;
            }
            return c.1;
        }
        [
            median(a.1[0] as i32, b.1[0] as i32, c.1[0] as i32) as i16,
            median(a.1[1] as i32, b.1[1] as i32, c.1[1] as i32) as i16,
        ]
    }

    fn fill_ref(&mut self, list: usize, x4: usize, y4: usize, w4: usize, h4: usize, r: i8) {
        let s4 = self.s4();
        let base = self.idx4(x4, y4);
        for y in 0..h4 {
            self.cur.refi[list][base + y * s4..base + y * s4 + w4].fill(r);
        }
    }

    fn fill_mv(&mut self, list: usize, x4: usize, y4: usize, w4: usize, h4: usize, mv: [i16; 2]) {
        let s4 = self.s4();
        let base = self.idx4(x4, y4);
        for y in 0..h4 {
            self.cur.mv[list][base + y * s4..base + y * s4 + w4].fill(mv);
        }
    }

    pub fn fill_mvd(&mut self, list: usize, x4: usize, y4: usize, w4: usize, h4: usize, mvd: [u8; 2]) {
        let s4 = self.s4();
        let base = self.idx4(x4, y4);
        for y in 0..h4 {
            self.fs.mvd[list][base + y * s4..base + y * s4 + w4].fill(mvd);
        }
    }

    // ------------------------------------------------------------------ direct prediction

    /// Colocated motion for local 4x4 block (x4, y4): (mvCol, refIdxCol, refPicCol id)
    fn colocated(&self, x4: usize, y4: usize) -> ([i16; 2], i8, u32) {
        let col = &self.pics[self.col.unwrap()];
        if col.non_existing || col.mb_intra.len() != self.mbw * self.mbh || col.mb_intra[self.addr] {
            return ([0, 0], -1, u32::MAX);
        }
        let i = self.idx4(x4, y4);
        let sl = col.mb_slice[self.addr] as usize;
        let lookup = |l: usize, r: i8| -> u32 { col.slice_refs.get(sl).and_then(|v| v[l].get(r as usize)).copied().unwrap_or(u32::MAX) };
        if col.refi[0][i] >= 0 {
            (col.mv[0][i], col.refi[0][i], lookup(0, col.refi[0][i]))
        } else if col.refi[1][i] >= 0 {
            (col.mv[1][i], col.refi[1][i], lookup(1, col.refi[1][i]))
        } else {
            ([0, 0], -1, u32::MAX)
        }
    }

    /// Derive direct-mode motion for the 8x8 blocks set in `mask` (8.4.1.2).
    fn direct(&mut self, mask: u8) -> Result<(), Error> {
        let inference = self.sps.direct_8x8_inference;
        if self.sh.direct_spatial {
            // reference indices and predictors from the 16x16 neighbourhood
            let mut refs = [-1i8; 2];
            let mut mvp = [[0i16; 2]; 2];
            let gx = (self.mbx * 4) as isize;
            let gy = (self.mby * 4) as isize;
            for l in 0..2 {
                let a = self.nb_motion(l, gx - 1, gy).0;
                let b = self.nb_motion(l, gx, gy - 1).0;
                let c = self.nb_c(l, 0, 0, 4).0;
                let mp = |x: i8, y: i8| if x >= 0 && y >= 0 { x.min(y) } else { x.max(y) };
                refs[l] = mp(a, mp(b, c));
                if refs[l] < -1 {
                    refs[l] = -1;
                }
            }
            let zero = refs[0] < 0 && refs[1] < 0;
            if zero {
                refs = [0, 0];
            } else {
                for l in 0..2 {
                    if refs[l] >= 0 {
                        mvp[l] = self.pred_mv(l, 0, 0, 4, refs[l], 0);
                    }
                }
            }
            for b8 in 0..4 {
                if mask & (1 << b8) == 0 {
                    continue;
                }
                let (bx, by) = ((b8 & 1) * 2, (b8 >> 1) * 2);
                let sub: &[(usize, usize)] = if inference { &[(0, 0)] } else { &[(0, 0), (1, 0), (0, 1), (1, 1)] };
                for &(sx, sy) in sub {
                    let (x4, y4) = (bx + sx, by + sy);
                    let (w4, cx, cy) = if inference { (2, (b8 & 1) * 3, (b8 >> 1) * 3) } else { (1, x4, y4) };
                    let mut col_zero = false;
                    if self.col_short && !zero {
                        let (mvc, rc, _) = self.colocated(cx, cy);
                        col_zero = rc == 0 && (-1..=1).contains(&mvc[0]) && (-1..=1).contains(&mvc[1]);
                    }
                    for l in 0..2 {
                        let r = refs[l];
                        let mv = if r < 0 || zero || (r == 0 && col_zero) { [0, 0] } else { mvp[l] };
                        self.fill_ref(l, x4, y4, w4, w4, r);
                        self.fill_mv(l, x4, y4, w4, w4, mv);
                    }
                }
            }
        } else {
            let l1 = self.col.unwrap();
            let poc1 = self.pics[l1].poc;
            for b8 in 0..4 {
                if mask & (1 << b8) == 0 {
                    continue;
                }
                let (bx, by) = ((b8 & 1) * 2, (b8 >> 1) * 2);
                let sub: &[(usize, usize)] = if inference { &[(0, 0)] } else { &[(0, 0), (1, 0), (0, 1), (1, 1)] };
                for &(sx, sy) in sub {
                    let (x4, y4) = (bx + sx, by + sy);
                    let (w4, cx, cy) = if inference { (2, (b8 & 1) * 3, (b8 >> 1) * 3) } else { (1, x4, y4) };
                    let (mvc, rc, rid) = self.colocated(cx, cy);
                    let r0 = if rc < 0 {
                        0
                    } else {
                        self.lists[0].iter().position(|&p| self.pics[p].id == rid).unwrap_or(0)
                    };
                    let p0 = *self.lists[0].get(r0).ok_or(Error::Invalid("temporal direct ref"))?;
                    let pic0 = &self.pics[p0];
                    let (mv0, mv1);
                    if pic0.reference == REF_LONG || poc1 - pic0.poc == 0 {
                        mv0 = mvc;
                        mv1 = [0, 0];
                    } else {
                        let tb = (self.cur_poc - pic0.poc).clamp(-128, 127);
                        let td = (poc1 - pic0.poc).clamp(-128, 127);
                        let tx = (16384 + (td / 2).abs()) / td;
                        let dsf = ((tb * tx + 32) >> 6).clamp(-1024, 1023);
                        let m0 = [((dsf * mvc[0] as i32 + 128) >> 8) as i16, ((dsf * mvc[1] as i32 + 128) >> 8) as i16];
                        mv0 = m0;
                        mv1 = [m0[0].wrapping_sub(mvc[0]), m0[1].wrapping_sub(mvc[1])];
                    }
                    self.fill_ref(0, x4, y4, w4, w4, r0 as i8);
                    self.fill_ref(1, x4, y4, w4, w4, 0);
                    self.fill_mv(0, x4, y4, w4, w4, mv0);
                    self.fill_mv(1, x4, y4, w4, w4, mv1);
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ macroblock decoding

    /// Called after the macroblock before `next` has been decoded: deblocks the macroblock rows
    /// that are complete and no longer needed unfiltered (all but the last decoded row), while
    /// their samples are still in cache.
    #[inline]
    pub fn after_mb(&mut self, next: usize) {
        if next % self.mbw != 0 {
            return;
        }
        let done = next / self.mbw;
        if done >= 2 && self.fs.dbk_next_row + 1 < done {
            let y0 = self.fs.dbk_next_row;
            crate::deblock::deblock_rows(self.fs, self.cur, y0, done - 1);
            self.fs.dbk_next_row = done - 1;
            // rows above the last deblocked one are final (its top edge filter is done)
            let fin = done - 2;
            if self.fs.out_next_row < fin {
                self.cur.copy_out_rows(self.fs.out_next_row, fin);
                self.fs.out_next_row = fin;
            }
        }
    }

    pub fn set_mb(&mut self, addr: usize) {
        self.addr = addr;
        self.mbx = addr % self.mbw;
        self.mby = addr / self.mbw;
        self.fs.slice[addr] = self.slice_num;
        self.cur.mb_slice[addr] = self.slice_num;
    }

    fn clear_mb_arrays(&mut self) {
        let s4 = self.s4();
        let base = self.idx4(0, 0);
        for y in 0..4 {
            let r = base + y * s4..base + y * s4 + 4;
            self.fs.nnz[r.clone()].fill(0);
            self.fs.nzd[r.clone()].fill(0);
            self.fs.mvd[0][r.clone()].fill([0, 0]);
            self.fs.mvd[1][r.clone()].fill([0, 0]);
        }
        let cs = self.mbw * 2;
        let plane = self.mbw * self.mbh * 4;
        for c in 0..2 {
            for y in 0..2 {
                let o = c * plane + (self.mby * 2 + y) * cs + self.mbx * 2;
                self.fs.nnzc[o..o + 2].fill(0);
            }
        }
    }

    fn set_intra_motion(&mut self) {
        for l in 0..2 {
            self.fill_ref(l, 0, 0, 4, 4, -1);
            self.fill_mv(l, 0, 0, 4, 4, [0, 0]);
        }
        self.cur.mb_intra[self.addr] = true;
    }

    fn finish_mb_info(&mut self, mbt: u32, cbp: u32, chroma_pred: u8) {
        let a = self.addr;
        self.fs.mbt[a] = mbt;
        let qp = if mbt & MB_PCM != 0 { 0 } else { self.qp };
        self.fs.qp[a] = qp as u8;
        self.fs.qpc[a] = [chroma_qp(qp, self.pps.chroma_qp_offset[0]) as u8, chroma_qp(qp, self.pps.chroma_qp_offset[1]) as u8];
        self.fs.cbp[a] = (self.fs.cbp[a] & 0x700) | cbp as u16;
        self.fs.chroma_pred[a] = chroma_pred;
        if mbt & MB_INTRA == 0 {
            self.cur.mb_intra[a] = false;
        }
        // deblocking non-zero flags
        let s4 = self.s4();
        let base = self.idx4(0, 0);
        if mbt & MB_8X8DCT != 0 {
            for b8 in 0..4 {
                let (bx, by) = ((b8 & 1) * 2, (b8 >> 1) * 2);
                let mut any = 0;
                for y in 0..2 {
                    for x in 0..2 {
                        any |= self.fs.nnz[base + (by + y) * s4 + bx + x];
                    }
                }
                let v = (any != 0) as u8;
                for y in 0..2 {
                    for x in 0..2 {
                        self.fs.nzd[base + (by + y) * s4 + bx + x] = v;
                    }
                }
            }
        } else {
            for y in 0..4 {
                for x in 0..4 {
                    let i = base + y * s4 + x;
                    self.fs.nzd[i] = (self.fs.nnz[i] != 0) as u8;
                }
            }
        }
    }

    /// P_Skip / B_Skip macroblock.
    pub fn decode_skip(&mut self) -> Result<(), Error> {
        self.clear_mb_arrays();
        self.fs.cbp[self.addr] = 0;
        self.fs.direct8[self.addr] = 0;
        self.cur.mb_intra[self.addr] = false;
        let mbt;
        if self.stype == SliceType::P {
            let gx = (self.mbx * 4) as isize;
            let gy = (self.mby * 4) as isize;
            let a = self.nb_motion(0, gx - 1, gy);
            let b = self.nb_motion(0, gx, gy - 1);
            let mv = if a.0 == -2 || b.0 == -2 || (a.0 == 0 && a.1 == [0, 0]) || (b.0 == 0 && b.1 == [0, 0]) {
                [0, 0]
            } else {
                self.pred_mv(0, 0, 0, 4, 0, 0)
            };
            self.fill_ref(0, 0, 0, 4, 4, 0);
            self.fill_mv(0, 0, 0, 4, 4, mv);
            self.fill_ref(1, 0, 0, 4, 4, -1);
            self.fill_mv(1, 0, 0, 4, 4, [0, 0]);
            mbt = MB_SKIP | MB_INTER;
        } else {
            self.fs.direct8[self.addr] = 15;
            self.direct(15)?;
            mbt = MB_SKIP | MB_DIRECT | MB_INTER;
        }
        self.finish_mb_info(mbt, 0, 0);
        if self.stype == SliceType::P {
            // P_Skip: one motion vector for the whole macroblock
            self.mc_block(0, 0, 4, 4)?;
        } else {
            self.mc_mb()?;
        }
        self.prev_qp_delta_nz = false;
        Ok(())
    }

    /// Decode one coded macroblock (macroblock_layer()).
    pub fn decode_mb<E: Entropy>(&mut self, e: &mut E) -> Result<(), Error> {
        self.clear_mb_arrays();
        self.fs.cbp[self.addr] = 0;
        self.fs.direct8[self.addr] = 0;
        self.fs.mbt[self.addr] = 0;
        let raw = e.mb_type(self)?;
        let itype: Option<u32> = match self.stype {
            SliceType::I => Some(raw),
            SliceType::P => {
                if raw >= 5 {
                    Some(raw - 5)
                } else {
                    None
                }
            }
            SliceType::B => {
                if raw >= 23 {
                    Some(raw - 23)
                } else {
                    None
                }
            }
        };
        if let Some(it) = itype {
            if it > 25 {
                return Err(Error::Invalid("mb_type"));
            }
            return self.decode_intra(e, it);
        }
        // ---- inter macroblock
        self.cur.mb_intra[self.addr] = false;
        for l in 0..2 {
            self.fill_ref(l, 0, 0, 4, 4, -1);
            self.fill_mv(l, 0, 0, 4, 4, [0, 0]);
        }
        let mut mbt = MB_INTER;
        let mut no_sub_lt8 = true;
        let num = self.sh.num_ref_idx;
        if self.stype == SliceType::B && raw == 0 {
            // B_Direct_16x16
            mbt |= MB_DIRECT;
            self.fs.direct8[self.addr] = 15;
            self.direct(15)?;
            if !self.sps.direct_8x8_inference {
                no_sub_lt8 = false;
            }
        } else if (self.stype == SliceType::P && raw >= 3) || (self.stype == SliceType::B && raw == 22) {
            // 8x8 partitions
            let mut sub = [(0u8, 0u8); 4];
            for s in sub.iter_mut() {
                let t = e.sub_mb_type(self);
                *s = if self.stype == SliceType::P {
                    if t > 3 {
                        return Err(Error::Invalid("sub_mb_type"));
                    }
                    (t as u8, L0)
                } else {
                    if t > 12 {
                        return Err(Error::Invalid("sub_mb_type"));
                    }
                    B_SUB[t as usize]
                };
            }
            let mut dmask = 0u8;
            for (i, s) in sub.iter().enumerate() {
                if self.stype == SliceType::B && s.1 == 0 {
                    dmask |= 1 << i;
                    if !self.sps.direct_8x8_inference {
                        no_sub_lt8 = false;
                    }
                } else if s.0 != 0 {
                    no_sub_lt8 = false;
                }
            }
            if dmask != 0 {
                self.fs.direct8[self.addr] = dmask;
                self.direct(dmask)?;
            }
            let ref0 = self.stype == SliceType::P && raw == 4;
            let mut refs = [[0i8; 4]; 2];
            for l in 0..2 {
                for i in 0..4 {
                    if sub[i].1 & (1 << l) == 0 {
                        continue;
                    }
                    let (x4, y4) = ((i & 1) * 2, (i >> 1) * 2);
                    let r = if num[l] > 1 && !ref0 { e.ref_idx(self, l, x4, y4, num[l]) } else { 0 };
                    if r >= num[l] {
                        return Err(Error::Invalid("ref_idx"));
                    }
                    refs[l][i] = r as i8;
                    self.fill_ref(l, x4, y4, 2, 2, r as i8);
                }
            }
            for l in 0..2 {
                for i in 0..4 {
                    if sub[i].1 & (1 << l) == 0 {
                        continue;
                    }
                    let (bx, by) = ((i & 1) * 2, (i >> 1) * 2);
                    let parts: &[(usize, usize, usize, usize)] = match sub[i].0 {
                        0 => &[(0, 0, 2, 2)],
                        1 => &[(0, 0, 2, 1), (0, 1, 2, 1)],
                        2 => &[(0, 0, 1, 2), (1, 0, 1, 2)],
                        _ => &[(0, 0, 1, 1), (1, 0, 1, 1), (0, 1, 1, 1), (1, 1, 1, 1)],
                    };
                    for &(px, py, w4, h4) in parts {
                        let (x4, y4) = (bx + px, by + py);
                        let d = e.mvd(self, l, x4, y4, w4, h4);
                        let p = self.pred_mv(l, x4, y4, w4, refs[l][i], 0);
                        let mv = [(p[0] as i32 + d[0]) as i16, (p[1] as i32 + d[1]) as i16];
                        self.fill_mv(l, x4, y4, w4, h4, mv);
                    }
                }
            }
        } else {
            let (shape, p0, p1) = if self.stype == SliceType::P {
                match raw {
                    0 => (0, L0, 0),
                    1 => (1, L0, L0),
                    _ => (2, L0, L0),
                }
            } else {
                B_TYPES[raw as usize]
            };
            let parts: &[(usize, usize, usize, usize, u8, u8)] = match shape {
                0 => &[(0, 0, 4, 4, p0, 0)],
                1 => &[(0, 0, 4, 2, p0, 1), (0, 2, 4, 2, p1, 2)],
                _ => &[(0, 0, 2, 4, p0, 3), (2, 0, 2, 4, p1, 4)],
            };
            let mut refs = [[0i8; 2]; 2];
            for l in 0..2 {
                for (k, &(x4, y4, w4, h4, pr, _)) in parts.iter().enumerate() {
                    if pr & (1 << l) == 0 {
                        continue;
                    }
                    let r = if num[l] > 1 { e.ref_idx(self, l, x4, y4, num[l]) } else { 0 };
                    if r >= num[l] {
                        return Err(Error::Invalid("ref_idx"));
                    }
                    refs[l][k] = r as i8;
                    self.fill_ref(l, x4, y4, w4, h4, r as i8);
                }
            }
            for l in 0..2 {
                for (k, &(x4, y4, w4, h4, pr, sh)) in parts.iter().enumerate() {
                    if pr & (1 << l) == 0 {
                        continue;
                    }
                    let d = e.mvd(self, l, x4, y4, w4, h4);
                    let p = self.pred_mv(l, x4, y4, w4, refs[l][k], sh);
                    let mv = [(p[0] as i32 + d[0]) as i16, (p[1] as i32 + d[1]) as i16];
                    self.fill_mv(l, x4, y4, w4, h4, mv);
                }
            }
        }
        if e.overrun() {
            return Err(Error::Invalid("slice data overrun"));
        }
        let cbp = e.cbp(self, false)?;
        if cbp & 15 != 0 && self.pps.transform_8x8_mode && no_sub_lt8 && e.transform8(self) {
            mbt |= MB_8X8DCT;
        }
        self.fs.mbt[self.addr] = mbt;
        self.fs.cbp[self.addr] = cbp as u16;
        if cbp != 0 {
            self.qp_delta(e)?;
            self.residual(e, mbt, cbp)?;
        } else {
            self.prev_qp_delta_nz = false;
        }
        self.finish_mb_info(mbt, cbp, 0);
        self.mc_mb()?;
        self.add_residual_luma(mbt);
        self.add_residual_chroma();
        Ok(())
    }

    fn qp_delta<E: Entropy>(&mut self, e: &mut E) -> Result<(), Error> {
        let d = e.qp_delta(self);
        if !(-26..=25).contains(&d) {
            return Err(Error::Invalid("mb_qp_delta"));
        }
        self.prev_qp_delta_nz = d != 0;
        self.qp = (self.qp + d + 52) % 52;
        Ok(())
    }

    fn decode_intra<E: Entropy>(&mut self, e: &mut E, it: u32) -> Result<(), Error> {
        self.set_intra_motion();
        let s4 = self.s4();
        if it == 25 {
            // I_PCM
            let mut pcm = core::mem::replace(&mut self.pcm, Box::new([0; 384]));
            e.pcm(&mut pcm)?;
            let base = self.idx4(0, 0);
            for y in 0..4 {
                self.fs.nnz[base + y * s4..base + y * s4 + 4].fill(16);
            }
            let cs = self.mbw * 2;
            let plane = self.mbw * self.mbh * 4;
            for c in 0..2 {
                for y in 0..2 {
                    let o = c * plane + (self.mby * 2 + y) * cs + self.mbx * 2;
                    self.fs.nnzc[o..o + 2].fill(16);
                }
            }
            for y in 0..4 {
                for x in 0..4 {
                    self.fs.ipm[base + y * s4 + x] = 2;
                }
            }
            self.fs.cbp[self.addr] = 0x700;
            self.finish_mb_info(MB_INTRA | MB_PCM, 0x2f, 0);
            let pl = &mut self.cur.planes;
            let (x0, y0) = (self.mbx * 16, self.mby * 16);
            for y in 0..16 {
                let o = pl.yoff(x0, y0 + y);
                pl.y[o..o + 16].copy_from_slice(&pcm[y * 16..y * 16 + 16]);
            }
            for y in 0..8 {
                let o = pl.coff(x0 / 2, y0 / 2 + y);
                pl.u[o..o + 8].copy_from_slice(&pcm[256 + y * 8..256 + y * 8 + 8]);
                pl.v[o..o + 8].copy_from_slice(&pcm[320 + y * 8..320 + y * 8 + 8]);
            }
            self.pcm = pcm;
            self.prev_qp_delta_nz = false;
            return Ok(());
        }
        let mut mbt = MB_INTRA;
        let mut cbp;
        let mut i16_mode = 0;
        let base = self.idx4(0, 0);
        if it == 0 {
            mbt |= MB_I4;
            if self.pps.transform_8x8_mode && e.transform8(self) {
                mbt = MB_INTRA | MB_I8 | MB_8X8DCT;
            }
            self.fs.mbt[self.addr] = mbt;
            let n = if mbt & MB_I8 != 0 { 4 } else { 16 };
            for k in 0..n {
                let (x4, y4) = if n == 4 { ((k & 1) * 2, (k >> 1) * 2) } else { (BLK_X[k] as usize, BLK_Y[k] as usize) };
                let pred = self.pred_intra_mode(x4, y4);
                let mode = match e.intra_mode() {
                    None => pred,
                    Some(rem) => {
                        if rem < pred as u32 {
                            rem as i8
                        } else {
                            rem as i8 + 1
                        }
                    }
                };
                let i = base + y4 * s4 + x4;
                self.fs.ipm[i] = mode;
                if n == 4 {
                    self.fs.ipm[i + 1] = mode;
                    self.fs.ipm[i + s4] = mode;
                    self.fs.ipm[i + s4 + 1] = mode;
                }
            }
            cbp = 0;
        } else {
            mbt |= MB_I16;
            i16_mode = (it - 1) % 4;
            cbp = (((it - 1) / 4) % 3) << 4;
            if it >= 13 {
                cbp |= 15;
            }
            for y in 0..4 {
                self.fs.ipm[base + y * s4..base + y * s4 + 4].fill(2);
            }
        }
        self.fs.mbt[self.addr] = mbt;
        let cmode = e.chroma_mode(self);
        if cmode > 3 {
            return Err(Error::Invalid("intra_chroma_pred_mode"));
        }
        self.fs.chroma_pred[self.addr] = cmode as u8;
        if e.overrun() {
            return Err(Error::Invalid("slice data overrun"));
        }
        if mbt & MB_I16 == 0 {
            cbp = e.cbp(self, true)?;
        }
        self.fs.cbp[self.addr] = cbp as u16;
        if cbp != 0 || mbt & MB_I16 != 0 {
            self.qp_delta(e)?;
            self.residual(e, mbt, cbp)?;
        } else {
            self.prev_qp_delta_nz = false;
        }
        self.finish_mb_info(mbt, cbp, cmode as u8);
        self.recon_intra(mbt, i16_mode, cmode)?;
        Ok(())
    }

    /// predIntra4x4PredMode / predIntra8x8PredMode (8.3.1.1, 8.3.2.1)
    fn pred_intra_mode(&self, x4: usize, y4: usize) -> i8 {
        let gx = (self.mbx * 4 + x4) as isize;
        let gy = (self.mby * 4 + y4) as isize;
        let a = self.nb4(gx - 1, gy);
        let b = self.nb4(gx, gy - 1);
        let (a, b) = match (a, b) {
            (Some(a), Some(b)) => (a, b),
            _ => return 2,
        };
        let s4 = self.s4();
        let ma = a / s4 / 4 * self.mbw + (a % s4) / 4;
        let mb = b / s4 / 4 * self.mbw + (b % s4) / 4;
        let ta = self.fs.mbt[ma];
        let tb = self.fs.mbt[mb];
        if self.pps.constrained_intra_pred && (ta & MB_INTER != 0 || tb & MB_INTER != 0) {
            return 2;
        }
        let mode = |t: u32, i: usize| if t & (MB_I4 | MB_I8) != 0 { self.fs.ipm[i] } else { 2 };
        mode(ta, a).min(mode(tb, b))
    }

    // ------------------------------------------------------------------ residual

    fn residual<E: Entropy>(&mut self, e: &mut E, mbt: u32, cbp: u32) -> Result<(), Error> {
        let mut cf = Coefs { lv: [0; 64], pos: [0; 64] };
        let qp = self.qp as usize;
        let intra = mbt & MB_INTRA != 0;
        let li = if intra { 0 } else { 3 };
        let s4 = self.s4();
        let base = self.idx4(0, 0);
        self.nz_luma = 0;
        self.nz_chroma = 0;
        if mbt & MB_I16 != 0 {
            let n = e.residual(self, 0, 0, 16, &mut cf)?;
            if n > 0 {
                self.fs.cbp[self.addr] |= 0x100;
                let mut c = [0i32; 16];
                for k in 0..n {
                    c[ZIGZAG4[cf.pos[k] as usize & 15] as usize] = cf.lv[k];
                }
                let dc = idct::luma_dc(&c, self.dq.d4[0][qp][0]);
                for r in 0..16 {
                    if dc[r] != 0 {
                        let blk = BLK_IDX[r] as usize;
                        self.coef[blk * 16] = dc[r];
                        self.nz_luma |= 1 << blk;
                    }
                }
            }
        }
        for b8 in 0..4 {
            if cbp & (1 << b8) == 0 {
                continue;
            }
            if mbt & MB_8X8DCT != 0 {
                let d8 = &self.dq.d8[if intra { 0 } else { 1 }][qp];
                let (bx, by) = ((b8 & 1) * 2, (b8 >> 1) * 2);
                if self.cabac {
                    let n = e.residual(self, 5, b8 * 4, 64, &mut cf)?;
                    for k in 0..n {
                        let p = ZIGZAG8[cf.pos[k] as usize & 63] as usize;
                        self.coef[b8 * 64 + p] = cf.lv[k].wrapping_mul(d8[p]).wrapping_add(32) >> 6;
                    }
                    if n > 0 {
                        self.nz_luma |= 1 << b8;
                    }
                    for y in 0..2 {
                        self.fs.nnz[base + (by + y) * s4 + bx..base + (by + y) * s4 + bx + 2].fill(n as u8);
                    }
                } else {
                    for i in 0..4 {
                        let blk = b8 * 4 + i;
                        let n = e.residual(self, 2, blk, 16, &mut cf)?;
                        for k in 0..n {
                            let p = ZIGZAG8[4 * (cf.pos[k] as usize & 15) + i] as usize;
                            self.coef[b8 * 64 + p] = cf.lv[k].wrapping_mul(d8[p]).wrapping_add(32) >> 6;
                        }
                        if n > 0 {
                            self.nz_luma |= 1 << b8;
                        }
                        self.fs.nnz[base + BLK_Y[blk] as usize * s4 + BLK_X[blk] as usize] = n as u8;
                    }
                }
            } else {
                let d4 = &self.dq.d4[li][qp];
                for i in 0..4 {
                    let blk = b8 * 4 + i;
                    let (cat, start, max) = if mbt & MB_I16 != 0 { (1, 1, 15) } else { (2, 0, 16) };
                    let n = e.residual(self, cat, blk, max, &mut cf)?;
                    if n > 0 {
                        for k in 0..n {
                            let p = ZIGZAG4[(cf.pos[k] as usize + start) & 15] as usize;
                            self.coef[blk * 16 + p] = cf.lv[k].wrapping_mul(d4[p]).wrapping_add(8) >> 4;
                        }
                        self.nz_luma |= 1 << blk;
                    }
                    self.fs.nnz[base + BLK_Y[blk] as usize * s4 + BLK_X[blk] as usize] = n as u8;
                }
            }
        }
        let cbpc = (cbp >> 4) & 3;
        if cbpc != 0 {
            let qpc = [chroma_qp(self.qp, self.pps.chroma_qp_offset[0]) as usize, chroma_qp(self.qp, self.pps.chroma_qp_offset[1]) as usize];
            for c in 0..2 {
                let n = e.residual(self, 3, c, 4, &mut cf)?;
                if n > 0 {
                    self.fs.cbp[self.addr] |= 0x200 << c;
                    let mut cl = [0i32; 4];
                    for k in 0..n {
                        cl[cf.pos[k] as usize & 3] = cf.lv[k];
                    }
                    let dc = idct::chroma_dc(&cl, self.dq.d4[li + 1 + c][qpc[c]][0]);
                    for b in 0..4 {
                        if dc[b] != 0 {
                            self.coef[256 + c * 64 + b * 16] = dc[b];
                            self.nz_chroma |= 1 << (c * 4 + b);
                        }
                    }
                }
            }
            if cbpc == 2 {
                let cs = self.mbw * 2;
                let plane = self.mbw * self.mbh * 4;
                for c in 0..2 {
                    let d4 = &self.dq.d4[li + 1 + c][qpc[c]];
                    for b in 0..4 {
                        let n = e.residual(self, 4, c * 4 + b, 15, &mut cf)?;
                        if n > 0 {
                            for k in 0..n {
                                let p = ZIGZAG4[(cf.pos[k] as usize + 1) & 15] as usize;
                                self.coef[256 + c * 64 + b * 16 + p] = cf.lv[k].wrapping_mul(d4[p]).wrapping_add(8) >> 4;
                            }
                            self.nz_chroma |= 1 << (c * 4 + b);
                        }
                        let o = c * plane + (self.mby * 2 + (b >> 1)) * cs + self.mbx * 2 + (b & 1);
                        self.fs.nnzc[o] = n as u8;
                    }
                }
            }
        }
        if e.overrun() {
            return Err(Error::Invalid("slice data overrun"));
        }
        Ok(())
    }

    fn add_residual_luma(&mut self, mbt: u32) {
        if self.nz_luma == 0 {
            return;
        }
        let pl = &mut self.cur.planes;
        let ys = pl.ys;
        let o0 = pl.yoff(self.mbx * 16, self.mby * 16);
        if mbt & MB_8X8DCT != 0 {
            for b8 in 0..4 {
                if self.nz_luma & (1 << b8) != 0 {
                    let o = o0 + (b8 >> 1) * 8 * ys + (b8 & 1) * 8;
                    idct::idct8_add(&mut self.coef[b8 * 64..b8 * 64 + 64], &mut pl.y, o, ys);
                }
            }
        } else {
            for blk in 0..16 {
                if self.nz_luma & (1 << blk) != 0 {
                    let o = o0 + BLK_Y[blk] as usize * 4 * ys + BLK_X[blk] as usize * 4;
                    idct::idct4_add(&mut self.coef[blk * 16..blk * 16 + 16], &mut pl.y, o, ys);
                }
            }
        }
        self.nz_luma = 0;
    }

    fn add_residual_chroma(&mut self) {
        if self.nz_chroma == 0 {
            return;
        }
        let pl = &mut self.cur.planes;
        let cs = pl.cs;
        let o0 = pl.coff(self.mbx * 8, self.mby * 8);
        for c in 0..2 {
            for b in 0..4 {
                if self.nz_chroma & (1 << (c * 4 + b)) != 0 {
                    let o = o0 + (b >> 1) * 4 * cs + (b & 1) * 4;
                    let buf = if c == 0 { &mut pl.u } else { &mut pl.v };
                    let cf = &mut self.coef[256 + c * 64 + b * 16..256 + c * 64 + b * 16 + 16];
                    idct::idct4_add(cf, buf, o, cs);
                }
            }
        }
        self.nz_chroma = 0;
    }

    // ------------------------------------------------------------------ intra reconstruction

    /// Is the neighbouring macroblock usable for intra prediction?
    fn intra_avail(&self, dx: isize, dy: isize) -> bool {
        let x = self.mbx as isize + dx;
        let y = self.mby as isize + dy;
        if !self.avail(x, y) {
            return false;
        }
        if dx == 0 && dy == 0 {
            return true;
        }
        if self.pps.constrained_intra_pred {
            let t = self.fs.mbt[y as usize * self.mbw + x as usize];
            return t & MB_INTER == 0;
        }
        true
    }

    fn recon_intra(&mut self, mbt: u32, i16_mode: u32, cmode: u32) -> Result<(), Error> {
        let left = self.intra_avail(-1, 0);
        let top = self.intra_avail(0, -1);
        let tl = self.intra_avail(-1, -1);
        let tr = self.intra_avail(1, -1);
        let ys = self.cur.planes.ys;
        let o0 = self.cur.planes.yoff(self.mbx * 16, self.mby * 16);
        if mbt & MB_I16 != 0 {
            let pl = &mut self.cur.planes;
            let mut t = [128i32; 17];
            let mut l = [128i32; 17];
            if top {
                for x in 0..16 {
                    t[1 + x] = pl.y[o0 - ys + x] as i32;
                }
            }
            if left {
                for y in 0..16 {
                    l[1 + y] = pl.y[o0 + y * ys - 1] as i32;
                }
            }
            if tl {
                t[0] = pl.y[o0 - ys - 1] as i32;
                l[0] = t[0];
            }
            match i16_mode {
                0 if !top => return Err(Error::Invalid("intra16 V without top")),
                1 if !left => return Err(Error::Invalid("intra16 H without left")),
                3 if !(top && left && tl) => return Err(Error::Invalid("intra16 plane without neighbours")),
                3 => intra::pred16_plane(&t, &l, &mut pl.y, o0, ys),
                m => intra::pred_vh_dc(16, m, &t[1..], &l[1..], top, left, &mut pl.y, o0, ys),
            }
            self.add_residual_luma(mbt);
        } else if mbt & MB_I8 != 0 {
            let s4 = self.s4();
            let base = self.idx4(0, 0);
            for b8 in 0..4 {
                let (bx, by) = (b8 & 1, b8 >> 1);
                let has_left = bx > 0 || left;
                let has_top = by > 0 || top;
                let has_tl = match (bx, by) {
                    (0, 0) => tl,
                    (0, _) => left,
                    (_, 0) => top,
                    _ => true,
                };
                let has_tr = match (bx, by) {
                    (0, 0) => top,
                    (1, 0) => tr,
                    (0, 1) => true,
                    _ => false,
                };
                let o = o0 + by * 8 * ys + bx * 8;
                let pl = &mut self.cur.planes;
                let mut t = [128i32; 17];
                let mut l = [128i32; 9];
                if has_top {
                    for x in 0..8 {
                        t[1 + x] = pl.y[o - ys + x] as i32;
                    }
                    for x in 8..16 {
                        t[1 + x] = if has_tr { pl.y[o - ys + x] as i32 } else { t[8] };
                    }
                }
                if has_left {
                    for y in 0..8 {
                        l[1 + y] = pl.y[o + y * ys - 1] as i32;
                    }
                }
                if has_tl {
                    t[0] = pl.y[o - ys - 1] as i32;
                    l[0] = t[0];
                }
                let mode = self.fs.ipm[base + by * 2 * s4 + bx * 2] as u32;
                check_dir_mode(mode, has_top, has_left, has_tl)?;
                intra::filter8(&mut t, &mut l, has_top, has_left, has_tl);
                if mode <= 2 {
                    intra::pred_vh_dc(8, mode, &t[1..], &l[1..], has_top, has_left, &mut pl.y, o, ys);
                } else {
                    intra::pred_dir(8, mode, &t, &l, &mut pl.y, o, ys);
                }
                if self.nz_luma & (1 << b8) != 0 {
                    idct::idct8_add(&mut self.coef[b8 * 64..b8 * 64 + 64], &mut pl.y, o, ys);
                }
            }
            self.nz_luma = 0;
        } else {
            let s4 = self.s4();
            let base = self.idx4(0, 0);
            for blk in 0..16 {
                let (bx, by) = (BLK_X[blk] as usize, BLK_Y[blk] as usize);
                let has_left = bx > 0 || left;
                let has_top = by > 0 || top;
                let has_tl = match (bx, by) {
                    (0, 0) => tl,
                    (0, _) => left,
                    (_, 0) => top,
                    _ => true,
                };
                let has_tr = if by == 0 {
                    if bx < 3 {
                        top
                    } else {
                        tr
                    }
                } else if bx == 3 {
                    false
                } else {
                    BLK_IDX[(by - 1) * 4 + bx + 1] < blk as u8
                };
                let o = o0 + by * 4 * ys + bx * 4;
                let pl = &mut self.cur.planes;
                let mut t = [128i32; 17];
                let mut l = [128i32; 9];
                if has_top {
                    for x in 0..4 {
                        t[1 + x] = pl.y[o - ys + x] as i32;
                    }
                    for x in 4..8 {
                        t[1 + x] = if has_tr { pl.y[o - ys + x] as i32 } else { t[4] };
                    }
                }
                if has_left {
                    for y in 0..4 {
                        l[1 + y] = pl.y[o + y * ys - 1] as i32;
                    }
                }
                if has_tl {
                    t[0] = pl.y[o - ys - 1] as i32;
                    l[0] = t[0];
                }
                let mode = self.fs.ipm[base + by * s4 + bx] as u32;
                check_dir_mode(mode, has_top, has_left, has_tl)?;
                if mode <= 2 {
                    intra::pred_vh_dc(4, mode, &t[1..], &l[1..], has_top, has_left, &mut pl.y, o, ys);
                } else {
                    intra::pred_dir(4, mode, &t, &l, &mut pl.y, o, ys);
                }
                if self.nz_luma & (1 << blk) != 0 {
                    idct::idct4_add(&mut self.coef[blk * 16..blk * 16 + 16], &mut pl.y, o, ys);
                }
            }
            self.nz_luma = 0;
        }
        // chroma
        match cmode {
            1 if !left => return Err(Error::Invalid("chroma H without left")),
            2 if !top => return Err(Error::Invalid("chroma V without top")),
            3 if !(top && left && tl) => return Err(Error::Invalid("chroma plane without neighbours")),
            _ => {}
        }
        let pl = &mut self.cur.planes;
        let cs = pl.cs;
        let oc = pl.coff(self.mbx * 8, self.mby * 8);
        for c in 0..2 {
            let buf = if c == 0 { &mut pl.u } else { &mut pl.v };
            let mut t = [128i32; 9];
            let mut l = [128i32; 9];
            if top {
                for x in 0..8 {
                    t[1 + x] = buf[oc - cs + x] as i32;
                }
            }
            if left {
                for y in 0..8 {
                    l[1 + y] = buf[oc + y * cs - 1] as i32;
                }
            }
            if tl {
                t[0] = buf[oc - cs - 1] as i32;
                l[0] = t[0];
            }
            intra::pred_chroma(cmode, &t, &l, top, left, buf, oc, cs);
        }
        self.add_residual_chroma();
        Ok(())
    }

    // ------------------------------------------------------------------ motion compensation

    fn weights(&self, r0: i8, r1: i8) -> Option<Wp> {
        let explicit = (self.stype == SliceType::P && self.pps.weighted_pred) || (self.stype == SliceType::B && self.pps.weighted_bipred_idc == 1);
        if explicit {
            let sh = self.sh;
            let mut wp = Wp { log_wd: [sh.luma_log2_wd as i32, sh.chroma_log2_wd as i32, sh.chroma_log2_wd as i32], w: [[0; 2]; 3], o: [[0; 2]; 3] };
            for (l, r) in [(0usize, r0), (1usize, r1)] {
                if r < 0 {
                    continue;
                }
                let w = sh.weights[l].get(r as usize).copied().unwrap_or([1 << sh.luma_log2_wd, 0, 1 << sh.chroma_log2_wd, 0, 1 << sh.chroma_log2_wd, 0]);
                for c in 0..3 {
                    wp.w[c][l] = w[c * 2];
                    wp.o[c][l] = w[c * 2 + 1];
                }
            }
            // single-list prediction with default weights is plain copying
            if (r0 < 0 || r1 < 0)
                && (0..3).all(|c| {
                    let l = if r0 >= 0 { 0 } else { 1 };
                    wp.w[c][l] == 1 << wp.log_wd[c] && wp.o[c][l] == 0
                })
            {
                return None;
            }
            return Some(wp);
        }
        if self.stype == SliceType::B && self.pps.weighted_bipred_idc == 2 && r0 >= 0 && r1 >= 0 {
            let p0 = &self.pics[self.lists[0][r0 as usize]];
            let p1 = &self.pics[self.lists[1][r1 as usize]];
            let (mut w0, mut w1) = (32, 32);
            let diff = p1.poc - p0.poc;
            if diff != 0 && p0.reference != REF_LONG && p1.reference != REF_LONG {
                let tb = (self.cur_poc - p0.poc).clamp(-128, 127);
                let td = diff.clamp(-128, 127);
                let tx = (16384 + (td / 2).abs()) / td;
                let dsf = ((tb * tx + 32) >> 6).clamp(-1024, 1023);
                if !((dsf >> 2) < -64 || (dsf >> 2) > 128) {
                    w0 = 64 - (dsf >> 2);
                    w1 = dsf >> 2;
                }
            }
            if w0 == 32 && w1 == 32 {
                return None;
            }
            return Some(Wp { log_wd: [5; 3], w: [[w0, w1]; 3], o: [[0; 2]; 3] });
        }
        None
    }

    fn mc_block(&mut self, x4: usize, y4: usize, w4: usize, h4: usize) -> Result<(), Error> {
        let i = self.idx4(x4, y4);
        let r = [self.cur.refi[0][i], self.cur.refi[1][i]];
        let mv = [self.cur.mv[0][i], self.cur.mv[1][i]];
        let (w, h) = (w4 * 4, h4 * 4);
        let px = (self.mbx * 16 + x4 * 4) as i32;
        let py = (self.mby * 16 + y4 * 4) as i32;
        const NO: usize = usize::MAX;
        let mut pis = [NO; 2];
        for l in 0..2 {
            if r[l] < 0 {
                continue;
            }
            let pi = *self.lists[l].get(r[l] as usize).ok_or(Error::Invalid("ref_idx beyond list"))?;
            let rp = &self.pics[pi].planes;
            if rp.w != self.cur.planes.w || rp.h != self.cur.planes.h {
                return Err(Error::Invalid("reference size mismatch"));
            }
            pis[l] = pi;
        }
        if pis == [NO; 2] {
            return Err(Error::Invalid("inter block without prediction"));
        }
        // start fetching both references before computing either prediction
        for l in 0..2 {
            if pis[l] != NO {
                inter::prefetch(&self.pics[pis[l]].planes, px, py, mv[l], w, h);
            }
        }
        let wp = self.weights(r[0], r[1]);
        let pl = &mut self.cur.planes;
        let yo = pl.yoff(px as usize, py as usize);
        let co = pl.coff(px as usize / 2, py as usize / 2);
        let (ys, cs) = (pl.ys, pl.cs);
        if wp.is_none() && (pis[0] == NO || pis[1] == NO) {
            // one unweighted prediction: interpolate straight into the picture
            let l = if pis[0] != NO { 0 } else { 1 };
            let rp = &self.pics[pis[l]].planes;
            inter::mc_luma(rp, px, py, mv[l], w, h, &mut pl.y[yo..], ys, &mut self.mct);
            inter::mc_chroma(&rp.u, rp, px / 2, py / 2, mv[l], w / 2, h / 2, &mut pl.u[co..], cs);
            inter::mc_chroma(&rp.v, rp, px / 2, py / 2, mv[l], w / 2, h / 2, &mut pl.v[co..], cs);
            return Ok(());
        }
        for l in 0..2 {
            if pis[l] == NO {
                continue;
            }
            let rp = &self.pics[pis[l]].planes;
            inter::mc_luma(rp, px, py, mv[l], w, h, &mut self.tmp[l][..], 16, &mut self.mct);
            inter::mc_chroma(&rp.u, rp, px / 2, py / 2, mv[l], w / 2, h / 2, &mut self.tmpc[l][0][..], 8);
            inter::mc_chroma(&rp.v, rp, px / 2, py / 2, mv[l], w / 2, h / 2, &mut self.tmpc[l][1][..], 8);
        }
        let used = [pis[0] != NO, pis[1] != NO];
        let t0 = if used[0] { Some(&self.tmp[0][..]) } else { None };
        let t1 = if used[1] { Some(&self.tmp[1][..]) } else { None };
        inter::weight(0, t0, t1, 16, w, h, wp.as_ref(), &mut pl.y, yo, ys);
        for c in 0..2 {
            let c0 = if used[0] { Some(&self.tmpc[0][c][..]) } else { None };
            let c1 = if used[1] { Some(&self.tmpc[1][c][..]) } else { None };
            let buf = if c == 0 { &mut pl.u } else { &mut pl.v };
            inter::weight(1 + c, c0, c1, 8, w / 2, h / 2, wp.as_ref(), buf, co, cs);
        }
        Ok(())
    }

    fn uniform(&self, x4: usize, y4: usize, w4: usize, h4: usize) -> bool {
        let i0 = self.idx4(x4, y4);
        let s4 = self.s4();
        let (r0, r1, m0, m1) = (self.cur.refi[0][i0], self.cur.refi[1][i0], self.cur.mv[0][i0], self.cur.mv[1][i0]);
        for y in 0..h4 {
            for x in 0..w4 {
                let i = i0 + y * s4 + x;
                if self.cur.refi[0][i] != r0 || self.cur.refi[1][i] != r1 || (r0 >= 0 && self.cur.mv[0][i] != m0) || (r1 >= 0 && self.cur.mv[1][i] != m1) {
                    return false;
                }
            }
        }
        true
    }

    fn mc_mb(&mut self) -> Result<(), Error> {
        if self.uniform(0, 0, 4, 4) {
            return self.mc_block(0, 0, 4, 4);
        }
        if self.uniform(0, 0, 4, 2) && self.uniform(0, 2, 4, 2) {
            self.mc_block(0, 0, 4, 2)?;
            return self.mc_block(0, 2, 4, 2);
        }
        if self.uniform(0, 0, 2, 4) && self.uniform(2, 0, 2, 4) {
            self.mc_block(0, 0, 2, 4)?;
            return self.mc_block(2, 0, 2, 4);
        }
        for b8 in 0..4 {
            let (bx, by) = ((b8 & 1) * 2, (b8 >> 1) * 2);
            if self.uniform(bx, by, 2, 2) {
                self.mc_block(bx, by, 2, 2)?;
            } else {
                for k in 0..4 {
                    self.mc_block(bx + (k & 1), by + (k >> 1), 1, 1)?;
                }
            }
        }
        Ok(())
    }
}

fn check_dir_mode(mode: u32, top: bool, left: bool, tl: bool) -> Result<(), Error> {
    let ok = match mode {
        0 | 3 | 7 => top,
        1 | 8 => left,
        2 => true,
        4 | 5 | 6 => top && left && tl,
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid("intra mode needs unavailable neighbours"))
    }
}
