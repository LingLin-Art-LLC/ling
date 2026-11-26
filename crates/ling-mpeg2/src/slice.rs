//! Slice and macroblock layer: VLC parsing, dequantisation, motion vector decoding,
//! prediction and reconstruction. Every error returns `Err(())` up to the slice loop, which
//! abandons the slice; the caller resumes at the next slice start code.

use crate::bits::Bits;
use crate::idct::{idct_add, idct_put};
use crate::mc::{mc, View};
use crate::tables::*;
use crate::{Decoder, PicBuf, PIC_B, PIC_I, PIC_P, PS_FRAME};

type R<T> = Result<T, ()>;

const MT_FIELD: u32 = 1;
const MT_FRAME: u32 = 2; // frame pictures: frame; field pictures: 16x8
const MT_DMV: u32 = 3;

/// Offsets of the pred buffer planes.
const PY: usize = 0;
const PCB: usize = 256;
const PCR: usize = 320;

/// Motion data of one macroblock.
#[derive(Clone, Copy, Default)]
struct Motion {
    /// mv[r][s] = (x, y), half-pel; field vectors are in field units.
    mv: [[[i32; 2]; 2]; 2],
    /// field_select[r][s]
    fsel: [[u8; 2]; 2],
    dmv: [i32; 2],
    mtype: u32,
    dirs: u8,
}

#[inline(always)]
fn lookup(bs: &Bits, t: &[u16], bits: u32) -> R<(u32, u32)> {
    let e = t[bs.peek(bits) as usize];
    let len = (e & 0xff) as u32;
    if len == 0 {
        return Err(());
    }
    Ok(((e >> 8) as u32, len))
}

/// Luma / chroma views of a picture: whole frame or one field.
#[inline(always)]
fn views(p: &PicBuf, field: Option<u8>) -> (View, View) {
    match field {
        None => (
            View { off: 0, stride: p.ys, w: p.ys as i32, h: p.yh as i32 },
            View { off: 0, stride: p.cs, w: p.cs as i32, h: p.ch as i32 },
        ),
        Some(f) => (
            View { off: f as usize * p.ys, stride: p.ys * 2, w: p.ys as i32, h: (p.yh / 2) as i32 },
            View { off: f as usize * p.cs, stride: p.cs * 2, w: p.cs as i32, h: (p.ch / 2) as i32 },
        ),
    }
}

/// Predict one part of a macroblock (luma `16 x h` at (x, y), chroma half of that).
/// `lo`/`ls`: luma offset / stride inside the pred buffer; `co`/`cs` the same for chroma.
#[allow(clippy::too_many_arguments)]
#[inline]
fn part(
    pred: &mut [u8; 384],
    p: &PicBuf,
    field: Option<u8>,
    x: i32,
    y: i32,
    h: usize,
    mv: [i32; 2],
    lo: usize,
    ls: usize,
    co: usize,
    cs: usize,
    avg: bool,
) {
    let (lv, cv) = views(p, field);
    mc::<16>(&p.y, lv, x, y, mv[0], mv[1], h, pred, PY + lo, ls, avg);
    let (cmx, cmy) = (mv[0] / 2, mv[1] / 2);
    mc::<8>(&p.cb, cv, x >> 1, y >> 1, cmx, cmy, h >> 1, pred, PCB + co, cs, avg);
    mc::<8>(&p.cr, cv, x >> 1, y >> 1, cmx, cmy, h >> 1, pred, PCR + co, cs, avg);
}

impl Decoder {
    #[inline(always)]
    fn qscale(&self) -> i32 {
        if self.hdr.q_scale_type {
            NONLINEAR_QSCALE[self.qcode as usize & 31] as i32
        } else {
            (self.qcode as i32) * 2
        }
    }

    fn reset_dc(&mut self) {
        let v = if self.seq.mpeg2 { 1 << (7 + self.hdr.dc_prec) } else { 128 };
        self.dc_pred = [v; 3];
    }

    fn read_mba(&self, bs: &mut Bits) -> R<usize> {
        let mut inc = 0usize;
        loop {
            let (v, len) = lookup(bs, &MBA, MBA_BITS)?;
            bs.skip(len);
            if bs.overrun() || inc > 4096 {
                return Err(());
            }
            match v as u16 {
                MBA_STUFFING => continue,
                MBA_ESCAPE => inc += 33,
                v => return Ok(inc + v as usize),
            }
        }
    }

