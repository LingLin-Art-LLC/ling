//! MP4 / ISO-BMFF demuxer for LingOS, freestanding (`no_std` + `alloc`).
//!
//! Handles regular MP4 (sample tables in `moov`) and fragmented MP4 / DASH
//! (YouTube's adaptive streams: `ftyp`, `moov` with `mvex`, `sidx`, then
//! `moof` + `mdat` pairs). Everything works on byte slices the caller already
//! has, so it fits a player that fetches byte ranges over HTTPS. No function
//! here reads a file or allocates in proportion to anything but its input
//! (sample tables are walked lazily), and garbage / truncated input returns
//! an [`Mp4Error`]; it never panics or loops.
//!
//! # Streaming workflow (DASH / YouTube)
//!
//! ```ignore
//! use ling_mp4::*;
//! // 1. Fetch the first few KiB and look at the top-level boxes.
//! let head: &[u8] = fetch(0..16384);
//! let boxes = scan_boxes(head, 0)?;          // ftyp, moov, sidx, (moof)...
//! // A box whose `complete` is false tells you how many bytes to fetch:
//! //   fetch(b.offset .. b.offset + b.size.unwrap())
//! let moov = boxes.iter().find(|b| &b.kind == b"moov").unwrap();
//! let mut movie = Movie::parse_moov(moov.payload(head).unwrap())?;
//! let track = &movie.tracks[0];
//! match &track.codec {
//!     Codec::Mp4a { asc, .. } => { /* ling_aac::AacDecoder::new(asc) */ }
//!     Codec::Avc { avcc, .. } => { /* AVCDecoderConfigurationRecord bytes */ }
//!     _ => {}
//! }
//! // 2. The segment index gives absolute byte ranges and times of each segment.
//! let sidx_box = boxes.iter().find(|b| &b.kind == b"sidx").unwrap();
//! let sidx = Sidx::parse(sidx_box.bytes(head).unwrap(), sidx_box.offset)?;
//! // 3. For each segment fetch its bytes (moof + mdat) and list the samples.
//! let seg = &sidx.entries[0];
//! let data = fetch(seg.offset .. seg.offset + seg.size as u64);
//! let frag = movie.parse_fragment(&data, seg.offset)?;
//! for s in &frag.samples {
//!     let au = &data[s.offset as usize .. s.offset as usize + s.size as usize];
//!     // s.dts / s.pts in track.timescale units, s.keyframe
//! }
//! ```
//!
//! # Regular MP4
//!
//! Parse `moov` the same way, then walk [`Track::samples`] (lazy iterator of
//! [`Sample`] with absolute file offsets) or collect [`Track::sample_table`].
//!
//! # Timestamps
//!
//! `dts` / `pts` are in the track's `timescale` (from `mdhd`), counted from
//! the start of the media timeline (`pts = dts + composition offset`). Edit
//! lists are *not* applied; the first edit's `media_time` (encoder delay /
//! B-frame shift) is exposed as [`Track::edit_media_time`] so the player can
//! subtract it.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;

mod reader;
/// MP4 writer (regular and fragmented) for one H.264 track.
pub mod writer;

pub use reader::BoxHeader;
use reader::{child, Children, Cursor};

/// Demuxer errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mp4Error {
    /// The data is malformed (bad sizes, truncated payloads, missing boxes).
    Invalid(&'static str),
    /// The buffer ends before a box the call needs; `needed` is the total
    /// number of bytes (from the start of the buffer) required.
    NeedMoreData { needed: u64 },
    /// Valid but not handled (e.g. a `moov` compressed with `cmov`).
    Unsupported(&'static str),
}

impl core::fmt::Display for Mp4Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Mp4Error::Invalid(s) => write!(f, "mp4: invalid ({})", s),
            Mp4Error::NeedMoreData { needed } => write!(f, "mp4: need {} bytes", needed),
            Mp4Error::Unsupported(s) => write!(f, "mp4: unsupported ({})", s),
        }
    }
}

/// Hard cap on samples per track / per `trun` so hostile tables cannot make
/// an iteration effectively endless (16.7 M samples is days of audio).
pub const MAX_SAMPLES: u32 = 1 << 24;

// ---------------------------------------------------------------------------
// Top-level scanning

/// A top-level box found by [`scan_boxes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxInfo {
    pub kind: [u8; 4],
    /// Absolute file offset of the box header.
    pub offset: u64,
    /// Total size including header (`None` = extends to end of file).
    pub size: Option<u64>,
    pub header_len: usize,
    /// The whole box is inside the scanned buffer.
    pub complete: bool,
}

