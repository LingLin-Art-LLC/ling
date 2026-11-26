//! CAVLC residual block writer (9.2, inverse of the decoder's `cavlc.rs`).

use super::bw::Bw;
use crate::tables::*;

/// Write one residual block. `lv` holds the levels in scan order (its length
/// is maxNumCoeff: 4 chroma DC, 15 AC, 16 full); `nc` < 0 selects the chroma
/// DC tables. Returns TotalCoeff.
pub fn write_block(bw: &mut Bw, lv: &[i32], nc: i32) -> u8 {
    let max = lv.len();
    // non-zero levels from the highest frequency down, and their positions
    let mut lvl = [0i32; 16];
    let mut pos = [0usize; 16];
    let mut total = 0usize;
    for i in (0..max).rev() {
        if lv[i] != 0 {
            lvl[total] = lv[i];
            pos[total] = i;
            total += 1;
        }
    }
    let mut t1 = 0usize;
    while t1 < total && t1 < 3 && lvl[t1].abs() == 1 {
        t1 += 1;
    }
    let tok = total * 4 + t1;
    if nc < 0 {
        bw.put(CHROMA_DC_COEFF_TOKEN_BITS[tok] as u32, CHROMA_DC_COEFF_TOKEN_LEN[tok] as u32);
    } else {
        let t = if nc < 2 {
            0
        } else if nc < 4 {
            1
        } else if nc < 8 {
            2
        } else {
            3
        };
        bw.put(COEFF_TOKEN_BITS[t][tok] as u32, COEFF_TOKEN_LEN[t][tok] as u32);
    }
    if total == 0 {
        return 0;
    }
    for i in 0..t1 {
        bw.put((lvl[i] < 0) as u32, 1);
    }
    let mut sl: u32 = if total > 10 && t1 < 3 { 1 } else { 0 };
    for i in t1..total {
        let l = lvl[i];
        let mut code: u32 = if l > 0 { (2 * l - 2) as u32 } else { (-2 * l - 1) as u32 };
        if i == t1 && t1 < 3 {
            code -= 2;
        }
        if sl == 0 {
            if code < 14 {
                bw.put(1, code + 1);
            } else if code < 30 {
                bw.put(1, 15);
                bw.put(code - 14, 4);
            } else {
                bw.put(1, 16);
                bw.put(code - 30, 12);
            }
        } else if code < (15 << sl) {
            bw.put(1, (code >> sl) + 1);
            bw.put(code & ((1 << sl) - 1), sl);
        } else {
            bw.put(1, 16);
            bw.put(code - (15 << sl), 12);
        }
        if sl == 0 {
            sl = 1;
        }
        if l.unsigned_abs() > (3 << (sl - 1)) && sl < 6 {
            sl += 1;
        }
    }
    if total < max {
        let tz = pos[0] + 1 - total;
        if nc < 0 {
            bw.put(CHROMA_DC_TOTAL_ZEROS_BITS[total - 1][tz] as u32, CHROMA_DC_TOTAL_ZEROS_LEN[total - 1][tz] as u32);
        } else {
            bw.put(TOTAL_ZEROS_BITS[total - 1][tz] as u32, TOTAL_ZEROS_LEN[total - 1][tz] as u32);
        }
        let mut left = tz;
        for i in 0..total - 1 {
            if left == 0 {
                break;
            }
            let run = pos[i] - pos[i + 1] - 1;
            let t = left.min(7) - 1;
            bw.put(RUN_BITS[t][run] as u32, RUN_LEN[t][run] as u32);
            left -= run;
        }
    }
    total as u8
}
