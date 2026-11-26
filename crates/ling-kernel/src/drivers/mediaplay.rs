//! The desktop's media player for local files (`os_media_*`, the Files app's
//! Now Playing window): MKV / WebM, MP4 / MOV (regular and fragmented, as
//! screenrec writes them), AVI (screenrec's MJPEG), animated GIF and image
//! sequences (LingAnime's PNG frames).
//!
//! Same shape as Horizon's YouTube player (`videoplay.rs`):
//! * a **reading** green thread opens the file (`mediafile.rs`: in pieces,
//!   never whole) and keeps packets coming a few seconds ahead of the clock;
//!   subtitle packets go straight into their track's event list;
//! * a **decoding** green thread hands batches to a worker core
//!   (`smp::bg_start`, allocations in `mm::aparena`): H.264 (`ling-h264`),
//!   MPEG-1/2 video with deinterlacing (`ling-mpeg2`), Motion JPEG / PNG
//!   pictures, AAC (`ling-aac`), MPEG audio layers I-III (`ling-mpa`),
//!   Vorbis, FLAC and PCM;
//! * the sound streams into an sfx sample on the music voice, whose position
//!   is the clock the pictures follow (a wall clock without sound); pictures
//!   that fall behind are dropped;
//! * `os_media_draw` scales the current picture (bilinear, YUV straight from
//!   the decoder for H.264) into the desktop's buffer across the cores, then
//!   draws the subtitles (`subs.rs`) over it.

use crate::drivers::mediafile::{self, ACodec, Codec, Demux, Meta, Pkt, SCodec, TrackDesc, VCodec};
use crate::drivers::{image, sfx, subs};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

fn now_ms() -> u64 {
    crate::arch::timer::now_ms()
}

fn log(m: &str) {
    crate::console_write(format!("[media] {m}\n").as_bytes());
}

/// Seconds of packets read ahead of the clock.
const AHEAD_S: f64 = 4.0;
/// Most compressed bytes kept waiting (a badly interleaved file must not fill memory).
const MAX_QUEUED: usize = 96 << 20;
/// Sound kept behind the clock before it is trimmed (seconds).
const KEEP_BEHIND_S: f64 = 3.0;
/// The worker arena (H.264 1080p keeps ~16 pictures of 3 MB in its DPB, plus our output).
const ARENA: usize = 256 << 20;

// ── pictures ─────────────────────────────────────────────────────────────────

/// A decoded picture: RGB, or YUV 4:2:0 straight from the H.264 decoder (converted while drawing).
pub enum Pic {
    Rgb(image::Image),
    Yuv { w: u32, h: u32, y: Vec<u8>, u: Vec<u8>, v: Vec<u8>, ys: usize, cs: usize, bt709: bool, full: bool },
}

impl Pic {
    fn size(&self) -> (u32, u32) {
        match self {
            Pic::Rgb(i) => (i.w, i.h),
            Pic::Yuv { w, h, .. } => (*w, *h),
        }
    }
}

struct VS {
    pts: f64,
    key: bool,
    data: Vec<u8>,
}

// ── decoding (on a worker core when there is one) ────────────────────────────

enum VDec {
    None,
    H264(ling_h264::Decoder),
    Mpeg2(ling_mpeg2::Decoder),
    /// every packet is a whole picture (MJPEG, PNG, raw)
    Pictures,
}

enum ADec {
    None,
    Aac(ling_aac::AacDecoder),
    Mpa(ling_mpa::Decoder),
    Vorbis(Box<(crate::codec::vorbis::header::IdentHeader, crate::codec::vorbis::header::SetupHeader, crate::codec::vorbis::audio::PreviousWindowRight)>),
    Flac,
    Pcm,
}

/// The decoders and one batch of work. On a worker core it only reads what the
/// boot core put in (`vin`, `ain`, the codec setups) and makes its results
/// (`frames`, `pcm`) as new allocations.
struct Work {
    gen: u32,
    reset_v: bool,
    reset_a: bool,
    vcodec: Option<VCodec>,
    acodec: Option<ACodec>,
    /// MPEG-2 frames are deinterlaced
    deint: bool,
    vin: Vec<VS>,
    ain: Vec<(f64, Vec<u8>)>,
    skip_before: f64,
    flush: bool,
    vdec: VDec,
    adec: ADec,
    pending: Vec<f64>,
    frames: Vec<(f64, Pic)>,
    /// MPEG-2: the display aspect the stream says
    dar: f64,
    pcm: Vec<i16>,
    pcm_t0: f64,
    rate: u32,
    channels: u16,
    verrs: u32,
    aerrs: u32,
    fatal: Option<&'static str>,
    us: u64,
}

impl Work {
    fn new() -> Work {
        Work {
            gen: 0,
            reset_v: true,
            reset_a: true,
            vcodec: None,
            acodec: None,
            deint: true,
            vin: Vec::new(),
            ain: Vec::new(),
            skip_before: 0.0,
            flush: false,
            vdec: VDec::None,
            adec: ADec::None,
            pending: Vec::new(),
            frames: Vec::new(),
            dar: 0.0,
            pcm: Vec::new(),
            pcm_t0: 0.0,
            rate: 0,
            channels: 0,
            verrs: 0,
            aerrs: 0,
            fatal: None,
            us: 0,
        }
    }
}

/// Interleaved samples (any channel layout) -> stereo or mono i16. `order` names the
/// source's channel layout for downmixing more than two: 0 = L R C LFE Ls Rs (FLAC, PCM,
/// MPEG), 1 = C L R Ls Rs LFE (AAC), 2 = L C R Ls Rs LFE (Vorbis).
fn to_out(f: &[f32], ch: usize, order: u8, out: &mut Vec<i16>) -> u16 {
    let q = |v: f32| (v * 32767.0).clamp(-32767.0, 32767.0) as i16;
    if ch == 0 {
        return 1;
    }
    if ch == 1 {
        out.extend(f.iter().map(|&v| q(v)));
        return 1;
    }
    for fr in f.chunks_exact(ch) {
        let (l, r) = if ch == 2 {
            (fr[0], fr[1])
        } else {
            let (li, ri, ci, sl, sr) = match order {
                1 => (1, 2, 0, 3, 4),
                2 => (0, 2, 1, 3, 4),
                _ => (0, 1, 2, 4, 5),
            };
            let g = |i: usize| fr.get(i).copied().unwrap_or(0.0);
            let c = if ch >= 3 { g(ci) } else { 0.0 };
            (0.5 * g(li) + 0.35 * c + 0.35 * g(sl), 0.5 * g(ri) + 0.35 * c + 0.35 * g(sr))
        };
        out.push(q(l));
        out.push(q(r));
    }
    2
}

fn take_h264(w: &mut Work, f: ling_h264::Frame) {
    let pts = if w.pending.is_empty() { f64::NAN } else { w.pending.remove(0) };
    if pts.is_nan() || pts + 0.001 < w.skip_before || f.width == 0 || f.height == 0 {
        return;
    }
    let ling_h264::Frame { width, height, y, u, v, y_stride, c_stride, bt709, full_range, .. } = f;
    w.frames.push((pts, Pic::Yuv { w: width, h: height, y, u, v, ys: y_stride, cs: c_stride, bt709, full: full_range }));
}

fn take_mpeg2(w: &mut Work, f: ling_mpeg2::Frame) {
    let pts = f.pts as f64 / 1e6;
    if pts + 0.001 < w.skip_before || f.width == 0 || f.height == 0 {
        return;
    }
    let (fw, fh) = (f.width as usize, f.height as usize);
    let mut px = alloc::vec![0u32; fw * fh];
    ling_mpeg2::frame_to_rgba(&f, &mut px, w.deint && !f.progressive);
    if f.dar.0 > 0 && f.dar.1 > 0 {
        w.dar = f.dar.0 as f64 / f.dar.1 as f64;
    }
    w.frames.push((pts, Pic::Rgb(image::Image { w: fw as u32, h: fh as u32, px })));
}