    pub(crate) fn decode_slice(&mut self, bs: &mut Bits, code: u32) -> R<()> {
        let h = self.hdr;
        let is_field = h.structure != PS_FRAME;
        let mbw = self.mbw;
        let mbh_pic = if is_field { self.mbh / 2 } else { self.mbh };
        let mut mb_row = (code - 1) as usize;
        if self.seq.mpeg2 && self.seq.height > 2800 {
            mb_row += (bs.get(3) as usize) << 7;
        }
        if mb_row >= mbh_pic || mbw == 0 {
            return Err(());
        }
        let q = bs.get(5);
        if q == 0 {
            return Err(());
        }
        self.qcode = q;
        if self.seq.mpeg2 && bs.peek(1) == 1 {
            bs.skip(9);
        }
        while bs.get1() {
            bs.skip(8);
            if bs.overrun() {
                return Err(());
            }
        }
        self.reset_dc();
        self.pmv = [[[0; 2]; 2]; 2];
        self.prev_dirs = 0;
        let total = mbw * mbh_pic;
        let inc = self.read_mba(bs)?;
        let mut addr = mb_row * mbw + inc - 1;
        loop {
            if addr >= total {
                return Err(());
            }
            if self.decode_mb(bs, addr % mbw, addr / mbw).is_err() {
                // Leave no half-parsed coefficients behind for the next slice.
                self.blk = [[0; 64]; 6];
                return Err(());
            }
            addr += 1;
            if bs.overrun() {
                return Err(());
            }
            if bs.peek(23) == 0 {
                return Ok(());
            }
            let inc = self.read_mba(bs)?;
            for _ in 1..inc {
                if addr >= total || h.coding_type == PIC_I {
                    return Err(());
                }
                self.skip_mb(addr % mbw, addr / mbw)?;
                addr += 1;
            }
        }
    }

    fn decode_motion(&self, bs: &mut Bits, fcode: u8, pred: i32) -> R<i32> {
        if fcode == 0 || fcode > 9 {
            return Err(());
        }
        let (code, len) = lookup(bs, &MOTION, MOTION_BITS)?;
        bs.skip(len);
        if code == 0 {
            return Ok(pred);
        }
        let neg = bs.get1();
        let r = fcode as u32 - 1;
        let mut delta = (((code as i32) - 1) << r) + bs.get(r) as i32 + 1;
        if neg {
            delta = -delta;
        }
        let lim = 16i32 << r;
        let mut v = pred + delta;
        if v < -lim {
            v += 2 * lim;
        } else if v >= lim {
            v -= 2 * lim;
        }
        Ok(v)
    }

    #[inline]
    fn get_dmv(bs: &mut Bits) -> i32 {
        if !bs.get1() {
            0
        } else if !bs.get1() {
            1
        } else {
            -1
        }
    }

    /// motion_vectors(s)
    fn parse_mvs(&mut self, bs: &mut Bits, s: usize, m: &mut Motion) -> R<()> {
        let h = self.hdr;
        let fc = h.f_code[s];
        let frame = h.structure == PS_FRAME;
        match (frame, m.mtype) {
            (true, MT_FRAME) => {
                let vx = self.decode_motion(bs, fc[0], self.pmv[0][s][0])?;
                let vy = self.decode_motion(bs, fc[1], self.pmv[0][s][1])?;
                self.pmv[0][s] = [vx, vy];
                self.pmv[1][s] = [vx, vy];
                m.mv[0][s] = if h.full_pel[s] { [vx * 2, vy * 2] } else { [vx, vy] };
            }
            (true, MT_FIELD) => {
                for r in 0..2 {
                    m.fsel[r][s] = bs.get1() as u8;
                    let vx = self.decode_motion(bs, fc[0], self.pmv[r][s][0])?;
                    let vy = self.decode_motion(bs, fc[1], self.pmv[r][s][1] >> 1)?;
                    self.pmv[r][s] = [vx, vy * 2];
                    m.mv[r][s] = [vx, vy];
                }
            }
            (true, MT_DMV) => {
                let vx = self.decode_motion(bs, fc[0], self.pmv[0][s][0])?;
                let dx = Self::get_dmv(bs);
                let vy = self.decode_motion(bs, fc[1], self.pmv[0][s][1] >> 1)?;
                let dy = Self::get_dmv(bs);
                self.pmv[0][s] = [vx, vy * 2];
                self.pmv[1][s] = [vx, vy * 2];
                m.mv[0][s] = [vx, vy];
                m.dmv = [dx, dy];
            }
            (false, MT_FIELD) => {
                m.fsel[0][s] = bs.get1() as u8;
                let vx = self.decode_motion(bs, fc[0], self.pmv[0][s][0])?;
                let vy = self.decode_motion(bs, fc[1], self.pmv[0][s][1])?;
                self.pmv[0][s] = [vx, vy];
                self.pmv[1][s] = [vx, vy];
                m.mv[0][s] = [vx, vy];
            }
            (false, MT_FRAME) => {
                // 16x8
                for r in 0..2 {
                    m.fsel[r][s] = bs.get1() as u8;
                    let vx = self.decode_motion(bs, fc[0], self.pmv[r][s][0])?;
                    let vy = self.decode_motion(bs, fc[1], self.pmv[r][s][1])?;
                    self.pmv[r][s] = [vx, vy];
                    m.mv[r][s] = [vx, vy];
                }
            }
            (false, MT_DMV) => {
                let vx = self.decode_motion(bs, fc[0], self.pmv[0][s][0])?;
                let dx = Self::get_dmv(bs);
                let vy = self.decode_motion(bs, fc[1], self.pmv[0][s][1])?;
                let dy = Self::get_dmv(bs);
                self.pmv[0][s] = [vx, vy];
                self.pmv[1][s] = [vx, vy];
                m.mv[0][s] = [vx, vy];
                m.dmv = [dx, dy];
            }
            _ => return Err(()),
        }
        Ok(())
    }

