//! Local media files for the player (`mediaplay.rs`): reading a file from any
//! volume in pieces (never whole: DVD rips are 7 GB on exFAT), and the
//! containers -- Matroska / WebM (`ling-mkv`), MP4 / MOV regular and
//! fragmented (`ling-mp4`), AVI (idx1, or a scan), animated GIF and an image
//! sequence (`frame_00001.png` ..., LingAnime's export) -- each turned into
//! the same tracks and packets.

use crate::drivers::image;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

// ── the file ─────────────────────────────────────────────────────────────────

enum Backing {
    Range(crate::fs::media::MediaRange),
    Mem(Vec<u8>),
}

/// A file opened for reads at any offset.
pub struct FileSrc {
    b: Backing,
    len: u64,
    /// read-ahead window
    win_off: u64,
    win: Vec<u8>,
}

const WIN: usize = 512 * 1024;

/// Open `path` (a home path, `media/...`, or anything vfs reads): big files on a data volume
/// are read in pieces; small ones (lingfs, the image) come in whole.
pub fn open_source(path: &str) -> Option<FileSrc> {
    use crate::fs::media;
    let n = crate::vfs::norm(path);
    let mut cands: Vec<String> = Vec::new();
    if media::is_media_path(&n) {
        cands.push(n.clone());
    } else {
        if let Some(c) = crate::vfs::storage_path(&n) {
            cands.push(c);
        }
        let r = crate::vfs::resolve(&n);
        if let Some(v) = crate::vfs::package_volume() {
            cands.push(format!("media/{v}/{r}"));
        }
    }
    for c in &cands {
        if let Some(r) = media::open_range(c) {
            let len = r.size();
            return Some(FileSrc { b: Backing::Range(r), len, win_off: 0, win: Vec::new() });
        }
    }
    let v = crate::vfs::read(&n)?;
    let len = v.len() as u64;
    Some(FileSrc { b: Backing::Mem(v), len, win_off: 0, win: Vec::new() })
}

impl FileSrc {
    /// The file's length in bytes (an upload sends it as Content-Length before the body).
    pub fn len(&self) -> u64 {
        self.len
    }

    fn raw_read(&mut self, off: u64, buf: &mut [u8]) -> bool {
        match &self.b {
            Backing::Mem(v) => {
                let s = off as usize;
                match v.get(s..s + buf.len()) {
                    Some(b) => {
                        buf.copy_from_slice(b);
                        true
                    },
                    None => false,
                }
            },
            Backing::Range(r) => {
                let mut done = 0;
                while done < buf.len() {
                    // 64 KiB pieces with a chance to switch between them: a player's reader
                    // coroutine (media-read) filled its 512 KiB window in one call, 40-100 ms
                    // on a slow disk, and the desktop missed present slots meanwhile
                    // (maybe_preempt is a no-op outside a coroutine or within its time slice)
                    let n = (buf.len() - done).min(64 << 10);
                    if !r.read_at(off + done as u64, &mut buf[done..done + n]) {
                        return false;
                    }
                    done += n;
                    crate::coro::maybe_preempt();
                }
                true
            },
        }
    }

    /// `n` bytes at `off` (fewer at the end of the file), through the read-ahead window.
    pub fn read(&mut self, off: u64, n: usize) -> Option<Vec<u8>> {
        if off >= self.len {
            return None;
        }
        let n = n.min((self.len - off) as usize);
        let mut v = alloc::vec![0u8; n];
        if self.fill(off, &mut v) {
            Some(v)
        } else {
            None
        }
    }

    fn fill(&mut self, off: u64, buf: &mut [u8]) -> bool {
        if off.checked_add(buf.len() as u64).is_none_or(|e| e > self.len) {
            return false;
        }
        if buf.len() >= WIN / 2 {
            return self.raw_read(off, buf);
        }
        let inside = off >= self.win_off && off + buf.len() as u64 <= self.win_off + self.win.len() as u64;
        if !inside {
            let want = WIN.min((self.len - off) as usize);
            let mut w = core::mem::take(&mut self.win);
            w.resize(want, 0);
            if !self.raw_read(off, &mut w) {
                return false;
            }
            self.win = w;
            self.win_off = off;
        }
        let s = (off - self.win_off) as usize;
        buf.copy_from_slice(&self.win[s..s + buf.len()]);
        true
    }
}

impl ling_mkv::Source for FileSrc {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> bool {
        self.fill(off, buf)
    }
}

// ── tracks and packets ───────────────────────────────────────────────────────

#[derive(Clone)]
pub enum VCodec {
    /// avcC
    H264(Vec<u8>),
    /// sequence header (+ extension), may be empty
    Mpeg2(Vec<u8>),
    Mjpeg,
    /// a PNG / JPEG / BMP file per packet
    Picture,
    /// 8-byte header (w, h as u32 LE) + 0xFFRRGGBB pixels
    Raw,
}

#[derive(Clone)]
pub enum ACodec {
    /// AudioSpecificConfig
    Aac(Vec<u8>),
    /// MPEG audio layer I / II / III
    Mpa,
    /// identification, comment, setup headers
    Vorbis(Vec<Vec<u8>>),
    /// "fLaC" + metadata blocks
    Flac(Vec<u8>),
    Pcm { bits: u16, float: bool, be: bool, ch: u16, rate: u32 },
}

#[derive(Clone)]
pub enum SCodec {
    Srt,
    /// the script header ([Script Info] + styles)
    Ass(String),
}

#[derive(Clone)]
pub enum Codec {
    V(VCodec),
    A(ACodec),
    S(SCodec),
    /// a track LingOS cannot decode: the reason, said in the player
    No(String),
}

#[derive(Clone)]
pub struct TrackDesc {
    pub id: u64,
    /// 0 video, 1 audio, 2 subtitles
    pub kind: u8,
    pub codec: Codec,
    /// short codec name for menus ("H.264", "AAC", "Opus")
    pub cname: String,
    pub lang: String,
    pub name: String,
    pub default: bool,
    pub forced: bool,
    /// Matroska zlib compression of the frames
    pub zlib: bool,
    pub w: u32,
    pub h: u32,
    /// display aspect (0 = from the picture)
    pub dar: f64,
    pub rate: u32,
    pub channels: u32,
    /// frames per second when known
    pub fps: f64,
    pub interlaced: bool,
}

impl TrackDesc {
    fn new(id: u64, kind: u8, codec: Codec, cname: &str) -> TrackDesc {
        TrackDesc {
            id,
            kind,
            codec,
            cname: cname.to_string(),
            lang: String::new(),
            name: String::new(),
            default: false,
            forced: false,
            zlib: false,
            w: 0,
            h: 0,
            dar: 0.0,
            rate: 0,
            channels: 0,
            fps: 0.0,
            interlaced: false,
        }
    }
}

pub struct Pkt {
    pub track: u64,
    /// seconds
    pub pts: f64,
    pub dur: f64,
    pub key: bool,
    pub data: Vec<u8>,
}

pub struct Meta {
    pub format: &'static str,
    pub duration: f64,
    pub tracks: Vec<TrackDesc>,
    /// (start seconds, title)
    pub chapters: Vec<(f64, String)>,
    pub title: String,
    /// an animation that repeats (GIF)
    pub loops: bool,
}

pub enum Demux {
    Mkv(Box<MkvD>),
    Mp4(Box<Mp4D>),
    Avi(Box<AviD>),
    Gif(Box<GifD>),
    Seq(Box<SeqD>),
}

/// What the first bytes say the file is.
fn sniff(head: &[u8], path: &str) -> &'static str {
    let lp = path.to_ascii_lowercase();
    if head.len() >= 4 && head[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        "mkv"
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"AVI " {
        "avi"
    } else if head.len() >= 6 && (&head[..6] == b"GIF89a" || &head[..6] == b"GIF87a") {
        "gif"
    } else if head.len() >= 8 && matches!(&head[4..8], b"ftyp" | b"moov" | b"mdat" | b"free" | b"wide" | b"skip" | b"styp" | b"moof") {
        "mp4"
    } else if lp.ends_with(".png") || lp.ends_with(".jpg") || lp.ends_with(".bmp") {
        "seq"
    } else {
        "?"
    }
}

