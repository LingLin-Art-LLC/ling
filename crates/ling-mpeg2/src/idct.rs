//! 8x8 integer inverse DCT: the "simple IDCT" algorithm (Michael Niedermayer's, as used by
//! FFmpeg as its default C IDCT; IEEE-1180 compliant). Rows are done in 16-bit storage with
//! the same truncation FFmpeg applies, so output matches FFmpeg's simple IDCT bit for bit.
//!
//! Coefficients are in natural (raster) order; `rowmask` has bit `r` set when row `r` may hold
//! a non-zero coefficient (rows outside the mask must be zero). The block is clobbered.

const W1: i32 = 22725;
const W2: i32 = 21407;
const W3: i32 = 19266;
const W4: i32 = 16383;
const W5: i32 = 12873;
const W6: i32 = 8867;
const W7: i32 = 4520;
const ROW_SHIFT: u32 = 11;
const COL_SHIFT: u32 = 20;

#[inline(always)]
fn idct_row(r: &mut [i16]) {
    let r0 = r[0] as i32;
    if r[1] == 0 && r[2] == 0 && r[3] == 0 && r[4] == 0 && r[5] == 0 && r[6] == 0 && r[7] == 0
    {
        let v = (r0 << 3) as i16;
        for x in r.iter_mut().take(8) {
            *x = v;
        }
        return;
    }
    let (r1, r2, r3) = (r[1] as i32, r[2] as i32, r[3] as i32);
    let (r4, r5, r6, r7) = (r[4] as i32, r[5] as i32, r[6] as i32, r[7] as i32);
    let mut a0 = W4 * r0 + (1 << (ROW_SHIFT - 1));
    let mut a1 = a0;
    let mut a2 = a0;
    let mut a3 = a0;
    a0 += W2 * r2;
    a1 += W6 * r2;
    a2 -= W6 * r2;
    a3 -= W2 * r2;
    let mut b0 = W1 * r1 + W3 * r3;
    let mut b1 = W3 * r1 - W7 * r3;
    let mut b2 = W5 * r1 - W1 * r3;
    let mut b3 = W7 * r1 - W5 * r3;
    a0 += W4 * r4 + W6 * r6;
    a1 += -W4 * r4 - W2 * r6;
    a2 += -W4 * r4 + W2 * r6;
    a3 += W4 * r4 - W6 * r6;
    b0 += W5 * r5 + W7 * r7;
    b1 += -W1 * r5 - W5 * r7;
    b2 += W7 * r5 + W3 * r7;
    b3 += W3 * r5 - W1 * r7;
    r[0] = ((a0 + b0) >> ROW_SHIFT) as i16;
    r[7] = ((a0 - b0) >> ROW_SHIFT) as i16;
    r[1] = ((a1 + b1) >> ROW_SHIFT) as i16;
    r[6] = ((a1 - b1) >> ROW_SHIFT) as i16;
    r[2] = ((a2 + b2) >> ROW_SHIFT) as i16;
    r[5] = ((a2 - b2) >> ROW_SHIFT) as i16;
    r[3] = ((a3 + b3) >> ROW_SHIFT) as i16;
    r[4] = ((a3 - b3) >> ROW_SHIFT) as i16;
}

/// Column pass; returns the 8 outputs of column `c` (not yet clipped).
#[inline(always)]
fn idct_col(b: &[i16; 64], c: usize) -> [i32; 8] {
    let c0 = b[c] as i32;
    let c1 = b[8 + c] as i32;
    let c2 = b[16 + c] as i32;
    let c3 = b[24 + c] as i32;
    let c4 = b[32 + c] as i32;
    let c5 = b[40 + c] as i32;
    let c6 = b[48 + c] as i32;
    let c7 = b[56 + c] as i32;
    // Every partial sum below is bounded by sum(|W|) * 32768 < 2^31; only a+b may wrap
    // (garbage input), where FFmpeg's C would also wrap.
    let mut a0 = W4 * (c0 + ((1 << (COL_SHIFT - 1)) / W4));
    let mut a1 = a0;
    let mut a2 = a0;
    let mut a3 = a0;
    a0 += W2 * c2;
    a1 += W6 * c2;
    a2 -= W6 * c2;
    a3 -= W2 * c2;
    let mut b0 = W1 * c1 + W3 * c3;
    let mut b1 = W3 * c1 - W7 * c3;
    let mut b2 = W5 * c1 - W1 * c3;
    let mut b3 = W7 * c1 - W5 * c3;
    a0 += W4 * c4;
    a1 -= W4 * c4;
    a2 -= W4 * c4;
    a3 += W4 * c4;
    b0 += W5 * c5;
    b1 -= W1 * c5;
    b2 += W7 * c5;
    b3 += W3 * c5;
    a0 += W6 * c6;
    a1 -= W2 * c6;
    a2 += W2 * c6;
    a3 -= W6 * c6;
    b0 += W7 * c7;
    b1 -= W5 * c7;
    b2 += W3 * c7;
    b3 -= W1 * c7;
    [
        a0.wrapping_add(b0) >> COL_SHIFT,
        a1.wrapping_add(b1) >> COL_SHIFT,
        a2.wrapping_add(b2) >> COL_SHIFT,
        a3.wrapping_add(b3) >> COL_SHIFT,
        a3.wrapping_sub(b3) >> COL_SHIFT,
        a2.wrapping_sub(b2) >> COL_SHIFT,
        a1.wrapping_sub(b1) >> COL_SHIFT,
        a0.wrapping_sub(b0) >> COL_SHIFT,
    ]
}

