//! Small local replacements for the parts of symphonia-core the MPEG audio decoder used:
//! the error helpers, an MSB-first ("left-to-right") bit reader, a two-level table-driven
//! Huffman codebook, and a race-safe lazily-initialised static.
//!
//! Written for LingOS (MPL-2.0, as part of this crate). Every read is bounds checked: running off
//! the end of a buffer returns [`Error::Invalid`] instead of panicking.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Deref;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[inline(always)]
pub fn decode_error<T>(desc: &'static str) -> Result<T> {
    Err(Error::Invalid(desc))
}

#[inline(always)]
pub fn unsupported_error<T>(desc: &'static str) -> Result<T> {
    Err(Error::Unsupported(desc))
}

/// Sign extend a `bits`-bit wide two's complement value held in the low bits of `value`.
#[inline(always)]
pub fn sign_extend_leq32_to_i32(value: u32, bits: u32) -> i32 {
    if bits == 0 || bits >= 32 {
        return value as i32;
    }
    let shift = 32 - bits;
    ((value << shift) as i32) >> shift
}

/// MSB-first bit reader over a byte slice.
pub struct BitReaderLtr<'a> {
    buf: &'a [u8],
    /// Position in bits.
    pos: usize,
    /// Length in bits.
    len: usize,
}

impl<'a> BitReaderLtr<'a> {
    #[inline]
    pub fn new(buf: &'a [u8]) -> Self {
        BitReaderLtr { buf, pos: 0, len: buf.len() * 8 }
    }

    /// 64 bits starting at the byte holding the current position (zeros past the end).
    #[inline(always)]
    fn window(&self) -> u64 {
        let b = self.pos >> 3;
        if let Some(s) = self.buf.get(b..b + 8) {
            let mut a = [0u8; 8];
            a.copy_from_slice(s);
            u64::from_be_bytes(a)
        }
        else {
            let mut acc = 0u64;
            for i in 0..8 {
                acc = (acc << 8) | u64::from(*self.buf.get(b + i).unwrap_or(&0));
            }
            acc
        }
    }

    /// Peek `n` (1..=32) bits without consuming them. Bits past the end read as zero.
    #[inline(always)]
    pub fn peek(&self, n: u32) -> u32 {
        debug_assert!(n >= 1 && n <= 32);
        ((self.window() << (self.pos & 7)) >> (64 - n)) as u32
    }

    #[inline(always)]
    pub fn bits_left(&self) -> usize {
        self.len - self.pos
    }

    #[inline(always)]
    fn consume(&mut self, n: u32) -> Result<()> {
        if n as usize > self.len - self.pos {
            self.pos = self.len;
            return decode_error("mpa: unexpected end of bitstream");
        }
        self.pos += n as usize;
        Ok(())
    }

    /// Read `n` (0..=32) bits.
    #[inline(always)]
    pub fn read_bits_leq32(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        if n > 32 {
            return decode_error("mpa: bit read too wide");
        }
        let v = self.peek(n);
        self.consume(n)?;
        Ok(v)
    }

    #[inline(always)]
    pub fn read_bit(&mut self) -> Result<u32> {
        self.read_bits_leq32(1)
    }

    #[inline(always)]
    pub fn read_bool(&mut self) -> Result<bool> {
        Ok(self.read_bits_leq32(1)? != 0)
    }

    #[inline(always)]
    pub fn ignore_bits(&mut self, n: u32) -> Result<()> {
        self.consume(n)
    }

    /// Decode one codeword: returns (value, code length in bits).
    #[inline(always)]
    pub fn read_codebook(&mut self, cb: &Codebook) -> Result<(u32, u32)> {
        let e = match cb.table.get(self.peek(cb.primary) as usize) {
            Some(e) => *e,
            None => return decode_error("mpa: invalid huffman code"),
        };
        let e = if e.sub == 0 {
            e
        }
        else {
            let sub = u32::from(e.sub);
            let idx = e.val as usize + (self.peek(cb.primary + sub) & ((1 << sub) - 1)) as usize;
            match cb.table.get(idx) {
                Some(e) => *e,
                None => return decode_error("mpa: invalid huffman code"),
            }
        };
        if e.len == 0 {
            return decode_error("mpa: invalid huffman code");
        }
        let len = u32::from(e.len);
        self.consume(len)?;
        Ok((e.val, len))
    }
}

#[derive(Clone, Copy, Default)]
struct Entry {
    /// The decoded value, or for a sub-table pointer (`sub != 0`) the sub-table offset.
    val: u32,
    /// Full code length (0 = invalid code / pointer entry).
    len: u8,
    /// Sub-table index width in bits (0 = leaf).
    sub: u8,
}

