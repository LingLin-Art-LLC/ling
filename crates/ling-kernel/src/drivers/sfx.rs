//! Sound effects and music for programs (LingDev games, the desktop's UI
//! sounds): WAV files decoded once into 48 kHz samples, played by up to 16
//! voices (pitch, volume, pan, loop) plus one music voice with pause / seek,
//! and short synthesised blips (`sfx`). The mix is added to whatever the
//! output is already playing -- LingStationX's engine -- by the HDMI audio
//! ring on the Pi and the AC'97 mixer on x86 (`mix_into`).
//!
//! Spatial calls take a position and the listener's orientation; here that
//! becomes a distance roll-off and a left/right pan, which is what a small
//! game needs to feel right.

use alloc::vec::Vec;

pub const RATE: f64 = 48_000.0;
/// Sounds at once (the last voice is the music's): a PC mixes many more than the Pi.
#[cfg(target_arch = "x86_64")]
const MAX_VOICES: usize = 48;
#[cfg(not(target_arch = "x86_64"))]
const MAX_VOICES: usize = 16;

pub struct Sample {
    rate: u32,
    stereo: bool,
    /// Interleaved L R when stereo.
    data: Vec<i16>,
    /// A streamed sound (`stream_*`): frames already dropped from the front,
    /// so positions stay counted from the start of the stream.
    base: usize,
    /// More pieces may still come: a voice that catches up waits (silent)
    /// instead of ending.
    open: bool,
}

#[derive(Clone, Copy)]
struct Voice {
    sample: usize,
    pos: f64,
    step: f64,
    vol: f32,
    pan: f32,
    looping: bool,
    active: bool,
    paused: bool,
    music: bool,
}

const SILENT: Voice = Voice { sample: 0, pos: 0.0, step: 1.0, vol: 1.0, pan: 0.0, looping: false, active: false, paused: false, music: false };

static mut SAMPLES: Vec<Sample> = Vec::new();
static mut VOICES: [Voice; MAX_VOICES] = [SILENT; MAX_VOICES];
static mut MASTER: f32 = 1.0;
static mut MUSIC_VOL: f32 = 1.0;
/// Listener yaw as (cos, sin): sounds to the listener's right pan right.
static mut LISTENER: (f32, f32) = (1.0, 0.0);
/// The music voice (one at a time), or MAX_VOICES for none.
static mut MUSIC_VOICE: usize = MAX_VOICES;

/// What the music voice played in each recent output block: (play generation,
/// first output frame, frames, position at its start and end in seconds,
/// seconds per output frame). `music_pos` looks up the block the speaker is
/// on now (mixer::audible_frame), so the position is what is heard -- across
/// a pause, a seek or a loop -- not the render cursor a whole queue ahead.
#[derive(Clone, Copy)]
struct Heard {
    gen: u32,
    frame0: u64,
    frames: u32,
    p0: f64,
    p1: f64,
    spf: f64,
}
const HEARD_N: usize = 256;
static mut HEARD: [Heard; HEARD_N] = [Heard { gen: 0, frame0: 0, frames: 0, p0: 0.0, p1: 0.0, spf: 0.0 }; HEARD_N];
static mut HEARD_AT: usize = 0;
static mut MUSIC_GEN: u32 = 0;

fn heard_note(frame0: u64, frames: usize, p0: f64, p1: f64, spf: f64) {
    unsafe {
        let h = &mut *(&raw mut HEARD);
        HEARD_AT = (HEARD_AT + 1) % HEARD_N;
        h[HEARD_AT] = Heard { gen: MUSIC_GEN, frame0, frames: frames as u32, p0, p1, spf };
    }
}

