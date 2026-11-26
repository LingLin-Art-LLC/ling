//! MSB-first bit reader over a byte slice. Reads past the end return zero bits (which the
//! callers treat as a start code / invalid code), so a truncated unit can never index out
//! of bounds; `overrun()` tells whether the cursor went beyond the data.

pub(crate) struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> Bits<'a> {
    #[inline]
    pub fn new(d: &'a [u8]) -> Self {
        Bits { d, pos: 0, end: d.len() * 8 }
    }

    #[inline(always)]
    fn load64(&self) -> u64 {
        let i = self.pos >> 3;
        if let Some(s) = self.d.get(i..i.wrapping_add(8)) {
            let mut a = [0u8; 8];
            a.copy_from_slice(s);
            u64::from_be_bytes(a)
        } else {
            let mut v = 0u64;
            for k in 0..8 {
                v <<= 8;
                if let Some(&b) = self.d.get(i.wrapping_add(k)) {
                    v |= b as u64;
                }
            }
            v
        }
    }

    /// The next 32 bits, MSB-aligned.
    #[inline(always)]
    pub fn peek32(&self) -> u32 {
        ((self.load64() << (self.pos & 7)) >> 32) as u32
    }

    /// The next `n` bits (1..=32).
    #[inline(always)]
    pub fn peek(&self, n: u32) -> u32 {
        self.peek32() >> (32 - n)
    }

    #[inline(always)]
    pub fn skip(&mut self, n: u32) {
        self.pos = self.pos.saturating_add(n as usize);
    }

    /// Read `n` bits (0..=32).
    #[inline(always)]
    pub fn get(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek(n);
        self.skip(n);
        v
    }

    #[inline(always)]
    pub fn get1(&mut self) -> bool {
        self.get(1) != 0
    }

    /// Read `n` bits as a two's complement signed number.
    #[inline(always)]
    pub fn get_signed(&mut self, n: u32) -> i32 {
        let v = self.get(n);
        ((v << (32 - n)) as i32) >> (32 - n)
    }

    #[inline(always)]
    pub fn overrun(&self) -> bool {
        self.pos > self.end
    }
}
