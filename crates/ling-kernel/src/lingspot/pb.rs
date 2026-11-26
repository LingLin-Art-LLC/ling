//! Just enough protobuf for Spotify's messages, written by hand: a writer that
//! appends fields, and a reader over (field, value) pairs.

use alloc::vec::Vec;

pub struct W(pub Vec<u8>);

impl W {
    pub fn new() -> W {
        W(Vec::new())
    }
    pub fn varint_raw(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }
    fn key(&mut self, field: u32, wt: u8) {
        self.varint_raw(((field as u64) << 3) | wt as u64);
    }
    pub fn uint(&mut self, field: u32, v: u64) -> &mut Self {
        self.key(field, 0);
        self.varint_raw(v);
        self
    }
    /// int32 / enum: negative values are 10-byte varints
    pub fn int(&mut self, field: u32, v: i64) -> &mut Self {
        self.uint(field, v as u64)
    }
    pub fn bool(&mut self, field: u32, v: bool) -> &mut Self {
        self.uint(field, v as u64)
    }
    pub fn bytes(&mut self, field: u32, b: &[u8]) -> &mut Self {
        self.key(field, 2);
        self.varint_raw(b.len() as u64);
        self.0.extend_from_slice(b);
        self
    }
    pub fn str(&mut self, field: u32, s: &str) -> &mut Self {
        self.bytes(field, s.as_bytes())
    }
    pub fn msg(&mut self, field: u32, m: &W) -> &mut Self {
        self.bytes(field, &m.0)
    }
    pub fn fixed64(&mut self, field: u32, v: u64) -> &mut Self {
        self.key(field, 1);
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn done(&self) -> Vec<u8> {
        self.0.clone()
    }
}

/// One field: its number and its value (varint / fixed as a number, length-delimited as bytes).
#[derive(Clone, Copy)]
pub enum V<'a> {
    N(u64),
    B(&'a [u8]),
}

pub fn read_varint(b: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let x = *b.get(*at)?;
        *at += 1;
        v |= ((x & 0x7f) as u64) << shift;
        if x & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// All fields of a message, in order (None when it is not protobuf).
pub fn fields(b: &[u8]) -> Option<Vec<(u32, V<'_>)>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < b.len() {
        let k = read_varint(b, &mut at)?;
        let f = (k >> 3) as u32;
        let v = match k & 7 {
            0 => V::N(read_varint(b, &mut at)?),
            1 => {
                let s = b.get(at..at + 8)?;
                at += 8;
                V::N(u64::from_le_bytes(s.try_into().ok()?))
            },
            2 => {
                let n = read_varint(b, &mut at)? as usize;
                let s = b.get(at..at.checked_add(n)?)?;
                at += n;
                V::B(s)
            },
            5 => {
                let s = b.get(at..at + 4)?;
                at += 4;
                V::N(u32::from_le_bytes(s.try_into().ok()?) as u64)
            },
            _ => return None,
        };
        out.push((f, v));
    }
    Some(out)
}

/// The first length-delimited field `f`.
pub fn get_b(b: &[u8], f: u32) -> Option<&[u8]> {
    fields(b)?.into_iter().find_map(|(k, v)| match v {
        V::B(x) if k == f => Some(x),
        _ => None,
    })
}

/// Every length-delimited field `f`.
pub fn get_all_b(b: &[u8], f: u32) -> Vec<&[u8]> {
    fields(b)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| match v {
            V::B(x) if k == f => Some(x),
            _ => None,
        })
        .collect()
}

/// The first varint field `f`.
pub fn get_n(b: &[u8], f: u32) -> Option<u64> {
    fields(b)?.into_iter().find_map(|(k, v)| match v {
        V::N(x) if k == f => Some(x),
        _ => None,
    })
}

/// Follow a path of nested message fields.
pub fn path<'a>(mut b: &'a [u8], p: &[u32]) -> Option<&'a [u8]> {
    for &f in p {
        b = get_b(b, f)?;
    }
    Some(b)
}

pub fn get_str(b: &[u8], f: u32) -> alloc::string::String {
    get_b(b, f).map(|x| alloc::string::String::from_utf8_lossy(x).into_owned()).unwrap_or_default()
}