/// The music position (seconds) at output frame `at`, from the blocks rendered; None when
/// none of the current play has been rendered yet.
fn heard_at(at: u64) -> Option<f64> {
    unsafe {
        let h = &*(&raw const HEARD);
        let gen = MUSIC_GEN;
        let mut oldest: Option<Heard> = None;
        let mut newest: Option<Heard> = None;
        for k in 0..HEARD_N {
            let e = h[(HEARD_AT + HEARD_N - k) % HEARD_N];
            if e.gen != gen || e.frames == 0 {
                continue;
            }
            if newest.is_none() {
                newest = Some(e);
            }
            if at >= e.frame0 && at < e.frame0 + e.frames as u64 {
                let into = (at - e.frame0) as f64;
                let p = e.p0 + into * e.spf;
                // a loop or seek inside the block: past its end, hold the end
                return Some(if e.p1 >= e.p0 { p.min(e.p1) } else { p });
            }
            oldest = Some(e);
        }
        let n = newest?;
        if at >= n.frame0 + n.frames as u64 {
            return Some(n.p1);
        }
        // before anything of this play was heard: its first block's start
        Some(oldest.map(|e| e.p0).unwrap_or(n.p0))
    }
}

fn samples() -> &'static mut Vec<Sample> {
    unsafe { &mut *(&raw mut SAMPLES) }
}

fn voices() -> &'static mut [Voice; MAX_VOICES] {
    unsafe { &mut *(&raw mut VOICES) }
}

// ── Per-app ownership (appres.rs) ────────────────────────────────────────────
// Samples an app loads through its builtins are marked as its own; when it
// ends they are released (voices on them stop, PCM freed, the id stays
// valid but silent). Kernel-side players (media, video, Spotify, the trash
// burn, the test tone) never mark theirs: they manage their own lifetime.

static mut OWNERS: crate::appres::Owners = crate::appres::Owners::new();

fn owners() -> &'static mut crate::appres::Owners {
    unsafe { &mut *(&raw mut OWNERS) }
}

/// Sample `id` was made for the running app (its builtin just loaded it).
pub fn own(id: usize) -> usize {
    let who = crate::appres::owner();
    if who != 0 {
        owners().set(id, who);
    }
    id
}

/// Does `id` still hold sound (not released, not freed with its app)?
pub fn is_live(id: usize) -> bool {
    samples().get(id).is_some_and(|s| s.open || !s.data.is_empty())
}

/// App `uid` ended: every voice on its samples stops and their PCM goes.
/// Returns the freed sample ids (caches keyed by id drop them).
pub fn app_ended(uid: u32, f: &mut crate::appres::Freed) -> Vec<usize> {
    let ids = owners().take(uid);
    if ids.is_empty() {
        return ids;
    }
    let _irq = crate::mm::IrqLock::acquire();
    let mi = unsafe { MUSIC_VOICE };
    for (vi, v) in voices().iter_mut().enumerate() {
        if v.active && ids.binary_search(&v.sample).is_ok() {
            v.active = false;
            f.voices += 1;
            if vi == mi {
                unsafe { MUSIC_VOICE = MAX_VOICES };
            }
        }
    }
    for &i in &ids {
        if let Some(s) = samples().get_mut(i) {
            f.bytes += (s.data.capacity() * 2) as u64;
            s.data = Vec::new();
            s.open = false;
            f.sounds += 1;
        }
    }
    ids
}

/// RIFF/WAVE header walk: (data offset, data length, rate, channels, bits).
fn parse_wav(buf: &[u8]) -> Option<(usize, usize, u32, u8, u8)> {
    let len = buf.len();
    if len < 12 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
        return None;
    }
    let u32le = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    let u16le = |o: usize| u16::from_le_bytes([buf[o], buf[o + 1]]);
    let mut off = 12;
    let (mut rate, mut ch, mut bits, mut have_fmt) = (0u32, 0u8, 0u8, false);
    while off + 8 <= len {
        let id = &buf[off..off + 4];
        let sz = u32le(off + 4) as usize;
        let body = off + 8;
        if id == b"fmt " && body + 16 <= len {
            ch = u16le(body + 2) as u8;
            rate = u32le(body + 4);
            bits = u16le(body + 14) as u8;
            have_fmt = true;
        } else if id == b"data" {
            let dlen = sz.min(len - body);
            if have_fmt {
                return Some((body, dlen, rate, ch, bits));
            }
        }
        off = body + sz + (sz & 1);
    }
    None
}

