//! YUV 4:2:0 -> RGB conversion.

use crate::Frame;
use alloc::vec::Vec;

/// Convert a frame to 32-bit pixels `0xAARRGGBB` (alpha = 0xFF; the LingOS framebuffer layout,
/// i.e. B, G, R, A bytes in memory). BT.601 limited range by default; BT.709 and/or full range
/// when the stream's VUI says so. When `dst_w x dst_h` differs from the frame size the picture is
/// scaled (nearest neighbour) to fill the destination. `dst` must hold `dst_w * dst_h` pixels.
pub fn frame_to_rgba(f: &Frame, dst: &mut [u32], dst_w: usize, dst_h: usize) {
    let (fw, fh) = (f.width as usize, f.height as usize);
    if fw == 0 || fh == 0 || dst_w == 0 || dst_h == 0 || dst.len() < dst_w * dst_h {
        return;
    }
    // coefficients in 1/65536 units: (y_scale, y_off, rv, gu, gv, bu)
    let (ys, yo, rv, gu, gv, bu): (i32, i32, i32, i32, i32, i32) = match (f.bt709, f.full_range) {
        (false, false) => (76309, 16, 104597, 25675, 53279, 132201),
        (true, false) => (76309, 16, 117489, 13975, 34925, 138438),
        (false, true) => (65536, 0, 91881, 22554, 46802, 116130),
        (true, true) => (65536, 0, 103206, 12276, 30679, 121609),
    };
    let cw = fw.div_ceil(2);
    let (ystr, cstr) = (f.y_stride, f.c_stride);
    if dst_w == fw
        && dst_h == fh
        && f.y.len() >= (fh - 1) * ystr + fw
        && f.u.len() >= ((fh - 1) / 2) * cstr + cw
        && f.v.len() >= ((fh - 1) / 2) * cstr + cw
    {
        // unscaled: one chroma evaluation per pair of pixels, rows as slices (no index checks)
        for y in 0..fh {
            let yrow = &f.y[y * ystr..y * ystr + fw];
            let c0 = (y / 2) * cstr;
            let urow = &f.u[c0..c0 + cw];
            let vrow = &f.v[c0..c0 + cw];
            let out = &mut dst[y * fw..y * fw + fw];
            #[allow(unused_mut, unused_assignments)]
            let mut done = 0;
            #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
            {
                // SAFETY: the row slices hold fw luma / cw chroma samples and fw output pixels
                done = unsafe { sse2::row(yrow, urow, vrow, out, [ys, yo, rv, gu, gv, bu]) };
            }
            let (out, yrow) = (&mut out[done..], &yrow[done..]);
            let (urow, vrow) = (&urow[done / 2..], &vrow[done / 2..]);
            for (((o2, y2), &u), &v) in out.chunks_mut(2).zip(yrow.chunks(2)).zip(urow).zip(vrow) {
                let u = u as i32 - 128;
                let v = v as i32 - 128;
                let cr = rv * v + 32768;
                let cg = 32768 - gu * u - gv * v;
                let cb = bu * u + 32768;
                for (o, &l) in o2.iter_mut().zip(y2) {
                    let l = (l as i32 - yo) * ys;
                    let r = ((l + cr) >> 16).clamp(0, 255) as u32;
                    let g = ((l + cg) >> 16).clamp(0, 255) as u32;
                    let b = ((l + cb) >> 16).clamp(0, 255) as u32;
                    *o = 0xFF00_0000 | (r << 16) | (g << 8) | b;
                }
            }
        }
        return;
    }
    let xmap: Vec<usize> = (0..dst_w).map(|x| (x * fw / dst_w).min(fw - 1)).collect();
    for dy in 0..dst_h {
        let sy = (dy * fh / dst_h).min(fh - 1);
        let yrow = &f.y[sy * f.y_stride..];
        let crow = (sy / 2) * f.c_stride;
        let out = &mut dst[dy * dst_w..dy * dst_w + dst_w];
        for (dx, o) in out.iter_mut().enumerate() {
            let sx = xmap[dx];
            let y = (yrow[sx] as i32 - yo) * ys;
            let u = f.u[crow + sx / 2] as i32 - 128;
            let v = f.v[crow + sx / 2] as i32 - 128;
            let r = ((y + rv * v + 32768) >> 16).clamp(0, 255) as u32;
            let g = ((y - gu * u - gv * v + 32768) >> 16).clamp(0, 255) as u32;
            let b = ((y + bu * u + 32768) >> 16).clamp(0, 255) as u32;
            *o = 0xFF00_0000 | (r << 16) | (g << 8) | b;
        }
    }
}

