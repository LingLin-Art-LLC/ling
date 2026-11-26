//! BigInt: arbitrary-precision integers (sign + magnitude in 32-bit limbs, little-endian).

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BigInt {
    pub neg: bool,
    /// no trailing zero limbs; zero is empty (and never negative)
    pub mag: Vec<u32>,
}

fn trim(v: &mut Vec<u32>) {
    while v.last() == Some(&0) {
        v.pop();
    }
}

fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    for i in (0..a.len()).rev() {
        if a[i] != b[i] {
            return a[i].cmp(&b[i]);
        }
    }
    Ordering::Equal
}

fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0u64;
    for i in 0..a.len().max(b.len()) {
        let s = *a.get(i).unwrap_or(&0) as u64 + *b.get(i).unwrap_or(&0) as u64 + carry;
        out.push(s as u32);
        carry = s >> 32;
    }
    if carry > 0 {
        out.push(carry as u32);
    }
    out
}

/// a - b with |a| >= |b|
fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for i in 0..a.len() {
        let mut d = a[i] as i64 - *b.get(i).unwrap_or(&0) as i64 - borrow;
        if d < 0 {
            d += 1 << 32;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(d as u32);
    }
    trim(&mut out);
    out
}

fn mul_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u32; a.len() + b.len()];
    for (i, &x) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, &y) in b.iter().enumerate() {
            let t = out[i + j] as u64 + x as u64 * y as u64 + carry;
            out[i + j] = t as u32;
            carry = t >> 32;
        }
        let mut k = i + b.len();
        while carry > 0 {
            let t = out[k] as u64 + carry;
            out[k] = t as u32;
            carry = t >> 32;
            k += 1;
        }
    }
    trim(&mut out);
    out
}

fn divrem_small(a: &[u32], d: u32) -> (Vec<u32>, u32) {
    let mut q = vec![0u32; a.len()];
    let mut r = 0u64;
    for i in (0..a.len()).rev() {
        let cur = (r << 32) | a[i] as u64;
        q[i] = (cur / d as u64) as u32;
        r = cur % d as u64;
    }
    trim(&mut q);
    (q, r as u32)
}

/// long division, bit by bit for big divisors (simple and fine for page scripts)
fn divrem_mag(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
    if cmp_mag(a, b) == Ordering::Less {
        return (Vec::new(), a.to_vec());
    }
    if b.len() == 1 {
        let (q, r) = divrem_small(a, b[0]);
        let mut rv = vec![r];
        trim(&mut rv);
        return (q, rv);
    }
    let bits = a.len() * 32;
    let mut q = vec![0u32; a.len()];
    let mut r: Vec<u32> = Vec::new();
    for i in (0..bits).rev() {
        // r = r << 1 | bit i of a
        let mut carry = (a[i / 32] >> (i % 32)) & 1;
        for limb in r.iter_mut() {
            let nc = *limb >> 31;
            *limb = (*limb << 1) | carry;
            carry = nc;
        }
        if carry > 0 {
            r.push(carry);
        }
        if cmp_mag(&r, b) != Ordering::Less {
            r = sub_mag(&r, b);
            q[i / 32] |= 1 << (i % 32);
        }
    }
    trim(&mut q);
    trim(&mut r);
    (q, r)
}

impl BigInt {
    pub fn zero() -> BigInt {
        BigInt { neg: false, mag: Vec::new() }
    }

    fn make(neg: bool, mut mag: Vec<u32>) -> BigInt {
        trim(&mut mag);
        let neg = neg && !mag.is_empty();
        BigInt { neg, mag }
    }

    pub fn from_i64(v: i64) -> BigInt {
        let neg = v < 0;
        let m = v.unsigned_abs();
        BigInt::make(neg, vec![m as u32, (m >> 32) as u32])
    }

    pub fn from_u64(m: u64) -> BigInt {
        BigInt::make(false, vec![m as u32, (m >> 32) as u32])
    }

    /// an integral finite number
    pub fn from_f64(n: f64) -> Option<BigInt> {
        if !n.is_finite() || libm::trunc(n) != n {
            return None;
        }
        let neg = n < 0.0;
        let mut x = libm::fabs(n);
        let mut mag = Vec::new();
        // peel off 32-bit limbs from the top with exact float arithmetic
        let mut limbs = Vec::new();
        while x >= 1.0 {
            let r = libm::fmod(x, 4294967296.0);
            limbs.push(r as u32);
            x = libm::floor(x / 4294967296.0);
        }
        mag.extend(limbs);
        Some(BigInt::make(neg, mag))
    }

