//! Video in Horizon on LingOS (`os_video_*`), played entirely on this machine:
//! no helper program, no server on the network.
//!
//! * **Finding the streams** -- `youtube.rs` asks YouTube's InnerTube API for
//!   the video's stream addresses (a client whose answers carry plain URLs)
//!   and picks H.264 (at most 480p; 360p on the Pi) plus AAC-LC sound (itag
//!   140): YouTube's DASH streams, fragmented MP4 with a segment index (sidx).
//! * **Fetching** -- one green thread per video reads the init segment and the
//!   sidx, then the media segments a few seconds ahead of the clock, each with
//!   YouTube's own `&range=a-b` requests in pieces of at most 2 MiB over the
//!   kernel's HTTPS (`tls::https_get_stream`). Seeking starts at the segment
//!   under the new time (a segment starts with a key frame).
//! * **Demuxing** -- `ling-mp4` reads the init segment, the sidx and the
//!   moof+mdat fragments into samples with presentation times.
//! * **Decoding** -- `ling-h264` (pictures, turned into 0xFFRRGGBB at the
//!   video's own size) and `ling-aac` (sound). A second green thread hands the
//!   decoding to a worker core (`smp::bg_start`; its allocations go to
//!   `mm::aparena`), so the desktop's core only copies results; without one
//!   (the Pi, a one-core machine) it decodes one picture at a time here and
//!   lets the desktop run in between.
//! * **Playing** -- the sound goes into a streamed sfx sample on the music
//!   voice (the mixer resamples 44.1 kHz itself); its position is the clock
//!   the pictures follow (a wall clock without sound). Pause, seek, volume and
//!   close as before.
//!
//! Stream addresses carry an IP-bound token: they are never written to the
//! console (only the itag and size).

use crate::compat;
use crate::drivers::{image, netstack as ns, sfx, youtube as yt};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

fn now_ms() -> u64 {
    crate::arch::timer::now_ms()
}

/// Seconds of compressed media fetched ahead of the clock.
const AHEAD_S: f64 = 10.0;
/// Decoded pictures kept ready.
const FRAMES_READY: usize = 8;
/// The tallest picture picked: decoding is all software (a PC core does 720p).
#[cfg(target_arch = "x86_64")]
pub(crate) const MAX_H: u32 = 720;
#[cfg(not(target_arch = "x86_64"))]
pub(crate) const MAX_H: u32 = 360;
/// Largest piece asked for in one request.
const CHUNK: u64 = 2 << 20;
/// Sound kept behind the clock before it is trimmed (seconds).
const KEEP_BEHIND_S: f64 = 3.0;

fn log(m: &str) {
    crate::console_write(format!("[video] {m}\n").as_bytes());
}

// ── the network ──────────────────────────────────────────────────────────────

/// The visitor id YouTube handed out (asked for again with it, it stops asking "are you a bot").
static mut VISITOR: String = String::new();

fn visitor() -> String {
    unsafe { (*(&raw const VISITOR)).clone() }
}

/// One `/player` request: the answer's JSON body.
///
/// Over a LingSockets connection (`lingsock::request_full`: a socket of its own, gzip -- the
/// answer is ~8x smaller on the wire -- keep-alive, waits up to 12 s of silence, a dead pooled
/// connection retried on a fresh one), twice; then once over the kernel's one-shot HTTPS
/// (`tls::https_request`). Every failure says what happened (DNS, connect, TLS, silence, bytes).
///
/// `signed`: with the user's YouTube sign-in (`ytauth.rs`: Cookie + SAPISIDHASH Authorization).
fn post_player(c: &yt::Client, id: &str, visitor: &str, signed: bool) -> Result<Vec<u8>, String> {
    let (mut headers, body) = yt::player_request(c, id, visitor);
    if signed {
        headers.push_str(&super::ytauth::api_headers());
    }
    let url = format!("https://{}{}", yt::API_HOST, yt::API_PATH);
    let hdrs = format!("Content-Type: application/json\r\nUser-Agent: {}\r\n{headers}", c.user_agent);
    let mut why = String::new();
    for attempt in 0..2 {
        match crate::lingsock::request_full("POST", &url, &hdrs, body.as_bytes(), 8 << 20) {
            Ok((status, head, b)) => {
                if signed {
                    super::ytauth::absorb(yt::API_HOST, &head);
                }
                if status == 200 {
                    return Ok(b);
                }
                if status == 0 || status >= 500 || status == 429 {
                    why = format!("{} answered HTTP {status} ({} client, {} bytes)", yt::API_HOST, c.key, b.len());
                    log(&why);
                    crate::coro::sleep_ms(500 + 700 * attempt);
                    continue;
                }
                return Err(format!("{} answered HTTP {status} ({} client)", yt::API_HOST, c.key));
            },
            Err(e) => {
                why = e;
                log(&format!("/player ({} client), try {}: {why}", c.key, attempt + 1));
                crate::coro::sleep_ms(400 + 600 * attempt);
            },
        }
    }
    // the other HTTPS path
    let mut out = alloc::vec![0u8; 4 << 20];
    let mut quiet = |_m: &[u8]| {};
    let n = match crate::tls::https_request(yt::API_HOST, 443, "POST", yt::API_PATH, "application/json", &headers, body.as_bytes(), &mut out, 3800, &mut quiet) {
        Ok(n) => n,
        Err(e) => return Err(format!("{} did not answer ({why}; then: {e})", yt::API_HOST)),
    };
    let resp = &out[..n];
    let Some(he) = super::netmedia::find(resp, b"\r\n\r\n") else {
        return Err(format!("{} sent no HTTP answer ({why}; then {n} bytes without a header)", yt::API_HOST));
    };
    let status: u16 = core::str::from_utf8(&resp[..he]).ok().and_then(|s| s.split(' ').nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    let head = &resp[..he];
    let mut body = resp[he + 4..].to_vec();
    if super::netmedia::header(head, "transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        body = super::netmedia::dechunk(&body);
    }
    if status != 200 {
        return Err(format!("{} answered HTTP {status} ({} client)", yt::API_HOST, c.key));
    }
    log(&format!("/player ({} client): LingSockets failed ({why}), the one-shot HTTPS worked", c.key));
    Ok(body)
}

/// Bytes `a..=b` of a stream, in pieces of at most [`CHUNK`] (one keep-alive LingSockets request
/// each), following googlevideo's redirects to another server and retrying a piece that failed.
pub(crate) fn get_range(url: &str, a: u64, b: u64, keep_going: &dyn Fn() -> bool) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::with_capacity((b + 1 - a) as usize);
    let mut cur = url.to_string();
    let mut tries = 0;
    let mut hops = 0;
    while (out.len() as u64) < b + 1 - a {
        if !keep_going() {
            return Err("stopped".to_string());
        }
        let from = a + out.len() as u64;
        let to = (from + CHUNK - 1).min(b);
        let full = yt::with_range(&cur, from, to);
        let host = ns::parse_url(&full).map(|(h, _, _, _)| h.to_string()).unwrap_or_default();
        match crate::lingsock::request_full("GET", &full, "", &[], (CHUNK as usize) + (1 << 20)) {
            Ok((status, head, body)) => {
                if (300..400).contains(&status) {
                    // googlevideo sends some requests on to another server: its address carries our
                    // range, so keep its base
                    let loc = head.lines().find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.trim().eq_ignore_ascii_case("location").then(|| v.trim().to_string())
                    });
                    let Some(loc) = loc else { return Err(format!("{host} answered HTTP {status} without a Location")) };
                    let loc = super::netmedia::resolve(&full, &loc);
                    cur = match loc.find("&range=") {
                        Some(i) => {
                            let rest = &loc[i + 7..];
                            let tail = rest.find('&').map(|k| &rest[k..]).unwrap_or("");
                            format!("{}{tail}", &loc[..i])
                        },
                        None => loc,
                    };
                    hops += 1;
                    if hops > 4 {
                        return Err(format!("{host}: too many redirects"));
                    }
                    continue;
                }
                if status == 416 && !out.is_empty() {
                    break; // asked past the end (the stated length was a little long): what came is all of it
                }
                if status != 200 && status != 206 {
                    return Err(format!("{host} answered HTTP {status}"));
                }
                if body.is_empty() {
                    tries += 1;
                    if tries > 3 {
                        return Err(format!("{host} answered HTTP {status} with no bytes"));
                    }
                    crate::coro::sleep_ms(300);
                    continue;
                }
                let want = (to + 1 - from) as usize;
                out.extend_from_slice(&body[..body.len().min(want)]);
                tries = 0;
            },
            Err(e) => {
                tries += 1;
                log(&format!("{host}: bytes {from}-{to}, try {tries}: {e}"));
                if tries > 3 {
                    return Err(format!("{host}: {e}"));
                }
                crate::coro::sleep_ms(300 * tries as u64);
            },
        }
    }
    out.truncate((b + 1 - a) as usize);
    Ok(out)
}

