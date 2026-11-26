//! YouTube downloads for Horizon's Download buttons (`os_video_save*`), done
//! by LingOS itself like the player (`videoplay.rs`):
//!
//! * **MP4** -- YouTube no longer hands out a file with picture and sound in
//!   one (itag 18 is gone for the clients that answer without a JavaScript
//!   challenge), so the same DASH streams the player uses -- H.264 (at most
//!   720p on a PC, 360p on the Pi) and AAC-LC (itag 140) -- are joined here
//!   into one fragmented MP4: the two init segments' `trak`s go into one
//!   `moov` (the sound becomes track 2), and the media segments (moof+mdat,
//!   whose sample offsets are relative to their moof) are copied in time
//!   order with their track id and sequence number rewritten. No re-encoding:
//!   the picture and sound are YouTube's own. The Files app's player
//!   (`mediafile.rs`) and every other player read it.
//! * **FLAC / WAV** -- only the sound stream is fetched, decoded with
//!   `ling-aac` and written as 16-bit PCM (WAV) or compressed losslessly
//!   (`codec::flac_enc`). The length is known before the first byte (the
//!   sound's segment index gives its exact duration), so both headers are
//!   written up front and the file streams straight onto the disk.
//!
//! Files go onto a data volume in pieces (`fs::media::file_create`), never
//! whole in memory; a folder on the system disk (lingfs, ~16 MiB) takes small
//! files only, a big one goes to the data drive's Downloads folder instead.

use crate::codec::flac_enc::{wav_header, FlacEncoder};
use crate::drivers::videoplay::{self as vp, mp4};
use crate::drivers::youtube as yt;
use crate::fs::media;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

fn log(m: &str) {
    crate::console_write(format!("[ytsave] {m}\n").as_bytes());
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Mp4,
    Flac,
    Wav,
}

struct Job {
    id: u32,
    /// 0 working, 1 done, -1 failed
    state: i8,
    progress: f64,
    /// what it is doing / the error / where the file went
    note: String,
    path: String,
    cancel: bool,
}

static mut JOBS: Vec<Job> = Vec::new();
static mut NEXT: u32 = 1;

fn jobs() -> &'static mut Vec<Job> {
    unsafe { &mut *(&raw mut JOBS) }
}

fn job(id: u32) -> Option<&'static mut Job> {
    jobs().iter_mut().find(|j| j.id == id)
}

fn set(id: u32, f: impl FnOnce(&mut Job)) {
    if let Some(j) = job(id) {
        f(j);
    }
}

fn cancelled(id: u32) -> bool {
    job(id).is_none_or(|j| j.cancel)
}

// ── where the bytes go ───────────────────────────────────────────────────────

/// Small files for the system disk are written whole at the end.
const MEM_MAX: u64 = 8 << 20;
/// Pieces (each at most 2 MiB) fetched ahead of the one being written.
const PREFETCH: usize = 3;

enum Sink {
    Media { f: media::MediaFile, path: String },
    Mem { buf: Vec<u8>, path: String },
}

impl Sink {
    /// Open `path` (a vfs path from the Save dialog, or `media/...`) for about `est` bytes:
    /// (sink, the path the file ends up at, a note when it went elsewhere).
    fn open(path: &str, est: u64) -> Result<(Sink, String, String), String> {
        let n = crate::vfs::norm(path);
        let target = if media::is_media_path(&n) { Some(n.clone()) } else { crate::vfs::storage_path(&n) };
        let (target, shown, note) = match target {
            Some(t) => (t, path.to_string(), String::new()),
            None if est <= MEM_MAX => return Ok((Sink::Mem { buf: Vec::with_capacity(est as usize + 4096), path: n.clone() }, path.to_string(), String::new())),
            None => {
                // the system disk holds only small files: the data drive's Downloads folder
                let Some(v) = crate::vfs::package_volume() else {
                    return Err(format!("{:.0} MB is too big for the system disk: choose a folder on a data drive (media/...)", est as f64 / 1048576.0));
                };
                let t = format!("media/{v}/Downloads/{}", crate::vfs::basename(&n));
                (t.clone(), t, String::from(" (too big for the system disk, so it went to the data drive)"))
            },
        };
        if let Some(i) = target.rfind('/') {
            media::mkdir_p(&target[..i]);
        }
        if media::exists(&target) {
            media::delete(&target);
        }
        let f = media::file_create(&target).ok_or_else(|| format!("cannot create {shown}"))?;
        Ok((Sink::Media { f, path: target }, shown, note))
    }

