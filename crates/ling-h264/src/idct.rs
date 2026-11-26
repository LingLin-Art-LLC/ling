//! Inverse transforms (8.5.10 - 8.5.13). Coefficients are already scaled (dequantised).

#[inline(always)]
fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// 4x4 inverse transform of `c` (raster) added to `dst`. `c` is cleared.
pub fn idct4_add(c: &mut [i32], dst: &mut [u8], off: usize, stride: usize) {
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        if c.len() >= 16 && off + 3 * stride + 4 <= dst.len() {
            // SAFETY: 16 coefficients and the 4x4 destination window are in bounds (checked above)
            unsafe { sse2::idct4_add(c.as_mut_ptr(), dst.as_mut_ptr().add(off), stride) };
            return;
        }
    }
    let mut t = [0i32; 16];
    for i in 0..4 {
        let d = &c[i * 4..i * 4 + 4];
        let e0 = d[0].wrapping_add(d[2]);
        let e1 = d[0].wrapping_sub(d[2]);
        let e2 = (d[1] >> 1).wrapping_sub(d[3]);
        let e3 = d[1].wrapping_add(d[3] >> 1);
        t[i * 4] = e0.wrapping_add(e3);
        t[i * 4 + 1] = e1.wrapping_add(e2);
        t[i * 4 + 2] = e1.wrapping_sub(e2);
        t[i * 4 + 3] = e0.wrapping_sub(e3);
    }
    for j in 0..4 {
        let (d0, d1, d2, d3) = (t[j], t[4 + j], t[8 + j], t[12 + j]);
        let e0 = d0.wrapping_add(d2);
        let e1 = d0.wrapping_sub(d2);
        let e2 = (d1 >> 1).wrapping_sub(d3);
        let e3 = d1.wrapping_add(d3 >> 1);
        let r = [e0.wrapping_add(e3), e1.wrapping_add(e2), e1.wrapping_sub(e2), e0.wrapping_sub(e3)];
        for i in 0..4 {
            let o = off + i * stride + j;
            dst[o] = clip(dst[o] as i32 + (r[i].wrapping_add(32) >> 6));
        }
    }
    c[..16].fill(0);
}

/// 8x8 inverse transform of `c` (raster) added to `dst`. `c` is cleared.
pub fn idct8_add(c: &mut [i32], dst: &mut [u8], off: usize, stride: usize) {
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        if c.len() >= 64 && off + 7 * stride + 8 <= dst.len() {
            // SAFETY: 64 coefficients and the 8x8 destination window are in bounds (checked above)
            unsafe { sse2::idct8_add(c.as_mut_ptr(), dst.as_mut_ptr().add(off), stride) };
            return;
        }
    }
    fn tr(d: [i32; 8]) -> [i32; 8] {
        let w = |a: i32, b: i32| a.wrapping_add(b);
        let s = |a: i32, b: i32| a.wrapping_sub(b);
        let e0 = w(d[0], d[4]);
        let e1 = s(s(w(-d[3], d[5]), d[7]), d[7] >> 1);
        let e2 = s(d[0], d[4]);
        let e3 = s(s(w(d[1], d[7]), d[3]), d[3] >> 1);
        let e4 = s(d[2] >> 1, d[6]);
        let e5 = w(w(s(d[7], d[1]), d[5]), d[5] >> 1);
        let e6 = w(d[2], d[6] >> 1);
        let e7 = w(w(w(d[3], d[5]), d[1]), d[1] >> 1);
        let f0 = w(e0, e6);
        let f1 = w(e1, e7 >> 2);
        let f2 = w(e2, e4);
        let f3 = w(e3, e5 >> 2);
        let f4 = s(e2, e4);
        let f5 = s(e3 >> 2, e5);
        let f6 = s(e0, e6);
        let f7 = s(e7, e1 >> 2);
        [w(f0, f7), w(f2, f5), w(f4, f3), w(f6, f1), s(f6, f1), s(f4, f3), s(f2, f5), s(f0, f7)]
    }
    let mut t = [0i32; 64];
    for i in 0..8 {
        let mut d = [0i32; 8];
        d.copy_from_slice(&c[i * 8..i * 8 + 8]);
        let r = tr(d);
        t[i * 8..i * 8 + 8].copy_from_slice(&r);
    }
    for j in 0..8 {
        let mut d = [0i32; 8];
        for i in 0..8 {
            d[i] = t[i * 8 + j];
        }
        let r = tr(d);
        for i in 0..8 {
            let o = off + i * stride + j;
            dst[o] = clip(dst[o] as i32 + (r[i].wrapping_add(32) >> 6));
        }
    }
    c[..64].fill(0);
}

