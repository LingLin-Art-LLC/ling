//! ling-mkv: a Matroska / WebM demuxer for LingOS (no_std + alloc).
//!
//! It reads straight from a random-access [`Source`] (a file on a FAT / exFAT
//! volume, possibly many GiB), never the whole file:
//!
//! * [`open`] reads the EBML header, the Segment's SeekHead, Info (timestamp
//!   scale, duration, title), Tracks (codec id, CodecPrivate, video / audio
//!   parameters, language, name, default / forced flags, content compression),
//!   Cues (the seek index) and Chapters (the default edition, flattened), and
//!   finds the first Cluster.
//! * [`Reader`] walks Clusters in file order and hands out [`Packet`]s:
//!   SimpleBlocks and BlockGroups (with BlockDuration / ReferenceBlock), split
//!   along Xiph, EBML and fixed-size lacing, with header-stripping compression
//!   undone. Unknown-size Clusters (live recordings) are handled.
//! * [`Reader::seek`] jumps to the Cue point at or before a time (or hops from
//!   Cluster to Cluster by their sizes when the file has no Cues).
//!
//! zlib-compressed tracks (`ContentCompAlgo` 0, common for subtitles) are
//! reported in [`Track::zlib`]; inflating is the caller's job.

#![no_std]
#![allow(clippy::manual_range_contains, clippy::type_complexity)]

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

// ── element ids ──────────────────────────────────────────────────────────────

pub mod id {
    pub const EBML: u32 = 0x1A45DFA3;
    pub const DOC_TYPE: u32 = 0x4282;
    pub const SEGMENT: u32 = 0x18538067;
    pub const SEEK_HEAD: u32 = 0x114D9B74;
    pub const SEEK: u32 = 0x4DBB;
    pub const SEEK_ID: u32 = 0x53AB;
    pub const SEEK_POSITION: u32 = 0x53AC;
    pub const INFO: u32 = 0x1549A966;
    pub const TIMESTAMP_SCALE: u32 = 0x2AD7B1;
    pub const DURATION: u32 = 0x4489;
    pub const TITLE: u32 = 0x7BA9;
    pub const TRACKS: u32 = 0x1654AE6B;
    pub const TRACK_ENTRY: u32 = 0xAE;
    pub const TRACK_NUMBER: u32 = 0xD7;
    pub const TRACK_TYPE: u32 = 0x83;
    pub const FLAG_ENABLED: u32 = 0xB9;
    pub const FLAG_DEFAULT: u32 = 0x88;
    pub const FLAG_FORCED: u32 = 0x55AA;
    pub const DEFAULT_DURATION: u32 = 0x23E383;
    pub const NAME: u32 = 0x536E;
    pub const LANGUAGE: u32 = 0x22B59C;
    pub const LANGUAGE_BCP47: u32 = 0x22B59D;
    pub const CODEC_ID: u32 = 0x86;
    pub const CODEC_PRIVATE: u32 = 0x63A2;
    pub const CODEC_NAME: u32 = 0x258688;
    pub const CODEC_DELAY: u32 = 0x56AA;
    pub const SEEK_PRE_ROLL: u32 = 0x56BB;
    pub const VIDEO: u32 = 0xE0;
    pub const PIXEL_WIDTH: u32 = 0xB0;
    pub const PIXEL_HEIGHT: u32 = 0xBA;
    pub const DISPLAY_WIDTH: u32 = 0x54B0;
    pub const DISPLAY_HEIGHT: u32 = 0x54BA;
    pub const DISPLAY_UNIT: u32 = 0x54B2;
    pub const FLAG_INTERLACED: u32 = 0x9A;
    pub const PIXEL_CROP_BOTTOM: u32 = 0x54AA;
    pub const PIXEL_CROP_TOP: u32 = 0x54BB;
    pub const PIXEL_CROP_LEFT: u32 = 0x54CC;
    pub const PIXEL_CROP_RIGHT: u32 = 0x54DD;
    pub const AUDIO: u32 = 0xE1;
    pub const SAMPLING_FREQUENCY: u32 = 0xB5;
    pub const OUTPUT_SAMPLING_FREQUENCY: u32 = 0x78B5;
    pub const CHANNELS: u32 = 0x9F;
    pub const BIT_DEPTH: u32 = 0x6264;
    pub const CONTENT_ENCODINGS: u32 = 0x6D80;
    pub const CONTENT_ENCODING: u32 = 0x6240;
    pub const CONTENT_ENCODING_SCOPE: u32 = 0x5032;
    pub const CONTENT_ENCODING_TYPE: u32 = 0x5033;
    pub const CONTENT_COMPRESSION: u32 = 0x5034;
    pub const CONTENT_COMP_ALGO: u32 = 0x4254;
    pub const CONTENT_COMP_SETTINGS: u32 = 0x4255;
    pub const CONTENT_ENCRYPTION: u32 = 0x5035;
    pub const CLUSTER: u32 = 0x1F43B675;
    pub const TIMESTAMP: u32 = 0xE7;
    pub const SIMPLE_BLOCK: u32 = 0xA3;
    pub const BLOCK_GROUP: u32 = 0xA0;
    pub const BLOCK: u32 = 0xA1;
    pub const BLOCK_DURATION: u32 = 0x9B;
    pub const REFERENCE_BLOCK: u32 = 0xFB;
    pub const CUES: u32 = 0x1C53BB6B;
    pub const CUE_POINT: u32 = 0xBB;
    pub const CUE_TIME: u32 = 0xB3;
    pub const CUE_TRACK_POSITIONS: u32 = 0xB7;
    pub const CUE_TRACK: u32 = 0xF7;
    pub const CUE_CLUSTER_POSITION: u32 = 0xF1;
    pub const CHAPTERS: u32 = 0x1043A770;
    pub const EDITION_ENTRY: u32 = 0x45B9;
    pub const EDITION_FLAG_DEFAULT: u32 = 0x45DB;
    pub const EDITION_FLAG_HIDDEN: u32 = 0x45BD;
    pub const CHAPTER_ATOM: u32 = 0xB6;
    pub const CHAPTER_TIME_START: u32 = 0x91;
    pub const CHAPTER_TIME_END: u32 = 0x92;
    pub const CHAPTER_FLAG_HIDDEN: u32 = 0x98;
    pub const CHAPTER_FLAG_ENABLED: u32 = 0x4598;
    pub const CHAPTER_DISPLAY: u32 = 0x80;
    pub const CHAP_STRING: u32 = 0x85;
    pub const CHAP_LANGUAGE: u32 = 0x437C;
    pub const ATTACHMENTS: u32 = 0x1941A469;
    pub const TAGS: u32 = 0x1254C367;
    pub const VOID: u32 = 0xEC;
    pub const CRC32: u32 = 0xBF;
}

