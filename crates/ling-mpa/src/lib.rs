//! MPEG-1 / MPEG-2 / MPEG-2.5 audio Layer I, II and III decoder for LingOS, freestanding
//! (`no_std` + `alloc`, float maths via `libm`).
//!
//! The layer decoders, synthesis filterbank and frame header parser are ported from
//! symphonia-bundle-mp3 0.5.5 (MPL-2.0, see LICENSE); the byte-stream front end (frame sync,
//! ID3v2 skipping, Xing / Info / VBRI / LAME handling, partial-frame carry) is written for LingOS.
//!
//! ```ignore
//! let mut dec = ling_mpa::Decoder::new();
//! let mut pcm = Vec::new();
//! for chunk in stream { dec.decode(chunk, &mut pcm)?; }   // interleaved f32, -1.0..1.0
//! let (rate, ch) = (dec.sample_rate(), dec.channels());
//! ```
//!
//! A Xing / Info / VBRI header frame (the first frame of most VBR and LAME-encoded files) carries
//! no audio: [`Decoder::decode`] drops it (outputs nothing for it). [`decode_file`] also honours
//! the LAME encoder delay / padding stored in it (same trimming as ffmpeg).
//!
//! Corrupt input never panics: undecodable frames whose header is valid are output as silence of
//! the frame's length (keeps A/V sync), garbage between frames is skipped.

#![cfg_attr(not(test), no_std)]
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::new_without_default)]
#![allow(dead_code)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;

mod bits;
mod common;
mod header;
mod layer1;
mod layer12;
mod layer2;
mod layer3;
mod synthesis;

use common::{AudioBuffer, FrameHeader, Layer, MpegLayer};

