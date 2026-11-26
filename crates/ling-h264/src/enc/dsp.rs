//! Encoder-side transforms, quantisation and block metrics.

/// Quantisation multipliers MF(QP % 6, position class) for the 4x4 core
/// transform (class 0: both coordinates even, 1: both odd, 2: mixed).
const MF: [[i32; 3]; 6] = [
    [13107, 5243, 8066],
    [11916, 4660, 7490],
    [10082, 4194, 6554],
    [9362, 3647, 5825],
    [8192, 3355, 5243],
    [7282, 2893, 4559],
];

/// The largest level CAVLC can carry in Baseline/Main (level_prefix <= 15).
pub const MAX_LEVEL: i32 = 2063;

#[inline(always)]
pub fn class(p: usize) -> usize {
    let (i, j) = (p >> 2, p & 3);
    if i % 2 == 0 && j % 2 == 0 {
        0
    } else if i % 2 == 1 && j % 2 == 1 {
        1
    } else {
        2
    }
}

/// Forward 4x4 core transform (raster in, raster out).
#[inline]
pub fn fdct4(d: &[i32; 16]) -> [i32; 16] {
    let mut t = [0i32; 16];
    for i in 0..4 {
        let r = &d[i * 4..i * 4 + 4];
        let s03 = r[0] + r[3];
        let d03 = r[0] - r[3];
        let s12 = r[1] + r[2];
        let d12 = r[1] - r[2];
        t[i * 4] = s03 + s12;
        t[i * 4 + 1] = 2 * d03 + d12;
        t[i * 4 + 2] = s03 - s12;
        t[i * 4 + 3] = d03 - 2 * d12;
    }
    let mut o = [0i32; 16];
    for j in 0..4 {
        let (a, b, c, e) = (t[j], t[4 + j], t[8 + j], t[12 + j]);
        let s03 = a + e;
        let d03 = a - e;
        let s12 = b + c;
        let d12 = b - c;
        o[j] = s03 + s12;
        o[4 + j] = 2 * d03 + d12;
        o[8 + j] = s03 - s12;
        o[12 + j] = d03 - 2 * d12;
    }
    o
}

/// Quantise raster coefficients `w[start..]` (start 1 skips the DC) into
/// `out`. Returns the number of non-zero levels.
#[inline]
pub fn quant4(w: &[i32; 16], out: &mut [i32; 16], qp: i32, intra: bool, start: usize) -> u32 {
    let qm = &MF[(qp % 6) as usize];
    let qb = 15 + qp / 6;
    let f = (1i32 << qb) / if intra { 3 } else { 6 };
    let mut nz = 0;
    out[0] = 0;
    for p in start..16 {
        let a = w[p].abs();
        let l = (((a as i64 * qm[class(p)] as i64) + f as i64) >> qb) as i32;
        let l = l.min(MAX_LEVEL);
        out[p] = if w[p] < 0 { -l } else { l };
        nz += (l != 0) as u32;
    }
    nz
}

/// Quantise a DC level already transformed (luma 16x16 DC after the halved
/// Hadamard, or chroma DC after the 2x2 transform): (|y| MF0 + 2f) >> (qbits + 1).
#[inline]
pub fn quant_dc(y: i32, qp: i32, intra: bool) -> i32 {
    let qb = 15 + qp / 6;
    let f = (1i64 << qb) / if intra { 3 } else { 6 };
    let l = (((y.abs() as i64 * MF[(qp % 6) as usize][0] as i64) + 2 * f) >> (qb + 1)) as i32;
    let l = l.min(MAX_LEVEL);
    if y < 0 {
        -l
    } else {
        l
    }
}

/// Forward 4x4 Hadamard of the 16 luma DC coefficients (raster of blocks), halved.
pub fn luma_dc_fwd(c: &[i32; 16]) -> [i32; 16] {
    let mut t = [0i32; 16];
    for i in 0..4 {
        let (a, b, cc, d) = (c[i * 4], c[i * 4 + 1], c[i * 4 + 2], c[i * 4 + 3]);
        t[i * 4] = a + b + cc + d;
        t[i * 4 + 1] = a + b - cc - d;
        t[i * 4 + 2] = a - b - cc + d;
        t[i * 4 + 3] = a - b + cc - d;
    }
    let mut o = [0i32; 16];
    for j in 0..4 {
        let (a, b, cc, d) = (t[j], t[4 + j], t[8 + j], t[12 + j]);
        o[j] = (a + b + cc + d) >> 1;
        o[4 + j] = (a + b - cc - d) >> 1;
        o[8 + j] = (a - b - cc + d) >> 1;
        o[12 + j] = (a - b + cc - d) >> 1;
    }
    o
}