fn decode_picture(data: &[u8], raw: bool) -> Option<image::Image> {
    if raw {
        if data.len() < 8 {
            return None;
        }
        let w = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let h = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let n = w as usize * h as usize;
        if data.len() < 8 + n * 4 {
            return None;
        }
        let px = data[8..8 + n * 4].chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) & 0xFF_FFFF).collect();
        return Some(image::Image { w, h, px });
    }
    if data.len() > 2 && data[0] == 0xFF && data[1] == 0xD8 {
        return image::decode_jpeg(data, 8192);
    }
    image::decode_png_rgba(data).map(|mut i| {
        // alpha over black
        for p in i.px.iter_mut() {
            let a = *p >> 24;
            if a < 255 {
                let m = |c: u32| ((c & 255) * a / 255) & 255;
                *p = (m(*p >> 16) << 16) | (m(*p >> 8) << 8) | m(*p);
            } else {
                *p &= 0xFF_FFFF;
            }
        }
        i
    }).or_else(|| image::decode_web(data))
}

/// Decode the batch (runs on a worker core, or here).
fn work_run(w: &mut Work) {
    let t0 = crate::arch::timer::now_us();
    w.fatal = None;
    if w.reset_a {
        w.reset_a = false;
        w.adec = match &w.acodec {
            None => ADec::None,
            Some(ACodec::Aac(asc)) => match ling_aac::AacDecoder::new(asc) {
                Ok(d) => ADec::Aac(d),
                Err(_) => {
                    w.fatal = Some("the sound is in an AAC flavour LingOS does not decode");
                    ADec::None
                },
            },
            Some(ACodec::Mpa) => ADec::Mpa(ling_mpa::Decoder::new()),
            Some(ACodec::Vorbis(h)) => {
                use crate::codec::vorbis::{audio, header};
                let ident = header::read_header_ident(&h[0]);
                match ident {
                    Ok(id) => match header::read_header_setup(&h[2], id.audio_channels, (id.blocksize_0, id.blocksize_1)) {
                        Ok(setup) => ADec::Vorbis(Box::new((id, setup, audio::PreviousWindowRight::new()))),
                        Err(_) => ADec::None,
                    },
                    Err(_) => ADec::None,
                }
            },
            Some(ACodec::Flac(_)) => ADec::Flac,
            Some(ACodec::Pcm { .. }) => ADec::Pcm,
        };
    }
    if w.reset_v {
        w.reset_v = false;
        w.pending = Vec::new();
        w.vdec = match &w.vcodec {
            None => VDec::None,
            Some(VCodec::H264(avcc)) => match ling_h264::Decoder::from_avcc(avcc) {
                Ok(d) => VDec::H264(d),
                Err(_) => {
                    w.fatal = Some("the picture is in an H.264 profile LingOS does not decode");
                    VDec::None
                },
            },
            Some(VCodec::Mpeg2(seq)) => match if seq.is_empty() { Ok(ling_mpeg2::Decoder::new()) } else { ling_mpeg2::Decoder::with_header(seq) } {
                Ok(d) => VDec::Mpeg2(d),
                Err(_) => {
                    w.fatal = Some("the MPEG-2 stream uses something LingOS does not decode");
                    VDec::None
                },
            },
            Some(_) => VDec::Pictures,
        };
    }
    // sound first: it is the clock
    if !w.ain.is_empty() {
        w.pcm_t0 = w.ain[0].0;
        let mut pcm: Vec<i16> = Vec::new();
        let mut chs = w.channels;
        let mut rate = w.rate;
        match &mut w.adec {
            ADec::None => {},
            ADec::Aac(dec) => {
                let mut f: Vec<f32> = Vec::new();
                for k in 0..w.ain.len() {
                    if dec.decode(&w.ain[k].1, &mut f).is_err() {
                        w.aerrs += 1;
                    }
                }
                rate = dec.sample_rate();
                chs = to_out(&f, dec.channels().max(1) as usize, 1, &mut pcm);
            },
            ADec::Mpa(dec) => {
                let mut f: Vec<f32> = Vec::new();
                for k in 0..w.ain.len() {
                    if dec.decode(&w.ain[k].1, &mut f).is_err() {
                        w.aerrs += 1;
                    }
                }
                rate = dec.sample_rate();
                chs = to_out(&f, dec.channels().max(1) as usize, 0, &mut pcm);
            },
            ADec::Vorbis(st) => {
                use crate::codec::vorbis::{audio, samples};
                let (ident, setup, pwr) = &mut **st;
                let ch = ident.audio_channels as usize;
                let mut f: Vec<f32> = Vec::new();
                for k in 0..w.ain.len() {
                    match audio::read_audio_packet_generic::<samples::InterleavedSamples<i16>>(ident, setup, &w.ain[k].1, pwr) {
                        Ok(s) => f.extend(s.samples.iter().map(|&v| v as f32 / 32768.0)),
                        Err(_) => w.aerrs += 1,
                    }
                }
                rate = ident.audio_sample_rate;
                chs = to_out(&f, ch, 2, &mut pcm);
            },
            ADec::Flac => {
                let head = match &w.acodec {
                    Some(ACodec::Flac(h)) => h.as_slice(),
                    _ => &[],
                };
                let mut f: Vec<f32> = Vec::new();
                let mut ch = 2usize;
                let mut buf: Vec<u8> = Vec::new();
                for k in 0..w.ain.len() {
                    buf.clear();
                    if !head.starts_with(b"fLaC") {
                        buf.extend_from_slice(b"fLaC");
                    }
                    buf.extend_from_slice(head);
                    buf.extend_from_slice(&w.ain[k].1);
                    match crate::codec::flac::decode(&buf) {
                        Some((r, c, s)) => {
                            rate = r;
                            ch = c.max(1) as usize;
                            f.extend(s.iter().map(|&v| v as f32 / 32768.0));
                        },
                        None => w.aerrs += 1,
                    }
                }
                chs = to_out(&f, ch, 0, &mut pcm);
            },
            ADec::Pcm => {
                if let Some(ACodec::Pcm { bits, float, be, ch, rate: r }) = &w.acodec {
                    let (bits, float, be, ch) = (*bits as usize, *float, *be, (*ch).max(1) as usize);
                    let bps = bits.div_ceil(8).max(1);
                    let mut f: Vec<f32> = Vec::new();
                    for k in 0..w.ain.len() {
                        for s in w.ain[k].1.chunks_exact(bps) {
                            let mut b = [0u8; 8];
                            if be {
                                for (i, x) in s.iter().rev().enumerate() {
                                    b[i] = *x;
                                }
                            } else {
                                b[..bps].copy_from_slice(s);
                            }
                            let v = if float && bps == 4 {
                                f32::from_le_bytes([b[0], b[1], b[2], b[3]])
                            } else if float && bps == 8 {
                                f64::from_le_bytes(b) as f32
                            } else if bps == 1 {
                                (b[0] as f32 - 128.0) / 128.0
                            } else {
                                let raw = u64::from_le_bytes(b) << (64 - 8 * bps);
                                (raw as i64 >> (64 - 8 * bps)) as f32 / (1u64 << (8 * bps - 1)) as f32
                            };
                            f.push(v);
                        }
                    }
                    rate = *r;
                    chs = to_out(&f, ch, 0, &mut pcm);
                }
            },
        }
        w.rate = rate;
        w.channels = chs;
        w.pcm = pcm;
    }
    // pictures
    let raw = matches!(w.vcodec, Some(VCodec::Raw));
    for k in 0..w.vin.len() {
        let pts = w.vin[k].pts;
        match &mut w.vdec {
            VDec::None => {},
            VDec::H264(d) => {
                let at = w.pending.partition_point(|&p| p < pts);
                w.pending.insert(at, pts);
                if d.decode_sample(&w.vin[k].data).is_err() {
                    w.verrs += 1;
                    if let Some(i) = w.pending.iter().position(|&p| p == pts) {
                        w.pending.remove(i);
                    }
                }
                while let Some(f) = match &mut w.vdec {
                    VDec::H264(d) => d.next_frame(),
                    _ => None,
                } {
                    take_h264(w, f);
                }
            },
            VDec::Mpeg2(d) => {
                if d.decode(&w.vin[k].data, (pts * 1e6) as i64).is_err() {
                    w.verrs += 1;
                }
                while let Some(f) = match &mut w.vdec {
                    VDec::Mpeg2(d) => d.next_frame(),
                    _ => None,
                } {
                    take_mpeg2(w, f);
                }
            },
            VDec::Pictures => {
                if pts + 0.001 < w.skip_before {
                    continue;
                }
                match decode_picture(&w.vin[k].data, raw) {
                    Some(img) => w.frames.push((pts, Pic::Rgb(img))),
                    None => w.verrs += 1,
                }
            },
        }
    }
    if w.flush {
        w.flush = false;
        match &mut w.vdec {
            VDec::H264(d) => d.flush(),
            VDec::Mpeg2(d) => d.flush(),
            _ => {},
        }
        loop {
            match &mut w.vdec {
                VDec::H264(d) => match d.next_frame() {
                    Some(f) => take_h264(w, f),
                    None => break,
                },
                VDec::Mpeg2(d) => match d.next_frame() {
                    Some(f) => take_mpeg2(w, f),
                    None => break,
                },
                _ => break,
            }
        }
    }
    w.us = crate::arch::timer::now_us().saturating_sub(t0);
}

