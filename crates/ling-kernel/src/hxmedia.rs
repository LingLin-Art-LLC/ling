//! Horizon's chat media (the Discord app on LingOS, `discord.ling`): sound files
//! played inline, file uploads, and colour emoji.
//!
//! - `hx_audio_open(url, headers)` -> handle. A green thread fetches the file
//!   (https / http through LingSockets' `request_full`, or a local path through
//!   vfs / the data volumes) and decodes it whole into an sfx sample: MP3 / MP2
//!   (`ling-mpa`) and AAC in MP4 / M4A (`ling-mp4` + `ling-aac`) in pieces with
//!   a yield between them, WAV / Ogg Vorbis / FLAC through `sfx::load_wav`.
//!   Ogg Opus (Discord's voice messages) has no decoder in LingOS yet: the
//!   handle ends in an error that says so.
//!   `hx_audio_state(h)` "loading" | "decoding" | "ready" | "error: <why>";
//!   `hx_audio_play(h)` / `hx_audio_pause(h)` / `hx_audio_seek(h, s)` on the
//!   music voice (one sound at a time, like any music player), `hx_audio_pos`,
//!   `hx_audio_duration`, `hx_audio_playing`, `hx_audio_info`, `hx_audio_close`.
//! - `hx_upload_async(method, url, headers, path)` -> job id: the file's bytes as
//!   the request body (Discord's attachment upload is a PUT of the raw file);
//!   `hx_upload_poll(id)` -> "" while it runs, then "<status>\n<body>" (0 and
//!   "ERROR: ..." when it could not be made), streamed from disk; `hx_upload_stage(id)`
//!   "reading" / "sending <sent> <total>" meanwhile. `hx_file_size(path)` (-1 missing).
//! - `hx_emoji_html(text, base, cls)`: Unicode emoji in (already escaped) text
//!   become `<img>`s of Twemoji pictures (`base` + "1f600.png"); the kernel's
//!   fonts have no colour emoji. `hx_cp("1f44d-1f3fd")` -> those characters.

use crate::drivers::{mediafile, sfx};
use crate::lingrt::{boolean, mk_str, num, to_f64, to_string, UNIT};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

fn log(m: &str) {
    crate::console_write(format!("[hxmedia] {m}\n").as_bytes());
}

/// Biggest sound file fetched (Discord's own limit for most accounts is 10-25 MB).
const MAX_FILE: usize = 40 << 20;
/// Decoded PCM kept per sound: ~6 minutes of 48 kHz stereo (the heap's biggest block is 64 MiB).
const MAX_PCM: usize = 36 << 20;

struct Snd {
    h: u32,
    state: String,
    sample: Option<usize>,
    info: String,
    dur: f64,
    /// where it was paused / stopped (seconds)
    at: f64,
    started: bool,
    /// stopped by pause (not by reaching its end)
    paused: bool,
    closed: bool,
}

static mut SOUNDS: Vec<Snd> = Vec::new();
static mut NEXT: u32 = 1;

fn sounds() -> &'static mut Vec<Snd> {
    unsafe { &mut *(&raw mut SOUNDS) }
}

fn snd(h: f64) -> Option<&'static mut Snd> {
    let h = h as u32;
    sounds().iter_mut().find(|s| s.h == h && !s.closed)
}

fn by_h(h: u32) -> Option<&'static mut Snd> {
    sounds().iter_mut().find(|s| s.h == h)
}

/// The whole file: a URL over the network, anything else as a path.
fn fetch(url: &str, headers: &str, max: usize) -> Result<Vec<u8>, String> {
    let lu = url.to_ascii_lowercase();
    if lu.starts_with("https://") || lu.starts_with("http://") {
        let (st, _head, body) = crate::lingsock::request_full("GET", url, headers, &[], max)?;
        if !(200..300).contains(&st) {
            return Err(format!("HTTP {st}"));
        }
        return Ok(body);
    }
    let path = url.strip_prefix("file://").unwrap_or(url);
    let mut src = mediafile::open_source(path).ok_or_else(|| format!("no such file: {path}"))?;
    let b = src.read(0, max + 1).ok_or_else(|| format!("could not read {path}"))?;
    if b.len() > max {
        return Err(format!("the file is over {} MB", max >> 20));
    }
    Ok(b)
}