/// Ask YouTube for the streams: each client in turn, again with a visitor id when it asks for one.
pub(crate) fn resolve(id: &str, note: &dyn Fn(&str)) -> Result<yt::Streams, String> {
    resolve_skip(id, note, &[])
}

/// [`resolve`] without the clients in `skip` (their stream addresses were refused).
pub(crate) fn resolve_skip(id: &str, note: &dyn Fn(&str), skip: &[&str]) -> Result<yt::Streams, String> {
    let mut bot = false;
    let r = resolve_with(id, note, skip, false, &mut bot);
    if r.is_ok() || !bot {
        return r;
    }
    let first = r.err().unwrap_or_default();
    if !super::ytauth::signed_in() {
        return Err(format!(
            "YouTube wants you to sign in (\"Sign in to confirm you're not a bot\"). Sign in on Horizon's YouTube page (Sign in to YouTube), or run youtube login <cookies.txt> in the Terminal. ({first})"
        ));
    }
    // signed in: the same video again with the account (yt-dlp's signed-in clients)
    log(&format!("{id}: YouTube asked for a sign-in; trying again signed in"));
    let mut bot2 = false;
    match resolve_with(id, note, skip, true, &mut bot2) {
        Ok(s) => Ok(s),
        Err(e) if bot2 && !e.contains("signature-ciphered") => Err(format!(
            "YouTube still asks to sign in, even with your account's cookies: they may have expired or been signed out. Export a fresh cookies.txt and sign in again (Horizon's YouTube page, or youtube login <cookies.txt>). ({e})"
        )),
        Err(e) => Err(format!("{e} (signed in; without the account: {first})")),
    }
}

/// One round of [`resolve_skip`]: the plain clients (`signed` false), or the signed-in ones --
/// the plain clients' first with the cookies too (yt-dlp says it takes none, but it costs one
/// request), then `yt::AUTHED_CLIENTS`. `bot` is set when YouTube answered with a sign-in demand.
fn resolve_with(id: &str, note: &dyn Fn(&str), skip: &[&str], signed: bool, bot: &mut bool) -> Result<yt::Streams, String> {
    let mut last = String::from("YouTube did not answer");
    // every client's outcome: the error shown used to be only the LAST client's, so a network
    // failure of a fallback client ("sent no HTTP answer") hid what YouTube itself had said to
    // the first one (a bot check, "unplayable", ...) -- the user saw only "no HTTP"
    let mut tried: Vec<String> = Vec::new();
    let mut said = false;
    let clients: Vec<&yt::Client> = if signed { yt::CLIENTS.iter().take(1).chain(yt::AUTHED_CLIENTS.iter()).collect() } else { yt::CLIENTS.iter().collect() };
    for c in clients.into_iter().filter(|c| !skip.contains(&c.key)) {
        let mut vis = visitor();
        for attempt in 0..2 {
            note(&format!("asking YouTube for the video ({}{})", c.key, if signed { ", signed in" } else { "" }));
            let body = match post_player(c, id, &vis, signed) {
                Ok(b) => b,
                Err(e) => {
                    log(&format!("{}: {e}", c.key));
                    tried.push(format!("{}: {e}", c.key));
                    if !said {
                        last = e;
                    }
                    break;
                },
            };
            match yt::parse_player(&body, c.key, MAX_H) {
                Ok(s) => {
                    if !vis.is_empty() {
                        unsafe { *(&raw mut VISITOR) = vis };
                    }
                    return Ok(s);
                },
                Err(r) => {
                    log(&format!("{}: {} {}", c.key, r.status, r.reason));
                    tried.push(format!("{}: {}", c.key, r.reason));
                    if yt::is_bot_check(&r.status, &r.reason) {
                        *bot = true;
                    }
                    // what YouTube itself says beats a later client's network failure
                    last = r.reason.clone();
                    said = true;
                    // "sign in to confirm you're not a bot": the same again with the visitor id it gave
                    if attempt == 0 && !r.visitor.is_empty() && r.visitor != vis && (r.status == "LOGIN_REQUIRED" || r.status == "UNPLAYABLE") {
                        vis = r.visitor;
                        continue;
                    }
                    if r.status == "ERROR" {
                        // "This video is unavailable": no other client will have it either
                        return Err(last);
                    }
                    break;
                },
            }
        }
    }
    if tried.len() > 1 {
        let all: String = tried.join("; ").chars().take(600).collect();
        return Err(format!("{last} -- every way tried: {all}"));
    }
    Err(last)
}

// ── MP4 (ling-mp4) ───────────────────────────────────────────────────────────

/// A media segment from the sidx: bytes in the file and its time.
#[derive(Clone, Copy)]
pub(crate) struct Seg {
    pub(crate) off: u64,
    pub(crate) size: u64,
    pub(crate) t0: f64,
}

/// One compressed picture or sound frame.
pub(crate) struct Sample {
    /// Presentation time (seconds).
    pub(crate) pts: f64,
    /// A key frame (decoding can start here).
    pub(crate) key: bool,
    pub(crate) data: Vec<u8>,
}

/// One stream (picture or sound) being fetched.
struct Track {
    fmt: yt::Format,
    init: mp4::Init,
    segs: Vec<Seg>,
    /// The next segment to fetch.
    next: usize,
    /// Media time up to which segments are fetched.
    ahead: f64,
}

impl Track {
    fn seg_at(&self, t: f64) -> usize {
        match self.segs.iter().rposition(|s| s.t0 <= t + 1e-6) {
            Some(i) => i,
            None => 0,
        }
    }
    fn seg_end(&self, i: usize, dur: f64) -> f64 {
        self.segs.get(i + 1).map(|s| s.t0).unwrap_or(dur.max(self.segs.get(i).map(|s| s.t0).unwrap_or(0.0)))
    }
}

/// The init segment and the sidx of a stream (one request: they are next to each other).
fn open_track(fmt: yt::Format, keep_going: &dyn Fn() -> bool) -> Result<Track, String> {
    let end = fmt.index.1.max(fmt.init.1);
    let head = get_range(&fmt.url, 0, end, keep_going)?;
    let init = mp4::parse_init(&head[..(fmt.init.1 as usize + 1).min(head.len())]).ok_or_else(|| format!("{}: unreadable init segment", yt::short(&fmt)))?;
    let idx = &head[(fmt.index.0 as usize).min(head.len())..];
    let segs = mp4::parse_sidx(idx, fmt.index.0, &init).ok_or_else(|| format!("{}: unreadable segment index", yt::short(&fmt)))?;
    if segs.is_empty() {
        return Err(format!("{}: empty segment index", yt::short(&fmt)));
    }
    Ok(Track { fmt, init, segs, next: 0, ahead: 0.0 })
}

// ── decoding (a worker core when there is one) ───────────────────────────────