impl Demux {
    pub fn open(src: &mut FileSrc, path: &str) -> Result<(Demux, Meta), String> {
        let head = src.read(0, 64).unwrap_or_default();
        match sniff(&head, path) {
            "mkv" => MkvD::open(src),
            "mp4" => Mp4D::open(src),
            "avi" => AviD::open(src),
            "gif" => GifD::open(src),
            "seq" => SeqD::open(path),
            _ => Err("this is not a video format LingOS knows (MKV, WebM, MP4, MOV, AVI, GIF)".to_string()),
        }
    }

    /// Only these tracks' packets come out.
    pub fn set_wanted(&mut self, ids: &[u64]) {
        match self {
            Demux::Mkv(d) => d.r.wanted = ids.to_vec(),
            Demux::Mp4(d) => d.wanted = ids.to_vec(),
            Demux::Avi(d) => d.wanted = ids.to_vec(),
            Demux::Gif(_) | Demux::Seq(_) => {},
        }
    }

    pub fn next(&mut self, src: &mut FileSrc) -> Option<Pkt> {
        match self {
            Demux::Mkv(d) => d.next(src),
            Demux::Mp4(d) => d.next(src),
            Demux::Avi(d) => d.next(src),
            Demux::Gif(d) => d.next(src),
            Demux::Seq(d) => d.next(),
        }
    }

    /// Go to a key frame at or before `t` (seconds): the time it starts at.
    pub fn seek(&mut self, src: &mut FileSrc, t: f64) -> f64 {
        match self {
            Demux::Mkv(d) => d.seek(src, t),
            Demux::Mp4(d) => d.seek(src, t),
            Demux::Avi(d) => d.seek(t),
            Demux::Gif(d) => d.seek(t),
            Demux::Seq(d) => d.seek(t),
        }
    }

    /// Byte position (for the status line).
    pub fn position(&self) -> u64 {
        match self {
            Demux::Mkv(d) => d.r.position(),
            Demux::Mp4(d) => d.pos,
            Demux::Avi(d) => d.idx.get(d.next).map(|e| e.off).unwrap_or(0),
            _ => 0,
        }
    }
}

fn lang_name(code: &str) -> String {
    let c = code.split('-').next().unwrap_or(code).to_ascii_lowercase();
    let n = match c.as_str() {
        "eng" | "en" => "English",
        "jpn" | "ja" => "Japanese",
        "kor" | "ko" => "Korean",
        "chi" | "zho" | "zh" => "Chinese",
        "fre" | "fra" | "fr" => "French",
        "ger" | "deu" | "de" => "German",
        "spa" | "es" => "Spanish",
        "ita" | "it" => "Italian",
        "por" | "pt" => "Portuguese",
        "rus" | "ru" => "Russian",
        "tha" | "th" => "Thai",
        "ara" | "ar" => "Arabic",
        "dut" | "nld" | "nl" => "Dutch",
        "pol" | "pl" => "Polish",
        "swe" | "sv" => "Swedish",
        "fin" | "fi" => "Finnish",
        "nor" | "no" | "nob" => "Norwegian",
        "dan" | "da" => "Danish",
        "tur" | "tr" => "Turkish",
        "vie" | "vi" => "Vietnamese",
        "ind" | "id" => "Indonesian",
        "hin" | "hi" => "Hindi",
        "und" | "" => "",
        _ => return code.to_string(),
    };
    n.to_string()
}

/// The menu label of a track: "English - AAC 2.0 - Commentary".
pub fn track_label(t: &TrackDesc) -> String {
    let mut parts: Vec<String> = Vec::new();
    let l = lang_name(&t.lang);
    if !l.is_empty() {
        parts.push(l);
    }
    let mut c = t.cname.clone();
    if t.kind == 1 && t.channels > 0 {
        c.push_str(match t.channels {
            1 => " mono",
            2 => " stereo",
            6 => " 5.1",
            8 => " 7.1",
            _ => "",
        });
    }
    if matches!(t.codec, Codec::No(_)) {
        c.push_str(" (can't play)");
    }
    parts.push(c);
    if !t.name.is_empty() {
        parts.push(t.name.clone());
    }
    if t.forced {
        parts.push("forced".to_string());
    }
    parts.join(" - ")
}

// ── Matroska / WebM ──────────────────────────────────────────────────────────

pub struct MkvD {
    info: ling_mkv::Info,
    r: ling_mkv::Reader,
}

/// Matroska's codec ids -> what LingOS does with them.
fn mkv_codec(t: &ling_mkv::Track) -> (Codec, &'static str) {
    let id = t.codec_id.as_str();
    let cp = t.codec_private.clone();
    let no = |what: &str, s: &'static str| (Codec::No(what.to_string()), s);
    match id {
        "V_MPEG4/ISO/AVC" => (Codec::V(VCodec::H264(cp)), "H.264"),
        "V_MPEG2" | "V_MPEG1" => (Codec::V(VCodec::Mpeg2(cp)), if id == "V_MPEG1" { "MPEG-1" } else { "MPEG-2" }),
        "V_MJPEG" => (Codec::V(VCodec::Mjpeg), "Motion JPEG"),
        "V_MPEGH/ISO/HEVC" => no("HEVC (H.265) video isn't supported yet", "HEVC"),
        "V_VP9" => no("VP9 video isn't supported yet", "VP9"),
        "V_VP8" => no("VP8 video isn't supported yet", "VP8"),
        "V_AV1" => no("AV1 video isn't supported yet", "AV1"),
        "V_THEORA" => no("Theora video isn't supported yet", "Theora"),
        "V_PRORES" => no("ProRes video isn't supported yet", "ProRes"),
        "V_MS/VFW/FOURCC" => {
            // a BITMAPINFOHEADER: the fourcc at offset 16
            let fcc = cp.get(16..20).map(|f| [f[0], f[1], f[2], f[3]]).unwrap_or([0; 4]);
            match &fcc {
                b"MJPG" | b"mjpg" | b"AVRn" | b"dmb1" => (Codec::V(VCodec::Mjpeg), "Motion JPEG"),
                b"H264" | b"h264" | b"avc1" | b"X264" | b"x264" => no("H.264 in VFW mode isn't supported yet", "H.264"),
                _ => no("this video codec (MPEG-4 Part 2 / DivX / Xvid, ...) isn't supported yet", "MPEG-4"),
            }
        },
        "A_AAC" => (Codec::A(ACodec::Aac(cp)), "AAC"),
        "A_MPEG/L2" | "A_MPEG/L3" | "A_MPEG/L1" => (Codec::A(ACodec::Mpa), if id == "A_MPEG/L3" { "MP3" } else if id == "A_MPEG/L2" { "MP2" } else { "MP1" }),
        "A_VORBIS" => match ling_mkv::xiph_headers(&cp) {
            Some(h) if h.len() == 3 => (Codec::A(ACodec::Vorbis(h.into_iter().map(|x| x.to_vec()).collect())), "Vorbis"),
            _ => no("damaged Vorbis headers", "Vorbis"),
        },
        "A_FLAC" => (Codec::A(ACodec::Flac(cp)), "FLAC"),
        "A_PCM/INT/LIT" | "A_PCM/INT/BIG" | "A_PCM/FLOAT/IEEE" => (
            Codec::A(ACodec::Pcm {
                bits: t.audio.bit_depth.max(8) as u16,
                float: id.ends_with("IEEE"),
                be: id.ends_with("BIG"),
                ch: t.audio.channels.max(1) as u16,
                rate: t.audio.rate as u32,
            }),
            "PCM",
        ),
        "A_OPUS" => no("no decoder for Opus audio yet", "Opus"),
        "A_AC3" | "A_EAC3" => no("no decoder for Dolby Digital (AC-3) audio yet", if id == "A_AC3" { "AC-3" } else { "E-AC-3" }),
        "A_DTS" => no("no decoder for DTS audio yet", "DTS"),
        "A_TRUEHD" => no("no decoder for Dolby TrueHD audio yet", "TrueHD"),
        "A_ALAC" => no("no decoder for ALAC audio yet", "ALAC"),
        "S_TEXT/UTF8" | "S_TEXT/ASCII" => (Codec::S(SCodec::Srt), "SRT"),
        "S_TEXT/ASS" | "S_TEXT/SSA" | "S_ASS" | "S_SSA" => (Codec::S(SCodec::Ass(String::from_utf8_lossy(&cp).into_owned())), if id.contains("SSA") { "SSA" } else { "ASS" }),
        "S_HDMV/PGS" => no("picture subtitles (PGS) aren't supported yet", "PGS"),
        "S_VOBSUB" => no("picture subtitles (VobSub) aren't supported yet", "VobSub"),
        "S_TEXT/WEBVTT" | "D_WEBVTT/SUBTITLES" => (Codec::S(SCodec::Srt), "WebVTT"),
        _ => (Codec::No(format!("{id} isn't supported")), "?"),
    }
}

