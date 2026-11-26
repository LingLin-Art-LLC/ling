//! Slice and macroblock coding: mode decision, motion search, transform /
//! quantisation, reconstruction and the CAVLC macroblock layer.

use super::bw::{se_bits, Bw};
use super::cavlc::write_block;
use super::dsp::*;
use super::{bw, Cur, Encoder, Fp, SliceOut};
use crate::idct;
use crate::intra;
use crate::tables::{chroma_qp, BLK_IDX, BLK_X, BLK_Y, ZIGZAG4};

/// Macroblock decisions of a slice (or a frame, summed).
#[derive(Clone, Copy, Default, Debug)]
pub struct SliceStats {
    pub skip: u32,
    pub inter: u32,
    pub i16: u32,
    pub i4: u32,
}

impl SliceStats {
    pub fn add(&mut self, o: &SliceStats) {
        self.skip += o.skip;
        self.inter += o.inter;
        self.i16 += o.i16;
        self.i4 += o.i4;
    }
}

const K_INTER: u8 = 0;
const K_I16: u8 = 1;
const K_I4: u8 = 2;

/// x264's lambda table (SAD / SATD domain) by QP.
const LAMBDA: [u32; 52] = [
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 6, 6, 7, 8, 9, 10, 11, 13, 14, 16, 18,
    20, 23, 25, 29, 32, 36, 40, 45, 51, 57, 64, 72,
];

/// Longest vectors searched (full pels); the vertical one stays inside every level's range.
const MAX_MV_X: i32 = 1024;
const MAX_MV_Y: i32 = 480;
/// Line search reach (full pels) for scrolling / dragging.
const LINE_Y: i32 = 128;
const LINE_X: i32 = 96;

struct S<'a> {
    e: &'a Encoder,
    cur: Cur,
    qp: i32,
    qpc: i32,
    lam: u32,
    mbw: usize,
    mbh: usize,
    ys: usize,
    cs: usize,
    r0: usize,
    sy: &'a [u8],
    su: &'a [u8],
    sv: &'a [u8],
    py: &'a [u8],
    pu: &'a [u8],
    pv: &'a [u8],
    ry: &'a mut [u8],
    ru: &'a mut [u8],
    rv: &'a mut [u8],
    fy: &'a [u8],
    fu: &'a [u8],
    fv: &'a [u8],
    mv: &'a mut [[i16; 2]],
    pmv: &'a [[i16; 2]],
    kind: &'a mut [u8],
    nnz: &'a mut [[u8; 16]],
    nnzc: &'a mut [[u8; 8]],
    i4: &'a mut [[u8; 16]],
    cqp: &'a mut [u8],
    bw: Bw<'a>,
    skip_run: u32,
    hints: [[i32; 2]; 4],
    nh: usize,
    line_fail: u32,
    st: SliceStats,
}

/// Quantised chroma of one macroblock.
struct ChromaLv {
    dc: [[i32; 4]; 2],
    /// raster levels per 4x4 block (index 0 unused)
    ac: [[[i32; 16]; 4]; 2],
    nac: [[u8; 4]; 2],
}

/// The intra choice for a macroblock's luma.
struct Intra {
    i4: bool,
    mode16: u8,
    /// Intra 4x4 modes and their predicted modes, by blkIdx
    modes: [u8; 16],
    pm: [u8; 16],
    /// raster levels per blkIdx (Intra 16x16: AC only)
    lv: [[i32; 16]; 16],
    cnt: [u8; 16],
    /// Intra 16x16 DC levels (raster of blocks)
    dc: [i32; 16],
    cost: u32,
}

pub(super) unsafe fn encode(e: &Encoder, fp: &Fp, cur: Cur, qp: i32, r0: usize, rows: usize, o: &mut SliceOut) {
    let mbw = e.mbw;
    let (ys, cs) = (mbw * 16, mbw * 8);
    let (ly, lc) = (rows * 16 * ys, rows * 8 * cs);
    let (oy, oc) = (r0 * 16 * ys, r0 * 8 * cs);
    let fy_len = e.mbh * 16 * ys;
    let fc_len = e.mbh * 8 * cs;
    let m0 = r0 * mbw;
    let nm = rows * mbw;
    use core::slice::{from_raw_parts as sl, from_raw_parts_mut as slm};
    let (len, over, st) = {
        let mut s = S {
            e,
            cur,
            qp,
            qpc: chroma_qp(qp, 0),
            lam: LAMBDA[qp as usize],
            mbw,
            mbh: e.mbh,
            ys,
            cs,
            r0,
            sy: sl(fp.src[0].add(oy), ly),
            su: sl(fp.src[1].add(oc), lc),
            sv: sl(fp.src[2].add(oc), lc),
            py: sl(fp.psrc[0].add(oy), ly),
            pu: sl(fp.psrc[1].add(oc), lc),
            pv: sl(fp.psrc[2].add(oc), lc),
            ry: slm(fp.rec[0].add(oy), ly),
            ru: slm(fp.rec[1].add(oc), lc),
            rv: slm(fp.rec[2].add(oc), lc),
            fy: sl(fp.refp[0], fy_len),
            fu: sl(fp.refp[1], fc_len),
            fv: sl(fp.refp[2], fc_len),
            mv: slm(fp.mv.add(m0), nm),
            pmv: sl(fp.pmv.add(m0), nm),
            kind: slm(fp.kind.add(m0), nm),
            nnz: slm(fp.nnz.add(m0), nm),
            nnzc: slm(fp.nnzc.add(m0), nm),
            i4: slm(fp.i4.add(m0), nm),
            cqp: slm(fp.cqp.add(m0), nm),
            bw: Bw::new(&mut o.rbsp[..]),
            skip_run: 0,
            hints: [[0; 2]; 4],
            nh: 0,
            line_fail: 0,
            st: SliceStats::default(),
        };
        s.header();
        for my in r0..r0 + rows {
            for mx in 0..mbw {
                s.mb(mx, my);
                if s.bw.over {
                    break;
                }
            }
        }
        if s.skip_run > 0 {
            s.bw.ue(s.skip_run);
        }
        s.bw.trailing();
        (s.bw.len(), s.bw.over, s.st)
    };
    o.qp = qp;
    o.stats = st;
    o.len = 0;
    if over {
        o.over = true;
        return;
    }
    o.nal[4] = if cur.key { 0x65 } else { 0x41 };
    match bw::escape(&o.rbsp[..len], &mut o.nal[5..]) {
        Some(n) => {
            o.nal[..4].copy_from_slice(&((n + 1) as u32).to_be_bytes());
            o.len = n + 5;
        },
        None => o.over = true,
    }
}

