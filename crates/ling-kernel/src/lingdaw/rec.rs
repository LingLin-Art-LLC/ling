//! LingStationX audio recording and audio clips on LingOS.
//!
//! **Recording** (`daw_audio_rec_*`, ling-daw `api.rs` semantics): the take
//! comes from the mixer's capture ring (`mixer::audio_in_read_stereo`, x86:
//! an HD Audio jack or a USB interface such as the PreSonus Studio 26c),
//! drained every audio pump by [`rec_service`] so nothing is lost while the
//! app draws, input gain applied. Stopping mirrors a mono mic that is on one
//! side of a stereo input (the 26c's input 1) to both channels, optionally
//! normalises a quiet take, writes it next to the project as
//! `audio/take_<n>.wav` (24-bit PCM) and adds an audio clip where recording
//! began.
//!
//! **Audio clips** (`ClipDoc::audio_path`): WAV files (8/16/24/32-bit PCM,
//! 32-bit float, any channel count -> stereo) are loaded when the project
//! changes and mixed by [`clip_mix`] into the engine's output with the clip's
//! gain / pitch / offset / loop, the track's level and pan, mute and solo.

use alloc::string::String;
use alloc::vec::Vec;

use super::{ClipDoc, Daw, ProjectDoc, SR};
use crate::lingrt::{boolean, list_from, mk_str, num, to_f64, to_string, UNIT};

/// Longest take kept in memory (stereo f32: 15 min = ~350 MB).
const MAX_REC_FRAMES: usize = 15 * 60 * SR as usize;
const CHUNK_FRAMES: usize = SR as usize;

struct Rec {
    track: usize,
    start: f64,
    /// One-second chunks of interleaved stereo (no big reallocations).
    chunks: Vec<Vec<f32>>,
    frames: usize,
    peak: f32,
    /// [min, max] per 1/100 s, for the live waveform.
    wave: Vec<(f32, f32)>,
    wave_acc: (f32, f32, usize),
    full: bool,
}

static mut REC: Option<Rec> = None;
static mut IN_PREF: String = String::new();
static mut IN_GAIN_DB: f32 = 0.0;
static mut IN_NORM: bool = true;

fn rec() -> &'static mut Option<Rec> {
    unsafe { &mut *(&raw mut REC) }
}

pub fn recording() -> bool {
    rec().is_some()
}