    fn write(&mut self, b: &[u8]) -> Result<(), String> {
        match self {
            Sink::Media { f, path } => {
                if media::file_write(f, b) {
                    Ok(())
                } else {
                    Err(format!("writing {path} failed: is the drive full?"))
                }
            },
            Sink::Mem { buf, .. } => {
                buf.extend_from_slice(b);
                Ok(())
            },
        }
    }

    fn finish(self) -> Result<(), String> {
        match self {
            Sink::Media { f, path } => {
                if media::file_finish(f) {
                    Ok(())
                } else {
                    Err(format!("finishing {path} failed"))
                }
            },
            Sink::Mem { buf, path } => {
                if let Some(i) = path.rfind('/') {
                    crate::vfs::mkdir(&path[..i]);
                }
                if crate::vfs::write(&path, &buf) {
                    Ok(())
                } else {
                    Err(format!("could not write {path} ({:.1} MB)", buf.len() as f64 / 1048576.0))
                }
            },
        }
    }

    fn abort(self) {
        if let Sink::Media { f, .. } = self {
            media::file_abort(f);
        }
    }
}

// ── MP4 boxes ────────────────────────────────────────────────────────────────

/// The boxes in `b`: (fourcc, start, end, header length).
fn boxes(b: &[u8]) -> Vec<([u8; 4], usize, usize, usize)> {
    let mut out = Vec::new();
    let mut o = 0usize;
    while o + 8 <= b.len() {
        let s32 = u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as u64;
        let kind = [b[o + 4], b[o + 5], b[o + 6], b[o + 7]];
        let (size, hl) = match s32 {
            0 => ((b.len() - o) as u64, 8),
            1 if o + 16 <= b.len() => (u64::from_be_bytes(b[o + 8..o + 16].try_into().unwrap_or([0; 8])), 16),
            s => (s, 8),
        };
        if size < hl as u64 || o as u64 + size > b.len() as u64 {
            break;
        }
        out.push((kind, o, o + size as usize, hl));
        o += size as usize;
    }
    out
}

/// The first child `kind` of the box `b[s..e]` (header length `hl`): (start, end, header length), absolute.
fn child(b: &[u8], s: usize, e: usize, hl: usize, kind: &[u8; 4]) -> Option<(usize, usize, usize)> {
    boxes(&b[s + hl..e]).into_iter().find(|x| &x.0 == kind).map(|x| (s + hl + x.1, s + hl + x.2, x.3))
}

fn top(b: &[u8], kind: &[u8; 4]) -> Option<(usize, usize, usize)> {
    boxes(b).into_iter().find(|x| &x.0 == kind).map(|x| (x.1, x.2, x.3))
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_be_bytes());
}

fn mkbox(kind: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
    let len: usize = 8 + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut o = Vec::with_capacity(len);
    o.extend_from_slice(&(len as u32).to_be_bytes());
    o.extend_from_slice(kind);
    for p in parts {
        o.extend_from_slice(p);
    }
    o
}

/// A movie header's timescale (mvhd at `s` with header length `hl`).
fn mvhd_timescale(b: &[u8], s: usize, hl: usize) -> u32 {
    let c = s + hl;
    if b[c] == 1 { be32(b, c + 20) } else { be32(b, c + 12) }
}