#[inline(always)]
fn median(a: i16, b: i16, c: i16) -> i16 {
    a.max(b).min(a.min(b).max(c))
}

impl<'a> S<'a> {
    fn header(&mut self) {
        let key = self.cur.key;
        self.bw.ue((self.r0 * self.mbw) as u32); // first_mb_in_slice
        self.bw.ue(if key { 2 } else { 0 }); // slice_type I / P
        self.bw.ue(0); // pic_parameter_set_id
        self.bw.put(self.cur.frame_num, super::LOG2_MAX_FRAME_NUM);
        if key {
            self.bw.ue(self.cur.idr_id);
        } else {
            self.bw.flag(false); // num_ref_idx_active_override_flag
            self.bw.flag(false); // ref_pic_list_modification_flag_l0
        }
        if key {
            self.bw.flag(false); // no_output_of_prior_pics_flag
            self.bw.flag(false); // long_term_reference_flag
        } else {
            self.bw.flag(false); // adaptive_ref_pic_marking_mode_flag
        }
        self.bw.se(self.qp - 26);
        self.bw.ue(1); // disable_deblocking_filter_idc: off
    }

    #[inline(always)]
    fn mi(&self, mx: usize, my: usize) -> usize {
        (my - self.r0) * self.mbw + mx
    }

    /// luma offset of a macroblock in the slice's planes
    #[inline(always)]
    fn lo(&self, mx: usize, my: usize) -> usize {
        (my - self.r0) * 16 * self.ys + mx * 16
    }

    #[inline(always)]
    fn co(&self, mx: usize, my: usize) -> usize {
        (my - self.r0) * 8 * self.cs + mx * 8
    }

    fn mb(&mut self, mx: usize, my: usize) {
        if self.cur.key {
            let c = self.intra_luma(mx, my, u32::MAX);
            self.write_intra(mx, my, &c);
            return;
        }
        let mi = self.mi(mx, my);
        let (mvp, smv) = self.mv_preds(mx, my);
        let unchanged = self.same(mx, my) && (self.cqp[mi] as i32) <= self.qp + 3;
        if unchanged {
            if smv == [0, 0] {
                self.skip(mx, my, smv);
            } else {
                self.code_inter(mx, my, [0, 0], smv, mvp);
            }
            return;
        }
        let (mv, sad) = self.search(mx, my, mvp);
        let mvq = [(mv[0] * 4) as i16, (mv[1] * 4) as i16];
        if sad > 256 * 2 {
            // compare with intra
            let mut pl = [0u8; 256];
            self.pred_luma(mx, my, mvq, &mut pl);
            let lo = self.lo(mx, my);
            let inter = satd(self.sy, lo, self.ys, &pl, 0, 16, 16, 16)
                + self.lam * (se_bits(mvq[0] as i32 - mvp[0] as i32) + se_bits(mvq[1] as i32 - mvp[1] as i32) + 2);
            let c = self.intra_luma(mx, my, inter);
            if c.cost < inter {
                self.write_intra(mx, my, &c);
                return;
            }
        }
        self.code_inter(mx, my, mvq, smv, mvp);
    }

    /// Source of this macroblock unchanged since the previous frame.
    fn same(&self, mx: usize, my: usize) -> bool {
        let lo = self.lo(mx, my);
        for y in 0..16 {
            let o = lo + y * self.ys;
            if self.sy[o..o + 16] != self.py[o..o + 16] {
                return false;
            }
        }
        let co = self.co(mx, my);
        for y in 0..8 {
            let o = co + y * self.cs;
            if self.su[o..o + 8] != self.pu[o..o + 8] || self.sv[o..o + 8] != self.pv[o..o + 8] {
                return false;
            }
        }
        true
    }

    // ---------------------------------------------------------------- motion vectors

    /// (available, refIdx, mv) of the neighbours A, B, C (D when C is unavailable).
    fn neighbours(&self, mx: usize, my: usize) -> [(bool, i8, [i16; 2]); 3] {
        const NA: (bool, i8, [i16; 2]) = (false, -1, [0, 0]);
        let get = |x: usize, y: usize| {
            let i = self.mi(x, y);
            if self.kind[i] == K_INTER {
                (true, 0i8, self.mv[i])
            } else {
                (true, -1i8, [0i16, 0])
            }
        };
        let top = my > self.r0;
        let a = if mx > 0 { get(mx - 1, my) } else { NA };
        let b = if top { get(mx, my - 1) } else { NA };
        let c = if top && mx + 1 < self.mbw {
            get(mx + 1, my - 1)
        } else if top && mx > 0 {
            get(mx - 1, my - 1)
        } else {
            NA
        };
        [a, b, c]
    }