impl BoxInfo {
    /// The whole box (header included), from a buffer that starts at file
    /// offset 0 (for other buffers use [`BoxInfo::bytes_at`]). `None` if the
    /// box is not completely inside `buf`.
    pub fn bytes<'a>(&self, buf: &'a [u8]) -> Option<&'a [u8]> {
        self.bytes_at(buf, 0)
    }

    /// Like [`BoxInfo::bytes`] for a buffer that starts at file offset `buf_offset`.
    pub fn bytes_at<'a>(&self, buf: &'a [u8], buf_offset: u64) -> Option<&'a [u8]> {
        let start = self.offset.checked_sub(buf_offset)? as usize;
        let size = match self.size {
            Some(s) => usize::try_from(s).ok()?,
            None => buf.len().checked_sub(start)?,
        };
        buf.get(start..start.checked_add(size)?)
    }

    /// The box payload (after the header). `None` if incomplete.
    pub fn payload<'a>(&self, buf: &'a [u8]) -> Option<&'a [u8]> {
        self.bytes(buf).map(|b| &b[self.header_len..])
    }

    /// The box payload for a buffer starting at file offset `buf_offset`.
    pub fn payload_at<'a>(&self, buf: &'a [u8], buf_offset: u64) -> Option<&'a [u8]> {
        self.bytes_at(buf, buf_offset).map(|b| &b[self.header_len..])
    }

    /// File offset just past this box (`None` when the size is "to EOF").
    pub fn end(&self) -> Option<u64> {
        self.size.map(|s| self.offset + s)
    }
}

/// List the top-level boxes in `buf`, which holds file bytes starting at
/// `file_offset`. Scanning stops at the first box that does not fit (it is
/// returned with `complete = false`, so its size says how much to fetch) or
/// when fewer than 16 bytes remain. A box with size 0 ("to EOF") is returned
/// complete and ends the scan.
pub fn scan_boxes(buf: &[u8], file_offset: u64) -> Result<Vec<BoxInfo>, Mp4Error> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < buf.len() {
        let hdr = match BoxHeader::parse(&buf[pos..])? {
            Some(h) => h,
            None => break,
        };
        let offset = file_offset + pos as u64;
        match hdr.size {
            None => {
                out.push(BoxInfo {
                    kind: hdr.kind,
                    offset,
                    size: None,
                    header_len: hdr.header_len,
                    complete: true,
                });
                break;
            }
            Some(size) => {
                let complete = (buf.len() - pos) as u64 >= size;
                out.push(BoxInfo { kind: hdr.kind, offset, size: Some(size), header_len: hdr.header_len, complete });
                if !complete {
                    break;
                }
                pos += size as usize;
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Movie / tracks

/// Track handler type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Other([u8; 4]),
}

/// Codec of a track's first sample description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Codec {
    /// H.264 (`avc1` / `avc3`): `avcc` is the AVCDecoderConfigurationRecord
    /// (payload of the `avcC` box, starting with configurationVersion = 1).
    Avc { fourcc: [u8; 4], avcc: Vec<u8> },
    /// MPEG-4 audio (`mp4a`): `asc` is the AudioSpecificConfig (the esds
    /// DecoderSpecificInfo), `object_type_indication` 0x40 = MPEG-4 audio.
    Mp4a { object_type_indication: u8, asc: Vec<u8> },
    /// Anything else: the sample entry type and the payload of its first
    /// child box that looks like a codec config (e.g. `hvcC`, `av1C`, `vpcC`,
    /// `dOps`), if any.
    Other { fourcc: [u8; 4], config: Vec<u8> },
}

/// `trex` defaults for fragmented tracks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrackDefaults {
    pub sample_description_index: u32,
    pub sample_duration: u32,
    pub sample_size: u32,
    pub sample_flags: u32,
}

/// One track of a movie.
#[derive(Clone, Debug)]
pub struct Track {
    pub id: u32,
    pub kind: TrackKind,
    pub codec: Codec,
    /// Media timescale (ticks per second) from `mdhd`.
    pub timescale: u32,
    /// Media duration in `timescale` units from `mdhd` (0 for DASH init segments).
    pub duration: u64,
    /// Display size from the visual sample entry (0 for audio).
    pub width: u16,
    pub height: u16,
    /// Audio sample entry fields (0 for video).
    pub channels: u16,
    pub sample_rate: u32,
    /// `media_time` of the first non-empty edit (`elst`), if any.
    pub edit_media_time: Option<i64>,
    /// Defaults from `trex` (fragmented files).
    pub defaults: TrackDefaults,
    tables: SampleTables,
}

/// A parsed `moov`.
#[derive(Clone, Debug)]
pub struct Movie {
    /// Movie timescale from `mvhd`.
    pub timescale: u32,
    /// Movie duration in `mvhd` timescale units (`mehd` fragment duration if larger).
    pub duration: u64,
    /// `mvex` present: samples live in `moof` fragments.
    pub fragmented: bool,
    pub tracks: Vec<Track>,
    /// Next decode time per track (by index), used when a `traf` has no `tfdt`.
    next_dts: Vec<u64>,
}

/// One media sample (an access unit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Byte offset of the sample data. For [`Track::samples`]: absolute file
    /// offset. For [`Movie::parse_fragment`]: offset within the buffer passed
    /// in (it may lie beyond the buffer if the `mdat` was not included).
    pub offset: u64,
    pub size: u32,
    /// Decode time in the track timescale.
    pub dts: u64,
    /// Presentation time (`dts` + composition offset) in the track timescale.
    pub pts: i64,
    /// Composition time offset (`ctts` / `trun`).
    pub cts_offset: i32,
    /// Sample duration in the track timescale.
    pub duration: u32,
    /// Sync sample (keyframe / random access point).
    pub keyframe: bool,
}

