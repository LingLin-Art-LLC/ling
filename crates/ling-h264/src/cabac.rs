//! CABAC arithmetic decoding engine (9.3.1.2, 9.3.3.2) and context initialisation.
//!
//! The engine keeps codIOffset in the top bits of `low` (bit 17 upwards, aligned with
//! `range << 17`) followed by up to 16 read-ahead bits and a sentinel bit marking where the
//! read-ahead ends; when the sentinel has shifted up to bit 16 the next two bytes are loaded.
//! Decisions are branch-free (combined MPS/LPS state transition table, normalisation shift
//! table). Bit-exact with the bit-serial description in the standard.

use crate::cabac_tables::*;

pub const NUM_CTX: usize = 460;

/// rangeTabLPS indexed by ((range & 0xC0) * 2 + state), state = pStateIdx << 1 | valMPS
static LPS_FLAT: [u8; 512] = {
    let mut t = [0u8; 512];
    let mut q = 0;
    while q < 4 {
        let mut s = 0;
        while s < 128 {
            t[q * 128 + s] = RANGE_LPS[s >> 1][q];
            s += 1;
        }
        q += 1;
    }
    t
};

/// Next state indexed by 128 + s (MPS decoded) or 127 - s (LPS decoded, i.e. 128 + !s).
static MLPS: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut s = 0;
    while s < 128 {
        let st = s >> 1;
        let mps = (s & 1) as u8;
        t[128 + s] = (TRANS_MPS[st] << 1) | mps;
        let nm = if st == 0 { mps ^ 1 } else { mps };
        t[127 - s] = (TRANS_LPS[st] << 1) | nm;
        s += 1;
    }
    t
};

/// Left shift that brings a range (1..=511) back to >= 256; [0] = 9.
static NORM_SHIFT: [u8; 512] = {
    let mut t = [0u8; 512];
    t[0] = 9;
    let mut i = 1;
    while i < 512 {
        let mut n = 0;
        while (i << n) < 256 {
            n += 1;
        }
        t[i] = n as u8;
        i += 1;
    }
    t
};

pub struct Cabac<'a> {
    data: &'a [u8],
    pos: usize,
    range: i32,
    low: i32,
    pub ctx: [u8; NUM_CTX],
}

impl<'a> Cabac<'a> {
    pub fn new(data: &'a [u8], byte_pos: usize, slice_type_i: bool, cabac_init_idc: u32, qp: i32) -> Cabac<'a> {
        let mut c = Cabac { data, pos: byte_pos, range: 510, low: 0, ctx: [0; NUM_CTX] };
        c.init_ctx(slice_type_i, cabac_init_idc, qp);
        c.init_engine();
        c
    }

    pub fn init_ctx(&mut self, slice_type_i: bool, idc: u32, qp: i32) {
        let tab: &[[i8; 2]; NUM_CTX] = if slice_type_i {
            &CABAC_INIT_I
        } else {
            match idc {
                0 => &CABAC_INIT_PB0,
                1 => &CABAC_INIT_PB1,
                _ => &CABAC_INIT_PB2,
            }
        };
        let qp = qp.clamp(0, 51);
        for i in 0..NUM_CTX {
            let m = tab[i][0] as i32;
            let n = tab[i][1] as i32;
            let pre = (((m * qp) >> 4) + n).clamp(1, 126);
            self.ctx[i] = if pre <= 63 { ((63 - pre) << 1) as u8 } else { (((pre - 64) << 1) | 1) as u8 };
        }
    }

    #[inline(always)]
    fn byte_at(&self, i: usize) -> i32 {
        match self.data.get(i) {
            Some(&b) => b as i32,
            None => 0,
        }
    }

    pub fn init_engine(&mut self) {
        let p = self.pos;
        self.range = 510;
        // 9 bits of codIOffset at bits 17..25, 15 read-ahead bits, sentinel at bit 1
        self.low = (self.byte_at(p) << 18) | (self.byte_at(p + 1) << 10) | (self.byte_at(p + 2) << 2) | 2;
        self.pos = p + 3;
    }

    /// Load 16 more bits below a sentinel that sits exactly at bit 16.
    #[inline(always)]
    fn refill(&mut self) {
        let v = (self.byte_at(self.pos) << 9) + (self.byte_at(self.pos + 1) << 1);
        self.low = self.low.wrapping_add(v - 0xFFFF);
        self.pos += 2;
    }

    /// Load 16 more bits below a sentinel that sits at bit 16 or higher.
    #[inline(always)]
    fn refill2(&mut self) {
        let x = self.low ^ self.low.wrapping_sub(1);
        let i = 7 - NORM_SHIFT[((x >> 15) & 511) as usize] as i32;
        let v = (self.byte_at(self.pos) << 9) + (self.byte_at(self.pos + 1) << 1) - 0xFFFF;
        self.low = self.low.wrapping_add(v << (i & 31));
        self.pos += 2;
    }

    /// Position (in bits from the start of the RBSP) of the next unread bit in the
    /// spec's bit-serial decoder model (9 bits read at init + one per renormalisation shift).
    pub fn bit_pos(&self) -> usize {
        // read-ahead bits = 16 - (sentinel position)
        ((self.pos * 8) as i64 - 16 + self.low.trailing_zeros() as i64).max(0) as usize
    }

    /// Re-initialise the engine at a byte position (after PCM samples).
    pub fn restart_at(&mut self, byte_pos: usize) {
        self.pos = byte_pos;
        self.init_engine();
    }

    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// True once the decoder has consumed well past the end of the slice data.
    pub fn overrun(&self) -> bool {
        self.pos > self.data.len() + 4
    }

    #[inline(always)]
    pub fn decision(&mut self, ctx_idx: usize) -> u32 {
        let s = self.ctx[ctx_idx] as i32;
        let lps = LPS_FLAT[(((self.range & 0xC0) << 1) + s) as usize & 511] as i32;
        let mut range = self.range - lps;
        // all ones when codIOffset >= codIRange (LPS)
        let mask = (range << 17).wrapping_sub(self.low) >> 31;
        self.low = self.low.wrapping_sub((range << 17) & mask);
        range += (lps - range) & mask;
        let s = s ^ mask;
        self.ctx[ctx_idx] = MLPS[((s + 128) & 255) as usize];
        let sh = NORM_SHIFT[(range & 511) as usize] as i32;
        self.range = range << sh;
        self.low <<= sh;
        if self.low & 0xFFFF == 0 {
            self.refill2();
        }
        (s & 1) as u32
    }

    #[inline(always)]
    pub fn bypass(&mut self) -> u32 {
        self.low <<= 1;
        if self.low & 0xFFFF == 0 {
            self.refill();
        }
        let r = self.range << 17;
        let d = self.low.wrapping_sub(r);
        // m = -1 when low < r (bin 0)
        let m = d >> 31;
        self.low = self.low.wrapping_sub(r & !m);
        (m + 1) as u32
    }

    pub fn terminate(&mut self) -> u32 {
        self.range -= 2;
        if self.low >= self.range << 17 {
            1
        } else {
            if self.range < 256 {
                self.range <<= 1;
                self.low <<= 1;
                if self.low & 0xFFFF == 0 {
                    self.refill();
                }
            }
            0
        }
    }
}
