//! YUV 4:2:0 -> 0xFFRRGGBB (BT.601, limited range) with optional deinterlacing.
//!
//! Deinterlacer (single frame, no temporal state): the top field lines are kept; every
//! bottom-field line is rebuilt from its top-field neighbours with a 3-direction ELA
//! (edge-based line average) and then blended with the original line according to a
//! comb measure: how much the line disagrees with the other field *beyond* how much it
//! disagrees with its own field (lines +-2). Static detail (which also differs from its
//! own field) keeps the original line; combing on moving content (consistent within the
//! field, inconsistent across fields) gets the interpolated line. Chroma planes get the
//! same treatment on chroma rows, which in interlaced 4:2:0 alternate fields too.

use crate::Frame;
use alloc::vec;

const CY: i32 = 76309; // 1.164383 * 65536
const CRV: i32 = 104597; // 1.596027
const CGU: i32 = 25675; // 0.391762
const CGV: i32 = 53279; // 0.812968
const CBU: i32 = 132201; // 2.017232

#[inline(always)]
fn clamp8(v: i32) -> u32 {
    v.clamp(0, 255) as u32
}

fn convert_row(y: &[u8], u: &[u8], v: &[u8], out: &mut [u32]) {
    let w = out.len();
    let pairs = w / 2;
    for i in 0..pairs {
        let uu = u[i] as i32 - 128;
        let vv = v[i] as i32 - 128;
        let rv = CRV * vv + 32768;
        let gv = -CGU * uu - CGV * vv + 32768;
        let bv = CBU * uu + 32768;
        for k in 0..2 {
            let yy = (y[2 * i + k] as i32 - 16) * CY;
            out[2 * i + k] = 0xFF00_0000
                | clamp8((yy + rv) >> 16) << 16
                | clamp8((yy + gv) >> 16) << 8
                | clamp8((yy + bv) >> 16);
        }
    }
    if w & 1 == 1 {
        let i = pairs;
        let uu = u[i] as i32 - 128;
        let vv = v[i] as i32 - 128;
        let yy = (y[w - 1] as i32 - 16) * CY;
        out[w - 1] = 0xFF00_0000
            | clamp8((yy + CRV * vv + 32768) >> 16) << 16
            | clamp8((yy - CGU * uu - CGV * vv + 32768) >> 16) << 8
            | clamp8((yy + CBU * uu + 32768) >> 16);
    }
}

/// Rebuild row `y` (odd) of a `w` x `h` plane into `out`.
fn deint_row(p: &[u8], w: usize, h: usize, y: usize, out: &mut [u8]) {
    let row = |r: usize| &p[r * w..r * w + w];
    let cur = row(y);
    let a = row(y - 1);
    let b = if y + 1 < h { row(y + 1) } else { a };
    let a2 = if y >= 2 { row(y - 2) } else { cur };
    let b2 = if y + 2 < h { row(y + 2) } else { cur };
    for x in 0..w {
        let av = a[x] as i32;
        let bv = b[x] as i32;
        // ELA: pick the direction with the smallest difference (vertical preferred).
        let mut e = (av + bv + 1) >> 1;
        if x > 0 && x + 1 < w {
            let dv = (av - bv).abs();
            let d1 = (a[x - 1] as i32 - b[x + 1] as i32).abs();
            let d2 = (a[x + 1] as i32 - b[x - 1] as i32).abs();
            if d1 + 8 < dv && d1 <= d2 {
                e = (a[x - 1] as i32 + b[x + 1] as i32 + 1) >> 1;
            } else if d2 + 8 < dv {
                e = (a[x + 1] as i32 + b[x - 1] as i32 + 1) >> 1;
            }
            // Keep the result within the vertical neighbours' range (+ a little slack).
            let lo = av.min(bv) - 4;
            let hi = av.max(bv) + 4;
            e = e.clamp(lo, hi);
        }
        let c = cur[x] as i32;
        let inter = (c - e).abs();
        let intra = (c - ((a2[x] as i32 + b2[x] as i32 + 1) >> 1)).abs();
        let comb = inter - intra;
        // Weight for the interpolated value: 0 below comb 6, 256 from comb 22.
        let wgt = ((comb - 6) * 16).clamp(0, 256);
        out[x] = ((c * (256 - wgt) + e * wgt + 128) >> 8) as u8;
    }
}

/// Convert `f` to 0xFFRRGGBB into `out` (`width * height` pixels, row-major). With
/// `deinterlace` set, interlaced frames are deinterlaced; progressive frames are never
/// touched. Does nothing if `out` or the planes are too small.
pub fn frame_to_rgba(f: &Frame, out: &mut [u32], deinterlace: bool) {
    let w = f.width as usize;
    let h = f.height as usize;
    let cw = w.div_ceil(2);
    let chh = h.div_ceil(2);
    if w == 0
        || h == 0
        || out.len() < w * h
        || f.y.len() < w * h
        || f.u.len() < cw * chh
        || f.v.len() < cw * chh
    {
        return;
    }
    if f.progressive || !deinterlace || h < 4 {
        for y in 0..h {
            let cr = if f.progressive || h < 4 {
                y / 2
            } else {
                // Interlaced 4:2:0: chroma rows alternate fields.
                ((y >> 2) << 1 | (y & 1)).min(chh - 1)
            };
            convert_row(
                &f.y[y * w..y * w + w],
                &f.u[cr * cw..cr * cw + cw],
                &f.v[cr * cw..cr * cw + cw],
                &mut out[y * w..y * w + w],
            );
        }
        return;
    }
    let mut ly = vec![0u8; w];
    let mut lu = vec![0u8; cw];
    let mut lv = vec![0u8; cw];
    let mut have_c = usize::MAX;
    for y in 0..h {
        let cr = y / 2;
        if cr != have_c {
            if cr & 1 == 1 {
                deint_row(&f.u, cw, chh, cr, &mut lu);
                deint_row(&f.v, cw, chh, cr, &mut lv);
            } else {
                lu.copy_from_slice(&f.u[cr * cw..cr * cw + cw]);
                lv.copy_from_slice(&f.v[cr * cw..cr * cw + cw]);
            }
            have_c = cr;
        }
        let yrow: &[u8] = if y & 1 == 1 {
            deint_row(&f.y, w, h, y, &mut ly);
            &ly
        } else {
            &f.y[y * w..y * w + w]
        };
        convert_row(yrow, &lu, &lv, &mut out[y * w..y * w + w]);
    }
}
