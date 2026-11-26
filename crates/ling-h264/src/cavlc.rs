//! CAVLC syntax element decoding (9.1, 9.2).

use crate::bits::BitReader;
use crate::mb::{Coefs, Entropy, SliceCtx};
use crate::tables::*;
use crate::Error;

pub struct Cavlc<'a, 'b> {
    pub br: BitReader<'a>,
    pub t: &'b CavlcTables,
}

impl<'a, 'b> Cavlc<'a, 'b> {
    fn nc_luma(c: &SliceCtx, blk: usize) -> i32 {
        let gx = (c.mbx * 4 + BLK_X[blk] as usize) as isize;
        let gy = (c.mby * 4 + BLK_Y[blk] as usize) as isize;
        let a = c.nb4(gx - 1, gy).map(|i| c.fs.nnz[i] as i32);
        let b = c.nb4(gx, gy - 1).map(|i| c.fs.nnz[i] as i32);
        match (a, b) {
            (Some(a), Some(b)) => (a + b + 1) >> 1,
            (Some(a), None) => a,
            (None, Some(b)) => b,
            _ => 0,
        }
    }

    fn nc_chroma(c: &SliceCtx, cc: usize, b: usize) -> i32 {
        let cs = c.mbw * 2;
        let plane = c.mbw * c.mbh * 4;
        let gx = (c.mbx * 2 + (b & 1)) as isize;
        let gy = (c.mby * 2 + (b >> 1)) as isize;
        let get = |x: isize, y: isize| -> Option<i32> {
            if x < 0 || y < 0 || !c.avail(x >> 1, y >> 1) {
                None
            } else {
                Some(c.fs.nnzc[cc * plane + y as usize * cs + x as usize] as i32)
            }
        };
        match (get(gx - 1, gy), get(gx, gy - 1)) {
            (Some(a), Some(b)) => (a + b + 1) >> 1,
            (Some(a), None) => a,
            (None, Some(b)) => b,
            _ => 0,
        }
    }

    fn block(&mut self, nc: i32, max: usize, out: &mut Coefs) -> Result<usize, Error> {
        let br = &mut self.br;
        let vlc = if nc < 0 {
            &self.t.chroma_dc_coeff_token
        } else if nc < 2 {
            &self.t.coeff_token[0]
        } else if nc < 4 {
            &self.t.coeff_token[1]
        } else if nc < 8 {
            &self.t.coeff_token[2]
        } else {
            &self.t.coeff_token[3]
        };
        let (sym, len) = vlc.lookup(br.peek32());
        if len == 0 {
            return Err(Error::Invalid("coeff_token"));
        }
        br.skip(len);
        let total = (sym >> 2) as usize;
        let t1 = (sym & 3) as usize;
        if total > max || t1 > total {
            return Err(Error::Invalid("total_coeff"));
        }
        if total == 0 {
            return Ok(0);
        }
        let mut lv = [0i32; 16];
        let mut suffix_len: u32 = if total > 10 && t1 < 3 { 1 } else { 0 };
        for i in 0..total {
            if i < t1 {
                lv[i] = if br.u(1) != 0 { -1 } else { 1 };
                continue;
            }
            let prefix = br.peek32().leading_zeros();
            if prefix > 25 {
                return Err(Error::Invalid("level_prefix"));
            }
            br.skip(prefix + 1);
            let mut code: i32 = (prefix.min(15) << suffix_len) as i32;
            if suffix_len > 0 || prefix >= 14 {
                let size = if prefix == 14 && suffix_len == 0 {
                    4
                } else if prefix >= 15 {
                    prefix - 3
                } else {
                    suffix_len
                };
                if size > 0 {
                    code += br.u(size) as i32;
                }
            }
            if prefix >= 15 && suffix_len == 0 {
                code += 15;
            }
            if prefix >= 16 {
                code += (1 << (prefix - 3)) - 4096;
            }
            if i == t1 && t1 < 3 {
                code += 2;
            }
            lv[i] = if code & 1 == 0 { (code + 2) >> 1 } else { (-code - 1) >> 1 };
            if suffix_len == 0 {
                suffix_len = 1;
            }
            if lv[i].abs() > (3 << (suffix_len - 1)) && suffix_len < 6 {
                suffix_len += 1;
            }
        }
        let mut zeros = 0usize;
        if total < max {
            let tz = if nc < 0 { &self.t.chroma_dc_total_zeros[total - 1] } else { &self.t.total_zeros[total - 1] };
            let (s, l) = tz.lookup(br.peek32());
            if l == 0 {
                return Err(Error::Invalid("total_zeros"));
            }
            br.skip(l);
            zeros = s as usize;
        }
        if total + zeros > max {
            return Err(Error::Invalid("total_zeros range"));
        }
        let mut pos = total + zeros - 1;
        let mut left = zeros;
        for i in 0..total {
            out.pos[i] = pos as u8;
            out.lv[i] = lv[i];
            if i == total - 1 {
                break;
            }
            let mut run = 0;
            if left > 0 {
                let rb = &self.t.run_before[left.min(7) - 1];
                let (s, l) = rb.lookup(br.peek32());
                if l == 0 {
                    return Err(Error::Invalid("run_before"));
                }
                br.skip(l);
                run = s as usize;
                if run > left {
                    return Err(Error::Invalid("run_before range"));
                }
            }
            left -= run;
            pos -= 1 + run;
        }
        Ok(total)
    }
}

