//! CABAC syntax elements: binarisations and context index selection (9.3.2, 9.3.3.1).

use crate::cabac::Cabac;
use crate::mb::{Coefs, Entropy, SliceCtx};
use crate::pic::*;
use crate::slice::SliceType;
use crate::tables::*;
use crate::Error;

pub struct CabacDec<'a> {
    pub c: Cabac<'a>,
    /// RBSP length in bytes (for PCM bounds)
    pub len: usize,
}

const SIG_OFF: [usize; 6] = [105, 105 + 15, 105 + 29, 105 + 44, 105 + 47, 402];
const LAST_OFF: [usize; 6] = [166, 166 + 15, 166 + 29, 166 + 44, 166 + 47, 417];
const ABS_OFF: [usize; 6] = [227, 227 + 10, 227 + 20, 227 + 30, 227 + 39, 426];
const CBF_OFF: [usize; 5] = [85, 89, 93, 97, 101];

const SIG8: [u8; 63] = [
    0, 1, 2, 3, 4, 5, 5, 4, 4, 3, 3, 4, 4, 4, 5, 5, 4, 4, 4, 4, 3, 3, 6, 7, 7, 7, 8, 9, 10, 9, 8, 7, 7, 6, 11, 12, 13, 11,
    6, 7, 8, 9, 14, 10, 9, 8, 6, 11, 12, 13, 11, 6, 9, 14, 10, 9, 11, 12, 13, 11, 14, 10, 12,
];
const LAST8: [u8; 63] = [
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3,
    3, 4, 4, 4, 4, 4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7, 8, 8, 8,
];

impl<'a> CabacDec<'a> {
    pub fn skip_flag(&mut self, s: &SliceCtx) -> bool {
        let mut inc = 0;
        if let Some(a) = s.mb_a() {
            if s.fs.mbt[a] & MB_SKIP == 0 {
                inc += 1;
            }
        }
        if let Some(b) = s.mb_b() {
            if s.fs.mbt[b] & MB_SKIP == 0 {
                inc += 1;
            }
        }
        let base = if s.stype == SliceType::B { 24 } else { 11 };
        self.c.decision(base + inc) != 0
    }

    pub fn end_of_slice(&mut self) -> bool {
        self.c.terminate() != 0
    }

    /// mb_type of an intra macroblock (I-slice numbering 0..=25); `base` = ctxIdxOffset (3, 17 or 32)
    fn intra_type(&mut self, s: &SliceCtx, base: usize, islice: bool) -> u32 {
        let c = &mut self.c;
        let b0 = if islice {
            let mut inc = 0;
            if let Some(a) = s.mb_a() {
                if s.fs.mbt[a] & MB_I4 == 0 && s.fs.mbt[a] & MB_I8 == 0 {
                    inc += 1;
                }
            }
            if let Some(b) = s.mb_b() {
                if s.fs.mbt[b] & MB_I4 == 0 && s.fs.mbt[b] & MB_I8 == 0 {
                    inc += 1;
                }
            }
            c.decision(base + inc)
        } else {
            c.decision(base)
        };
        if b0 == 0 {
            return 0;
        }
        if c.terminate() != 0 {
            return 25;
        }
        let st = if islice { base + 2 } else { base };
        let isl = islice as usize;
        let mut t = 1 + 12 * c.decision(st + 1);
        if c.decision(st + 2) != 0 {
            t += 4 + 4 * c.decision(st + 2 + isl);
        }
        t += 2 * c.decision(st + 3 + isl);
        t += c.decision(st + 3 + 2 * isl);
        t
    }

    fn cbf_ctx(&self, s: &SliceCtx, cat: u32, blk: usize) -> usize {
        let intra = s.fs.mbt[s.addr] & MB_INTRA != 0;
        // condTermFlag for an unavailable macroblock: 1 if the current MB is intra
        let unavail = intra as usize;
        let (a, b) = match cat {
            0 | 3 => {
                let bit = if cat == 0 { 0x100 } else { 0x200 << blk };
                let f = |m: Option<usize>| match m {
                    None => unavail,
                    Some(m) => {
                        if s.fs.mbt[m] & MB_PCM != 0 {
                            1
                        } else {
                            (s.fs.cbp[m] & bit != 0) as usize
                        }
                    }
                };
                (f(s.mb_a()), f(s.mb_b()))
            }
            1 | 2 | 5 => {
                let gx = (s.mbx * 4 + BLK_X[blk] as usize) as isize;
                let gy = (s.mby * 4 + BLK_Y[blk] as usize) as isize;
                let f = |i: Option<usize>| match i {
                    None => unavail,
                    Some(i) => (s.fs.nnz[i] != 0) as usize,
                };
                (f(s.nb4(gx - 1, gy)), f(s.nb4(gx, gy - 1)))
            }
            _ => {
                let cc = blk >> 2;
                let b4 = blk & 3;
                let cs = s.mbw * 2;
                let plane = s.mbw * s.mbh * 4;
                let gx = (s.mbx * 2 + (b4 & 1)) as isize;
                let gy = (s.mby * 2 + (b4 >> 1)) as isize;
                let f = |x: isize, y: isize| -> usize {
                    if x < 0 || y < 0 || !s.avail(x >> 1, y >> 1) {
                        unavail
                    } else {
                        (s.fs.nnzc[cc * plane + y as usize * cs + x as usize] != 0) as usize
                    }
                };
                (f(gx - 1, gy), f(gx, gy - 1))
            }
        };
        CBF_OFF[cat as usize] + a + 2 * b
    }
}