    /// (mvp for P_L0_16x16, the P_Skip vector), quarter pels.
    fn mv_preds(&self, mx: usize, my: usize) -> ([i16; 2], [i16; 2]) {
        let [a, mut b, mut c] = self.neighbours(mx, my);
        let skip_zero = !a.0 || !b.0 || (a.1 == 0 && a.2 == [0, 0]) || (b.1 == 0 && b.2 == [0, 0]);
        if !b.0 && !c.0 && a.0 {
            b = a;
            c = a;
        }
        let m = (a.1 == 0) as u8 + (b.1 == 0) as u8 + (c.1 == 0) as u8;
        let mvp = if m == 1 {
            if a.1 == 0 {
                a.2
            } else if b.1 == 0 {
                b.2
            } else {
                c.2
            }
        } else {
            [median(a.2[0], b.2[0], c.2[0]), median(a.2[1], b.2[1], c.2[1])]
        };
        (mvp, if skip_zero { [0, 0] } else { mvp })
    }

    /// Full-pel motion search; returns (vector in full pels, SAD).
    fn search(&mut self, mx: usize, my: usize, mvp: [i16; 2]) -> ([i32; 2], u32) {
        let (gx, gy) = ((mx * 16) as i32, (my * 16) as i32);
        let xmin = (-gx).max(-MAX_MV_X);
        let xmax = ((self.mbw * 16) as i32 - 16 - gx).min(MAX_MV_X);
        let ymin = (-gy).max(-MAX_MV_Y);
        let ymax = ((self.mbh * 16) as i32 - 16 - gy).min(MAX_MV_Y);
        let lo = self.lo(mx, my);
        let (sy, fy, ys, lam) = (self.sy, self.fy, self.ys, self.lam);
        let base = gy as usize * ys + gx as usize;
        let sad_at = |x: i32, y: i32| -> u32 { sad16(sy, lo, ys, fy, (base as isize + y as isize * ys as isize + x as isize) as usize, ys) };
        let cost_of = |x: i32, y: i32, sad: u32| -> u32 { sad + lam * (se_bits(x * 4 - mvp[0] as i32) + se_bits(y * 4 - mvp[1] as i32)) };
        let ok = |x: i32, y: i32| x >= xmin && x <= xmax && y >= ymin && y <= ymax;

        let mut cands = [[0i32; 2]; 12];
        let mut nc = 0;
        let mut push = |c: [i32; 2], cands: &mut [[i32; 2]; 12]| {
            if nc < 12 && ok(c[0], c[1]) && !cands[..nc].contains(&c) {
                cands[nc] = c;
                nc += 1;
            }
        };
        let q = |v: [i16; 2]| [(v[0] as i32) >> 2, (v[1] as i32) >> 2];
        push([0, 0], &mut cands);
        push(q(mvp), &mut cands);
        let mi = self.mi(mx, my);
        push(q(self.pmv[mi]), &mut cands);
        for n in self.neighbours(mx, my) {
            if n.1 == 0 {
                push(q(n.2), &mut cands);
            }
        }
        for h in 0..self.nh {
            push(self.hints[h], &mut cands);
        }
        let mut best = [0i32; 2];
        let mut bsad = u32::MAX;
        let mut bcost = u32::MAX;
        for c in &cands[..nc] {
            let s = sad_at(c[0], c[1]);
            let k = cost_of(c[0], c[1], s);
            if k < bcost {
                bcost = k;
                bsad = s;
                best = *c;
            }
        }
        // diamond refinement: large steps, then small
        let refine = |best: &mut [i32; 2], bsad: &mut u32, bcost: &mut u32| {
            for pat in [&[[2, 0], [-2, 0], [0, 2], [0, -2], [1, 1], [1, -1], [-1, 1], [-1, -1]][..], &[[1, 0], [-1, 0], [0, 1], [0, -1]][..]] {
                let mut iters = 0;
                loop {
                    if *bsad == 0 || iters >= 24 {
                        break;
                    }
                    let c0 = *best;
                    for d in pat {
                        let (x, y) = (c0[0] + d[0], c0[1] + d[1]);
                        if !ok(x, y) {
                            continue;
                        }
                        let s = sad_at(x, y);
                        let k = cost_of(x, y, s);
                        if k < *bcost {
                            *bcost = k;
                            *bsad = s;
                            *best = [x, y];
                        }
                    }
                    if *best == c0 {
                        break;
                    }
                    iters += 1;
                }
            }
        };
        refine(&mut best, &mut bsad, &mut bcost);
        // scrolling / dragging: search a vertical and a horizontal line
        if bsad > 256 * 3 && self.line_fail < 6 {
            let before = bsad;
            for y in (-LINE_Y).max(ymin)..=LINE_Y.min(ymax) {
                let s = sad_at(0, y);
                let k = cost_of(0, y, s);
                if k < bcost {
                    bcost = k;
                    bsad = s;
                    best = [0, y];
                }
            }
            for x in (-LINE_X).max(xmin)..=LINE_X.min(xmax) {
                let s = sad_at(x, 0);
                let k = cost_of(x, 0, s);
                if k < bcost {
                    bcost = k;
                    bsad = s;
                    best = [x, 0];
                }
            }
            refine(&mut best, &mut bsad, &mut bcost);
            if bsad < before / 4 {
                self.line_fail = 0;
            } else {
                self.line_fail += 1;
            }
        }
        if bsad < 256 * 2 && best != [0, 0] && !self.hints[..self.nh].contains(&best) {
            // newest first
            let n = self.nh.min(3);
            self.hints.copy_within(0..n, 1);
            self.hints[0] = best;
            self.nh = (self.nh + 1).min(4);
        }
        (best, bsad)
    }

    // ---------------------------------------------------------------- prediction

    fn pred_luma(&self, mx: usize, my: usize, mv: [i16; 2], out: &mut [u8; 256]) {
        let x = (mx * 16) as isize + (mv[0] >> 2) as isize;
        let y = (my * 16) as isize + (mv[1] >> 2) as isize;
        let o = y as usize * self.ys + x as usize;
        for r in 0..16 {
            out[r * 16..r * 16 + 16].copy_from_slice(&self.fy[o + r * self.ys..o + r * self.ys + 16]);
        }
    }