/// From the mixer's pump: move what the input recorded into the take.
pub fn rec_service() {
    let Some(r) = rec() else { return };
    #[cfg(target_arch = "x86_64")]
    {
        let got = crate::drivers::mixer::audio_in_read_stereo(usize::MAX);
        take_push(r, &got);
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = r;
}

fn take_push(r: &mut Rec, stereo: &[f32]) {
    let g = db_to_gain(unsafe { IN_GAIN_DB });
    for f in stereo.chunks_exact(2) {
        if r.frames >= MAX_REC_FRAMES {
            r.full = true;
            return;
        }
        if r.chunks.last().is_none_or(|c| c.len() >= CHUNK_FRAMES * 2) {
            r.chunks.push(Vec::with_capacity(CHUNK_FRAMES * 2));
        }
        let (l, rr) = ((f[0] * g).clamp(-1.0, 1.0), (f[1] * g).clamp(-1.0, 1.0));
        let c = r.chunks.last_mut().unwrap();
        c.push(l);
        c.push(rr);
        r.frames += 1;
        let a = l.abs().max(rr.abs());
        r.peak = r.peak.max(a);
        let (lo, hi, n) = &mut r.wave_acc;
        *lo = lo.min(l.min(rr));
        *hi = hi.max(l.max(rr));
        *n += 1;
        if *n >= SR as usize / 100 {
            r.wave.push((*lo, *hi));
            r.wave_acc = (0.0, 0.0, 0);
        }
    }
}

fn db_to_gain(db: f32) -> f32 {
    libm::powf(10.0, db / 20.0)
}

/// Mirror a one-sided mono input; normalise a quiet take (ling-daw `fix_take`).
fn fix_take(data: &mut [f32], normalize: bool) {
    let (mut el, mut er, mut pk) = (0.0f64, 0.0f64, 0.0f32);
    for f in data.chunks_exact(2) {
        el += (f[0] * f[0]) as f64;
        er += (f[1] * f[1]) as f64;
        pk = pk.max(f[0].abs()).max(f[1].abs());
    }
    if el.max(er) > 0.0 && el.min(er) < el.max(er) * 0.01 {
        let left_live = el > er;
        for f in data.chunks_exact_mut(2) {
            let v = if left_live { f[0] } else { f[1] };
            f[0] = v;
            f[1] = v;
        }
    }
    if normalize && pk > 1e-5 && pk < 0.5 {
        let g = 0.7 / pk;
        for s in data.iter_mut() {
            *s *= g;
        }
    }
}

/// The inputs LingStationX can pick ("" = the Settings choice).
fn inputs() -> Vec<String> {
    #[cfg(target_arch = "x86_64")]
    {
        crate::drivers::mixer::input_rows()
            .iter()
            .filter_map(|r| {
                let c: Vec<&str> = r.split('\t').collect();
                (c.len() > 1 && c[0] != "auto" && c[0] != "none").then(|| String::from(c[1]))
            })
            .collect()
    }
    #[cfg(not(target_arch = "x86_64"))]
    Vec::new()
}

fn start_capture(dev: &str) -> Result<String, String> {
    #[cfg(target_arch = "x86_64")]
    {
        use crate::drivers::mixer;
        if !mixer::audio_in_start_from(dev) {
            let e = mixer::capture_error();
            return Err(if e.is_empty() || e == "no input" {
                String::from("no audio input -- plug in a microphone or a USB audio interface (Settings > Sound > Input)")
            } else {
                e
            });
        }
        // what was waiting is from before Record: not part of the take
        let _ = mixer::audio_in_read_stereo(usize::MAX);
        Ok(mixer::input_label())
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = dev;
        Err(String::from("audio input is not supported on this machine yet"))
    }
}

fn stop_capture() {
    #[cfg(target_arch = "x86_64")]
    crate::drivers::mixer::audio_in_stop();
}

/// The `daw_audio_*` input / recording builtins; None for other names.
pub(super) fn dispatch(name: &[u8], args: &[u64], d: &mut Daw) -> Option<u64> {
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).unwrap_or(0.0);
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    Some(match name {
        b"daw_audio_inputs" => list_from(inputs().iter().map(|x| mk_str(x)).collect()),
        b"daw_audio_input_set" => {
            unsafe { *(&raw mut IN_PREF) = s(0) };
            UNIT
        },
        b"daw_audio_input_get" => mk_str(unsafe { &*(&raw const IN_PREF) }),
        b"daw_audio_gain_set" => {
            unsafe { IN_GAIN_DB = n(0).clamp(-24.0, 60.0) as f32 };
            UNIT
        },
        b"daw_audio_gain_get" => num(unsafe { IN_GAIN_DB } as f64),
        b"daw_audio_norm_set" => {
            unsafe { IN_NORM = n(0) > 0.5 };
            UNIT
        },
        b"daw_audio_norm_get" => boolean(unsafe { IN_NORM }),
        b"daw_audio_rec_start" => {
            if rec().is_some() {
                return Some(num(1.0));
            }
            let dev = if s(1).is_empty() { unsafe { (*(&raw const IN_PREF)).clone() } } else { s(1) };
            let track = n(0).max(0.0) as usize;
            if d.project.tracks.get(track).is_none_or(|t| !t.is_audio()) {
                d.status = String::from("Audio recording needs an audio track");
                return Some(num(0.0));
            }
            match start_capture(&dev) {
                Ok(label) => {
                    *rec() = Some(Rec {
                        track,
                        start: d.position.max(0.0),
                        chunks: Vec::new(),
                        frames: 0,
                        peak: 0.0,
                        wave: Vec::new(),
                        wave_acc: (0.0, 0.0, 0),
                        full: false,
                    });
                    d.status = alloc::format!("Recording audio from {label}");
                    num(1.0)
                },
                Err(e) => {
                    d.status = alloc::format!("Audio input failed: {e}");
                    num(0.0)
                },
            }
        },
        b"daw_audio_rec_active" => boolean(rec().is_some()),
        b"daw_audio_rec_level" => {
            rec_service();
            match rec() {
                Some(r) => {
                    let p = r.peak;
                    r.peak *= 0.7; // the meter falls back between reads
                    num(p as f64)
                },
                None => num(0.0),
            }
        },
        b"daw_audio_rec_wave" => {
            let cols = (n(0).max(1.0) as usize).min(4096);
            let Some(r) = rec() else { return Some(list_from(Vec::new())) };
            let total = r.wave.len();
            if total == 0 {
                return Some(list_from(Vec::new()));
            }
            let cols = cols.min(total);
            let mut out = Vec::with_capacity(cols * 2);
            for c in 0..cols {
                let a = c * total / cols;
                let b = ((c + 1) * total / cols).max(a + 1).min(total);
                let (mut lo, mut hi) = (0.0f32, 0.0f32);
                for w in &r.wave[a..b] {
                    lo = lo.min(w.0);
                    hi = hi.max(w.1);
                }
                out.push(num(lo as f64));
                out.push(num(hi as f64));
            }
            list_from(out)
        },
        b"daw_audio_rec_info" => match {
            rec_service();
            rec()
        } {
            Some(r) => {
                let secs = r.frames as f64 / SR;
                let bpm = d.project.bpm_at(r.start);
                if r.full {
                    d.status = String::from("Recording is at its 15 minute limit: stop to keep the take");
                }
                list_from(alloc::vec![num(r.start), num(secs * bpm / 60.0), num(r.track as f64)])
            },
            None => list_from(Vec::new()),
        },
        b"daw_audio_rec_stop" => {
            rec_service();
            stop_capture();
            let Some(r) = rec().take() else { return Some(num(-1.0)) };
            num(finish_take(d, r))
        },
        _ => return None,
    })
}