fn q16(v: f32) -> i16 {
    libm::roundf(v * 32767.0).clamp(-32767.0, 32767.0) as i16
}

/// f32 interleaved (any channels) -> i16 mono / stereo (front left / right).
fn push_pcm(f: &[f32], ch: usize, out: &mut Vec<i16>) {
    if ch <= 1 {
        out.extend(f.iter().map(|&v| q16(v)));
    } else {
        for fr in f.chunks_exact(ch) {
            out.push(q16(fr[0]));
            out.push(q16(fr[1]));
        }
    }
}

fn is_opus(b: &[u8]) -> bool {
    b.len() > 36 && &b[..4] == b"OggS" && b[..b.len().min(128)].windows(8).any(|w| w == b"OpusHead")
}

fn is_mp4(b: &[u8]) -> bool {
    b.len() >= 12 && matches!(&b[4..8], b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide")
}

/// MP3 in pieces (a yield between them, so the window keeps drawing).
fn decode_mp3(b: &[u8]) -> Result<(u32, u16, Vec<i16>), String> {
    let mut dec = ling_mpa::Decoder::new();
    let mut out: Vec<i16> = Vec::new();
    let mut f: Vec<f32> = Vec::new();
    for piece in b.chunks(48 * 1024) {
        f.clear();
        let _ = dec.decode(piece, &mut f);
        let ch = dec.channels().max(1) as usize;
        push_pcm(&f, ch, &mut out);
        if out.len() * 2 > MAX_PCM {
            return Err("the sound is too long to hold (over ~6 minutes)".to_string());
        }
        crate::coro::yield_now();
    }
    if out.is_empty() || dec.sample_rate() == 0 {
        return Err("no MPEG audio frames in it".to_string());
    }
    Ok((dec.sample_rate(), dec.channels().min(2).max(1), out))
}

/// AAC in an MP4 / M4A (moov at either end).
fn decode_m4a(b: &[u8]) -> Result<(u32, u16, Vec<i16>), String> {
    let boxes = ling_mp4::scan_boxes(b, 0).map_err(|_| "not an MP4 file".to_string())?;
    let moov = boxes.iter().find(|x| &x.kind == b"moov").and_then(|x| x.payload(b)).ok_or("the MP4 has no moov box")?;
    let movie = ling_mp4::Movie::parse_moov(moov).map_err(|_| "the MP4's moov could not be read".to_string())?;
    let tr = movie
        .tracks
        .iter()
        .find(|t| matches!(t.codec, ling_mp4::Codec::Mp4a { .. }))
        .ok_or("no AAC sound track in it")?;
    let ling_mp4::Codec::Mp4a { asc, .. } = &tr.codec else { return Err("no AAC sound track in it".to_string()) };
    let mut dec = ling_aac::AacDecoder::new(asc).map_err(|_| "this AAC kind is not supported".to_string())?;
    let mut out: Vec<i16> = Vec::new();
    let mut f: Vec<f32> = Vec::new();
    for (i, s) in tr.samples().enumerate() {
        let (o, n) = (s.offset as usize, s.size as usize);
        let Some(au) = b.get(o..o + n) else { break };
        f.clear();
        if dec.decode(au, &mut f).is_ok() {
            push_pcm(&f, dec.channels().max(1) as usize, &mut out);
        }
        if out.len() * 2 > MAX_PCM {
            return Err("the sound is too long to hold (over ~6 minutes)".to_string());
        }
        if i % 48 == 47 {
            crate::coro::yield_now();
        }
    }
    if out.is_empty() {
        return Err("no AAC frames decoded".to_string());
    }
    Ok((dec.sample_rate(), dec.channels().min(2).max(1), out))
}

fn load(h: u32, url: String, headers: String) {
    let res: Result<(usize, String), String> = (|| {
        let b = fetch(&url, &headers, MAX_FILE)?;
        if let Some(s) = by_h(h) {
            if s.closed {
                return Err("closed".to_string());
            }
            s.state = "decoding".to_string();
        }
        if is_opus(&b) {
            return Err("Opus sound (Discord voice messages) has no decoder in LingOS yet".to_string());
        }
        let kind = crate::codec::sniff(&b);
        let (pcm, what) = if kind == Some("mp3") {
            (decode_mp3(&b)?, "MP3")
        } else if is_mp4(&b) {
            (decode_m4a(&b)?, "AAC")
        } else if matches!(kind, Some("ogg") | Some("flac") | Some("wav")) {
            let id = sfx::own(sfx::load_wav(&b).ok_or("the sound could not be decoded")?);
            let what = match kind {
                Some("ogg") => "Vorbis",
                Some("flac") => "FLAC",
                _ => "WAV",
            };
            return Ok((id, what.to_string()));
        } else {
            return Err("not a sound format LingOS knows (MP3, M4A / AAC, Ogg Vorbis, FLAC, WAV)".to_string());
        };
        let (rate, ch, data) = pcm;
        let id = sfx::own(sfx::stream_open(rate, ch == 2));
        sfx::stream_push(id, &data);
        sfx::stream_close(id);
        Ok((id, format!("{what} {} Hz {}", rate, if ch == 2 { "stereo" } else { "mono" })))
    })();
    let Some(s) = by_h(h) else { return };
    match res {
        Ok((id, info)) => {
            if s.closed {
                sfx::release(id);
                return;
            }
            s.sample = Some(id);
            s.dur = sfx::duration(id);
            s.info = info;
            s.state = "ready".to_string();
            log(&format!("sound {h} ready: {} {:.2} s", s.info, s.dur));
        },
        Err(e) => {
            log(&format!("sound {h}: {e}"));
            s.state = format!("error: {e}");
        },
    }
}

fn audio_open(url: String, headers: String) -> f64 {
    let h = unsafe {
        NEXT += 1;
        NEXT
    };
    sounds().push(Snd { h, state: "loading".to_string(), sample: None, info: String::new(), dur: 0.0, at: 0.0, started: false, paused: false, closed: false });
    let spawned = crate::coro::spawn("hx_audio", false, Box::new(move || load(h, url, headers)));
    if spawned == 0 {
        if let Some(s) = by_h(h) {
            s.state = "error: could not start".to_string();
        }
    }
    h as f64
}

/// Is this sound the one on the music voice?
fn owns(s: &Snd) -> bool {
    s.sample.is_some() && sfx::music_sample() == s.sample
}

fn pos(s: &mut Snd) -> f64 {
    if owns(s) {
        s.at = sfx::music_pos();
    } else if s.started {
        // it ended (or something else took the music voice): from the start next time
        s.started = false;
        if !s.paused || s.at >= s.dur - 0.6 {
            s.at = 0.0;
        }
    }
    s.at.clamp(0.0, s.dur.max(0.0))
}

fn play(s: &mut Snd) {
    let Some(id) = s.sample else { return };
    if owns(s) {
        if !sfx::music_playing() {
            sfx::pause_music();
        }
    } else {
        let at = pos(s);
        let _ = sfx::play(id, 1.0, 0.0, false, 1.0, true);
        if at > 0.05 {
            sfx::seek_music(at);
        }
    }
    s.started = true;
    s.paused = false;
    log(&format!("sound {} playing from {:.2} s", s.h, s.at));
}

fn pause(s: &mut Snd) {
    if owns(s) && sfx::music_playing() {
        s.at = sfx::music_pos();
        s.paused = true;
        sfx::pause_music();
    }
}

fn close(h: f64) {
    if let Some(s) = snd(h) {
        if owns(s) {
            sfx::stop_music();
        }
        if let Some(id) = s.sample.take() {
            sfx::release(id);
        }
        s.closed = true;
    }
    sounds().retain(|s| !(s.closed && s.state != "loading" && s.state != "decoding"));
}

// ── the message chime: two soft bell notes (C6 then G6), made here once ──

static mut CHIME: Option<usize> = None;

fn chime(vol: f32) {
    let id = match unsafe { CHIME } {
        Some(id) => id,
        None => {
            let rate = 48_000.0f32;
            let n = (rate * 0.75) as usize;
            let mut pcm: Vec<i16> = Vec::with_capacity(n);
            let note = |t: f32, f: f32, at: f32| -> f32 {
                let t = t - at;
                if t < 0.0 {
                    return 0.0;
                }
                let attack = (t / 0.004).min(1.0);
                let env = attack * libm::expf(-t * 6.5);
                let w = core::f32::consts::TAU * f * t;
                env * (libm::sinf(w) + 0.28 * libm::sinf(2.0 * w) + 0.08 * libm::sinf(3.0 * w))
            };
            for i in 0..n {
                let t = i as f32 / rate;
                let fade = ((n - i) as f32 / (rate * 0.05)).min(1.0);
                let v = (note(t, 1046.5, 0.0) * 0.55 + note(t, 1568.0, 0.11) * 0.5) * 0.42 * fade;
                pcm.push((v.clamp(-1.0, 1.0) * 32767.0) as i16);
            }
            let id = sfx::stream_open(48_000, false);
            sfx::stream_push(id, &pcm);
            sfx::stream_close(id);
            unsafe { CHIME = Some(id) };
            id
        },
    };
    let _ = sfx::play(id, vol.clamp(0.0, 1.5), 0.0, false, 1.0, false);
    log("chime");
}

// ── uploads ──

static mut JOBS: Vec<(String, Option<String>)> = Vec::new();

fn jobs() -> &'static mut Vec<(String, Option<String>)> {
    unsafe { &mut *(&raw mut JOBS) }
}