impl<'a> Entropy for CabacDec<'a> {
    fn mb_type(&mut self, s: &SliceCtx) -> Result<u32, Error> {
        match s.stype {
            SliceType::I => Ok(self.intra_type(s, 3, true)),
            SliceType::P => {
                let c = &mut self.c;
                if c.decision(14) == 0 {
                    if c.decision(15) == 0 {
                        Ok(3 * c.decision(16))
                    } else {
                        Ok(2 - c.decision(17))
                    }
                } else {
                    Ok(5 + self.intra_type(s, 17, false))
                }
            }
            SliceType::B => {
                let mut inc = 0;
                if let Some(a) = s.mb_a() {
                    if s.fs.mbt[a] & MB_DIRECT == 0 {
                        inc += 1;
                    }
                }
                if let Some(b) = s.mb_b() {
                    if s.fs.mbt[b] & MB_DIRECT == 0 {
                        inc += 1;
                    }
                }
                let c = &mut self.c;
                if c.decision(27 + inc) == 0 {
                    return Ok(0);
                }
                if c.decision(27 + 3) == 0 {
                    return Ok(1 + c.decision(27 + 5));
                }
                let mut bits = c.decision(27 + 4) << 3;
                bits |= c.decision(27 + 5) << 2;
                bits |= c.decision(27 + 5) << 1;
                bits |= c.decision(27 + 5);
                if bits < 8 {
                    Ok(bits + 3)
                } else if bits == 13 {
                    Ok(23 + self.intra_type(s, 32, false))
                } else if bits == 14 {
                    Ok(11)
                } else if bits == 15 {
                    Ok(22)
                } else {
                    bits = (bits << 1) | c.decision(27 + 5);
                    Ok(bits - 4)
                }
            }
        }
    }

    fn transform8(&mut self, s: &SliceCtx) -> bool {
        let mut inc = 0;
        if let Some(a) = s.mb_a() {
            inc += (s.fs.mbt[a] & MB_8X8DCT != 0) as usize;
        }
        if let Some(b) = s.mb_b() {
            inc += (s.fs.mbt[b] & MB_8X8DCT != 0) as usize;
        }
        self.c.decision(399 + inc) != 0
    }

    fn intra_mode(&mut self) -> Option<u32> {
        if self.c.decision(68) != 0 {
            return None;
        }
        let mut m = self.c.decision(69);
        m |= self.c.decision(69) << 1;
        m |= self.c.decision(69) << 2;
        Some(m)
    }

    fn chroma_mode(&mut self, s: &SliceCtx) -> u32 {
        let mut inc = 0;
        let cond = |m: Option<usize>| match m {
            Some(m) => s.fs.mbt[m] & MB_INTRA != 0 && s.fs.mbt[m] & MB_PCM == 0 && s.fs.chroma_pred[m] != 0,
            None => false,
        };
        if cond(s.mb_a()) {
            inc += 1;
        }
        if cond(s.mb_b()) {
            inc += 1;
        }
        let c = &mut self.c;
        if c.decision(64 + inc) == 0 {
            return 0;
        }
        if c.decision(67) == 0 {
            return 1;
        }
        if c.decision(67) == 0 {
            2
        } else {
            3
        }
    }

    fn sub_mb_type(&mut self, s: &SliceCtx) -> u32 {
        let c = &mut self.c;
        if s.stype == SliceType::P {
            if c.decision(21) != 0 {
                return 0;
            }
            if c.decision(22) == 0 {
                return 1;
            }
            if c.decision(23) != 0 {
                return 2;
            }
            return 3;
        }
        if c.decision(36) == 0 {
            return 0;
        }
        if c.decision(37) == 0 {
            return 1 + c.decision(39);
        }
        let mut t = 3;
        if c.decision(38) != 0 {
            if c.decision(39) != 0 {
                return 11 + c.decision(39);
            }
            t += 4;
        }
        t += 2 * c.decision(39);
        t += c.decision(39);
        t
    }