/// Inverse Hadamard of the 16 Intra16x16 DC levels (raster 4x4 of blocks) and scaling (8-326..8-328).
/// `dq` = LevelScale4x4(qp%6, 0, 0) << (qp/6). Output dc values in raster block order.
pub fn luma_dc(c: &[i32; 16], dq: i32) -> [i32; 16] {
    let mut t = [0i32; 16];
    for i in 0..4 {
        let (a, b, cc, d) = (c[i * 4], c[i * 4 + 1], c[i * 4 + 2], c[i * 4 + 3]);
        let e0 = a.wrapping_add(b);
        let e1 = a.wrapping_sub(b);
        let e2 = cc.wrapping_sub(d);
        let e3 = cc.wrapping_add(d);
        t[i * 4] = e0.wrapping_add(e3);
        t[i * 4 + 1] = e0.wrapping_sub(e3);
        t[i * 4 + 2] = e1.wrapping_sub(e2);
        t[i * 4 + 3] = e1.wrapping_add(e2);
    }
    let mut out = [0i32; 16];
    for j in 0..4 {
        let (a, b, cc, d) = (t[j], t[4 + j], t[8 + j], t[12 + j]);
        let e0 = a.wrapping_add(b);
        let e1 = a.wrapping_sub(b);
        let e2 = cc.wrapping_sub(d);
        let e3 = cc.wrapping_add(d);
        let f = [e0.wrapping_add(e3), e0.wrapping_sub(e3), e1.wrapping_sub(e2), e1.wrapping_add(e2)];
        for i in 0..4 {
            out[i * 4 + j] = f[i].wrapping_mul(dq).wrapping_add(32) >> 6;
        }
    }
    out
}

/// 2x2 chroma DC transform + scaling (8-329, 8-330). `dq` = LevelScale4x4(qpc%6,0,0) << (qpc/6).
pub fn chroma_dc(c: &[i32; 4], dq: i32) -> [i32; 4] {
    let f0 = c[0].wrapping_add(c[1]).wrapping_add(c[2]).wrapping_add(c[3]);
    let f1 = c[0].wrapping_sub(c[1]).wrapping_add(c[2]).wrapping_sub(c[3]);
    let f2 = c[0].wrapping_add(c[1]).wrapping_sub(c[2]).wrapping_sub(c[3]);
    let f3 = c[0].wrapping_sub(c[1]).wrapping_sub(c[2]).wrapping_add(c[3]);
    [f0.wrapping_mul(dq) >> 5, f1.wrapping_mul(dq) >> 5, f2.wrapping_mul(dq) >> 5, f3.wrapping_mul(dq) >> 5]
}

/// SSE2 versions of the inverse transforms (32-bit lanes, wrapping arithmetic: identical results
/// to the scalar code), with a DC-only shortcut.
#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
mod sse2 {
    use core::arch::x86_64::*;

    #[inline(always)]
    unsafe fn tr4x4(a: [__m128i; 4]) -> [__m128i; 4] {
        let t0 = _mm_unpacklo_epi32(a[0], a[1]);
        let t1 = _mm_unpacklo_epi32(a[2], a[3]);
        let t2 = _mm_unpackhi_epi32(a[0], a[1]);
        let t3 = _mm_unpackhi_epi32(a[2], a[3]);
        [_mm_unpacklo_epi64(t0, t1), _mm_unpackhi_epi64(t0, t1), _mm_unpacklo_epi64(t2, t3), _mm_unpackhi_epi64(t2, t3)]
    }

    #[inline(always)]
    unsafe fn tr4(d: [__m128i; 4]) -> [__m128i; 4] {
        let e0 = _mm_add_epi32(d[0], d[2]);
        let e1 = _mm_sub_epi32(d[0], d[2]);
        let e2 = _mm_sub_epi32(_mm_srai_epi32(d[1], 1), d[3]);
        let e3 = _mm_add_epi32(d[1], _mm_srai_epi32(d[3], 1));
        [_mm_add_epi32(e0, e3), _mm_add_epi32(e1, e2), _mm_sub_epi32(e1, e2), _mm_sub_epi32(e0, e3)]
    }

    #[inline(always)]
    unsafe fn tr8(d: [__m128i; 8]) -> [__m128i; 8] {
        let a = |x, y| _mm_add_epi32(x, y);
        let s = |x, y| _mm_sub_epi32(x, y);
        let h = |x| _mm_srai_epi32(x, 1);
        let q = |x| _mm_srai_epi32(x, 2);
        let e0 = a(d[0], d[4]);
        let e1 = s(s(s(d[5], d[3]), d[7]), h(d[7]));
        let e2 = s(d[0], d[4]);
        let e3 = s(s(a(d[1], d[7]), d[3]), h(d[3]));
        let e4 = s(h(d[2]), d[6]);
        let e5 = a(a(s(d[7], d[1]), d[5]), h(d[5]));
        let e6 = a(d[2], h(d[6]));
        let e7 = a(a(a(d[3], d[5]), d[1]), h(d[1]));
        let f0 = a(e0, e6);
        let f1 = a(e1, q(e7));
        let f2 = a(e2, e4);
        let f3 = a(e3, q(e5));
        let f4 = s(e2, e4);
        let f5 = s(q(e3), e5);
        let f6 = s(e0, e6);
        let f7 = s(e7, q(e1));
        [a(f0, f7), a(f2, f5), a(f4, f3), a(f6, f1), s(f6, f1), s(f4, f3), s(f2, f5), s(f0, f7)]
    }