fn rescale(v: u64, from: u32, to: u32) -> u64 {
    if from == 0 || from == to {
        return v;
    }
    ((v as u128 * to as u128) / from as u128) as u64
}

/// The sound's `trak`, made track 2 of a movie whose timescale is `to` (it had `from`).
fn audio_trak(head: &[u8], trak: (usize, usize, usize), from: u32, to: u32) -> Option<Vec<u8>> {
    let mut t = head[trak.0..trak.1].to_vec();
    let (ts, te, thl) = (0usize, t.len(), trak.2);
    let (ks, _, khl) = child(&t, ts, te, thl, b"tkhd")?;
    let c = ks + khl;
    if t[c] == 1 {
        put32(&mut t, c + 20, 2);
        let d = u64::from_be_bytes(t[c + 28..c + 36].try_into().ok()?);
        t[c + 28..c + 36].copy_from_slice(&rescale(d, from, to).to_be_bytes());
    } else {
        put32(&mut t, c + 12, 2);
        let d = be32(&t, c + 20) as u64;
        put32(&mut t, c + 20, rescale(d, from, to).min(u32::MAX as u64) as u32);
    }
    // an edit list's durations are in the movie's timescale too
    if let Some((es, ee, ehl)) = child(&t, ts, te, thl, b"edts") {
        if let Some((ls, _, lhl)) = child(&t, es, ee, ehl, b"elst") {
            let c = ls + lhl;
            let v1 = t[c] == 1;
            let n = be32(&t, c + 4) as usize;
            let mut o = c + 8;
            for _ in 0..n {
                if v1 {
                    let d = u64::from_be_bytes(t[o..o + 8].try_into().ok()?);
                    t[o..o + 8].copy_from_slice(&rescale(d, from, to).to_be_bytes());
                    o += 20;
                } else {
                    let d = be32(&t, o) as u64;
                    put32(&mut t, o, rescale(d, from, to).min(u32::MAX as u64) as u32);
                    o += 12;
                }
            }
        }
    }
    Some(t)
}

/// ftyp + one moov holding the picture's trak (track 1) and the sound's (track 2).
fn joined_header(vhead: &[u8], ahead: &[u8]) -> Option<Vec<u8>> {
    let ftyp = top(vhead, b"ftyp")?;
    let vm = top(vhead, b"moov")?;
    let am = top(ahead, b"moov")?;
    let vmvhd = child(vhead, vm.0, vm.1, vm.2, b"mvhd")?;
    let amvhd = child(ahead, am.0, am.1, am.2, b"mvhd")?;
    let vtrak = child(vhead, vm.0, vm.1, vm.2, b"trak")?;
    let atrak = child(ahead, am.0, am.1, am.2, b"trak")?;
    let vmvex = child(vhead, vm.0, vm.1, vm.2, b"mvex")?;
    let amvex = child(ahead, am.0, am.1, am.2, b"mvex")?;
    let vtrex = child(vhead, vmvex.0, vmvex.1, vmvex.2, b"trex")?;
    let atrex = child(ahead, amvex.0, amvex.1, amvex.2, b"trex")?;
    let vts = mvhd_timescale(vhead, vmvhd.0, vmvhd.2);
    let ats = mvhd_timescale(ahead, amvhd.0, amvhd.2);
    let mut mvhd = vhead[vmvhd.0..vmvhd.1].to_vec();
    let l = mvhd.len();
    put32(&mut mvhd, l - 4, 3); // next_track_ID
    let mut trex2 = ahead[atrex.0..atrex.1].to_vec();
    put32(&mut trex2, atrex.2 + 4, 2);
    let mvex = mkbox(b"mvex", &[&vhead[vtrex.0..vtrex.1], &trex2]);
    let atrak2 = audio_trak(ahead, atrak, ats, vts)?;
    let moov = mkbox(b"moov", &[&mvhd, &mvex, &vhead[vtrak.0..vtrak.1], &atrak2]);
    let mut out = vhead[ftyp.0..ftyp.1].to_vec();
    out.extend_from_slice(&moov);
    Some(out)
}