fn upload_start(method: String, url: String, headers: String, path: String) -> String {
    let id = format!("hxu{}", unsafe {
        NEXT += 1;
        NEXT
    });
    jobs().push((id.clone(), None));
    let jid = id.clone();
    let spawned = crate::coro::spawn(
        "hx_upload",
        false,
        Box::new(move || {
            // streamed from disk in 256 KiB pieces (Discord takes up to 500 MB with Nitro: never
            // the whole file in memory), the bytes sent so far kept for the app's progress line
            let out = match mediafile::open_source(&path) {
                Some(mut src) => {
                    let total = src.len();
                    stage_set(&jid, format!("sending 0 {total}"));
                    log(&format!("upload {jid}: {total} bytes {method} {}", url.split('?').next().unwrap_or("")));
                    let mut read = |off: u64, buf: &mut Vec<u8>| match src.read(off, 256 * 1024) {
                        Some(v) => {
                            buf.extend_from_slice(&v);
                            true
                        },
                        None => false,
                    };
                    let mut sent = |n: u64| stage_set(&jid, format!("sending {n} {total}"));
                    match crate::lingsock::request_upload(&method, &url, &headers, total, &mut read, &mut sent, 64 * 1024) {
                        Ok((st, _h, b)) => format!("{st}\n{}", String::from_utf8_lossy(&b)),
                        Err(e) => format!("0\nERROR: {e}"),
                    }
                },
                None => format!("0\nERROR: no such file: {path}"),
            };
            log(&format!("upload {jid}: {}", out.lines().next().unwrap_or("")));
            if let Some(j) = jobs().iter_mut().find(|j| j.0 == jid) {
                j.1 = Some(out);
            }
        }),
    );
    if spawned == 0 {
        jobs().retain(|j| j.0 != id);
        return String::new();
    }
    id
}