/// The take as a WAV beside the project and a clip on its track; the clip's index, or -1.
fn finish_take(d: &mut Daw, r: Rec) -> f64 {
    if r.frames < 256 {
        d.status = String::from("Recording was empty (no sound came in from the input)");
        return -1.0;
    }
    let mut data: Vec<f32> = Vec::new();
    if data.try_reserve_exact(r.frames * 2).is_err() {
        d.status = String::from("Recording too long for the memory left");
        return -1.0;
    }
    for c in r.chunks {
        data.extend_from_slice(&c);
    }
    fix_take(&mut data, unsafe { IN_NORM });
    let path = take_path(d);
    let wav = wav_24(&data, SR as u32);
    // lingfs holds a few MiB and a too-big write takes minutes to fail: only
    // a data volume (installed system, USB disk, the Pi's card) gets big takes
    let on_volume = crate::vfs::storage_path(&path).is_some() || crate::fs::media::is_media_path(&crate::vfs::norm(&path));
    let saved = if on_volume || wav.len() < 4 << 20 { crate::vfs::write(&path, &wav) } else { false };
    let secs = (data.len() / 2) as f64 / SR;
    put_sample(&path, Sample { rate: SR as u32, data });
    let bpm = d.project.bpm_at(r.start);
    let p = &mut d.project;
    let Some(t) = p.tracks.get_mut(r.track) else { return -1.0 };
    t.clips.push(ClipDoc {
        name: String::from("Recording"),
        start: r.start,
        len: (secs * bpm / 60.0).max(0.25),
        midi_notes: Vec::new(),
        audio_path: path.clone(),
        ..ClipDoc::default()
    });
    let idx = t.clips.len() - 1;
    d.dirty = true;
    d.rev += 1;
    d.status = if saved {
        alloc::format!("Recorded {path} ({:.1} s)", secs)
    } else {
        alloc::format!("Recorded {:.1} s (not saved to disk: no room for {path} -- it plays until LingOS restarts)", secs)
    };
    idx as f64
}

/// `<project folder>/audio/take_<n>.wav`, or the LingStationX projects folder
/// for an unsaved project / a built-in template.
fn take_path(d: &Daw) -> String {
    let base = if !d.path.is_empty() && !d.path.starts_with("assets/") && !d.path.starts_with("projects/") {
        crate::vfs::parent(&d.path)
    } else {
        crate::vfs::projects_root().unwrap_or_else(|| alloc::format!("{}/Projects/LingStationX", crate::vfs::home()))
    };
    let dir = alloc::format!("{base}/audio");
    let stamp = crate::arch::timer::now_ms();
    let mut k = 0u32;
    loop {
        let p = if k == 0 { alloc::format!("{dir}/take_{stamp}.wav") } else { alloc::format!("{dir}/take_{stamp}_{k}.wav") };
        if !crate::vfs::exists(&p) {
            return p;
        }
        k += 1;
    }
}