/// Decode a WAV (PCM 8 / 16 / 24 / 32-bit integer or 32-bit float, mono or
/// stereo, any rate) into a sample; None if it is not one we can read.
pub fn load_wav(bytes: &[u8]) -> Option<usize> {
    // Ogg Vorbis / FLAC: decoded whole (outside the IRQ lock -- it takes a
    // while), then stored like any WAV
    if matches!(crate::codec::sniff(bytes), Some("ogg") | Some("flac") | Some("mp3")) {
        let (rate, ch, data) = crate::codec::decode(bytes)?;
        let _irq = crate::mm::IrqLock::acquire();
        samples().push(Sample { rate, stereo: ch == 2, data, base: 0, open: false });
        return Some(samples().len() - 1);
    }
    let _irq = crate::mm::IrqLock::acquire();
    let (off, len, rate, ch, bits) = parse_wav(bytes)?;
    if rate == 0 || !(1..=2).contains(&ch) {
        return None;
    }
    let body = &bytes[off..off + len];
    let bps = (bits as usize / 8).max(1);
    let n = body.len() / bps;
    // one allocation, and the heap's biggest block is 64 MiB: a WAV past that is refused, not a panic
    let mut data: Vec<i16> = Vec::new();
    data.try_reserve_exact(n).ok()?;
    for i in 0..n {
        let b = &body[i * bps..i * bps + bps];
        let v: i16 = match bits {
            8 => ((b[0] as i16) - 128) << 8,
            16 => i16::from_le_bytes([b[0], b[1]]),
            24 => i16::from_le_bytes([b[1], b[2]]),
            32 => {
                // Float or integer: the fmt tag is not kept by parse_wav; a
                // float looks like |x| <= 1.0 when read as f32.
                let f = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                if f.is_finite() && f.abs() <= 1.0001 {
                    (f * 32767.0) as i16
                } else {
                    i16::from_le_bytes([b[2], b[3]])
                }
            },
            _ => return None,
        };
        data.push(v);
    }
    samples().push(Sample { rate, stereo: ch == 2, data, base: 0, open: false });
    Some(samples().len() - 1)
}

/// A synthesised tone: `wave` 0 sine, 1 square, 2 saw, 3 triangle, 4 noise;
/// `dur` seconds, a short fade in and out. Returns a sample id.
pub fn synth(freq: f64, amp: f64, dur: f64, wave: u32) -> usize {
    let _irq = crate::mm::IrqLock::acquire();
    let n = ((dur.clamp(0.01, 4.0)) * RATE) as usize;
    let mut data: Vec<i16> = Vec::with_capacity(n);
    let mut phase = 0.0f64;
    let mut seed = 0x9E37_79B9u32;
    let fade = (RATE * 0.004) as usize;
    for i in 0..n {
        phase += freq / RATE;
        if phase >= 1.0 {
            phase -= 1.0;
        }
        let s = match wave {
            1 => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            },
            2 => phase * 2.0 - 1.0,
            3 => {
                if phase < 0.5 {
                    phase * 4.0 - 1.0
                } else {
                    3.0 - phase * 4.0
                }
            },
            4 => {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed as f64 / u32::MAX as f64) * 2.0 - 1.0
            },
            _ => libm::sin(phase * core::f64::consts::TAU),
        };
        let env = (i.min(fade) as f64 / fade as f64).min((n - i).min(fade) as f64 / fade as f64);
        // A gentle decay so blips sound like blips.
        let decay = 1.0 - i as f64 / n as f64 * 0.5;
        data.push((s * amp.clamp(0.0, 1.0) * env * decay * 12000.0) as i16);
    }
    samples().push(Sample { rate: RATE as u32, stereo: false, data, base: 0, open: false });
    samples().len() - 1
}

pub fn duration(id: usize) -> f64 {
    match samples().get(id) {
        Some(s) => (s.base as f64 + s.data.len() as f64 / if s.stereo { 2.0 } else { 1.0 }) / s.rate as f64,
        None => 0.0,
    }
}

/// Drop a sample's PCM (a streamed video's finished audio segment): voices
/// on it stop, the id stays valid but silent.
pub fn release(id: usize) {
    let _irq = crate::mm::IrqLock::acquire();
    for v in voices().iter_mut() {
        if v.active && v.sample == id {
            v.active = false;
        }
    }
    if let Some(s) = samples().get_mut(id) {
        s.data = Vec::new();
        s.open = false;
    }
}

// ── Streamed sounds: a video's decoded sound, appended as it is decoded ──────

