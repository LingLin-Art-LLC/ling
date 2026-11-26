//! Table-driven decoder for the prefix (Huffman) codes used by AAC and SBR.
//!
//! A code table is given as parallel `codes` / `lens` arrays (MSB-first codes,
//! the layout used by the ISO tables). Short codes resolve through one lookup
//! of `PRIMARY` bits; the few longer codes fall back to a short linear scan.

use alloc::vec;
use alloc::vec::Vec;

use crate::bits::BitReader;
use crate::AacError;

const PRIMARY: u32 = 10;

#[derive(Clone)]
pub struct Vlc {
    /// (symbol, length); length 0 = not a short code.
    table: Vec<(u16, u8)>,
    /// Long codes: (code, length, symbol).
    long: Vec<(u32, u8, u16)>,
    bits: u32,
    max_len: u32,
}

impl Vlc {
    pub fn new(codes: &[u32], lens: &[u8]) -> Self {
        let max_len = lens.iter().copied().max().unwrap_or(1) as u32;
        let bits = PRIMARY.min(max_len);
        let mut table = vec![(0u16, 0u8); 1 << bits];
        let mut long = Vec::new();
        for (sym, (&code, &len)) in codes.iter().zip(lens.iter()).enumerate() {
            let len32 = u32::from(len);
            if len32 == 0 {
                continue;
            }
            if len32 <= bits {
                let base = (code << (bits - len32)) as usize;
                let n = 1usize << (bits - len32);
                for e in &mut table[base..base + n] {
                    *e = (sym as u16, len);
                }
            } else {
                long.push((code, len, sym as u16));
            }
        }
        long.sort_by_key(|e| e.1);
        Self { table, long, bits, max_len }
    }

    /// Decode one symbol (its index in the code table).
    #[inline]
    pub fn read(&self, bs: &mut BitReader<'_>) -> Result<u16, AacError> {
        let (sym, len) = self.table[bs.peek(self.bits) as usize];
        if len != 0 {
            bs.skip(len as usize)?;
            return Ok(sym);
        }
        let peek = bs.peek(self.max_len);
        for &(code, len, sym) in &self.long {
            if peek >> (self.max_len - u32::from(len)) == code {
                bs.skip(len as usize)?;
                return Ok(sym);
            }
        }
        Err(AacError::InvalidData("invalid huffman code"))
    }
}