    fn pred_chroma(&self, plane: &[u8], mx: usize, my: usize, mv: [i16; 2], out: &mut [u8; 64]) {
        let (mvx, mvy) = (mv[0] as i32, mv[1] as i32);
        let (fx, fy) = (mvx & 7, mvy & 7);
        let bx = (mx * 8) as i32 + (mvx >> 3);
        let by = (my * 8) as i32 + (mvy >> 3);
        let cw = (self.mbw * 8) as i32;
        let ch = (self.mbh * 8) as i32;
        let cs = self.cs;
        if fx == 0 && fy == 0 && bx >= 0 && by >= 0 && bx + 8 <= cw && by + 8 <= ch {
            let o = by as usize * cs + bx as usize;
            for r in 0..8 {
                out[r * 8..r * 8 + 8].copy_from_slice(&plane[o + r * cs..o + r * cs + 8]);
            }
            return;
        }
        let (wa, wb, wc, wd) = ((8 - fx) * (8 - fy), fx * (8 - fy), (8 - fx) * fy, fx * fy);
        for y in 0..8 {
            let y0 = (by + y).clamp(0, ch - 1) as usize * cs;
            let y1 = (by + y + 1).clamp(0, ch - 1) as usize * cs;
            for x in 0..8 {
                let x0 = (bx + x).clamp(0, cw - 1) as usize;
                let x1 = (bx + x + 1).clamp(0, cw - 1) as usize;
                let v = wa * plane[y0 + x0] as i32 + wb * plane[y0 + x1] as i32 + wc * plane[y1 + x0] as i32 + wd * plane[y1 + x1] as i32;
                out[(y * 8 + x) as usize] = ((v + 32) >> 6) as u8;
            }
        }
    }

    // ---------------------------------------------------------------- inter macroblocks

    fn skip(&mut self, mx: usize, my: usize, smv: [i16; 2]) {
        let mut pl = [0u8; 256];
        let mut pu = [0u8; 64];
        let mut pv = [0u8; 64];
        self.pred_luma(mx, my, smv, &mut pl);
        self.pred_chroma(self.fu, mx, my, smv, &mut pu);
        self.pred_chroma(self.fv, mx, my, smv, &mut pv);
        self.put_pred(mx, my, &pl, &pu, &pv);
        let mi = self.mi(mx, my);
        self.kind[mi] = K_INTER;
        self.mv[mi] = smv;
        self.nnz[mi] = [0; 16];
        self.nnzc[mi] = [0; 8];
        self.skip_run += 1;
        self.st.skip += 1;
    }

    fn put_pred(&mut self, mx: usize, my: usize, pl: &[u8; 256], pu: &[u8; 64], pv: &[u8; 64]) {
        let lo = self.lo(mx, my);
        for r in 0..16 {
            let o = lo + r * self.ys;
            self.ry[o..o + 16].copy_from_slice(&pl[r * 16..r * 16 + 16]);
        }
        let co = self.co(mx, my);
        for r in 0..8 {
            let o = co + r * self.cs;
            self.ru[o..o + 8].copy_from_slice(&pu[r * 8..r * 8 + 8]);
            self.rv[o..o + 8].copy_from_slice(&pv[r * 8..r * 8 + 8]);
        }
    }

    fn code_inter(&mut self, mx: usize, my: usize, mv: [i16; 2], smv: [i16; 2], mvp: [i16; 2]) {
        let mi = self.mi(mx, my);
        let mut pl = [0u8; 256];
        let mut pu = [0u8; 64];
        let mut pv = [0u8; 64];
        self.pred_luma(mx, my, mv, &mut pl);
        self.pred_chroma(self.fu, mx, my, mv, &mut pu);
        self.pred_chroma(self.fv, mx, my, mv, &mut pv);
        let lo = self.lo(mx, my);
        let qp = self.qp;
        let mut lv = [[0i32; 16]; 16];
        let mut cnt = [0u8; 16];
        let mut score = [0u32; 4];
        for blk in 0..16 {
            let (bx, by) = (BLK_X[blk] as usize * 4, BLK_Y[blk] as usize * 4);
            let mut d = [0i32; 16];
            for y in 0..4 {
                let so = lo + (by + y) * self.ys + bx;
                for x in 0..4 {
                    d[y * 4 + x] = self.sy[so + x] as i32 - pl[(by + y) * 16 + bx + x] as i32;
                }
            }
            let w = fdct4(&d);
            cnt[blk] = quant4(&w, &mut lv[blk], qp, false, 0) as u8;
            if cnt[blk] > 0 {
                let mut s = [0i32; 16];
                for k in 0..16 {
                    s[k] = lv[blk][ZIGZAG4[k] as usize];
                }
                score[blk / 4] += decimate_score(&s);
            }
        }
        let total: u32 = score.iter().sum();
        let mut cbp = 0u32;
        for b8 in 0..4 {
            if total < 6 || score[b8] < 4 {
                for i in 0..4 {
                    lv[b8 * 4 + i] = [0; 16];
                    cnt[b8 * 4 + i] = 0;
                }
            }
            if (0..4).any(|i| cnt[b8 * 4 + i] > 0) {
                cbp |= 1 << b8;
            }
        }
        let mut c = self.chroma_resid(mx, my, &pu, &pv, false);
        // drop weak chroma AC (inter): cheaper than coding it
        let mut cs = 0;
        for k in 0..2 {
            for b in 0..4 {
                if c.nac[k][b] > 0 {
                    let mut s = [0i32; 15];
                    for i in 0..15 {
                        s[i] = c.ac[k][b][ZIGZAG4[i + 1] as usize];
                    }
                    cs += decimate_score(&s);
                }
            }
        }
        if cs < 7 {
            c.ac = [[[0; 16]; 4]; 2];
            c.nac = [[0; 4]; 2];
        }
        let cbpc = Self::cbpc(&c);
        let cbp = cbp | (cbpc << 4);
        if cbp == 0 && mv == smv {
            self.put_pred(mx, my, &pl, &pu, &pv);
            self.kind[mi] = K_INTER;
            self.mv[mi] = mv;
            self.nnz[mi] = [0; 16];
            self.nnzc[mi] = [0; 8];
            self.cqp[mi] = qp as u8;
            self.skip_run += 1;
            self.st.skip += 1;
            return;
        }
        // reconstruct
        self.put_pred(mx, my, &pl, &pu, &pv);
        for blk in 0..16 {
            if cnt[blk] > 0 {
                let (bx, by) = (BLK_X[blk] as usize * 4, BLK_Y[blk] as usize * 4);
                let mut coef = [0i32; 16];
                for p in 0..16 {
                    coef[p] = dequant(lv[blk][p], qp, p);
                }
                idct::idct4_add(&mut coef, self.ry, lo + by * self.ys + bx, self.ys);
            }
        }
        self.chroma_recon(mx, my, &c, cbpc);
        // bookkeeping before the residual (nC reads it)
        self.kind[mi] = K_INTER;
        self.mv[mi] = mv;
        self.cqp[mi] = qp as u8;
        let mut nz = [0u8; 16];
        for blk in 0..16 {
            nz[(BLK_Y[blk] * 4 + BLK_X[blk]) as usize] = cnt[blk];
        }
        self.nnz[mi] = nz;
        self.set_nnzc(mi, &c, cbpc);
        // syntax
        self.bw.ue(self.skip_run);
        self.skip_run = 0;
        self.bw.ue(0); // P_L0_16x16
        self.bw.se(mv[0] as i32 - mvp[0] as i32);
        self.bw.se(mv[1] as i32 - mvp[1] as i32);
        self.bw.ue(self.e.cbp_code[1][cbp as usize] as u32);
        if cbp != 0 {
            self.bw.se(0); // mb_qp_delta
            self.write_luma(mi, mx, my, &lv, cbp & 15);
            self.write_chroma(mi, mx, my, &c, cbpc);
        }
        self.st.inter += 1;
    }

