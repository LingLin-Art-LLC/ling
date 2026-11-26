//! Inter prediction sample interpolation (8.4.2.2) and weighted sample prediction (8.4.2.3).
//!
//! The reference planes are padded (`PAD`/`PADC` samples of edge replication) and the integer
//! block position is clamped so every read stays inside the padded plane: no per-sample bounds
//! handling. The kernels are written row-wise over constant block widths so the compiler can
//! vectorise them (SSE2 on x86_64, NEON on aarch64).

use crate::pic::{Planes, PAD, PADC};

#[inline(always)]
fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[inline(always)]
fn clip16(v: i16) -> u8 {
    v.clamp(0, 255) as u8
}

/// 6-tap filter of `r[x..x + 6]` (r[x] is sample "-2"), unrounded. Fits in i16 for 8-bit input.
#[inline(always)]
fn tap6(r: &[u8], x: usize) -> i16 {
    let a = r[x] as i16 + r[x + 5] as i16;
    let b = r[x + 1] as i16 + r[x + 4] as i16;
    let c = r[x + 2] as i16 + r[x + 3] as i16;
    a - 5 * b + 20 * c
}

/// Horizontal half-sample `b` for rows `dy..dy+h` (dst stride ds). `o` = index of sample (0,0).
#[inline(always)]
fn hpel_h<const W: usize>(src: &[u8], o: usize, s: usize, h: usize, dy: usize, dst: &mut [u8], ds: usize) {
    for y in 0..h {
        let st = o + (y + dy) * s - 2;
        let r = &src[st..st + W + 5];
        let d = &mut dst[y * ds..y * ds + W];
        for x in 0..W {
            d[x] = clip16((tap6(r, x) + 16) >> 5);
        }
    }
}

/// Vertical half-sample `h` for columns `dx..dx+w`.
#[inline(always)]
fn hpel_v<const W: usize>(src: &[u8], o: usize, s: usize, h: usize, dx: usize, dst: &mut [u8], ds: usize) {
    for y in 0..h {
        let b = o + y * s + dx - 2 * s;
        let r0 = &src[b..b + W];
        let r1 = &src[b + s..b + s + W];
        let r2 = &src[b + 2 * s..b + 2 * s + W];
        let r3 = &src[b + 3 * s..b + 3 * s + W];
        let r4 = &src[b + 4 * s..b + 4 * s + W];
        let r5 = &src[b + 5 * s..b + 5 * s + W];
        let d = &mut dst[y * ds..y * ds + W];
        for x in 0..W {
            let a = r0[x] as i16 + r5[x] as i16;
            let bb = r1[x] as i16 + r4[x] as i16;
            let c = r2[x] as i16 + r3[x] as i16;
            d[x] = clip16((a - 5 * bb + 20 * c + 16) >> 5);
        }
    }
}

/// Centre half-sample `j`.
#[inline(always)]
fn hpel_c<const W: usize>(src: &[u8], o: usize, s: usize, h: usize, dst: &mut [u8], ds: usize, b1: &mut [i16; 16 * 21]) {
    for y in 0..h + 5 {
        let st = o + y * s - 2 * s - 2;
        let r = &src[st..st + W + 5];
        let t = &mut b1[y * 16..y * 16 + W];
        for x in 0..W {
            t[x] = tap6(r, x);
        }
    }
    for y in 0..h {
        let t0 = &b1[y * 16..y * 16 + W];
        let t1 = &b1[(y + 1) * 16..(y + 1) * 16 + W];
        let t2 = &b1[(y + 2) * 16..(y + 2) * 16 + W];
        let t3 = &b1[(y + 3) * 16..(y + 3) * 16 + W];
        let t4 = &b1[(y + 4) * 16..(y + 4) * 16 + W];
        let t5 = &b1[(y + 5) * 16..(y + 5) * 16 + W];
        let d = &mut dst[y * ds..y * ds + W];
        for x in 0..W {
            let a = t0[x] as i32 + t5[x] as i32;
            let b = t1[x] as i32 + t4[x] as i32;
            let c = t2[x] as i32 + t3[x] as i32;
            d[x] = clip((a - 5 * b + 20 * c + 512) >> 10);
        }
    }
}

#[inline(always)]
fn full<const W: usize>(src: &[u8], o: usize, s: usize, h: usize, dx: usize, dy: usize, dst: &mut [u8], ds: usize) {
    for y in 0..h {
        let r = o + (y + dy) * s + dx;
        dst[y * ds..y * ds + W].copy_from_slice(&src[r..r + W]);
    }
}

