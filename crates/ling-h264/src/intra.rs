//! Intra prediction (8.3.1 - 8.3.4).

#[inline(always)]
fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Directional Intra_4x4 / Intra_8x8 prediction.
/// `top[0]` = p[-1,-1], `top[1 + x]` = p[x,-1] for x in 0..2n; `left[0]` = p[-1,-1], `left[1 + y]` = p[-1,y].
/// Modes 3..=8 (0, 1, 2 are handled by the caller).
pub fn pred_dir(n: usize, mode: u32, top: &[i32; 17], left: &[i32; 9], out: &mut [u8], off: usize, stride: usize) {
    let t = |x: i32| top[(x + 1) as usize];
    let l = |y: i32| left[(y + 1) as usize];
    let n = n as i32;
    let last = 2 * n - 1;
    for y in 0..n {
        for x in 0..n {
            let v = match mode {
                3 => {
                    if x == n - 1 && y == n - 1 {
                        (t(last - 1) + 3 * t(last) + 2) >> 2
                    } else {
                        (t(x + y) + 2 * t(x + y + 1) + t(x + y + 2) + 2) >> 2
                    }
                }
                4 => {
                    if x > y {
                        (t(x - y - 2) + 2 * t(x - y - 1) + t(x - y) + 2) >> 2
                    } else if x < y {
                        (l(y - x - 2) + 2 * l(y - x - 1) + l(y - x) + 2) >> 2
                    } else {
                        (t(0) + 2 * t(-1) + l(0) + 2) >> 2
                    }
                }
                5 => {
                    let z = 2 * x - y;
                    if z >= 0 && z & 1 == 0 {
                        (t(x - (y >> 1) - 1) + t(x - (y >> 1)) + 1) >> 1
                    } else if z >= 0 {
                        (t(x - (y >> 1) - 2) + 2 * t(x - (y >> 1) - 1) + t(x - (y >> 1)) + 2) >> 2
                    } else if z == -1 {
                        (l(0) + 2 * l(-1) + t(0) + 2) >> 2
                    } else {
                        (l(y - 2 * x - 1) + 2 * l(y - 2 * x - 2) + l(y - 2 * x - 3) + 2) >> 2
                    }
                }
                6 => {
                    let z = 2 * y - x;
                    if z >= 0 && z & 1 == 0 {
                        (l(y - (x >> 1) - 1) + l(y - (x >> 1)) + 1) >> 1
                    } else if z >= 0 {
                        (l(y - (x >> 1) - 2) + 2 * l(y - (x >> 1) - 1) + l(y - (x >> 1)) + 2) >> 2
                    } else if z == -1 {
                        (l(0) + 2 * l(-1) + t(0) + 2) >> 2
                    } else {
                        (t(x - 2 * y - 1) + 2 * t(x - 2 * y - 2) + t(x - 2 * y - 3) + 2) >> 2
                    }
                }
                7 => {
                    if y & 1 == 0 {
                        (t(x + (y >> 1)) + t(x + (y >> 1) + 1) + 1) >> 1
                    } else {
                        (t(x + (y >> 1)) + 2 * t(x + (y >> 1) + 1) + t(x + (y >> 1) + 2) + 2) >> 2
                    }
                }
                _ => {
                    let z = x + 2 * y;
                    let lim = 2 * n - 3;
                    if z < lim && z & 1 == 0 {
                        (l(y + (x >> 1)) + l(y + (x >> 1) + 1) + 1) >> 1
                    } else if z < lim {
                        (l(y + (x >> 1)) + 2 * l(y + (x >> 1) + 1) + l(y + (x >> 1) + 2) + 2) >> 2
                    } else if z == lim {
                        (l(n - 2) + 3 * l(n - 1) + 2) >> 2
                    } else {
                        l(n - 1)
                    }
                }
            };
            out[off + y as usize * stride + x as usize] = v as u8;
        }
    }
}

/// Vertical / horizontal / DC for an n x n block (n = 4, 8, 16) given gathered edges.
pub fn pred_vh_dc(n: usize, mode: u32, top: &[i32], left: &[i32], has_top: bool, has_left: bool, out: &mut [u8], off: usize, stride: usize) {
    match mode {
        0 => {
            for y in 0..n {
                for x in 0..n {
                    out[off + y * stride + x] = top[x] as u8;
                }
            }
        }
        1 => {
            for y in 0..n {
                let v = left[y] as u8;
                out[off + y * stride..off + y * stride + n].fill(v);
            }
        }
        _ => {
            let shift = n.trailing_zeros();
            let st: i32 = top[..n].iter().sum();
            let sl: i32 = left[..n].iter().sum();
            let dc = if has_top && has_left {
                (st + sl + n as i32) >> (shift + 1)
            } else if has_left {
                (sl + (n as i32 >> 1)) >> shift
            } else if has_top {
                (st + (n as i32 >> 1)) >> shift
            } else {
                128
            };
            for y in 0..n {
                out[off + y * stride..off + y * stride + n].fill(dc as u8);
            }
        }
    }
}

