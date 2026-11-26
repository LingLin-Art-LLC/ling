//! Numbers: JavaScript's Number::toString (shortest round-trip digits, the spec's exponent
//! rules), string -> number, radix conversion, toFixed / toPrecision / toExponential.

use crate::strs::{is_ws, to_rust};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// what `std` gives f64 and `core` does not
pub trait F64Ext {
    fn fract(self) -> f64;
    fn rem_euclid(self, m: f64) -> f64;
}

impl F64Ext for f64 {
    #[inline]
    fn fract(self) -> f64 {
        self - libm::trunc(self)
    }
    #[inline]
    fn rem_euclid(self, m: f64) -> f64 {
        let r = libm::fmod(self, m);
        if r < 0.0 { r + m.abs() } else { r }
    }
}

/// the shortest digits that round-trip, and the decimal exponent n such that value = 0.d1d2... * 10^n
fn shortest(v: f64) -> (Vec<u8>, i32) {
    // Rust's `{:e}` prints the shortest round-trip digits: "1.2345e-7"
    let s = format!("{:e}", v);
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: Vec<u8> = mant.bytes().filter(|b| b.is_ascii_digit()).map(|b| b - b'0').collect();
    (digits, exp + 1)
}

pub fn to_string_rust(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v == 0.0 {
        return "0".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if v < 0.0 {
        return format!("-{}", to_string_rust(-v));
    }
    if v.fract() == 0.0 && v < 1e21 {
        // an integer: exact digits
        return format!("{}", v as u128);
    }
    let (d, n) = shortest(v);
    let k = d.len() as i32;
    let ds: String = d.iter().map(|x| (x + b'0') as char).collect();
    if k <= n && n <= 21 {
        let mut s = ds;
        for _ in 0..(n - k) {
            s.push('0');
        }
        s
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &ds[..n as usize], &ds[n as usize..])
    } else if -6 < n && n <= 0 {
        let mut s = String::from("0.");
        for _ in 0..(-n) {
            s.push('0');
        }
        s + &ds
    } else {
        let e = n - 1;
        let sign = if e < 0 { '-' } else { '+' };
        if k == 1 {
            format!("{}e{}{}", ds, sign, e.abs())
        } else {
            format!("{}.{}e{}{}", &ds[..1], &ds[1..], sign, e.abs())
        }
    }
}

pub fn radix_string(v: f64, radix: u32) -> String {
    if radix == 10 || v.is_nan() || v.is_infinite() {
        return to_string_rust(v);
    }
    let neg = v < 0.0;
    let v = v.abs();
    let mut ip = libm::floor(v);
    let mut fp = v - ip;
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut int_part: Vec<u8> = Vec::new();
    if ip == 0.0 {
        int_part.push(b'0');
    }
    while ip >= 1.0 {
        let d = libm::fmod(ip, radix as f64) as usize;
        int_part.push(digits[d]);
        ip = libm::floor(ip / radix as f64);
    }
    int_part.reverse();
    let mut s = String::from_utf8(int_part).unwrap_or_default();
    if fp > 0.0 {
        s.push('.');
        let mut n = 0;
        // enough digits to identify the double, like engines do (about 52 bits)
        let max = (52.0 / libm::log2(radix as f64)) as usize + 1;
        while fp > 0.0 && n < max {
            fp *= radix as f64;
            let d = libm::floor(fp) as usize;
            s.push(digits[d] as char);
            fp -= d as f64;
            n += 1;
        }
    }
    if neg {
        format!("-{}", s)
    } else {
        s
    }
}