    // ---------------------------------------------------------------- chroma

    fn cbpc(c: &ChromaLv) -> u32 {
        if c.nac.iter().any(|a| a.iter().any(|&n| n > 0)) {
            2
        } else if c.dc.iter().any(|d| d.iter().any(|&v| v != 0)) {
            1
        } else {
            0
        }
    }

    fn chroma_resid(&self, mx: usize, my: usize, pu: &[u8; 64], pv: &[u8; 64], intra: bool) -> ChromaLv {
        let co = self.co(mx, my);
        let qpc = self.qpc;
        let mut c = ChromaLv { dc: [[0; 4]; 2], ac: [[[0; 16]; 4]; 2], nac: [[0; 4]; 2] };
        for k in 0..2 {
            let (src, pred) = if k == 0 { (self.su, pu) } else { (self.sv, pv) };
            let mut dcs = [0i32; 4];
            for b in 0..4 {
                let (bx, by) = ((b & 1) * 4, (b >> 1) * 4);
                let mut d = [0i32; 16];
                for y in 0..4 {
                    let so = co + (by + y) * self.cs + bx;
                    for x in 0..4 {
                        d[y * 4 + x] = src[so + x] as i32 - pred[(by + y) * 8 + bx + x] as i32;
                    }
                }
                let w = fdct4(&d);
                dcs[b] = w[0];
                c.nac[k][b] = quant4(&w, &mut c.ac[k][b], qpc, intra, 1) as u8;
            }
            let t = chroma_dc_fwd(&dcs);
            for b in 0..4 {
                c.dc[k][b] = quant_dc(t[b], qpc, intra);
            }
        }
        c
    }

    /// Prediction must already be in the recon planes.
    fn chroma_recon(&mut self, mx: usize, my: usize, c: &ChromaLv, cbpc: u32) {
        if cbpc == 0 {
            return;
        }
        let co = self.co(mx, my);
        let qpc = self.qpc;
        let cs = self.cs;
        for k in 0..2 {
            let dc = idct::chroma_dc(&c.dc[k], dc_scale(qpc));
            let plane: &mut [u8] = if k == 0 { &mut *self.ru } else { &mut *self.rv };
            for b in 0..4 {
                let mut coef = [0i32; 16];
                coef[0] = dc[b];
                let mut any = dc[b] != 0;
                if cbpc == 2 {
                    for p in 1..16 {
                        coef[p] = dequant(c.ac[k][b][p], qpc, p);
                        any |= coef[p] != 0;
                    }
                }
                if any {
                    let (bx, by) = ((b & 1) * 4, (b >> 1) * 4);
                    idct::idct4_add(&mut coef, plane, co + by * cs + bx, cs);
                }
            }
        }
    }

    fn set_nnzc(&mut self, mi: usize, c: &ChromaLv, cbpc: u32) {
        let mut n = [0u8; 8];
        if cbpc == 2 {
            for k in 0..2 {
                for b in 0..4 {
                    n[k * 4 + b] = c.nac[k][b];
                }
            }
        }
        self.nnzc[mi] = n;
    }

    fn nc_luma(&self, mi: usize, mx: usize, my: usize, r: usize) -> i32 {
        let (x4, y4) = (r & 3, r >> 2);
        let a = if x4 > 0 {
            Some(self.nnz[mi][r - 1])
        } else if mx > 0 {
            Some(self.nnz[mi - 1][r + 3])
        } else {
            None
        };
        let b = if y4 > 0 {
            Some(self.nnz[mi][r - 4])
        } else if my > self.r0 {
            Some(self.nnz[mi - self.mbw][r + 12])
        } else {
            None
        };
        match (a, b) {
            (Some(a), Some(b)) => (a as i32 + b as i32 + 1) >> 1,
            (Some(a), None) => a as i32,
            (None, Some(b)) => b as i32,
            _ => 0,
        }
    }