/// The decoders and one batch of work. It lives on the heap (a task can be
/// killed while a worker core still runs its batch). On a worker core it may
/// only read what the boot core put in (`vin`, `ain`, `avcc`, `asc`), and
/// makes its results (`frames`, `pcm`) as new allocations.
struct Work {
    gen: u32,
    reset_v: bool,
    reset_a: bool,
    avcc: Vec<u8>,
    asc: Vec<u8>,
    vin: Vec<Sample>,
    ain: Vec<Vec<u8>>,
    /// Pictures before this time are decoded (they are references) but not kept.
    skip_before: f64,
    /// The stream ended: hand out the pictures the decoder still holds back.
    flush: bool,
    vdec: Option<ling_h264::Decoder>,
    adec: Option<ling_aac::AacDecoder>,
    /// Presentation times of pictures fed and not out yet, smallest first
    /// (the decoder hands pictures out in display order).
    pending: Vec<f64>,
    frames: Vec<(f64, image::Image)>,
    pcm: Vec<i16>,
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
            avcc: Vec::new(),
            asc: Vec::new(),
            vin: Vec::new(),
            ain: Vec::new(),
            skip_before: 0.0,
            flush: false,
            vdec: None,
            adec: None,
            pending: Vec::new(),
            frames: Vec::new(),
            pcm: Vec::new(),
            rate: 0,
            channels: 0,
            verrs: 0,
            aerrs: 0,
            fatal: None,
            us: 0,
        }
    }
}

fn take_frame(w: &mut Work, f: ling_h264::Frame) {
    let pts = if w.pending.is_empty() { f64::NAN } else { w.pending.remove(0) };
    if pts.is_nan() || pts + 0.001 < w.skip_before {
        return;
    }
    let (fw, fh) = (f.width as usize, f.height as usize);
    if fw == 0 || fh == 0 {
        return;
    }
    let mut px = alloc::vec![0u32; fw * fh];
    ling_h264::frame_to_rgba(&f, &mut px, fw, fh);
    for p in px.iter_mut() {
        *p |= 0xFF00_0000;
    }
    w.frames.push((pts, image::Image { w: fw as u32, h: fh as u32, px }));
}

/// Decode the batch (runs on a worker core, or here).
fn work_run(w: &mut Work) {
    let t0 = crate::arch::timer::now_us();
    w.fatal = None;
    if w.reset_a {
        w.reset_a = false;
        w.adec = None;
        if !w.asc.is_empty() {
            match ling_aac::AacDecoder::new(&w.asc) {
                Ok(d) => w.adec = Some(d),
                Err(_) => w.fatal = Some("the sound is in an AAC flavour LingOS does not decode"),
            }
        }
    }
    if w.reset_v {
        w.reset_v = false;
        w.vdec = None;
        w.pending = Vec::new();
        if !w.avcc.is_empty() {
            match ling_h264::Decoder::from_avcc(&w.avcc) {
                Ok(d) => w.vdec = Some(d),
                Err(_) => w.fatal = Some("the picture is in an H.264 profile LingOS does not decode"),
            }
        }
    }
    // sound first: it is the clock
    if let Some(dec) = w.adec.as_mut() {
        let mut f: Vec<f32> = Vec::new();
        for k in 0..w.ain.len() {
            if dec.decode(&w.ain[k], &mut f).is_err() {
                w.aerrs += 1;
            }
        }
        w.rate = dec.sample_rate();
        w.channels = dec.channels();
        let ch = w.channels.max(1) as usize;
        // to stereo (or mono) i16
        let out_ch = if ch >= 2 { 2 } else { 1 };
        let mut pcm: Vec<i16> = Vec::with_capacity(f.len() / ch * out_ch);
        for fr in f.chunks_exact(ch) {
            for c in 0..out_ch {
                pcm.push((fr[c] * 32767.0).clamp(-32767.0, 32767.0) as i16);
            }
        }
        w.channels = out_ch as u16;
        w.pcm = pcm;
    }
    if w.vdec.is_some() {
        for k in 0..w.vin.len() {
            let pts = w.vin[k].pts;
            let at = w.pending.partition_point(|&p| p < pts);
            w.pending.insert(at, pts);
            let ok = w.vdec.as_mut().is_some_and(|d| d.decode_sample(&w.vin[k].data).is_ok());
            if !ok {
                w.verrs += 1;
                if let Some(i) = w.pending.iter().position(|&p| p == pts) {
                    w.pending.remove(i);
                }
            }
            while let Some(f) = w.vdec.as_mut().and_then(|d| d.next_frame()) {
                take_frame(w, f);
            }
        }
        if w.flush {
            w.flush = false;
            if let Some(d) = w.vdec.as_mut() {
                d.flush();
            }
            while let Some(f) = w.vdec.as_mut().and_then(|d| d.next_frame()) {
                take_frame(w, f);
            }
        }
    }
    w.us = crate::arch::timer::now_us().saturating_sub(t0);
}

unsafe fn work_trampoline(ctx: usize, _i: usize) {
    work_run(&mut *(ctx as *mut Work));
}

// ── the players ──────────────────────────────────────────────────────────────

struct Player {
    id: u32,
    url: String,
    /// -1 error, 0 loading, 1 ready, 2 ended
    state: i8,
    err: String,
    note: String,
    closed: bool,
    dur: f64,
    w: u32,
    h: u32,
    video: Option<Track>,
    audio: Option<Track>,
    /// Compressed pictures (decode order) and sound frames waiting for the decoder.
    vq: VecDeque<Sample>,
    aq: VecDeque<Vec<u8>>,
    /// Decoded pictures, in display order.
    frames: VecDeque<(f64, image::Image)>,
    frame_img: f64,
    shown: bool,
    /// which picture goes out when, and how even that is (avpace.rs)
    pace: crate::drivers::avpace::Pace,
    /// The streamed sound, the media time of its first frame, and its rate.
    stream: Option<usize>,
    stream_t0: f64,
    rate: u32,
    /// Decoded sound still to drop (seconds; after a seek into a segment).
    a_skip: f64,
    /// The stream sounds on the music voice.
    voice_on: bool,
    /// Another program took the music voice: the clock goes by the wall.
    no_voice: bool,
    /// All of the video's segments are fetched.
    v_done: bool,
    /// The decoder was told the pictures ended.
    flushed: bool,
    a_done: bool,
    playing: bool,
    /// The clock: `t` at wall time `t_ms` (paused: just `t`).
    t: f64,
    t_ms: u64,
    vol: f32,
    /// Bumped by each seek: work for an older position is thrown away.
    gen: u32,
    /// A seek the fetcher has not acted on yet.
    seek: Option<f64>,
    /// Decoders reset at the next batch (after a seek).
    reset: bool,
    fetch_task: u64,
    decode_task: u64,
    work: *mut Work,
    /// A batch is on a worker core.
    in_flight: bool,
    use_worker: bool,
    /// Decoding time (us) and pictures, for the console.
    dec_us: u64,
    dec_frames: u64,
    // ── streaming ──
    /// Where the last seek went (each track's fetcher re-aims there when `gen` moves).
    seek_t: f64,
    /// The sound stream is expected (YouTube offered one) / its opening failed.
    audio_expected: bool,
    audio_failed: bool,
    /// A newly opened sound stream's setup for the decoder (applied between batches).
    asc_new: Option<Vec<u8>>,
    /// The picture stream's quality ladder (lowest first) and the rung playing.
    ladder: Vec<yt::Format>,
    rung: usize,
    /// Quality switches queued behind the pictures already fetched: (avcC, width, height), one per
    /// marker sample (`pts` NaN, no data) in `vq`; the decoder is flushed, then re-made, there.
    v_switches: VecDeque<(Vec<u8>, u32, u32)>,
    switch_flushed: bool,
    /// Measured download speed (bytes/s, smoothed) and the read-ahead it buys (seconds).
    tp_bps: f64,
    ahead_s: f64,
    /// Segments fetched since the last quality change (a step up waits for two).
    rung_segs: u32,
    /// When the video was opened and when its first picture could show (ms; 0 = not yet).
    open_ms: u64,
    first_ms: u64,
    audio_task: u64,
}