/// One media segment's moof+mdat pairs, with the sequence numbers and (for the sound) the track id rewritten.
fn patch_segment(seg: &mut [u8], track: u32, seq: &mut u32) -> Vec<(usize, usize)> {
    let mut keep = Vec::new();
    for (kind, s, e, hl) in boxes(seg) {
        match &kind {
            b"moof" => {
                for (ck, cs, ce, chl) in boxes(&seg[s + hl..e]).into_iter().map(|x| (x.0, s + hl + x.1, s + hl + x.2, x.3)) {
                    if &ck == b"mfhd" {
                        *seq += 1;
                        put32(seg, cs + chl + 4, *seq);
                    } else if &ck == b"traf" {
                        if let Some((ts, _, thl)) = child(seg, cs, ce, chl, b"tfhd") {
                            put32(seg, ts + thl + 4, track);
                        }
                    }
                }
                keep.push((s, e));
            },
            b"mdat" => keep.push((s, e)),
            _ => {},
        }
    }
    keep
}

// ── fetching ─────────────────────────────────────────────────────────────────

/// A stream's head (init segment + sidx), its parsed init, segments, and the sidx's total duration.
struct Stream {
    fmt: yt::Format,
    /// The address in use (a fresh one replaces it when YouTube refuses the old one), and the video's id.
    url: core::cell::RefCell<String>,
    vid: String,
    head: Vec<u8>,
    init: mp4::Init,
    segs: Vec<vp::Seg>,
    /// (sum of the segment durations, sidx timescale)
    dur: (u64, u32),
}

fn open_stream(fmt: yt::Format, vid: &str, keep: &dyn Fn() -> bool) -> Result<Stream, String> {
    let end = fmt.index.1.max(fmt.init.1);
    let head = vp::get_range(&fmt.url, 0, end, keep)?;
    let init = mp4::parse_init(&head[..(fmt.init.1 as usize + 1).min(head.len())]).ok_or("unreadable init segment")?;
    let idx = &head[(fmt.index.0 as usize).min(head.len())..];
    let segs = mp4::parse_sidx(idx, fmt.index.0, &init).ok_or("unreadable segment index")?;
    let sidx = ling_mp4::Sidx::parse(idx, fmt.index.0).map_err(|_| "unreadable segment index")?;
    let dur = (sidx.entries.iter().map(|e| e.duration as u64).sum::<u64>(), sidx.timescale.max(1));
    if segs.is_empty() {
        return Err("empty segment index".to_string());
    }
    Ok(Stream { url: core::cell::RefCell::new(fmt.url.clone()), vid: vid.to_string(), fmt, head, init, segs, dur })
}

/// Segments `i..` that fit one request (at most 2 MiB, at least one): the end index.
fn group_end(segs: &[vp::Seg], i: usize) -> usize {
    let mut j = i + 1;
    let mut size = segs[i].size;
    while j < segs.len() && segs[j].off == segs[j - 1].off + segs[j - 1].size && size + segs[j].size <= 2 << 20 {
        size += segs[j].size;
        j += 1;
    }
    j
}

/// Bytes of segments `i..j` in one piece (retried a few times).
fn fetch(st: &Stream, i: usize, j: usize, keep: &dyn Fn() -> bool) -> Result<Vec<u8>, String> {
    let (a, b) = (st.segs[i].off, st.segs[j - 1].off + st.segs[j - 1].size - 1);
    let mut last = String::new();
    let mut refreshed = false;
    for _ in 0..4 {
        let url = st.url.borrow().clone();
        match vp::get_range(&url, a, b, keep) {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !keep() {
                    return Err("cancelled".to_string());
                }
                if e.contains("HTTP 403") || e.contains("HTTP 404") || e.contains("HTTP 410") {
                    // the address expired or was withdrawn: a fresh one for the same stream, once
                    let fresh = if refreshed { None } else { vp::resolve(&st.vid, &|_m| {}).ok() };
                    refreshed = true;
                    let f = fresh.and_then(|s| [s.video, s.audio].into_iter().flatten().find(|f| f.itag == st.fmt.itag));
                    match f {
                        Some(f) => {
                            log(&format!("{}: fresh address for {}", st.vid, yt::short(&st.fmt)));
                            *st.url.borrow_mut() = f.url;
                            continue;
                        },
                        None => return Err(format!("YouTube stopped serving the video ({e})")),
                    }
                }
                last = e;
                crate::coro::sleep_ms(800);
            },
        }
    }
    Err(last)
}