impl Movie {
    /// Parse a `moov` box *payload* (the bytes after the 8-byte header; see
    /// [`BoxInfo::payload`]).
    pub fn parse_moov(payload: &[u8]) -> Result<Movie, Mp4Error> {
        let mut timescale = 0;
        let mut duration = 0u64;
        let mut fragmented = false;
        let mut tracks = Vec::new();
        let mut trex: Vec<(u32, TrackDefaults)> = Vec::new();

        for c in Children::new(payload) {
            let (kind, p) = c?;
            match &kind {
                b"mvhd" => {
                    let mut r = Cursor::new(p);
                    let (v, _) = r.full_header()?;
                    if v == 1 {
                        r.skip(16)?;
                        timescale = r.u32()?;
                        duration = r.u64()?;
                    } else {
                        r.skip(8)?;
                        timescale = r.u32()?;
                        duration = u64::from(r.u32()?);
                    }
                }
                b"trak" => {
                    if let Some(t) = parse_trak(p)? {
                        tracks.push(t);
                    }
                }
                b"mvex" => {
                    fragmented = true;
                    for c in Children::new(p) {
                        let (k, p) = c?;
                        let mut r = Cursor::new(p);
                        if &k == b"trex" {
                            r.full_header()?;
                            let id = r.u32()?;
                            let d = TrackDefaults {
                                sample_description_index: r.u32()?,
                                sample_duration: r.u32()?,
                                sample_size: r.u32()?,
                                sample_flags: r.u32()?,
                            };
                            trex.push((id, d));
                        } else if &k == b"mehd" {
                            let (v, _) = r.full_header()?;
                            let d = if v == 1 { r.u64()? } else { u64::from(r.u32()?) };
                            duration = duration.max(d);
                        }
                    }
                }
                b"cmov" => return Err(Mp4Error::Unsupported("compressed moov")),
                _ => {}
            }
        }
        for t in tracks.iter_mut() {
            if let Some((_, d)) = trex.iter().find(|(id, _)| *id == t.id) {
                t.defaults = *d;
            }
        }
        let next_dts = alloc::vec![0; tracks.len()];
        Ok(Movie { timescale, duration, fragmented, tracks, next_dts })
    }

    /// Find the top-level `moov` in `buf` (file bytes from offset 0, i.e. an
    /// init segment or the head of a file) and parse it. Returns
    /// [`Mp4Error::NeedMoreData`] if `moov` is cut off, or if the scan ran
    /// off the end of `buf` before finding it (for a regular MP4 with `moov`
    /// at the end, use [`scan_boxes`] to locate it and fetch it separately).
    pub fn parse_init(buf: &[u8]) -> Result<Movie, Mp4Error> {
        let boxes = scan_boxes(buf, 0)?;
        for b in &boxes {
            if !b.complete {
                return Err(Mp4Error::NeedMoreData { needed: b.end().unwrap_or(u64::MAX) });
            }
            if &b.kind == b"moov" {
                let p = b.payload(buf).ok_or(Mp4Error::Invalid("moov"))?;
                return Movie::parse_moov(p);
            }
        }
        Err(Mp4Error::NeedMoreData { needed: buf.len() as u64 + 8 })
    }

    /// Track by id.
    pub fn track(&self, id: u32) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    /// Reset the running decode times used for fragments without `tfdt`
    /// (call after seeking if your stream lacks `tfdt`).
    pub fn reset_fragment_times(&mut self) {
        for d in self.next_dts.iter_mut() {
            *d = 0;
        }
    }

