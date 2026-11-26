//! NAL unit unescaping and the RBSP bit reader (u(n), ue(v), se(v)).

use alloc::vec::Vec;

/// Copy a NAL unit payload (after the 1-byte header) removing emulation_prevention_three_bytes.
/// Eight zero bytes are appended so the reader can always load 64 bits.
pub fn unescape(src: &[u8], dst: &mut Vec<u8>) {
    dst.clear();
    dst.reserve(src.len() + 8);
    let mut zeros = 0u32;
    for &b in src {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        dst.push(b);
    }
}

/// Number of meaningful bits in an RBSP: everything before the rbsp_stop_one_bit.
pub fn rbsp_bits(data: &[u8]) -> usize {
    let mut n = data.len();
    while n > 0 && data[n - 1] == 0 {
        n -= 1;
    }
    if n == 0 {
        return 0;
    }
    let last = data[n - 1];
    n * 8 - 1 - last.trailing_zeros() as usize
}

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    /// bits before the stop bit
    end: usize,
    pub err: bool,
}

impl<'a> BitReader<'a> {
    /// `data` must be followed by >= 8 readable bytes of padding (see `unescape`), or be short:
    /// loads past `data.len()` read zeros.
    pub fn new(data: &'a [u8]) -> Self {
        let end = rbsp_bits(data);
        BitReader { data, pos: 0, end, err: false }
    }

    #[inline(always)]
    fn load64(&self, byte: usize) -> u64 {
        if byte + 8 <= self.data.len() {
            u64::from_be_bytes([
                self.data[byte],
                self.data[byte + 1],
                self.data[byte + 2],
                self.data[byte + 3],
                self.data[byte + 4],
                self.data[byte + 5],
                self.data[byte + 6],
                self.data[byte + 7],
            ])
        } else {
            let mut v = 0u64;
            for i in 0..8 {
                v <<= 8;
                if byte + i < self.data.len() {
                    v |= self.data[byte + i] as u64;
                }
            }
            v
        }
    }

    /// Peek the next 32 bits (left aligned).
    #[inline(always)]
    pub fn peek32(&self) -> u32 {
        let v = self.load64(self.pos >> 3);
        ((v << (self.pos & 7)) >> 32) as u32
    }

    #[inline(always)]
    pub fn skip(&mut self, n: u32) {
        self.pos += n as usize;
        if self.pos > self.end + 64 {
            self.err = true;
            self.pos = self.end + 64;
        }
    }

    #[inline(always)]
    pub fn u(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek32() >> (32 - n);
        self.skip(n);
        v
    }

    #[inline(always)]
    pub fn flag(&mut self) -> bool {
        self.u(1) != 0
    }

    #[inline(always)]
    pub fn ue(&mut self) -> u32 {
        let p = self.peek32();
        let lz = p.leading_zeros();
        if lz < 16 {
            // whole code fits in the 32 peeked bits
            let len = 2 * lz + 1;
            self.skip(len);
            return (p >> (32 - len)) - 1;
        }
        if lz >= 32 {
            self.err = true;
            self.skip(32);
            return u32::MAX / 2;
        }
        self.skip(lz + 1);
        let suffix = self.u(lz);
        ((1u64 << lz) - 1 + suffix as u64).min(u32::MAX as u64 / 2) as u32
    }

    #[inline(always)]
    pub fn se(&mut self) -> i32 {
        let k = self.ue();
        if k & 1 == 1 {
            ((k >> 1) + 1) as i32
        } else {
            -((k >> 1) as i32)
        }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn set_pos(&mut self, p: usize) {
        self.pos = p;
    }

    pub fn byte_align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }


    /// more_rbsp_data()
    pub fn more_rbsp_data(&self) -> bool {
        self.pos < self.end
    }

    /// True if the reader went past the end of the payload.
    pub fn overrun(&self) -> bool {
        self.err || self.pos > self.end
    }


}