fn save_mp4(id: u32, vid: &str, streams: &yt::Streams, path: &str) -> Result<(String, String), String> {
    let keep = move || !cancelled(id);
    let vf = streams.video.clone().ok_or("YouTube offered no H.264 picture stream for this video")?;
    let af = streams.audio.clone().ok_or("YouTube offered no AAC sound stream for this video")?;
    log(&format!("mp4: picture {}, sound {}", yt::short(&vf), yt::short(&af)));
    set(id, |j| j.note = "opening the streams".to_string());
    let v = open_stream(vf, vid, &keep).map_err(|e| format!("picture stream: {e}"))?;
    let a = open_stream(af, vid, &keep).map_err(|e| format!("sound stream: {e}"))?;
    let header = joined_header(&v.head, &a.head).ok_or("the streams' MP4 headers could not be joined")?;
    let total: u64 = v.segs.iter().chain(a.segs.iter()).map(|s| s.size).sum();
    let (mut sink, shown, note) = Sink::open(path, total + header.len() as u64)?;
    let r = (|| -> Result<(), String> {
        sink.write(&header)?;
        // the order the pieces go into the file: the stream that is behind in time goes next
        let mut order: Vec<(bool, usize, usize)> = Vec::new();
        {
            let (mut vi, mut ai) = (0usize, 0usize);
            while vi < v.segs.len() || ai < a.segs.len() {
                let pic = ai >= a.segs.len() || (vi < v.segs.len() && v.segs[vi].t0 <= a.segs[ai].t0);
                let (st, i) = if pic { (&v, vi) } else { (&a, ai) };
                let j = group_end(&st.segs, i);
                order.push((pic, i, j));
                if pic {
                    vi = j;
                } else {
                    ai = j;
                }
            }
        }
        // PREFETCH pieces are fetched ahead on their own green threads (and connections) while the
        // one at the front is written: one 2 MiB request at a time left a 423 MB video at under
        // 0.25 MB/s. Memory stays at PREFETCH x 2 MiB; a piece that failed is fetched again in
        // place by `fetch` (which also renews an expired address).
        type Slot = alloc::rc::Rc<core::cell::RefCell<Option<Result<Vec<u8>, String>>>>;
        let start = |st: &Stream, i: usize, j: usize| -> Slot {
            let slot: Slot = alloc::rc::Rc::new(core::cell::RefCell::new(None));
            let s2 = slot.clone();
            let url = st.url.borrow().clone();
            let (a0, b0) = (st.segs[i].off, st.segs[j - 1].off + st.segs[j - 1].size - 1);
            let t = crate::coro::spawn("ytsave-fetch", false, Box::new(move || {
                let r = vp::get_range(&url, a0, b0, &move || !cancelled(id));
                *s2.borrow_mut() = Some(r);
            }));
            if t == 0 {
                *slot.borrow_mut() = Some(Err("no task".to_string()));
            }
            slot
        };
        let mut ahead: alloc::collections::VecDeque<Slot> = alloc::collections::VecDeque::new();
        let mut launched = 0usize;
        let (mut seq, mut done) = (0u32, 0u64);
        for n in 0..order.len() {
            if !keep() {
                return Err("cancelled".to_string());
            }
            while launched < order.len() && launched < n + PREFETCH {
                let (pic, i, j) = order[launched];
                ahead.push_back(start(if pic { &v } else { &a }, i, j));
                launched += 1;
            }
            let (pic, i, j) = order[n];
            let (st, track) = if pic { (&v, 1u32) } else { (&a, 2u32) };
            let slot = ahead.pop_front().ok_or("the fetch queue ran dry")?;
            let got = loop {
                if let Some(r) = slot.borrow_mut().take() {
                    break r;
                }
                if !keep() {
                    return Err("cancelled".to_string());
                }
                crate::coro::sleep_ms(5);
            };
            let mut bytes = match got {
                Ok(b) => b,
                Err(_) => fetch(st, i, j, &keep)?,
            };
            for k in i..j {
                let o = (st.segs[k].off - st.segs[i].off) as usize;
                let e = (o + st.segs[k].size as usize).min(bytes.len());
                let seg = &mut bytes[o..e];
                for (s, e) in patch_segment(seg, track, &mut seq) {
                    sink.write(&seg[s..e])?;
                }
                done += st.segs[k].size;
            }
            set(id, |jb| {
                jb.progress = done as f64 / total.max(1) as f64;
                jb.note = format!("{:.1} of {:.1} MB", done as f64 / 1048576.0, total as f64 / 1048576.0);
            });
            crate::coro::yield_now();
        }
        Ok(())
    })();
    match r {
        Ok(()) => sink.finish().map(|_| (shown, note)),
        Err(e) => {
            sink.abort();
            Err(e)
        },
    }
}