    /// Parse one fragment: `buf` must start at a `moof` box (file offset
    /// `buf_file_offset`, needed only when `tfhd` carries absolute
    /// base-data-offsets) and should normally also contain the following
    /// `mdat`. Only the `moof` itself has to be complete. Returns every
    /// sample of every track fragment, with offsets relative to `buf`.
    pub fn parse_fragment(&mut self, buf: &[u8], buf_file_offset: u64) -> Result<Fragment, Mp4Error> {
        let hdr = BoxHeader::parse(buf)?.ok_or(Mp4Error::NeedMoreData { needed: 16 })?;
        if &hdr.kind != b"moof" {
            return Err(Mp4Error::Invalid("fragment does not start with moof"));
        }
        let size = hdr.size.ok_or(Mp4Error::Invalid("moof without size"))?;
        if size > buf.len() as u64 {
            return Err(Mp4Error::NeedMoreData { needed: size });
        }
        let moof = &buf[hdr.header_len..size as usize];
        let mut frag = Fragment { sequence: 0, moof_size: size, samples: Vec::new(), runs: Vec::new() };
        // Implicit data offset for a traf without base-data-offset and without
        // default-base-is-moof: the end of the previous traf's data (first traf: moof start).
        let mut prev_traf_end: u64 = 0;

        for c in Children::new(moof) {
            let (kind, p) = c?;
            if &kind == b"mfhd" {
                let mut r = Cursor::new(p);
                r.full_header()?;
                frag.sequence = r.u32()?;
                continue;
            }
            if &kind != b"traf" {
                continue;
            }
            let tfhd = child(p, b"tfhd")?.ok_or(Mp4Error::Invalid("traf without tfhd"))?;
            let mut r = Cursor::new(tfhd);
            let (_, flags) = r.full_header()?;
            let track_id = r.u32()?;
            let tidx = match self.tracks.iter().position(|t| t.id == track_id) {
                Some(i) => i,
                None => continue, // unknown track: skip
            };
            let defaults = self.tracks[tidx].defaults;
            let base_data_offset: u64 = if flags & 0x1 != 0 {
                let abs = r.u64()?;
                abs.checked_sub(buf_file_offset).ok_or(Mp4Error::Invalid("base data offset before buffer"))?
            } else if flags & 0x2_0000 != 0 {
                0
            } else {
                prev_traf_end
            };
            if flags & 0x2 != 0 {
                r.u32()?; // sample_description_index
            }
            let def_dur = if flags & 0x8 != 0 { r.u32()? } else { defaults.sample_duration };
            let def_size = if flags & 0x10 != 0 { r.u32()? } else { defaults.sample_size };
            let def_flags = if flags & 0x20 != 0 { r.u32()? } else { defaults.sample_flags };

            let mut dts = self.next_dts[tidx];
            if let Some(tfdt) = child(p, b"tfdt")? {
                let mut r = Cursor::new(tfdt);
                let (v, _) = r.full_header()?;
                dts = if v == 1 { r.u64()? } else { u64::from(r.u32()?) };
            }

            let mut data_pos = base_data_offset;
            for c in Children::new(p) {
                let (k, tp) = c?;
                if &k != b"trun" {
                    continue;
                }
                let mut r = Cursor::new(tp);
                let (_version, tf) = r.full_header()?;
                let count = r.u32()?;
                if count > MAX_SAMPLES || frag.samples.len() as u64 + u64::from(count) > u64::from(MAX_SAMPLES) {
                    return Err(Mp4Error::Invalid("trun sample count too large"));
                }
                if tf & 0x1 != 0 {
                    let off = r.u32()? as i32;
                    data_pos = (base_data_offset as i64 + i64::from(off))
                        .try_into()
                        .map_err(|_| Mp4Error::Invalid("negative data offset"))?;
                }
                let first_flags = if tf & 0x4 != 0 { Some(r.u32()?) } else { None };
                let per = [tf & 0x100 != 0, tf & 0x200 != 0, tf & 0x400 != 0, tf & 0x800 != 0];
                let per_bytes = per.iter().filter(|b| **b).count() * 4;
                if per_bytes > 0 && (count as usize) > r.remaining() / per_bytes {
                    return Err(Mp4Error::Invalid("trun shorter than its sample count"));
                }
                let run_start = frag.samples.len();
                for i in 0..count {
                    let duration = if per[0] { r.u32()? } else { def_dur };
                    let size = if per[1] { r.u32()? } else { def_size };
                    let sflags = match (per[2], i, first_flags) {
                        (true, _, _) => r.u32()?,
                        (false, 0, Some(f)) => f,
                        _ => def_flags,
                    };
                    // Composition offset: unsigned in version 0, signed in version 1;
                    // like other demuxers, treat both as signed 32-bit.
                    let cts = if per[3] { r.u32()? as i32 } else { 0 };
                    frag.samples.push(Sample {
                        offset: data_pos,
                        size,
                        dts,
                        pts: dts as i64 + i64::from(cts),
                        cts_offset: cts,
                        duration,
                        keyframe: sflags & 0x0001_0000 == 0,
                    });
                    data_pos = data_pos.saturating_add(u64::from(size));
                    dts = dts.wrapping_add(u64::from(duration));
                }
                frag.runs.push(TrackRun { track_id, start: run_start, end: frag.samples.len() });
            }
            prev_traf_end = data_pos;
            self.next_dts[tidx] = dts;
        }
        Ok(frag)
    }
}

/// Samples of one `trun`, as a range into [`Fragment::samples`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrackRun {
    pub track_id: u32,
    pub start: usize,
    pub end: usize,
}

/// The result of [`Movie::parse_fragment`].
#[derive(Clone, Debug)]
pub struct Fragment {
    /// `mfhd` sequence number.
    pub sequence: u32,
    /// Size of the `moof` box (the `mdat` normally starts right after).
    pub moof_size: u64,
    /// All samples, in `traf` / `trun` order.
    pub samples: Vec<Sample>,
    /// Which track each range of `samples` belongs to.
    pub runs: Vec<TrackRun>,
}

impl Fragment {
    /// Samples of one track.
    pub fn track_samples(&self, track_id: u32) -> impl Iterator<Item = &Sample> + '_ {
        self.runs
            .iter()
            .filter(move |r| r.track_id == track_id)
            .flat_map(move |r| self.samples[r.start..r.end].iter())
    }
}