    pub fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    pub fn to_f64(&self) -> f64 {
        let mut x = 0.0f64;
        for &l in self.mag.iter().rev() {
            x = x * 4294967296.0 + l as f64;
        }
        if self.neg {
            -x
        } else {
            x
        }
    }

    /// the low 64 bits, two's complement
    pub fn to_u64_wrapping(&self) -> u64 {
        let lo = *self.mag.first().unwrap_or(&0) as u64 | (*self.mag.get(1).unwrap_or(&0) as u64) << 32;
        if self.neg {
            lo.wrapping_neg()
        } else {
            lo
        }
    }

    pub fn parse(s: &str) -> Option<BigInt> {
        let t = s.trim();
        if t.is_empty() {
            return Some(BigInt::zero());
        }
        let (neg, body) = if let Some(r) = t.strip_prefix('-') {
            (true, r)
        } else if let Some(r) = t.strip_prefix('+') {
            (false, r)
        } else {
            (false, t)
        };
        let (radix, digits) = if body.len() > 2 && (body.starts_with("0x") || body.starts_with("0X")) {
            (16, &body[2..])
        } else if body.len() > 2 && (body.starts_with("0o") || body.starts_with("0O")) {
            (8, &body[2..])
        } else if body.len() > 2 && (body.starts_with("0b") || body.starts_with("0B")) {
            (2, &body[2..])
        } else {
            (10, body)
        };
        if radix != 10 && neg {
            return None;
        }
        BigInt::parse_radix(digits, radix).map(|b| BigInt::make(neg, b.mag))
    }

    pub fn parse_radix(digits: &str, radix: u32) -> Option<BigInt> {
        if digits.is_empty() {
            return None;
        }
        let mut mag: Vec<u32> = Vec::new();
        for c in digits.chars() {
            if c == '_' {
                continue;
            }
            let d = c.to_digit(radix)?;
            // mag = mag * radix + d
            let mut carry = d as u64;
            for limb in mag.iter_mut() {
                let t = *limb as u64 * radix as u64 + carry;
                *limb = t as u32;
                carry = t >> 32;
            }
            if carry > 0 {
                mag.push(carry as u32);
            }
        }
        Some(BigInt::make(false, mag))
    }

    pub fn to_string_radix(&self, radix: u32) -> String {
        if self.mag.is_empty() {
            return "0".into();
        }
        let mut digits = Vec::new();
        let mut cur = self.mag.clone();
        while !cur.is_empty() {
            let (q, r) = divrem_small(&cur, radix);
            digits.push(core::char::from_digit(r, radix).unwrap());
            cur = q;
        }
        let mut s = String::new();
        if self.neg {
            s.push('-');
        }
        for c in digits.iter().rev() {
            s.push(*c);
        }
        s
    }

    pub fn neg(&self) -> BigInt {
        BigInt::make(!self.neg, self.mag.clone())
    }

    pub fn add(&self, o: &BigInt) -> BigInt {
        if self.neg == o.neg {
            return BigInt::make(self.neg, add_mag(&self.mag, &o.mag));
        }
        match cmp_mag(&self.mag, &o.mag) {
            Ordering::Less => BigInt::make(o.neg, sub_mag(&o.mag, &self.mag)),
            _ => BigInt::make(self.neg, sub_mag(&self.mag, &o.mag)),
        }
    }

    pub fn sub(&self, o: &BigInt) -> BigInt {
        self.add(&o.neg())
    }

    pub fn mul(&self, o: &BigInt) -> BigInt {
        BigInt::make(self.neg != o.neg, mul_mag(&self.mag, &o.mag))
    }

    /// truncating division; None for division by zero
    pub fn divrem(&self, o: &BigInt) -> Option<(BigInt, BigInt)> {
        if o.is_zero() {
            return None;
        }
        let (q, r) = divrem_mag(&self.mag, &o.mag);
        Some((BigInt::make(self.neg != o.neg, q), BigInt::make(self.neg, r)))
    }

    pub fn pow(&self, mut e: u64) -> BigInt {
        let mut base = self.clone();
        let mut acc = BigInt::from_i64(1);
        while e > 0 {
            if e & 1 == 1 {
                acc = acc.mul(&base);
            }
            e >>= 1;
            if e > 0 {
                base = base.mul(&base);
            }
        }
        acc
    }