/// 2x2 chroma DC transform (raster in and out).
#[inline]
pub fn chroma_dc_fwd(c: &[i32; 4]) -> [i32; 4] {
    [c[0] + c[1] + c[2] + c[3], c[0] - c[1] + c[2] - c[3], c[0] + c[1] - c[2] - c[3], c[0] - c[1] - c[2] + c[3]]
}

/// Dequantised AC coefficient (flat scaling matrices): level * normAdjust << qp/6.
#[inline(always)]
pub fn dequant(level: i32, qp: i32, p: usize) -> i32 {
    level * crate::tables::norm4((qp % 6) as usize, p) as i32 * (1 << (qp / 6))
}

/// LevelScale4x4(qp % 6, 0, 0) << (qp / 6) with the flat matrix (16), as the decoder's DC paths expect.
#[inline(always)]
pub fn dc_scale(qp: i32) -> i32 {
    (16 * crate::tables::norm4((qp % 6) as usize, 0) as i32) << (qp / 6)
}

/// Sum of absolute Hadamard-transformed differences of a 4x4 block (halved).
#[inline]
pub fn satd4(d: &[i32; 16]) -> u32 {
    let mut t = [0i32; 16];
    for i in 0..4 {
        let (a, b, c, e) = (d[i * 4], d[i * 4 + 1], d[i * 4 + 2], d[i * 4 + 3]);
        let s0 = a + b;
        let s1 = a - b;
        let s2 = c + e;
        let s3 = c - e;
        t[i * 4] = s0 + s2;
        t[i * 4 + 1] = s1 + s3;
        t[i * 4 + 2] = s0 - s2;
        t[i * 4 + 3] = s1 - s3;
    }
    let mut s = 0u32;
    for j in 0..4 {
        let (a, b, c, e) = (t[j], t[4 + j], t[8 + j], t[12 + j]);
        let s0 = a + b;
        let s1 = a - b;
        let s2 = c + e;
        let s3 = c - e;
        s += ((s0 + s2).unsigned_abs()) + ((s1 + s3).unsigned_abs()) + ((s0 - s2).unsigned_abs()) + ((s1 - s3).unsigned_abs());
    }
    s >> 1
}

/// SATD of a `w` x `h` area (multiples of 4) between `a` (stride `as_`) and `b` (stride `bs`).
pub fn satd(a: &[u8], ao: usize, as_: usize, b: &[u8], bo: usize, bs: usize, w: usize, h: usize) -> u32 {
    let mut s = 0;
    let mut d = [0i32; 16];
    for by in (0..h).step_by(4) {
        for bx in (0..w).step_by(4) {
            for y in 0..4 {
                let ra = ao + (by + y) * as_ + bx;
                let rb = bo + (by + y) * bs + bx;
                for x in 0..4 {
                    d[y * 4 + x] = a[ra + x] as i32 - b[rb + x] as i32;
                }
            }
            s += satd4(&d);
        }
    }
    s
}

/// SAD of a 16x16 block.
#[inline]
pub fn sad16(a: &[u8], ao: usize, as_: usize, b: &[u8], bo: usize, bs: usize) -> u32 {
    assert!(ao + 15 * as_ + 16 <= a.len() && bo + 15 * bs + 16 <= b.len());
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    unsafe {
        use core::arch::x86_64::*;
        let mut acc = _mm_setzero_si128();
        let (pa, pb) = (a.as_ptr().add(ao), b.as_ptr().add(bo));
        for r in 0..16 {
            let x = _mm_loadu_si128(pa.add(r * as_) as *const __m128i);
            let y = _mm_loadu_si128(pb.add(r * bs) as *const __m128i);
            acc = _mm_add_epi64(acc, _mm_sad_epu8(x, y));
        }
        let hi = _mm_unpackhi_epi64(acc, acc);
        (_mm_cvtsi128_si64(acc) + _mm_cvtsi128_si64(hi)) as u32
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "sse2")))]
    {
        let mut s = 0u32;
        for r in 0..16 {
            let ra = &a[ao + r * as_..ao + r * as_ + 16];
            let rb = &b[bo + r * bs..bo + r * bs + 16];
            for x in 0..16 {
                s += (ra[x] as i32 - rb[x] as i32).unsigned_abs();
            }
        }
        s
    }
}

/// x264-style decimation score of a block's levels in scan order: small
/// scores (lone +-1s far apart) are cheaper to drop than to code.
pub fn decimate_score(lv: &[i32]) -> u32 {
    const DS: [u32; 16] = [3, 2, 2, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut i = lv.len();
    while i > 0 && lv[i - 1] == 0 {
        i -= 1;
    }
    let mut score = 0;
    while i > 0 {
        i -= 1;
        if lv[i].abs() > 1 {
            return 9;
        }
        let mut run = 0;
        while i > 0 && lv[i - 1] == 0 {
            i -= 1;
            run += 1;
        }
        score += DS[run.min(15)];
    }
    score
}