// ---------------------------------------------------------------------------
// trak parsing

#[derive(Clone, Debug, Default)]
struct SampleTables {
    /// stts: (count, delta)
    stts: Vec<(u32, u32)>,
    /// ctts: (count, offset)
    ctts: Vec<(u32, i32)>,
    /// stsc: (first_chunk, samples_per_chunk)
    stsc: Vec<(u32, u32)>,
    /// stsz constant size (0 = per-sample sizes in `sizes`)
    sample_size: u32,
    sample_count: u32,
    sizes: Vec<u32>,
    chunk_offsets: Vec<u64>,
    /// stss (1-based sample numbers); None = every sample is a sync sample
    stss: Option<Vec<u32>>,
}

fn parse_trak(p: &[u8]) -> Result<Option<Track>, Mp4Error> {
    let mut id = 0;
    let mut width = 0;
    let mut height = 0;
    let mut edit_media_time = None;
    let mut mdia = None;
    for c in Children::new(p) {
        let (k, p) = c?;
        match &k {
            b"tkhd" => {
                let mut r = Cursor::new(p);
                let (v, _) = r.full_header()?;
                if v == 1 {
                    r.skip(16)?;
                    id = r.u32()?;
                    r.skip(4 + 8)?;
                } else {
                    r.skip(8)?;
                    id = r.u32()?;
                    r.skip(4 + 4)?;
                }
                r.skip(8 + 2 + 2 + 2 + 2 + 36)?;
                width = (r.u32()? >> 16) as u16;
                height = (r.u32()? >> 16) as u16;
            }
            b"edts" => {
                if let Some(elst) = child(p, b"elst")? {
                    let mut r = Cursor::new(elst);
                    let (v, _) = r.full_header()?;
                    let n = r.u32()?;
                    for _ in 0..n {
                        let (_dur, mt) = if v == 1 {
                            (r.u64()?, r.u64()? as i64)
                        } else {
                            (u64::from(r.u32()?), i64::from(r.u32()? as i32))
                        };
                        r.skip(4)?; // media rate
                        if mt != -1 {
                            edit_media_time = Some(mt);
                            break;
                        }
                    }
                }
            }
            b"mdia" => mdia = Some(p),
            _ => {}
        }
    }
    let mdia = match mdia {
        Some(m) => m,
        None => return Ok(None),
    };

    let mut timescale = 0;
    let mut duration = 0;
    let mut kind = TrackKind::Other(*b"    ");
    let mut stbl = None;
    for c in Children::new(mdia) {
        let (k, p) = c?;
        match &k {
            b"mdhd" => {
                let mut r = Cursor::new(p);
                let (v, _) = r.full_header()?;
                if v == 1 {
                    r.skip(16)?;
                    timescale = r.u32()?;
                    duration = r.u64()?;
                } else {
                    r.skip(8)?;
                    timescale = r.u32()?;
                    duration = u64::from(r.u32()?);
                }
            }
            b"hdlr" => {
                let mut r = Cursor::new(p);
                r.full_header()?;
                r.skip(4)?;
                let h = r.fourcc()?;
                kind = match &h {
                    b"vide" => TrackKind::Video,
                    b"soun" => TrackKind::Audio,
                    _ => TrackKind::Other(h),
                };
            }
            b"minf" => stbl = child(p, b"stbl")?,
            _ => {}
        }
    }
    let stbl = match stbl {
        Some(s) => s,
        None => return Ok(None),
    };

    let mut tables = SampleTables::default();
    let mut codec = Codec::Other { fourcc: *b"    ", config: Vec::new() };
    let mut channels = 0;
    let mut sample_rate = 0;
    for c in Children::new(stbl) {
        let (k, p) = c?;
        let mut r = Cursor::new(p);
        match &k {
            b"stsd" => {
                r.full_header()?;
                let n = r.u32()?;
                if n > 0 {
                    let entries = &p[r.pos..];
                    if let Some(e) = Children::new(entries).next() {
                        let (fourcc, ep) = e?;
                        let (cd, w, h, ch, sr) = parse_sample_entry(fourcc, ep)?;
                        codec = cd;
                        if w != 0 {
                            width = w;
                            height = h;
                        }
                        channels = ch;
                        sample_rate = sr;
                    }
                }
            }
            b"stts" => {
                r.full_header()?;
                let n = bounded_count(&mut r, 8)?;
                tables.stts.reserve(n);
                for _ in 0..n {
                    tables.stts.push((r.u32()?, r.u32()?));
                }
            }
            b"ctts" => {
                r.full_header()?;
                let n = bounded_count(&mut r, 8)?;
                tables.ctts.reserve(n);
                for _ in 0..n {
                    let c = r.u32()?;
                    let o = r.u32()? as i32; // v0 is nominally unsigned; treat as signed like everyone
                    tables.ctts.push((c, o));
                }
            }
            b"stsc" => {
                r.full_header()?;
                let n = bounded_count(&mut r, 12)?;
                tables.stsc.reserve(n);
                for _ in 0..n {
                    let first = r.u32()?;
                    let spc = r.u32()?;
                    r.u32()?;
                    tables.stsc.push((first, spc));
                }
            }
            b"stsz" => {
                r.full_header()?;
                tables.sample_size = r.u32()?;
                tables.sample_count = r.u32()?.min(MAX_SAMPLES);
                if tables.sample_size == 0 {
                    let n = tables.sample_count as usize;
                    if n > r.remaining() / 4 {
                        return Err(Mp4Error::Invalid("stsz truncated"));
                    }
                    tables.sizes.reserve(n);
                    for _ in 0..n {
                        tables.sizes.push(r.u32()?);
                    }
                }
            }
            b"stz2" => {
                r.full_header()?;
                r.skip(3)?;
                let field = r.u8()?;
                let n = r.u32()?.min(MAX_SAMPLES) as usize;
                let bits = match field {
                    4 | 8 | 16 => field as usize,
                    _ => return Err(Mp4Error::Invalid("stz2 field size")),
                };
                if n > r.remaining() * 8 / bits {
                    return Err(Mp4Error::Invalid("stz2 truncated"));
                }
                tables.sample_count = n as u32;
                tables.sizes.reserve(n);
                for i in 0..n {
                    let s = match bits {
                        4 => {
                            let b = p[r.pos + i / 2];
                            u32::from(if i % 2 == 0 { b >> 4 } else { b & 15 })
                        }
                        8 => u32::from(p[r.pos + i]),
                        _ => u32::from(u16::from_be_bytes([p[r.pos + 2 * i], p[r.pos + 2 * i + 1]])),
                    };
                    tables.sizes.push(s);
                }
            }
            b"stco" => {
                r.full_header()?;
                let n = bounded_count(&mut r, 4)?;
                tables.chunk_offsets.reserve(n);
                for _ in 0..n {
                    tables.chunk_offsets.push(u64::from(r.u32()?));
                }
            }
            b"co64" => {
                r.full_header()?;
                let n = bounded_count(&mut r, 8)?;
                tables.chunk_offsets.reserve(n);
                for _ in 0..n {
                    tables.chunk_offsets.push(r.u64()?);
                }
            }
            b"stss" => {
                r.full_header()?;
                let n = bounded_count(&mut r, 4)?;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push(r.u32()?);
                }
                tables.stss = Some(v);
            }
            _ => {}
        }
    }

    Ok(Some(Track {
        id,
        kind,
        codec,
        timescale,
        duration,
        width,
        height,
        channels,
        sample_rate,
        edit_media_time,
        defaults: TrackDefaults::default(),
        tables,
    }))
}