/// ToNumber on a string
pub fn parse_number(s: &[u16]) -> f64 {
    let t = crate::strs::trim(s, true, true);
    if t.is_empty() {
        return 0.0;
    }
    let r = to_rust(t);
    let r = r.as_str();
    let (neg, body) = if let Some(b) = r.strip_prefix('-') {
        (true, b)
    } else if let Some(b) = r.strip_prefix('+') {
        (false, b)
    } else {
        (false, r)
    };
    let radix = if body.len() > 2 && body.as_bytes()[0] == b'0' {
        match body.as_bytes()[1] {
            b'x' | b'X' => 16,
            b'o' | b'O' => 8,
            b'b' | b'B' => 2,
            _ => 10,
        }
    } else {
        10
    };
    if radix != 10 {
        if neg || r.starts_with('+') {
            return f64::NAN;
        }
        let mut v = 0.0;
        for c in body[2..].chars() {
            match c.to_digit(radix) {
                Some(d) => v = v * radix as f64 + d as f64,
                None => return f64::NAN,
            }
        }
        return v;
    }
    if body == "Infinity" {
        return if neg { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    // JS decimal literal: digits [. digits] [e[+-]digits]; Rust's parser accepts "inf"/"nan", JS does not
    let ok = !body.is_empty()
        && body.bytes().all(|b| b.is_ascii_digit() || b == b'.' || b == b'e' || b == b'E' || b == b'+' || b == b'-')
        && body.bytes().any(|b| b.is_ascii_digit());
    if !ok {
        return f64::NAN;
    }
    match body.parse::<f64>() {
        Ok(v) => {
            if neg {
                -v
            } else {
                v
            }
        },
        Err(_) => f64::NAN,
    }
}

/// parseFloat: the longest prefix that is a number
pub fn parse_float(s: &[u16]) -> f64 {
    let t = crate::strs::trim(s, true, false);
    let r = to_rust(t);
    let b = r.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    if r[i..].starts_with("Infinity") {
        return if b.first() == Some(&b'-') { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    let ds = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == ds || (i == ds + 1 && b[ds] == b'.') {
        return f64::NAN;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let es = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > es {
            i = j;
        }
    }
    r[..i].parse::<f64>().unwrap_or(f64::NAN)
}

pub fn parse_int(s: &[u16], radix: i32) -> f64 {
    let t = crate::strs::trim(s, true, false);
    let mut i = 0;
    let mut neg = false;
    if i < t.len() && (t[i] == b'+' as u16 || t[i] == b'-' as u16) {
        neg = t[i] == b'-' as u16;
        i += 1;
    }
    let mut radix = radix;
    let mut strip = true;
    if radix != 0 {
        if !(2..=36).contains(&radix) {
            return f64::NAN;
        }
        if radix != 16 {
            strip = false;
        }
    } else {
        radix = 10;
    }
    if strip && i + 1 < t.len() && t[i] == b'0' as u16 && (t[i + 1] == b'x' as u16 || t[i + 1] == b'X' as u16) {
        i += 2;
        radix = 16;
    }
    let start = i;
    let mut v = 0.0f64;
    while i < t.len() {
        let c = t[i];
        let d = match c {
            48..=57 => (c - 48) as i32,
            97..=122 => (c - 97 + 10) as i32,
            65..=90 => (c - 65 + 10) as i32,
            _ => 99,
        };
        if d >= radix {
            break;
        }
        v = v * radix as f64 + d as f64;
        i += 1;
    }
    if i == start {
        return f64::NAN;
    }
    if neg {
        -v
    } else {
        v
    }
}

pub fn to_fixed(v: f64, f: usize) -> String {
    if !v.is_finite() || v.abs() >= 1e21 {
        return to_string_rust(v);
    }
    let s = format!("{:.*}", f, v);
    if s.starts_with("-") && s[1..].bytes().all(|b| b == b'0' || b == b'.') {
        return s[1..].to_string();
    }
    s
}

pub fn to_exponential(v: f64, f: Option<usize>) -> String {
    if !v.is_finite() {
        return to_string_rust(v);
    }
    let s = match f {
        Some(f) => format!("{:.*e}", f, v),
        None => format!("{:e}", v),
    };
    // Rust: 1.5e-7 / 1.5e7 ; JS: 1.5e-7 / 1.5e+7
    match s.split_once('e') {
        Some((m, e)) if !e.starts_with('-') => format!("{}e+{}", m, e),
        _ => s,
    }
}

pub fn to_precision(v: f64, p: usize) -> String {
    if !v.is_finite() {
        return to_string_rust(v);
    }
    if v == 0.0 {
        return if p > 1 { format!("0.{}", "0".repeat(p - 1)) } else { "0".into() };
    }
    let e = libm::floor(libm::log10(v.abs())) as i32;
    let es = format!("{:.*e}", p.saturating_sub(1), v);
    let (_, ex) = es.split_once('e').unwrap_or(("", "0"));
    let e2: i32 = ex.parse().unwrap_or(e);
    if e2 < -6 || e2 >= p as i32 {
        return to_exponential(v, Some(p.saturating_sub(1)));
    }
    let decimals = (p as i32 - 1 - e2).max(0) as usize;
    format!("{:.*}", decimals, v)
}

pub fn to_int32(v: f64) -> i32 {
    if !v.is_finite() || v == 0.0 {
        return 0;
    }
    let t = libm::trunc(v);
    let m = libm::fmod(t, 4294967296.0);
    let m = if m < 0.0 { m + 4294967296.0 } else { m };
    (m as u64 as u32) as i32
}

pub fn to_uint32(v: f64) -> u32 {
    to_int32(v) as u32
}

/// is the whitespace-only check used by == between a string and a number
pub fn is_blank(s: &[u16]) -> bool {
    s.iter().all(|&c| is_ws(c))
}