impl MkvD {
    fn open(src: &mut FileSrc) -> Result<(Demux, Meta), String> {
        let info = ling_mkv::open(src).map_err(|e| match e {
            ling_mkv::Error::NotMatroska => "not a Matroska file".to_string(),
            ling_mkv::Error::Corrupt(w) => format!("damaged Matroska file ({w})"),
            ling_mkv::Error::Io => "the file could not be read".to_string(),
        })?;
        let mut tracks = Vec::new();
        for t in &info.tracks {
            let kind = match t.kind {
                ling_mkv::Kind::Video => 0,
                ling_mkv::Kind::Audio => 1,
                ling_mkv::Kind::Subtitle => 2,
                ling_mkv::Kind::Other => continue,
            };
            let (mut codec, cname) = mkv_codec(t);
            if t.encrypted {
                codec = Codec::No("this track is encrypted or compressed in a way LingOS can't read".to_string());
            }
            let mut d = TrackDesc::new(t.number, kind, codec, cname);
            d.lang = t.language.clone();
            d.name = t.name.clone();
            d.default = t.default;
            d.forced = t.forced;
            d.zlib = t.zlib;
            d.w = t.video.pixel_w;
            d.h = t.video.pixel_h;
            let (dw, dh) = (t.video.display_w, t.video.display_h);
            if dw > 0 && dh > 0 {
                d.dar = dw as f64 / dh as f64;
            }
            d.interlaced = t.video.interlaced == 1;
            d.rate = (if t.audio.out_rate > 0.0 { t.audio.out_rate } else { t.audio.rate }) as u32;
            d.channels = t.audio.channels;
            if t.default_duration > 0 && kind == 0 {
                d.fps = 1e9 / t.default_duration as f64;
            }
            tracks.push(d);
        }
        let chapters = info.chapters.iter().map(|c| (c.start as f64 / 1e9, if c.title.is_empty() { String::new() } else { c.title.clone() })).collect();
        let meta = Meta {
            format: if info.doc_type == "webm" { "WebM" } else { "Matroska" },
            duration: info.duration as f64 / 1e9,
            tracks,
            chapters,
            title: info.title.clone(),
            loops: false,
        };
        let r = ling_mkv::Reader::new(&info);
        Ok((Demux::Mkv(Box::new(MkvD { info, r })), meta))
    }

    fn next(&mut self, src: &mut FileSrc) -> Option<Pkt> {
        let p = self.r.next_packet(src, &self.info)?;
        Some(Pkt { track: p.track, pts: p.pts as f64 / 1e9, dur: p.duration.unwrap_or(0) as f64 / 1e9, key: p.key, data: p.data })
    }

    fn seek(&mut self, src: &mut FileSrc, t: f64) -> f64 {
        self.r.seek(src, &self.info, (t.max(0.0) * 1e9) as u64) as f64 / 1e9
    }
}

// ── MP4 / MOV ────────────────────────────────────────────────────────────────

/// A track's walk through the `moov` sample tables.
struct Mp4Cur {
    id: u32,
    ts: f64,
    shift: i64,
    /// the iterator borrows `Mp4D::movie` (boxed, dropped after this)
    it: ling_mp4::SampleIter<'static>,
    peek: Option<ling_mp4::Sample>,
}

pub struct Mp4D {
    // the cursors borrow `movie`: they are declared (and dropped) first
    curs: Vec<Mp4Cur>,
    movie: Box<ling_mp4::Movie>,
    fragmented: bool,
    wanted: Vec<u64>,
    /// fragmented: where the next top-level box is, and packets of the current fragment
    pos: u64,
    frag_q: VecDeque<Pkt>,
    /// fragmented: (moof offset, first video pts, starts with a key frame) seen so far
    frags: Vec<(u64, f64, bool)>,
    first_frag: u64,
    len: u64,
    video_id: Option<u32>,
}

/// A top-level box header at `off`: (fourcc, total size, header length); size 0 = to the end.
fn mp4_box(src: &mut FileSrc, off: u64) -> Option<([u8; 4], u64, u64)> {
    let h = src.read(off, 16)?;
    if h.len() < 8 {
        return None;
    }
    let size32 = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
    let kind = [h[4], h[5], h[6], h[7]];
    let (size, hl) = match size32 {
        0 => (src.len - off, 8),
        1 if h.len() >= 16 => (u64::from_be_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]]), 16),
        s => (s, 8),
    };
    if size < hl {
        return None;
    }
    Some((kind, size, hl))
}

/// The payload of the first child box `kind` in a box payload.
fn child_box<'a>(b: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    let mut p = 0usize;
    while p + 8 <= b.len() {
        let mut size = u32::from_be_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]) as usize;
        let mut hl = 8;
        if size == 1 {
            size = u64::from_be_bytes(b.get(p + 8..p + 16)?.try_into().ok()?) as usize;
            hl = 16;
        } else if size == 0 {
            size = b.len() - p;
        }
        if size < hl || p + size > b.len() {
            return None;
        }
        if &b[p + 4..p + 8] == kind {
            return Some(&b[p + hl..p + size]);
        }
        p += size;
    }
    None
}

/// Nero chapters (`moov/udta/chpl`): (seconds, title).
fn mp4_chapters(moov: &[u8]) -> Vec<(f64, String)> {
    let mut out = Vec::new();
    let Some(udta) = child_box(moov, b"udta") else { return out };
    let Some(chpl) = child_box(udta, b"chpl") else { return out };
    if chpl.len() < 5 {
        return out;
    }
    let version = chpl[0];
    let mut p = 4 + if version >= 1 { 4 } else { 0 };
    let Some(&count) = chpl.get(p) else { return out };
    p += 1;
    for _ in 0..count {
        let Some(t) = chpl.get(p..p + 8) else { break };
        let t = u64::from_be_bytes([t[0], t[1], t[2], t[3], t[4], t[5], t[6], t[7]]);
        p += 8;
        let Some(&l) = chpl.get(p) else { break };
        p += 1;
        let title = chpl.get(p..p + l as usize).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
        p += l as usize;
        out.push((t as f64 / 1e7, title));
    }
    out
}