#[inline(always)]
fn avg_into<const W: usize>(dst: &mut [u8], ds: usize, b: &[u8; 256], h: usize) {
    for y in 0..h {
        let d = &mut dst[y * ds..y * ds + W];
        let s = &b[y * 16..y * 16 + W];
        for x in 0..W {
            d[x] = ((d[x] as u16 + s[x] as u16 + 1) >> 1) as u8;
        }
    }
}

#[inline(always)]
fn mc_luma_w<const W: usize>(src: &[u8], o: usize, s: usize, fx: usize, fy: usize, h: usize, dst: &mut [u8], ds: usize, t: &mut McTmp) {
    let McTmp { b, t: b1 } = t;
    match (fx, fy) {
        (0, 0) => full::<W>(src, o, s, h, 0, 0, dst, ds),
        (2, 0) => hpel_h::<W>(src, o, s, h, 0, dst, ds),
        (0, 2) => hpel_v::<W>(src, o, s, h, 0, dst, ds),
        (2, 2) => hpel_c::<W>(src, o, s, h, dst, ds, b1),
        (1, 0) | (3, 0) => {
            full::<W>(src, o, s, h, fx >> 1, 0, dst, ds);
            hpel_h::<W>(src, o, s, h, 0, b, 16);
            avg_into::<W>(dst, ds, b, h);
        }
        (0, 1) | (0, 3) => {
            full::<W>(src, o, s, h, 0, fy >> 1, dst, ds);
            hpel_v::<W>(src, o, s, h, 0, b, 16);
            avg_into::<W>(dst, ds, b, h);
        }
        (1, 2) | (3, 2) => {
            hpel_c::<W>(src, o, s, h, dst, ds, b1);
            hpel_v::<W>(src, o, s, h, fx >> 1, b, 16);
            avg_into::<W>(dst, ds, b, h);
        }
        (2, 1) | (2, 3) => {
            hpel_c::<W>(src, o, s, h, dst, ds, b1);
            hpel_h::<W>(src, o, s, h, fy >> 1, b, 16);
            avg_into::<W>(dst, ds, b, h);
        }
        _ => {
            // diagonal quarter positions e, g, p, r: b (row fy>>1) averaged with h (column fx>>1)
            hpel_h::<W>(src, o, s, h, fy >> 1, dst, ds);
            hpel_v::<W>(src, o, s, h, fx >> 1, b, 16);
            avg_into::<W>(dst, ds, b, h);
        }
    }
}

/// Scratch buffers of the luma interpolation (kept off the stack frame of every call).
pub struct McTmp {
    pub b: [u8; 256],
    pub t: [i16; 16 * 21],
}

/// Luma prediction of a w x h block at integer picture position (px, py) with motion vector mv (quarter pel).
/// Writes into `dst` (block origin at index 0) with stride `ds`. `w` is 4, 8 or 16; `h` <= 16.
pub fn mc_luma(r: &Planes, px: i32, py: i32, mv: [i16; 2], w: usize, h: usize, dst: &mut [u8], ds: usize, t: &mut McTmp) {
    let mvx = mv[0] as i32;
    let mvy = mv[1] as i32;
    let fx = (mvx & 3) as usize;
    let fy = (mvy & 3) as usize;
    let pw = r.w as i32;
    let ph = r.h as i32;
    let xi = (px + (mvx >> 2)).clamp(-(w as i32) - 3, pw + 1);
    let yi = (py + (mvy >> 2)).clamp(-(h as i32) - 3, ph + 1);
    let s = r.ys;
    let o = ((yi + PAD as i32) as usize) * s + (xi + PAD as i32) as usize;
    let src = &r.y[..];
    match w {
        16 => mc_luma_w::<16>(src, o, s, fx, fy, h, dst, ds, t),
        8 => mc_luma_w::<8>(src, o, s, fx, fy, h, dst, ds, t),
        _ => mc_luma_w::<4>(src, o, s, fx, fy, h, dst, ds, t),
    }
}