    fn decode_mb(&mut self, bs: &mut Bits, mbx: usize, mby: usize) -> R<()> {
        let h = self.hdr;
        let frame = h.structure == PS_FRAME;
        let tab: &[u16; 64] = match h.coding_type {
            PIC_I => &MBTYPE_I,
            PIC_P => &MBTYPE_P,
            _ => &MBTYPE_B,
        };
        let (t, len) = lookup(bs, tab, MBTYPE_BITS)?;
        bs.skip(len);
        let mbt = t as u8;
        let intra = mbt & MB_INTRA != 0;
        let pat = mbt & MB_PAT != 0;
        let mut m = Motion { mtype: if frame { MT_FRAME } else { MT_FIELD }, ..Default::default() };
        if mbt & (MB_FWD | MB_BWD) != 0 && (!frame || !h.fpfd) {
            m.mtype = bs.get(2);
            if m.mtype == 0 || (m.mtype == MT_DMV && h.coding_type != PIC_P) {
                return Err(());
            }
        }
        let field_dct = frame && !h.fpfd && (intra || pat) && bs.get1();
        if mbt & MB_QUANT != 0 {
            let q = bs.get(5);
            if q == 0 {
                return Err(());
            }
            self.qcode = q;
        }
        if intra {
            if h.concealment {
                if !frame {
                    bs.skip(1);
                }
                let fc = h.f_code[0];
                let vx = self.decode_motion(bs, fc[0], self.pmv[0][0][0])?;
                let vy = self.decode_motion(bs, fc[1], self.pmv[0][0][1])?;
                self.pmv[0][0] = [vx, vy];
                self.pmv[1][0] = [vx, vy];
                bs.skip(1);
            } else {
                self.pmv = [[[0; 2]; 2]; 2];
            }
            self.prev_dirs = 0;
            for i in 0..6 {
                self.intra_block(bs, i)?;
            }
            for i in 0..6 {
                let (off, stride) = block_pos(i, field_dct);
                idct_put(&mut self.blk[i], self.rowmask[i], &mut self.pred, off, stride);
                self.blk[i] = [0; 64];
            }
        } else {
            self.reset_dc();
            m.dirs = mbt & (MB_FWD | MB_BWD);
            if h.coding_type == PIC_P && mbt & MB_FWD == 0 {
                // No MC: zero vector from the same-parity field / frame.
                self.pmv = [[[0; 2]; 2]; 2];
                m.dirs = MB_FWD;
                m.mtype = if frame { MT_FRAME } else { MT_FIELD };
                m.fsel[0][0] = h.structure.wrapping_sub(1) & 1;
            }
            if mbt & MB_FWD != 0 {
                self.parse_mvs(bs, 0, &mut m)?;
            }
            if mbt & MB_BWD != 0 {
                self.parse_mvs(bs, 1, &mut m)?;
            }
            let cbp = if pat {
                let (c, len) = lookup(bs, &CBP, CBP_BITS)?;
                bs.skip(len);
                c
            } else {
                0
            };
            for i in 0..6 {
                if cbp & (32 >> i) != 0 {
                    self.inter_block(bs, i)?;
                } else {
                    self.rowmask[i] = 0;
                }
            }
            if bs.overrun() {
                // Do not reconstruct garbage; clear any parsed coefficients.
                for b in self.blk.iter_mut() {
                    *b = [0; 64];
                }
                return Err(());
            }
            self.predict(&m, mbx, mby);
            for i in 0..6 {
                if self.rowmask[i] != 0 {
                    let (off, stride) = block_pos(i, field_dct);
                    idct_add(&mut self.blk[i], self.rowmask[i], &mut self.pred, off, stride);
                    self.blk[i] = [0; 64];
                }
            }
            self.prev_dirs = m.dirs;
        }
        self.store_mb(mbx, mby);
        Ok(())
    }