fn save_audio(id: u32, vid: &str, streams: &yt::Streams, path: &str, kind: Kind) -> Result<(String, String), String> {
    let keep = move || !cancelled(id);
    let af = streams.audio.clone().ok_or("YouTube offered no AAC sound stream for this video")?;
    log(&format!("{}: sound {}", if kind == Kind::Flac { "flac" } else { "wav" }, yt::short(&af)));
    set(id, |j| j.note = "opening the sound stream".to_string());
    let mut a = open_stream(af, vid, &keep).map_err(|e| format!("sound stream: {e}"))?;
    let mut dec = ling_aac::AacDecoder::new(&a.init.asc).map_err(|_| "the sound is in an AAC flavour LingOS does not decode".to_string())?;
    let rate = dec.sample_rate();
    let ch_in = dec.channels().max(1) as usize;
    let ch = ch_in.min(2);
    if rate == 0 {
        return Err("the sound stream has no sample rate".to_string());
    }
    // the exact length, from the segment index; the edit list's lead-in (encoder delay) is dropped
    let lead = rescale(a.init.shift.max(0) as u64, a.init.timescale, rate);
    let frames = rescale(a.dur.0, a.dur.1, rate).saturating_sub(lead);
    let est = match kind {
        Kind::Wav => 44 + frames * ch as u64 * 2,
        _ => 42 + frames * ch as u64 * 2 * 6 / 10,
    };
    let (mut sink, shown, note) = Sink::open(path, est)?;
    let total_bytes: u64 = a.segs.iter().map(|s| s.size).sum();
    let r = (|| -> Result<(), String> {
        let mut enc = FlacEncoder::new(rate, ch);
        match kind {
            Kind::Wav => sink.write(&wav_header(rate, ch as u16, frames))?,
            _ => sink.write(&enc.header(frames))?,
        }
        let (mut i, mut done, mut written, mut skip) = (0usize, 0u64, 0u64, lead);
        let mut f: Vec<f32> = Vec::new();
        let mut pcm: Vec<i16> = Vec::new();
        let mut out: Vec<u8> = Vec::new();
        while i < a.segs.len() {
            if !keep() {
                return Err("cancelled".to_string());
            }
            let j = group_end(&a.segs, i);
            let bytes = fetch(&a, i, j, &keep)?;
            for k in i..j {
                let seg = a.segs[k];
                let o = (seg.off - a.segs[i].off) as usize;
                let e = (o + seg.size as usize).min(bytes.len());
                let samples = mp4::parse_fragment(&mut a.init, &bytes[o..e], seg.off);
                f.clear();
                for (n, s) in samples.iter().enumerate() {
                    let _ = dec.decode(&s.data, &mut f);
                    if n % 96 == 95 {
                        crate::coro::yield_now();
                    }
                }
                pcm.clear();
                for fr in f.chunks_exact(ch_in) {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    if written >= frames {
                        break;
                    }
                    for c in 0..ch {
                        pcm.push((fr[c] * 32767.0).clamp(-32768.0, 32767.0) as i16);
                    }
                    written += 1;
                }
                done += seg.size;
                out.clear();
                match kind {
                    Kind::Wav => {
                        for s in &pcm {
                            out.extend_from_slice(&s.to_le_bytes());
                        }
                    },
                    _ => {
                        // a few frames at a time, the desktop's turn in between
                        for piece in pcm.chunks(crate::codec::flac_enc::BLOCK * ch * 16) {
                            out.extend_from_slice(&enc.push(piece));
                            crate::coro::yield_now();
                        }
                    },
                }
                sink.write(&out)?;
                set(id, |jb| {
                    jb.progress = done as f64 / total_bytes.max(1) as f64;
                    jb.note = format!("{:.0} of {:.0} s of sound", written as f64 / rate as f64, frames as f64 / rate as f64);
                });
                // decoding + encoding a segment is a few tens of ms: the desktop gets its turn between them
                crate::coro::yield_now();
            }
            i = j;
            set(id, |jb| {
                jb.progress = done as f64 / total_bytes.max(1) as f64;
                jb.note = format!("{:.0} of {:.0} s of sound", written as f64 / rate as f64, frames as f64 / rate as f64);
            });
            crate::coro::yield_now();
        }
        // the header promised `frames`: a stream a little short is padded with silence
        if written < frames {
            let pad = alloc::vec![0i16; ((frames - written) as usize) * ch];
            match kind {
                Kind::Wav => {
                    let b: Vec<u8> = pad.iter().flat_map(|s| s.to_le_bytes()).collect();
                    sink.write(&b)?;
                },
                _ => {
                    let b = enc.push(&pad);
                    sink.write(&b)?;
                },
            }
        }
        if kind != Kind::Wav {
            let b = enc.finish();
            sink.write(&b)?;
        }
        Ok(())
    })();
    match r {
        Ok(()) => sink.finish().map(|_| (shown, note)),
        Err(e) => {
            sink.abort();
            Err(e)
        },
    }
}