/// A new, empty streamed sound at `rate` Hz (mono or interleaved stereo).
/// Play it on the music voice like any sample; `music_pos` counts from the
/// stream's start, whatever was trimmed off its front since.
pub fn stream_open(rate: u32, stereo: bool) -> usize {
    let _irq = crate::mm::IrqLock::acquire();
    samples().push(Sample { rate: rate.max(1), stereo, data: Vec::new(), base: 0, open: true });
    samples().len() - 1
}

/// Append decoded frames (interleaved when stereo).
pub fn stream_push(id: usize, pcm: &[i16]) {
    let _irq = crate::mm::IrqLock::acquire();
    if let Some(s) = samples().get_mut(id) {
        if s.open {
            s.data.extend_from_slice(pcm);
        }
    }
}

/// Drop what lies before frame `before` (counted from the stream's start).
pub fn stream_trim(id: usize, before: usize) {
    let _irq = crate::mm::IrqLock::acquire();
    if let Some(s) = samples().get_mut(id) {
        let ch = if s.stereo { 2 } else { 1 };
        let k = before.saturating_sub(s.base).min(s.data.len() / ch);
        if k > 0 {
            s.data.drain(..k * ch);
            s.base += k;
        }
    }
}

/// No more pieces: a voice on it ends at its end.
pub fn stream_close(id: usize) {
    let _irq = crate::mm::IrqLock::acquire();
    if let Some(s) = samples().get_mut(id) {
        s.open = false;
    }
}

/// Frames held so far, counted from the stream's start (the end of what can play).
pub fn stream_end(id: usize) -> usize {
    match samples().get(id) {
        Some(s) => s.base + s.data.len() / if s.stereo { 2 } else { 1 },
        None => 0,
    }
}

/// Start a voice; `pitch` 1.0 = as recorded. Returns the voice index, or None.
pub fn play(id: usize, vol: f32, pan: f32, looping: bool, pitch: f64, music: bool) -> Option<usize> {
    let _irq = crate::mm::IrqLock::acquire();
    let s = samples().get(id)?;
    let step = s.rate as f64 / RATE * pitch.clamp(0.1, 8.0);
    let vs = voices();
    let slot = if music {
        stop_music();
        MAX_VOICES - 1
    } else {
        // A free voice, else the oldest effect (never the music slot).
        (0..MAX_VOICES - 1).find(|&i| !vs[i].active).unwrap_or_else(|| {
            let mut best = 0;
            for i in 0..MAX_VOICES - 1 {
                if vs[i].pos * vs[best].step > vs[best].pos * vs[i].step {
                    best = i;
                }
            }
            best
        })
    };
    vs[slot] = Voice { sample: id, pos: 0.0, step, vol: vol.clamp(0.0, 2.0), pan: pan.clamp(-1.0, 1.0), looping, active: true, paused: false, music };
    if music {
        unsafe {
            MUSIC_VOICE = slot;
            MUSIC_GEN = MUSIC_GEN.wrapping_add(1);
        }
    }
    Some(slot)
}

/// A sound at a point: louder when near, panned by where it is relative to
/// the listener's facing (`set_listener`). `reach` is the distance it fades
/// out over (the host API's `w`; 0 means a default of 24 units).
pub fn play_at(id: usize, x: f32, _y: f32, z: f32, reach: f32, vol: f32, looping: bool) -> Option<usize> {
    let (c, s) = unsafe { LISTENER };
    let dist = libm::sqrtf(x * x + z * z);
    let reach = if reach > 0.0 { reach } else { 24.0 };
    let gain = vol * (1.0 - (dist / reach).clamp(0.0, 1.0)).max(0.0);
    // Right of the listener: the facing rotated by +90 degrees.
    let right = (x * c - z * s) / dist.max(0.001);
    let pan = if dist < 0.5 { 0.0 } else { right.clamp(-1.0, 1.0) * 0.8 };
    play(id, gain, pan, looping, 1.0, false)
}

pub fn set_listener(cos_yaw: f32, sin_yaw: f32) {
    unsafe { LISTENER = (cos_yaw, sin_yaw) };
}

pub fn stop(voice: usize) {
    let _irq = crate::mm::IrqLock::acquire();
    if voice < MAX_VOICES {
        voices()[voice].active = false;
    }
}