/// Hint the cache about the reference samples a later `mc_luma`/`mc_chroma` of this block reads,
/// so their memory latency overlaps with other work.
#[inline(always)]
pub fn prefetch(r: &Planes, px: i32, py: i32, mv: [i16; 2], w: usize, h: usize) {
    #[cfg(all(target_arch = "x86_64", target_feature = "sse"))]
    {
        use core::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
        let pf = |buf: &[u8], i: usize| {
            let i = i.min(buf.len().saturating_sub(1));
            // SAFETY: prefetching an in-bounds address has no side effects
            unsafe { _mm_prefetch::<_MM_HINT_T0>(buf.as_ptr().add(i) as *const i8) };
        };
        let (mvx, mvy) = (mv[0] as i32, mv[1] as i32);
        let xi = (px + (mvx >> 2)).clamp(-(w as i32) - 3, r.w as i32 + 1);
        let yi = (py + (mvy >> 2)).clamp(-(h as i32) - 3, r.h as i32 + 1);
        let s = r.ys;
        let o = ((yi + PAD as i32 - 2) as usize) * s + (xi + PAD as i32 - 2) as usize;
        for y in 0..h + 5 {
            pf(&r.y, o + y * s);
            if w > 8 {
                pf(&r.y, o + y * s + w + 4);
            }
        }
        let (cw, ch) = (w / 2, h / 2);
        let xc = (px / 2 + (mvx >> 3)).clamp(-(cw as i32), (r.w / 2) as i32 - 1);
        let yc = (py / 2 + (mvy >> 3)).clamp(-(ch as i32), (r.h / 2) as i32 - 1);
        let s = r.cs;
        let o = ((yc + PADC as i32) as usize) * s + (xc + PADC as i32) as usize;
        for y in 0..ch + 1 {
            pf(&r.u, o + y * s);
            pf(&r.v, o + y * s);
        }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "sse")))]
    let _ = (r, px, py, mv, w, h);
}

#[inline(always)]
fn mc_chroma_w<const W: usize>(plane: &[u8], base: usize, s: usize, h: usize, fx: u16, fy: u16, dst: &mut [u8], ds: usize) {
    if fx == 0 && fy == 0 {
        for y in 0..h {
            let o = base + y * s;
            dst[y * ds..y * ds + W].copy_from_slice(&plane[o..o + W]);
        }
        return;
    }
    let wa = (8 - fx) * (8 - fy);
    let wb = fx * (8 - fy);
    let wc = (8 - fx) * fy;
    let wd = fx * fy;
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        // 16-byte row loads (only W + 1 bytes used) stay inside the padded plane
        if W >= 4 && h >= 1 && h <= 8 && base + h * s + 16 <= plane.len() && (h - 1) * ds + W <= dst.len() {
            use core::arch::x86_64::*;
            // SAFETY: rows base + y*s .. +16 for y <= h are in bounds (checked above); dst rows
            // y*ds .. y*ds+W lie inside `dst` for y < h (checked above)
            unsafe {
                let z = _mm_setzero_si128();
                let p = plane.as_ptr().add(base);
                let row = |y: usize| {
                    let r = _mm_loadu_si128(p.add(y * s) as *const __m128i);
                    (_mm_unpacklo_epi8(r, z), _mm_unpacklo_epi8(_mm_srli_si128(r, 1), z))
                };
                let (wa, wb, wc, wd) = (_mm_set1_epi16(wa as i16), _mm_set1_epi16(wb as i16), _mm_set1_epi16(wc as i16), _mm_set1_epi16(wd as i16));
                let r32 = _mm_set1_epi16(32);
                let (mut a, mut b) = row(0);
                for y in 0..h {
                    let (c, d) = row(y + 1);
                    let v = _mm_add_epi16(_mm_add_epi16(_mm_mullo_epi16(a, wa), _mm_mullo_epi16(b, wb)), _mm_add_epi16(_mm_mullo_epi16(c, wc), _mm_mullo_epi16(d, wd)));
                    let v = _mm_srli_epi16(_mm_add_epi16(v, r32), 6);
                    let px = _mm_packus_epi16(v, v);
                    let dp = dst.as_mut_ptr().add(y * ds);
                    if W == 8 {
                        _mm_storel_epi64(dp as *mut __m128i, px);
                    } else {
                        (dp as *mut i32).write_unaligned(_mm_cvtsi128_si32(px));
                    }
                    a = c;
                    b = d;
                }
            }
            return;
        }
    }
    for y in 0..h {
        let o = base + y * s;
        let r0 = &plane[o..o + W + 1];
        let r1 = &plane[o + s..o + s + W + 1];
        let d = &mut dst[y * ds..y * ds + W];
        for x in 0..W {
            let v = wa * r0[x] as u16 + wb * r0[x + 1] as u16 + wc * r1[x] as u16 + wd * r1[x + 1] as u16;
            d[x] = ((v + 32) >> 6) as u8;
        }
    }
}