fn mp4_codec(t: &ling_mp4::Track) -> (u8, Codec, &'static str) {
    use ling_mp4::{Codec as C, TrackKind as K};
    let kind = match t.kind {
        K::Video => 0,
        K::Audio => 1,
        K::Other(h) if &h == b"sbtl" || &h == b"subt" || &h == b"text" => 2,
        K::Other(_) => 3,
    };
    let no = |k: u8, m: &str, s: &'static str| (k, Codec::No(m.to_string()), s);
    match &t.codec {
        C::Avc { avcc, .. } => (0, Codec::V(VCodec::H264(avcc.clone())), "H.264"),
        C::Mp4a { object_type_indication: oti, asc } => match oti {
            0x40 | 0x66 | 0x67 | 0x68 => (1, Codec::A(ACodec::Aac(asc.clone())), "AAC"),
            0x69 | 0x6B => (1, Codec::A(ACodec::Mpa), "MP3"),
            0xA5 | 0xA6 => no(1, "no decoder for Dolby Digital (AC-3) audio yet", "AC-3"),
            _ => no(1, "this MPEG-4 audio type isn't supported", "?"),
        },
        C::Other { fourcc, config } => match fourcc {
            b"hvc1" | b"hev1" => no(0, "HEVC (H.265) video isn't supported yet", "HEVC"),
            b"vp09" => no(0, "VP9 video isn't supported yet", "VP9"),
            b"av01" => no(0, "AV1 video isn't supported yet", "AV1"),
            b"apch" | b"apcn" | b"apcs" | b"apco" | b"ap4h" | b"ap4x" => no(0, "ProRes video isn't supported yet", "ProRes"),
            b"mp4v" => no(0, "MPEG-4 Part 2 video isn't supported yet", "MPEG-4"),
            b"jpeg" | b"mjpa" | b"mjpb" | b"MJPG" => (0, Codec::V(VCodec::Mjpeg), "Motion JPEG"),
            b"png " => (0, Codec::V(VCodec::Picture), "PNG"),
            b"m2v1" | b"mx5p" | b"xdv2" | b"hdv2" | b"mp2v" => (0, Codec::V(VCodec::Mpeg2(Vec::new())), "MPEG-2"),
            b".mp3" | b"ms\0U" => (1, Codec::A(ACodec::Mpa), "MP3"),
            b"Opus" => no(1, "no decoder for Opus audio yet", "Opus"),
            b"ac-3" | b"ec-3" => no(1, "no decoder for Dolby Digital (AC-3) audio yet", "AC-3"),
            b"alac" => no(1, "no decoder for ALAC audio yet", "ALAC"),
            b"fLaC" => {
                // dfLa: version/flags + metadata blocks
                let mut h = b"fLaC".to_vec();
                h.extend_from_slice(config.get(4..).unwrap_or(&[]));
                (1, Codec::A(ACodec::Flac(h)), "FLAC")
            },
            b"sowt" | b"twos" | b"lpcm" | b"raw " | b"in24" | b"fl32" => (
                1,
                Codec::A(ACodec::Pcm {
                    bits: match fourcc {
                        b"in24" => 24,
                        b"fl32" => 32,
                        b"raw " => 8,
                        _ => 16,
                    },
                    float: fourcc == b"fl32",
                    be: matches!(fourcc, b"twos" | b"in24" | b"fl32"),
                    ch: t.channels.max(1),
                    rate: t.sample_rate,
                }),
                "PCM",
            ),
            b"tx3g" | b"text" => no(2, "MP4 text subtitles aren't supported yet", "Timed Text"),
            _ => (kind, Codec::No(format!("the {} codec isn't supported", String::from_utf8_lossy(fourcc).trim())), "?"),
        },
    }
}

impl Mp4D {
    fn open(src: &mut FileSrc) -> Result<(Demux, Meta), String> {
        // find moov (at the start, or after the mdat)
        let mut off = 0u64;
        let mut moov: Option<(u64, u64, u64)> = None;
        let mut first_frag = 0u64;
        let mut guard = 0;
        while off < src.len && guard < 100_000 {
            guard += 1;
            let Some((kind, size, hl)) = mp4_box(src, off) else { break };
            if &kind == b"moov" {
                moov = Some((off, size, hl));
            }
            if (&kind == b"moof" || &kind == b"mdat") && first_frag == 0 {
                first_frag = off;
            }
            if moov.is_some() && first_frag != 0 {
                break;
            }
            off = off.saturating_add(size);
        }
        let (moff, msize, mhl) = moov.ok_or("this MP4 file has no movie header (moov): it may be unfinished")?;
        if msize > 256 << 20 {
            return Err("the MP4 movie header is too big".to_string());
        }
        let body = src.read(moff + mhl, (msize - mhl) as usize).ok_or("the MP4 movie header could not be read")?;
        let movie = ling_mp4::Movie::parse_moov(&body).map_err(|_| "damaged MP4 movie header".to_string())?;
        let chapters = mp4_chapters(&body);
        drop(body);
        let movie = Box::new(movie);
        let mut tracks = Vec::new();
        let mut video_id = None;
        for t in &movie.tracks {
            let (kind, codec, cname) = mp4_codec(t);
            if kind > 2 {
                continue;
            }
            let mut d = TrackDesc::new(t.id as u64, kind, codec, cname);
            d.w = t.width as u32;
            d.h = t.height as u32;
            d.rate = t.sample_rate;
            d.channels = t.channels as u32;
            if kind == 0 && video_id.is_none() && !matches!(d.codec, Codec::No(_)) {
                video_id = Some(t.id);
            }
            tracks.push(d);
        }
        // the first audio track is the default
        if let Some(a) = tracks.iter_mut().find(|t| t.kind == 1) {
            a.default = true;
        }
        let ts = movie.timescale.max(1) as f64;
        let mut duration = movie.duration as f64 / ts;
        if duration <= 0.0 {
            duration = movie.tracks.iter().map(|t| t.duration as f64 / t.timescale.max(1) as f64).fold(0.0, f64::max);
        }
        let fragmented = movie.fragmented && movie.tracks.iter().all(|t| t.sample_count() == 0);
        let mut d = Box::new(Mp4D {
            curs: Vec::new(),
            movie,
            fragmented,
            wanted: Vec::new(),
            pos: if first_frag != 0 { first_frag } else { moff + msize },
            frag_q: VecDeque::new(),
            frags: Vec::new(),
            first_frag: if first_frag != 0 { first_frag } else { moff + msize },
            len: src.len,
            video_id,
        });
        if fragmented && duration <= 0.0 {
            // no duration in the header: the last fragment's times
            duration = d.scan_duration(src);
        }
        d.restart_cursors(None);
        let meta = Meta { format: if fragmented { "MP4 (fragmented)" } else { "MP4" }, duration, tracks, chapters, title: String::new(), loops: false };
        Ok((Demux::Mp4(d), meta))
    }

    fn track_times(&self, id: u32) -> (f64, i64) {
        let t = self.movie.track(id);
        (t.map(|t| t.timescale.max(1) as f64).unwrap_or(1.0), t.and_then(|t| t.edit_media_time).unwrap_or(0).max(0))
    }

    /// Restart the sample-table walks (from the start, or each track at its first sample with dts >= `from`).
    fn restart_cursors(&mut self, from: Option<f64>) {
        self.curs.clear();
        if self.fragmented {
            return;
        }
        let ids: Vec<u32> = self.movie.tracks.iter().map(|t| t.id).collect();
        for id in ids {
            let (ts, shift) = self.track_times(id);
            let Some(t) = self.movie.track(id) else { continue };
            // SAFETY: `movie` is boxed (never moves) and outlives the cursors (dropped first, and
            // cleared before the movie is replaced, which it never is).
            let t: &'static ling_mp4::Track = unsafe { &*(t as *const ling_mp4::Track) };
            let mut it = t.samples();
            let mut peek = it.next();
            if let Some(f) = from {
                while let Some(s) = peek {
                    if (s.dts as i64 - shift) as f64 / ts >= f - 1e-6 {
                        break;
                    }
                    peek = it.next();
                }
            }
            self.curs.push(Mp4Cur { id, ts, shift, it, peek });
        }
    }

    fn want(&self, id: u32) -> bool {
        self.wanted.is_empty() || self.wanted.contains(&(id as u64))
    }