fn wav_24(data: &[f32], rate: u32) -> Vec<u8> {
    let frames = data.len() / 2;
    let bytes = frames * 6;
    let mut w = Vec::with_capacity(44 + bytes);
    let u32le = |w: &mut Vec<u8>, v: u32| w.extend_from_slice(&v.to_le_bytes());
    w.extend_from_slice(b"RIFF");
    u32le(&mut w, 36 + bytes as u32);
    w.extend_from_slice(b"WAVEfmt ");
    u32le(&mut w, 16);
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&2u16.to_le_bytes());
    u32le(&mut w, rate);
    u32le(&mut w, rate * 6);
    w.extend_from_slice(&6u16.to_le_bytes());
    w.extend_from_slice(&24u16.to_le_bytes());
    w.extend_from_slice(b"data");
    u32le(&mut w, bytes as u32);
    for &x in data {
        let v = (x.clamp(-1.0, 1.0) * 8_388_607.0) as i32;
        w.extend_from_slice(&v.to_le_bytes()[..3]);
    }
    w
}

// ── audio clips ─────────────────────────────────────────────────────────────

pub(super) struct Sample {
    rate: u32,
    /// Interleaved stereo.
    data: Vec<f32>,
}

/// Loaded clip audio by path (None: tried and could not be read).
static mut SAMPLES: Vec<(String, Option<Sample>)> = Vec::new();
static mut SAMPLES_REV: u64 = 0;

fn samples() -> &'static mut Vec<(String, Option<Sample>)> {
    unsafe { &mut *(&raw mut SAMPLES) }
}

fn put_sample(path: &str, s: Sample) {
    let v = samples();
    v.retain(|(p, _)| p != path);
    v.push((String::from(path), Some(s)));
}

fn sample(path: &str) -> Option<&'static Sample> {
    samples().iter().find(|(p, _)| p == path).and_then(|(_, s)| s.as_ref())
}

/// Load the audio of every audio clip not yet loaded (once per project change).
pub(super) fn ensure_samples(d: &Daw) {
    if unsafe { SAMPLES_REV } == d.rev {
        return;
    }
    unsafe { SAMPLES_REV = d.rev };
    let mut want: Vec<&str> = Vec::new();
    for t in &d.project.tracks {
        for c in &t.clips {
            if c.is_audio() && !want.contains(&c.audio_path.as_str()) {
                want.push(c.audio_path.as_str());
            }
        }
    }
    // the vocal editor's recordings and renders (`ved.<id>.src` / `.out`) stay while an edit names them: Compare plays the
    // original, and a recording that was never saved to disk could not be read again
    for (k, v) in &d.project.meta {
        if k.starts_with("ved.") && (k.ends_with(".src") || k.ends_with(".out")) && !v.is_empty() && !want.contains(&v.as_str()) {
            want.push(v.as_str());
        }
    }
    // forget audio no clip uses any more
    samples().retain(|(p, _)| want.contains(&p.as_str()));
    for p in want {
        if samples().iter().any(|(q, _)| q == p) {
            continue;
        }
        let s = load_sample(d, p);
        samples().push((String::from(p), s));
    }
}

/// The length in seconds of an audio file a clip is about to use (loaded into the cache now), None when it cannot be read.
pub(super) fn sample_seconds(d: &Daw, path: &str) -> Option<f64> {
    if sample(path).is_none() {
        let s = load_sample(d, path)?;
        samples().retain(|(p, _)| p != path);
        samples().push((String::from(path), Some(s)));
    }
    let s = sample(path)?;
    Some((s.data.len() / 2) as f64 / s.rate.max(1) as f64)
}

/// A clip's audio (interleaved stereo) and its rate, loading it if need be (the vocal editor). The slice lives until the path
/// is forgotten or replaced: use it before the next [`put_audio`] / [`forget`] / [`ensure_samples`].
pub(super) fn sample_data(d: &Daw, path: &str) -> Option<(&'static [f32], u32)> {
    sample_seconds(d, path)?;
    let s = sample(path)?;
    Some((&s.data[..], s.rate))
}

