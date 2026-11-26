//! Deblocking filter (8.7), applied to a whole decoded frame in macroblock order.
//!
//! Boundary strengths are derived per macroblock from a per-frame table of reference picture ids
//! (built once per frame). On x86_64 with SSE2 the sample filters run on whole 16-line luma
//! edges (vertical edges via an in-register transpose) and on Cb+Cr edges together; elsewhere a
//! per-line scalar filter is used. Both produce identical results.

use crate::pic::*;
use crate::tables::*;
use alloc::vec::Vec;

/// "no reference" marker in the per-4x4 reference id table
const NONE: u32 = u32::MAX;

#[inline(always)]
fn clip1(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

struct Ctx<'a> {
    fs: &'a FrameState,
    pic: &'a Pic,
    rid: &'a [Vec<u32>; 2],
}

impl<'a> Ctx<'a> {
    /// Do all 16 4x4 blocks of the macroblock at 4x4 index `b` share references and vectors?
    #[inline(always)]
    fn uniform(&self, b: usize, s4: usize) -> bool {
        let (r0, r1) = (self.rid[0][b], self.rid[1][b]);
        let (m0, m1) = (self.pic.mv[0][b], self.pic.mv[1][b]);
        let mut same = true;
        for y in 0..4 {
            let o = b + y * s4;
            same &= self.rid[0][o..o + 4] == [r0; 4] && self.rid[1][o..o + 4] == [r1; 4];
            same &= self.pic.mv[0][o..o + 4] == [m0; 4] && self.pic.mv[1][o..o + 4] == [m1; 4];
        }
        same
    }

    /// bS for the edge between 4x4 blocks p and q of two inter macroblocks.
    #[inline(always)]
    fn bs_inter(&self, p: usize, q: usize) -> u8 {
        if self.fs.nzd[p] | self.fs.nzd[q] != 0 {
            return 2;
        }
        let (p0, p1) = (self.rid[0][p], self.rid[1][p]);
        let (q0, q1) = (self.rid[0][q], self.rid[1][q]);
        let mv = &self.pic.mv;
        let far = |a: [i16; 2], b: [i16; 2]| (a[0] as i32 - b[0] as i32).abs() >= 4 || (a[1] as i32 - b[1] as i32).abs() >= 4;
        let np = (p0 != NONE) as u8 + (p1 != NONE) as u8;
        let nq = (q0 != NONE) as u8 + (q1 != NONE) as u8;
        if np != nq {
            return 1;
        }
        if np == 1 {
            let (lp, rp) = if p0 != NONE { (0, p0) } else { (1, p1) };
            let (lq, rq) = if q0 != NONE { (0, q0) } else { (1, q1) };
            if rp != rq {
                return 1;
            }
            return far(mv[lp][p], mv[lq][q]) as u8;
        }
        if np == 0 {
            return 0;
        }
        // two motion vectors each
        if !((p0 == q0 && p1 == q1) || (p0 == q1 && p1 == q0)) {
            return 1;
        }
        let (mp0, mp1, mq0, mq1) = (mv[0][p], mv[1][p], mv[0][q], mv[1][q]);
        if p0 != p1 {
            if p0 == q0 {
                (far(mp0, mq0) || far(mp1, mq1)) as u8
            } else {
                (far(mp0, mq1) || far(mp1, mq0)) as u8
            }
        } else {
            ((far(mp0, mq0) || far(mp1, mq1)) && (far(mp0, mq1) || far(mp1, mq0))) as u8
        }
    }
}