// (a NaN -- a game's settings file holding "()" read back as a number -- keeps the old level
// instead of silencing everything: NaN.clamp() stays NaN and the mix turns it into 0)
pub fn set_master(v: f32) {
    if v.is_finite() {
        unsafe { MASTER = v.clamp(0.0, 1.0) };
    }
}

/// The current track's own level (`audio_bgm(path, vol)`).
pub fn set_music_volume(v: f32) {
    if v.is_finite() {
        unsafe { MUSIC_VOL = v.clamp(0.0, 1.0) };
    }
}

/// The player's Music and Sound-effects sliders (the host's `audio_music_volume` /
/// `audio_sfx_volume`), on top of the master: music voices are scaled by the first,
/// every other voice, blip and tone by the second.
static mut MUSIC_USER: f32 = 1.0;
static mut SFX_USER: f32 = 1.0;
pub fn set_music_user(v: f32) {
    if v.is_finite() {
        unsafe { MUSIC_USER = v.clamp(0.0, 1.0) };
    }
}
pub fn set_sfx_user(v: f32) {
    if v.is_finite() {
        unsafe { SFX_USER = v.clamp(0.0, 1.0) };
    }
}

/// An app ended: the next one starts at full volume (a game that turned its master
/// down, or to 0, must not leave every later program silent).
pub fn reset_levels() {
    unsafe {
        MASTER = 1.0;
        MUSIC_VOL = 1.0;
        MUSIC_USER = 1.0;
        SFX_USER = 1.0;
    }
}

fn music_voice() -> Option<&'static mut Voice> {
    let i = unsafe { MUSIC_VOICE };
    if i < MAX_VOICES && voices()[i].music {
        Some(&mut voices()[i])
    } else {
        None
    }
}

pub fn stop_music() {
    let _irq = crate::mm::IrqLock::acquire();
    if let Some(v) = music_voice() {
        v.active = false;
    }
    unsafe { MUSIC_VOICE = MAX_VOICES };
}

pub fn pause_music() {
    let _irq = crate::mm::IrqLock::acquire();
    if let Some(v) = music_voice() {
        v.paused = !v.paused;
    }
}

pub fn seek_music(seconds: f64) {
    let _irq = crate::mm::IrqLock::acquire();
    if let Some(v) = music_voice() {
        let s = &samples()[v.sample];
        let frames = s.data.len() / if s.stereo { 2 } else { 1 };
        v.pos = (seconds.max(0.0) * s.rate as f64).min((s.base + frames) as f64 - 1.0).max(s.base as f64);
    }
}

/// The music's position in seconds as heard now (see [`Heard`]).
pub fn music_pos() -> f64 {
    let _irq = crate::mm::IrqLock::acquire();
    let raw = match music_voice() {
        Some(v) => v.pos / samples()[v.sample].rate.max(1) as f64,
        None => return 0.0,
    };
    heard_at(crate::drivers::mixer::audible_frame()).unwrap_or(raw)
}

/// The render cursor: where the mixer is in the music (a queue ahead of the speaker).
pub fn music_pos_rendered() -> f64 {
    match music_voice() {
        Some(v) => v.pos / samples()[v.sample].rate.max(1) as f64,
        None => 0.0,
    }
}

/// The sample the music voice is on while it sounds (a video player checks
/// it still owns the music voice), None when nothing plays there.
pub fn music_sample() -> Option<usize> {
    music_voice().filter(|v| v.active).map(|v| v.sample)
}

pub fn music_playing() -> bool {
    music_voice().is_some_and(|v| v.active && !v.paused)
}

// ── Visualizer taps (`music_fft_groups`, `music_wave`) ───────────────────────
// Read straight from the decoded sample at the music voice's play position --
// what is sounding right now -- so no ring has to be fed from the mixer and
// both outputs (AC'97 / HDA on x86, HDMI on the Pi) get it for free.

/// Mono -1..1 frame `i` of a sample (0 past either end).
fn mono_at(s: &Sample, i: isize) -> f32 {
    if i < 0 {
        return 0.0;
    }
    let i = i as usize;
    if s.stereo {
        match (s.data.get(i * 2), s.data.get(i * 2 + 1)) {
            (Some(&l), Some(&r)) => (l as f32 + r as f32) * (0.5 / 32768.0),
            _ => 0.0,
        }
    } else {
        s.data.get(i).map(|&v| v as f32 / 32768.0).unwrap_or(0.0)
    }
}