/// Audio made in memory (a vocal edit's render) under `path`, playable by any clip that names it.
pub(super) fn put_audio(path: &str, rate: u32, data: Vec<f32>) {
    put_sample(path, Sample { rate, data });
}

/// Drop `path` from the loaded audio.
pub(super) fn forget(path: &str) {
    samples().retain(|(p, _)| p != path);
}

/// The folder new audio of the project goes in: `<project folder>/audio`, or the LingStationX projects folder's for an unsaved
/// project / a built-in template (as takes).
pub(super) fn audio_dir(d: &Daw) -> String {
    let base = if !d.path.is_empty() && !d.path.starts_with("assets/") && !d.path.starts_with("projects/") {
        crate::vfs::parent(&d.path)
    } else {
        crate::vfs::projects_root().unwrap_or_else(|| alloc::format!("{}/Projects/LingStationX", crate::vfs::home()))
    };
    alloc::format!("{base}/audio")
}

/// Write a WAV the way takes are written: lingfs holds a few MiB and a too-big write takes minutes to fail, so only a data
/// volume gets a big file. False when it was not written (it still plays from memory).
pub(super) fn save_audio_file(path: &str, wav: &[u8]) -> bool {
    let on_volume = crate::vfs::storage_path(path).is_some() || crate::fs::media::is_media_path(&crate::vfs::norm(path));
    if on_volume || wav.len() < 4 << 20 { crate::vfs::write(path, wav) } else { false }
}