    fn skip_mb(&mut self, mbx: usize, mby: usize) -> R<()> {
        let h = self.hdr;
        let frame = h.structure == PS_FRAME;
        self.reset_dc();
        let mut m = Motion { mtype: if frame { MT_FRAME } else { MT_FIELD }, ..Default::default() };
        m.fsel[0] = [h.structure.wrapping_sub(1) & 1; 2];
        if h.coding_type == PIC_P {
            self.pmv = [[[0; 2]; 2]; 2];
            m.dirs = MB_FWD;
        } else {
            if self.prev_dirs == 0 {
                return Err(());
            }
            m.dirs = self.prev_dirs;
            for s in 0..2 {
                let v = self.pmv[0][s];
                m.mv[0][s] = if h.full_pel[s] { [v[0] * 2, v[1] * 2] } else { v };
            }
        }
        self.predict(&m, mbx, mby);
        self.store_mb(mbx, mby);
        Ok(())
    }

    /// Reference frame slot for direction `s` (0 forward, 1 backward).
    #[inline]
    fn ref_slot(&self, s: usize) -> usize {
        let r = if self.hdr.coding_type == PIC_B && s == 0 { self.fwd.or(self.bwd) } else { self.bwd };
        r.unwrap_or(self.cur_slot)
    }

    /// Reference slot for a field prediction in a field picture.
    #[inline]
    fn ref_field_slot(&self, s: usize, sel: u8) -> usize {
        let parity = (self.hdr.structure - 1) & 1;
        if self.hdr.coding_type == PIC_P && self.second_field && sel != parity {
            self.cur_slot
        } else {
            self.ref_slot(s)
        }
    }

    fn predict(&mut self, m: &Motion, mbx: usize, mby: usize) {
        let h = self.hdr;
        let x = (mbx * 16) as i32;
        let mut avg = false;
        for s in 0..2 {
            if m.dirs & (MB_FWD << s) == 0 {
                continue;
            }
            if h.structure == PS_FRAME {
                let p = &self.bufs[self.ref_slot(s)];
                let pred = &mut self.pred;
                match m.mtype {
                    MT_FIELD => {
                        let y = (mby * 8) as i32;
                        for r in 0..2 {
                            part(pred, p, Some(m.fsel[r][s]), x, y, 8, m.mv[r][s], r * 16, 32, r * 8, 16, avg);
                        }
                    }
                    MT_DMV => {
                        let y = (mby * 8) as i32;
                        let [mx, my] = m.mv[0][s];
                        let [dx, dy] = m.dmv;
                        let k = if h.tff { 1 } else { 3 };
                        let opp = [
                            [((mx * k + (mx > 0) as i32) >> 1) + dx, ((my * k + (my > 0) as i32) >> 1) + dy - 1],
                            [((mx * (4 - k) + (mx > 0) as i32) >> 1) + dx, ((my * (4 - k) + (my > 0) as i32) >> 1) + dy + 1],
                        ];
                        for r in 0..2 {
                            part(pred, p, Some(r as u8), x, y, 8, [mx, my], r * 16, 32, r * 8, 16, false);
                        }
                        for r in 0..2 {
                            part(pred, p, Some(1 - r as u8), x, y, 8, opp[r], r * 16, 32, r * 8, 16, true);
                        }
                    }
                    _ => {
                        part(pred, p, None, x, (mby * 16) as i32, 16, m.mv[0][s], 0, 16, 0, 8, avg);
                    }
                }
            } else {
                let parity = (h.structure - 1) & 1;
                let y = (mby * 16) as i32;
                match m.mtype {
                    MT_FRAME => {
                        // 16x8
                        for r in 0..2 {
                            let sel = m.fsel[r][s];
                            let p = &self.bufs[self.ref_field_slot(s, sel)];
                            part(&mut self.pred, p, Some(sel), x, y + 8 * r as i32, 8, m.mv[r][s], r * 128, 16, r * 32, 8, avg);
                        }
                    }
                    MT_DMV => {
                        let [mx, my] = m.mv[0][s];
                        let [dx, dy] = m.dmv;
                        let e = if parity == 0 { -1 } else { 1 };
                        let opp = [((mx + (mx > 0) as i32) >> 1) + dx, ((my + (my > 0) as i32) >> 1) + dy + e];
                        let p = &self.bufs[self.ref_field_slot(s, parity)];
                        part(&mut self.pred, p, Some(parity), x, y, 16, [mx, my], 0, 16, 0, 8, false);
                        let p = &self.bufs[self.ref_field_slot(s, 1 - parity)];
                        part(&mut self.pred, p, Some(1 - parity), x, y, 16, opp, 0, 16, 0, 8, true);
                    }
                    _ => {
                        let sel = m.fsel[0][s];
                        let p = &self.bufs[self.ref_field_slot(s, sel)];
                        part(&mut self.pred, p, Some(sel), x, y, 16, m.mv[0][s], 0, 16, 0, 8, avg);
                    }
                }
            }
            avg = true;
        }
    }