unsafe fn work_trampoline(ctx: usize, _i: usize) {
    work_run(&mut *(ctx as *mut Work));
}

// ── the players ──────────────────────────────────────────────────────────────

struct SubTrack {
    desc: TrackDesc,
    subs: subs::Subs,
}

struct Player {
    id: u32,
    path: String,
    /// -1 error, 0 loading, 1 ready, 2 ended
    state: i8,
    err: String,
    note: String,
    closed: bool,
    meta: Option<Meta>,
    dur: f64,
    /// the chosen tracks (ids), None = none
    vtrack: Option<u64>,
    atrack: Option<u64>,
    /// index into `subs`, None = off
    strack: Option<usize>,
    subs: Vec<SubTrack>,
    /// the reader must pick the tracks again (an audio track was switched)
    retrack: bool,
    dar: f64,
    vq: VecDeque<VS>,
    aq: VecDeque<(f64, Vec<u8>)>,
    queued: usize,
    v_last: f64,
    a_last: f64,
    eof: bool,
    frames: VecDeque<(f64, Pic)>,
    cur: Option<Pic>,
    shown: bool,
    /// sound: the stream, the media time of its first frame, its rate
    stream: Option<usize>,
    stream_t0: f64,
    rate: u32,
    voice_on: bool,
    no_voice: bool,
    flushed: bool,
    playing: bool,
    t: f64,
    t_ms: u64,
    vol: f32,
    gen: u32,
    seek: Option<f64>,
    /// the seek target: sound and pictures before it are dropped
    seek_to: f64,
    reset: bool,
    fetch_task: u64,
    decode_task: u64,
    work: *mut Work,
    in_flight: bool,
    use_worker: bool,
    dec_us: u64,
    dec_frames: u64,
    dropped: u64,
    shown_frames: u64,
    /// which picture goes out when, and how even that is (avpace.rs)
    pace: crate::drivers::avpace::Pace,
}

static mut PLAYERS: Vec<Player> = Vec::new();
static mut NEXT_PLAYER: u32 = 1;

fn players() -> &'static mut Vec<Player> {
    unsafe { &mut *(&raw mut PLAYERS) }
}

fn player(id: f64) -> Option<&'static mut Player> {
    let id = id as u32;
    players().iter_mut().find(|p| p.id == id && !p.closed)
}

fn by_id(id: u32) -> Option<&'static mut Player> {
    players().iter_mut().find(|p| p.id == id)
}

fn live(pid: u32) -> Option<&'static mut Player> {
    by_id(pid).filter(|p| !p.closed && p.state >= 0)
}

fn fail(pid: u32, msg: String) {
    log(&msg);
    if let Some(p) = by_id(pid) {
        p.state = -1;
        p.err = msg;
    }
}

fn track<'a>(p: &'a Player, id: Option<u64>) -> Option<&'a TrackDesc> {
    let id = id?;
    p.meta.as_ref()?.tracks.iter().find(|t| t.id == id)
}

/// Pick the tracks to play: the first decodable video, the default (else first) decodable
/// audio, and the default (or forced) subtitles.
fn choose_tracks(p: &mut Player) {
    let Some(m) = p.meta.as_ref() else { return };
    let ok = |t: &&TrackDesc| !matches!(t.codec, Codec::No(_));
    p.vtrack = m.tracks.iter().filter(|t| t.kind == 0).find(ok).map(|t| t.id);
    let audio: Vec<&TrackDesc> = m.tracks.iter().filter(|t| t.kind == 1).collect();
    p.atrack = audio.iter().find(|t| t.default && ok(t)).or_else(|| audio.iter().find(|t| ok(t))).map(|t| t.id);
    let mut notes: Vec<String> = Vec::new();
    if p.vtrack.is_none() {
        if let Some(Codec::No(why)) = m.tracks.iter().find(|t| t.kind == 0).map(|t| &t.codec) {
            notes.push(why.clone());
        }
    }
    if p.atrack.is_none() {
        if let Some(Codec::No(why)) = audio.first().map(|t| &t.codec) {
            notes.push(format!("{why} - playing without sound"));
        }
    }
    p.note = notes.join("; ");
    // subtitles: a forced or default track; else, when the sound is in another language than a
    // subtitle track (Japanese audio, English subtitles), that track; else off (S turns them on)
    let alang = p.atrack.and_then(|a| m.tracks.iter().find(|t| t.id == a)).map(|t| t.lang.clone()).unwrap_or_default();
    let known = |l: &str| !l.is_empty() && l != "und";
    p.strack = p
        .subs
        .iter()
        .position(|s| s.desc.forced)
        .or_else(|| p.subs.iter().position(|s| s.desc.default && s.desc.id < 1_000_000))
        .or_else(|| if known(&alang) { p.subs.iter().position(|s| known(&s.desc.lang) && s.desc.lang != alang) } else { None })
        .or_else(|| p.subs.iter().position(|s| s.desc.id >= 1_000_000));
}

/// What the reader should hand out.
fn wanted(p: &Player) -> Vec<u64> {
    let mut w: Vec<u64> = p.vtrack.into_iter().chain(p.atrack).collect();
    for s in &p.subs {
        if s.desc.id < 1_000_000 {
            w.push(s.desc.id);
        }
    }
    w
}

fn set_work_codecs(p: &mut Player) {
    let v = track(p, p.vtrack).and_then(|t| match &t.codec {
        Codec::V(c) => Some(c.clone()),
        _ => None,
    });
    let a = track(p, p.atrack).and_then(|t| match &t.codec {
        Codec::A(c) => Some(c.clone()),
        _ => None,
    });
    unsafe {
        let w = &mut *p.work;
        w.vcodec = v;
        w.acodec = a;
    }
}