/// Where each running upload is: "reading" (connecting, before the first byte), then
/// "sending <sent> <total>" as the file streams out (lingsock::request_upload's progress).
static mut STAGES: Vec<(String, String)> = Vec::new();

fn stage_set(id: &str, s: String) {
    let v = unsafe { &mut *(&raw mut STAGES) };
    match v.iter_mut().find(|e| e.0 == id) {
        Some(e) => e.1 = s,
        None => v.push((String::from(id), s)),
    }
}

fn upload_stage(id: &str) -> String {
    let v = unsafe { &*(&raw const STAGES) };
    v.iter().find(|e| e.0 == id).map(|e| e.1.clone()).unwrap_or_else(|| String::from("reading"))
}

fn upload_poll(id: &str) -> String {
    match jobs().iter().position(|j| j.0 == id && j.1.is_some()) {
        Some(i) => {
            unsafe { (*(&raw mut STAGES)).retain(|e| e.0 != id) };
            jobs().remove(i).1.unwrap_or_default()
        },
        None if !jobs().iter().any(|j| j.0 == id) => String::from("0\nERROR: no such upload"),
        None => String::new(),
    }
}

fn file_size(path: &str) -> f64 {
    match mediafile::open_source(path) {
        Some(src) => src.len() as f64,
        None => -1.0,
    }
}

