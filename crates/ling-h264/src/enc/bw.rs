//! Bit writer for RBSP data, Exp-Golomb codes and NAL emulation prevention.
//!
//! The writer never grows its buffer: it writes into a caller-provided slice
//! and only records an overflow, so slice encoding can run on worker cores
//! that must not allocate.

pub struct Bw<'a> {
    buf: &'a mut [u8],
    pos: usize,
    acc: u64,
    n: u32,
    /// set when the buffer was too small (the output is then unusable)
    pub over: bool,
}

impl<'a> Bw<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Bw { buf, pos: 0, acc: 0, n: 0, over: false }
    }

    #[inline(always)]
    fn byte(&mut self, b: u8) {
        if let Some(d) = self.buf.get_mut(self.pos) {
            *d = b;
        } else {
            self.over = true;
        }
        self.pos += 1;
    }

    /// The low `len` bits of `v` (len <= 32), most significant first.
    #[inline(always)]
    pub fn put(&mut self, v: u32, len: u32) {
        if len == 0 {
            return;
        }
        self.acc = (self.acc << len) | (v as u64 & ((1u64 << len) - 1));
        self.n += len;
        while self.n >= 8 {
            self.n -= 8;
            let b = (self.acc >> self.n) as u8;
            self.byte(b);
        }
        self.acc &= (1u64 << self.n) - 1;
    }

    #[inline(always)]
    pub fn flag(&mut self, b: bool) {
        self.put(b as u32, 1);
    }

    #[inline]
    pub fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let nb = 64 - x.leading_zeros();
        let len = 2 * nb - 1;
        if len <= 32 {
            self.put(x as u32, len);
        } else {
            self.put(0, nb - 1);
            self.put(x as u32, nb);
        }
    }

    #[inline]
    pub fn se(&mut self, v: i32) {
        self.ue(if v > 0 { (2 * v - 1) as u32 } else { (-2 * v) as u32 });
    }

    /// rbsp_trailing_bits: a stop bit, then zeros to the byte boundary.
    pub fn trailing(&mut self) {
        self.put(1, 1);
        if self.n > 0 {
            self.put(0, 8 - self.n);
        }
    }

    /// Bytes written so far (whole bytes; call after `trailing`).
    pub fn len(&self) -> usize {
        self.pos
    }
}

/// Bits of ue(v).
#[inline(always)]
pub fn ue_bits(v: u32) -> u32 {
    let x = v as u64 + 1;
    2 * (64 - x.leading_zeros()) - 1
}

/// Bits of se(v).
#[inline(always)]
pub fn se_bits(v: i32) -> u32 {
    ue_bits(if v > 0 { (2 * v - 1) as u32 } else { (-2 * v) as u32 })
}

/// Emulation prevention (7.4.1): RBSP -> NAL payload. Returns the bytes
/// written, or None when `out` is too small.
pub fn escape(rbsp: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut o = 0usize;
    let mut zeros = 0u32;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            *out.get_mut(o)? = 3;
            o += 1;
            zeros = 0;
        }
        *out.get_mut(o)? = b;
        o += 1;
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    Some(o)
}