/// The reading task: open the file, then keep packets coming ahead of the clock.
fn fetch_task(pid: u32) {
    let Some(p) = by_id(pid) else { return };
    let path = p.path.clone();
    p.note = "opening".to_string();
    let Some(mut src) = mediafile::open_source(&path) else {
        return fail(pid, format!("{} could not be opened", crate::vfs::basename(&path)));
    };
    let (mut dmx, meta) = match Demux::open(&mut src, &path) {
        Ok(x) => x,
        Err(e) => return fail(pid, e),
    };
    let Some(p) = live(pid) else { return };
    log(&format!(
        "{}: {} {:.0} s, {}",
        crate::vfs::basename(&path),
        meta.format,
        meta.duration,
        meta.tracks.iter().map(|t| format!("#{} {} {}", t.id, t.cname, t.lang)).collect::<Vec<_>>().join(", ")
    ));
    // subtitle tracks: the file's, then files next to it
    for t in meta.tracks.iter().filter(|t| t.kind == 2) {
        let s = match &t.codec {
            Codec::S(SCodec::Srt) => subs::Subs::new(false),
            Codec::S(SCodec::Ass(h)) => {
                let mut s = subs::Subs::new(true);
                s.load_ass(h);
                s
            },
            _ => continue,
        };
        p.subs.push(SubTrack { desc: t.clone(), subs: s });
    }
    for (k, (f, mid)) in mediafile::external_subs(&path).into_iter().enumerate() {
        let Some(b) = crate::vfs::read(&f) else { continue };
        let text = String::from_utf8_lossy(&b).into_owned();
        let l = f.to_ascii_lowercase();
        let ass = l.ends_with(".ass") || l.ends_with(".ssa");
        let mut s = subs::Subs::new(ass);
        if ass {
            s.load_ass(&text);
        } else {
            s.load_srt(&text.replace("WEBVTT", ""));
        }
        let mut d = TrackDesc {
            id: 1_000_000 + k as u64,
            kind: 2,
            codec: Codec::S(SCodec::Srt),
            cname: if ass { "ASS".to_string() } else { "SRT".to_string() },
            lang: mid.clone(),
            name: crate::vfs::basename(&f),
            default: true,
            forced: false,
            zlib: false,
            w: 0,
            h: 0,
            dar: 0.0,
            rate: 0,
            channels: 0,
            fps: 0.0,
            interlaced: false,
        };
        d.name = format!("file {}", crate::vfs::basename(&f));
        p.subs.push(SubTrack { desc: d, subs: s });
    }
    p.dur = meta.duration;
    p.dar = meta.tracks.iter().find(|t| t.kind == 0).map(|t| t.dar).unwrap_or(0.0);
    p.meta = Some(meta);
    choose_tracks(p);
    if p.vtrack.is_none() && p.atrack.is_none() {
        let why = if p.note.is_empty() { "nothing in this file can be played".to_string() } else { p.note.clone() };
        return fail(pid, why);
    }
    set_work_codecs(p);
    dmx.set_wanted(&wanted(p));
    let mut sub_ids: Vec<(u64, bool)> = p.subs.iter().map(|s| (s.desc.id, s.desc.zlib)).collect();
    loop {
        let Some(p) = live(pid) else { return };
        if p.retrack {
            p.retrack = false;
            set_work_codecs(p);
            dmx.set_wanted(&wanted(p));
            sub_ids = p.subs.iter().map(|s| (s.desc.id, s.desc.zlib)).collect();
            if p.seek.is_none() {
                p.seek = Some(p.t);
            }
        }
        if let Some(t) = p.seek.take() {
            let gen = p.gen;
            let start = dmx.seek(&mut src, t);
            let Some(p) = live(pid) else { return };
            if p.gen != gen {
                continue;
            }
            p.eof = false;
            p.flushed = false;
            p.v_last = start;
            p.a_last = start;
        }
        let t = clock(p);
        let has_a = p.atrack.is_some();
        let has_v = p.vtrack.is_some();
        let v_ok = !has_v || p.v_last > t + AHEAD_S || p.vq.len() > 400;
        let a_ok = !has_a || p.a_last > t + AHEAD_S;
        if p.eof || (v_ok && a_ok) || p.queued > MAX_QUEUED {
            crate::coro::sleep_ms(15);
            continue;
        }
        let gen = p.gen;
        // read in short turns: this task runs on the boot core between frames, and a batch of
        // 48 packets (~1.5 MB of 1080p, read + demuxed) took 50-100 ms there -- the desktop
        // missed its present slot on about half the frames while a video played (prof:
        // "isolates + tasks"). Yield after ~2 ms; the read-ahead (AHEAD_S) keeps it fed.
        let turn0 = crate::arch::timer::now_us();
        for _ in 0..48 {
            if crate::arch::timer::now_us().saturating_sub(turn0) > 2_000 {
                break;
            }
            let pk = dmx.next(&mut src);
            let Some(p) = live(pid) else { return };
            if p.gen != gen || p.seek.is_some() || p.retrack {
                break; // a seek came in while reading: its packets are for the old place
            }
            let Some(pk) = pk else {
                p.eof = true;
                break;
            };
            route(p, pk, &sub_ids);
        }
        crate::coro::yield_now();
    }
}

/// A packet to its queue (subtitles straight into their track).
fn route(p: &mut Player, pk: Pkt, sub_ids: &[(u64, bool)]) {
    if Some(pk.track) == p.vtrack {
        p.v_last = p.v_last.max(pk.pts);
        p.queued += pk.data.len();
        p.vq.push_back(VS { pts: pk.pts, key: pk.key, data: pk.data });
    } else if Some(pk.track) == p.atrack {
        p.a_last = p.a_last.max(pk.pts);
        p.queued += pk.data.len();
        p.aq.push_back((pk.pts, pk.data));
    } else if let Some(&(_, zlib)) = sub_ids.iter().find(|s| s.0 == pk.track) {
        let data = if zlib { miniz_oxide::inflate::decompress_to_vec_zlib(&pk.data).unwrap_or_default() } else { pk.data };
        if let Some(s) = p.subs.iter_mut().find(|s| s.desc.id == pk.track) {
            s.subs.add_packet(pk.pts, pk.dur, &data);
        }
    }
}

/// Frames kept decoded ahead (fewer at big sizes).
fn frames_ready(p: &Player) -> usize {
    let px = track(p, p.vtrack).map(|t| t.w as usize * t.h as usize).unwrap_or(0);
    if px > 1280 * 720 {
        6
    } else {
        10
    }
}

/// Hand the decoders their next batch, and take what they made.
fn decode_task(pid: u32) {
    loop {
        let Some(p) = by_id(pid) else { return };
        if p.closed && !p.in_flight {
            if p.fetch_task == 0 || !crate::coro::alive(p.fetch_task) {
                return finish(pid);
            }
            crate::coro::sleep_ms(20);
            continue;
        }
        if p.state < 0 && !p.in_flight {
            return;
        }
        let w = unsafe { &mut *p.work };
        if p.in_flight {
            #[cfg(target_arch = "x86_64")]
            let done = crate::arch::smp::bg_finish();
            #[cfg(not(target_arch = "x86_64"))]
            let done = true;
            if !done {
                crate::coro::sleep_ms(1);
                continue;
            }
            p.in_flight = false;
            collect(p);
            continue;
        }
        if p.meta.is_none() {
            crate::coro::sleep_ms(30);
            continue;
        }
        if p.reset {
            p.reset = false;
            w.reset_v = true;
            w.reset_a = true;
            w.skip_before = p.seek_to;
        }
        w.gen = p.gen;
        w.vin.clear();
        w.ain.clear();
        let t = clock(p);
        // the decoder fell well behind the clock: go on from the next key frame
        if p.playing && p.vq.front().is_some_and(|s| s.pts < t - 1.5) {
            if let Some(k) = p.vq.iter().position(|s| s.key && s.pts > t - 0.5) {
                for s in p.vq.drain(..k) {
                    p.queued = p.queued.saturating_sub(s.data.len());
                    p.dropped += 1;
                }
                p.frames.retain(|f| f.0 >= t - 0.5);
                // the reference pictures are gone: start the decoder over at the key frame
                w.reset_v = true;
            }
        }
        if p.eof && p.vq.is_empty() && !p.flushed {
            p.flushed = true;
            w.flush = true;
        }
        let room = frames_ready(p).saturating_sub(p.frames.len());
        let per_batch = if p.use_worker { room + 2 } else { 1 };
        if room > 0 {
            for _ in 0..per_batch {
                match p.vq.pop_front() {
                    Some(s) => {
                        p.queued = p.queued.saturating_sub(s.data.len());
                        w.vin.push(s)
                    },
                    None => break,
                }
            }
        }
        let have_to = match p.stream {
            Some(s) if p.rate > 0 => p.stream_t0 + sfx::stream_end(s) as f64 / p.rate as f64,
            _ => t,
        };
        if have_to < t + 3.0 {
            let n = if p.use_worker { 128 } else { 16 };
            for _ in 0..n {
                match p.aq.pop_front() {
                    Some(a) => {
                        p.queued = p.queued.saturating_sub(a.1.len());
                        w.ain.push(a)
                    },
                    None => break,
                }
            }
        }
        if w.vin.is_empty() && w.ain.is_empty() && !w.reset_v && !w.reset_a && !w.flush {
            maybe_ready(p);
            crate::coro::sleep_ms(4);
            continue;
        }
        if p.use_worker {
            #[cfg(target_arch = "x86_64")]
            {
                if crate::arch::smp::bg_start(work_trampoline, p.work as usize) {
                    p.in_flight = true;
                } else {
                    for s in w.vin.drain(..).rev() {
                        p.queued += s.data.len();
                        p.vq.push_front(s);
                    }
                    for a in w.ain.drain(..).rev() {
                        p.queued += a.1.len();
                        p.aq.push_front(a);
                    }
                    if w.flush {
                        w.flush = false;
                        p.flushed = false;
                    }
                    crate::coro::sleep_ms(3);
                }
                continue;
            }
        }
        work_run(w);
        collect(p);
        crate::coro::yield_now();
    }
}