    fn nc_chroma(&self, mi: usize, mx: usize, my: usize, k: usize, b: usize) -> i32 {
        let (bx, by) = (b & 1, b >> 1);
        let a = if bx > 0 {
            Some(self.nnzc[mi][k * 4 + by * 2])
        } else if mx > 0 {
            Some(self.nnzc[mi - 1][k * 4 + by * 2 + 1])
        } else {
            None
        };
        let t = if by > 0 {
            Some(self.nnzc[mi][k * 4 + bx])
        } else if my > self.r0 {
            Some(self.nnzc[mi - self.mbw][k * 4 + 2 + bx])
        } else {
            None
        };
        match (a, t) {
            (Some(a), Some(b)) => (a as i32 + b as i32 + 1) >> 1,
            (Some(a), None) => a as i32,
            (None, Some(b)) => b as i32,
            _ => 0,
        }
    }

    fn write_luma(&mut self, mi: usize, mx: usize, my: usize, lv: &[[i32; 16]; 16], cbp: u32) {
        for b8 in 0..4 {
            if cbp & (1 << b8) == 0 {
                continue;
            }
            for i in 0..4 {
                let blk = b8 * 4 + i;
                let mut s = [0i32; 16];
                for k in 0..16 {
                    s[k] = lv[blk][ZIGZAG4[k] as usize];
                }
                let nc = self.nc_luma(mi, mx, my, (BLK_Y[blk] * 4 + BLK_X[blk]) as usize);
                write_block(&mut self.bw, &s, nc);
            }
        }
    }

    fn write_chroma(&mut self, mi: usize, mx: usize, my: usize, c: &ChromaLv, cbpc: u32) {
        if cbpc == 0 {
            return;
        }
        for k in 0..2 {
            write_block(&mut self.bw, &c.dc[k], -1);
        }
        if cbpc == 2 {
            for k in 0..2 {
                for b in 0..4 {
                    let mut s = [0i32; 15];
                    for i in 0..15 {
                        s[i] = c.ac[k][b][ZIGZAG4[i + 1] as usize];
                    }
                    let nc = self.nc_chroma(mi, mx, my, k, b);
                    write_block(&mut self.bw, &s, nc);
                }
            }
        }
    }

    // ---------------------------------------------------------------- intra

    /// Predicted Intra 4x4 mode of the block at (x4, y4); `cur` = this macroblock's modes so far (raster).
    fn pred_mode(&self, mi: usize, mx: usize, my: usize, x4: usize, y4: usize, cur: &[u8; 16]) -> u8 {
        let a = if x4 > 0 {
            Some(cur[y4 * 4 + x4 - 1])
        } else if mx > 0 {
            let n = mi - 1;
            Some(if self.kind[n] == K_I4 { self.i4[n][y4 * 4 + 3] } else { 2 })
        } else {
            None
        };
        let b = if y4 > 0 {
            Some(cur[(y4 - 1) * 4 + x4])
        } else if my > self.r0 {
            let n = mi - self.mbw;
            Some(if self.kind[n] == K_I4 { self.i4[n][12 + x4] } else { 2 })
        } else {
            None
        };
        match (a, b) {
            (Some(a), Some(b)) => a.min(b),
            _ => 2,
        }
    }