fn task(id: u32, url: String, path: String, kind: Kind) {
    let Some(vid) = yt::video_id(&url) else {
        return set(id, |j| {
            j.state = -1;
            j.note = "this is not a YouTube address".to_string();
        });
    };
    let streams = match vp::resolve(&vid, &|m| set(id, |j| j.note = m.to_string())) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("{vid}: {e}"));
            return set(id, |j| {
                j.state = -1;
                j.note = format!("YouTube: {e}");
            });
        },
    };
    if streams.live {
        return set(id, |j| {
            j.state = -1;
            j.note = "live streams cannot be saved".to_string();
        });
    }
    let t0 = crate::arch::timer::now_ms();
    let r = match kind {
        Kind::Mp4 => save_mp4(id, &vid, &streams, &path),
        _ => save_audio(id, &vid, &streams, &path, kind),
    };
    let secs = crate::arch::timer::now_ms().saturating_sub(t0) as f64 / 1000.0;
    match r {
        Ok((shown, note)) => {
            log(&format!("{vid}: saved {shown} in {secs:.1} s"));
            set(id, |j| {
                j.state = 1;
                j.progress = 1.0;
                j.path = shown.clone();
                j.note = format!("Saved {shown}{note}");
            });
        },
        Err(e) => {
            log(&format!("{vid}: {e}"));
            set(id, |j| {
                j.state = -1;
                j.note = e;
            });
        },
    }
}