/// The frame the music voice is at when it plays sample `id`, else None.
fn music_frame(id: usize) -> Option<isize> {
    let v = music_voice()?;
    if v.sample != id || !v.active {
        return None;
    }
    Some(v.pos as isize - samples().get(id).map(|s| s.base as isize).unwrap_or(0))
}

/// In-place radix-2 FFT (size a power of two).
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -core::f32::consts::TAU / len as f32;
        let (wr, wi) = (libm::cosf(ang), libm::sinf(ang));
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (ar, ai) = (re[i + k], im[i + k]);
                let (br, bi) = (re[i + k + len / 2], im[i + k + len / 2]);
                let (tr, ti) = (br * cr - bi * ci, br * ci + bi * cr);
                re[i + k] = ar + tr;
                im[i + k] = ai + ti;
                re[i + k + len / 2] = ar - tr;
                im[i + k + len / 2] = ai - ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

/// `groups` log-spaced band magnitudes (30 Hz .. 16 kHz) of the `size`
/// frames around where the music voice is in sample `id` -- the host
/// runtime's `music_fft_groups(id, size, groups)`. Zeros while that sample
/// is not the playing music. A 1024-point FFT is ~50 us: fine per frame.
pub fn fft_groups(id: usize, size: usize, groups: usize) -> Vec<f32> {
    let groups = groups.clamp(1, 256);
    let size = size.clamp(64, 4096).next_power_of_two();
    let Some(s) = samples().get(id) else { return alloc::vec![0.0; groups] };
    let Some(centre) = music_frame(id) else { return alloc::vec![0.0; groups] };
    if music_voice().is_some_and(|v| v.paused) {
        return alloc::vec![0.0; groups];
    }
    let start = centre - size as isize / 2;
    let mut re: Vec<f32> = Vec::with_capacity(size);
    let mut im = alloc::vec![0.0f32; size];
    for i in 0..size {
        let w = 0.5 * (1.0 - libm::cosf(core::f32::consts::TAU * i as f32 / (size - 1) as f32));
        re.push(mono_at(s, start + i as isize) * w);
    }
    fft(&mut re, &mut im);
    let bins = size / 2;
    let rate = s.rate.max(1) as f32;
    let hz_per_bin = rate / size as f32;
    let (lo, hi) = (30.0f32, (rate * 0.5).min(16000.0));
    let norm = 2.0 / (size as f32 * 0.5);
    (0..groups)
        .map(|g| {
            let hz0 = lo * libm::powf(hi / lo, g as f32 / groups as f32);
            let hz1 = lo * libm::powf(hi / lo, (g + 1) as f32 / groups as f32);
            let k0 = ((hz0 / hz_per_bin) as usize).clamp(1, bins - 1);
            let k1 = (libm::ceilf(hz1 / hz_per_bin) as usize).clamp(k0 + 1, bins);
            let sum: f32 = (k0..k1).map(|k| libm::sqrtf(re[k] * re[k] + im[k] * im[k])).sum();
            sum / (k1 - k0) as f32 * norm
        })
        .collect()
}

/// `n` waveform points (-1..1) spanning the next ~`n * 8` frames of the
/// playing music in sample `id` (an oscilloscope trace); zeros otherwise.
pub fn wave(id: usize, n: usize) -> Vec<f32> {
    let n = n.clamp(1, 1024);
    let Some(s) = samples().get(id) else { return alloc::vec![0.0; n] };
    let Some(at) = music_frame(id) else { return alloc::vec![0.0; n] };
    if music_voice().is_some_and(|v| v.paused) {
        return alloc::vec![0.0; n];
    }
    let span = (s.rate as usize / 40).max(n); // ~25 ms of signal
    (0..n).map(|i| mono_at(s, at + (i * span / n) as isize)).collect()
}

// ── Tone voices (`audio_tone`): continuous oscillators a program retunes every
// frame -- the host runtime's chip-style synth that Ling Arcade's NES APU
// drives (2 pulses, triangle, noise). Amplitude and frequency glide toward
// their targets (`audio_glide`) so per-frame updates ramp instead of click.