/// 32-bit float stereo WAV (what ling-daw's vocal editor writes).
pub(super) fn wav_f32(data: &[f32], rate: u32) -> Vec<u8> {
    let frames = data.len() / 2;
    let bytes = frames * 8;
    let mut w = Vec::with_capacity(44 + bytes);
    let u32le = |w: &mut Vec<u8>, v: u32| w.extend_from_slice(&v.to_le_bytes());
    w.extend_from_slice(b"RIFF");
    u32le(&mut w, 36 + bytes as u32);
    w.extend_from_slice(b"WAVEfmt ");
    u32le(&mut w, 16);
    w.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    w.extend_from_slice(&2u16.to_le_bytes());
    u32le(&mut w, rate);
    u32le(&mut w, rate * 8);
    w.extend_from_slice(&8u16.to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(b"data");
    u32le(&mut w, bytes as u32);
    for &x in data {
        w.extend_from_slice(&x.to_le_bytes());
    }
    w
}

/// Peaks (0..1, both channels) of the `[f0, f1)` part of a loaded sample (fractions of its length) in `bins` bins
/// (ling-daw `Sample::peaks` / `daw_clip_wave`).
pub(super) fn peaks(path: &str, f0: f64, f1: f64, bins: usize) -> Vec<f32> {
    let Some(s) = sample(path) else { return alloc::vec![0.0; bins] };
    let frames = s.data.len() / 2;
    if frames == 0 {
        return alloc::vec![0.0; bins];
    }
    (0..bins)
        .map(|i| {
            let u0 = f0 + (f1 - f0) * i as f64 / bins as f64;
            let u1 = f0 + (f1 - f0) * (i + 1) as f64 / bins as f64;
            if u1 <= 0.0 || u0 >= 1.0 {
                return 0.0;
            }
            let a = (u0.max(0.0) * frames as f64) as usize;
            let b = ((u1.min(1.0) * frames as f64) as usize).max(a + 1).min(frames);
            (a..b).map(|f| libm::fabsf(s.data[f * 2]).max(libm::fabsf(s.data[f * 2 + 1]))).fold(0.0, f32::max)
        })
        .collect()
}

/// An audio clip's file as mono (both channels averaged) and its rate, loading it if need be (BPM detection).
pub(super) fn mono_of(d: &Daw, path: &str) -> Option<(Vec<f32>, u32)> {
    sample_seconds(d, path)?;
    let s = sample(path)?;
    Some((s.data.chunks_exact(2).map(|f| (f[0] + f[1]) * 0.5).collect(), s.rate))
}

/// A sample file for an effect that works from one (ling-fxcore's loader hook: the Hypergrowl's source).
pub(super) fn fx_loader(path: &str) -> Option<ling_fxcore::host::Decoded> {
    let bytes = crate::vfs::read(&path.replace('\\', "/"))?;
    let s = decode_wav(&bytes)?;
    Some(ling_fxcore::host::Decoded { stereo: s.data, rate: s.rate })
}

fn load_sample(d: &Daw, path: &str) -> Option<Sample> {
    let p = path.replace('\\', "/");
    let dir = crate::vfs::parent(&d.path);
    let base = p.rsplit('/').next().unwrap_or(&p);
    // as given, beside the project, in its audio/ folder (a project made on a PC)
    let tries = [p.clone(), alloc::format!("{dir}/{p}"), alloc::format!("{dir}/audio/{base}"), alloc::format!("{dir}/{base}")];
    for t in tries.iter() {
        if let Some(bytes) = crate::vfs::read(t) {
            if let Some(s) = decode_wav(&bytes) {
                return Some(s);
            }
            // MP3 / Ogg Vorbis / FLAC: the kernel's own decoders (the sound store's), to interleaved stereo
            if let Some((rate, ch, pcm)) = crate::codec::decode(&bytes) {
                let mut data = Vec::with_capacity(pcm.len() / ch.max(1) as usize * 2);
                if ch == 1 {
                    for &x in &pcm {
                        let v = x as f32 / 32768.0;
                        data.push(v);
                        data.push(v);
                    }
                } else {
                    data.extend(pcm.iter().map(|&x| x as f32 / 32768.0));
                }
                return Some(Sample { rate, data });
            }
        }
    }
    None
}

fn decode_wav(b: &[u8]) -> Option<Sample> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return None;
    }
    let rd16 = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let rd32 = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let (mut fmt, mut ch, mut rate, mut bits) = (0u16, 0usize, 0u32, 0usize);
    let mut off = 12;
    while off + 8 <= b.len() {
        let id = &b[off..off + 4];
        let sz = rd32(off + 4) as usize;
        let body = off + 8;
        if id == b"fmt " && body + 16 <= b.len() {
            fmt = rd16(body);
            ch = rd16(body + 2) as usize;
            rate = rd32(body + 4);
            bits = rd16(body + 14) as usize;
            if fmt == 0xFFFE && body + 26 <= b.len() {
                fmt = rd16(body + 24); // WAVE_FORMAT_EXTENSIBLE: the subformat
            }
        } else if id == b"data" {
            let end = (body + sz).min(b.len());
            let bps = bits / 8;
            if ch == 0 || rate == 0 || bps == 0 || !(fmt == 1 || fmt == 3) {
                return None;
            }
            let fb = ch * bps;
            let frames = (end - body) / fb;
            let mut data = Vec::new();
            data.try_reserve_exact(frames * 2).ok()?;
            let smp = |o: usize| -> f32 {
                match (fmt, bps) {
                    (3, 4) => f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]),
                    (_, 1) => (b[o] as f32 - 128.0) / 128.0,
                    (_, 2) => i16::from_le_bytes([b[o], b[o + 1]]) as f32 / 32768.0,
                    (_, 3) => ((((b[o] as u32) << 8 | (b[o + 1] as u32) << 16 | (b[o + 2] as u32) << 24) as i32) >> 8) as f32 / 8_388_608.0,
                    (_, 4) => i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as f32 / 2_147_483_648.0,
                    _ => 0.0,
                }
            };
            for f in 0..frames {
                let o = body + f * fb;
                let l = smp(o);
                let r = if ch > 1 { smp(o + bps) } else { l };
                data.push(l);
                data.push(r);
            }
            return Some(Sample { rate, data });
        }
        off = body + sz + (sz & 1);
    }
    None
}