    fn next(&mut self, src: &mut FileSrc) -> Option<Pkt> {
        if self.fragmented {
            return self.next_frag(src);
        }
        // the wanted track whose next sample decodes first
        let mut best: Option<(usize, f64)> = None;
        for (i, c) in self.curs.iter().enumerate() {
            if !self.want(c.id) {
                continue;
            }
            if let Some(s) = c.peek {
                let t = (s.dts as i64 - c.shift) as f64 / c.ts;
                if best.is_none_or(|(_, b)| t < b) {
                    best = Some((i, t));
                }
            }
        }
        let (i, _) = best?;
        let c = &mut self.curs[i];
        let s = c.peek?;
        c.peek = c.it.next();
        let pts = (s.pts - c.shift) as f64 / c.ts;
        let (id, dur) = (c.id, s.duration as f64 / c.ts);
        self.pos = s.offset;
        let data = src.read(s.offset, s.size as usize).unwrap_or_default();
        Some(Pkt { track: id as u64, pts, dur, key: s.keyframe, data })
    }

    /// The next moof+mdat's samples into `frag_q`.
    fn next_frag(&mut self, src: &mut FileSrc) -> Option<Pkt> {
        let mut guard = 0;
        while self.frag_q.is_empty() {
            guard += 1;
            if guard > 100_000 || self.pos >= self.len {
                return None;
            }
            let (kind, size, _) = mp4_box(src, self.pos)?;
            let at = self.pos;
            self.pos = at.saturating_add(size);
            if &kind != b"moof" || size > 32 << 20 {
                continue;
            }
            let moof = src.read(at, size as usize)?;
            let Ok(frag) = self.movie.parse_fragment(&moof, at) else { continue };
            // (decode time, packet): decoders need decode order, B-frames come before their pts order
            let mut pk: Vec<(f64, Pkt)> = Vec::new();
            let mut first_v: Option<(f64, bool)> = None;
            let wanted = self.wanted.clone();
            let ids: Vec<u32> = self.movie.tracks.iter().map(|t| t.id).collect();
            for id in ids {
                let (ts, shift) = self.track_times(id);
                for s in frag.track_samples(id) {
                    let pts = (s.pts - shift) as f64 / ts;
                    if Some(id) == self.video_id && first_v.is_none() {
                        first_v = Some((pts, s.keyframe));
                    }
                    if !(wanted.is_empty() || wanted.contains(&(id as u64))) {
                        continue;
                    }
                    let data = src.read(at + s.offset, s.size as usize).unwrap_or_default();
                    let dts = (s.dts as i64 - shift) as f64 / ts;
                    pk.push((dts, Pkt { track: id as u64, pts, dur: s.duration as f64 / ts, key: s.keyframe, data }));
                }
            }
            if let Some((t, k)) = first_v {
                if self.frags.last().is_none_or(|f| f.0 < at) {
                    self.frags.push((at, t, k));
                }
            }
            // across tracks by decode time (stable: each track keeps its own order)
            pk.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
            self.frag_q.extend(pk.into_iter().map(|x| x.1));
        }
        self.frag_q.pop_front()
    }

    /// Hop over the fragments to the last one (its first time) for the duration.
    fn scan_duration(&mut self, src: &mut FileSrc) -> f64 {
        let mut off = self.first_frag;
        let mut last_moof = None;
        let mut guard = 0;
        while off < self.len && guard < 1_000_000 {
            guard += 1;
            let Some((kind, size, _)) = mp4_box(src, off) else { break };
            if &kind == b"moof" {
                last_moof = Some((off, size));
            }
            off = off.saturating_add(size);
        }
        let Some((at, size)) = last_moof else { return 0.0 };
        let Some(moof) = src.read(at, size as usize) else { return 0.0 };
        let mut m = self.movie.clone();
        let Ok(frag) = m.parse_fragment(&moof, at) else { return 0.0 };
        let mut end: f64 = 0.0;
        for t in &self.movie.tracks {
            let (ts, shift) = self.track_times(t.id);
            for s in frag.track_samples(t.id) {
                end = end.max((s.pts - shift + s.duration as i64) as f64 / ts);
            }
        }
        end
    }

    fn seek(&mut self, src: &mut FileSrc, t: f64) -> f64 {
        if self.fragmented {
            self.frag_q.clear();
            // fragments known so far; hop on (reading only moofs) when t lies beyond them
            let mut off = self.frags.last().map(|f| f.0).unwrap_or(self.first_frag);
            let mut guard = 0;
            while self.frags.last().is_none_or(|f| f.1 <= t) && off < self.len && guard < 1_000_000 {
                guard += 1;
                let Some((kind, size, _)) = mp4_box(src, off) else { break };
                if &kind == b"moof" && size < 32 << 20 && self.frags.last().is_none_or(|f| f.0 < off) {
                    if let Some(moof) = src.read(off, size as usize) {
                        if let Ok(frag) = self.movie.parse_fragment(&moof, off) {
                            let vid = self.video_id.unwrap_or(0);
                            let (ts, shift) = self.track_times(vid);
                            if let Some(s) = frag.track_samples(vid).next() {
                                self.frags.push((off, (s.pts - shift) as f64 / ts, s.keyframe));
                            }
                        }
                    }
                }
                off = off.saturating_add(size);
            }
            let pick = self.frags.iter().filter(|f| f.2 && f.1 <= t + 1e-6).last().or(self.frags.first()).copied();
            match pick {
                Some((at, ft, _)) => {
                    self.pos = at;
                    // fragment timing continues from the tfdt; times must not carry over
                    self.movie.reset_fragment_times();
                    return ft;
                },
                None => {
                    self.pos = self.first_frag;
                    return 0.0;
                },
            }
        }
        // regular: the key frame at or before t on the video track, every track from there
        let start = match self.video_id.and_then(|v| self.movie.track(v)) {
            Some(vt) => {
                let (ts, shift) = self.track_times(vt.id);
                let target = (t * ts) as i64 + shift;
                vt.seek_sample(target).map(|(_, s)| (s.pts - shift) as f64 / ts).unwrap_or(0.0)
            },
            None => t,
        };
        // video from its key frame (by dts, which is <= pts), audio and the rest from there too
        self.restart_cursors(Some(start - 1.0));
        // drop video samples before the key frame
        if let Some(v) = self.video_id {
            if let Some(c) = self.curs.iter_mut().find(|c| c.id == v) {
                while let Some(s) = c.peek {
                    if s.keyframe && (s.pts - c.shift) as f64 / c.ts >= start - 1e-6 {
                        break;
                    }
                    c.peek = c.it.next();
                }
            }
        }
        start
    }
}

// ── AVI ──────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct AviEntry {
    stream: u8,
    off: u64,
    size: u32,
    key: bool,
    t: f64,
}

pub struct AviD {
    idx: Vec<AviEntry>,
    next: usize,
    wanted: Vec<u64>,
}

struct AviStream {
    video: bool,
    scale: u32,
    rate: u32,
    sample_size: u32,
    avg_bytes: u32,
    block_align: u32,
    codec: Codec,
    cname: &'static str,
    w: u32,
    h: u32,
    sr: u32,
    ch: u32,
    /// running count: frames (video, VBR audio) or bytes (CBR audio)
    count: u64,
}

fn le32(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]])).unwrap_or(0)
}
fn le16(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2).map(|x| u16::from_le_bytes([x[0], x[1]])).unwrap_or(0)
}