    fn ref_idx(&mut self, s: &SliceCtx, list: usize, x4: usize, y4: usize, _num: u32) -> u32 {
        let gx = (s.mbx * 4 + x4) as isize;
        let gy = (s.mby * 4 + y4) as isize;
        let cond = |i: Option<usize>, gx: isize, gy: isize| -> usize {
            match i {
                None => 0,
                Some(i) => {
                    if s.stype == SliceType::B {
                        let m = (gy as usize >> 2) * s.mbw + (gx as usize >> 2);
                        let b8 = ((gy as usize & 3) >> 1) * 2 + ((gx as usize & 3) >> 1);
                        if s.fs.direct8[m] & (1 << b8) != 0 {
                            return 0;
                        }
                    }
                    (s.cur.refi[list][i] > 0) as usize
                }
            }
        };
        let a = cond(s.nb4(gx - 1, gy), gx - 1, gy);
        let b = cond(s.nb4(gx, gy - 1), gx, gy - 1);
        let mut inc = a + 2 * b;
        let mut r = 0;
        while self.c.decision(54 + inc) != 0 {
            r += 1;
            inc = if r == 1 { 4 } else { 5 };
            if r >= 32 {
                break;
            }
        }
        r
    }

    fn mvd(&mut self, s: &mut SliceCtx, list: usize, x4: usize, y4: usize, w4: usize, h4: usize) -> [i32; 2] {
        let gx = (s.mbx * 4 + x4) as isize;
        let gy = (s.mby * 4 + y4) as isize;
        let a = s.nb4(gx - 1, gy).map(|i| s.fs.mvd[list][i]).unwrap_or([0, 0]);
        let b = s.nb4(gx, gy - 1).map(|i| s.fs.mvd[list][i]).unwrap_or([0, 0]);
        let mut out = [0i32; 2];
        let mut abs = [0u8; 2];
        for comp in 0..2 {
            let sum = a[comp] as u32 + b[comp] as u32;
            let base = if comp == 0 { 40 } else { 47 };
            let inc = if sum < 3 {
                0
            } else if sum <= 32 {
                1
            } else {
                2
            };
            let c = &mut self.c;
            if c.decision(base + inc) == 0 {
                continue;
            }
            let mut v: i32 = 1;
            let mut ctx = base + 3;
            while v < 9 && c.decision(ctx) != 0 {
                if v < 4 {
                    ctx += 1;
                }
                v += 1;
            }
            if v >= 9 {
                let mut k = 3;
                while c.bypass() != 0 {
                    v += 1 << k;
                    k += 1;
                    if k > 24 {
                        break;
                    }
                }
                while k > 0 {
                    k -= 1;
                    v += (c.bypass() << k) as i32;
                }
            }
            abs[comp] = v.min(64) as u8;
            out[comp] = if c.bypass() != 0 { -v } else { v };
        }
        s.fill_mvd(list, x4, y4, w4, h4, abs);
        out
    }

    fn cbp(&mut self, s: &SliceCtx, _intra: bool) -> Result<u32, Error> {
        let ma = s.mb_a();
        let mb = s.mb_b();
        // luma: condTermFlagN = ((cbp_N >> b8N) & 1) == 0, unavailable/PCM -> 0, skip -> 1
        let lbit = |m: Option<usize>, b8: usize| -> usize {
            match m {
                None => 0,
                Some(m) => {
                    let t = s.fs.mbt[m];
                    if t & MB_PCM != 0 {
                        0
                    } else if t & MB_SKIP != 0 {
                        1
                    } else {
                        ((s.fs.cbp[m] >> b8) & 1 == 0) as usize
                    }
                }
            }
        };
        let mut cbp = 0u32;
        for b8 in 0..4usize {
            let (bx, by) = (b8 & 1, b8 >> 1);
            let a = if bx == 1 { ((cbp >> (b8 - 1)) & 1 == 0) as usize } else { lbit(ma, b8 + 1) };
            let b = if by == 1 { ((cbp >> (b8 - 2)) & 1 == 0) as usize } else { lbit(mb, b8 + 2) };
            cbp |= self.c.decision(73 + a + 2 * b) << b8;
        }
        let cc = |m: Option<usize>| -> u32 {
            match m {
                None => 0,
                Some(m) => {
                    let t = s.fs.mbt[m];
                    if t & MB_PCM != 0 {
                        2
                    } else if t & MB_SKIP != 0 {
                        0
                    } else {
                        ((s.fs.cbp[m] >> 4) & 3) as u32
                    }
                }
            }
        };
        let (ca, cb) = (cc(ma), cc(mb));
        let inc = (ca != 0) as usize + 2 * (cb != 0) as usize;
        if self.c.decision(77 + inc) != 0 {
            let inc = (ca == 2) as usize + 2 * (cb == 2) as usize + 4;
            cbp |= (1 + self.c.decision(77 + inc)) << 4;
        }
        Ok(cbp)
    }