/// Start saving the video at `url` to `path` as `kind` ("mp4", "flac", "wav"); the job's id.
pub fn start(url: &str, path: &str, kind: &str) -> f64 {
    let kind = match kind.trim().to_ascii_lowercase().as_str() {
        "flac" => Kind::Flac,
        "wav" => Kind::Wav,
        _ => Kind::Mp4,
    };
    let id = unsafe {
        let id = NEXT;
        NEXT += 1;
        id
    };
    // finished jobs are kept a while (the page reads their outcome), the oldest go first
    while jobs().len() >= 16 {
        match jobs().iter().position(|j| j.state != 0) {
            Some(i) => {
                jobs().remove(i);
            },
            None => break,
        }
    }
    jobs().push(Job { id, state: 0, progress: 0.0, note: "starting".to_string(), path: path.to_string(), cancel: false });
    let (u, p) = (url.to_string(), path.to_string());
    let t = crate::coro::spawn("ytsave", false, Box::new(move || task(id, u, p, kind)));
    if t == 0 {
        set(id, |j| {
            j.state = -1;
            j.note = "no memory for the download's task".to_string();
        });
    }
    id as f64
}

/// The `os_video_save*` builtins.
pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    use crate::lingrt::{mk_str, num, to_f64, to_string, UNIT};
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).unwrap_or(-1.0) as u32;
    Some(match name {
        b"os_video_save" => num(start(&s(0), &s(1), &s(2))),
        b"os_video_save_state" => num(job(n(0)).map(|j| j.state as f64).unwrap_or(-1.0)),
        b"os_video_save_progress" => num(job(n(0)).map(|j| j.progress).unwrap_or(0.0)),
        b"os_video_save_note" => mk_str(&job(n(0)).map(|j| j.note.clone()).unwrap_or_else(|| "no such download".to_string())),
        b"os_video_save_path" => mk_str(&job(n(0)).map(|j| j.path.clone()).unwrap_or_default()),
        b"os_video_save_cancel" => {
            set(n(0), |j| j.cancel = true);
            UNIT
        },
        _ => return None,
    })
}

/// `ytsave <url> <path> [mp4|flac|wav]` (SSH / Terminal): the same download, waited for -- the test's hook.
/// `ytsave <url> <path> [mp4|flac|wav] bg` returns at once (the job runs on; `ytsave status <id>`
/// tells how far it is) -- the SSH server serves one session at a time, so a long save held it.
pub fn command(arg: &str) -> String {
    let mut w = arg.split_whitespace();
    if let (Some("status"), Some(n)) = (w.clone().next(), w.clone().nth(1)) {
        let id = n.parse::<u32>().unwrap_or(0);
        return match job(id) {
            Some(j) => format!("ytsave {id}: state {} progress {:.1}% {}\n", j.state, j.progress * 100.0, j.note),
            None => format!("ytsave {id}: no such job\n"),
        };
    }
    let (Some(url), Some(path)) = (w.next(), w.next()) else {
        return String::from("usage: ytsave <youtube url> <path> [mp4|flac|wav] [bg] | ytsave status <id>\n");
    };
    let kind = w.next().unwrap_or("mp4");
    let id = start(url, path, kind) as u32;
    if w.next() == Some("bg") {
        return format!("ytsave: job {id} started\n");
    }
    let t0 = crate::arch::timer::now_ms();
    loop {
        crate::coro::sleep_ms(250);
        let Some(j) = job(id) else { return String::from("the job vanished\n") };
        if j.state != 0 {
            let ms = crate::arch::timer::now_ms().saturating_sub(t0);
            return format!("ytsave: state {} in {} ms: {}\n", j.state, ms, j.note);
        }
        if crate::arch::timer::now_ms().saturating_sub(t0) > 3 * 60 * 60 * 1000 {
            j.cancel = true;
            return String::from("ytsave: gave up after 3 hours\n");
        }
    }
}