fn avi_stream(strh: &[u8], strf: &[u8]) -> AviStream {
    let video = strh.get(0..4) == Some(b"vids");
    let scale = le32(strh, 20).max(1);
    let rate = le32(strh, 24).max(1);
    let sample_size = le32(strh, 44);
    let mut s = AviStream { video, scale, rate, sample_size, avg_bytes: 0, block_align: 1, codec: Codec::No("unknown stream".to_string()), cname: "?", w: 0, h: 0, sr: 0, ch: 0, count: 0 };
    if video {
        s.w = le32(strf, 4);
        s.h = (le32(strf, 8) as i32).unsigned_abs();
        let fcc = strf.get(16..20).unwrap_or(&[0; 4]);
        let (c, n): (Codec, &'static str) = match fcc {
            b"MJPG" | b"mjpg" | b"AVRn" | b"dmb1" | b"JPEG" => (Codec::V(VCodec::Mjpeg), "Motion JPEG"),
            b"H264" | b"h264" | b"X264" | b"x264" | b"avc1" => (Codec::No("H.264 in AVI isn't supported yet (remux it to MKV or MP4)".to_string()), "H.264"),
            b"mpg2" | b"MPG2" | b"mpg1" => (Codec::V(VCodec::Mpeg2(Vec::new())), "MPEG-2"),
            _ => (Codec::No(format!("the {} video codec isn't supported yet", String::from_utf8_lossy(fcc).trim())), "?"),
        };
        s.codec = c;
        s.cname = n;
    } else {
        let tag = le16(strf, 0);
        s.ch = le16(strf, 2) as u32;
        s.sr = le32(strf, 4);
        s.avg_bytes = le32(strf, 8);
        s.block_align = le16(strf, 12).max(1) as u32;
        let bits = le16(strf, 14);
        let (c, n): (Codec, &'static str) = match tag {
            1 => (Codec::A(ACodec::Pcm { bits: bits.max(8), float: false, be: false, ch: s.ch.max(1) as u16, rate: s.sr }), "PCM"),
            3 => (Codec::A(ACodec::Pcm { bits: 32, float: true, be: false, ch: s.ch.max(1) as u16, rate: s.sr }), "PCM"),
            0x50 | 0x55 => (Codec::A(ACodec::Mpa), if tag == 0x55 { "MP3" } else { "MP2" }),
            0x2000 => (Codec::No("no decoder for Dolby Digital (AC-3) audio yet".to_string()), "AC-3"),
            _ => (Codec::No(format!("audio format 0x{tag:04x} isn't supported")), "?"),
        };
        s.codec = c;
        s.cname = n;
    }
    s
}

impl AviD {
    fn open(src: &mut FileSrc) -> Result<(Demux, Meta), String> {
        // RIFF 'AVI ' > LIST hdrl > LIST strl (strh, strf)...; LIST movi; idx1
        let mut streams: Vec<AviStream> = Vec::new();
        let mut movi: Option<(u64, u64)> = None; // fourcc offset, end
        let mut idx1: Option<(u64, u32)> = None;
        let mut off = 12u64;
        let riff_end = (le32(&src.read(4, 4).unwrap_or_default(), 0) as u64 + 8).min(src.len);
        let mut guard = 0;
        while off + 8 <= riff_end && guard < 10_000 {
            guard += 1;
            let h = src.read(off, 12).ok_or("unreadable AVI")?;
            let id = [h[0], h[1], h[2], h[3]];
            let size = le32(&h, 4) as u64;
            if &id == b"LIST" && h.get(8..12) == Some(b"hdrl") {
                let body = src.read(off + 12, (size.saturating_sub(4)).min(1 << 20) as usize).ok_or("unreadable AVI header")?;
                // walk strl lists
                let mut p = 0usize;
                while p + 8 <= body.len() {
                    let cid = &body[p..p + 4];
                    let cs = le32(&body, p + 4) as usize;
                    if cid == b"LIST" && body.get(p + 8..p + 12) == Some(b"strl") {
                        let l = &body[p + 12..(p + 8 + cs).min(body.len())];
                        let (mut strh, mut strf): (&[u8], &[u8]) = (&[], &[]);
                        let mut q = 0usize;
                        while q + 8 <= l.len() {
                            let qs = le32(l, q + 4) as usize;
                            let d = &l[q + 8..(q + 8 + qs).min(l.len())];
                            match &l[q..q + 4] {
                                b"strh" => strh = d,
                                b"strf" => strf = d,
                                _ => {},
                            }
                            q += 8 + qs + (qs & 1);
                        }
                        streams.push(avi_stream(strh, strf));
                    }
                    p += 8 + cs + (cs & 1);
                }
            } else if &id == b"LIST" && h.get(8..12) == Some(b"movi") {
                movi = Some((off + 8, off + 8 + size));
            } else if &id == b"idx1" {
                idx1 = Some((off + 8, size as u32));
            }
            off = off.saturating_add(8 + size + (size & 1));
        }
        let (movi_at, movi_end) = movi.ok_or("this AVI file has no movie data")?;
        if streams.is_empty() {
            return Err("this AVI file has no streams".to_string());
        }
        let mut raw: Vec<(u8, u64, u32, bool)> = Vec::new(); // stream, data offset, size, key
        if let Some((at, size)) = idx1.filter(|i| i.1 >= 16) {
            let b = src.read(at, size as usize).ok_or("unreadable AVI index")?;
            // offsets are from the movi fourcc (usually) or the file start
            let first = le32(&b, 8) as u64;
            let base = if first >= movi_at && first < movi_end && first != 4 { 0 } else { movi_at };
            for e in b.chunks_exact(16) {
                let ck = &e[0..4];
                let (Some(s0), Some(s1)) = ((ck[0] as char).to_digit(10), (ck[1] as char).to_digit(10)) else { continue };
                let st = (s0 * 10 + s1) as u8;
                if &ck[2..4] == b"ix" || st as usize >= streams.len() {
                    continue;
                }
                let flags = le32(e, 4);
                let off = base + le32(e, 8) as u64 + 8;
                raw.push((st, off, le32(e, 12), flags & 0x10 != 0));
            }
        } else {
            // no index: scan the movi list (rec lists included)
            let mut p = movi_at + 4;
            let mut guard = 0u32;
            while p + 8 <= movi_end && guard < 10_000_000 {
                guard += 1;
                let Some(h) = src.read(p, 12) else { break };
                let size = le32(&h, 4) as u64;
                if &h[0..4] == b"LIST" {
                    p += 12;
                    continue;
                }
                if let (Some(s0), Some(s1)) = ((h[0] as char).to_digit(10), (h[1] as char).to_digit(10)) {
                    let st = (s0 * 10 + s1) as usize;
                    if st < streams.len() && &h[2..4] != b"ix" {
                        raw.push((st as u8, p + 8, size as u32, streams[st].video));
                    }
                }
                p = p.saturating_add(8 + size + (size & 1));
            }
        }
        // times: frame counts or byte counts per stream
        let mut idx = Vec::with_capacity(raw.len());
        for (st, off, size, key) in raw {
            let s = &mut streams[st as usize];
            let t = if s.video || s.sample_size == 0 {
                let t = s.count as f64 * s.scale as f64 / s.rate as f64;
                s.count += 1;
                t
            } else {
                let t = s.count as f64 / (s.avg_bytes.max(1) as f64);
                s.count += size as u64;
                t
            };
            // an empty video chunk holds the previous picture: no packet
            if size == 0 {
                continue;
            }
            idx.push(AviEntry { stream: st, off, size, key: key || !s.video, t });
        }
        let mut tracks = Vec::new();
        let mut duration: f64 = 0.0;
        for (i, s) in streams.iter().enumerate() {
            let len_t = if s.video || s.sample_size == 0 { s.count as f64 * s.scale as f64 / s.rate as f64 } else { s.count as f64 / s.avg_bytes.max(1) as f64 };
            duration = duration.max(len_t);
            let mut d = TrackDesc::new(i as u64, if s.video { 0 } else { 1 }, s.codec.clone(), s.cname);
            d.w = s.w;
            d.h = s.h;
            d.rate = s.sr;
            d.channels = s.ch;
            d.default = true;
            if s.video {
                d.fps = s.rate as f64 / s.scale as f64;
            }
            tracks.push(d);
        }
        // MJPEG frames are all key frames
        for e in idx.iter_mut() {
            if matches!(tracks.get(e.stream as usize).map(|t| &t.codec), Some(Codec::V(VCodec::Mjpeg))) {
                e.key = true;
            }
        }
        let meta = Meta { format: "AVI", duration, tracks, chapters: Vec::new(), title: String::new(), loops: false };
        Ok((Demux::Avi(Box::new(AviD { idx, next: 0, wanted: Vec::new() })), meta))
    }