    /// Choose Intra 16x16 or 4x4 for the luma (the 4x4 search reconstructs
    /// into the recon plane as it goes). `bound`: give up on 4x4 above this cost.
    fn intra_luma(&mut self, mx: usize, my: usize, bound: u32) -> Intra {
        let mi = self.mi(mx, my);
        let lo = self.lo(mx, my);
        let ys = self.ys;
        let lam = self.lam;
        let qp = self.qp;
        let has_left = mx > 0;
        let has_top = my > self.r0;
        let mut res = Intra { i4: false, mode16: 2, modes: [2; 16], pm: [2; 16], lv: [[0; 16]; 16], cnt: [0; 16], dc: [0; 16], cost: u32::MAX };

        // Intra 16x16 candidates
        let (t16, l16) = self.edges16(lo, has_top, has_left);
        let mut p16 = [0u8; 256];
        let mut c16 = u32::MAX;
        for mode in 0..4u8 {
            let ok = match mode {
                0 => has_top,
                1 => has_left,
                2 => true,
                _ => has_top && has_left,
            };
            if !ok {
                continue;
            }
            pred16(mode, &t16, &l16, has_top, has_left, &mut p16);
            let c = satd(self.sy, lo, ys, &p16, 0, 16, 16, 16) + lam * 4;
            if c < c16 {
                c16 = c;
                res.mode16 = mode;
            }
        }

        // Intra 4x4, block by block on the reconstruction
        let mut cost4 = lam * 24;
        let limit = c16.min(bound);
        let mut cur = [2u8; 16];
        let mut ok4 = true;
        for blk in 0..16 {
            let (x4, y4) = (BLK_X[blk] as usize, BLK_Y[blk] as usize);
            let (bx, by) = (x4 * 4, y4 * 4);
            let o = lo + by * ys + bx;
            let hl = bx > 0 || has_left;
            let ht = by > 0 || has_top;
            let htl = match (bx > 0, by > 0) {
                (true, true) => true,
                (false, true) => has_left,
                (true, false) => has_top,
                (false, false) => has_left && has_top,
            };
            let htr = if by == 0 {
                if bx + 4 < 16 {
                    has_top
                } else {
                    has_top && mx + 1 < self.mbw
                }
            } else if bx + 4 >= 16 {
                false
            } else {
                (BLK_IDX[(y4 - 1) * 4 + x4 + 1] as usize) < blk
            };
            let mut top = [0i32; 17];
            let mut left = [0i32; 9];
            if htl {
                top[0] = self.ry[o - ys - 1] as i32;
            }
            if ht {
                for x in 0..4 {
                    top[1 + x] = self.ry[o - ys + x] as i32;
                }
                for x in 0..4 {
                    top[5 + x] = if htr { self.ry[o - ys + 4 + x] as i32 } else { top[4] };
                }
            }
            left[0] = top[0];
            if hl {
                for y in 0..4 {
                    left[1 + y] = self.ry[o + y * ys - 1] as i32;
                }
            }
            let pm = self.pred_mode(mi, mx, my, x4, y4, &cur);
            let mut p4 = [0u8; 16];
            let mut best = (u32::MAX, 2u8);
            for mode in 0..9u8 {
                let ok = match mode {
                    0 | 3 | 7 => ht,
                    1 | 8 => hl,
                    2 => true,
                    _ => ht && hl && htl,
                };
                if !ok {
                    continue;
                }
                pred4(mode, &top, &left, ht, hl, &mut p4);
                let c = satd(self.sy, o, ys, &p4, 0, 4, 4, 4) + lam * if mode == pm { 1 } else { 4 };
                if c < best.0 {
                    best = (c, mode);
                }
            }
            let mode = best.1;
            cost4 += best.0;
            pred4(mode, &top, &left, ht, hl, &mut p4);
            let mut d = [0i32; 16];
            for y in 0..4 {
                for x in 0..4 {
                    d[y * 4 + x] = self.sy[o + y * ys + x] as i32 - p4[y * 4 + x] as i32;
                    self.ry[o + y * ys + x] = p4[y * 4 + x];
                }
            }
            let w = fdct4(&d);
            res.cnt[blk] = quant4(&w, &mut res.lv[blk], qp, true, 0) as u8;
            if res.cnt[blk] > 0 {
                let mut coef = [0i32; 16];
                for p in 0..16 {
                    coef[p] = dequant(res.lv[blk][p], qp, p);
                }
                idct::idct4_add(&mut coef, self.ry, o, ys);
            }
            res.modes[blk] = mode;
            res.pm[blk] = pm;
            cur[y4 * 4 + x4] = mode;
            if cost4 >= limit {
                ok4 = false;
                break;
            }
        }
        if ok4 && cost4 < c16 {
            res.i4 = true;
            res.cost = cost4;
            return res;
        }
        // Intra 16x16 it is: code it on the reconstruction
        res.i4 = false;
        res.cost = c16;
        res.lv = [[0; 16]; 16];
        res.cnt = [0; 16];
        pred16(res.mode16, &t16, &l16, has_top, has_left, &mut p16);
        let mut dcs = [0i32; 16];
        for blk in 0..16 {
            let (bx, by) = (BLK_X[blk] as usize * 4, BLK_Y[blk] as usize * 4);
            let mut d = [0i32; 16];
            for y in 0..4 {
                for x in 0..4 {
                    d[y * 4 + x] = self.sy[lo + (by + y) * ys + bx + x] as i32 - p16[(by + y) * 16 + bx + x] as i32;
                }
            }
            let w = fdct4(&d);
            dcs[(by / 4) * 4 + bx / 4] = w[0];
            res.cnt[blk] = quant4(&w, &mut res.lv[blk], qp, true, 1) as u8;
        }
        let t = luma_dc_fwd(&dcs);
        for r in 0..16 {
            res.dc[r] = quant_dc(t[r], qp, true);
        }
        let ac = res.cnt.iter().any(|&n| n > 0);
        let dcd = idct::luma_dc(&res.dc, dc_scale(qp));
        for r in 0..16 {
            let o = lo + r * ys;
            self.ry[o..o + 16].copy_from_slice(&p16[r * 16..r * 16 + 16]);
        }
        for blk in 0..16 {
            let (bx, by) = (BLK_X[blk] as usize * 4, BLK_Y[blk] as usize * 4);
            let r = (by / 4) * 4 + bx / 4;
            let mut coef = [0i32; 16];
            coef[0] = dcd[r];
            let mut any = dcd[r] != 0;
            if ac {
                for p in 1..16 {
                    coef[p] = dequant(res.lv[blk][p], qp, p);
                    any |= coef[p] != 0;
                }
            }
            if any {
                idct::idct4_add(&mut coef, self.ry, lo + by * ys + bx, ys);
            }
        }
        res
    }

    fn edges16(&self, lo: usize, has_top: bool, has_left: bool) -> ([i32; 17], [i32; 17]) {
        let ys = self.ys;
        let mut t = [0i32; 17];
        let mut l = [0i32; 17];
        if has_top && has_left {
            t[0] = self.ry[lo - ys - 1] as i32;
        }
        l[0] = t[0];
        if has_top {
            for x in 0..16 {
                t[1 + x] = self.ry[lo - ys + x] as i32;
            }
        }
        if has_left {
            for y in 0..16 {
                l[1 + y] = self.ry[lo + y * ys - 1] as i32;
            }
        }
        (t, l)
    }