#[inline(always)]
fn rows(b: &mut [i16; 64], rowmask: u8) {
    for r in 0..8 {
        if rowmask & (1 << r) != 0 {
            idct_row(&mut b[r * 8..r * 8 + 8]);
        }
    }
}

#[inline(always)]
fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// IDCT and store (intra blocks). `dst[off + y*stride + x]`.
pub fn idct_put(b: &mut [i16; 64], rowmask: u8, dst: &mut [u8], off: usize, stride: usize) {
    if rowmask == 0 {
        for y in 0..8 {
            let o = off + y * stride;
            for x in dst[o..o + 8].iter_mut() {
                *x = 0;
            }
        }
        return;
    }
    rows(b, rowmask);
    if rowmask == 1 {
        // Only row 0: each column is constant.
        let mut v = [0u8; 8];
        for (c, vc) in v.iter_mut().enumerate() {
            *vc = clip((W4 * (b[c] as i32 + ((1 << (COL_SHIFT - 1)) / W4))) >> COL_SHIFT);
        }
        for y in 0..8 {
            let o = off + y * stride;
            dst[o..o + 8].copy_from_slice(&v);
        }
        return;
    }
    for c in 0..8 {
        let o = idct_col(b, c);
        for (y, &v) in o.iter().enumerate() {
            dst[off + y * stride + c] = clip(v);
        }
    }
}

/// IDCT and add to the prediction already in `dst` (non-intra blocks).
pub fn idct_add(b: &mut [i16; 64], rowmask: u8, dst: &mut [u8], off: usize, stride: usize) {
    if rowmask == 0 {
        return;
    }
    rows(b, rowmask);
    if rowmask == 1 {
        let mut v = [0i32; 8];
        for (c, vc) in v.iter_mut().enumerate() {
            *vc = (W4 * (b[c] as i32 + ((1 << (COL_SHIFT - 1)) / W4))) >> COL_SHIFT;
        }
        for y in 0..8 {
            let o = off + y * stride;
            for (d, &a) in dst[o..o + 8].iter_mut().zip(v.iter()) {
                *d = clip(*d as i32 + a);
            }
        }
        return;
    }
    for c in 0..8 {
        let o = idct_col(b, c);
        for (y, &v) in o.iter().enumerate() {
            let d = &mut dst[off + y * stride + c];
            *d = clip(*d as i32 + v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference float IDCT.
    fn ref_idct(b: &[i16; 64]) -> [f64; 64] {
        let mut out = [0f64; 64];
        let pi = core::f64::consts::PI;
        for y in 0..8 {
            for x in 0..8 {
                let mut s = 0.0;
                for v in 0..8 {
                    for u in 0..8 {
                        let cu = if u == 0 { (0.5f64).sqrt() } else { 1.0 };
                        let cv = if v == 0 { (0.5f64).sqrt() } else { 1.0 };
                        s += cu
                            * cv
                            * b[v * 8 + u] as f64
                            * (((2 * x + 1) as f64 * u as f64 * pi) / 16.0).cos()
                            * (((2 * y + 1) as f64 * v as f64 * pi) / 16.0).cos();
                    }
                }
                out[y * 8 + x] = s / 4.0;
            }
        }
        out
    }

    #[test]
    fn close_to_float_reference() {
        let mut seed = 12345u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            (seed >> 16) as i32
        };
        let mut maxerr = 0.0f64;
        for _ in 0..2000 {
            let mut b = [0i16; 64];
            for k in 0..64 {
                if rnd() % 4 == 0 {
                    b[k] = ((rnd() % 512) - 256) as i16;
                }
            }
            let r = ref_idct(&b);
            let mut mask = 0u8;
            for k in 0..64 {
                if b[k] != 0 {
                    mask |= 1 << (k / 8);
                }
            }
            let mut dst = [128u8; 64];
            let mut bb = b;
            idct_add(&mut bb, mask, &mut dst, 0, 8);
            for k in 0..64 {
                let want = (r[k] + 128.0).round().clamp(0.0, 255.0);
                let e = (dst[k] as f64 - want).abs();
                if e > maxerr {
                    maxerr = e;
                }
            }
        }
        assert!(maxerr <= 1.0, "max error {maxerr}");
    }
}