static mut PLAYERS: Vec<Player> = Vec::new();
static mut NEXT_PLAYER: u32 = 1;

fn players() -> &'static mut Vec<Player> {
    unsafe { &mut *(&raw mut PLAYERS) }
}

/// Whether a YouTube player exists (it may have work on the worker arena).
pub fn busy() -> bool {
    !players().is_empty()
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

fn set_note(pid: u32, m: &str) {
    if let Some(p) = by_id(pid) {
        p.note = m.to_string();
    }
}

/// New stream addresses for a playing video (they are IP-bound and expire): the same itags from a
/// fresh /player answer, from any client. False when YouTube no longer offers those streams.
fn refresh_urls(pid: u32, id: &str) -> bool {
    let Some(s) = resolve(id, &|_m| {}).ok() else { return false };
    let Some(p) = live(pid) else { return false };
    let mut ok = true;
    if let Some(v) = p.video.as_mut() {
        match s.video.as_ref().filter(|f| f.itag == v.fmt.itag) {
            Some(f) => v.fmt.url = f.url.clone(),
            None => ok = false,
        }
    }
    if let Some(a) = p.audio.as_mut() {
        match s.audio.as_ref().filter(|f| f.itag == a.fmt.itag) {
            Some(f) => a.fmt.url = f.url.clone(),
            None => ok = false,
        }
    }
    log(&format!("{id}: fresh stream addresses via {}{}", s.client, if ok { "" } else { " (the streams changed)" }));
    ok
}

/// The fetching task: find the streams, then keep segments coming ahead of the clock.
fn fetch_task(pid: u32) {
    let Some(p) = by_id(pid) else { return };
    let url = p.url.clone();
    let alive = move || live(pid).is_some();
    let Some(id) = yt::video_id(&url) else {
        return fail(pid, "LingOS plays YouTube videos (this is not a YouTube address)".to_string());
    };
    // each client in turn until one's stream addresses actually serve (a client that now needs a
    // PO token still answers /player, but its streams say 403). The picture starts LOW -- the
    // first rung of at least START_H -- so the first segment is small and arrives fast; the
    // picture fetcher steps up as the measured speed allows (`adapt`).
    let mut skip: Vec<&'static str> = Vec::new();
    let (streams, vt, ladder, rung) = loop {
        let streams = match resolve_skip(&id, &|m| set_note(pid, m), &skip) {
            Ok(s) => s,
            Err(e) => return fail(pid, format!("YouTube: {e}")),
        };
        if streams.live {
            return fail(pid, "live streams do not play on LingOS yet".to_string());
        }
        let Some(best) = streams.video.clone() else {
            return fail(pid, "YouTube offered no H.264 picture stream LingOS can play for this video".to_string());
        };
        let ladder: Vec<yt::Format> = if streams.ladder.is_empty() { alloc::vec![best] } else { streams.ladder.clone() };
        let rung = ladder.iter().position(|f| f.height >= START_H).unwrap_or(ladder.len() - 1);
        let vfmt = ladder[rung].clone();
        log(&format!(
            "{id}: starting at {} via {} (ladder {}), sound {}; streams {} MB at the top rung + {} MB sound",
            yt::short(&vfmt),
            streams.client,
            ladder.iter().map(|f| format!("{}p", f.height)).collect::<Vec<_>>().join(" "),
            streams.audio.as_ref().map(yt::short).unwrap_or_else(|| "none".to_string()),
            ladder.last().map(|f| f.len >> 20).unwrap_or(0),
            streams.audio.as_ref().map(|f| f.len >> 20).unwrap_or(0)
        ));
        set_note(pid, "opening the video stream");
        // the sound opens alongside on its own task (and its own connection) as soon as the
        // client is known, rather than after the picture
        if let (Some(af), Some(p)) = (streams.audio.clone(), live(pid)) {
            if p.audio_task == 0 {
                p.audio_expected = true;
                let idc = id.clone();
                p.audio_task = crate::coro::spawn("horizon-video-snd", false, Box::new(move || audio_task(pid, af, idc)));
                if p.audio_task == 0 {
                    p.audio_failed = true;
                }
            }
        }
        match open_track(vfmt, &alive) {
            Ok(t) => break (streams, t, ladder, rung),
            Err(e) => {
                log(&format!("{}: video stream: {e}", streams.client));
                let refused = e.contains("HTTP 4");
                if refused && skip.len() + 1 < yt::CLIENTS.len() {
                    skip.push(streams.client);
                    continue;
                }
                return fail(pid, format!("YouTube video stream ({} client): {e}", streams.client));
            },
        }
    };
    let Some(p) = live(pid) else { return };
    p.dur = if streams.duration > 0.0 { streams.duration } else { vt.seg_end(vt.segs.len() - 1, 0.0) };
    p.w = vt.fmt.width;
    p.h = vt.fmt.height;
    if vt.init.avcc.is_empty() {
        return fail(pid, "the video stream has no H.264 setup (avcC)".to_string());
    }
    unsafe {
        (*p.work).avcc = vt.init.avcc.clone();
    }
    p.video = Some(vt);
    p.ladder = ladder;
    p.rung = rung;
    set_ahead(p);
    p.note = "buffering".to_string();
    track_loop(pid, true, &id);
}

/// The sound's own fetcher: open its stream (in parallel with the picture's), then keep its
/// segments coming -- two connections, so neither stream waits behind the other's 2 MiB pieces.
fn audio_task(pid: u32, fmt: yt::Format, id: String) {
    let alive = move || live(pid).is_some();
    match open_track(fmt, &alive) {
        Ok(t) => {
            let Some(p) = live(pid) else { return };
            p.asc_new = Some(t.init.asc.clone());
            p.audio = Some(t);
            set_ahead(p);
        },
        Err(e) => {
            log(&format!("sound stream: {e}; playing without sound"));
            if let Some(p) = live(pid) {
                p.audio_failed = true;
            }
            return;
        },
    }
    track_loop(pid, false, &id);
}

/// Bytes of compressed media kept fetched ahead (the read-ahead in seconds follows from the
/// streams' bitrates, between AHEAD_S and AHEAD_MAX_S).
const BUFFER_BYTES: f64 = 8.0 * 1048576.0;
const AHEAD_MAX_S: f64 = 30.0;
/// The picture starts on the first rung at least this tall.
const START_H: u32 = 360;

fn set_ahead(p: &mut Player) {
    let vb = p.video.as_ref().map(|v| v.fmt.bitrate).unwrap_or(0);
    let ab = p.audio.as_ref().map(|a| a.fmt.bitrate).unwrap_or(0);
    let bps = (vb + ab) as f64 / 8.0;
    p.ahead_s = if bps > 0.0 { (BUFFER_BYTES / bps).clamp(AHEAD_S, AHEAD_MAX_S) } else { AHEAD_S };
}

fn track<'a>(p: &'a mut Player, is_v: bool) -> Option<&'a mut Track> {
    if is_v { p.video.as_mut() } else { p.audio.as_mut() }
}