    /// Chroma intra mode, coded on the reconstruction; returns (mode, levels, cbpc).
    fn intra_chroma(&mut self, mx: usize, my: usize) -> (u8, ChromaLv, u32) {
        let co = self.co(mx, my);
        let cs = self.cs;
        let has_left = mx > 0;
        let has_top = my > self.r0;
        let edges = |p: &[u8]| {
            let mut t = [0i32; 9];
            let mut l = [0i32; 9];
            if has_top && has_left {
                t[0] = p[co - cs - 1] as i32;
            }
            l[0] = t[0];
            if has_top {
                for x in 0..8 {
                    t[1 + x] = p[co - cs + x] as i32;
                }
            }
            if has_left {
                for y in 0..8 {
                    l[1 + y] = p[co + y * cs - 1] as i32;
                }
            }
            (t, l)
        };
        let (tu, lu) = edges(&*self.ru);
        let (tv, lv) = edges(&*self.rv);
        let mut pu = [0u8; 64];
        let mut pv = [0u8; 64];
        let mut best = (u32::MAX, 0u8);
        for mode in 0..4u8 {
            let ok = match mode {
                0 => true,
                1 => has_left,
                2 => has_top,
                _ => has_top && has_left,
            };
            if !ok {
                continue;
            }
            intra::pred_chroma(mode as u32, &tu, &lu, has_top, has_left, &mut pu, 0, 8);
            intra::pred_chroma(mode as u32, &tv, &lv, has_top, has_left, &mut pv, 0, 8);
            let c = satd(self.su, co, cs, &pu, 0, 8, 8, 8) + satd(self.sv, co, cs, &pv, 0, 8, 8, 8) + self.lam * if mode == 0 { 1 } else { 3 };
            if c < best.0 {
                best = (c, mode);
            }
        }
        let mode = best.1;
        intra::pred_chroma(mode as u32, &tu, &lu, has_top, has_left, &mut pu, 0, 8);
        intra::pred_chroma(mode as u32, &tv, &lv, has_top, has_left, &mut pv, 0, 8);
        let c = self.chroma_resid(mx, my, &pu, &pv, true);
        for r in 0..8 {
            let o = co + r * cs;
            self.ru[o..o + 8].copy_from_slice(&pu[r * 8..r * 8 + 8]);
            self.rv[o..o + 8].copy_from_slice(&pv[r * 8..r * 8 + 8]);
        }
        let cbpc = Self::cbpc(&c);
        self.chroma_recon(mx, my, &c, cbpc);
        (mode, c, cbpc)
    }

    fn write_intra(&mut self, mx: usize, my: usize, c: &Intra) {
        let mi = self.mi(mx, my);
        let (cmode, cl, cbpc) = self.intra_chroma(mx, my);
        let ac = c.cnt.iter().any(|&n| n > 0);
        // bookkeeping
        self.kind[mi] = if c.i4 { K_I4 } else { K_I16 };
        self.mv[mi] = [0, 0];
        self.cqp[mi] = self.qp as u8;
        let mut nz = [0u8; 16];
        let mut modes = [2u8; 16];
        for blk in 0..16 {
            let r = (BLK_Y[blk] * 4 + BLK_X[blk]) as usize;
            nz[r] = if c.i4 || ac { c.cnt[blk] } else { 0 };
            modes[r] = c.modes[blk];
        }
        self.nnz[mi] = nz;
        self.i4[mi] = modes;
        self.set_nnzc(mi, &cl, cbpc);
        let off = if self.cur.key { 0 } else { 5 };
        if !self.cur.key {
            self.bw.ue(self.skip_run);
            self.skip_run = 0;
        }
        if c.i4 {
            let mut cbp = 0u32;
            for b8 in 0..4 {
                if (0..4).any(|i| c.cnt[b8 * 4 + i] > 0) {
                    cbp |= 1 << b8;
                }
            }
            cbp |= cbpc << 4;
            self.bw.ue(off); // I_NxN
            for blk in 0..16 {
                let (m, pm) = (c.modes[blk], c.pm[blk]);
                if m == pm {
                    self.bw.flag(true);
                } else {
                    self.bw.flag(false);
                    self.bw.put(if m < pm { m } else { m - 1 } as u32, 3);
                }
            }
            self.bw.ue(cmode as u32);
            self.bw.ue(self.e.cbp_code[0][cbp as usize] as u32);
            if cbp != 0 {
                self.bw.se(0);
                self.write_luma(mi, mx, my, &c.lv, cbp & 15);
                self.write_chroma(mi, mx, my, &cl, cbpc);
            }
            self.st.i4 += 1;
        } else {
            let t = 1 + c.mode16 as u32 + 4 * cbpc + if ac { 12 } else { 0 };
            self.bw.ue(off + t);
            self.bw.ue(cmode as u32);
            self.bw.se(0);
            let mut s = [0i32; 16];
            for k in 0..16 {
                s[k] = c.dc[ZIGZAG4[k] as usize];
            }
            let nc = self.nc_luma(mi, mx, my, 0);
            write_block(&mut self.bw, &s, nc);
            if ac {
                for blk in 0..16 {
                    let mut s = [0i32; 15];
                    for k in 0..15 {
                        s[k] = c.lv[blk][ZIGZAG4[k + 1] as usize];
                    }
                    let nc = self.nc_luma(mi, mx, my, (BLK_Y[blk] * 4 + BLK_X[blk]) as usize);
                    write_block(&mut self.bw, &s, nc);
                }
            }
            self.write_chroma(mi, mx, my, &cl, cbpc);
            self.st.i16 += 1;
        }
    }
}

fn pred16(mode: u8, t: &[i32; 17], l: &[i32; 17], has_top: bool, has_left: bool, out: &mut [u8; 256]) {
    if mode == 3 {
        intra::pred16_plane(t, l, out, 0, 16);
    } else {
        intra::pred_vh_dc(16, mode as u32, &t[1..], &l[1..], has_top, has_left, out, 0, 16);
    }
}

fn pred4(mode: u8, top: &[i32; 17], left: &[i32; 9], ht: bool, hl: bool, out: &mut [u8; 16]) {
    if mode < 3 {
        intra::pred_vh_dc(4, mode as u32, &top[1..], &left[1..], ht, hl, out, 0, 4);
    } else {
        intra::pred_dir(4, mode as u32, top, left, out, 0, 4);
    }
}
