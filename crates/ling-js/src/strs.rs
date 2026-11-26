//! JavaScript strings: immutable UTF-16 code unit slices, shared by reference count.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

pub type JsStr = Rc<[u16]>;

pub fn js(s: &str) -> JsStr {
    let v: Vec<u16> = s.encode_utf16().collect();
    Rc::from(v)
}

pub fn from_units(v: Vec<u16>) -> JsStr {
    Rc::from(v)
}

/// UTF-16 -> Rust text (a lone surrogate becomes U+FFFD)
pub fn to_rust(s: &[u16]) -> String {
    char::decode_utf16(s.iter().copied()).map(|r| r.unwrap_or('\u{FFFD}')).collect()
}

pub fn concat(a: &[u16], b: &[u16]) -> JsStr {
    let mut v = Vec::with_capacity(a.len() + b.len());
    v.extend_from_slice(a);
    v.extend_from_slice(b);
    Rc::from(v)
}

pub fn eq_str(a: &[u16], b: &str) -> bool {
    let mut it = b.encode_utf16();
    for &u in a {
        if it.next() != Some(u) {
            return false;
        }
    }
    it.next().is_none()
}

pub fn index_of(hay: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return if from <= hay.len() { Some(from) } else { Some(hay.len()) };
    }
    if needle.len() > hay.len() {
        return None;
    }
    let last = hay.len() - needle.len();
    let mut i = from;
    while i <= last {
        if hay[i] == needle[0] && &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

pub fn last_index_of(hay: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    let mut i = core::cmp::min(from, hay.len() - needle.len()) as isize;
    while i >= 0 {
        let u = i as usize;
        if &hay[u..u + needle.len()] == needle {
            return Some(u);
        }
        i -= 1;
    }
    None
}

/// JS whitespace + line terminators (for trim / Number parsing)
pub fn is_ws(c: u16) -> bool {
    matches!(c, 0x09 | 0x0A | 0x0B | 0x0C | 0x0D | 0x20 | 0xA0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF)
}

pub fn trim(s: &[u16], start: bool, end: bool) -> &[u16] {
    let mut a = 0;
    let mut b = s.len();
    if start {
        while a < b && is_ws(s[a]) {
            a += 1;
        }
    }
    if end {
        while b > a && is_ws(s[b - 1]) {
            b -= 1;
        }
    }
    &s[a..b]
}

pub fn lower(s: &[u16]) -> JsStr {
    if s.iter().all(|&c| c < 128) {
        return Rc::from(s.iter().map(|&c| if (b'A' as u16..=b'Z' as u16).contains(&c) { c + 32 } else { c }).collect::<Vec<_>>());
    }
    let t: String = to_rust(s).chars().flat_map(|c| c.to_lowercase()).collect();
    js(&t)
}

pub fn upper(s: &[u16]) -> JsStr {
    if s.iter().all(|&c| c < 128) {
        return Rc::from(s.iter().map(|&c| if (b'a' as u16..=b'z' as u16).contains(&c) { c - 32 } else { c }).collect::<Vec<_>>());
    }
    let t: String = to_rust(s).chars().flat_map(|c| c.to_uppercase()).collect();
    js(&t)
}

/// "123" -> Some(123) when the string is a canonical array index
pub fn array_index(s: &[u16]) -> Option<u32> {
    if s.is_empty() || s.len() > 10 {
        return None;
    }
    if s[0] == b'0' as u16 {
        return if s.len() == 1 { Some(0) } else { None };
    }
    let mut n: u64 = 0;
    for &c in s {
        if !(b'0' as u16..=b'9' as u16).contains(&c) {
            return None;
        }
        n = n * 10 + (c - b'0' as u16) as u64;
    }
    if n < 4294967295 { Some(n as u32) } else { None }
}

pub fn hash(s: &[u16]) -> u32 {
    // FNV-1a
    let mut h: u32 = 0x811c9dc5;
    for &c in s {
        h ^= c as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}