/// One stream's fetcher: segments in order, `ahead_s` ahead of the clock; a seek (a new `gen`)
/// re-aims it at the segment under the new time.
fn track_loop(pid: u32, is_v: bool, id: &str) {
    let what = if is_v { "picture" } else { "sound" };
    let mut my_gen = u32::MAX;
    let mut refreshes = 0u32;
    loop {
        let Some(p) = live(pid) else { return };
        if p.gen != my_gen {
            my_gen = p.gen;
            let t = p.seek_t;
            let Some(tr) = track(p, is_v) else { return };
            tr.next = tr.seg_at(t);
            tr.ahead = tr.segs[tr.next].t0;
            let seg_t0 = tr.ahead;
            if is_v {
                p.v_done = false;
                p.flushed = false;
                p.rung_segs = 0;
            } else {
                p.a_done = false;
                // the sound restarts at the segment start; what lies before the seek is dropped
                p.a_skip = (t - seg_t0).max(0.0);
            }
        }
        let t = clock(p);
        let ahead_s = p.ahead_s;
        let Some(tr) = track(p, is_v) else { return };
        if tr.next >= tr.segs.len() || tr.ahead > t + ahead_s {
            crate::coro::sleep_ms(150);
            continue;
        }
        if is_v && adapt(pid, t) {
            continue; // switched (or tried): look again
        }
        let Some(p) = live(pid) else { return };
        let gen = p.gen;
        if gen != my_gen {
            continue;
        }
        let Some(tr) = track(p, is_v) else { return };
        let (url, seg, idx) = (tr.fmt.url.clone(), tr.segs[tr.next], tr.next);
        let t0 = crate::arch::timer::now_us();
        let bytes = get_range(&url, seg.off, seg.off + seg.size - 1, &|| live(pid).is_some_and(|p| p.gen == gen));
        let us = crate::arch::timer::now_us().saturating_sub(t0);
        let Some(p) = live(pid) else { return };
        if p.gen != gen {
            continue; // a seek came in meanwhile
        }
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => {
                log(&format!("segment {idx} ({what}): {e}"));
                if e.contains("HTTP 403") || e.contains("HTTP 404") || e.contains("HTTP 410") {
                    // the addresses expired or were withdrawn: ask YouTube for fresh ones (same streams)
                    refreshes += 1;
                    if refreshes > 3 || !refresh_urls(pid, id) {
                        return fail(pid, format!("YouTube stopped serving the video ({e})"));
                    }
                    continue;
                }
                crate::coro::sleep_ms(1000);
                continue;
            },
        };
        // the picture's speed is the one that matters for the quality (sound is a small stream)
        if is_v && us > 0 && bytes.len() >= 64 * 1024 {
            let s = bytes.len() as f64 * 1_000_000.0 / us as f64;
            p.tp_bps = if p.tp_bps <= 0.0 { s } else { 0.6 * p.tp_bps + 0.4 * s };
        }
        let dur = p.dur;
        let Some(tr) = track(p, is_v) else { return };
        let samples = mp4::parse_fragment(&mut tr.init, &bytes, seg.off);
        drop(bytes);
        tr.next += 1;
        tr.ahead = tr.seg_end(idx, dur);
        let done = tr.next >= tr.segs.len();
        if is_v {
            p.vq.extend(samples);
            p.v_done = done;
            p.rung_segs += 1;
        } else {
            p.aq.extend(samples.into_iter().map(|s| s.data));
            p.a_done = done;
        }
        crate::coro::yield_now();
    }
}

/// Step the picture's quality up when the measured speed has room for the next rung (with 60 %
/// to spare, two segments in and 6 s fetched ahead), down when the read-ahead runs low on a
/// speed the rung does not fit. True when a switch was made or tried.
fn adapt(pid: u32, t: f64) -> bool {
    let Some(p) = live(pid) else { return false };
    if p.ladder.len() < 2 || p.tp_bps <= 0.0 {
        return false;
    }
    let Some(v) = p.video.as_ref() else { return false };
    let abr = p.audio.as_ref().map(|a| a.fmt.bitrate).unwrap_or(128_000) as f64;
    let buffered = v.ahead - t;
    let have = p.tp_bps * 8.0;
    let need = |r: usize| p.ladder[r].bitrate as f64 + abr;
    let cur = p.rung;
    let to = if cur + 1 < p.ladder.len() && p.rung_segs >= 2 && buffered >= 6.0 && have >= 1.6 * need(cur + 1) {
        cur + 1
    } else if cur > 0 && p.playing && p.state == 1 && buffered < 3.0 && have < 1.1 * need(cur) {
        cur - 1
    } else {
        return false;
    };
    switch_rung(pid, to)
}

/// Continue the picture on rung `to` from the segment boundary the fetching has reached: the new
/// stream's init + index, a marker in the picture queue where the decoder must change setup.
fn switch_rung(pid: u32, to: usize) -> bool {
    let Some(p) = live(pid) else { return false };
    let fmt = p.ladder[to].clone();
    let gen = p.gen;
    let from_t = p.video.as_ref().map(|v| v.ahead).unwrap_or(0.0);
    let (old_h, mbps) = (p.video.as_ref().map(|v| v.fmt.height).unwrap_or(0), p.tp_bps * 8.0 / 1e6);
    let alive = move || live(pid).is_some_and(|p| p.gen == gen);
    let nt = match open_track(fmt, &alive) {
        Ok(t) => t,
        Err(e) => {
            log(&format!("quality {}p: {e}", p.ladder.get(to).map(|f| f.height).unwrap_or(0)));
            if let Some(p) = live(pid) {
                p.rung_segs = 0; // not again right away
            }
            return true;
        },
    };
    let Some(p) = live(pid) else { return false };
    if p.gen != gen || nt.init.avcc.is_empty() {
        return true; // a seek came in (the fetcher re-aims the old stream) / unusable
    }
    let Some(k) = nt.segs.iter().position(|s| s.t0 >= from_t - 0.05) else { return false };
    let mut nt = nt;
    nt.next = k;
    nt.ahead = nt.segs[k].t0;
    p.vq.push_back(Sample { pts: f64::NAN, key: true, data: Vec::new() });
    p.v_switches.push_back((nt.init.avcc.clone(), nt.fmt.width, nt.fmt.height));
    log(&format!("quality {old_h}p -> {}p at {:.1} s ({mbps:.1} Mbit/s measured)", nt.fmt.height, nt.ahead));
    p.video = Some(nt);
    p.rung = to;
    p.rung_segs = 0;
    set_ahead(p);
    true
}

fn quality(p: &mut Player) -> String {
    let h = p.video.as_ref().map(|v| v.fmt.height).unwrap_or(0);
    if h == 0 {
        return String::new();
    }
    if p.tp_bps > 0.0 { format!("{h}p \u{b7} {:.1} Mbit/s", p.tp_bps * 8.0 / 1e6) } else { format!("{h}p") }
}

fn is_marker(s: &Sample) -> bool {
    s.pts.is_nan() && s.data.is_empty()
}

/// Media time up to which both streams are fetched (the seek bar's buffered part).
fn buffered_to(p: &Player) -> f64 {
    let end = |tr: &Track, done: bool| if done { p.dur } else { tr.ahead };
    let v = p.video.as_ref().map(|v| end(v, p.v_done)).unwrap_or(p.t);
    let a = match p.audio.as_ref() {
        Some(a) => end(a, p.a_done),
        None if p.audio_expected && !p.audio_failed => p.t,
        None => v,
    };
    v.min(a).max(p.t).min(if p.dur > 0.0 { p.dur } else { f64::MAX })
}

/// Waiting on the network: loading, or playing with nothing decoded ahead of the clock.
fn buffering(p: &mut Player) -> bool {
    if p.state == 0 {
        return true;
    }
    if p.state != 1 || !p.playing {
        return false;
    }
    let t = clock(p);
    let snd_ahead = match p.stream {
        Some(s) if p.rate > 0 && p.voice_on => p.stream_t0 + sfx::stream_end(s) as f64 / p.rate as f64 - t,
        _ => if p.frames.is_empty() { 0.0 } else { 1.0 },
    };
    let ended = p.v_done && (p.a_done || p.audio.is_none()) && p.vq.is_empty() && p.aq.is_empty();
    !ended && snd_ahead < 0.15
}