/// Read an entry count and check the entries fit in the rest of the box.
fn bounded_count(r: &mut Cursor<'_>, entry_size: usize) -> Result<usize, Mp4Error> {
    let n = r.u32()? as usize;
    if n > r.remaining() / entry_size {
        return Err(Mp4Error::Invalid("table entry count exceeds box size"));
    }
    Ok(n)
}

type EntryInfo = (Codec, u16, u16, u16, u32);

fn parse_sample_entry(fourcc: [u8; 4], p: &[u8]) -> Result<EntryInfo, Mp4Error> {
    let mut r = Cursor::new(p);
    r.skip(6 + 2)?; // reserved + data_reference_index
    match &fourcc {
        b"avc1" | b"avc3" | b"hvc1" | b"hev1" | b"av01" | b"vp09" | b"encv" | b"mp4v" => {
            r.skip(16)?;
            let w = r.u16()?;
            let h = r.u16()?;
            r.skip(4 + 4 + 4 + 2 + 32 + 2 + 2)?;
            let rest = &p[r.pos..];
            let codec = match &fourcc {
                b"avc1" | b"avc3" => {
                    let avcc = child(rest, b"avcC")?.ok_or(Mp4Error::Invalid("avc1 without avcC"))?;
                    Codec::Avc { fourcc, avcc: avcc.to_vec() }
                }
                _ => Codec::Other { fourcc, config: first_config(rest)? },
            };
            Ok((codec, w, h, 0, 0))
        }
        b"mp4a" | b"Opus" | b"ac-3" | b"ec-3" | b"fLaC" | b"enca" => {
            let version = r.u16()?;
            r.skip(6)?;
            let channels = r.u16()?;
            r.skip(2 + 4)?;
            let sample_rate = r.u32()? >> 16;
            match version {
                1 => r.skip(16)?,
                2 => r.skip(36)?,
                _ => {}
            }
            let rest = &p[r.pos..];
            let codec = if &fourcc == b"mp4a" {
                let mut found = None;
                for c in Children::new(rest) {
                    let (k, cp) = c?;
                    if &k == b"esds" {
                        found = Some(parse_esds(cp)?);
                        break;
                    }
                    if &k == b"wave" {
                        // QuickTime: esds nested in wave
                        if let Some(e) = child(cp, b"esds")? {
                            found = Some(parse_esds(e)?);
                            break;
                        }
                    }
                }
                let (oti, asc) = found.ok_or(Mp4Error::Invalid("mp4a without esds"))?;
                Codec::Mp4a { object_type_indication: oti, asc }
            } else {
                Codec::Other { fourcc, config: first_config(rest)? }
            };
            Ok((codec, 0, 0, channels, sample_rate))
        }
        _ => Ok((Codec::Other { fourcc, config: Vec::new() }, 0, 0, 0, 0)),
    }
}