/// A prefix-code lookup table: one `primary`-bit level plus optional second-level tables.
#[derive(Default)]
pub struct Codebook {
    table: Vec<Entry>,
    primary: u32,
}

impl Codebook {
    const PRIMARY_BITS: u32 = 9;

    /// Build from MSB-first `codes` / `lens` (length 0 = unused) and their `values`.
    pub fn new(codes: &[u32], lens: &[u8], values: &[u16]) -> Codebook {
        let max_len = lens.iter().copied().max().unwrap_or(0) as u32;
        if max_len == 0 {
            return Codebook { table: Vec::new(), primary: 1 };
        }
        let p = Self::PRIMARY_BITS.min(max_len);
        let mut table = vec![Entry::default(); 1 << p];

        // Widest suffix needed per primary prefix of a long code.
        let mut sub_bits = vec![0u8; 1 << p];
        for (&code, &len) in codes.iter().zip(lens) {
            let len = u32::from(len);
            if len > p {
                let prefix = (code >> (len - p)) as usize;
                sub_bits[prefix] = sub_bits[prefix].max((len - p) as u8);
            }
        }
        for (prefix, &sb) in sub_bits.iter().enumerate() {
            if sb != 0 {
                let off = table.len();
                table.resize(off + (1 << sb), Entry::default());
                table[prefix] = Entry { val: off as u32, len: 0, sub: sb };
            }
        }

        for ((&code, &len), &val) in codes.iter().zip(lens).zip(values) {
            let l = u32::from(len);
            if l == 0 {
                continue;
            }
            if l <= p {
                let base = (code << (p - l)) as usize;
                for e in &mut table[base..base + (1 << (p - l))] {
                    *e = Entry { val: u32::from(val), len, sub: 0 };
                }
            }
            else {
                let prefix = (code >> (l - p)) as usize;
                let ptr = table[prefix];
                let sb = u32::from(ptr.sub);
                let rest = l - p;
                let suffix = code & ((1 << rest) - 1);
                let base = ptr.val as usize + (suffix << (sb - rest)) as usize;
                for e in &mut table[base..base + (1 << (sb - rest))] {
                    *e = Entry { val: u32::from(val), len, sub: 0 };
                }
            }
        }

        Codebook { table, primary: p }
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }
}

/// A lazily computed static value (a no_std stand-in for `lazy_static!`). Initialisation is
/// race-safe: concurrent first users may each compute the value, one wins, the others drop theirs.
/// The value is built straight into a `Box` so big tables never live on the stack.
pub struct Lazy<T: 'static> {
    ptr: AtomicPtr<T>,
    init: fn() -> Box<T>,
}

unsafe impl<T: Sync + Send> Sync for Lazy<T> {}

impl<T> Lazy<T> {
    pub const fn new(init: fn() -> Box<T>) -> Self {
        Lazy { ptr: AtomicPtr::new(core::ptr::null_mut()), init }
    }

    #[inline]
    pub fn get(&self) -> &T {
        let p = self.ptr.load(Ordering::Acquire);
        if !p.is_null() {
            // SAFETY: a non-null pointer was published by `init_slow` and is never freed.
            return unsafe { &*p };
        }
        self.init_slow()
    }

    #[cold]
    fn init_slow(&self) -> &T {
        let fresh = Box::into_raw((self.init)());
        match self.ptr.compare_exchange(
            core::ptr::null_mut(),
            fresh,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            // SAFETY: we just published `fresh`; it lives for the rest of the program.
            Ok(_) => unsafe { &*fresh },
            Err(winner) => {
                // SAFETY: `fresh` was never shared; `winner` is published and never freed.
                unsafe {
                    drop(Box::from_raw(fresh));
                    &*winner
                }
            }
        }
    }
}

impl<T> Deref for Lazy<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        self.get()
    }
}

/// Allocate a zeroed `[T; N]` on the heap without building it on the stack first.
pub fn boxed_array<T: Copy + Default, const N: usize>() -> Box<[T; N]> {
    let v: Box<[T]> = vec![T::default(); N].into_boxed_slice();
    match v.try_into() {
        Ok(b) => b,
        Err(_) => unreachable!(),
    }
}

/// Heap-allocate a `T` whose all-zero bit pattern is a valid value, without building it on the
/// stack first.
///
/// # Safety
/// The all-zero bit pattern must be a valid `T` (e.g. arrays of floats / integers).
pub unsafe fn boxed_zeroed<T>() -> Box<T> {
    let layout = core::alloc::Layout::new::<T>();
    if layout.size() == 0 {
        return Box::new(core::mem::zeroed());
    }
    let p = alloc::alloc::alloc_zeroed(layout) as *mut T;
    if p.is_null() {
        alloc::alloc::handle_alloc_error(layout);
    }
    Box::from_raw(p)
}