/// Hand the decoders their next batch, and take what they made.
fn decode_task(pid: u32) {
    loop {
        let Some(p) = by_id(pid) else { return };
        if p.closed && !p.in_flight {
            // closed: free it all once the fetching task has seen it too
            if p.fetch_task == 0 || !crate::coro::alive(p.fetch_task) {
                return finish(pid);
            }
            crate::coro::sleep_ms(20);
            continue;
        }
        if p.state < 0 && !p.in_flight {
            return; // os_video_close frees it
        }
        let w = unsafe { &mut *p.work };
        if p.in_flight {
            #[cfg(target_arch = "x86_64")]
            let done = crate::arch::smp::bg_finish();
            #[cfg(not(target_arch = "x86_64"))]
            let done = true;
            if !done {
                crate::coro::sleep_ms(2);
                continue;
            }
            p.in_flight = false;
            collect(p);
            continue;
        }
        if p.video.is_none() {
            crate::coro::sleep_ms(50);
            continue;
        }
        // the next batch
        // the sound stream opened (on its own task): its decoder setup
        if let Some(asc) = p.asc_new.take() {
            w.asc = asc;
            w.reset_a = true;
        }
        if p.reset {
            p.reset = false;
            w.reset_v = true;
            w.reset_a = true;
            w.skip_before = p.t;
            // after a seek the decoder starts on the stream the fetcher plays now (the quality may
            // have changed since it was made)
            if let Some(v) = p.video.as_ref() {
                w.avcc = v.init.avcc.clone();
            }
        }
        w.gen = p.gen;
        w.vin.clear();
        w.ain.clear();
        // the decoder fell well behind the clock: go on from the next key frame (never past a
        // quality switch: the pictures after it need the other setup)
        let t = clock(p);
        if p.playing && p.vq.front().is_some_and(|s| !is_marker(s) && s.pts < t - 1.5) {
            let stop = p.vq.iter().position(is_marker).unwrap_or(p.vq.len());
            if let Some(k) = p.vq.iter().take(stop).position(|s| s.key && s.pts > t - 0.5) {
                p.vq.drain(..k);
                p.frames.retain(|f| f.0 >= t - 0.5);
            } else if stop < p.vq.len() && p.vq.iter().take(stop).all(|s| s.pts < t - 0.5) {
                // everything before a quality switch is behind the clock: drop it up to the switch
                // (stopping short of it left the queue growing behind it -- 5000 pictures in a test)
                p.vq.drain(..stop);
                p.frames.retain(|f| f.0 >= t - 0.5);
            }
        }
        // a switch at the front with only stale pictures ready: they go, so the decoder moves on
        if p.playing && p.vq.front().is_some_and(is_marker) && p.frames.iter().all(|f| f.0 < t - 0.5) {
            p.frames.clear();
        }
        if p.v_done && p.vq.is_empty() && !p.flushed {
            p.flushed = true;
            w.flush = true;
        }
        let room = FRAMES_READY.saturating_sub(p.frames.len());
        let per_batch = if p.use_worker { room + 2 } else { 1 };
        if room > 0 {
            for _ in 0..per_batch {
                if p.vq.front().is_some_and(is_marker) {
                    break; // the batch ends at a quality switch
                }
                match p.vq.pop_front() {
                    Some(s) => w.vin.push(s),
                    None => break,
                }
            }
        }
        // a quality switch at the front: first a batch that hands out the pictures the decoder
        // still holds back (flush), then the decoder is made again on the new stream's setup
        if w.vin.is_empty() && p.vq.front().is_some_and(is_marker) {
            if !p.switch_flushed {
                p.switch_flushed = true;
                w.flush = true;
            } else {
                p.vq.pop_front();
                p.switch_flushed = false;
                if let Some((avcc, sw, sh)) = p.v_switches.pop_front() {
                    w.avcc = avcc;
                    w.reset_v = true;
                    p.w = sw;
                    p.h = sh;
                }
            }
        }
        // sound: what fills the stream to a few seconds ahead of the clock
        let have_to = match p.stream {
            Some(s) if p.rate > 0 => p.stream_t0 + sfx::stream_end(s) as f64 / p.rate as f64,
            _ => t,
        };
        if have_to < t + 4.0 {
            let n = if p.use_worker { 96 } else { 12 };
            for _ in 0..n {
                match p.aq.pop_front() {
                    Some(a) => w.ain.push(a),
                    None => break,
                }
            }
        }
        if w.vin.is_empty() && w.ain.is_empty() && !w.reset_v && !w.flush {
            maybe_ready(p);
            crate::coro::sleep_ms(8);
            continue;
        }
        if p.use_worker {
            #[cfg(target_arch = "x86_64")]
            {
                if crate::arch::smp::bg_start(work_trampoline, p.work as usize) {
                    p.in_flight = true;
                } else {
                    // the slot is busy (a password hash): put the batch back and wait
                    for s in w.vin.drain(..).rev() {
                        p.vq.push_front(s);
                    }
                    for a in w.ain.drain(..).rev() {
                        p.aq.push_front(a);
                    }
                    if w.flush {
                        w.flush = false;
                        p.flushed = false;
                        if p.vq.front().is_some_and(is_marker) {
                            p.switch_flushed = false; // the switch's flush goes again
                        }
                    }
                    crate::coro::sleep_ms(5);
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
    if let Some(f) = w.fatal.take() {
        log(f);
        if w.vdec.is_none() && !w.avcc.is_empty() {
            p.state = -1;
            p.err = f.to_string();
            return;
        }
    }
    p.dec_us += w.us;
    if w.gen != p.gen {
        return; // for a position the video has left
    }
    p.dec_frames += frames.len() as u64;
    for (pts, img) in frames {
        let at = p.frames.iter().position(|(q, _)| *q > pts).unwrap_or(p.frames.len());
        p.frames.insert(at, (pts, img));
    }
    if !pcm.is_empty() && w.rate > 0 {
        let ch = w.channels.max(1) as usize;
        let mut pcm = &pcm[..];
        if p.stream.is_none() {
            p.rate = w.rate;
            p.stream = Some(sfx::stream_open(w.rate, ch == 2));
            p.stream_t0 = p.audio.as_ref().map(|a| a.segs[a.seg_at(p.t)].t0 + p.a_skip).unwrap_or(p.t);
            p.voice_on = false;
        }
        if p.a_skip > 0.0 {
            let drop_frames = ((p.a_skip * w.rate as f64) as usize).min(pcm.len() / ch);
            pcm = &pcm[drop_frames * ch..];
            p.a_skip -= drop_frames as f64 / w.rate as f64;
            if p.a_skip < 1.0 / w.rate as f64 {
                p.a_skip = 0.0;
            }
        }
        if let Some(s) = p.stream {
            sfx::stream_push(s, pcm);
            // the sound well behind the clock goes
            let behind = p.t - p.stream_t0 - KEEP_BEHIND_S;
            if behind > 10.0 {
                sfx::stream_trim(s, (behind * p.rate as f64) as usize);
            }
        }
    }
    // the last of the sound is in: a voice on it ends at its end
    if p.a_done && p.aq.is_empty() {
        if let Some(s) = p.stream {
            sfx::stream_close(s);
        }
    }
    maybe_ready(p);
}

/// Loading -> ready once the first picture and a second of sound are in.
fn maybe_ready(p: &mut Player) {
    if p.state != 0 || p.video.is_none() {
        return;
    }
    let pic = !p.frames.is_empty();
    // the sound opens on its own task: wait for it when YouTube offered one (its first second)
    let snd = (!p.audio_expected && p.audio.is_none())
        || p.audio_failed
        || p.no_voice
        || match p.stream {
            Some(s) if p.rate > 0 => (sfx::stream_end(s) as f64 / p.rate as f64) >= 1.0 || (p.a_done && p.aq.is_empty()),
            _ => false,
        };
    if pic && snd {
        p.state = 1;
        p.note.clear();
        p.t_ms = now_ms();
        if p.first_ms == 0 {
            p.first_ms = now_ms().saturating_sub(p.open_ms).max(1);
            log(&format!("ready to play {} ms after opening ({}p)", p.first_ms, p.h));
        }
    }
}

/// Free what a closed player holds and forget it.
fn finish(pid: u32) {
    let Some(i) = players().iter().position(|p| p.id == pid) else { return };
    let p = &mut players()[i];
    if p.in_flight {
        // a worker core still decodes into it: wait for it (a batch is short)
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
    if p.frame_img >= 0.0 {
        compat::image_release(p.frame_img);
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

/// Where the video is (seconds). While playing with sound, the music voice is
/// the clock (it holds while the sound runs dry); otherwise the wall clock.
fn clock(p: &mut Player) -> f64 {
    if !p.playing || p.state != 1 {
        return p.t;
    }
    if p.audio.is_some() && !p.no_voice {
        if p.voice_on {
            if p.stream.is_some() && sfx::music_sample() == p.stream {
                p.t = p.stream_t0 + sfx::music_pos();
            } else {
                p.voice_on = false;
                let at_end = p.a_done && p.aq.is_empty() && p.stream.is_some_and(|s| p.rate > 0 && p.t >= p.stream_t0 + sfx::stream_end(s) as f64 / p.rate as f64 - 0.2);
                if at_end {
                    p.t = p.t.max(p.dur);
                } else {
                    // another program took the music voice: go on by the wall clock
                    p.no_voice = true;
                    p.t_ms = now_ms();
                }
            }
        }
        if !p.voice_on && !p.no_voice && p.t < p.dur - 0.05 {
            if let Some(s) = p.stream {
                if sfx::play(s, p.vol, 0.0, false, 1.0, true).is_some() {
                    sfx::seek_music(p.t - p.stream_t0);
                    p.voice_on = true;
                }
            }
            // no sound decoded yet: the clock holds
        }
    } else {
        let now = now_ms();
        p.t += (now - p.t_ms) as f64 / 1000.0;
        p.t_ms = now;
    }
    if p.dur > 0.0 && p.t >= p.dur - 0.05 {
        p.t = p.dur;
        p.playing = false;
        p.state = 2;
        stop_sound(p);
    }
    p.t
}

/// Players whose task is gone (Horizon quit): silence them and free what they hold.
fn reap_players() {
    let dead: Vec<u32> = players().iter().filter(|p| p.fetch_task != 0 && !crate::coro::alive(p.fetch_task) && !crate::coro::alive(p.decode_task)).map(|p| p.id).collect();
    for id in dead {
        finish(id);
    }
}

pub fn open(url: &str) -> f64 {
    crate::coro::on_kill(reap_players);
    if players().len() >= 4 {
        let id = players()[0].id;
        close(id as f64);
    }
    let id = unsafe {
        let id = NEXT_PLAYER;
        NEXT_PLAYER += 1;
        id
    };
    #[cfg(target_arch = "x86_64")]
    // the worker arena: 720p keeps up to 16 reference pictures plus a batch of 4 MiB RGB pictures
    // in power-of-two blocks -- 64 MiB ran out on 1280x720 (an allocation failure panicked the
    // worker core); the media player's 256 MiB is the same arena
    let use_worker = crate::arch::smp::has_workers() && crate::mm::aparena::activate(256 << 20);
    #[cfg(not(target_arch = "x86_64"))]
    let use_worker = false;
    let work = Box::into_raw(Box::new(Work::new()));
    players().push(Player {
        id,
        url: url.to_string(),
        state: 0,
        err: String::new(),
        note: "starting".to_string(),
        closed: false,
        dur: 0.0,
        w: 0,
        h: 0,
        video: None,
        audio: None,
        vq: VecDeque::new(),
        aq: VecDeque::new(),
        frames: VecDeque::new(),
        frame_img: -1.0,
        shown: false,
        pace: Default::default(),
        stream: None,
        stream_t0: 0.0,
        rate: 0,
        a_skip: 0.0,
        voice_on: false,
        no_voice: false,
        v_done: false,
        flushed: false,
        a_done: false,
        playing: false,
        t: 0.0,
        t_ms: now_ms(),
        vol: 1.0,
        gen: 0,
        seek: None,
        reset: false,
        fetch_task: 0,
        decode_task: 0,
        work,
        in_flight: false,
        use_worker,
        dec_us: 0,
        dec_frames: 0,
        seek_t: 0.0,
        audio_expected: false,
        audio_failed: false,
        asc_new: None,
        ladder: Vec::new(),
        rung: 0,
        v_switches: VecDeque::new(),
        switch_flushed: false,
        tp_bps: 0.0,
        ahead_s: AHEAD_S,
        rung_segs: 0,
        open_ms: now_ms(),
        first_ms: 0,
        audio_task: 0,
    });
    let ft = crate::coro::spawn("horizon-video", false, Box::new(move || fetch_task(id)));
    let dt = if ft != 0 { crate::coro::spawn("horizon-video-dec", false, Box::new(move || decode_task(id))) } else { 0 };
    if let Some(p) = by_id(id) {
        p.fetch_task = ft;
        p.decode_task = dt;
        if ft == 0 || dt == 0 {
            p.state = -1;
            p.err = "no memory for the video's tasks".to_string();
        }
    }
    id as f64
}

/// The picture for now: the newest decoded one at or before the clock.
fn frame(p: &mut Player) -> f64 {
    let t = clock(p);
    // the picture for when this frame is on screen, on the smoothed audio clock (avpace.rs)
    let mut tp = p.pace.target(t, p.playing && p.state == 1);
    if crate::drivers::avpace::legacy() {
        tp = t + 0.003;
    }
    let mut pick: Option<(f64, image::Image)> = None;
    while let Some((pts, _)) = p.frames.front() {
        if *pts <= tp + 0.002 || (!p.shown && pick.is_none()) {
            pick = p.frames.pop_front();
            p.shown = true;
        } else {
            break;
        }
    }
    if let Some((pts, img)) = pick {
        p.pace.shown(pts, tp);
        p.w = img.w;
        p.h = img.h;
        p.frame_img = match compat::image_set(p.frame_img, img) {
            Ok(()) => p.frame_img,
            Err(img) => compat::image_push(img),
        };
    }
    p.frame_img
}

fn seek(p: &mut Player, t: f64) {
    let t = if p.dur > 0.0 { t.clamp(0.0, (p.dur - 0.1).max(0.0)) } else { t.max(0.0) };
    stop_sound(p);
    if let Some(s) = p.stream.take() {
        sfx::release(s);
    }
    p.t = t;
    p.t_ms = now_ms();
    p.gen = p.gen.wrapping_add(1);
    p.seek = Some(t);
    p.seek_t = t;
    p.reset = true;
    // queued quality switches go with the pictures they stood between
    p.v_switches.clear();
    p.switch_flushed = false;
    p.vq.clear();
    p.aq.clear();
    p.frames.clear();
    p.shown = false;
    p.no_voice = false;
    // back to loading until a picture and a second of sound are in at the new place (maybe_ready):
    // the clock holds meanwhile and Horizon shows the spinner. Sound alone used to run on over a
    // still picture for seconds after a seek (the clock follows the sound).
    if p.state >= 1 {
        p.state = 0;
        p.note = "buffering".to_string();
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
        // the tasks end themselves when they next look; with none alive, free it here
        if dead(ft) && dead(dt) {
            finish(pid);
        }
    }
}

/// The `os_video_*` builtins (netmedia.rs routes them here).
pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    use crate::lingrt::{boolean, mk_str, num, to_f64, to_string, UNIT};
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let n = |i: usize, d: f64| args.get(i).map(|&v| to_f64(v)).unwrap_or(d);
    Some(match name {
        b"os_video_open" => num(open(&s(0))),
        b"os_video_state" => num(player(n(0, -1.0))
            .map(|p| {
                if p.state == 1 && p.playing {
                    clock(p);
                }
                p.state as f64
            })
            .unwrap_or(-1.0)),
        b"os_video_error" => mk_str(&player(n(0, -1.0)).map(|p| p.err.clone()).unwrap_or_else(|| "no such video".to_string())),
        b"os_video_note" => mk_str(&player(n(0, -1.0)).map(|p| p.note.clone()).unwrap_or_default()),
        b"os_video_frame" => num(player(n(0, -1.0)).filter(|p| p.state >= 1).map(frame).unwrap_or(-1.0)),
        b"os_video_width" => num(player(n(0, -1.0)).map(|p| p.w as f64).unwrap_or(0.0)),
        b"os_video_height" => num(player(n(0, -1.0)).map(|p| p.h as f64).unwrap_or(0.0)),
        b"os_video_duration" => num(player(n(0, -1.0)).map(|p| p.dur).unwrap_or(0.0)),
        b"os_video_time" => num(player(n(0, -1.0)).map(clock).unwrap_or(0.0)),
        b"os_video_playing" => boolean(player(n(0, -1.0)).is_some_and(|p| p.playing)),
        b"os_video_play" => {
            if let Some(p) = player(n(0, -1.0)) {
                play(p);
            }
            UNIT
        },
        b"os_video_pause" => {
            if let Some(p) = player(n(0, -1.0)) {
                pause(p);
            }
            UNIT
        },
        b"os_video_seek" => {
            if let Some(p) = player(n(0, -1.0)) {
                seek(p, n(1, 0.0));
            }
            UNIT
        },
        b"os_video_volume" => {
            if let Some(p) = player(n(0, -1.0)) {
                p.vol = n(1, 1.0).clamp(0.0, 1.0) as f32;
                if p.voice_on {
                    // the voice keeps the volume it started with: start it again where it is
                    let t = clock(p);
                    stop_sound(p);
                    p.t = t;
                }
            }
            UNIT
        },
        b"os_video_close" => {
            close(n(0, -1.0));
            UNIT
        },
        // streaming: the media time the fetched part reaches (the seek bar's buffered stretch),
        // whether playback waits on the network, and "480p · 3.4 Mbit/s"
        b"os_video_buffered" => num(player(n(0, -1.0)).map(|p| buffered_to(p)).unwrap_or(0.0)),
        b"os_video_buffering" => boolean(player(n(0, -1.0)).is_some_and(buffering)),
        b"os_video_quality" => mk_str(&player(n(0, -1.0)).map(quality).unwrap_or_default()),
        // downloads (Horizon's Download buttons)
        _ if name.starts_with(b"os_video_save") => return super::ytsave::dispatch(name, args),
        _ => return None,
    })
}

/// `video [open <youtube url> | close]` (SSH): play a video the way Horizon does (the tests'
/// hook), close them all, or -- with nothing -- what the players are doing.
pub fn command(arg: &str) -> String {
    let arg = arg.trim();
    if let Some(u) = arg.strip_prefix("open ") {
        let id = open(u.trim());
        if let Some(p) = player(id) {
            play(p);
        }
        return format!("video {id}: opening {}\n", u.trim());
    }
    if let Some(t) = arg.strip_prefix("seek ").and_then(|t| t.trim().parse::<f64>().ok()) {
        let ids: Vec<u32> = players().iter().filter(|p| !p.closed).map(|p| p.id).collect();
        for id in &ids {
            if let Some(p) = player(*id as f64) {
                seek(p, t);
                play(p);
            }
        }
        return format!("seeked {} video(s) to {t:.0} s\n", ids.len());
    }
    if arg == "close" {
        let ids: Vec<u32> = players().iter().map(|p| p.id).collect();
        for id in ids {
            close(id as f64);
        }
        return String::from("closed\n");
    }
    status()
}

/// The `video` command (SSH): what the players are doing.
pub fn status() -> String {
    let mut s = String::new();
    for p in players().iter() {
        let fps = if p.dec_us > 0 { p.dec_frames as f64 * 1e6 / p.dec_us as f64 } else { 0.0 };
        s.push_str(&format!(
            "video {}: state {} t {:.1}/{:.0}s {}x{} queued {}v/{}a ready {} decode {:.0} fps{}\n",
            p.id,
            p.state,
            p.t,
            p.dur,
            p.w,
            p.h,
            p.vq.len(),
            p.aq.len(),
            p.frames.len(),
            fps,
            if p.use_worker { " (worker core)" } else { "" }
        ));
        s.push_str(&format!("  {}\n", p.pace.report()));
        if !p.err.is_empty() {
            s.push_str(&format!("  error: {}\n", p.err));
        } else if p.state == 0 && !p.note.is_empty() {
            s.push_str(&format!("  {}\n", p.note));
        }
        s.push_str(&format!(
            "  stream: {} (ladder {}), buffered to {:.1}s (read-ahead {:.0}s), first picture after {}\n",
            if p.tp_bps > 0.0 { format!("{}p at {:.1} Mbit/s", p.video.as_ref().map(|v| v.fmt.height).unwrap_or(0), p.tp_bps * 8.0 / 1e6) } else { format!("{}p", p.video.as_ref().map(|v| v.fmt.height).unwrap_or(0)) },
            p.ladder.iter().map(|f| format!("{}p", f.height)).collect::<Vec<_>>().join("/"),
            buffered_to(p),
            p.ahead_s,
            if p.first_ms > 0 { format!("{} ms", p.first_ms) } else { String::from("(not yet)") }
        ));
    }
    if s.is_empty() {
        s.push_str("no video playing\n");
    }
    s
}

// ── the MP4 side, through ling-mp4 ───────────────────────────────────────────

pub(crate) mod mp4 {
    use super::{Sample, Seg};
    use alloc::vec::Vec;
    use ling_mp4::{Codec, Movie, Sidx};

    /// What a stream's init segment says.
    pub struct Init {
        pub movie: Movie,
        pub track_id: u32,
        pub timescale: u32,
        /// The first edit's media time (encoder delay / B-frame shift), taken off every time.
        pub shift: i64,
        /// H.264 decoder setup (the avcC box's body), or empty.
        pub avcc: Vec<u8>,
        /// AAC AudioSpecificConfig, or empty.
        pub asc: Vec<u8>,
    }

    pub fn parse_init(b: &[u8]) -> Option<Init> {
        let movie = Movie::parse_init(b).ok()?;
        let t = movie.tracks.iter().find(|t| matches!(t.codec, Codec::Avc { .. } | Codec::Mp4a { .. }))?;
        let (avcc, asc) = match &t.codec {
            Codec::Avc { avcc, .. } => (avcc.clone(), Vec::new()),
            Codec::Mp4a { asc, .. } => (Vec::new(), asc.clone()),
            _ => return None,
        };
        let (track_id, timescale, shift) = (t.id, t.timescale.max(1), t.edit_media_time.unwrap_or(0).max(0));
        Some(Init { movie, track_id, timescale, shift, avcc, asc })
    }

    /// Segments from the sidx box at the start of `b` (which starts at file offset `at`).
    pub fn parse_sidx(b: &[u8], at: u64, init: &Init) -> Option<Vec<Seg>> {
        let sidx = Sidx::parse(b, at).ok()?;
        let ts = sidx.timescale.max(1) as f64;
        let shift = init.shift as f64 / init.timescale as f64;
        let mut out = Vec::with_capacity(sidx.entries.len());
        for e in &sidx.entries {
            if e.is_sidx {
                return None; // a nested index: YouTube does not make them
            }
            out.push(Seg { off: e.offset, size: e.size as u64, t0: (e.start_time as f64 / ts - shift).max(0.0) });
        }
        Some(out)
    }

    /// The samples of the stream's track in one segment (moof+mdat pairs) at file offset `at`, in decode order.
    pub fn parse_fragment(init: &mut Init, b: &[u8], at: u64) -> Vec<Sample> {
        let mut out = Vec::new();
        let Ok(boxes) = ling_mp4::scan_boxes(b, 0) else { return out };
        let ts = init.timescale as f64;
        for bx in boxes.iter().filter(|bx| &bx.kind == b"moof") {
            let o = bx.offset as usize;
            let Ok(frag) = init.movie.parse_fragment(&b[o..], at + o as u64) else { continue };
            for s in frag.track_samples(init.track_id) {
                let start = o + s.offset as usize;
                let Some(data) = b.get(start..start + s.size as usize) else { continue };
                out.push(Sample { pts: (s.pts - init.shift) as f64 / ts, key: s.keyframe, data: data.to_vec() });
            }
        }
        out
    }
}