/// Take a finished batch's pictures and sound.
fn collect(p: &mut Player) {
    let w = unsafe { &mut *p.work };
    w.vin.clear();
    w.ain.clear();
    let frames = core::mem::take(&mut w.frames);
    let pcm = core::mem::take(&mut w.pcm);
    if w.dar > 0.0 && p.dar == 0.0 {
        p.dar = w.dar;
    }
    if let Some(f) = w.fatal.take() {
        log(f);
        if p.note.is_empty() {
            p.note = f.to_string();
        }
    }
    p.dec_us += w.us;
    if w.gen != p.gen {
        return;
    }
    p.dec_frames += frames.len() as u64;
    for (pts, img) in frames {
        let at = p.frames.iter().position(|(q, _)| *q > pts).unwrap_or(p.frames.len());
        p.frames.insert(at, (pts, img));
    }
    if !pcm.is_empty() && w.rate > 0 {
        let ch = w.channels.max(1) as usize;
        let mut pcm = &pcm[..];
        let mut t0 = w.pcm_t0;
        if p.stream.is_none() {
            // the first sound after a seek: drop what lies before the target
            if t0 < p.seek_to {
                let drop_frames = (((p.seek_to - t0) * w.rate as f64) as usize).min(pcm.len() / ch);
                pcm = &pcm[drop_frames * ch..];
                t0 += drop_frames as f64 / w.rate as f64;
            }
            if !pcm.is_empty() {
                p.rate = w.rate;
                p.stream = Some(sfx::stream_open(w.rate, ch == 2));
                p.stream_t0 = t0;
                p.voice_on = false;
            }
        }
        if let Some(s) = p.stream {
            if !pcm.is_empty() {
                sfx::stream_push(s, pcm);
            }
            let behind = p.t - p.stream_t0 - KEEP_BEHIND_S;
            if behind > 10.0 && p.rate > 0 {
                sfx::stream_trim(s, (behind * p.rate as f64) as usize);
            }
        }
    }
    if p.eof && p.aq.is_empty() {
        if let Some(s) = p.stream {
            sfx::stream_close(s);
        }
    }
    maybe_ready(p);
}

/// Loading -> ready once the first picture (and a second of sound) are in.
fn maybe_ready(p: &mut Player) {
    if p.state != 0 || p.meta.is_none() {
        return;
    }
    let pic = p.vtrack.is_none() || !p.frames.is_empty() || (p.eof && p.vq.is_empty());
    let snd = p.atrack.is_none()
        || p.no_voice
        || match p.stream {
            Some(s) if p.rate > 0 => (sfx::stream_end(s) as f64 / p.rate as f64) >= 0.7 || (p.eof && p.aq.is_empty()),
            _ => p.eof && p.aq.is_empty(),
        };
    if pic && snd {
        p.state = 1;
        p.t_ms = now_ms();
        if p.dur <= 0.0 {
            p.dur = 0.0;
        }
    }
}

fn finish(pid: u32) {
    let Some(i) = players().iter().position(|p| p.id == pid) else { return };
    let p = &mut players()[i];
    if p.in_flight {
        #[cfg(target_arch = "x86_64")]
        {
            let t0 = now_ms();
            while !crate::arch::smp::bg_finish() {
                if now_ms() - t0 > 5000 {
                    log("a decoding batch did not end; leaving its memory");
                    p.work = core::ptr::null_mut();
                    break;
                }
                core::hint::spin_loop();
            }
        }
        p.in_flight = false;
    }
    stop_sound(p);
    if let Some(s) = p.stream.take() {
        sfx::release(s);
    }
    if !p.work.is_null() {
        drop(unsafe { Box::from_raw(p.work) });
    }
    players().remove(i);
    if players().iter().all(|q| !q.use_worker) {
        crate::mm::aparena::deactivate();
    }
}

fn stop_sound(p: &mut Player) {
    if p.voice_on {
        if p.stream.is_some() && sfx::music_sample() == p.stream {
            sfx::stop_music();
        }
        p.voice_on = false;
    }
}

/// Where the media is (seconds): the music voice while it plays our sound, else the wall clock.
fn clock(p: &mut Player) -> f64 {
    if !p.playing || p.state != 1 {
        return p.t;
    }
    let sound = p.atrack.is_some() && !p.no_voice;
    if sound {
        if p.voice_on {
            if p.stream.is_some() && sfx::music_sample() == p.stream {
                p.t = p.stream_t0 + sfx::music_pos();
            } else {
                p.voice_on = false;
                let at_end = p.eof && p.aq.is_empty() && p.stream.is_some_and(|s| p.rate > 0 && p.t >= p.stream_t0 + sfx::stream_end(s) as f64 / p.rate as f64 - 0.2);
                if at_end {
                    p.t = p.t.max(if p.dur > 0.0 { p.dur } else { p.t });
                    if p.dur <= 0.0 {
                        p.dur = p.t;
                    }
                } else {
                    p.no_voice = true;
                    p.t_ms = now_ms();
                }
            }
        }
        if !p.voice_on && !p.no_voice && (p.dur <= 0.0 || p.t < p.dur - 0.05) {
            if let Some(s) = p.stream {
                let end = p.stream_t0 + sfx::stream_end(s) as f64 / p.rate.max(1) as f64;
                if p.t < end - 0.05 && sfx::play(s, p.vol, 0.0, false, 1.0, true).is_some() {
                    sfx::seek_music((p.t - p.stream_t0).max(0.0));
                    p.voice_on = true;
                }
            }
            // no sound decoded yet: the clock holds
        }
    }
    if !sound || p.no_voice {
        let now = now_ms();
        p.t += (now - p.t_ms) as f64 / 1000.0;
        p.t_ms = now;
    }
    // the end: past the duration, or everything shown
    let drained = p.eof && p.vq.is_empty() && p.frames.is_empty() && p.aq.is_empty() && !p.in_flight;
    let past = p.dur > 0.0 && p.t >= p.dur - 0.05;
    if past || (drained && (!sound || !p.voice_on)) {
        if p.dur <= 0.0 || drained {
            p.dur = p.dur.max(p.t);
        }
        p.t = p.t.min(p.dur.max(p.t));
        p.playing = false;
        p.state = 2;
        stop_sound(p);
    }
    p.t
}

