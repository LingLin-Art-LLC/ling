//! Motion compensation: half-pel block prediction (put or average) from a frame or one field
//! of a reference picture into the macroblock prediction buffer. Vectors pointing outside the
//! reference are handled by edge clamping (like FFmpeg's emulated edge), never by reading out
//! of bounds.

/// A rectangular window of a plane: a whole frame or one of its fields.
#[derive(Clone, Copy)]
pub struct View {
    pub off: usize,
    pub stride: usize,
    pub w: i32,
    pub h: i32,
}

#[inline(always)]
fn kern<const W: usize, const FX: bool, const FY: bool, const AVG: bool>(
    s: &[u8],
    so: usize,
    ss: usize,
    d: &mut [u8],
    doff: usize,
    ds: usize,
    h: usize,
) {
    let fx = FX as usize;
    for r in 0..h {
        let s0 = so + r * ss;
        let row0 = &s[s0..s0 + W + fx];
        let row1 = if FY { &s[s0 + ss..s0 + ss + W + fx] } else { row0 };
        let drow = &mut d[doff + r * ds..doff + r * ds + W];
        for x in 0..W {
            let p: u16 = match (FX, FY) {
                (false, false) => row0[x] as u16,
                (true, false) => (row0[x] as u16 + row0[x + fx] as u16 + 1) >> 1,
                (false, true) => (row0[x] as u16 + row1[x] as u16 + 1) >> 1,
                (true, true) => {
                    (row0[x] as u16
                        + row0[x + fx] as u16
                        + row1[x] as u16
                        + row1[x + fx] as u16
                        + 2)
                        >> 2
                }
            };
            drow[x] = if AVG { ((drow[x] as u16 + p + 1) >> 1) as u8 } else { p as u8 };
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn kernel<const W: usize>(
    s: &[u8],
    so: usize,
    ss: usize,
    d: &mut [u8],
    doff: usize,
    ds: usize,
    h: usize,
    fx: bool,
    fy: bool,
    avg: bool,
) {
    match (fx, fy, avg) {
        (false, false, false) => kern::<W, false, false, false>(s, so, ss, d, doff, ds, h),
        (true, false, false) => kern::<W, true, false, false>(s, so, ss, d, doff, ds, h),
        (false, true, false) => kern::<W, false, true, false>(s, so, ss, d, doff, ds, h),
        (true, true, false) => kern::<W, true, true, false>(s, so, ss, d, doff, ds, h),
        (false, false, true) => kern::<W, false, false, true>(s, so, ss, d, doff, ds, h),
        (true, false, true) => kern::<W, true, false, true>(s, so, ss, d, doff, ds, h),
        (false, true, true) => kern::<W, false, true, true>(s, so, ss, d, doff, ds, h),
        (true, true, true) => kern::<W, true, true, true>(s, so, ss, d, doff, ds, h),
    }
}

/// Predict a `W` x `h` block whose top-left (in `v` coordinates) is `(x, y)`, displaced by the
/// half-pel vector `(mvx, mvy)`, into `dst[doff + r*ds + c]`. `h` <= 16, `W` <= 16.
#[allow(clippy::too_many_arguments)]
#[inline]
pub fn mc<const W: usize>(
    src: &[u8],
    v: View,
    x: i32,
    y: i32,
    mvx: i32,
    mvy: i32,
    h: usize,
    dst: &mut [u8],
    doff: usize,
    ds: usize,
    avg: bool,
) {
    let ix = x + (mvx >> 1);
    let iy = y + (mvy >> 1);
    let fx = mvx & 1 != 0;
    let fy = mvy & 1 != 0;
    if ix >= 0
        && iy >= 0
        && ix + W as i32 + fx as i32 <= v.w
        && iy + h as i32 + fy as i32 <= v.h
    {
        let so = v.off + iy as usize * v.stride + ix as usize;
        kernel::<W>(src, so, v.stride, dst, doff, ds, h, fx, fy, avg);
    } else {
        // Edge emulation: gather the (W+1) x (h+1) source window with clamped coordinates.
        let mut tmp = [0u8; 17 * 17];
        let hh = h.min(16);
        for r in 0..=hh {
            let sy = (iy + r as i32).clamp(0, v.h - 1) as usize;
            let base = v.off + sy * v.stride;
            for c in 0..=W {
                let sx = (ix + c as i32).clamp(0, v.w - 1) as usize;
                tmp[r * 17 + c] = src[base + sx];
            }
        }
        kernel::<W>(&tmp, 0, 17, dst, doff, ds, hh, fx, fy, avg);
    }
}