/// Filter one line of samples across an edge. `pix[o]` = q0, `pix[o - step]` = p0.
#[allow(dead_code)]
#[inline(always)]
fn filter_luma(pix: &mut [u8], o: usize, step: usize, bs: u8, alpha: i32, beta: i32, tc0: i32) {
    let p0 = pix[o - step] as i32;
    let q0 = pix[o] as i32;
    let p1 = pix[o - 2 * step] as i32;
    let q1 = pix[o + step] as i32;
    if (p0 - q0).abs() >= alpha || (p1 - p0).abs() >= beta || (q1 - q0).abs() >= beta {
        return;
    }
    let p2 = pix[o - 3 * step] as i32;
    let q2 = pix[o + 2 * step] as i32;
    let ap = (p2 - p0).abs();
    let aq = (q2 - q0).abs();
    if bs < 4 {
        let tc = tc0 + (ap < beta) as i32 + (aq < beta) as i32;
        let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
        pix[o - step] = clip1(p0 + delta);
        pix[o] = clip1(q0 - delta);
        if ap < beta {
            pix[o - 2 * step] = (p1 + ((p2 + ((p0 + q0 + 1) >> 1) - (p1 << 1)) >> 1).clamp(-tc0, tc0)) as u8;
        }
        if aq < beta {
            pix[o + step] = (q1 + ((q2 + ((p0 + q0 + 1) >> 1) - (q1 << 1)) >> 1).clamp(-tc0, tc0)) as u8;
        }
    } else {
        let p3 = pix[o - 4 * step] as i32;
        let q3 = pix[o + 3 * step] as i32;
        let strong = (p0 - q0).abs() < ((alpha >> 2) + 2);
        if ap < beta && strong {
            pix[o - step] = ((p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3) as u8;
            pix[o - 2 * step] = ((p2 + p1 + p0 + q0 + 2) >> 2) as u8;
            pix[o - 3 * step] = ((2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3) as u8;
        } else {
            pix[o - step] = ((2 * p1 + p0 + q1 + 2) >> 2) as u8;
        }
        if aq < beta && strong {
            pix[o] = ((p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3) as u8;
            pix[o + step] = ((p0 + q0 + q1 + q2 + 2) >> 2) as u8;
            pix[o + 2 * step] = ((2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3) as u8;
        } else {
            pix[o] = ((2 * q1 + q0 + p1 + 2) >> 2) as u8;
        }
    }
}

#[allow(dead_code)]
#[inline(always)]
fn filter_chroma(pix: &mut [u8], o: usize, step: usize, bs: u8, alpha: i32, beta: i32, tc0: i32) {
    let p0 = pix[o - step] as i32;
    let q0 = pix[o] as i32;
    let p1 = pix[o - 2 * step] as i32;
    let q1 = pix[o + step] as i32;
    if (p0 - q0).abs() >= alpha || (p1 - p0).abs() >= beta || (q1 - q0).abs() >= beta {
        return;
    }
    if bs < 4 {
        let tc = tc0 + 1;
        let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
        pix[o - step] = clip1(p0 + delta);
        pix[o] = clip1(q0 - delta);
    } else {
        pix[o - step] = ((2 * p1 + p0 + q1 + 2) >> 2) as u8;
        pix[o] = ((2 * q1 + q0 + p1 + 2) >> 2) as u8;
    }
}

/// Filter a 16-sample luma edge. `o` = index of q0 of the first line; vertical edges run down
/// the rows, horizontal edges along a row. `b` = bS per 4-line segment (all < 4 or all 4).
#[inline(always)]
fn luma_edge(y: &mut [u8], o: usize, ys: usize, vertical: bool, b: [u8; 4], alpha: i32, beta: i32, tc0: [i32; 4]) {
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        // every sample touched lies in o-4..o+3 (vertical) / o-4*ys..o+3*ys (horizontal), 16 lines
        let (lo, hi) = if vertical { (o.wrapping_sub(4), o + 15 * ys + 4) } else { (o.wrapping_sub(4 * ys), o + 3 * ys + 16) };
        if lo < hi && hi <= y.len() {
            // SAFETY: bounds of the whole access window checked above
            unsafe { sse2::luma_edge(y.as_mut_ptr().add(o), ys, vertical, b, alpha, beta, tc0) };
            return;
        }
    }
    for k in 0..4 {
        if b[k] == 0 {
            continue;
        }
        for i in 0..4 {
            let (oo, step) = if vertical { (o + (k * 4 + i) * ys, 1) } else { (o + k * 4 + i, ys) };
            filter_luma(y, oo, step, b[k], alpha, beta, tc0[k]);
        }
    }
}

/// Filter an 8-sample chroma edge in both Cb (`u`) and Cr (`v`). `b` = bS per pair of lines;
/// alpha/beta/tc0 per component (alpha or beta == 0 disables that component).
#[inline(always)]
fn chroma_edge(u: &mut [u8], v: &mut [u8], o: usize, cs: usize, vertical: bool, b: [u8; 4], alpha: [i32; 2], beta: [i32; 2], tc0: [[i32; 4]; 2]) {
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        let (lo, hi) = if vertical { (o.wrapping_sub(2), o + 7 * cs + 2) } else { (o.wrapping_sub(2 * cs), o + cs + 8) };
        if lo < hi && hi <= u.len() && hi <= v.len() {
            // SAFETY: bounds of the access window checked above
            unsafe { sse2::chroma_edge(u.as_mut_ptr().add(o), v.as_mut_ptr().add(o), cs, vertical, b, alpha, beta, tc0) };
            return;
        }
    }
    for (cc, buf) in [u, v].into_iter().enumerate() {
        if alpha[cc] == 0 || beta[cc] == 0 {
            continue;
        }
        for i in 0..8 {
            let bsv = b[i >> 1];
            if bsv == 0 {
                continue;
            }
            let (oo, step) = if vertical { (o + i * cs, 1) } else { (o + i, cs) };
            filter_chroma(buf, oo, step, bsv, alpha[cc], beta[cc], tc0[cc][i >> 1]);
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
mod sse2 {
    use core::arch::x86_64::*;

    #[inline(always)]
    unsafe fn absdiff(a: __m128i, b: __m128i) -> __m128i {
        _mm_or_si128(_mm_subs_epu8(a, b), _mm_subs_epu8(b, a))
    }

    /// lanes where x < t (t >= 1, passed as t - 1)
    #[inline(always)]
    unsafe fn lt(x: __m128i, tm1: __m128i) -> __m128i {
        _mm_cmpeq_epi8(_mm_subs_epu8(x, tm1), _mm_setzero_si128())
    }

    #[inline(always)]
    unsafe fn sel(m: __m128i, a: __m128i, b: __m128i) -> __m128i {
        _mm_or_si128(_mm_and_si128(m, a), _mm_andnot_si128(m, b))
    }

    #[inline(always)]
    unsafe fn lo16(a: __m128i) -> __m128i {
        _mm_unpacklo_epi8(a, _mm_setzero_si128())
    }

    #[inline(always)]
    unsafe fn hi16(a: __m128i) -> __m128i {
        _mm_unpackhi_epi8(a, _mm_setzero_si128())
    }

    /// Normal (bS < 4) filter for p0/q0: returns (p0', q0') unclipped, given tc (i16 lanes).
    #[inline(always)]
    unsafe fn delta16(p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i, tc: __m128i) -> (__m128i, __m128i) {
        let d = _mm_add_epi16(_mm_slli_epi16(_mm_sub_epi16(q0, p0), 2), _mm_sub_epi16(p1, q1));
        let d = _mm_srai_epi16(_mm_add_epi16(d, _mm_set1_epi16(4)), 3);
        let d = _mm_max_epi16(_mm_min_epi16(d, tc), _mm_sub_epi16(_mm_setzero_si128(), tc));
        (_mm_add_epi16(p0, d), _mm_sub_epi16(q0, d))
    }

    /// p1' (or q1') of the normal filter: p1 + clip3(-tc0, tc0, (p2 + ((p0 + q0 + 1) >> 1) - 2 p1) >> 1)
    #[inline(always)]
    unsafe fn side16(p2: __m128i, p1: __m128i, avg: __m128i, tc0: __m128i) -> __m128i {
        let d = _mm_srai_epi16(_mm_sub_epi16(_mm_add_epi16(p2, avg), _mm_slli_epi16(p1, 1)), 1);
        let d = _mm_max_epi16(_mm_min_epi16(d, tc0), _mm_sub_epi16(_mm_setzero_si128(), tc0));
        _mm_add_epi16(p1, d)
    }

    /// Filter 16 lanes; `r` = [p3, p2, p1, p0, q0, q1, q2, q3]. `en` = lanes with bS > 0,
    /// `tc0` = per-lane tc0 bytes (unused when strong).
    #[inline(always)]
    unsafe fn luma16(r: &mut [__m128i; 8], strong: bool, en: __m128i, alpha: i32, beta: i32, tc0: __m128i) {
        let [p3, p2, p1, p0, q0, q1, q2, q3] = *r;
        let am1 = _mm_set1_epi8((alpha - 1) as u8 as i8);
        let bm1 = _mm_set1_epi8((beta - 1) as u8 as i8);
        let mask = _mm_and_si128(
            _mm_and_si128(lt(absdiff(p0, q0), am1), en),
            _mm_and_si128(lt(absdiff(p1, p0), bm1), lt(absdiff(q1, q0), bm1)),
        );
        if _mm_movemask_epi8(mask) == 0 {
            return;
        }
        let ap = lt(absdiff(p2, p0), bm1);
        let aq = lt(absdiff(q2, q0), bm1);
        if !strong {
            let mut np0 = [_mm_setzero_si128(); 2];
            let mut nq0 = [_mm_setzero_si128(); 2];
            let mut np1 = [_mm_setzero_si128(); 2];
            let mut nq1 = [_mm_setzero_si128(); 2];
            for h in 0..2 {
                let u = |a: __m128i| if h == 0 { lo16(a) } else { hi16(a) };
                let (p2w, p1w, p0w, q0w, q1w, q2w) = (u(p2), u(p1), u(p0), u(q0), u(q1), u(q2));
                let tc0w = u(tc0);
                // ap/aq lanes are 0xFF: unpacked with themselves they become i16 -1
                let apw = if h == 0 { _mm_unpacklo_epi8(ap, ap) } else { _mm_unpackhi_epi8(ap, ap) };
                let aqw = if h == 0 { _mm_unpacklo_epi8(aq, aq) } else { _mm_unpackhi_epi8(aq, aq) };
                let tc = _mm_sub_epi16(_mm_sub_epi16(tc0w, apw), aqw);
                let (a, b) = delta16(p1w, p0w, q0w, q1w, tc);
                np0[h] = a;
                nq0[h] = b;
                let avg = _mm_srli_epi16(_mm_add_epi16(_mm_add_epi16(p0w, q0w), _mm_set1_epi16(1)), 1);
                np1[h] = side16(p2w, p1w, avg, tc0w);
                nq1[h] = side16(q2w, q1w, avg, tc0w);
            }
            let np0 = _mm_packus_epi16(np0[0], np0[1]);
            let nq0 = _mm_packus_epi16(nq0[0], nq0[1]);
            let np1 = _mm_packus_epi16(np1[0], np1[1]);
            let nq1 = _mm_packus_epi16(nq1[0], nq1[1]);
            r[3] = sel(mask, np0, p0);
            r[4] = sel(mask, nq0, q0);
            r[2] = sel(_mm_and_si128(mask, ap), np1, p1);
            r[5] = sel(_mm_and_si128(mask, aq), nq1, q1);
        } else {
            let sm1 = _mm_set1_epi8(((alpha >> 2) + 1) as u8 as i8);
            let small = lt(absdiff(p0, q0), sm1);
            let sp = _mm_and_si128(mask, _mm_and_si128(ap, small));
            let sq = _mm_and_si128(mask, _mm_and_si128(aq, small));
            let mut o = [[_mm_setzero_si128(); 2]; 8];
            let two = _mm_set1_epi16(2);
            let four = _mm_set1_epi16(4);
            for h in 0..2 {
                let u = |a: __m128i| if h == 0 { lo16(a) } else { hi16(a) };
                let (p3w, p2w, p1w, p0w, q0w, q1w, q2w, q3w) = (u(p3), u(p2), u(p1), u(p0), u(q0), u(q1), u(q2), u(q3));
                let pq = _mm_add_epi16(p0w, q0w);
                // p0' = (p2 + 2p1 + 2p0 + 2q0 + q1 + 4) >> 3
                o[0][h] = _mm_srai_epi16(_mm_add_epi16(_mm_add_epi16(_mm_add_epi16(p2w, q1w), _mm_slli_epi16(_mm_add_epi16(p1w, pq), 1)), four), 3);
                // p1' = (p2 + p1 + p0 + q0 + 2) >> 2
                let tp = _mm_add_epi16(_mm_add_epi16(p1w, pq), p2w);
                o[1][h] = _mm_srai_epi16(_mm_add_epi16(tp, two), 2);
                // p2' = (2p3 + 3p2 + p1 + p0 + q0 + 4) >> 3
                o[2][h] = _mm_srai_epi16(_mm_add_epi16(_mm_add_epi16(_mm_slli_epi16(_mm_add_epi16(p3w, p2w), 1), tp), four), 3);
                // weak p0' = (2p1 + p0 + q1 + 2) >> 2
                o[3][h] = _mm_srai_epi16(_mm_add_epi16(_mm_add_epi16(_mm_slli_epi16(p1w, 1), _mm_add_epi16(p0w, q1w)), two), 2);
                // q side
                o[4][h] = _mm_srai_epi16(_mm_add_epi16(_mm_add_epi16(_mm_add_epi16(q2w, p1w), _mm_slli_epi16(_mm_add_epi16(q1w, pq), 1)), four), 3);
                let tq = _mm_add_epi16(_mm_add_epi16(q1w, pq), q2w);
                o[5][h] = _mm_srai_epi16(_mm_add_epi16(tq, two), 2);
                o[6][h] = _mm_srai_epi16(_mm_add_epi16(_mm_add_epi16(_mm_slli_epi16(_mm_add_epi16(q3w, q2w), 1), tq), four), 3);
                o[7][h] = _mm_srai_epi16(_mm_add_epi16(_mm_add_epi16(_mm_slli_epi16(q1w, 1), _mm_add_epi16(q0w, p1w)), two), 2);
            }
            let pk = |i: usize| _mm_packus_epi16(o[i][0], o[i][1]);
            let (sp0, sp1, sp2, wp0, sq0, sq1, sq2, wq0) = (pk(0), pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), pk(7));
            r[3] = sel(mask, sel(sp, sp0, wp0), p0);
            r[2] = sel(sp, sp1, p1);
            r[1] = sel(sp, sp2, p2);
            r[4] = sel(mask, sel(sq, sq0, wq0), q0);
            r[5] = sel(sq, sq1, q1);
            r[6] = sel(sq, sq2, q2);
        }
    }

    /// 16 rows x 8 bytes (row i at p + i*s) -> 8 vectors of 16 lanes.
    #[inline(always)]
    unsafe fn load_t16x8(p: *const u8, s: usize) -> [__m128i; 8] {
        let ld = |i: usize| _mm_loadl_epi64(p.add(i * s) as *const __m128i);
        let a: [__m128i; 8] = core::array::from_fn(|k| _mm_unpacklo_epi8(ld(2 * k), ld(2 * k + 1)));
        let b0 = _mm_unpacklo_epi16(a[0], a[1]);
        let b1 = _mm_unpackhi_epi16(a[0], a[1]);
        let b2 = _mm_unpacklo_epi16(a[2], a[3]);
        let b3 = _mm_unpackhi_epi16(a[2], a[3]);
        let b4 = _mm_unpacklo_epi16(a[4], a[5]);
        let b5 = _mm_unpackhi_epi16(a[4], a[5]);
        let b6 = _mm_unpacklo_epi16(a[6], a[7]);
        let b7 = _mm_unpackhi_epi16(a[6], a[7]);
        let c0 = _mm_unpacklo_epi32(b0, b2);
        let c1 = _mm_unpackhi_epi32(b0, b2);
        let c2 = _mm_unpacklo_epi32(b1, b3);
        let c3 = _mm_unpackhi_epi32(b1, b3);
        let c4 = _mm_unpacklo_epi32(b4, b6);
        let c5 = _mm_unpackhi_epi32(b4, b6);
        let c6 = _mm_unpacklo_epi32(b5, b7);
        let c7 = _mm_unpackhi_epi32(b5, b7);
        [
            _mm_unpacklo_epi64(c0, c4),
            _mm_unpackhi_epi64(c0, c4),
            _mm_unpacklo_epi64(c1, c5),
            _mm_unpackhi_epi64(c1, c5),
            _mm_unpacklo_epi64(c2, c6),
            _mm_unpackhi_epi64(c2, c6),
            _mm_unpacklo_epi64(c3, c7),
            _mm_unpackhi_epi64(c3, c7),
        ]
    }

    /// Inverse of `load_t16x8`.
    #[inline(always)]
    unsafe fn store_t8x16(p: *mut u8, s: usize, x: &[__m128i; 8]) {
        let d0l = _mm_unpacklo_epi8(x[0], x[1]);
        let d0h = _mm_unpackhi_epi8(x[0], x[1]);
        let d1l = _mm_unpacklo_epi8(x[2], x[3]);
        let d1h = _mm_unpackhi_epi8(x[2], x[3]);
        let d2l = _mm_unpacklo_epi8(x[4], x[5]);
        let d2h = _mm_unpackhi_epi8(x[4], x[5]);
        let d3l = _mm_unpacklo_epi8(x[6], x[7]);
        let d3h = _mm_unpackhi_epi8(x[6], x[7]);
        for (half, (a, b, c, d)) in [(d0l, d1l, d2l, d3l), (d0h, d1h, d2h, d3h)].into_iter().enumerate() {
            let e0 = _mm_unpacklo_epi16(a, b); // lines 0..3, bytes 0..3
            let e1 = _mm_unpackhi_epi16(a, b); // lines 4..7, bytes 0..3
            let e2 = _mm_unpacklo_epi16(c, d); // lines 0..3, bytes 4..7
            let e3 = _mm_unpackhi_epi16(c, d); // lines 4..7, bytes 4..7
            let rows = [_mm_unpacklo_epi32(e0, e2), _mm_unpackhi_epi32(e0, e2), _mm_unpacklo_epi32(e1, e3), _mm_unpackhi_epi32(e1, e3)];
            for (k, r) in rows.into_iter().enumerate() {
                let line = half * 8 + k * 2;
                _mm_storel_epi64(p.add(line * s) as *mut __m128i, r);
                _mm_storel_epi64(p.add((line + 1) * s) as *mut __m128i, _mm_unpackhi_epi64(r, r));
            }
        }
    }

    /// See `super::luma_edge`. `p` points at q0 of the first line.
    pub unsafe fn luma_edge(p: *mut u8, ys: usize, vertical: bool, b: [u8; 4], alpha: i32, beta: i32, tc0: [i32; 4]) {
        let mut en = [0u8; 16];
        let mut tcb = [0u8; 16];
        for i in 0..16 {
            en[i] = if b[i >> 2] != 0 { 0xFF } else { 0 };
            tcb[i] = tc0[i >> 2] as u8;
        }
        let en = _mm_loadu_si128(en.as_ptr() as *const __m128i);
        let tc = _mm_loadu_si128(tcb.as_ptr() as *const __m128i);
        let strong = b[0] == 4;
        if vertical {
            let base = p.sub(4);
            let mut r = load_t16x8(base, ys);
            let orig = r;
            luma16(&mut r, strong, en, alpha, beta, tc);
            let mut same = 0xFFFF;
            for k in 1..7 {
                same &= _mm_movemask_epi8(_mm_cmpeq_epi8(r[k], orig[k]));
            }
            if same != 0xFFFF {
                store_t8x16(base, ys, &r);
            }
        } else {
            let base = p.sub(4 * ys);
            let mut r: [__m128i; 8] = core::array::from_fn(|k| _mm_loadu_si128(base.add(k * ys) as *const __m128i));
            luma16(&mut r, strong, en, alpha, beta, tc);
            for k in 1..7 {
                _mm_storeu_si128(base.add(k * ys) as *mut __m128i, r[k]);
            }
        }
    }

    /// See `super::chroma_edge`. `u`/`v` point at q0 of the first line. Lanes 0..8 Cb, 8..16 Cr.
    pub unsafe fn chroma_edge(u: *mut u8, v: *mut u8, cs: usize, vertical: bool, b: [u8; 4], alpha: [i32; 2], beta: [i32; 2], tc0: [[i32; 4]; 2]) {
        let mut en = [0u8; 16];
        let mut tcb = [0u8; 16];
        let mut am1 = [0u8; 16];
        let mut bm1 = [0u8; 16];
        for i in 0..16 {
            let (cc, l) = (i >> 3, i & 7);
            let ok = b[l >> 1] != 0 && alpha[cc] != 0 && beta[cc] != 0;
            en[i] = if ok { 0xFF } else { 0 };
            tcb[i] = (tc0[cc][l >> 1] + 1) as u8;
            am1[i] = (alpha[cc] - 1).max(0) as u8;
            bm1[i] = (beta[cc] - 1).max(0) as u8;
        }
        let ld = |a: &[u8; 16]| _mm_loadu_si128(a.as_ptr() as *const __m128i);
        let (en, tc, am1, bm1) = (ld(&en), ld(&tcb), ld(&am1), ld(&bm1));
        // rows p1, p0, q0, q1
        let mut rv = [_mm_setzero_si128(); 4];
        if vertical {
            let mut r = [[0u8; 16]; 4];
            for l in 0..8 {
                for k in 0..4 {
                    r[k][l] = *u.add(l * cs + k).sub(2);
                    r[k][8 + l] = *v.add(l * cs + k).sub(2);
                }
            }
            for k in 0..4 {
                rv[k] = ld(&r[k]);
            }
        } else {
            for k in 0..4 {
                let a = _mm_loadl_epi64(u.add(k * cs).sub(2 * cs) as *const __m128i);
                let c = _mm_loadl_epi64(v.add(k * cs).sub(2 * cs) as *const __m128i);
                rv[k] = _mm_unpacklo_epi64(a, c);
            }
        }
        let [p1, p0, q0, q1] = rv;
        let mask = _mm_and_si128(
            _mm_and_si128(lt(absdiff(p0, q0), am1), en),
            _mm_and_si128(lt(absdiff(p1, p0), bm1), lt(absdiff(q1, q0), bm1)),
        );
        if _mm_movemask_epi8(mask) == 0 {
            return;
        }
        let (np0, nq0) = if b[0] == 4 {
            let two = _mm_set1_epi16(2);
            let f = |a: __m128i, b: __m128i, c: __m128i| _mm_srai_epi16(_mm_add_epi16(_mm_add_epi16(_mm_slli_epi16(a, 1), _mm_add_epi16(b, c)), two), 2);
            (
                _mm_packus_epi16(f(lo16(p1), lo16(p0), lo16(q1)), f(hi16(p1), hi16(p0), hi16(q1))),
                _mm_packus_epi16(f(lo16(q1), lo16(q0), lo16(p1)), f(hi16(q1), hi16(q0), hi16(p1))),
            )
        } else {
            let (a0, b0) = delta16(lo16(p1), lo16(p0), lo16(q0), lo16(q1), lo16(tc));
            let (a1, b1) = delta16(hi16(p1), hi16(p0), hi16(q0), hi16(q1), hi16(tc));
            (_mm_packus_epi16(a0, a1), _mm_packus_epi16(b0, b1))
        };
        let np0 = sel(mask, np0, p0);
        let nq0 = sel(mask, nq0, q0);
        if vertical {
            let mut a = [0u8; 16];
            let mut c = [0u8; 16];
            _mm_storeu_si128(a.as_mut_ptr() as *mut __m128i, np0);
            _mm_storeu_si128(c.as_mut_ptr() as *mut __m128i, nq0);
            for l in 0..8 {
                *u.add(l * cs).sub(1) = a[l];
                *u.add(l * cs) = c[l];
                *v.add(l * cs).sub(1) = a[8 + l];
                *v.add(l * cs) = c[8 + l];
            }
        } else {
            _mm_storel_epi64(u.sub(cs) as *mut __m128i, np0);
            _mm_storel_epi64(v.sub(cs) as *mut __m128i, _mm_unpackhi_epi64(np0, np0));
            _mm_storel_epi64(u as *mut __m128i, nq0);
            _mm_storel_epi64(v as *mut __m128i, _mm_unpackhi_epi64(nq0, nq0));
        }
    }
}

/// Fill the per-4x4 reference picture id table (NONE when the list is unused) for MB rows y0..y1.
fn build_ref_ids(fs: &FrameState, pic: &Pic, rid: &mut [Vec<u32>; 2], y0: usize, y1: usize) {
    let mbw = fs.mbw;
    let s4 = mbw * 4;
    let n4 = fs.mbw * fs.mbh * 16;
    for l in 0..2 {
        if rid[l].len() != n4 {
            rid[l].clear();
            rid[l].resize(n4, NONE);
        }
    }
    if pic.refi[0].len() < n4 || pic.refi[1].len() < n4 || pic.mv[0].len() < n4 || pic.mv[1].len() < n4 {
        return;
    }
    for m in y0 * mbw..y1.min(fs.mbh) * mbw {
        let sl = fs.slice[m];
        let base = (m / mbw) * 4 * s4 + (m % mbw) * 4;
        if sl == NOT_DECODED || fs.mbt[m] & MB_INTRA != 0 {
            for l in 0..2 {
                for y in 0..4 {
                    rid[l][base + y * s4..base + y * s4 + 4].fill(NONE);
                }
            }
            continue;
        }
        let refs = pic.slice_refs.get(sl as usize);
        for l in 0..2 {
            let list = refs.map(|v| &v[l][..]).unwrap_or(&[]);
            for y in 0..4 {
                let o = base + y * s4;
                let src = &pic.refi[l][o..o + 4];
                let dst = &mut rid[l][o..o + 4];
                for (d, &r) in dst.iter_mut().zip(src) {
                    *d = if r < 0 { NONE } else { list.get(r as usize).copied().unwrap_or(u32::MAX - 1) };
                }
            }
        }
    }
}

/// Deblock macroblock rows y0..y1 of the picture. Rows must be filtered in order, each once all
/// macroblocks of the row (and the one below, whose intra prediction reads unfiltered samples)
/// have been decoded.
pub fn deblock_rows(fs: &mut FrameState, pic: &mut Pic, y0: usize, y1: usize) {
    let y1 = y1.min(fs.mbh);
    if y0 >= y1 {
        return;
    }
    let mut rid = core::mem::take(&mut fs.dbk_ref);
    build_ref_ids(fs, pic, &mut rid, y0, y1);
    deblock_inner(fs, pic, &rid, y0, y1);
    fs.dbk_ref = rid;
}

fn deblock_inner(fs: &FrameState, pic: &mut Pic, rid: &[Vec<u32>; 2], y0: usize, y1: usize) {
    let mbw = fs.mbw;
    let s4 = mbw * 4;
    let mut planes = core::mem::take(&mut pic.planes);
    {
        let c = Ctx { fs, pic, rid };
        for mby in y0..y1 {
            for mbx in 0..mbw {
                let m = mby * mbw + mbx;
                let sl = fs.slice[m];
                if sl == NOT_DECODED {
                    continue;
                }
                let sp = match fs.slice_params.get(sl as usize) {
                    Some(p) => *p,
                    None => continue,
                };
                if sp.disable_deblock == 1 {
                    continue;
                }
                let left_ok = mbx > 0 && fs.slice[m - 1] != NOT_DECODED && !(sp.disable_deblock == 2 && fs.slice[m - 1] != sl);
                let top_ok = mby > 0 && fs.slice[m - mbw] != NOT_DECODED && !(sp.disable_deblock == 2 && fs.slice[m - mbw] != sl);
                let t8 = fs.mbt[m] & MB_8X8DCT != 0;
                let base4 = mby * 4 * s4 + mbx * 4;
                let intra = fs.mbt[m] & MB_INTRA != 0;
                // one motion for the whole macroblock: internal edges only depend on coefficients
                let uni = !intra && c.uniform(base4, s4);
                // bS: [dir][edge][segment]
                let mut bs = [[[0u8; 4]; 4]; 2];
                for dir in 0..2 {
                    let ok = if dir == 0 { left_ok } else { top_ok };
                    for e in 0..4 {
                        if e == 0 {
                            if !ok {
                                continue;
                            }
                            let pm = if dir == 0 { m - 1 } else { m - mbw };
                            if intra || fs.mbt[pm] & MB_INTRA != 0 {
                                bs[dir][0] = [4; 4];
                                continue;
                            }
                        } else if intra {
                            bs[dir][e] = [3; 4];
                            continue;
                        } else if t8 && e & 1 == 1 {
                            // not filtered; only edges 0 and 2 are used by chroma
                            continue;
                        } else if uni {
                            for k in 0..4 {
                                let (q, p) = if dir == 0 {
                                    let q = base4 + k * s4 + e;
                                    (q, q - 1)
                                } else {
                                    let q = base4 + e * s4 + k;
                                    (q, q - s4)
                                };
                                bs[dir][e][k] = if fs.nzd[p] | fs.nzd[q] != 0 { 2 } else { 0 };
                            }
                            continue;
                        }
                        for k in 0..4 {
                            let (q, p) = if dir == 0 {
                                let q = base4 + k * s4 + e;
                                (q, q - 1)
                            } else {
                                let q = base4 + e * s4 + k;
                                (q, q - s4)
                            };
                            bs[dir][e][k] = c.bs_inter(p, q);
                        }
                    }
                }
                let ys = planes.ys;
                let o0 = planes.yoff(mbx * 16, mby * 16);
                for dir in 0..2 {
                    for e in 0..4 {
                        if t8 && e & 1 == 1 {
                            continue;
                        }
                        let b = bs[dir][e];
                        if b == [0; 4] {
                            continue;
                        }
                        // neighbour macroblock for QP
                        let pm = if e == 0 { if dir == 0 { m - 1 } else { m - mbw } } else { m };
                        let qpav = (fs.qp[pm] as i32 + fs.qp[m] as i32 + 1) >> 1;
                        let ia = (qpav + sp.alpha_offset).clamp(0, 51) as usize;
                        let ib = (qpav + sp.beta_offset).clamp(0, 51) as usize;
                        let alpha = ALPHA[ia] as i32;
                        let beta = BETA[ib] as i32;
                        if alpha == 0 || beta == 0 {
                            continue;
                        }
                        let mut tc0 = [0i32; 4];
                        for k in 0..4 {
                            if b[k] != 0 && b[k] < 4 {
                                tc0[k] = TC0[ia][b[k] as usize - 1] as i32;
                            }
                        }
                        let o = if dir == 0 { o0 + e * 4 } else { o0 + e * 4 * ys };
                        luma_edge(&mut planes.y, o, ys, dir == 0, b, alpha, beta, tc0);
                    }
                }
                // chroma: edges 0 and 2 (luma units) -> chroma sample 0 and 4
                let cs = planes.cs;
                let oc0 = planes.coff(mbx * 8, mby * 8);
                for dir in 0..2 {
                    for ce in 0..2 {
                        let e = ce * 2;
                        let b = bs[dir][e];
                        if b == [0; 4] {
                            continue;
                        }
                        let pm = if e == 0 { if dir == 0 { m - 1 } else { m - mbw } } else { m };
                        let mut alpha = [0i32; 2];
                        let mut beta = [0i32; 2];
                        let mut tc0 = [[0i32; 4]; 2];
                        for cc in 0..2 {
                            let qpav = (fs.qpc[pm][cc] as i32 + fs.qpc[m][cc] as i32 + 1) >> 1;
                            let ia = (qpav + sp.alpha_offset).clamp(0, 51) as usize;
                            let ib = (qpav + sp.beta_offset).clamp(0, 51) as usize;
                            alpha[cc] = ALPHA[ia] as i32;
                            beta[cc] = BETA[ib] as i32;
                            for k in 0..4 {
                                if b[k] != 0 && b[k] < 4 {
                                    tc0[cc][k] = TC0[ia][b[k] as usize - 1] as i32;
                                }
                            }
                        }
                        if (alpha[0] == 0 || beta[0] == 0) && (alpha[1] == 0 || beta[1] == 0) {
                            continue;
                        }
                        let o = if dir == 0 { oc0 + ce * 4 } else { oc0 + ce * 4 * cs };
                        chroma_edge(&mut planes.u, &mut planes.v, o, cs, dir == 0, b, alpha, beta, tc0);
                    }
                }
            }
        }
    }
    pic.planes = planes;
}