    /// Copy the reconstructed macroblock from the pred buffer into the current picture.
    fn store_mb(&mut self, mbx: usize, mby: usize) {
        let h = self.hdr;
        let p = &mut self.bufs[self.cur_slot];
        let (ys, cs) = (p.ys, p.cs);
        let (yo, yst, co, cst) = if h.structure == PS_FRAME {
            (mby * 16 * ys + mbx * 16, ys, mby * 8 * cs + mbx * 8, cs)
        } else {
            let f = ((h.structure - 1) & 1) as usize;
            (f * ys + mby * 32 * ys + mbx * 16, ys * 2, f * cs + mby * 16 * cs + mbx * 8, cs * 2)
        };
        for r in 0..16 {
            p.y[yo + r * yst..yo + r * yst + 16].copy_from_slice(&self.pred[PY + r * 16..PY + r * 16 + 16]);
        }
        for r in 0..8 {
            p.cb[co + r * cst..co + r * cst + 8].copy_from_slice(&self.pred[PCB + r * 8..PCB + r * 8 + 8]);
            p.cr[co + r * cst..co + r * cst + 8].copy_from_slice(&self.pred[PCR + r * 8..PCR + r * 8 + 8]);
        }
    }

    fn intra_block(&mut self, bs: &mut Bits, i: usize) -> R<()> {
        let h = self.hdr;
        let mpeg2 = self.seq.mpeg2;
        let comp = if i < 4 { 0 } else { i - 3 };
        let (size, len) = if comp == 0 {
            lookup(bs, &DC_LUMA, DC_LUMA_BITS)?
        } else {
            lookup(bs, &DC_CHROMA, DC_CHROMA_BITS)?
        };
        bs.skip(len);
        let diff = if size == 0 {
            0
        } else {
            let v = bs.get(size) as i32;
            if v < (1 << (size - 1)) {
                v - (1 << size) + 1
            } else {
                v
            }
        };
        let dc = (self.dc_pred[comp] + diff).clamp(-(1 << 14), 1 << 14);
        self.dc_pred[comp] = dc;
        let qs = self.qscale();
        let prec = if mpeg2 { h.dc_prec as u32 } else { 0 };
        let tab = if h.intra_vlc && mpeg2 { &B15 } else { &B14 };
        let scan = if h.alt_scan { &ALT_SCAN } else { &ZIGZAG };
        let qm = if comp == 0 { &self.mats.intra } else { &self.mats.c_intra };
        let blk = &mut self.blk[i];
        let dcv = (dc << (3 - prec)).clamp(-2048, 2047);
        blk[0] = dcv as i16;
        let mut mismatch = dcv;
        let mut mask = 1u8;
        let mut idx = 0usize;
        loop {
            let v = bs.peek32();
            let top = (v >> 16) as usize;
            let e = if top >= 0x400 { tab.a[top >> 8] } else { tab.b[top] };
            let len = e & 0xff;
            let run = (e >> 8) & 0xff;
            if len == 0 {
                return Err(());
            }
            bs.skip(len);
            let (run, level, neg) = if run == DCT_EOB {
                break;
            } else if run == DCT_ESC {
                let run = bs.get(6);
                let l = if mpeg2 { bs.get_signed(12) } else { mpeg1_escape_level(bs) };
                if l == 0 {
                    return Err(());
                }
                (run, l.unsigned_abs() as i32, l < 0)
            } else {
                (run, (e >> 16) as i32, bs.get1())
            };
            idx += run as usize + 1;
            if idx > 63 {
                return Err(());
            }
            let j = scan[idx] as usize;
            let mut val = (level * qs * qm[j] as i32) >> 4;
            if !mpeg2 && val & 1 == 0 && val != 0 {
                val -= 1;
            }
            let val = if neg { -(val.min(2048)) } else { val.min(2047) };
            blk[j] = val as i16;
            mismatch ^= val;
            mask |= 1 << (j >> 3);
        }
        if mpeg2 && mismatch & 1 == 0 {
            blk[63] ^= 1;
            mask |= 0x80;
        }
        self.rowmask[i] = mask;
        Ok(())
    }