/// Decoder errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The bitstream is malformed.
    Invalid(&'static str),
    /// A valid feature this decoder does not implement (free-format bit rate).
    Unsupported(&'static str),
}

/// Header bits that must stay constant across the frames of one stream: sync, version, layer and
/// sample rate.
const STREAM_MASK: u32 = 0xfffe_0c00;

/// LAME / Xing information from an Info frame.
#[derive(Clone, Copy, Debug, Default)]
struct XingInfo {
    /// Number of audio frames (if given).
    frames: Option<u32>,
    /// LAME encoder delay and padding in samples (if a LAME tag was present).
    delay: Option<(u32, u32)>,
}

/// Streaming MPEG audio decoder.
pub struct Decoder {
    /// Bytes of an incomplete frame (or tag) carried to the next call.
    pending: Vec<u8>,
    /// Bytes of an ID3v2 tag still to be skipped at the start of the next call.
    skip: usize,
    /// Header of the last decoded frame (for lock-on without look-ahead).
    locked: Option<u32>,
    l1: Option<Box<layer1::Layer1>>,
    l2: Option<Box<layer2::Layer2>>,
    l3: Option<Box<layer3::Layer3>>,
    buf: AudioBuffer,
    rate: u32,
    channels: u16,
    /// Channel count of the output, fixed by the first decoded frame.
    out_channels: u16,
    frames_seen: u64,
    errors: u64,
    xing: Option<XingInfo>,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder {
            pending: Vec::new(),
            skip: 0,
            locked: None,
            l1: None,
            l2: None,
            l3: None,
            buf: AudioBuffer::new(),
            rate: 0,
            channels: 0,
            out_channels: 0,
            frames_seen: 0,
            errors: 0,
            xing: None,
        }
    }

    /// Sample rate of the last decoded frame (0 before any).
    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// Output channel count (1 or 2; 0 before any frame). Fixed by the first decoded frame: a
    /// later mono / stereo switch is up- or down-mixed to it.
    pub fn channels(&self) -> u16 {
        if self.out_channels != 0 {
            self.out_channels
        }
        else {
            self.channels
        }
    }

    /// Number of frames that had a valid header but failed to decode (output as silence).
    pub fn error_count(&self) -> u64 {
        self.errors
    }

    /// LAME encoder delay and padding (samples) from a LAME tag, if one was seen.
    pub fn lame_delay_padding(&self) -> Option<(u32, u32)> {
        self.xing.and_then(|x| x.delay)
    }

    /// Forget the partial-frame buffer and bit reservoir (after a seek). The synthesis filter
    /// history is cleared too.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.skip = 0;
        self.locked = None;
        self.l1 = None;
        self.l2 = None;
        self.l3 = None;
    }

    /// Decode every complete MPEG audio frame in `data` (one Matroska/MP4 sample, which may hold
    /// several frames back to back, or an arbitrary chunk of a raw .mp2/.mp3 stream). Appends
    /// interleaved f32 samples (-1..1) to `out`; returns how many sample frames (per channel) were
    /// appended. Resyncs past garbage, skips ID3v2 tags, keeps the Layer III bit reservoir across
    /// calls. A partial frame at the end of `data` is kept and completed by the next call.
    ///
    /// Returns an error only when nothing was appended and something went wrong (e.g. only
    /// free-format frames were found).
    pub fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) -> Result<usize, Error> {
        let mut data = data;
        if self.skip > 0 {
            let n = self.skip.min(data.len());
            self.skip -= n;
            data = &data[n..];
        }

        let mut last_err = None;
        let appended;
        if self.pending.is_empty() {
            let (consumed, n) = self.decode_buf(data, out, &mut last_err);
            appended = n;
            self.pending.extend_from_slice(&data[consumed.min(data.len())..]);
        }
        else {
            let mut pending = core::mem::take(&mut self.pending);
            pending.extend_from_slice(data);
            let (consumed, n) = self.decode_buf(&pending, out, &mut last_err);
            appended = n;
            pending.drain(..consumed.min(pending.len()));
            self.pending = pending;
        }

        match last_err {
            Some(e) if appended == 0 => Err(e),
            _ => Ok(appended),
        }
    }

    /// Decode the frames in `buf`. Returns (bytes consumed, sample frames appended); bytes after
    /// the consumed prefix must be carried to the next call.
    fn decode_buf(
        &mut self,
        buf: &[u8],
        out: &mut Vec<f32>,
        last_err: &mut Option<Error>,
    ) -> (usize, usize) {
        let mut pos = 0usize;
        let mut appended = 0usize;

        while pos < buf.len() {
            let rest = &buf[pos..];

            // ID3v2 tag ("ID3" vv ff ssss with a sync-safe size).
            if rest.len() >= 3 && &rest[..3] == b"ID3" {
                if rest.len() < 10 {
                    return (pos, appended);
                }
                if rest[3] != 0xff && rest[4] != 0xff && rest[6..10].iter().all(|&b| b < 0x80) {
                    let size = rest[6..10].iter().fold(0usize, |a, &b| (a << 7) | usize::from(b));
                    let total = 10 + size + if rest[5] & 0x10 != 0 { 10 } else { 0 };
                    self.locked = None;
                    if total > rest.len() {
                        self.skip = total - rest.len();
                        return (buf.len(), appended);
                    }
                    pos += total;
                    continue;
                }
            }

            if rest.len() < 4 {
                return (pos, appended);
            }

            let word = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
            if !header::is_frame_header_word_synced(word) || !header::check_header(word) {
                self.locked = None;
                pos += 1;
                continue;
            }

            let hdr = match header::parse_frame_header(word) {
                Ok(h) => h,
                Err(e) => {
                    if let Error::Unsupported(_) = e {
                        *last_err = Some(e);
                    }
                    self.locked = None;
                    pos += 1;
                    continue;
                }
            };

            let len = 4 + hdr.frame_size;
            let locked = matches!(self.locked, Some(l) if l & STREAM_MASK == word & STREAM_MASK);

            if !locked {
                // Not following a decoded frame: confirm the sync with the next frame's header.
                if rest.len() >= len + 4 {
                    let next =
                        u32::from_be_bytes([rest[len], rest[len + 1], rest[len + 2], rest[len + 3]]);
                    if next & STREAM_MASK != word & STREAM_MASK
                        || !header::check_header(next)
                        || header::parse_frame_header(next).is_err()
                    {
                        pos += 1;
                        continue;
                    }
                }
                else if rest.len() > len {
                    // The next header is only partly here: wait for more data.
                    return (pos, appended);
                }
            }

            if rest.len() < len {
                // Incomplete frame: carry it over.
                return (pos, appended);
            }

            let frame = &rest[4..len];
            appended += self.decode_frame(&hdr, frame, out, last_err);
            self.locked = Some(word);
            pos += len;
        }

        (pos, appended)
    }

    /// Decode one frame (`frame` excludes the 4-byte header); returns the sample frames appended.
    fn decode_frame(
        &mut self,
        hdr: &FrameHeader,
        frame: &[u8],
        out: &mut Vec<f32>,
        last_err: &mut Option<Error>,
    ) -> usize {
        self.frames_seen += 1;
        self.rate = hdr.sample_rate;
        self.channels = hdr.n_channels() as u16;

        if hdr.layer == MpegLayer::Layer3 {
            if let Some(info) = parse_xing(hdr, frame) {
                if self.xing.is_none() {
                    self.xing = Some(info);
                }
                // An Info frame carries no audio; drop it. The reservoir starts empty after it.
                if let Some(l3) = self.l3.as_mut() {
                    l3.resevoir.clear();
                }
                return 0;
            }
        }

        self.buf.clear();
        let res = match hdr.layer {
            MpegLayer::Layer1 => self
                .l1
                .get_or_insert_with(layer1::Layer1::new_boxed)
                .decode(frame, hdr, &mut self.buf),
            MpegLayer::Layer2 => self
                .l2
                .get_or_insert_with(layer2::Layer2::new_boxed)
                .decode(frame, hdr, &mut self.buf),
            MpegLayer::Layer3 => self
                .l3
                .get_or_insert_with(|| Box::new(layer3::Layer3::new()))
                .decode(frame, hdr, &mut self.buf),
        };

        let n = hdr.duration() as usize;
        let src_ch = hdr.n_channels();
        if self.out_channels == 0 {
            self.out_channels = src_ch as u16;
        }
        let dst_ch = usize::from(self.out_channels);

        if let Err(e) = res {
            *last_err = Some(e);
            self.errors += 1;
            out.resize(out.len() + n * dst_ch, 0.0);
            return n;
        }

        let frames = self.buf.frames().min(n);
        out.reserve(frames * dst_ch);
        let l = self.buf.chan(0);
        let r = self.buf.chan(if src_ch > 1 { 1 } else { 0 });
        match (src_ch, dst_ch) {
            (1, 1) => out.extend_from_slice(&l[..frames]),
            (_, 1) => out.extend(l[..frames].iter().zip(&r[..frames]).map(|(a, b)| 0.5 * (a + b))),
            _ => {
                for i in 0..frames {
                    out.push(l[i]);
                    out.push(r[i]);
                }
            }
        }
        frames
    }
}