    pub fn cmp(&self, o: &BigInt) -> Ordering {
        match (self.neg, o.neg) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => cmp_mag(&self.mag, &o.mag),
            (true, true) => cmp_mag(&o.mag, &self.mag),
        }
    }

    /// compare with a number (NaN: None)
    pub fn cmp_f64(&self, n: f64) -> Option<Ordering> {
        if n.is_nan() {
            return None;
        }
        if n == f64::INFINITY {
            return Some(Ordering::Less);
        }
        if n == f64::NEG_INFINITY {
            return Some(Ordering::Greater);
        }
        let fl = libm::floor(n);
        let b = BigInt::from_f64(fl).unwrap();
        match self.cmp(&b) {
            Ordering::Equal => Some(if fl < n { Ordering::Less } else { Ordering::Equal }),
            o => Some(o),
        }
    }

    /// two's complement limbs, `n` long
    fn twos(&self, n: usize) -> Vec<u32> {
        let mut v = self.mag.clone();
        v.resize(n, 0);
        if self.neg {
            for l in v.iter_mut() {
                *l = !*l;
            }
            for l in v.iter_mut() {
                let (s, c) = l.overflowing_add(1);
                *l = s;
                if !c {
                    break;
                }
            }
        }
        v
    }

    fn from_twos(mut v: Vec<u32>) -> BigInt {
        let neg = v.last().is_some_and(|l| l >> 31 == 1);
        if neg {
            for l in v.iter_mut() {
                *l = !*l;
            }
            for l in v.iter_mut() {
                let (s, c) = l.overflowing_add(1);
                *l = s;
                if !c {
                    break;
                }
            }
        }
        BigInt::make(neg, v)
    }

    pub fn bitop(&self, o: &BigInt, op: u8) -> BigInt {
        let n = self.mag.len().max(o.mag.len()) + 1;
        let a = self.twos(n);
        let b = o.twos(n);
        let r: Vec<u32> = a
            .iter()
            .zip(b.iter())
            .map(|(x, y)| match op {
                0 => x & y,
                1 => x | y,
                _ => x ^ y,
            })
            .collect();
        BigInt::from_twos(r)
    }

    pub fn not(&self) -> BigInt {
        // ~x = -x - 1
        self.neg().sub(&BigInt::from_i64(1))
    }

    pub fn shl(&self, by: i64) -> BigInt {
        if by < 0 {
            return self.shr(-by);
        }
        let words = (by / 32) as usize;
        let bits = (by % 32) as u32;
        let mut v = vec![0u32; words];
        let mut carry = 0u32;
        for &l in &self.mag {
            if bits == 0 {
                v.push(l);
            } else {
                v.push((l << bits) | carry);
                carry = l >> (32 - bits);
            }
        }
        if carry > 0 {
            v.push(carry);
        }
        BigInt::make(self.neg, v)
    }

    /// arithmetic shift right (rounds toward -infinity)
    pub fn shr(&self, by: i64) -> BigInt {
        if by < 0 {
            return self.shl(-by);
        }
        let d = BigInt::from_i64(1).shl(by);
        let (q, r) = self.divrem(&d).unwrap();
        if self.neg && !r.is_zero() {
            q.sub(&BigInt::from_i64(1))
        } else {
            q
        }
    }

    pub fn as_uint_n(&self, bits: u64) -> BigInt {
        if bits == 0 {
            return BigInt::zero();
        }
        let n = (bits as usize).div_ceil(32) + 1;
        let mut v = self.twos(n.max(self.mag.len() + 1));
        v.truncate(n);
        let total = n * 32;
        // clear bits >= `bits`
        for i in (bits as usize)..total {
            v[i / 32] &= !(1 << (i % 32));
        }
        BigInt::make(false, v)
    }

    pub fn as_int_n(&self, bits: u64) -> BigInt {
        if bits == 0 {
            return BigInt::zero();
        }
        let u = self.as_uint_n(bits);
        let top = bits - 1;
        let set = u.mag.get((top / 32) as usize).is_some_and(|l| (l >> (top % 32)) & 1 == 1);
        if set {
            u.sub(&BigInt::from_i64(1).shl(bits as i64))
        } else {
            u
        }
    }
}