/// Intra_16x16 plane prediction. `top[0]` = p[-1,-1], `top[1+x]` = p[x,-1]; `left[1+y]` = p[-1,y].
pub fn pred16_plane(top: &[i32; 17], left: &[i32; 17], out: &mut [u8], off: usize, stride: usize) {
    let mut h = 0;
    let mut v = 0;
    for i in 0..8 {
        h += (i + 1) * (top[(8 + i + 1) as usize] - top[(6 - i + 1) as usize]);
        v += (i + 1) * (left[(8 + i + 1) as usize] - left[(6 - i + 1) as usize]);
    }
    let a = 16 * (left[16] + top[16]);
    let b = (5 * h + 32) >> 6;
    let c = (5 * v + 32) >> 6;
    for y in 0..16 {
        for x in 0..16 {
            out[off + y * stride + x] = clip((a + b * (x as i32 - 7) + c * (y as i32 - 7) + 16) >> 5);
        }
    }
}

/// Chroma (8x8, 4:2:0) prediction. Mode: 0 DC, 1 horizontal, 2 vertical, 3 plane.
pub fn pred_chroma(mode: u32, top: &[i32; 9], left: &[i32; 9], has_top: bool, has_left: bool, out: &mut [u8], off: usize, stride: usize) {
    // top[0]/left[0] = p[-1,-1]; top[1+x], left[1+y]
    match mode {
        0 => {
            for by in 0..2 {
                for bx in 0..2 {
                    let st: i32 = (0..4).map(|i| top[1 + bx * 4 + i]).sum();
                    let sl: i32 = (0..4).map(|i| left[1 + by * 4 + i]).sum();
                    let dc = if bx == by {
                        if has_top && has_left {
                            (st + sl + 4) >> 3
                        } else if has_left {
                            (sl + 2) >> 2
                        } else if has_top {
                            (st + 2) >> 2
                        } else {
                            128
                        }
                    } else if bx == 1 {
                        // (4, 0): prefer top
                        if has_top {
                            (st + 2) >> 2
                        } else if has_left {
                            (sl + 2) >> 2
                        } else {
                            128
                        }
                    } else if has_left {
                        (sl + 2) >> 2
                    } else if has_top {
                        (st + 2) >> 2
                    } else {
                        128
                    };
                    for y in 0..4 {
                        let o = off + (by * 4 + y) * stride + bx * 4;
                        out[o..o + 4].fill(dc as u8);
                    }
                }
            }
        }
        1 => {
            for y in 0..8 {
                out[off + y * stride..off + y * stride + 8].fill(left[1 + y] as u8);
            }
        }
        2 => {
            for y in 0..8 {
                for x in 0..8 {
                    out[off + y * stride + x] = top[1 + x] as u8;
                }
            }
        }
        _ => {
            let mut h = 0;
            let mut v = 0;
            for i in 0..4i32 {
                h += (i + 1) * (top[(4 + i + 1) as usize] - top[(2 - i + 1) as usize]);
                v += (i + 1) * (left[(4 + i + 1) as usize] - left[(2 - i + 1) as usize]);
            }
            let a = 16 * (left[8] + top[8]);
            let b = (34 * h + 32) >> 6;
            let c = (34 * v + 32) >> 6;
            for y in 0..8 {
                for x in 0..8 {
                    out[off + y * stride + x] = clip((a + b * (x as i32 - 3) + c * (y as i32 - 3) + 16) >> 5);
                }
            }
        }
    }
}

/// Reference sample filtering for Intra_8x8 (8.3.2.2.1). Arrays as for `pred_dir` with n = 8.
pub fn filter8(top: &mut [i32; 17], left: &mut [i32; 9], has_top: bool, has_left: bool, has_tl: bool) {
    let t = *top;
    let l = *left;
    // t[0] = p[-1,-1], t[1+x] = p[x,-1]
    if has_top {
        top[1] = if has_tl { (t[0] + 2 * t[1] + t[2] + 2) >> 2 } else { (3 * t[1] + t[2] + 2) >> 2 };
        for x in 1..15 {
            top[1 + x] = (t[x] + 2 * t[1 + x] + t[2 + x] + 2) >> 2;
        }
        top[16] = (t[15] + 3 * t[16] + 2) >> 2;
    }
    if has_tl {
        let v = if !has_top || !has_left {
            if has_top {
                (3 * t[0] + t[1] + 2) >> 2
            } else if has_left {
                (3 * t[0] + l[1] + 2) >> 2
            } else {
                t[0]
            }
        } else {
            (t[1] + 2 * t[0] + l[1] + 2) >> 2
        };
        top[0] = v;
        left[0] = v;
    }
    if has_left {
        left[1] = if has_tl { (l[0] + 2 * l[1] + l[2] + 2) >> 2 } else { (3 * l[1] + l[2] + 2) >> 2 };
        for y in 1..7 {
            left[1 + y] = (l[y] + 2 * l[1 + y] + l[2 + y] + 2) >> 2;
        }
        left[8] = (l[7] + 3 * l[8] + 2) >> 2;
    }
}