const MAX_TONES: usize = 8;

#[derive(Clone, Copy)]
struct Tone {
    freq: f32,
    amp: f32,
    target_freq: f32,
    target_amp: f32,
    /// 0 sine, 1 square, 2 saw, 3 triangle, 4 noise
    wave: u8,
    duty: f32,
    pan: f32,
    phase: f32,
    lfsr: u32,
}

const TONE_OFF: Tone = Tone { freq: 220.0, amp: 0.0, target_freq: 220.0, target_amp: 0.0, wave: 0, duty: 0.5, pan: 0.0, phase: 0.0, lfsr: 1 };
static mut TONES: [Tone; MAX_TONES] = [TONE_OFF; MAX_TONES];
/// Glide time constants in ms (amplitude, frequency); <= 0 = the default.
static mut GLIDE_MS: (f32, f32) = (5.0, 2.0);

fn tones() -> &'static mut [Tone; MAX_TONES] {
    unsafe { &mut *(&raw mut TONES) }
}

/// Retune voice `idx`: frequency (Hz), amplitude (0..1), waveform, square duty, pan (-1..1).
pub fn set_tone(idx: usize, freq: f32, amp: f32, wave: u8, duty: f32, pan: f32) {
    if let Some(t) = tones().get_mut(idx) {
        if t.target_amp <= 0.0 && t.amp <= 1e-4 {
            t.freq = freq.max(0.0); // a voice starting from silence starts on pitch
        }
        t.target_freq = freq.max(0.0);
        t.target_amp = amp.clamp(0.0, 1.0);
        t.wave = wave;
        t.duty = duty.clamp(0.01, 0.99);
        t.pan = pan.clamp(-1.0, 1.0);
    }
}

pub fn set_tone_glide(amp_ms: f32, freq_ms: f32) {
    unsafe {
        GLIDE_MS = (if amp_ms > 0.0 { amp_ms } else { 5.0 }, if freq_ms > 0.0 { freq_ms } else { 2.0 });
    }
}

/// Every tone voice fades out (an app that set them is closing or freezing).
pub fn tones_off() {
    for t in tones().iter_mut() {
        t.target_amp = 0.0;
    }
}

fn tones_active() -> bool {
    tones().iter().any(|t| t.amp > 1e-4 || t.target_amp > 0.0)
}

fn mix_tones(out: &mut [i16]) {
    let frames = out.len() / 2;
    let master = unsafe { MASTER * SFX_USER };
    let (ams, fms) = unsafe { GLIDE_MS };
    // one-pole smoothing coefficients per sample
    let ka = 1.0 - libm::expf(-1000.0 / (ams * RATE as f32));
    let kf = 1.0 - libm::expf(-1000.0 / (fms * RATE as f32));
    for t in tones().iter_mut() {
        if t.amp <= 1e-4 && t.target_amp <= 0.0 {
            t.amp = 0.0;
            continue;
        }
        let (lg, rg) = if t.pan <= 0.0 { (1.0, 1.0 + t.pan) } else { (1.0 - t.pan, 1.0) };
        for fr in 0..frames {
            t.amp += (t.target_amp - t.amp) * ka;
            t.freq += (t.target_freq - t.freq) * kf;
            let step = t.freq / RATE as f32;
            let p = t.phase;
            let v = match t.wave {
                1 => {
                    if p < t.duty {
                        1.0
                    } else {
                        -1.0
                    }
                },
                2 => 2.0 * p - 1.0,
                3 => 1.0 - 4.0 * libm::fabsf(p - 0.5),
                4 => {
                    if t.lfsr & 1 == 1 {
                        1.0
                    } else {
                        -1.0
                    }
                },
                _ => libm::sinf(p * core::f32::consts::TAU),
            };
            let mut np = p + step;
            if np >= 1.0 {
                np -= libm::floorf(np);
                if t.wave == 4 {
                    // 15-bit LFSR, clocked once per period (the NES noise channel's shape)
                    let bit = (t.lfsr ^ (t.lfsr >> 1)) & 1;
                    t.lfsr = (t.lfsr >> 1) | (bit << 14);
                }
            }
            t.phase = np;
            let s = v * t.amp * master * 12000.0;
            let ol = out[fr * 2] as f32 + s * lg;
            let or = out[fr * 2 + 1] as f32 + s * rg;
            out[fr * 2] = ol.clamp(-32767.0, 32767.0) as i16;
            out[fr * 2 + 1] = or.clamp(-32767.0, 32767.0) as i16;
        }
    }
}