/// Mix the audio clips sounding in `[b0, b1)` (the beats of `n` frames,
/// `b1` past a loop end when the transport wraps) into `out` (2 * n, f64
/// stereo, before the master level). False when no audio clip sounds.
/// Tracks in `fx` (running effects) are left out: [`clip_track`] gives those dry for their chain.
pub(super) fn clip_mix_gains(p: &ProjectDoc, b0: f64, b1: f64, n: usize, out: &mut [f64], fx: &[bool], gains: &[(f64, f64)]) -> bool {
    clips_into(p, b0, b1, n, out, |ti, t| {
        if fx.get(ti).copied().unwrap_or(false) {
            return None;
        }
        // the sub-block's level and pan (automation may be moving them)
        let (lvl, pan) = gains.get(ti).copied().unwrap_or((t.level as f64, t.pan as f64));
        let x = (pan.clamp(-1.0, 1.0) + 1.0) * core::f64::consts::FRAC_PI_4;
        Some((lvl * libm::cos(x), lvl * libm::sin(x)))
    })
}

/// The audio clips of track `ti` alone, before its fader and pan (the input of its effects), added to `out`.
pub(super) fn clip_track(p: &ProjectDoc, ti: usize, b0: f64, b1: f64, n: usize, out: &mut [f64]) -> bool {
    clips_into(p, b0, b1, n, out, |i, _| if i == ti { Some((1.0, 1.0)) } else { None })
}

fn clips_into(p: &ProjectDoc, b0: f64, b1: f64, n: usize, out: &mut [f64], gains: impl Fn(usize, &super::TrackDoc) -> Option<(f64, f64)>) -> bool {
    let wrap = p.loop_on && p.loop_end > p.loop_start && b1 > p.loop_end && b0 < p.loop_end;
    let span = p.loop_end - p.loop_start;
    let any_solo = p.tracks.iter().any(|t| t.solo);
    let mut any = false;
    for (ti, t) in p.tracks.iter().enumerate() {
        if !t.is_audio() || t.muted || (any_solo && !t.solo) || t.clips.is_empty() {
            continue;
        }
        let Some((gl, gr)) = gains(ti, t) else { continue };
        for c in &t.clips {
            if !c.is_audio() {
                continue;
            }
            // quick reject against the block (both halves when it wraps)
            let hit = |a: f64, z: f64| c.end() > a && c.start < z;
            if !(hit(b0, b1.min(if wrap { p.loop_end } else { b1 })) || (wrap && hit(p.loop_start, p.loop_start + (b1 - p.loop_end)))) {
                continue;
            }
            let Some(s) = sample(&c.audio_path) else { continue };
            let frames = s.data.len() / 2;
            if frames == 0 {
                continue;
            }
            let speed = libm::pow(2.0, c.pitch as f64 / 12.0) * s.rate as f64;
            // the engine scales its mix by 0.35 before the limiter: undo that so
            // a take plays back at the level it was recorded
            let g = c.gain as f64 / 0.35;
            for i in 0..n {
                let mut beat = b0 + (b1 - b0) * i as f64 / n as f64;
                if wrap && beat >= p.loop_end {
                    beat -= span;
                }
                if beat < c.start || beat >= c.end() {
                    continue;
                }
                let mut rel = beat - c.start;
                if c.loop_len > 0.0 {
                    rel %= c.loop_len;
                }
                let secs = c.offset_s + rel * 60.0 / p.bpm_at(beat);
                let pos = secs * speed;
                let k = pos as usize;
                if k + 1 >= frames {
                    continue;
                }
                let fr = (pos - k as f64) as f32;
                let l = s.data[k * 2] + (s.data[k * 2 + 2] - s.data[k * 2]) * fr;
                let r = s.data[k * 2 + 1] + (s.data[k * 2 + 3] - s.data[k * 2 + 1]) * fr;
                out[i * 2] += l as f64 * g * gl;
                out[i * 2 + 1] += r as f64 * g * gr;
                any = true;
            }
        }
    }
    any
}

// ── `daw rec ...` (Terminal / SSH): the recorder without the app ────────────

/// How much of `x` is a sine at `freq` (1 = all of it, ~0 = none).
fn tonality(x: &[f64], freq: f64) -> f64 {
    let e: f64 = x.iter().map(|v| v * v).sum();
    if e <= 1e-12 || x.is_empty() {
        return 0.0;
    }
    super::goertzel(x, freq) / (x.len() as f64 / 2.0 * e)
}

fn peak_db(x: &[f64]) -> f64 {
    let p = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    if p > 1e-9 { 20.0 * libm::log10(p) } else { -120.0 }
}