/// SSE2 row conversion, 8 pixels per step. The products need more than 16 bits, so each
/// coefficient c is split as lo + k * hi (k a power of two, hi < 2^15) and applied with
/// `pmaddwd` on (x, k * x) pairs: exact 32-bit results, identical to the scalar code.
#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
mod sse2 {
    use core::arch::x86_64::*;

    /// (k, hi << 16 | lo) with c = lo + k * hi
    fn split(c: i32) -> (i16, i32) {
        let mut k = 1;
        while c / k > 32767 {
            k *= 2;
        }
        let hi = c / k;
        let lo = c - hi * k;
        (k as i16, (hi << 16) | (lo & 0xFFFF))
    }

    struct Coef {
        k: __m128i,
        lohi: __m128i,
    }

    impl Coef {
        unsafe fn new(c: i32) -> Coef {
            let (k, lohi) = split(c);
            Coef { k: _mm_set1_epi16(k), lohi: _mm_set1_epi32(lohi) }
        }
        /// x (8 x i16) * c -> (lanes 0..4, lanes 4..8) as i32
        #[inline(always)]
        unsafe fn mul(&self, x: __m128i) -> (__m128i, __m128i) {
            let xk = _mm_mullo_epi16(x, self.k);
            (_mm_madd_epi16(_mm_unpacklo_epi16(x, xk), self.lohi), _mm_madd_epi16(_mm_unpackhi_epi16(x, xk), self.lohi))
        }
    }

    /// Converts the leading multiple of 8 pixels; returns how many were done.
    pub unsafe fn row(y: &[u8], u: &[u8], v: &[u8], out: &mut [u32], c: [i32; 6]) -> usize {
        let [ys, yo, rv, gu, gv, bu] = c;
        let n = out.len().min(y.len()).min(u.len() * 2).min(v.len() * 2) & !7;
        let (cy, cr, cgu, cgv, cb) = (Coef::new(ys), Coef::new(rv), Coef::new(gu), Coef::new(gv), Coef::new(bu));
        let z = _mm_setzero_si128();
        let yo = _mm_set1_epi16(yo as i16);
        let c128 = _mm_set1_epi16(128);
        let rnd = _mm_set1_epi32(32768);
        let alpha = _mm_set1_epi8(-1);
        let mut x = 0;
        while x < n {
            let l = _mm_sub_epi16(_mm_unpacklo_epi8(_mm_loadl_epi64(y.as_ptr().add(x) as *const __m128i), z), yo);
            let uu = _mm_cvtsi32_si128((u.as_ptr().add(x / 2) as *const i32).read_unaligned());
            let vv = _mm_cvtsi32_si128((v.as_ptr().add(x / 2) as *const i32).read_unaligned());
            let uu = _mm_sub_epi16(_mm_unpacklo_epi8(_mm_unpacklo_epi8(uu, uu), z), c128);
            let vv = _mm_sub_epi16(_mm_unpacklo_epi8(_mm_unpacklo_epi8(vv, vv), z), c128);
            let (l0, l1) = cy.mul(l);
            let (l0, l1) = (_mm_add_epi32(l0, rnd), _mm_add_epi32(l1, rnd));
            let (r0, r1) = cr.mul(vv);
            let (b0, b1) = cb.mul(uu);
            let (gu0, gu1) = cgu.mul(uu);
            let (gv0, gv1) = cgv.mul(vv);
            let ch = |a: __m128i, b: __m128i| _mm_packs_epi32(_mm_srai_epi32(a, 16), _mm_srai_epi32(b, 16));
            let r = ch(_mm_add_epi32(l0, r0), _mm_add_epi32(l1, r1));
            let g = ch(_mm_sub_epi32(_mm_sub_epi32(l0, gu0), gv0), _mm_sub_epi32(_mm_sub_epi32(l1, gu1), gv1));
            let b = ch(_mm_add_epi32(l0, b0), _mm_add_epi32(l1, b1));
            let (r, g, b) = (_mm_packus_epi16(r, r), _mm_packus_epi16(g, g), _mm_packus_epi16(b, b));
            let bg = _mm_unpacklo_epi8(b, g);
            let ra = _mm_unpacklo_epi8(r, alpha);
            let o = out.as_mut_ptr().add(x) as *mut __m128i;
            _mm_storeu_si128(o, _mm_unpacklo_epi16(bg, ra));
            _mm_storeu_si128(o.add(1), _mm_unpackhi_epi16(bg, ra));
            x += 8;
        }
        n
    }
}