/// Anything audible right now (so an output that is otherwise idle keeps running).
pub fn active() -> bool {
    voices().iter().any(|v| v.active && !v.paused) || tones_active()
}

/// Add the voices' next `frames` frames to `out` (interleaved stereo i16),
/// saturating. Silent voices cost nothing.
pub fn mix_into(out: &mut [i16]) {
    mix_tones(out);
    let frames = out.len() / 2;
    let mv = unsafe { MUSIC_VOICE };
    let music_before = music_voice().map(|v| (v.pos, v.step, v.active && !v.paused, v.sample));
    let master = unsafe { MASTER };
    let mvol = unsafe { MUSIC_VOL * MUSIC_USER };
    let fxvol = unsafe { SFX_USER };
    for v in voices().iter_mut() {
        if !v.active || v.paused {
            continue;
        }
        let Some(s) = samples().get(v.sample) else {
            v.active = false;
            continue;
        };
        let ch = if s.stereo { 2 } else { 1 };
        let n = s.data.len() / ch;
        if n < 2 {
            // a stream whose next piece is not decoded yet waits
            if !s.open {
                v.active = false;
            }
            continue;
        }
        let base = s.base as f64;
        let gain = v.vol * master * if v.music { mvol } else { fxvol };
        let (lg, rg) = if v.pan <= 0.0 { (1.0, 1.0 + v.pan) } else { (1.0 - v.pan, 1.0) };
        for f in 0..frames {
            if v.pos < base {
                v.pos = base; // the front was trimmed away under the voice
            }
            if v.pos - base >= (n - 1) as f64 {
                if s.open {
                    break; // a stream ran dry: wait (the position, the clock, holds)
                }
                if v.looping {
                    v.pos -= (n - 1) as f64;
                } else {
                    v.active = false;
                    break;
                }
            }
            let rel = v.pos - base;
            let i = rel as usize;
            let t = (rel - i as f64) as f32;
            let i1 = (i + 1).min(n - 1);
            let (l, r) = if s.stereo {
                let l = s.data[i * 2] as f32 * (1.0 - t) + s.data[i1 * 2] as f32 * t;
                let r = s.data[i * 2 + 1] as f32 * (1.0 - t) + s.data[i1 * 2 + 1] as f32 * t;
                (l, r)
            } else {
                let m = s.data[i] as f32 * (1.0 - t) + s.data[i1] as f32 * t;
                (m, m)
            };
            let ol = out[f * 2] as f32 + l * gain * lg;
            let or = out[f * 2 + 1] as f32 + r * gain * rg;
            out[f * 2] = ol.clamp(-32767.0, 32767.0) as i16;
            out[f * 2 + 1] = or.clamp(-32767.0, 32767.0) as i16;
            v.pos += v.step;
        }
    }
    if let Some((p0, step, moving, id)) = music_before {
        if mv < MAX_VOICES {
            let rate = samples().get(id).map(|s| s.rate.max(1) as f64).unwrap_or(RATE);
            let v = &voices()[mv];
            let p1 = if v.sample == id { v.pos } else { p0 };
            // a stream that ran dry holds its position: spread what actually moved over the block
            let moved = if p1 >= p0 { (p1 - p0) / frames.max(1) as f64 } else { step };
            let spf = if moving { moved / rate } else { 0.0 };
            heard_note(crate::drivers::mixer::block_start(), frames, p0 / rate, p1 / rate, spf);
        }
    }
}

/// How far voice `voice` is into sample `id`, in seconds; None once it ended or plays something
/// else (fsx_burn.rs runs its animation on this clock, so a stalled frame never splits picture and sound).
pub fn voice_pos(voice: usize, id: usize) -> Option<f64> {
    let v = voices().get(voice)?;
    if !v.active || v.sample != id {
        return None;
    }
    let s = samples().get(id)?;
    Some(v.pos / s.rate.max(1) as f64)
}