/// Chroma prediction (4:2:0) of a w x h block at chroma position (px, py), luma mv (1/8 chroma pel).
/// Writes `dst` (block origin at index 0) with stride `ds`. `w` is 2, 4 or 8; `h` <= 8.
pub fn mc_chroma(plane: &[u8], r: &Planes, px: i32, py: i32, mv: [i16; 2], w: usize, h: usize, dst: &mut [u8], ds: usize) {
    let mvx = mv[0] as i32;
    let mvy = mv[1] as i32;
    let fx = (mvx & 7) as u16;
    let fy = (mvy & 7) as u16;
    let cw = (r.w / 2) as i32;
    let ch = (r.h / 2) as i32;
    let xi = (px + (mvx >> 3)).clamp(-(w as i32), cw - 1);
    let yi = (py + (mvy >> 3)).clamp(-(h as i32), ch - 1);
    let s = r.cs;
    let base = ((yi + PADC as i32) as usize) * s + (xi + PADC as i32) as usize;
    match w {
        8 => mc_chroma_w::<8>(plane, base, s, h, fx, fy, dst, ds),
        4 => mc_chroma_w::<4>(plane, base, s, h, fx, fy, dst, ds),
        _ => mc_chroma_w::<2>(plane, base, s, h, fx, fy, dst, ds),
    }
}

/// Weighting parameters for one block: (logWD, w0, w1, o0, o1) per component (Y, Cb, Cr).
#[derive(Clone, Copy)]
pub struct Wp {
    pub log_wd: [i32; 3],
    pub w: [[i32; 2]; 3],
    pub o: [[i32; 2]; 3],
}

/// Combine predictions: `p0`/`p1` present per list. Explicit/implicit weights when `wp` is Some.
/// `w` is 2, 4, 8 or 16.
pub fn weight(
    comp: usize,
    p0: Option<&[u8]>,
    p1: Option<&[u8]>,
    sstride: usize,
    w: usize,
    h: usize,
    wp: Option<&Wp>,
    dst: &mut [u8],
    doff: usize,
    dstride: usize,
) {
    match w {
        16 => weight_w::<16>(comp, p0, p1, sstride, h, wp, dst, doff, dstride),
        8 => weight_w::<8>(comp, p0, p1, sstride, h, wp, dst, doff, dstride),
        4 => weight_w::<4>(comp, p0, p1, sstride, h, wp, dst, doff, dstride),
        _ => weight_w::<2>(comp, p0, p1, sstride, h, wp, dst, doff, dstride),
    }
}

#[inline(always)]
fn weight_w<const W: usize>(
    comp: usize,
    p0: Option<&[u8]>,
    p1: Option<&[u8]>,
    sstride: usize,
    h: usize,
    wp: Option<&Wp>,
    dst: &mut [u8],
    doff: usize,
    dstride: usize,
) {
    let w = W;
    match (p0, p1, wp) {
        (Some(a), Some(b), None) => {
            for y in 0..h {
                let d = &mut dst[doff + y * dstride..doff + y * dstride + w];
                let ra = &a[y * sstride..y * sstride + w];
                let rb = &b[y * sstride..y * sstride + w];
                for ((d, &a), &b) in d.iter_mut().zip(ra).zip(rb) {
                    *d = ((a as u16 + b as u16 + 1) >> 1) as u8;
                }
            }
        }
        (Some(a), None, None) | (None, Some(a), None) => {
            for y in 0..h {
                dst[doff + y * dstride..doff + y * dstride + w].copy_from_slice(&a[y * sstride..y * sstride + w]);
            }
        }
        (Some(a), Some(b), Some(wp)) => {
            let lwd = wp.log_wd[comp];
            let (w0, w1) = (wp.w[comp][0], wp.w[comp][1]);
            let o = (wp.o[comp][0] + wp.o[comp][1] + 1) >> 1;
            let rnd = 1 << lwd;
            for y in 0..h {
                let d = &mut dst[doff + y * dstride..doff + y * dstride + w];
                let ra = &a[y * sstride..y * sstride + w];
                let rb = &b[y * sstride..y * sstride + w];
                for ((d, &a), &b) in d.iter_mut().zip(ra).zip(rb) {
                    *d = clip(((a as i32 * w0 + b as i32 * w1 + rnd) >> (lwd + 1)) + o);
                }
            }
        }
        (Some(a), None, Some(wp)) | (None, Some(a), Some(wp)) => {
            let li = if p0.is_some() { 0 } else { 1 };
            let lwd = wp.log_wd[comp];
            let (wt, o) = (wp.w[comp][li], wp.o[comp][li]);
            for y in 0..h {
                let d = &mut dst[doff + y * dstride..doff + y * dstride + w];
                let ra = &a[y * sstride..y * sstride + w];
                if lwd >= 1 {
                    let rnd = 1 << (lwd - 1);
                    for (d, &p) in d.iter_mut().zip(ra) {
                        *d = clip(((p as i32 * wt + rnd) >> lwd) + o);
                    }
                } else {
                    for (d, &p) in d.iter_mut().zip(ra) {
                        *d = clip(p as i32 * wt + o);
                    }
                }
            }
        }
        (None, None, _) => {}
    }
}