impl<'a, 'b> Entropy for Cavlc<'a, 'b> {
    fn mb_type(&mut self, _c: &SliceCtx) -> Result<u32, Error> {
        let v = self.br.ue();
        if v > 48 {
            return Err(Error::Invalid("mb_type"));
        }
        Ok(v)
    }

    fn transform8(&mut self, _c: &SliceCtx) -> bool {
        self.br.flag()
    }

    fn intra_mode(&mut self) -> Option<u32> {
        if self.br.flag() {
            None
        } else {
            Some(self.br.u(3))
        }
    }

    fn chroma_mode(&mut self, _c: &SliceCtx) -> u32 {
        self.br.ue()
    }

    fn sub_mb_type(&mut self, _c: &SliceCtx) -> u32 {
        self.br.ue()
    }

    fn ref_idx(&mut self, _c: &SliceCtx, _list: usize, _x4: usize, _y4: usize, num: u32) -> u32 {
        if num == 2 {
            1 - self.br.u(1)
        } else {
            self.br.ue()
        }
    }

    fn mvd(&mut self, _c: &mut SliceCtx, _list: usize, _x4: usize, _y4: usize, _w4: usize, _h4: usize) -> [i32; 2] {
        let x = self.br.se();
        let y = self.br.se();
        [x, y]
    }

    fn cbp(&mut self, _c: &SliceCtx, intra: bool) -> Result<u32, Error> {
        let v = self.br.ue();
        if v > 47 {
            return Err(Error::Invalid("coded_block_pattern"));
        }
        Ok(if intra { GOLOMB_TO_INTRA_CBP[v as usize] } else { GOLOMB_TO_INTER_CBP[v as usize] } as u32)
    }

    fn qp_delta(&mut self, _c: &SliceCtx) -> i32 {
        self.br.se()
    }

    fn residual(&mut self, c: &SliceCtx, cat: u32, blk: usize, max: usize, out: &mut Coefs) -> Result<usize, Error> {
        let nc = match cat {
            0 | 1 | 2 => Self::nc_luma(c, blk),
            3 => -1,
            _ => Self::nc_chroma(c, blk >> 2, blk & 3),
        };
        self.block(nc, max, out)
    }

    fn pcm(&mut self, out: &mut [u8; 384]) -> Result<(), Error> {
        self.br.byte_align();
        for v in out.iter_mut() {
            *v = self.br.u(8) as u8;
        }
        if self.br.overrun() {
            return Err(Error::Invalid("truncated PCM"));
        }
        Ok(())
    }

    fn overrun(&self) -> bool {
        self.br.overrun()
    }
}