fn reap_players() {
    let dead: Vec<u32> = players().iter().filter(|p| p.fetch_task != 0 && !crate::coro::alive(p.fetch_task) && !crate::coro::alive(p.decode_task)).map(|p| p.id).collect();
    for id in dead {
        finish(id);
    }
}

pub fn open(path: &str) -> f64 {
    crate::coro::on_kill(reap_players);
    // one local video at a time: the older ones go now (their tasks look the player up after
    // every wait and end when it is gone), so the worker arena can start empty -- its size
    // classes never merge, and a 1080p video after a few smaller ones ran out of room
    let old: Vec<u32> = players().iter().map(|p| p.id).collect();
    for id in old {
        close(id as f64);
        finish(id);
    }
    #[cfg(target_arch = "x86_64")]
    if !crate::drivers::videoplay::busy() {
        crate::mm::aparena::deactivate();
    }
    let id = unsafe {
        let id = NEXT_PLAYER;
        NEXT_PLAYER += 1;
        id
    };
    #[cfg(target_arch = "x86_64")]
    let use_worker = crate::arch::smp::has_workers() && crate::mm::aparena::activate(ARENA);
    #[cfg(not(target_arch = "x86_64"))]
    let use_worker = false;
    let work = Box::into_raw(Box::new(Work::new()));
    players().push(Player {
        id,
        path: path.to_string(),
        state: 0,
        err: String::new(),
        note: String::new(),
        closed: false,
        meta: None,
        dur: 0.0,
        vtrack: None,
        atrack: None,
        strack: None,
        subs: Vec::new(),
        retrack: false,
        dar: 0.0,
        vq: VecDeque::new(),
        aq: VecDeque::new(),
        queued: 0,
        v_last: 0.0,
        a_last: 0.0,
        eof: false,
        frames: VecDeque::new(),
        cur: None,
        shown: false,
        stream: None,
        stream_t0: 0.0,
        rate: 0,
        voice_on: false,
        no_voice: false,
        flushed: false,
        playing: true,
        t: 0.0,
        t_ms: now_ms(),
        vol: 1.0,
        gen: 0,
        seek: None,
        seek_to: 0.0,
        reset: false,
        fetch_task: 0,
        decode_task: 0,
        work,
        in_flight: false,
        use_worker,
        dec_us: 0,
        dec_frames: 0,
        dropped: 0,
        shown_frames: 0,
        pace: Default::default(),
    });
    let ft = crate::coro::spawn("media-read", false, Box::new(move || fetch_task(id)));
    let dt = if ft != 0 { crate::coro::spawn("media-decode", false, Box::new(move || decode_task(id))) } else { 0 };
    if let Some(p) = by_id(id) {
        p.fetch_task = ft;
        p.decode_task = dt;
        if ft == 0 || dt == 0 {
            p.state = -1;
            p.err = "no memory for the player's tasks".to_string();
        }
    }
    id as f64
}

/// Move to the picture for now: the newest decoded one at or before the clock.
fn advance(p: &mut Player) {
    let t = clock(p);
    // the picture for when this frame is on screen (the next present slot), on the smoothed
    // audio clock -- see avpace.rs
    let mut tp = p.pace.target(t, p.playing && p.state == 1);
    if crate::drivers::avpace::legacy() {
        tp = t + 0.003; // `media pace old`: the previous rule (raw clock at draw time), for A/B runs
    }
    let mut pick: Option<(f64, Pic)> = None;
    while let Some((pts, _)) = p.frames.front() {
        if *pts <= tp + 0.002 || (!p.shown && pick.is_none()) {
            if pick.is_some() {
                p.dropped += 1;
            }
            pick = p.frames.pop_front();
            p.shown = true;
        } else {
            break;
        }
    }
    if let Some((pts, img)) = pick {
        p.cur = Some(img);
        p.shown_frames += 1;
        p.pace.shown(pts, tp);
    }
}

fn seek(p: &mut Player, t: f64) {
    let t = if p.dur > 0.0 { t.clamp(0.0, (p.dur - 0.1).max(0.0)) } else { t.max(0.0) };
    stop_sound(p);
    if let Some(s) = p.stream.take() {
        sfx::release(s);
    }
    p.t = t;
    p.t_ms = now_ms();
    p.pace.reset();
    p.gen = p.gen.wrapping_add(1);
    p.seek = Some(t);
    p.seek_to = t;
    p.reset = true;
    p.vq.clear();
    p.aq.clear();
    p.queued = 0;
    p.frames.clear();
    p.shown = false;
    p.no_voice = false;
    p.eof = false;
    p.flushed = false;
    if p.state == 2 {
        p.state = 1;
    }
}

fn play(p: &mut Player) {
    if p.state == 2 {
        seek(p, 0.0);
    }
    if !p.playing {
        p.playing = true;
        p.t_ms = now_ms();
        p.voice_on = false;
    }
}

fn pause(p: &mut Player) {
    if p.playing {
        let t = clock(p);
        p.playing = false;
        stop_sound(p);
        p.t = t;
    }
}

fn close(id: f64) {
    if let Some(p) = player(id) {
        p.closed = true;
        p.playing = false;
        stop_sound(p);
        let (pid, ft, dt) = (p.id, p.fetch_task, p.decode_task);
        let dead = |t: u64| t == 0 || !crate::coro::alive(t);
        if dead(ft) && dead(dt) {
            finish(pid);
        }
    }
}

// ── drawing ──────────────────────────────────────────────────────────────────

/// Column map for a scale: source x (16.16 fixed) per output column.
fn xmap(src_w: usize, dst_w: usize) -> Vec<u32> {
    let step = ((src_w as u64) << 16) / dst_w.max(1) as u64;
    (0..dst_w)
        .map(|x| {
            let s = (x as u64 * step + step / 2).saturating_sub(1 << 15);
            s.min(((src_w.max(1) - 1) as u64) << 16) as u32
        })
        .collect()
}

#[inline]
fn lerp(a: u32, b: u32, f: u32) -> u32 {
    (a * (256 - f) + b * f) >> 8
}

