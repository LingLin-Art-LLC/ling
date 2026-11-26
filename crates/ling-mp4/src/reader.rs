//! Bounds-checked big-endian cursor and box header parsing.

use crate::Mp4Error;

#[derive(Clone)]
pub struct Cursor<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], Mp4Error> {
        if n > self.remaining() {
            return Err(Mp4Error::Invalid("box payload truncated"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn skip(&mut self, n: usize) -> Result<(), Mp4Error> {
        self.bytes(n).map(|_| ())
    }

    pub fn u8(&mut self) -> Result<u8, Mp4Error> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, Mp4Error> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    #[allow(dead_code)]
    pub fn u24(&mut self) -> Result<u32, Mp4Error> {
        let b = self.bytes(3)?;
        Ok(u32::from_be_bytes([0, b[0], b[1], b[2]]))
    }

    pub fn u32(&mut self) -> Result<u32, Mp4Error> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64, Mp4Error> {
        let b = self.bytes(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }

    pub fn fourcc(&mut self) -> Result<[u8; 4], Mp4Error> {
        let b = self.bytes(4)?;
        Ok([b[0], b[1], b[2], b[3]])
    }

    /// Version (8 bits) + flags (24 bits) of a full box.
    pub fn full_header(&mut self) -> Result<(u8, u32), Mp4Error> {
        let v = self.u32()?;
        Ok(((v >> 24) as u8, v & 0x00ff_ffff))
    }
}

/// A box header as found in a byte buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxHeader {
    /// Four-character box type, e.g. `*b"moov"`.
    pub kind: [u8; 4],
    /// Header length in bytes (8, 16 for a 64-bit size, +16 for `uuid`).
    pub header_len: usize,
    /// Total box size including the header. `None` = size 0 ("to the end of the file").
    pub size: Option<u64>,
}

impl BoxHeader {
    /// Parse a box header at the start of `buf`. `Ok(None)` when `buf` is too
    /// short to hold the header.
    pub fn parse(buf: &[u8]) -> Result<Option<BoxHeader>, Mp4Error> {
        if buf.len() < 8 {
            return Ok(None);
        }
        let size32 = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let kind = [buf[4], buf[5], buf[6], buf[7]];
        let (mut header_len, size) = match size32 {
            0 => (8, None),
            1 => {
                if buf.len() < 16 {
                    return Ok(None);
                }
                let mut a = [0u8; 8];
                a.copy_from_slice(&buf[8..16]);
                (16, Some(u64::from_be_bytes(a)))
            }
            s => (8, Some(u64::from(s))),
        };
        if &kind == b"uuid" {
            header_len += 16;
            if buf.len() < header_len {
                return Ok(None);
            }
        }
        if let Some(s) = size {
            if s < header_len as u64 {
                return Err(Mp4Error::Invalid("box size smaller than its header"));
            }
        }
        Ok(Some(BoxHeader { kind, header_len, size }))
    }
}

/// Iterate the child boxes inside a complete parent payload.
pub struct Children<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Children<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
}

impl<'a> Iterator for Children<'a> {
    /// (box type, payload) or an error for a malformed child (iteration stops after it).
    type Item = Result<([u8; 4], &'a [u8]), Mp4Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = &self.buf[self.pos..];
        if rest.len() < 8 {
            // Trailing padding (or nothing) inside a parent: ignore.
            self.pos = self.buf.len();
            return None;
        }
        let hdr = match BoxHeader::parse(rest) {
            Ok(Some(h)) => h,
            Ok(None) => {
                self.pos = self.buf.len();
                return Some(Err(Mp4Error::Invalid("child box header truncated")));
            }
            Err(e) => {
                self.pos = self.buf.len();
                return Some(Err(e));
            }
        };
        let size = hdr.size.unwrap_or(rest.len() as u64);
        if size > rest.len() as u64 {
            self.pos = self.buf.len();
            return Some(Err(Mp4Error::Invalid("child box overruns its parent")));
        }
        let size = size as usize;
        self.pos += size;
        Some(Ok((hdr.kind, &rest[hdr.header_len..size])))
    }
}

/// Find the first child of the given type.
pub fn child<'a>(buf: &'a [u8], kind: &[u8; 4]) -> Result<Option<&'a [u8]>, Mp4Error> {
    for c in Children::new(buf) {
        let (k, p) = c?;
        if &k == kind {
            return Ok(Some(p));
        }
    }
    Ok(None)
}