    /// dst row (4 pixels) += v (8 x i16 lanes: low 4 = row 0, high 4 = row 1), two rows
    #[inline(always)]
    unsafe fn add2x4(dst: *mut u8, stride: usize, v: __m128i) {
        let z = _mm_setzero_si128();
        let r0 = _mm_cvtsi32_si128((dst as *const i32).read_unaligned());
        let r1 = _mm_cvtsi32_si128((dst.add(stride) as *const i32).read_unaligned());
        let p = _mm_unpacklo_epi8(_mm_unpacklo_epi32(r0, r1), z);
        let o = _mm_packus_epi16(_mm_adds_epi16(p, v), z);
        (dst as *mut i32).write_unaligned(_mm_cvtsi128_si32(o));
        (dst.add(stride) as *mut i32).write_unaligned(_mm_cvtsi128_si32(_mm_srli_si128(o, 4)));
    }

    /// dst row (8 pixels) += v (8 x i16)
    #[inline(always)]
    unsafe fn add8(dst: *mut u8, v: __m128i) {
        let z = _mm_setzero_si128();
        let p = _mm_unpacklo_epi8(_mm_loadl_epi64(dst as *const __m128i), z);
        _mm_storel_epi64(dst as *mut __m128i, _mm_packus_epi16(_mm_adds_epi16(p, v), z));
    }

    #[inline(always)]
    unsafe fn round6(x: __m128i) -> __m128i {
        _mm_srai_epi32(_mm_add_epi32(x, _mm_set1_epi32(32)), 6)
    }

    /// Only the DC coefficient non-zero? (`v` = all coefficient vectors, v[0] starts with DC)
    #[inline(always)]
    unsafe fn dc_only(v: &[__m128i]) -> bool {
        let mut acc = _mm_and_si128(v[0], _mm_set_epi32(-1, -1, -1, 0));
        for x in &v[1..] {
            acc = _mm_or_si128(acc, *x);
        }
        _mm_movemask_epi8(_mm_cmpeq_epi32(acc, _mm_setzero_si128())) == 0xFFFF
    }

    pub unsafe fn idct4_add(c: *mut i32, dst: *mut u8, stride: usize) {
        let ld = |i: usize| _mm_loadu_si128(c.add(i * 4) as *const __m128i);
        let r = [ld(0), ld(1), ld(2), ld(3)];
        if dc_only(&r) {
            let v = _mm_set1_epi16(((*c).wrapping_add(32) >> 6).clamp(-32768, 32767) as i16);
            add2x4(dst, stride, v);
            add2x4(dst.add(2 * stride), stride, v);
            *c = 0;
            return;
        }
        let t = tr4(tr4x4(r));
        let o = tr4(tr4x4(t));
        add2x4(dst, stride, _mm_packs_epi32(round6(o[0]), round6(o[1])));
        add2x4(dst.add(2 * stride), stride, _mm_packs_epi32(round6(o[2]), round6(o[3])));
        let z = _mm_setzero_si128();
        for i in 0..4 {
            _mm_storeu_si128(c.add(i * 4) as *mut __m128i, z);
        }
    }

    pub unsafe fn idct8_add(c: *mut i32, dst: *mut u8, stride: usize) {
        // rr[r * 2 + h] = row r, columns 4h..4h+3
        let rr: [__m128i; 16] = core::array::from_fn(|i| _mm_loadu_si128(c.add(i * 4) as *const __m128i));
        let z = _mm_setzero_si128();
        for i in 0..16 {
            _mm_storeu_si128(c.add(i * 4) as *mut __m128i, z);
        }
        if dc_only(&rr) {
            let v = _mm_set1_epi16((_mm_cvtsi128_si32(rr[0]).wrapping_add(32) >> 6).clamp(-32768, 32767) as i16);
            for i in 0..8 {
                add8(dst.add(i * stride), v);
            }
            return;
        }
        // horizontal pass, 4 rows per vector lane group: t[g][k] lane i = row 4g+i, output column k
        let mut t = [[z; 8]; 2];
        for g in 0..2 {
            let mut v = [z; 8];
            for cb in 0..2 {
                let b = tr4x4([rr[(4 * g) * 2 + cb], rr[(4 * g + 1) * 2 + cb], rr[(4 * g + 2) * 2 + cb], rr[(4 * g + 3) * 2 + cb]]);
                v[4 * cb..4 * cb + 4].copy_from_slice(&b);
            }
            t[g] = tr8(v);
        }
        // vertical pass: out[h][i] lane m = row i, column 4h+m
        let mut out = [[z; 8]; 2];
        for h in 0..2 {
            let mut w = [z; 8];
            for g in 0..2 {
                let b = tr4x4([t[g][4 * h], t[g][4 * h + 1], t[g][4 * h + 2], t[g][4 * h + 3]]);
                w[4 * g..4 * g + 4].copy_from_slice(&b);
            }
            out[h] = tr8(w);
        }
        for i in 0..8 {
            add8(dst.add(i * stride), _mm_packs_epi32(round6(out[0][i]), round6(out[1][i])));
        }
    }
}