// ── emoji ──

/// Shown as a picture on its own (default emoji presentation), not only with U+FE0F.
fn emoji_base(c: u32) -> bool {
    matches!(c,
        0x1F000..=0x1FAFF
        | 0x231A | 0x231B | 0x23E9..=0x23EC | 0x23F0 | 0x23F3 | 0x25FD | 0x25FE
        | 0x2614 | 0x2615 | 0x2648..=0x2653 | 0x267F | 0x2693 | 0x26A1 | 0x26AA | 0x26AB
        | 0x26BD | 0x26BE | 0x26C4 | 0x26C5 | 0x26CE | 0x26D4 | 0x26EA | 0x26F2 | 0x26F3
        | 0x26F5 | 0x26FA | 0x26FD | 0x2705 | 0x270A | 0x270B | 0x2728 | 0x274C | 0x274E
        | 0x2753..=0x2755 | 0x2757 | 0x2795..=0x2797 | 0x27B0 | 0x27BF | 0x2B1B | 0x2B1C
        | 0x2B50 | 0x2B55)
}

/// A picture only when U+FE0F follows (hearts, check marks, arrows ...).
fn emoji_text_default(c: u32) -> bool {
    matches!(c, 0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139 | 0x2194..=0x21AA | 0x2300..=0x23FF | 0x24C2 | 0x25AA..=0x25FE | 0x2600..=0x27BF | 0x2934 | 0x2935 | 0x2B05..=0x2B55 | 0x3030 | 0x303D | 0x3297 | 0x3299)
}

fn is_mod(c: u32) -> bool {
    (0x1F3FB..=0x1F3FF).contains(&c) || (0xE0020..=0xE007F).contains(&c)
}