/// Look for a Xing / Info (or VBRI) header in a Layer III frame (`frame` excludes the 4-byte
/// header).
fn parse_xing(hdr: &FrameHeader, frame: &[u8]) -> Option<XingInfo> {
    let crc = if hdr.has_crc { 2 } else { 0 };
    let is_tag = |o: usize| matches!(frame.get(o..o + 4), Some(b"Xing") | Some(b"Info"));
    // LAME writes the tag right after the side info even when the frame has a CRC word (and
    // ffmpeg looks there); accept it after the CRC too.
    let off = if is_tag(hdr.side_info_len()) {
        Some(hdr.side_info_len())
    }
    else if crc != 0 && is_tag(crc + hdr.side_info_len()) {
        Some(crc + hdr.side_info_len())
    }
    else {
        None
    };
    if let Some(off) = off {
        let be32 = |o: usize| -> Option<u32> {
            let b = frame.get(o..o + 4)?;
            Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        };
        let flags = be32(off + 4)?;
        let mut p = off + 8;
        let mut info = XingInfo::default();
        if flags & 1 != 0 {
            info.frames = be32(p);
            p += 4;
        }
        if flags & 2 != 0 {
            p += 4;
        }
        if flags & 4 != 0 {
            p += 100;
        }
        if flags & 8 != 0 {
            p += 4;
        }
        // LAME tag: 9-byte encoder string, then delay / padding (12 bits each) at +21.
        if let Some(lame) = frame.get(p..p + 24) {
            let enc_ok = lame[0] == b'L' && lame[1..4].iter().all(|c| c.is_ascii_alphanumeric());
            if enc_ok {
                let d = (u32::from(lame[21]) << 4) | (u32::from(lame[22]) >> 4);
                let pad = ((u32::from(lame[22]) & 0xf) << 8) | u32::from(lame[23]);
                info.delay = Some((d, pad));
            }
        }
        return Some(info);
    }
    // VBRI (Fraunhofer) sits 32 bytes after the header regardless of channel mode and CRC.
    if frame.get(32..36) == Some(b"VBRI") {
        let b = frame.get(46..50)?;
        return Some(XingInfo {
            frames: Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]])),
            delay: None,
        });
    }
    None
}

/// Parse a frame header at the start of `b`: (frame length in bytes, sample rate, channels,
/// samples per frame).
pub fn frame_info(b: &[u8]) -> Option<(usize, u32, u16, usize)> {
    let w = u32::from_be_bytes([*b.first()?, *b.get(1)?, *b.get(2)?, *b.get(3)?]);
    if !header::is_frame_header_word_synced(w) || !header::check_header(w) {
        return None;
    }
    let h = header::parse_frame_header(w).ok()?;
    Some((4 + h.frame_size, h.sample_rate, h.n_channels() as u16, h.duration() as usize))
}

/// Decode a whole .mp3/.mp2 file: (rate, channels, interleaved i16). For the desktop's music
/// player. The Xing/Info frame is dropped and the LAME encoder delay / padding is trimmed the
/// way ffmpeg does (delay + 529 decoder samples at the start, padding at the end).
pub fn decode_file(data: &[u8]) -> Option<(u32, u16, Vec<i16>)> {
    let mut dec = Decoder::new();
    let mut pcm = Vec::new();
    let _ = dec.decode(data, &mut pcm);
    let ch = usize::from(dec.channels());
    if pcm.is_empty() || dec.rate == 0 || ch == 0 {
        return None;
    }
    let mut start = 0usize;
    let mut end = pcm.len() / ch;
    if let Some(XingInfo { frames, delay: Some((delay, padding)) }) = dec.xing {
        start = (delay as usize + 529).min(end);
        if let Some(f) = frames {
            // Samples per Layer III frame: 1152 for MPEG-1, 576 for MPEG-2 / 2.5.
            let spf = if dec.rate >= 32_000 { 1152 } else { 576 };
            let total = (f as usize * spf).saturating_sub(delay as usize + padding as usize);
            end = end.min(start + total);
        }
        else {
            end = end.saturating_sub(padding as usize).max(start);
        }
    }
    let out = pcm[start * ch..end * ch]
        .iter()
        .map(|&s| libm::roundf(s * 32768.0).clamp(-32768.0, 32767.0) as i16)
        .collect();
    Some((dec.rate, dec.channels(), out))
}

#[cfg(test)]
mod tests;