fn first_config(rest: &[u8]) -> Result<Vec<u8>, Mp4Error> {
    for c in Children::new(rest) {
        let (k, cp) = c?;
        if matches!(&k, b"hvcC" | b"av1C" | b"vpcC" | b"dOps" | b"dac3" | b"dec3" | b"dfLa" | b"esds") {
            return Ok(cp.to_vec());
        }
    }
    Ok(Vec::new())
}

fn read_desc_len(r: &mut Cursor<'_>) -> Result<usize, Mp4Error> {
    let mut len = 0usize;
    for _ in 0..4 {
        let b = r.u8()?;
        len = (len << 7) | usize::from(b & 0x7f);
        if b & 0x80 == 0 {
            break;
        }
    }
    Ok(len)
}

/// Parse an `esds` payload: returns (objectTypeIndication, DecoderSpecificInfo).
pub fn parse_esds(p: &[u8]) -> Result<(u8, Vec<u8>), Mp4Error> {
    let mut r = Cursor::new(p);
    r.full_header()?;
    if r.u8()? != 0x03 {
        return Err(Mp4Error::Invalid("esds: no ES_Descriptor"));
    }
    let _ = read_desc_len(&mut r)?;
    r.u16()?; // ES_ID
    let flags = r.u8()?;
    if flags & 0x80 != 0 {
        r.u16()?;
    }
    if flags & 0x40 != 0 {
        let n = r.u8()? as usize;
        r.skip(n)?;
    }
    if flags & 0x20 != 0 {
        r.u16()?;
    }
    if r.u8()? != 0x04 {
        return Err(Mp4Error::Invalid("esds: no DecoderConfigDescriptor"));
    }
    let dcd_len = read_desc_len(&mut r)?;
    let dcd_end = r.pos.saturating_add(dcd_len).min(p.len());
    let oti = r.u8()?;
    r.skip(1 + 3 + 4 + 4)?;
    let mut asc = Vec::new();
    while r.pos + 2 <= dcd_end {
        let tag = r.u8()?;
        let len = read_desc_len(&mut r)?;
        let body = r.bytes(len)?;
        if tag == 0x05 {
            asc = body.to_vec();
            break;
        }
    }
    Ok((oti, asc))
}

// ---------------------------------------------------------------------------
// Sample table walking

impl Track {
    /// Number of samples in the `moov` sample tables (0 for fragmented tracks).
    pub fn sample_count(&self) -> u32 {
        self.tables.sample_count
    }

    /// Lazily walk the `moov` sample tables (regular MP4). Offsets are
    /// absolute file offsets. Stops early if the tables are inconsistent.
    pub fn samples(&self) -> SampleIter<'_> {
        SampleIter {
            t: &self.tables,
            index: 0,
            dts: 0,
            stts_i: 0,
            stts_left: self.tables.stts.first().map(|e| e.0).unwrap_or(0),
            ctts_i: 0,
            ctts_left: self.tables.ctts.first().map(|e| e.0).unwrap_or(0),
            stss_i: 0,
            chunk: 0,
            stsc_i: 0,
            chunk_left: 0,
            pos: 0,
            started: false,
        }
    }

    /// Collect [`Track::samples`] into a vector.
    pub fn sample_table(&self) -> Vec<Sample> {
        self.samples().collect()
    }

    /// Index of the last sync sample whose pts <= `pts` (for seeking), from
    /// the `moov` tables.
    pub fn seek_sample(&self, pts: i64) -> Option<(usize, Sample)> {
        let mut best = None;
        for (i, s) in self.samples().enumerate() {
            if s.dts as i64 > pts && best.is_some() {
                break;
            }
            if s.keyframe && s.pts <= pts {
                best = Some((i, s));
            }
        }
        best
    }
}

/// Iterator over the samples of a non-fragmented track.
pub struct SampleIter<'a> {
    t: &'a SampleTables,
    index: u32,
    dts: u64,
    stts_i: usize,
    stts_left: u32,
    ctts_i: usize,
    ctts_left: u32,
    stss_i: usize,
    chunk: usize,
    stsc_i: usize,
    chunk_left: u32,
    pos: u64,
    started: bool,
}

impl<'a> Iterator for SampleIter<'a> {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        let t = self.t;
        if self.index >= t.sample_count {
            return None;
        }
        // Chunk bookkeeping.
        if !self.started || self.chunk_left == 0 {
            if self.started {
                self.chunk += 1;
            }
            // Skip chunks with zero samples; bounded by the chunk count.
            loop {
                if self.chunk >= t.chunk_offsets.len() {
                    return None;
                }
                let cnum = self.chunk as u64 + 1;
                while self.stsc_i + 1 < t.stsc.len() && u64::from(t.stsc[self.stsc_i + 1].0) <= cnum {
                    self.stsc_i += 1;
                }
                let spc = t.stsc.get(self.stsc_i).map(|e| e.1).unwrap_or(0);
                if spc > 0 {
                    self.chunk_left = spc;
                    break;
                }
                if t.stsc.is_empty() {
                    return None;
                }
                self.chunk += 1;
            }
            self.pos = t.chunk_offsets[self.chunk];
            self.started = true;
        }

