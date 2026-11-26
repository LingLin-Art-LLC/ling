//! MSB-first bit reader over a byte slice. Every read is bounds checked and
//! returns [`AacError::EndOfData`] instead of panicking on truncated input.

use crate::AacError;

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    len: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, len: data.len() * 8 }
    }

    /// A reader limited to bits `start .. start + len` of `data`.
    pub fn new_range(data: &'a [u8], start: usize, len: usize) -> Self {
        let end = (start.saturating_add(len)).min(data.len() * 8);
        Self { data, pos: start.min(end), len: end }
    }

    #[inline]
    pub fn bits_left(&self) -> usize {
        self.len - self.pos
    }

    #[inline]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Peek up to 32 bits; bits past the end read as zero.
    #[inline]
    pub fn peek(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let byte = self.pos >> 3;
        let mut acc: u64 = 0;
        for i in 0..5 {
            acc = (acc << 8) | u64::from(*self.data.get(byte + i).unwrap_or(&0));
        }
        // acc holds 40 bits starting at bit (byte * 8).
        let shift = 40 - (self.pos & 7) as u32 - n;
        ((acc >> shift) & ((1u64 << n) - 1)) as u32
    }

    #[inline]
    pub fn skip(&mut self, n: usize) -> Result<(), AacError> {
        if n > self.bits_left() {
            self.pos = self.len;
            return Err(AacError::EndOfData);
        }
        self.pos += n;
        Ok(())
    }

    #[inline]
    pub fn read(&mut self, n: u32) -> Result<u32, AacError> {
        if n as usize > self.bits_left() {
            self.pos = self.len;
            return Err(AacError::EndOfData);
        }
        let v = self.peek(n);
        self.pos += n as usize;
        Ok(v)
    }

    #[inline]
    pub fn read_bool(&mut self) -> Result<bool, AacError> {
        Ok(self.read(1)? != 0)
    }

    /// Count consecutive 1 bits (consuming the terminating 0), up to `max`.
    pub fn read_unary_ones(&mut self, max: u32) -> Result<u32, AacError> {
        let mut n = 0;
        while self.read(1)? == 1 {
            n += 1;
            if n > max {
                return Err(AacError::InvalidData("unary code too long"));
            }
        }
        Ok(n)
    }

    /// Align to the next byte boundary (relative to the start of the slice).
    pub fn align(&mut self) {
        self.pos = ((self.pos + 7) & !7).min(self.len);
    }
}