    fn next(&mut self, src: &mut FileSrc) -> Option<Pkt> {
        while self.next < self.idx.len() {
            let e = self.idx[self.next];
            self.next += 1;
            if !self.wanted.is_empty() && !self.wanted.contains(&(e.stream as u64)) {
                continue;
            }
            let data = src.read(e.off, e.size as usize)?;
            return Some(Pkt { track: e.stream as u64, pts: e.t, dur: 0.0, key: e.key, data });
        }
        None
    }

    fn seek(&mut self, t: f64) -> f64 {
        // the last key frame of a video stream at or before t; from there in file order
        let vid = self.idx.iter().find(|e| e.key && e.size > 0).map(|e| e.stream);
        let mut best = 0usize;
        for (i, e) in self.idx.iter().enumerate() {
            if Some(e.stream) == vid && e.key {
                if e.t <= t + 1e-6 {
                    best = i;
                } else {
                    break;
                }
            }
        }
        // audio chunks written just before the key frame belong to its time too
        let kt = self.idx.get(best).map(|e| e.t).unwrap_or(0.0);
        let mut start = best;
        while start > 0 && self.idx[start - 1].t >= kt - 0.5 && Some(self.idx[start - 1].stream) != vid {
            start -= 1;
        }
        self.next = start;
        kt
    }
}

// ── animated GIF ─────────────────────────────────────────────────────────────

pub struct GifD {
    data: Vec<u8>,
    w: usize,
    h: usize,
    gct: Vec<u32>,
    bg: u8,
    /// (offset of the frame's graphic control / image descriptor, start time, delay)
    frames: Vec<(usize, f64)>,
    next: usize,
    canvas: Vec<u32>,
    /// what disposal 3 restores
    saved: Vec<u32>,
    /// the last frame's area and disposal, applied before the next
    pending_dispose: Option<(usize, usize, usize, usize, u8)>,
}