pub(super) fn command(rest: &str) -> String {
    use super::{audio_start, daw, dispatch, Audio};
    let mut w = rest.split_whitespace();
    let sub = w.next().unwrap_or("");
    let d = daw();
    match sub {
        "inputs" => {
            let v = inputs();
            if v.is_empty() { String::from("no audio inputs\n") } else { v.iter().map(|x| alloc::format!("  {x}\n")).collect() }
        },
        "start" => {
            // onto the first audio track, made if there is none; from `[input name]`
            let dev: String = w.collect::<Vec<_>>().join(" ");
            let t = match d.project.tracks.iter().position(|t| t.is_audio()) {
                Some(t) => t,
                None => {
                    let _ = dispatch(b"daw_track_add", &[mk_str("audio"), mk_str("")]);
                    let d = daw();
                    d.project.tracks.len().saturating_sub(1)
                },
            };
            let ok = dispatch(b"daw_audio_rec_start", &[num(t as f64), mk_str(&dev)]).map(to_f64).unwrap_or(0.0) > 0.5;
            let d = daw();
            if ok && !d.playing {
                // the transport runs while recording, as with LingStationX's Record button
                d.playing = true;
                d.base_beat = d.position;
                d.base_ms = crate::arch::timer::now_ms();
                audio_start(d.position);
            }
            alloc::format!("rec: {} (track {t}) -- {}\n", if ok { "recording" } else { "FAILED" }, core::mem::take(&mut d.status))
        },
        "status" => {
            rec_service();
            match rec() {
                Some(r) => alloc::format!("rec: recording {:.2} s, peak {:.3}, input {}\n", r.frames as f64 / SR, r.peak, {
                    #[cfg(target_arch = "x86_64")]
                    {
                        crate::drivers::mixer::input_label()
                    }
                    #[cfg(not(target_arch = "x86_64"))]
                    {
                        String::new()
                    }
                }),
                None => String::from("rec: idle\n"),
            }
        },
        "stop" => {
            let freq: f64 = w.next().and_then(|f| f.parse().ok()).unwrap_or(440.0);
            let c = dispatch(b"daw_audio_rec_stop", &[]).map(to_f64).unwrap_or(-1.0);
            let d = daw();
            super::audio_stop();
            d.playing = false;
            let mut out = alloc::format!("rec: stopped -> clip {c}: {}\n", core::mem::take(&mut d.status));
            if c < 0.0 {
                return out;
            }
            let Some((ti, clip)) = d.project.tracks.iter().enumerate().find_map(|(i, t)| t.clips.get(c as usize).filter(|x| x.is_audio()).map(|x| (i, x.clone()))) else {
                return out;
            };
            if let Some(s) = sample(&clip.audio_path) {
                // the take itself (skip the first 0.2 s: the input's start-up)
                let l: Vec<f64> = s.data.chunks_exact(2).skip(SR as usize / 5).map(|f| f[0] as f64).collect();
                out.push_str(&alloc::format!(
                    "rec: take {:.2} s at {} Hz, peak {:.1} dBFS, {freq} Hz tonality {:.3}, file {} ({})\n",
                    (s.data.len() / 2) as f64 / s.rate as f64,
                    s.rate,
                    peak_db(&l),
                    tonality(&l, freq),
                    clip.audio_path,
                    if crate::vfs::exists(&clip.audio_path) { "on disk" } else { "NOT on disk" }
                ));
            }
            // and what the engine plays for it: 0.5 s from 0.3 s into the clip
            let mut a = Audio::new();
            a.running = true;
            let bps = d.project.bpm_at(clip.start) / 60.0;
            a.beat = clip.start + 0.3 * bps;
            let mut buf = alloc::vec![0.0f32; SR as usize]; // 0.5 s stereo
            super::synth(&mut a, &d.project, &mut buf);
            let l: Vec<f64> = buf.chunks_exact(2).map(|f| f[0] as f64).collect();
            out.push_str(&alloc::format!(
                "rec: playback of track {ti} clip {c}: peak {:.1} dBFS, {freq} Hz tonality {:.3}\n",
                peak_db(&l),
                tonality(&l, freq)
            ));
            out
        },
        _ => String::from("usage: daw rec inputs | start [input] | status | stop [test Hz]\n"),
    }
}