/// Scale `pic` into `buf` (stride `bw`) at (ox, oy, dw, dh), clipped to `clip`.
fn blit(pic: &Pic, buf: &mut [u32], bw: usize, clip: [i32; 4], ox: i32, oy: i32, dw: i32, dh: i32) {
    let (sw, sh) = pic.size();
    let (sw, sh) = (sw as usize, sh as usize);
    if sw == 0 || sh == 0 || dw <= 0 || dh <= 0 {
        return;
    }
    let x0 = ox.max(clip[0]);
    let x1 = (ox + dw).min(clip[2]);
    let y0 = oy.max(clip[1]);
    let y1 = (oy + dh).min(clip[3]);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let xm = xmap(sw, dw as usize);
    let ym = xmap(sh, dh as usize);
    let rows = (y1 - y0) as usize;
    let out = crate::par::Shared(buf.as_mut_ptr());
    let blen = buf.len();
    let (chunks, per) = crate::par::row_chunks(rows, 16);
    let job = |c: usize| {
        let ptr = out.get();
        for r in c * per..((c + 1) * per).min(rows) {
            let y = y0 as usize + r;
            let sy = ym[y - oy as usize];
            let (yi, yf) = ((sy >> 16) as usize, (sy >> 8) & 255);
            let yi2 = (yi + 1).min(sh - 1);
            let row_start = y * bw;
            if row_start + x1 as usize > blen {
                continue;
            }
            let dst = unsafe { core::slice::from_raw_parts_mut(ptr.add(row_start), bw) };
            match pic {
                Pic::Rgb(img) => {
                    let a = &img.px[yi * sw..yi * sw + sw];
                    let b = &img.px[yi2 * sw..yi2 * sw + sw];
                    for x in x0 as usize..x1 as usize {
                        let sx = xm[x - ox as usize];
                        let (xi, xf) = ((sx >> 16) as usize, (sx >> 8) & 255);
                        let xi2 = (xi + 1).min(sw - 1);
                        let (p00, p01, p10, p11) = (a[xi], a[xi2], b[xi], b[xi2]);
                        let ch = |s: u32| {
                            let t = lerp((p00 >> s) & 255, (p01 >> s) & 255, xf);
                            let u = lerp((p10 >> s) & 255, (p11 >> s) & 255, xf);
                            lerp(t, u, yf)
                        };
                        dst[x] = (ch(16) << 16) | (ch(8) << 8) | ch(0);
                    }
                },
                Pic::Yuv { y: yp, u: up, v: vp, ys, cs, bt709, full, h, .. } => {
                    let ch_h = (*h as usize).div_ceil(2);
                    let (cyi, cyf) = {
                        // chroma rows sit between luma rows (4:2:0, MPEG-2 siting)
                        let c = (sy as i64 - (1 << 15)).max(0) as u64 / 2;
                        ((c >> 16) as usize, ((c >> 8) & 255) as u32)
                    };
                    let cyi2 = (cyi + 1).min(ch_h.saturating_sub(1));
                    let ya = &yp[yi * ys..];
                    let yb = &yp[yi2 * ys..];
                    let ua = &up[cyi * cs..];
                    let ub = &up[cyi2 * cs..];
                    let va = &vp[cyi * cs..];
                    let vb = &vp[cyi2 * cs..];
                    // BT.601 / 709, limited or full range, in 8.8 fixed point
                    let (cr_r, cb_g, cr_g, cb_b) = if *bt709 { (403i32, 48i32, 120i32, 475i32) } else { (359i32, 88i32, 183i32, 454i32) };
                    let (ky, oy_) = if *full { (256i32, 0i32) } else { (298i32, 16i32) };
                    let kc = if *full { 256i32 } else { 292i32 };
                    let cw = (*cs).min((sw + 1) / 2).max(1);
                    for x in x0 as usize..x1 as usize {
                        let sx = xm[x - ox as usize];
                        let (xi, xf) = ((sx >> 16) as usize, (sx >> 8) & 255);
                        let xi2 = (xi + 1).min(sw - 1);
                        let l0 = lerp(ya[xi] as u32, ya[xi2] as u32, xf);
                        let l1 = lerp(yb[xi] as u32, yb[xi2] as u32, xf);
                        let luma = lerp(l0, l1, yf) as i32;
                        let cx = (sx / 2) as usize;
                        let (cxi, cxf) = ((cx >> 16).min(cw - 1), ((cx >> 8) & 255) as u32);
                        let cxi2 = (cxi + 1).min(cw - 1);
                        let u = lerp(lerp(ua[cxi] as u32, ua[cxi2] as u32, cxf), lerp(ub[cxi] as u32, ub[cxi2] as u32, cxf), cyf) as i32 - 128;
                        let v = lerp(lerp(va[cxi] as u32, va[cxi2] as u32, cxf), lerp(vb[cxi] as u32, vb[cxi2] as u32, cxf), cyf) as i32 - 128;
                        let yy = (luma - oy_) * ky;
                        let (u, v) = (u * kc / 256, v * kc / 256);
                        let r = ((yy + cr_r * v + 128) >> 8).clamp(0, 255) as u32;
                        let g = ((yy - cb_g * u - cr_g * v + 128) >> 8).clamp(0, 255) as u32;
                        let b = ((yy + cb_b * u + 128) >> 8).clamp(0, 255) as u32;
                        dst[x] = (r << 16) | (g << 8) | b;
                    }
                },
            }
        }
    };
    crate::par::for_each(chunks, &job);
}

/// `os_media_draw(id, x, y, w, h)`: the picture for now, letterboxed on black, with subtitles.
fn draw(p: &mut Player, rx: f64, ry: f64, rw: f64, rh: f64) -> bool {
    advance(p);
    let t = p.t;
    let Some((buf, bw, bh)) = crate::lingapp::surface() else { return false };
    let mut clip = crate::lingapp::surface_clip();
    clip[2] = clip[2].min(bw as i32);
    clip[3] = clip[3].min(bh as i32);
    let (rx, ry, rw, rh) = (rx as i32, ry as i32, rw.max(1.0) as i32, rh.max(1.0) as i32);
    // black behind
    for y in ry.max(clip[1])..(ry + rh).min(clip[3]) {
        let s = y as usize * bw;
        let (a, b) = (rx.max(clip[0]) as usize, (rx + rw).min(clip[2]).max(0) as usize);
        if a < b {
            buf[s + a..s + b].fill(0);
        }
    }
    // the picture's place: its display aspect
    let (pw, ph) = p.cur.as_ref().map(|c| c.size()).or_else(|| track(p, p.vtrack).map(|t| (t.w, t.h))).unwrap_or((16, 9));
    let aspect = if p.dar > 0.1 { p.dar } else { pw.max(1) as f64 / ph.max(1) as f64 };
    let (mut dw, mut dh) = (rw, (rw as f64 / aspect) as i32);
    if dh > rh {
        dh = rh;
        dw = (rh as f64 * aspect) as i32;
    }
    let (ox, oy) = (rx + (rw - dw) / 2, ry + (rh - dh) / 2);
    if let Some(pic) = p.cur.as_ref() {
        blit(pic, buf, bw, clip, ox, oy, dw, dh);
    }
    if let Some(s) = p.strack.and_then(|i| p.subs.get(i)) {
        let mut cv = subs::Canvas { buf, stride: bw, clip };
        // subtitles keep inside the window even for a narrow picture (audio-only: the whole area)
        let r = if p.cur.is_some() { (ox, oy, dw, dh) } else { (rx, ry, rw, rh) };
        subs::draw(&s.subs, t, &mut cv, r);
    }
    true
}

// ── the builtins ─────────────────────────────────────────────────────────────

fn tracks_of(p: &Player, kind: u8) -> Vec<String> {
    match kind {
        2 => p.subs.iter().map(|s| mediafile::track_label(&s.desc)).collect(),
        _ => p.meta.as_ref().map(|m| m.tracks.iter().filter(|t| t.kind == kind).map(mediafile::track_label).collect()).unwrap_or_default(),
    }
}

fn track_index(p: &Player, kind: u8) -> f64 {
    match kind {
        2 => p.strack.map(|i| i as f64).unwrap_or(-1.0),
        _ => {
            let cur = if kind == 0 { p.vtrack } else { p.atrack };
            p.meta.as_ref().and_then(|m| m.tracks.iter().filter(|t| t.kind == kind).position(|t| Some(t.id) == cur)).map(|i| i as f64).unwrap_or(-1.0)
        },
    }
}

fn set_track(p: &mut Player, kind: u8, i: f64) {
    if kind == 2 {
        p.strack = if i < 0.0 || i as usize >= p.subs.len() { None } else { Some(i as usize) };
        return;
    }
    if kind != 1 {
        return;
    }
    let Some(m) = p.meta.as_ref() else { return };
    let list: Vec<&TrackDesc> = m.tracks.iter().filter(|t| t.kind == 1).collect();
    let pick = if i < 0.0 { None } else { list.get(i as usize).copied() };
    if let Some(t) = pick {
        if let Codec::No(why) = &t.codec {
            p.note = format!("{why} - playing without sound");
            p.atrack = None;
        } else {
            p.note.clear();
            p.atrack = Some(t.id);
        }
    } else {
        p.atrack = None;
    }
    // the reader picks the new track up and starts over here
    let t = clock(p);
    seek(p, t);
    p.retrack = true;
}