fn gif_palette(b: &[u8], n: usize) -> Vec<u32> {
    (0..n).map(|i| b.get(i * 3..i * 3 + 3).map(|c| 0xFF00_0000 | (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32).unwrap_or(0xFF00_0000)).collect()
}

/// Skip a run of data sub-blocks at `p`: the offset after the terminator.
fn gif_skip_blocks(d: &[u8], mut p: usize) -> usize {
    while let Some(&n) = d.get(p) {
        p += 1 + n as usize;
        if n == 0 {
            break;
        }
    }
    p
}

/// GIF's variable-width LZW: the index stream of one image.
fn gif_lzw(d: &[u8], mut p: usize, min: u8, want: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(want);
    if !(1..=11).contains(&min) {
        return out;
    }
    let clear = 1u32 << min;
    let end = clear + 1;
    let mut prefix = alloc::vec![0u16; 4096];
    let mut suffix = alloc::vec![0u8; 4096];
    let mut first = alloc::vec![0u8; 4096];
    let mut stack: Vec<u8> = Vec::with_capacity(4096);
    for i in 0..clear as usize {
        suffix[i] = i as u8;
        first[i] = i as u8;
    }
    let mut size = min as u32 + 1;
    let mut next = end + 1;
    let mut prev: Option<u32> = None;
    let (mut acc, mut bits) = (0u32, 0u32);
    let mut block_left = 0usize;
    'outer: loop {
        while bits < size {
            if block_left == 0 {
                let Some(&n) = d.get(p) else { break 'outer };
                p += 1;
                if n == 0 {
                    break 'outer;
                }
                block_left = n as usize;
            }
            let Some(&byte) = d.get(p) else { break 'outer };
            p += 1;
            block_left -= 1;
            acc |= (byte as u32) << bits;
            bits += 8;
        }
        let code = acc & ((1 << size) - 1);
        acc >>= size;
        bits -= size;
        if code == clear {
            size = min as u32 + 1;
            next = end + 1;
            prev = None;
            continue;
        }
        if code == end {
            break;
        }
        let Some(pc) = prev else {
            if code < clear {
                out.push(code as u8);
                prev = Some(code);
            }
            continue;
        };
        let cur = if code < next { code } else if code == next { pc } else { break };
        stack.clear();
        let mut c = cur;
        let mut guard = 0;
        while c >= clear && guard < 4096 {
            stack.push(suffix[c as usize]);
            c = prefix[c as usize] as u32;
            guard += 1;
        }
        stack.push(c as u8);
        let head = c as u8;
        if code == next {
            // KwKwK: the string of prev plus its own first char
            out.extend(stack.iter().rev());
            out.push(head);
        } else {
            out.extend(stack.iter().rev());
        }
        if next < 4096 {
            prefix[next as usize] = pc as u16;
            suffix[next as usize] = head;
            first[next as usize] = first[pc as usize];
            next += 1;
            if next == (1 << size) && size < 12 {
                size += 1;
            }
        }
        prev = Some(code);
        if out.len() >= want {
            break;
        }
    }
    out
}

impl GifD {
    fn open(src: &mut FileSrc) -> Result<(Demux, Meta), String> {
        if src.len > 256 << 20 {
            return Err("this GIF is too big".to_string());
        }
        let data = src.read(0, src.len as usize).ok_or("unreadable GIF")?;
        let w = le16(&data, 6) as usize;
        let h = le16(&data, 8) as usize;
        if w == 0 || h == 0 || w * h > 4096 * 4096 {
            return Err("a GIF of an unusable size".to_string());
        }
        let flags = data[10];
        let bg = data[11];
        let mut p = 13;
        let mut gct = Vec::new();
        if flags & 0x80 != 0 {
            let n = 2usize << (flags & 7);
            gct = gif_palette(&data[p..], n);
            p += 3 * n;
        }
        // the frames and their delays
        let mut frames = Vec::new();
        let mut t = 0.0f64;
        let mut delay = 0.1f64;
        let mut start = p;
        let mut guard = 0;
        while p < data.len() && guard < 1_000_000 {
            guard += 1;
            match data[p] {
                0x21 => {
                    if data.get(p + 1) == Some(&0xF9) {
                        start = p;
                        let d = le16(&data, p + 4) as f64 / 100.0;
                        delay = if d < 0.02 { 0.1 } else { d };
                    }
                    p = gif_skip_blocks(&data, p + 2);
                },
                0x2C => {
                    let lf = data.get(p + 9).copied().unwrap_or(0);
                    let mut q = p + 10;
                    if lf & 0x80 != 0 {
                        q += 3 * (2usize << (lf & 7));
                    }
                    q = gif_skip_blocks(&data, q + 1);
                    frames.push((if start < p && start > 0 { start } else { p }, t));
                    t += delay;
                    delay = 0.1;
                    start = 0;
                    p = q;
                },
                0x3B => break,
                _ => break,
            }
        }
        if frames.is_empty() {
            return Err("this GIF has no pictures".to_string());
        }
        let mut d = TrackDesc::new(0, 0, Codec::V(VCodec::Raw), "GIF");
        d.w = w as u32;
        d.h = h as u32;
        d.default = true;
        let meta = Meta { format: "GIF", duration: t, tracks: alloc::vec![d], chapters: Vec::new(), title: String::new(), loops: frames.len() > 1 };
        let g = GifD { data, w, h, gct, bg, frames, next: 0, canvas: alloc::vec![0xFF00_0000; w * h], saved: Vec::new(), pending_dispose: None };
        Ok((Demux::Gif(Box::new(g)), meta))
    }

    /// Compose frame `self.next` onto the canvas.
    fn render_next(&mut self) -> Option<f64> {
        let &(mut p, t) = self.frames.get(self.next)?;
        self.next += 1;
        if let Some((x, y, w, h, disp)) = self.pending_dispose.take() {
            match disp {
                2 => {
                    for yy in y..(y + h).min(self.h) {
                        for xx in x..(x + w).min(self.w) {
                            self.canvas[yy * self.w + xx] = 0xFF00_0000;
                        }
                    }
                },
                3 if self.saved.len() == self.canvas.len() => self.canvas.copy_from_slice(&self.saved),
                _ => {},
            }
        }
        let d = &self.data;
        let mut transparent: Option<u8> = None;
        let mut disposal = 0u8;
        let mut guard = 0;
        while p < d.len() && d[p] == 0x21 && guard < 1000 {
            guard += 1;
            if d.get(p + 1) == Some(&0xF9) {
                let pf = d.get(p + 3).copied().unwrap_or(0);
                disposal = (pf >> 2) & 7;
                if pf & 1 != 0 {
                    transparent = d.get(p + 6).copied();
                }
            }
            p = gif_skip_blocks(d, p + 2);
        }
        if d.get(p) != Some(&0x2C) {
            return Some(t);
        }
        let (fx, fy, fw, fh) = (le16(d, p + 1) as usize, le16(d, p + 3) as usize, le16(d, p + 5) as usize, le16(d, p + 7) as usize);
        let lf = d[p + 9];
        let mut q = p + 10;
        let lct;
        let pal: &[u32] = if lf & 0x80 != 0 {
            let n = 2usize << (lf & 7);
            lct = gif_palette(&d[q..], n);
            q += 3 * n;
            &lct
        } else {
            &self.gct
        };
        let min = d.get(q).copied().unwrap_or(8);
        let ix = gif_lzw(d, q + 1, min, fw * fh);
        if disposal == 3 {
            self.saved = self.canvas.clone();
        }
        let interlaced = lf & 0x40 != 0;
        let rows: Vec<usize> = if interlaced {
            let mut r = Vec::with_capacity(fh);
            for (start, step) in [(0, 8), (4, 8), (2, 4), (1, 2)] {
                let mut y = start;
                while y < fh {
                    r.push(y);
                    y += step;
                }
            }
            r
        } else {
            (0..fh).collect()
        };
        for (k, &ry) in rows.iter().enumerate() {
            let yy = fy + ry;
            if yy >= self.h {
                continue;
            }
            for xx in 0..fw {
                let Some(&c) = ix.get(k * fw + xx) else { break };
                if Some(c) == transparent {
                    continue;
                }
                let x = fx + xx;
                if x < self.w {
                    self.canvas[yy * self.w + x] = pal.get(c as usize).copied().unwrap_or(0xFF00_0000);
                }
            }
        }
        let _ = self.bg;
        self.pending_dispose = Some((fx, fy, fw, fh, disposal));
        Some(t)
    }

    fn next(&mut self, _src: &mut FileSrc) -> Option<Pkt> {
        let t = self.render_next()?;
        let mut data = Vec::with_capacity(8 + self.canvas.len() * 4);
        data.extend_from_slice(&(self.w as u32).to_le_bytes());
        data.extend_from_slice(&(self.h as u32).to_le_bytes());
        for &px in &self.canvas {
            data.extend_from_slice(&px.to_le_bytes());
        }
        Some(Pkt { track: 0, pts: t, dur: 0.0, key: true, data })
    }

    fn seek(&mut self, t: f64) -> f64 {
        // frames build on each other: render from the start up to the one before t
        self.next = 0;
        self.pending_dispose = None;
        self.canvas.iter_mut().for_each(|p| *p = 0xFF00_0000);
        let target = self.frames.iter().rposition(|f| f.1 <= t + 1e-6).unwrap_or(0);
        while self.next < target {
            if self.render_next().is_none() {
                break;
            }
        }
        self.frames.get(target).map(|f| f.1).unwrap_or(0.0)
    }
}

// ── an image sequence (LingAnime's PNG frames) ───────────────────────────────

pub struct SeqD {
    files: Vec<String>,
    fps: f64,
    next: usize,
}

/// The frames next to `path` that share its name pattern: `frame_00001.png` -> every `frame_<digits>.png`.
fn seq_files(path: &str) -> Vec<String> {
    let dir = crate::vfs::parent(path);
    let base = crate::vfs::basename(path);
    let lower = base.to_ascii_lowercase();
    let dot = lower.rfind('.').unwrap_or(lower.len());
    let stem = &lower[..dot];
    let ext = &lower[dot..];
    let digits = stem.chars().rev().take_while(|c| c.is_ascii_digit()).count();
    let prefix = &stem[..stem.len() - digits];
    let mut v: Vec<(u64, String)> = crate::vfs::list(&dir)
        .into_iter()
        .filter(|(n, d, _)| !d && {
            let l = n.to_ascii_lowercase();
            l.ends_with(ext) && l.starts_with(prefix) && l[prefix.len()..l.len() - ext.len()].chars().all(|c| c.is_ascii_digit()) && l.len() > prefix.len() + ext.len()
        })
        .map(|(n, _, _)| {
            let l = n.to_ascii_lowercase();
            let num: u64 = l[prefix.len()..l.len() - ext.len()].parse().unwrap_or(0);
            (num, crate::vfs::join(&dir, &n))
        })
        .collect();
    v.sort();
    v.into_iter().map(|x| x.1).collect()
}

impl SeqD {
    fn open(path: &str) -> Result<(Demux, Meta), String> {
        let files = seq_files(path);
        if files.len() < 2 {
            return Err("a single picture: open it in the viewer".to_string());
        }
        // LingAnime writes its frame rate nowhere: 24 fps, or "fps.txt" / "<n>fps" in the folder name
        let dir = crate::vfs::parent(path);
        let mut fps = 24.0;
        if let Some(b) = crate::vfs::read(&crate::vfs::join(&dir, "fps.txt")) {
            if let Ok(v) = core::str::from_utf8(&b).unwrap_or("").trim().parse::<f64>() {
                if v > 0.5 && v < 240.0 {
                    fps = v;
                }
            }
        }
        let first = crate::vfs::read(&files[0]).and_then(|b| image::decode_png_rgba(&b).or_else(|| image::decode_web(&b)));
        let mut d = TrackDesc::new(0, 0, Codec::V(VCodec::Picture), "PNG frames");
        if let Some(img) = first {
            d.w = img.w;
            d.h = img.h;
        }
        d.fps = fps;
        d.default = true;
        let n = files.len();
        let meta = Meta { format: "image sequence", duration: n as f64 / fps, tracks: alloc::vec![d], chapters: Vec::new(), title: String::new(), loops: true };
        Ok((Demux::Seq(Box::new(SeqD { files, fps, next: 0 })), meta))
    }

    fn next(&mut self) -> Option<Pkt> {
        let f = self.files.get(self.next)?.clone();
        let t = self.next as f64 / self.fps;
        self.next += 1;
        let data = crate::vfs::read(&f).unwrap_or_default();
        Some(Pkt { track: 0, pts: t, dur: 1.0 / self.fps, key: true, data })
    }

    fn seek(&mut self, t: f64) -> f64 {
        self.next = ((t.max(0.0) * self.fps) as usize).min(self.files.len().saturating_sub(1));
        self.next as f64 / self.fps
    }
}

/// Subtitle files next to a video with its name: `movie.srt`, `movie.en.srt`, `movie.ass`, ...
pub fn external_subs(path: &str) -> Vec<(String, String)> {
    let dir = crate::vfs::parent(path);
    let base = crate::vfs::basename(path);
    let stem = match base.rfind('.') {
        Some(i) => &base[..i],
        None => &base[..],
    };
    let sl = stem.to_ascii_lowercase();
    let mut out = Vec::new();
    for (n, d, _) in crate::vfs::list(&dir) {
        if d {
            continue;
        }
        let l = n.to_ascii_lowercase();
        if !(l.ends_with(".srt") || l.ends_with(".ass") || l.ends_with(".ssa") || l.ends_with(".vtt")) {
            continue;
        }
        if l.starts_with(&sl) && (l.len() == sl.len() + 4 || l.as_bytes().get(sl.len()) == Some(&b'.')) {
            // the middle part is a language or a name: "movie.en.srt" -> "en"
            let mid = &n[stem.len()..n.len() - 4];
            out.push((crate::vfs::join(&dir, &n), mid.trim_matches('.').to_string()));
        }
    }
    out.sort();
    out
}