    fn qp_delta(&mut self, s: &SliceCtx) -> i32 {
        let c = &mut self.c;
        let mut ctx = 60 + s.prev_qp_delta_nz as usize;
        let mut k = 0;
        while c.decision(ctx) != 0 {
            k += 1;
            ctx = if k == 1 { 62 } else { 63 };
            if k > 104 {
                return 1000;
            }
        }
        if k & 1 == 1 {
            (k + 1) / 2
        } else {
            -(k / 2)
        }
    }

    fn residual(&mut self, s: &SliceCtx, cat: u32, blk: usize, max: usize, out: &mut Coefs) -> Result<usize, Error> {
        if cat != 5 {
            let ctx = self.cbf_ctx(s, cat, blk);
            if self.c.decision(ctx) == 0 {
                return Ok(0);
            }
        }
        let c = &mut self.c;
        let cat = cat as usize;
        let (sig, last, absb) = (SIG_OFF[cat], LAST_OFF[cat], ABS_OFF[cat]);
        let lastpos = max.clamp(1, 64) - 1;
        // significance map: scan positions of the non-zero coefficients
        #[inline(always)]
        fn sigmap(c: &mut crate::cabac::Cabac, pos: &mut [u8; 64], lastpos: usize, ctx: impl Fn(usize) -> (usize, usize)) -> usize {
            let mut n = 0;
            let mut i = 0;
            while i < lastpos {
                let (si, li) = ctx(i);
                if c.decision(si) != 0 {
                    pos[n & 63] = i as u8;
                    n += 1;
                    if c.decision(li) != 0 {
                        return n;
                    }
                }
                i += 1;
            }
            pos[n & 63] = lastpos as u8;
            n + 1
        }
        let n = match cat {
            5 => sigmap(c, &mut out.pos, lastpos, |i| (sig + SIG8[i.min(62)] as usize, last + LAST8[i.min(62)] as usize)),
            3 => sigmap(c, &mut out.pos, lastpos, |i| (sig + i.min(2), last + i.min(2))),
            _ => sigmap(c, &mut out.pos, lastpos, |i| (sig + i, last + i)),
        };
        let mut eq1 = 0usize;
        let mut gt1 = 0usize;
        let gt1_cap = if cat == 3 { 3 } else { 4 };
        for k in (0..n.min(64)).rev() {
            let inc0 = if gt1 != 0 { 0 } else { (1 + eq1).min(4) };
            let mut v: i32;
            if c.decision(absb + inc0) == 0 {
                v = 1;
                eq1 += 1;
            } else {
                let ctx = absb + 5 + gt1.min(gt1_cap);
                v = 2;
                while v < 15 && c.decision(ctx) != 0 {
                    v += 1;
                }
                if v >= 15 {
                    let mut kk = 0;
                    while c.bypass() != 0 {
                        kk += 1;
                        if kk > 23 {
                            return Err(Error::Invalid("coeff_abs_level escape"));
                        }
                    }
                    let mut suf = 1i32;
                    for _ in 0..kk {
                        suf = (suf << 1) | c.bypass() as i32;
                    }
                    v = 14 + suf;
                }
                gt1 += 1;
            }
            let sign = c.bypass() as i32;
            out.lv[k] = (v ^ -sign) + sign;
        }
        Ok(n)
    }

    fn pcm(&mut self, out: &mut [u8; 384]) -> Result<(), Error> {
        let bit = self.c.bit_pos();
        let start = (bit + 7) / 8;
        let data = self.c_data();
        if start + 384 > self.len {
            return Err(Error::Invalid("truncated PCM"));
        }
        out.copy_from_slice(&data[start..start + 384]);
        self.c.restart_at(start + 384);
        Ok(())
    }

    fn overrun(&self) -> bool {
        self.c.overrun()
    }
}

impl<'a> CabacDec<'a> {
    fn c_data(&self) -> &'a [u8] {
        self.c.data()
    }
}