    fn inter_block(&mut self, bs: &mut Bits, i: usize) -> R<()> {
        let mpeg2 = self.seq.mpeg2;
        let comp = if i < 4 { 0 } else { 1 };
        let qs = self.qscale();
        let scan = if self.hdr.alt_scan { &ALT_SCAN } else { &ZIGZAG };
        let qm = if comp == 0 { &self.mats.non_intra } else { &self.mats.c_non_intra };
        let blk = &mut self.blk[i];
        let mut mismatch = 0i32;
        let mut mask = 0u8;
        let mut idx = usize::MAX; // so the first coefficient lands on 0
        let mut first = true;
        loop {
            let v = bs.peek32();
            let (run, level, neg);
            if first && v >> 31 == 1 {
                // "1s": run 0, level 1 for the first coefficient.
                run = 0;
                level = 1;
                neg = (v >> 30) & 1 != 0;
                bs.skip(2);
            } else {
                let top = (v >> 16) as usize;
                let e = if top >= 0x400 { B14.a[top >> 8] } else { B14.b[top] };
                let len = e & 0xff;
                let r = (e >> 8) & 0xff;
                if len == 0 {
                    return Err(());
                }
                bs.skip(len);
                if r == DCT_EOB {
                    if first {
                        return Err(());
                    }
                    break;
                } else if r == DCT_ESC {
                    run = bs.get(6);
                    let l = if mpeg2 { bs.get_signed(12) } else { mpeg1_escape_level(bs) };
                    if l == 0 {
                        return Err(());
                    }
                    level = l.unsigned_abs() as i32;
                    neg = l < 0;
                } else {
                    run = r;
                    level = (e >> 16) as i32;
                    neg = bs.get1();
                }
            }
            first = false;
            idx = idx.wrapping_add(run as usize + 1);
            if idx > 63 {
                return Err(());
            }
            let j = scan[idx] as usize;
            let mut val = ((2 * level + 1) * qs * qm[j] as i32) >> 5;
            if !mpeg2 && val & 1 == 0 && val != 0 {
                val -= 1;
            }
            let val = if neg { -(val.min(2048)) } else { val.min(2047) };
            blk[j] = val as i16;
            mismatch ^= val;
            mask |= 1 << (j >> 3);
        }
        if mpeg2 && mismatch & 1 == 0 {
            blk[63] ^= 1;
            mask |= 0x80;
        }
        self.rowmask[i] = mask;
        Ok(())
    }
}

/// MPEG-1 escape level: 8 bits signed, with -128 / 0 extending to 16 bits.
#[inline]
fn mpeg1_escape_level(bs: &mut Bits) -> i32 {
    let l = bs.get_signed(8);
    if l == -128 {
        bs.get(8) as i32 - 256
    } else if l == 0 {
        bs.get(8) as i32
    } else {
        l
    }
}

/// (offset, stride) of block `i` inside the pred buffer.
#[inline(always)]
fn block_pos(i: usize, field_dct: bool) -> (usize, usize) {
    match (i, field_dct) {
        (0, false) => (PY, 16),
        (1, false) => (PY + 8, 16),
        (2, false) => (PY + 128, 16),
        (3, false) => (PY + 136, 16),
        (0, true) => (PY, 32),
        (1, true) => (PY + 8, 32),
        (2, true) => (PY + 16, 32),
        (3, true) => (PY + 24, 32),
        (4, _) => (PCB, 8),
        _ => (PCR, 8),
    }
}