/// The emoji sequences of `t`: (start char index, end char index, code points).
fn emoji_runs(cs: &[char]) -> Vec<(usize, usize, Vec<u32>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i] as u32;
        let next = cs.get(i + 1).map(|&x| x as u32);
        // keycaps: 1 FE0F 20E3
        if cs[i].is_ascii_digit() || c == 0x23 || c == 0x2A {
            let (a, b) = (next, cs.get(i + 2).map(|&x| x as u32));
            if a == Some(0x20E3) {
                out.push((i, i + 2, alloc::vec![c, 0x20E3]));
                i += 2;
                continue;
            }
            if a == Some(0xFE0F) && b == Some(0x20E3) {
                out.push((i, i + 3, alloc::vec![c, 0xFE0F, 0x20E3]));
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }
        let start = emoji_base(c) || (emoji_text_default(c) && next == Some(0xFE0F));
        if !start {
            i += 1;
            continue;
        }
        let s = i;
        let mut cps = alloc::vec![c];
        i += 1;
        // a flag: two regional indicators
        if (0x1F1E6..=0x1F1FF).contains(&c) {
            if let Some(n) = next.filter(|n| (0x1F1E6..=0x1F1FF).contains(n)) {
                cps.push(n);
                i += 1;
            }
            out.push((s, i, cps));
            continue;
        }
        loop {
            let Some(&n) = cs.get(i) else { break };
            let n = n as u32;
            if n == 0xFE0F || is_mod(n) {
                cps.push(n);
                i += 1;
            } else if n == 0x200D {
                let Some(&m) = cs.get(i + 1) else { break };
                let m = m as u32;
                if emoji_base(m) || emoji_text_default(m) {
                    cps.push(0x200D);
                    cps.push(m);
                    i += 2;
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        out.push((s, i, cps));
    }
    out
}

/// Twemoji's file name: the code points in hex, U+FE0F left out unless the sequence has a ZWJ.
fn twemoji_name(cps: &[u32]) -> String {
    let zwj = cps.contains(&0x200D);
    let parts: Vec<String> = cps.iter().filter(|&&c| zwj || c != 0xFE0F).map(|c| format!("{c:x}")).collect();
    parts.join("-")
}

fn emoji_html(t: &str, base: &str, cls: &str) -> String {
    let cs: Vec<char> = t.chars().collect();
    let runs = emoji_runs(&cs);
    if runs.is_empty() {
        return t.to_string();
    }
    // only emoji (and spaces), and not many: drawn big, like Discord does
    let covered: usize = runs.iter().map(|r| r.1 - r.0).sum();
    let blank = cs.iter().filter(|c| c.is_whitespace()).count();
    let jumbo = covered + blank == cs.len() && runs.len() <= 27;
    let mut out = String::with_capacity(t.len() + runs.len() * 96);
    let mut at = 0;
    for (s, e, cps) in &runs {
        out.extend(cs[at..*s].iter());
        out.push_str(&format!("<img class=\"{cls}{}\" src=\"{base}{}.png\" alt=\"\">", if jumbo { " jumbo" } else { "" }, twemoji_name(cps)));
        at = *e;
    }
    out.extend(cs[at..].iter());
    out
}

fn cp_str(hex: &str) -> String {
    hex.split(['-', ' ', ',']).filter_map(|h| u32::from_str_radix(h.trim(), 16).ok()).filter_map(char::from_u32).collect()
}

// ── the builtins ──

pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    if !name.starts_with(b"hx_") {
        return None;
    }
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).unwrap_or(-1.0);
    Some(match name {
        b"hx_audio_open" => num(audio_open(s(0), s(1))),
        b"hx_audio_state" => mk_str(&snd(n(0)).map(|s| s.state.clone()).unwrap_or_else(|| "error: no such sound".to_string())),
        b"hx_audio_info" => mk_str(&snd(n(0)).map(|s| s.info.clone()).unwrap_or_default()),
        b"hx_audio_duration" => num(snd(n(0)).map(|s| s.dur).unwrap_or(0.0)),
        b"hx_audio_pos" => num(snd(n(0)).map(pos).unwrap_or(0.0)),
        b"hx_audio_playing" => boolean(snd(n(0)).is_some_and(|s| owns(s) && sfx::music_playing())),
        b"hx_audio_play" => {
            if let Some(s) = snd(n(0)) {
                play(s);
            }
            UNIT
        },
        b"hx_audio_pause" => {
            if let Some(s) = snd(n(0)) {
                pause(s);
            }
            UNIT
        },
        b"hx_audio_seek" => {
            if let Some(s) = snd(n(0)) {
                s.at = n(1).clamp(0.0, s.dur.max(0.0));
                if owns(s) {
                    sfx::seek_music(s.at);
                }
            }
            UNIT
        },
        b"hx_audio_close" => {
            close(n(0));
            UNIT
        },
        b"hx_upload_async" => mk_str(&upload_start(s(0), s(1), s(2), s(3))),
        b"hx_upload_poll" => mk_str(&upload_poll(&s(0))),
        // "reading", then "sending <sent> <total>" while it runs
        b"hx_upload_stage" => mk_str(&upload_stage(&s(0))),
        b"hx_file_size" => num(file_size(&s(0))),
        b"hx_emoji_html" => mk_str(&emoji_html(&s(0), &s(1), &s(2))),
        b"hx_cp" => mk_str(&cp_str(&s(0))),
        b"hx_chime" => {
            chime(args.first().map(|&v| to_f64(v) as f32).unwrap_or(1.0));
            UNIT
        },
        _ => return None,
    })
}