/// Random access to the file's bytes.
pub trait Source {
    fn len(&self) -> u64;
    /// Fill `buf` from `off`; false on a read error or past the end.
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> bool;
}

impl Source for &[u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> bool {
        if off > usize::MAX as u64 {
            return false;
        }
        let s = off as usize;
        match self.get(s..s.saturating_add(buf.len())) {
            Some(b) => {
                buf.copy_from_slice(b);
                true
            },
            None => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Not a Matroska / WebM file.
    NotMatroska,
    /// The file is cut short or damaged where it matters.
    Corrupt(&'static str),
    Io,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
    Subtitle,
    Other,
}

#[derive(Debug, Clone, Default)]
pub struct VideoInfo {
    pub pixel_w: u32,
    pub pixel_h: u32,
    /// Display size (0 when not given = the pixel size); `display_unit` 0 = pixels, 3 = aspect ratio.
    pub display_w: u32,
    pub display_h: u32,
    pub display_unit: u32,
    /// 0 undetermined, 1 interlaced, 2 progressive
    pub interlaced: u32,
    /// top, bottom, left, right
    pub crop: [u32; 4],
}

#[derive(Debug, Clone, Default)]
pub struct AudioInfo {
    pub rate: f64,
    pub out_rate: f64,
    pub channels: u32,
    pub bit_depth: u32,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub number: u64,
    pub kind: Kind,
    pub codec_id: String,
    pub codec_private: Vec<u8>,
    pub codec_name: String,
    pub name: String,
    /// ISO 639-2 ("eng", "jpn", "und") or BCP 47 when given.
    pub language: String,
    pub default: bool,
    pub forced: bool,
    pub enabled: bool,
    /// Nanoseconds per frame, 0 when not given.
    pub default_duration: u64,
    pub codec_delay: u64,
    pub seek_preroll: u64,
    pub video: VideoInfo,
    pub audio: AudioInfo,
    /// Bytes stripped from the start of every frame (header-stripping compression), put back by the Reader.
    pub strip: Vec<u8>,
    /// Frames are zlib-compressed (the caller inflates).
    pub zlib: bool,
    /// Frames are encrypted or use a compression LingOS lacks (unplayable).
    pub encrypted: bool,
}

impl Track {
    fn new() -> Track {
        Track {
            number: 0,
            kind: Kind::Other,
            codec_id: String::new(),
            codec_private: Vec::new(),
            codec_name: String::new(),
            name: String::new(),
            language: String::from("eng"),
            default: true,
            forced: false,
            enabled: true,
            default_duration: 0,
            codec_delay: 0,
            seek_preroll: 0,
            video: VideoInfo::default(),
            audio: AudioInfo::default(),
            strip: Vec::new(),
            zlib: false,
            encrypted: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Chapter {
    /// nanoseconds
    pub start: u64,
    pub end: Option<u64>,
    pub title: String,
}

#[derive(Debug, Clone, Copy)]
pub struct CuePoint {
    /// nanoseconds
    pub time: u64,
    pub track: u64,
    /// absolute file offset of the Cluster
    pub cluster: u64,
}

#[derive(Debug, Clone)]
pub struct Info {
    pub doc_type: String,
    /// nanoseconds per timestamp tick
    pub timescale: u64,
    /// nanoseconds (0 when unknown)
    pub duration: u64,
    pub title: String,
    pub tracks: Vec<Track>,
    pub chapters: Vec<Chapter>,
    pub cues: Vec<CuePoint>,
    /// Absolute offset of the Segment's data (SeekPosition / CueClusterPosition are relative to it).
    pub segment_start: u64,
    /// End of the Segment (the file's end when its size is unknown).
    pub segment_end: u64,
    /// Absolute offset of the first Cluster.
    pub first_cluster: u64,
}

impl Info {
    pub fn track(&self, number: u64) -> Option<&Track> {
        self.tracks.iter().find(|t| t.number == number)
    }
}

/// One frame of one track.
#[derive(Debug, Clone)]
pub struct Packet {
    pub track: u64,
    /// Presentation time, nanoseconds (may be negative for a codec delay).
    pub pts: i64,
    /// nanoseconds, when the block or the track said
    pub duration: Option<u64>,
    pub key: bool,
    pub data: Vec<u8>,
}

// ── EBML primitives ──────────────────────────────────────────────────────────

/// A variable-length integer at the start of `b`: (value with the marker removed, length, all ones?).
pub fn vint(b: &[u8]) -> Option<(u64, usize, bool)> {
    let first = *b.first()?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if b.len() < len {
        return None;
    }
    let mask = (0xFFu32 >> len) as u64;
    let mut v = (first as u64) & mask;
    let mut ones = v == mask;
    for &x in &b[1..len] {
        v = (v << 8) | x as u64;
        ones &= x == 0xFF;
    }
    Some((v, len, ones))
}

/// An element id (marker bits kept) at the start of `b`: (id, length).
pub fn read_id(b: &[u8]) -> Option<(u32, usize)> {
    let first = *b.first()?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 4 || b.len() < len {
        return None;
    }
    let mut v = 0u32;
    for &x in &b[..len] {
        v = (v << 8) | x as u32;
    }
    Some((v, len))
}

/// An element header: (id, data size or None for unknown, header length).
pub fn element_header(b: &[u8]) -> Option<(u32, Option<u64>, usize)> {
    let (eid, il) = read_id(b)?;
    let (sz, sl, ones) = vint(&b[il..])?;
    Some((eid, if ones { None } else { Some(sz) }, il + sl))
}

fn uint(b: &[u8]) -> u64 {
    b.iter().take(8).fold(0u64, |a, &x| (a << 8) | x as u64)
}

fn sint(b: &[u8]) -> i64 {
    if b.is_empty() {
        return 0;
    }
    let n = b.len().min(8);
    let u = uint(&b[..n]);
    let shift = 64 - 8 * n as u32;
    if shift == 64 {
        return u as i64;
    }
    ((u << shift) as i64) >> shift
}

fn float(b: &[u8]) -> f64 {
    match b.len() {
        4 => f32::from_bits(uint(b) as u32) as f64,
        8 => f64::from_bits(uint(b)),
        _ => 0.0,
    }
}

fn string(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// The child elements of a master element's body: (id, body).
pub fn children(b: &[u8]) -> Children<'_> {
    Children { b, pos: 0 }
}

pub struct Children<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for Children<'a> {
    type Item = (u32, &'a [u8]);
    fn next(&mut self) -> Option<(u32, &'a [u8])> {
        let rest = self.b.get(self.pos..)?;
        let (eid, sz, hl) = element_header(rest)?;
        let avail = rest.len() - hl;
        let sz = sz.map(|s| s.min(avail as u64) as usize).unwrap_or(avail);
        let body = &rest[hl..hl + sz];
        self.pos += hl + sz;
        Some((eid, body))
    }
}

// ── buffered reading ─────────────────────────────────────────────────────────

const WINDOW: usize = 256 * 1024;

struct Buf {
    off: u64,
    data: Vec<u8>,
}

impl Buf {
    fn new() -> Buf {
        Buf { off: 0, data: Vec::new() }
    }

    /// Up to `n` bytes at `off` (fewer near the end of the file).
    fn get<S: Source + ?Sized>(&mut self, src: &mut S, off: u64, n: usize) -> Option<&[u8]> {
        let len = src.len();
        if off >= len {
            return None;
        }
        let n = n.min((len - off) as usize);
        let have = off >= self.off && off + n as u64 <= self.off + self.data.len() as u64;
        if !have {
            let want = n.max(WINDOW).min((len - off) as usize);
            self.data.resize(want, 0);
            if !src.read_at(off, &mut self.data) {
                self.data.clear();
                return None;
            }
            self.off = off;
        }
        let s = (off - self.off) as usize;
        Some(&self.data[s..s + n])
    }

    /// An element header at `off`: (id, size, header length).
    fn header<S: Source + ?Sized>(&mut self, src: &mut S, off: u64) -> Option<(u32, Option<u64>, usize)> {
        element_header(self.get(src, off, 12)?)
    }

    /// A whole element body (through the window when it fits in one).
    fn body<S: Source + ?Sized>(&mut self, src: &mut S, off: u64, size: u64) -> Option<Vec<u8>> {
        if size > MAX_META || off.checked_add(size)? > src.len() {
            return None;
        }
        if (size as usize) <= WINDOW {
            return self.get(src, off, size as usize).filter(|b| b.len() as u64 == size).map(|b| b.to_vec());
        }
        read_body(src, off, size)
    }
}

/// Element bodies bigger than this are not read whole (bad sizes, Attachments).
const MAX_META: u64 = 64 << 20;

/// Read a whole element body at `off` (`size` bytes).
fn read_body<S: Source + ?Sized>(src: &mut S, off: u64, size: u64) -> Option<Vec<u8>> {
    if size > MAX_META || off.checked_add(size)? > src.len() {
        return None;
    }
    let mut v = vec![0u8; size as usize];
    if !src.read_at(off, &mut v) {
        return None;
    }
    Some(v)
}

// ── the headers ──────────────────────────────────────────────────────────────

fn parse_track(b: &[u8]) -> Track {
    let mut t = Track::new();
    for (eid, v) in children(b) {
        match eid {
            id::TRACK_NUMBER => t.number = uint(v),
            id::TRACK_TYPE => {
                t.kind = match uint(v) {
                    1 => Kind::Video,
                    2 => Kind::Audio,
                    0x11 => Kind::Subtitle,
                    _ => Kind::Other,
                }
            },
            id::FLAG_ENABLED => t.enabled = uint(v) != 0,
            id::FLAG_DEFAULT => t.default = uint(v) != 0,
            id::FLAG_FORCED => t.forced = uint(v) != 0,
            id::DEFAULT_DURATION => t.default_duration = uint(v),
            id::NAME => t.name = string(v),
            id::LANGUAGE => t.language = string(v),
            id::LANGUAGE_BCP47 => t.language = string(v),
            id::CODEC_ID => t.codec_id = string(v),
            id::CODEC_PRIVATE => t.codec_private = v.to_vec(),
            id::CODEC_NAME => t.codec_name = string(v),
            id::CODEC_DELAY => t.codec_delay = uint(v),
            id::SEEK_PRE_ROLL => t.seek_preroll = uint(v),
            id::VIDEO => {
                for (e, x) in children(v) {
                    match e {
                        id::PIXEL_WIDTH => t.video.pixel_w = uint(x) as u32,
                        id::PIXEL_HEIGHT => t.video.pixel_h = uint(x) as u32,
                        id::DISPLAY_WIDTH => t.video.display_w = uint(x) as u32,
                        id::DISPLAY_HEIGHT => t.video.display_h = uint(x) as u32,
                        id::DISPLAY_UNIT => t.video.display_unit = uint(x) as u32,
                        id::FLAG_INTERLACED => t.video.interlaced = uint(x) as u32,
                        id::PIXEL_CROP_TOP => t.video.crop[0] = uint(x) as u32,
                        id::PIXEL_CROP_BOTTOM => t.video.crop[1] = uint(x) as u32,
                        id::PIXEL_CROP_LEFT => t.video.crop[2] = uint(x) as u32,
                        id::PIXEL_CROP_RIGHT => t.video.crop[3] = uint(x) as u32,
                        _ => {},
                    }
                }
            },
            id::AUDIO => {
                t.audio.rate = 8000.0;
                t.audio.channels = 1;
                for (e, x) in children(v) {
                    match e {
                        id::SAMPLING_FREQUENCY => t.audio.rate = float(x),
                        id::OUTPUT_SAMPLING_FREQUENCY => t.audio.out_rate = float(x),
                        id::CHANNELS => t.audio.channels = uint(x) as u32,
                        id::BIT_DEPTH => t.audio.bit_depth = uint(x) as u32,
                        _ => {},
                    }
                }
            },
            id::CONTENT_ENCODINGS => {
                for (e, enc) in children(v) {
                    if e != id::CONTENT_ENCODING {
                        continue;
                    }
                    let mut scope = 1;
                    let mut kind = 0;
                    let mut algo = None;
                    let mut settings: &[u8] = &[];
                    for (e2, x) in children(enc) {
                        match e2 {
                            id::CONTENT_ENCODING_SCOPE => scope = uint(x),
                            id::CONTENT_ENCODING_TYPE => kind = uint(x),
                            id::CONTENT_COMPRESSION => {
                                algo = Some(0);
                                for (e3, y) in children(x) {
                                    match e3 {
                                        id::CONTENT_COMP_ALGO => algo = Some(uint(y)),
                                        id::CONTENT_COMP_SETTINGS => settings = y,
                                        _ => {},
                                    }
                                }
                            },
                            id::CONTENT_ENCRYPTION => kind = 1,
                            _ => {},
                        }
                    }
                    if scope & 1 == 0 {
                        continue; // only the private data is encoded
                    }
                    if kind == 1 {
                        t.encrypted = true;
                    } else if algo == Some(3) {
                        t.strip = settings.to_vec();
                    } else if algo == Some(0) {
                        t.zlib = true;
                    } else if algo.is_some() {
                        t.encrypted = true; // bzlib / lzo1x: unplayable here
                    }
                }
            },
            _ => {},
        }
    }
    if t.kind == Kind::Other {
        // some muxers leave TrackType out: guess from the codec
        t.kind = match t.codec_id.as_bytes().first() {
            Some(b'V') => Kind::Video,
            Some(b'A') => Kind::Audio,
            Some(b'S') => Kind::Subtitle,
            _ => Kind::Other,
        };
    }
    t
}

/// Cue points (times in ticks; `open` scales them).
fn parse_cues(b: &[u8], seg: u64, out: &mut Vec<CuePoint>) {
    for (eid, cp) in children(b) {
        if eid != id::CUE_POINT {
            continue;
        }
        let mut time = 0u64;
        let mut tps: Vec<(u64, u64)> = Vec::new();
        for (e, v) in children(cp) {
            match e {
                id::CUE_TIME => time = uint(v),
                id::CUE_TRACK_POSITIONS => {
                    let (mut tr, mut pos) = (0u64, u64::MAX);
                    for (e2, x) in children(v) {
                        match e2 {
                            id::CUE_TRACK => tr = uint(x),
                            id::CUE_CLUSTER_POSITION => pos = uint(x),
                            _ => {},
                        }
                    }
                    if pos != u64::MAX {
                        tps.push((tr, pos));
                    }
                },
                _ => {},
            }
        }
        for (tr, pos) in tps {
            out.push(CuePoint { time, track: tr, cluster: seg.saturating_add(pos) });
        }
    }
}

fn parse_chapters(b: &[u8], out: &mut Vec<Chapter>) {
    // the default edition (else the first that is not hidden, else the first)
    let mut editions: Vec<(&[u8], bool, bool)> = Vec::new();
    for (eid, ed) in children(b) {
        if eid != id::EDITION_ENTRY {
            continue;
        }
        let mut def = false;
        let mut hidden = false;
        for (e, v) in children(ed) {
            match e {
                id::EDITION_FLAG_DEFAULT => def = uint(v) != 0,
                id::EDITION_FLAG_HIDDEN => hidden = uint(v) != 0,
                _ => {},
            }
        }
        editions.push((ed, def, hidden));
    }
    let pick = editions.iter().find(|e| e.1).or_else(|| editions.iter().find(|e| !e.2)).or(editions.first());
    let Some(&(ed, _, _)) = pick else { return };
    for (e, atom) in children(ed) {
        if e != id::CHAPTER_ATOM {
            continue;
        }
        let mut c = Chapter { start: 0, end: None, title: String::new() };
        let mut hidden = false;
        let mut enabled = true;
        for (e2, v) in children(atom) {
            match e2 {
                id::CHAPTER_TIME_START => c.start = uint(v),
                id::CHAPTER_TIME_END => c.end = Some(uint(v)),
                id::CHAPTER_FLAG_HIDDEN => hidden = uint(v) != 0,
                id::CHAPTER_FLAG_ENABLED => enabled = uint(v) != 0,
                id::CHAPTER_DISPLAY => {
                    if c.title.is_empty() {
                        for (e3, x) in children(v) {
                            if e3 == id::CHAP_STRING {
                                c.title = string(x);
                            }
                        }
                    }
                },
                _ => {},
            }
        }
        if !hidden && enabled {
            out.push(c);
        }
    }
    out.sort_by_key(|c| c.start);
}

/// Where the SeekHead points.
#[derive(Default)]
struct Seeks {
    cues: Option<u64>,
    chapters: Option<u64>,
    heads: Vec<u64>,
}

fn parse_seekhead(body: &[u8], seg: u64, s: &mut Seeks) {
    for (e2, sk) in children(body) {
        if e2 != id::SEEK {
            continue;
        }
        let (mut sid, mut spos) = (0u32, u64::MAX);
        for (e3, v) in children(sk) {
            match e3 {
                id::SEEK_ID => sid = uint(v) as u32,
                id::SEEK_POSITION => spos = uint(v),
                _ => {},
            }
        }
        if spos == u64::MAX {
            continue;
        }
        let at = seg.saturating_add(spos);
        match sid {
            id::CUES => s.cues = Some(at),
            id::CHAPTERS => s.chapters = Some(at),
            id::SEEK_HEAD if !s.heads.contains(&at) => s.heads.push(at),
            _ => {},
        }
    }
}

/// Read the file's headers (see the crate docs).
pub fn open<S: Source + ?Sized>(src: &mut S) -> Result<Info, Error> {
    let mut buf = Buf::new();
    let (eid, sz, hl) = buf.header(src, 0).ok_or(Error::NotMatroska)?;
    if eid != id::EBML {
        return Err(Error::NotMatroska);
    }
    let ebml_size = sz.ok_or(Error::Corrupt("EBML header size"))?;
    let head = buf.body(src, hl as u64, ebml_size).ok_or(Error::Io)?;
    let mut doc_type = String::from("matroska");
    for (e, v) in children(&head) {
        if e == id::DOC_TYPE {
            doc_type = string(v);
        }
    }
    if doc_type != "matroska" && doc_type != "webm" {
        return Err(Error::NotMatroska);
    }
    // the Segment (skipping anything before it)
    let mut off = hl as u64 + ebml_size;
    let file_len = src.len();
    let mut guard = 0;
    let (seg_start, seg_end) = loop {
        guard += 1;
        if guard > 1000 {
            return Err(Error::Corrupt("no Segment"));
        }
        let (e, sz, hl) = buf.header(src, off).ok_or(Error::Corrupt("no Segment"))?;
        if e == id::SEGMENT {
            let start = off + hl as u64;
            break (start, sz.map(|s| start.saturating_add(s).min(file_len)).unwrap_or(file_len));
        }
        off = off.saturating_add(hl as u64 + sz.ok_or(Error::Corrupt("element before the Segment"))?);
    };
    let mut info = Info {
        doc_type,
        timescale: 1_000_000,
        duration: 0,
        title: String::new(),
        tracks: Vec::new(),
        chapters: Vec::new(),
        cues: Vec::new(),
        segment_start: seg_start,
        segment_end: seg_end,
        first_cluster: 0,
    };
    let mut seen_info = false;
    let mut seen_tracks = false;
    let mut cues_done = false;
    let mut chapters_done = false;
    let mut duration_ticks = 0.0f64;
    let mut seeks = Seeks::default();
    let on_master = |e: u32, body: &[u8], info: &mut Info, seeks: &mut Seeks, duration_ticks: &mut f64| match e {
        id::INFO => {
            for (e2, v) in children(body) {
                match e2 {
                    id::TIMESTAMP_SCALE => info.timescale = uint(v).max(1),
                    id::DURATION => *duration_ticks = float(v),
                    id::TITLE => info.title = string(v),
                    _ => {},
                }
            }
        },
        id::TRACKS => {
            for (e2, v) in children(body) {
                if e2 == id::TRACK_ENTRY {
                    let t = parse_track(v);
                    if t.number != 0 {
                        info.tracks.push(t);
                    }
                }
            }
        },
        id::SEEK_HEAD => parse_seekhead(body, seg_start, seeks),
        id::CUES => parse_cues(body, seg_start, &mut info.cues),
        id::CHAPTERS => parse_chapters(body, &mut info.chapters),
        _ => {},
    };
    // top-level elements up to the first Cluster
    let mut pos = seg_start;
    let mut guard = 0;
    while pos < seg_end && guard < 10_000 {
        guard += 1;
        let Some((e, sz, hl)) = buf.header(src, pos) else { break };
        if e == id::CLUSTER {
            info.first_cluster = pos;
            break;
        }
        let Some(sz) = sz else { break };
        let body_at = pos + hl as u64;
        if matches!(e, id::INFO | id::TRACKS | id::SEEK_HEAD | id::CUES | id::CHAPTERS) {
            if let Some(body) = buf.body(src, body_at, sz) {
                on_master(e, &body, &mut info, &mut seeks, &mut duration_ticks);
                seen_info |= e == id::INFO;
                seen_tracks |= e == id::TRACKS;
                cues_done |= e == id::CUES;
                chapters_done |= e == id::CHAPTERS;
            }
        }
        pos = body_at.saturating_add(sz);
    }
    if !seen_info || !seen_tracks {
        return Err(Error::Corrupt("no Info / Tracks before the first Cluster"));
    }
    // further SeekHeads, then what sits after the Clusters
    let mut k = 0;
    while k < seeks.heads.len() && k < 8 {
        let at = seeks.heads[k];
        k += 1;
        if let Some((id::SEEK_HEAD, Some(sz), hl)) = buf.header(src, at) {
            if let Some(body) = buf.body(src, at + hl as u64, sz) {
                on_master(id::SEEK_HEAD, &body, &mut info, &mut seeks, &mut duration_ticks);
            }
        }
    }
    for (want, at, done) in [(id::CHAPTERS, seeks.chapters, chapters_done), (id::CUES, seeks.cues, cues_done)] {
        let Some(at) = at else { continue };
        if done {
            continue;
        }
        if let Some((e, Some(sz), hl)) = buf.header(src, at) {
            if e == want {
                if let Some(body) = buf.body(src, at + hl as u64, sz) {
                    on_master(e, &body, &mut info, &mut seeks, &mut duration_ticks);
                }
            }
        }
    }
    // cue times were in ticks
    let ts = info.timescale;
    for c in info.cues.iter_mut() {
        c.time = c.time.saturating_mul(ts);
    }
    info.cues.sort_by_key(|c| (c.time, c.cluster));
    info.duration = (duration_ticks * ts as f64) as u64;
    if info.first_cluster == 0 {
        // no Cluster right after the headers: the first Cue knows
        info.first_cluster = info.cues.iter().map(|c| c.cluster).min().unwrap_or(pos);
    }
    Ok(info)
}

// ── reading packets ──────────────────────────────────────────────────────────

/// Split a laced block's frames area. `lacing` is the 2-bit lacing field (1 Xiph, 2 fixed, 3 EBML).
pub fn unlace(lacing: u8, b: &[u8]) -> Option<Vec<&[u8]>> {
    if lacing == 0 {
        return Some(vec![b]);
    }
    let count = *b.first()? as usize + 1;
    let mut p = 1usize;
    let mut sizes: Vec<usize> = Vec::with_capacity(count);
    match lacing {
        1 => {
            for _ in 0..count - 1 {
                let mut s = 0usize;
                loop {
                    let x = *b.get(p)?;
                    p += 1;
                    s += x as usize;
                    if x != 255 {
                        break;
                    }
                }
                sizes.push(s);
            }
        },
        3 => {
            let (first, l, _) = vint(b.get(p..)?)?;
            p += l;
            sizes.push(first as usize);
            let mut prev = first as i64;
            for _ in 1..count - 1 {
                let (raw, l, _) = vint(b.get(p..)?)?;
                p += l;
                // a signed difference: the unsigned value minus half the range
                let bias = (1i64 << (7 * l - 1)) - 1;
                let s = prev + (raw as i64 - bias);
                if s < 0 {
                    return None;
                }
                sizes.push(s as usize);
                prev = s;
            }
        },
        2 => {
            let total = b.len().checked_sub(p)?;
            if total % count != 0 {
                return None;
            }
            let each = total / count;
            return Some((0..count).map(|i| &b[p + i * each..p + (i + 1) * each]).collect());
        },
        _ => return None,
    }
    let used = sizes.iter().try_fold(0usize, |a, &s| a.checked_add(s))?;
    let last = b.len().checked_sub(p)?.checked_sub(used)?;
    sizes.push(last);
    let mut out = Vec::with_capacity(count);
    for s in sizes {
        out.push(b.get(p..p + s)?);
        p += s;
    }
    Some(out)
}

/// Top-level elements that end an unknown-size Cluster.
fn is_top_level(e: u32) -> bool {
    matches!(e, id::CLUSTER | id::CUES | id::CHAPTERS | id::TAGS | id::ATTACHMENTS | id::SEEK_HEAD | id::INFO | id::TRACKS | id::SEGMENT | id::EBML)
}

/// Largest block accepted (a bad size must not allocate gigabytes).
const MAX_BLOCK: u64 = 64 << 20;

/// Walks the Clusters, handing out packets.
pub struct Reader {
    buf: Buf,
    /// Offset of the next element to look at.
    pos: u64,
    /// The current Cluster's timestamp (ticks) and end (u64::MAX = unknown size).
    cluster_ts: i64,
    cluster_end: u64,
    in_cluster: bool,
    queue: alloc::collections::VecDeque<Packet>,
    /// Only these tracks are returned (empty = all).
    pub wanted: Vec<u64>,
    pub done: bool,
}

impl Reader {
    pub fn new(info: &Info) -> Reader {
        Reader {
            buf: Buf::new(),
            pos: info.first_cluster,
            cluster_ts: 0,
            cluster_end: 0,
            in_cluster: false,
            queue: alloc::collections::VecDeque::new(),
            wanted: Vec::new(),
            done: false,
        }
    }

    /// Byte offset the reader is at (for progress).
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// Continue from the Cluster at `cluster` (an absolute offset).
    pub fn goto_cluster(&mut self, cluster: u64) {
        self.pos = cluster;
        self.in_cluster = false;
        self.queue.clear();
        self.done = false;
    }

    /// Go to the Cluster holding time `t` (ns) or the last one before it: the Cue point at or
    /// before `t` (of a video track when there is one), else by hopping over Clusters from the
    /// first. Packets before `t` still come out (decoders need the key frame before it);
    /// returns the time of the Cluster landed on.
    pub fn seek<S: Source + ?Sized>(&mut self, src: &mut S, info: &Info, t: u64) -> u64 {
        let video: Vec<u64> = info.tracks.iter().filter(|x| x.kind == Kind::Video).map(|x| x.number).collect();
        let cue_ok = |c: &&CuePoint| video.is_empty() || video.contains(&c.track);
        let pick = info.cues.iter().filter(cue_ok).filter(|c| c.time <= t).last().or_else(|| info.cues.iter().find(|c| cue_ok(c)));
        if let Some(c) = pick {
            self.goto_cluster(c.cluster);
            return c.time;
        }
        // no Cues: hop along the Clusters by their sizes
        let mut at = info.first_cluster;
        let mut best = (info.first_cluster, 0u64);
        let mut guard = 0u32;
        while at < info.segment_end && guard < 4_000_000 {
            guard += 1;
            let Some((e, sz, hl)) = self.buf.header(src, at) else { break };
            if e != id::CLUSTER {
                match sz {
                    Some(s) => {
                        at = at.saturating_add(hl as u64 + s);
                        continue;
                    },
                    None => break,
                }
            }
            // the Cluster's Timestamp is its first child
            let ts = self.buf.get(src, at + hl as u64, 16).and_then(|b| {
                let (e2, s2, h2) = element_header(b)?;
                if e2 == id::TIMESTAMP {
                    Some(uint(b.get(h2..h2 + s2? as usize)?))
                } else {
                    None
                }
            });
            let Some(ts) = ts else { break };
            let tn = ts.saturating_mul(info.timescale);
            if tn > t {
                break;
            }
            best = (at, tn);
            match sz {
                Some(s) => at = at.saturating_add(hl as u64 + s),
                None => break,
            }
        }
        self.goto_cluster(best.0);
        best.1
    }

    /// The next packet (of a wanted track), or None at the end of the file.
    pub fn next_packet<S: Source + ?Sized>(&mut self, src: &mut S, info: &Info) -> Option<Packet> {
        let mut guard = 0u32;
        loop {
            if let Some(p) = self.queue.pop_front() {
                return Some(p);
            }
            if self.done {
                return None;
            }
            guard += 1;
            if guard > 1_000_000 || self.pos >= info.segment_end {
                self.done = true;
                return None;
            }
            if self.in_cluster && self.pos >= self.cluster_end {
                self.in_cluster = false;
            }
            let Some((e, sz, hl)) = self.buf.header(src, self.pos) else {
                self.done = true;
                return None;
            };
            let body = self.pos + hl as u64;
            if self.in_cluster && self.cluster_end == u64::MAX && is_top_level(e) {
                self.in_cluster = false;
            }
            if e == id::CLUSTER {
                self.in_cluster = true;
                self.cluster_ts = 0;
                self.cluster_end = match sz {
                    Some(s) => body.saturating_add(s),
                    None => u64::MAX,
                };
                self.pos = body; // step into it
                continue;
            }
            let Some(sz) = sz else {
                // an unknown-size element that is not a Cluster cannot be skipped
                self.done = true;
                return None;
            };
            let next = body.saturating_add(sz);
            if self.in_cluster {
                match e {
                    id::TIMESTAMP => {
                        if let Some(b) = self.buf.get(src, body, sz.min(8) as usize) {
                            self.cluster_ts = uint(b) as i64;
                        }
                    },
                    id::SIMPLE_BLOCK if sz <= MAX_BLOCK => {
                        // only the track number first: unwanted blocks are not copied
                        let want = self.buf.get(src, body, 8).and_then(vint).is_some_and(|(t, _, _)| self.wanted.is_empty() || self.wanted.contains(&t));
                        if want {
                            if let Some(b) = self.buf.body(src, body, sz) {
                                self.block(info, &b, true, None, false);
                            }
                        }
                    },
                    id::BLOCK_GROUP if sz <= MAX_BLOCK => {
                        if let Some(g) = self.buf.body(src, body, sz) {
                            let mut blk: Option<&[u8]> = None;
                            let mut dur = None;
                            let mut has_ref = false;
                            for (e2, v) in children(&g) {
                                match e2 {
                                    id::BLOCK => blk = Some(v),
                                    id::BLOCK_DURATION => dur = Some(uint(v)),
                                    id::REFERENCE_BLOCK => has_ref = true,
                                    _ => {},
                                }
                            }
                            if let Some(b) = blk {
                                self.block(info, b, false, dur, !has_ref);
                            }
                        }
                    },
                    _ => {},
                }
            }
            self.pos = next;
        }
    }

    fn block(&mut self, info: &Info, b: &[u8], simple: bool, dur_ticks: Option<u64>, group_key: bool) {
        let Some((track, l, _)) = vint(b) else { return };
        if !self.wanted.is_empty() && !self.wanted.contains(&track) {
            return;
        }
        let Some(hdr) = b.get(l..l + 3) else { return };
        let rel = i16::from_be_bytes([hdr[0], hdr[1]]) as i64;
        let flags = hdr[2];
        let key = if simple { flags & 0x80 != 0 } else { group_key };
        let lacing = (flags >> 1) & 3;
        let Some(frames) = unlace(lacing, &b[l + 3..]) else { return };
        let Some(t) = info.track(track) else { return };
        let ts = info.timescale as i64;
        let base = (self.cluster_ts + rel).saturating_mul(ts);
        let dd = t.default_duration;
        let n = frames.len() as u64;
        let dur = dur_ticks.map(|d| d.saturating_mul(info.timescale)).or(if dd > 0 { Some(dd * n) } else { None });
        // laced frames share the timestamp: spread them by the frame duration
        let each = if n > 1 { dur.map(|d| d / n).unwrap_or(0) } else { 0 };
        for (i, f) in frames.into_iter().enumerate() {
            let mut data = Vec::with_capacity(t.strip.len() + f.len());
            data.extend_from_slice(&t.strip);
            data.extend_from_slice(f);
            self.queue.push_back(Packet {
                track,
                pts: base + (each * i as u64) as i64,
                duration: if n > 1 { Some(each) } else { dur },
                key: key || t.kind != Kind::Video,
                data,
            });
        }
    }
}

/// A signed EBML integer (e.g. a ReferenceBlock value).
pub fn signed(b: &[u8]) -> i64 {
    sint(b)
}

/// Split Xiph-laced CodecPrivate (Vorbis / Theora headers) into its packets.
pub fn xiph_headers(cp: &[u8]) -> Option<Vec<&[u8]>> {
    // the same layout as Xiph lacing: count-1, sizes, then the packets
    unlace(1, cp)
}

#[cfg(test)]
mod tests;