        // Duration.
        while self.stts_left == 0 {
            self.stts_i += 1;
            if self.stts_i >= t.stts.len() {
                return None;
            }
            self.stts_left = t.stts[self.stts_i].0;
        }
        let duration = t.stts[self.stts_i].1;
        self.stts_left -= 1;

        // Composition offset.
        let mut cts = 0;
        if !t.ctts.is_empty() {
            while self.ctts_left == 0 && self.ctts_i + 1 < t.ctts.len() {
                self.ctts_i += 1;
                self.ctts_left = t.ctts[self.ctts_i].0;
            }
            if self.ctts_left > 0 {
                cts = t.ctts[self.ctts_i].1;
                self.ctts_left -= 1;
            }
        }

        let size = if t.sample_size != 0 { t.sample_size } else { t.sizes[self.index as usize] };
        let number = self.index + 1;
        let keyframe = match &t.stss {
            None => true,
            Some(v) => {
                while self.stss_i < v.len() && v[self.stss_i] < number {
                    self.stss_i += 1;
                }
                self.stss_i < v.len() && v[self.stss_i] == number
            }
        };

        let s = Sample {
            offset: self.pos,
            size,
            dts: self.dts,
            pts: self.dts as i64 + i64::from(cts),
            cts_offset: cts,
            duration,
            keyframe,
        };
        self.pos = self.pos.saturating_add(u64::from(size));
        self.dts = self.dts.wrapping_add(u64::from(duration));
        self.chunk_left -= 1;
        self.index += 1;
        Some(s)
    }
}

// ---------------------------------------------------------------------------
// sidx

/// One reference of a segment index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SidxEntry {
    /// Absolute file offset of the referenced segment (its `moof`, or a nested `sidx`).
    pub offset: u64,
    /// Size in bytes.
    pub size: u32,
    /// Earliest presentation time of the segment, in `Sidx::timescale` units.
    pub start_time: u64,
    /// Duration in `Sidx::timescale` units.
    pub duration: u32,
    /// The reference is another `sidx` (hierarchical index) rather than media.
    pub is_sidx: bool,
    pub starts_with_sap: bool,
    pub sap_type: u8,
}

/// A parsed `sidx` (segment index).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sidx {
    pub reference_id: u32,
    pub timescale: u32,
    pub earliest_presentation_time: u64,
    pub entries: Vec<SidxEntry>,
}

impl Sidx {
    /// Parse a whole `sidx` box (header included, see [`BoxInfo::bytes`])
    /// that starts at absolute file offset `box_offset`.
    pub fn parse(bytes: &[u8], box_offset: u64) -> Result<Sidx, Mp4Error> {
        let hdr = BoxHeader::parse(bytes)?.ok_or(Mp4Error::NeedMoreData { needed: 16 })?;
        if &hdr.kind != b"sidx" {
            return Err(Mp4Error::Invalid("not a sidx box"));
        }
        let size = hdr.size.unwrap_or(bytes.len() as u64);
        if size > bytes.len() as u64 {
            return Err(Mp4Error::NeedMoreData { needed: size });
        }
        let mut r = Cursor::new(&bytes[hdr.header_len..size as usize]);
        let (v, _) = r.full_header()?;
        let reference_id = r.u32()?;
        let timescale = r.u32()?;
        let (ept, first_offset) = if v == 0 {
            (u64::from(r.u32()?), u64::from(r.u32()?))
        } else {
            (r.u64()?, r.u64()?)
        };
        r.u16()?;
        let n = r.u16()? as usize;
        if n > r.remaining() / 12 {
            return Err(Mp4Error::Invalid("sidx truncated"));
        }
        let mut entries = Vec::with_capacity(n);
        let mut offset = box_offset.saturating_add(size).saturating_add(first_offset);
        let mut time = ept;
        for _ in 0..n {
            let a = r.u32()?;
            let duration = r.u32()?;
            let c = r.u32()?;
            let size = a & 0x7fff_ffff;
            entries.push(SidxEntry {
                offset,
                size,
                start_time: time,
                duration,
                is_sidx: a & 0x8000_0000 != 0,
                starts_with_sap: c & 0x8000_0000 != 0,
                sap_type: ((c >> 28) & 7) as u8,
            });
            offset = offset.saturating_add(u64::from(size));
            time = time.saturating_add(u64::from(duration));
        }
        Ok(Sidx { reference_id, timescale, earliest_presentation_time: ept, entries })
    }

    /// Index of the segment containing time `t` (in `timescale` units).
    pub fn segment_at(&self, t: u64) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let i = self.entries.partition_point(|e| e.start_time <= t);
        Some(i.saturating_sub(1))
    }
}