fn info_line(p: &Player) -> String {
    let Some(m) = p.meta.as_ref() else { return String::new() };
    let mut parts: Vec<String> = alloc::vec![m.format.to_string()];
    if let Some(v) = track(p, p.vtrack) {
        let (w, h) = p.cur.as_ref().map(|c| c.size()).unwrap_or((v.w, v.h));
        let mut s = format!("{} {}x{}", v.cname, w, h);
        if v.interlaced || matches!(v.codec, Codec::V(VCodec::Mpeg2(_))) && h == 576 || h == 480 && matches!(v.codec, Codec::V(VCodec::Mpeg2(_))) {
            s.push('i');
        }
        if v.fps > 0.0 {
            s.push_str(&format!(" {:.3} fps", v.fps).replace(".000", ""));
        }
        parts.push(s);
    }
    if let Some(a) = track(p, p.atrack) {
        let mut s = a.cname.clone();
        if a.rate > 0 {
            s.push_str(&format!(" {} kHz", a.rate as f64 / 1000.0));
        }
        parts.push(s);
    }
    parts.join("  -  ")
}

/// The `os_media_*` builtins (lingapp.rs routes them here).
pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    use crate::lingrt::{boolean, list_from, mk_str, num, to_f64, to_string, UNIT};
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let n = |i: usize, d: f64| args.get(i).map(|&v| to_f64(v)).unwrap_or(d);
    let pl = || player(n(0, -1.0));
    Some(match name {
        b"os_media_open" => num(open(&s(0))),
        b"os_media_state" => num(pl()
            .map(|p| {
                if p.state == 1 && p.playing {
                    clock(p);
                }
                p.state as f64
            })
            .unwrap_or(-1.0)),
        b"os_media_error" => mk_str(&pl().map(|p| p.err.clone()).unwrap_or_else(|| "no such video".to_string())),
        b"os_media_note" => mk_str(&pl().map(|p| if p.state == 0 && p.note.is_empty() { "loading".to_string() } else { p.note.clone() }).unwrap_or_default()),
        b"os_media_info" => mk_str(&pl().map(|p| info_line(p)).unwrap_or_default()),
        b"os_media_draw" => boolean(pl().filter(|p| p.state >= 0).is_some_and(|p| draw(p, n(1, 0.0), n(2, 0.0), n(3, 320.0), n(4, 240.0)))),
        b"os_media_has_video" => boolean(pl().is_some_and(|p| p.vtrack.is_some())),
        b"os_media_width" => num(pl().and_then(|p| p.cur.as_ref().map(|c| c.size().0 as f64).or_else(|| track(p, p.vtrack).map(|t| t.w as f64))).unwrap_or(0.0)),
        b"os_media_height" => num(pl().and_then(|p| p.cur.as_ref().map(|c| c.size().1 as f64).or_else(|| track(p, p.vtrack).map(|t| t.h as f64))).unwrap_or(0.0)),
        b"os_media_aspect" => num(pl()
            .map(|p| {
                if p.dar > 0.1 {
                    p.dar
                } else {
                    p.cur.as_ref().map(|c| c.size()).map(|(w, h)| w as f64 / h.max(1) as f64).unwrap_or(16.0 / 9.0)
                }
            })
            .unwrap_or(16.0 / 9.0)),
        b"os_media_duration" => num(pl().map(|p| p.dur).unwrap_or(0.0)),
        b"os_media_time" => num(pl().map(clock).unwrap_or(0.0)),
        b"os_media_playing" => boolean(pl().is_some_and(|p| p.playing)),
        b"os_media_loops" => boolean(pl().is_some_and(|p| p.meta.as_ref().is_some_and(|m| m.loops))),
        b"os_media_play" => {
            if let Some(p) = pl() {
                play(p);
            }
            UNIT
        },
        b"os_media_pause" => {
            if let Some(p) = pl() {
                pause(p);
            }
            UNIT
        },
        b"os_media_seek" => {
            if let Some(p) = pl() {
                seek(p, n(1, 0.0));
            }
            UNIT
        },
        b"os_media_volume" => {
            if let Some(p) = pl() {
                let v = n(1, 1.0).clamp(0.0, 1.0) as f32;
                if (v - p.vol).abs() > 0.001 {
                    p.vol = v;
                    if p.voice_on {
                        // the voice keeps the volume it started with: start it again where it is
                        let t = clock(p);
                        stop_sound(p);
                        p.t = t;
                    }
                }
            }
            UNIT
        },
        b"os_media_get_volume" => num(pl().map(|p| p.vol as f64).unwrap_or(1.0)),
        b"os_media_tracks" => list_from(pl().map(|p| tracks_of(p, n(1, 1.0) as u8)).unwrap_or_default().iter().map(|t| mk_str(t)).collect()),
        b"os_media_track" => num(pl().map(|p| track_index(p, n(1, 1.0) as u8)).unwrap_or(-1.0)),
        b"os_media_set_track" => {
            if let Some(p) = pl() {
                set_track(p, n(1, 1.0) as u8, n(2, -1.0));
            }
            UNIT
        },
        b"os_media_chapters" => list_from(
            pl().and_then(|p| p.meta.as_ref())
                .map(|m| m.chapters.iter().enumerate().map(|(i, (t, title))| mk_str(&format!("{}|{}", t, if title.is_empty() { format!("Chapter {}", i + 1) } else { title.clone() }))).collect())
                .unwrap_or_default(),
        ),
        b"os_media_close" => {
            close(n(0, -1.0));
            UNIT
        },
        _ => return None,
    })
}

/// `media` (SSH / terminal): what the players are doing; `media open <path>` etc. for tests.
pub fn command(arg: &str) -> String {
    let mut it = arg.split_whitespace();
    match it.next() {
        Some("open") => {
            let path = arg.trim_start()[4..].trim();
            let id = open(path);
            format!("media {id}: opening {path}\n")
        },
        Some("seek") => {
            let t: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
            if let Some(p) = players().iter_mut().rfind(|p| !p.closed) {
                seek(p, t);
            }
            status()
        },
        Some("pause") => {
            if let Some(p) = players().iter_mut().rfind(|p| !p.closed) {
                if p.playing {
                    pause(p)
                } else {
                    play(p)
                }
            }
            status()
        },
        Some("pace") => {
            // A/B: `media pace old` = the previous picture choice, `media pace new` = avpace.rs
            match it.next() {
                Some("old") => crate::drivers::avpace::set_legacy(true),
                Some("new") => crate::drivers::avpace::set_legacy(false),
                _ => {},
            }
            format!("media pace: {}\n", if crate::drivers::avpace::legacy() { "old (raw clock when drawing)" } else { "new (present slot, smoothed audio clock)" })
        },
        Some("close") => {
            let ids: Vec<u32> = players().iter().map(|p| p.id).collect();
            for id in ids {
                close(id as f64);
            }
            String::from("closed\n")
        },
        _ => status(),
    }
}

pub fn status() -> String {
    let mut s = String::new();
    for p in players().iter() {
        let fps = if p.dec_us > 0 { p.dec_frames as f64 * 1e6 / p.dec_us as f64 } else { 0.0 };
        s.push_str(&format!(
            "media {}: state {} t {:.2}/{:.0}s {} | {} | queued {}v/{}a ({} KB) ready {} shown {} dropped {} decode {:.0} fps{}{}{}\n",
            p.id,
            p.state,
            p.t,
            p.dur,
            crate::vfs::basename(&p.path),
            info_line(p),
            p.vq.len(),
            p.aq.len(),
            p.queued / 1024,
            p.frames.len(),
            p.shown_frames,
            p.dropped,
            fps,
            if p.use_worker { format!(" (worker core, arena {} of {} MiB)", crate::mm::aparena::stats().1 >> 20, crate::mm::aparena::stats().0 >> 20) } else { String::new() },
            if p.note.is_empty() { String::new() } else { format!(" note: {}", p.note) },
            if p.err.is_empty() { String::new() } else { format!(" error: {}", p.err) },
        ));
        s.push_str(&format!("  {}\n", p.pace.report()));
    }
    if s.is_empty() {
        s.push_str("no media playing\n");
    }
    s
}
