//! OS audio mixer + pentatonic synthesizer -- the audio layer between the
//! output DMA ring (`ac97.rs` or `hda.rs`) and everything that makes sound.
//!
//! Model, per the design request: Windows-audio-shaped, deliberately NOT a
//! JACK-style routing graph. Every app (window kind / shell / player) owns
//! one implicit stream with its own volume; all active streams are
//! software-mixed into the one hardware ring ("shared mode"). One stream
//! may claim *exclusive mode* (ASIO-flavored takeover): while claimed,
//! only that stream is audible -- others keep rendering into silence
//! rather than blocking, so releasing exclusivity is glitchless.
//!
//! The synthesizer is real synthesis, not samples: pentatonic-tuned
//! (C-major pentatonic, two octaves) triangle/sine voices with attack/
//! exponential-decay envelopes, mixed at the AC'97's native 48 kHz. The
//! sine is Bhaskara's 7th-century approximation (no libm in no_std;
//! max error ~1.8% -- inaudible for chimes). "Sound themes" are named
//! per-event note sequences (boot/click/open/close/error/login), all in
//! the same pentatonic tuning so nothing can sound dissonant next to
//! anything else -- the property the pentatonic scale was chosen for.
//!
//! The tape player ("play" in lsh) is a stream like any other: a seeded
//! generative pentatonic track. Separately, a real *PCM sample* source
//! (`pcm_*`) plays actual WAV audio from lingfs -- now that multi-block
//! files exist, a real song fits -- resampling to 48 kHz and mixing it in
//! alongside the synth voices. That's what the desktop Media player uses.

use alloc::string::String;
use alloc::vec::Vec;

use crate::drivers::{ac97, hda};

/// Which output controller `init` brought up: ICH AC'97 or Intel HD Audio
/// (VirtualBox's default for new VMs). Both take the same pull-model
/// `pump(render)`, 48 kHz 16-bit stereo, FRAMES_PER_BUFFER frames a call.
const BACKEND_NONE: u8 = 0;
const BACKEND_AC97: u8 = 1;
const BACKEND_HDA: u8 = 2;
static mut BACKEND: u8 = BACKEND_NONE;

pub const SAMPLE_RATE: u32 = 48_000;

// -- Streams (one per app) --------------------------------------------------

pub const APP_SYSTEM: usize = 0; // jingles, UI feedback
pub const APP_SHELL: usize = 1; // lsh beeps
pub const APP_PLAYER: usize = 2; // the tape player
pub const APP_ABOUT: usize = 3;
pub const APP_SETTINGS: usize = 4;
pub const APP_FILES: usize = 5;
pub const APP_DAW: usize = 6; // the LingStation window
pub const NUM_STREAMS: usize = 7;

static STREAM_NAMES: [&str; NUM_STREAMS] =
    ["system", "shell", "player", "about", "settings", "files", "lingstation"];

static mut STREAM_VOL: [u32; NUM_STREAMS] = [80; NUM_STREAMS];
static mut MASTER_VOL: u32 = 80;
static mut EXCLUSIVE: i32 = -1;

pub fn stream_count() -> usize {
    NUM_STREAMS
}

pub fn stream_name(i: usize) -> &'static str {
    STREAM_NAMES.get(i).copied().unwrap_or("")
}

pub fn stream_volume(i: usize) -> u32 {
    unsafe { STREAM_VOL.get(i).copied().unwrap_or(0) }
}

pub fn set_stream_volume(i: usize, vol: u32) {
    unsafe {
        if i < NUM_STREAMS {
            STREAM_VOL[i] = vol.min(100);
        }
    }
}

pub fn master_volume() -> u32 {
    unsafe { MASTER_VOL }
}

pub fn set_master_volume(vol: u32) {
    unsafe { MASTER_VOL = vol.min(100) };
}

/// Claim exclusive mode for a stream (ASIO-style takeover): every other
/// stream renders but is muted at the mix stage. Returns false if a
/// different stream already holds it.
pub fn exclusive_claim(i: usize) -> bool {
    unsafe {
        if EXCLUSIVE >= 0 && EXCLUSIVE != i as i32 {
            return false;
        }
        EXCLUSIVE = i as i32;
        true
    }
}

pub fn exclusive_release(i: usize) {
    unsafe {
        if EXCLUSIVE == i as i32 {
            EXCLUSIVE = -1;
        }
    }
}

pub fn exclusive_holder() -> i32 {
    unsafe { EXCLUSIVE }
}

// -- Synth voices ------------------------------------------------------------

const MAX_VOICES: usize = 12;

#[derive(Clone, Copy)]
struct Voice {
    active: bool,
    stream: usize,
    phase: f64,      // 0..1
    phase_inc: f64,  // freq / SAMPLE_RATE
    level: f64,      // current envelope level 0..1
    attack: f64,     // per-sample level increment while attacking
    decay: f64,      // per-sample multiplicative decay after attack
    attacking: bool,
    sine: bool,      // false = triangle
    /// Samples until this voice starts (scheduler delay for jingle notes).
    delay: u32,
    /// Stereo pan: -1.0 hard left, 0 center, +1.0 hard right.
    pan: f64,
    /// Samples held at full level before the decay starts (sustained tones).
    hold: u32,
}

const SILENT_VOICE: Voice = Voice {
    active: false,
    stream: 0,
    phase: 0.0,
    phase_inc: 0.0,
    level: 0.0,
    attack: 0.0,
    decay: 0.0,
    attacking: false,
    sine: false,
    delay: 0,
    pan: 0.0,
    hold: 0,
};

static mut VOICES: [Voice; MAX_VOICES] = [SILENT_VOICE; MAX_VOICES];

// Downmix everything to centre (Settings > Audio > Channels = Mono).
static mut MONO: bool = false;
// Output level meter (0..100 per channel), fast attack / slow decay.
static mut PEAK_L: u32 = 0;
static mut PEAK_R: u32 = 0;

pub fn set_mono(m: bool) {
    unsafe { MONO = m };
}
pub fn mono() -> bool {
    unsafe { MONO }
}
pub fn output_level_l() -> u32 {
    unsafe { PEAK_L }
}
pub fn output_level_r() -> u32 {
    unsafe { PEAK_R }
}

// Honest device facts (the AC'97 driver is fixed 48kHz/16-bit stereo, PCM-out
// only -- see its module doc), surfaced read-only in the Audio settings tab.
/// The output controller in use, for Settings > Audio.
pub fn device_name() -> &'static str {
    if usb_active() {
        return crate::drivers::usbaudio::name();
    }
    match unsafe { BACKEND } {
        BACKEND_NONE => "none",
        b if b == BACKEND_AC97 => "AC'97",
        _ => hda::label(),
    }
}

pub fn sample_rate() -> u32 {
    SAMPLE_RATE
}
pub fn bit_depth() -> u32 {
    if usb_active() {
        return crate::drivers::usbaudio::bits();
    }
    16
}
pub fn buffer_frames() -> u32 {
    if usb_active() {
        return SAMPLE_RATE / 1000; // one 1 ms packet's worth (125 µs packets at high speed)
    }
    match unsafe { BACKEND } {
        BACKEND_HDA => hda::OUT_FRAMES as u32,
        _ => ac97::FRAMES_PER_BUFFER as u32,
    }
}
/// Buffers kept queued ahead of the one playing (the latency budget).
pub fn ring_buffers() -> u32 {
    if usb_active() {
        return crate::drivers::usbaudio::target_ms();
    }
    match unsafe { BACKEND } {
        BACKEND_HDA => hda::target() as u32,
        _ => ac97::target() as u32,
    }
}

// -- The audible clock -------------------------------------------------------
// Everything rendered is counted in output frames (RENDERED). Each pump reads
// how much of that the device still holds (its DMA position), so "the frame
// coming out of the speaker now" is RENDERED - queued at that moment, carried
// forward at 48 kHz between pumps. Programs' music position (sfx::music_pos),
// video sync and rhythm games run on this clock, not on the render cursor that
// runs a whole queue ahead of what is heard.
static mut RENDERED: u64 = 0;
static mut BLOCK_START: u64 = 0;
static mut AUD_FRAME: u64 = 0;
static mut AUD_US: u64 = 0;
static mut AUD_LAST: u64 = 0;
// how long the pump goes between calls (µs, a slowly decaying peak): the
// queue is kept just deep enough to ride that out
static mut GAP_EST_US: u64 = 50_000;
static mut LAST_PUMP_US: u64 = 0;

/// The output frame the block being rendered starts at (sfx stamps its music history with it).
pub fn block_start() -> u64 {
    unsafe { BLOCK_START }
}
/// Output frames rendered so far.
pub fn rendered_frames() -> u64 {
    unsafe { RENDERED }
}
/// The output frame being heard now (never ahead of what was rendered, never backwards).
pub fn audible_frame() -> u64 {
    unsafe {
        let now = crate::arch::timer::now_us();
        let mut f = AUD_FRAME + now.saturating_sub(AUD_US) * SAMPLE_RATE as u64 / 1_000_000;
        if f > RENDERED {
            f = RENDERED;
        }
        if f < AUD_LAST {
            f = AUD_LAST;
        }
        AUD_LAST = f;
        f
    }
}
/// Seconds between what is rendered and what is heard right now.
pub fn output_latency() -> f64 {
    (rendered_frames().saturating_sub(audible_frame())) as f64 / SAMPLE_RATE as f64
}
/// `audio latency` (Terminal / SSH); `audio clock` also starts a 20 s test tone as music
/// (once) and reports its heard position against the mixer's render cursor.
pub fn latency_report() -> alloc::string::String {
    let (q, b) = (ring_buffers(), buffer_frames());
    alloc::format!(
        "audio: {} buffers x {} frames queued ({:.1} ms), heard {:.1} ms behind the mixer, pump gap {:.1} ms, music heard {:.3} s rendered {:.3} s\n",
        q,
        b,
        (q * b) as f64 * 1000.0 / SAMPLE_RATE as f64,
        output_latency() * 1000.0,
        unsafe { GAP_EST_US } as f64 / 1000.0,
        crate::drivers::sfx::music_pos(),
        crate::drivers::sfx::music_pos_rendered()
    )
}
fn clock_test() -> alloc::string::String {
    if crate::drivers::sfx::music_sample().is_none() {
        let id = crate::drivers::sfx::synth(440.0, 0.3, 4.0, 0);
        crate::drivers::sfx::play(id, 1.0, 0.0, true, 1.0, true);
    }
    latency_report()
}

fn pace_queue(now: u64) {
    unsafe {
        let gap = if LAST_PUMP_US == 0 { 0 } else { now.saturating_sub(LAST_PUMP_US) };
        LAST_PUMP_US = now;
        // a stall past 200 ms (a load) underruns whatever the depth; don't let it pin the queue at max
        let g = gap.min(200_000);
        // the peak decays 1/600 of the time that passes (a 40 ms spike is remembered ~25 s): at
        // 1% the queue shrank back within seconds of a garbage-collector pause and the next one
        // (LingForFun on the user's PC: dozens a minute, up to ~40 ms) caught it short -- stutter
        GAP_EST_US = g.max(GAP_EST_US.saturating_sub(gap / 600));
        // 2x the worst recent gap + 6 ms of slack, in whole buffers, + the one playing
        // (1.5x + 4 ms let a one-off long frame through: a 5 ms dropout in a 30 s run)
        let want_us = GAP_EST_US * 2 + 6_000;
        let want_frames = want_us * SAMPLE_RATE as u64 / 1_000_000;
        // while the screen recorder takes the sound, the queue may grow (no dropouts) but
        // not shrink: its depth is the distance between when a sound is made and when it
        // is heard, and the recording keeps picture and sound together on that distance
        let hold = HOLD_DEPTH.load(core::sync::atomic::Ordering::Relaxed);
        match BACKEND {
            BACKEND_HDA => {
                let n = want_frames.div_ceil(hda::OUT_FRAMES as u64) as usize + 1;
                hda::set_target(if hold { n.max(hda::target()) } else { n })
            },
            BACKEND_AC97 => {
                let n = want_frames.div_ceil(ac97::FRAMES_PER_BUFFER as u64) as usize + 1;
                ac97::set_target(if hold { n.max(ac97::target()) } else { n })
            },
            _ => {}
        }
    }
}

static HOLD_DEPTH: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The screen recorder holds the output queue's depth (it may grow, never shrink) while it records sound.
pub fn hold_depth(on: bool) {
    if on {
        // a queue still draining toward a lower target stops there, at its depth now
        match unsafe { BACKEND } {
            BACKEND_HDA => hda::set_target(hda::target().max(hda::queued_frames().div_ceil(hda::OUT_FRAMES))),
            BACKEND_AC97 => ac97::set_target(ac97::target().max(ac97::queued_frames().div_ceil(ac97::FRAMES_PER_BUFFER))),
            _ => {}
        }
    }
    HOLD_DEPTH.store(on, core::sync::atomic::Ordering::Relaxed);
}

fn stamp_audible(now: u64, usb: bool) {
    let queued = if usb {
        (ring_buffers() * buffer_frames()) as u64
    } else {
        match unsafe { BACKEND } {
            BACKEND_HDA => hda::queued_frames() as u64,
            BACKEND_AC97 => ac97::queued_frames() as u64,
            _ => 0,
        }
    };
    unsafe {
        let f = RENDERED.saturating_sub(queued);
        // the device position only moves forward; ignore a stale read that would step back
        AUD_FRAME = f.max(AUD_LAST.min(RENDERED));
        AUD_US = now;
    }
}
/// An input (HD Audio mic / line-in jack, a USB interface's inputs) can be recorded from.
pub fn has_capture() -> bool {
    ((unsafe { BACKEND }) == BACKEND_HDA && hda::input_count() > 0) || crate::drivers::usbaudio::has_capture()
}

/// Play a short test ding panned to one side: -1 left, 0 centre, +1 right.
pub fn test_tone(pan: i32) {
    let p = pan.clamp(-1, 1) as f64;
    unsafe {
        let voices = &mut *&raw mut VOICES;
        let Some(v) = voices.iter_mut().find(|v| !v.active) else { return };
        let dur_samples = 0.28 * SAMPLE_RATE as f64;
        *v = Voice {
            active: true,
            stream: APP_SETTINGS,
            phase: 0.0,
            phase_inc: 880.0 / SAMPLE_RATE as f64, // A5
            level: 0.0,
            attack: 1.0 / (0.004 * SAMPLE_RATE as f64),
            decay: libm_free_exp_decay(dur_samples),
            attacking: true,
            sine: true,
            delay: 0,
            pan: p,
            hold: 0,
        };
    }
}

/// A steady sine of `freq_hz` for `ms` (then a 30 ms fade): the `audio tone`
/// test signal, easy to check for gaps on a recording.
pub fn tone(freq_hz: u32, ms: u32) {
    unsafe {
        let voices = &mut *&raw mut VOICES;
        let Some(v) = voices.iter_mut().find(|v| !v.active) else { return };
        *v = Voice {
            active: true,
            stream: APP_SETTINGS,
            phase: 0.0,
            phase_inc: freq_hz.clamp(20, 20_000) as f64 / SAMPLE_RATE as f64,
            level: 0.0,
            attack: 1.0 / (0.004 * SAMPLE_RATE as f64),
            decay: libm_free_exp_decay(0.03 * SAMPLE_RATE as f64),
            attacking: true,
            sine: true,
            delay: 0,
            pan: 0.0,
            hold: (ms.min(60_000) as u64 * SAMPLE_RATE as u64 / 1000) as u32,
        };
    }
}

/// Bhaskara I's sine approximation over 0..pi, mirrored for the full
/// cycle. Input phase 0..1, output -1..1.
fn fast_sin(phase: f64) -> f64 {
    let (p, sign) = if phase < 0.5 { (phase * 2.0, 1.0) } else { ((phase - 0.5) * 2.0, -1.0) };
    // x in 0..pi expressed via p in 0..1: sin = 16p(1-p) / (5 - 4p(1-p))
    let t = p * (1.0 - p);
    sign * (16.0 * t) / (5.0 - 4.0 * t)
}

fn triangle(phase: f64) -> f64 {
    if phase < 0.25 {
        phase * 4.0
    } else if phase < 0.75 {
        2.0 - phase * 4.0
    } else {
        phase * 4.0 - 4.0
    }
}

/// Start one voice. `freq_centihz` is Hz*100 (flat-u64 FFI has no floats),
/// `dur_ms` shapes the decay so the note audibly ends around then,
/// `delay_ms` schedules it into the future (jingle sequencing).
pub fn note(stream: usize, freq_centihz: u32, dur_ms: u32, delay_ms: u32, sine: bool) {
    let freq = freq_centihz as f64 / 100.0;
    if freq <= 0.0 {
        return;
    }
    unsafe {
        let voices = &mut *&raw mut VOICES;
        let Some(v) = voices.iter_mut().find(|v| !v.active) else { return };
        // Decay constant: level falls to ~1% over dur_ms.
        let dur_samples = (dur_ms.max(30) as f64 / 1000.0) * SAMPLE_RATE as f64;
        *v = Voice {
            active: true,
            stream: stream.min(NUM_STREAMS - 1),
            phase: 0.0,
            phase_inc: freq / SAMPLE_RATE as f64,
            level: 0.0,
            attack: 1.0 / (0.004 * SAMPLE_RATE as f64), // 4ms attack
            decay: libm_free_exp_decay(dur_samples),
            attacking: true,
            sine,
            delay: (delay_ms as u64 * SAMPLE_RATE as u64 / 1000) as u32,
            pan: 0.0,
            hold: 0,
        };
    }
}

/// decay-per-sample d with d^n = 0.01 for n = dur_samples, computed
/// without libm: d = exp(ln(0.01)/n), approximated via (1 + x/k)^k with
/// enough halvings -- cheap, once per note.
fn libm_free_exp_decay(dur_samples: f64) -> f64 {
    let x = -4.605_170_186 / dur_samples; // ln(0.01)
    // exp(x) ~ (1 + x/64)^64 for the small negative x this sees.
    let mut base = 1.0 + x / 64.0;
    for _ in 0..6 {
        base *= base;
    }
    base.clamp(0.0, 0.999_999)
}

// -- Pentatonic scale + sound themes ----------------------------------------

/// C-major pentatonic, C4..A5, in centihertz. Every jingle and the
/// generative tape track index into this -- one tuning everywhere.
pub const PENTATONIC_CENTIHZ: [u32; 10] = [
    26163, 29366, 32963, 39200, 44000, // C4 D4 E4 G4 A4
    52325, 58733, 65925, 78399, 88000, // C5 D5 E5 G5 A5
];

pub const EVENT_BOOT: usize = 0;
pub const EVENT_CLICK: usize = 1;
pub const EVENT_OPEN: usize = 2;
pub const EVENT_CLOSE: usize = 3;
pub const EVENT_ERROR: usize = 4;
pub const EVENT_LOGIN: usize = 5;
pub const EVENT_PING: usize = 6; // Messenger attention nudge

/// One jingle note: (pentatonic index, start delay ms, duration ms).
type JingleNote = (usize, u32, u32);

struct SoundTheme {
    name: &'static str,
    /// Indexed by EVENT_*; a slice of scheduled notes (empty = silent).
    events: [&'static [JingleNote]; 7],
}

static SOUND_THEMES: [SoundTheme; 3] = [
    SoundTheme {
        name: "Dawn Chimes",
        events: [
            // boot: rising C-E-G-A-C5 sparkle
            &[(0, 0, 350), (2, 120, 350), (3, 240, 350), (4, 360, 400), (5, 520, 700)],
            &[(7, 0, 60)],                            // click: short high tick
            &[(3, 0, 140), (5, 70, 220)],             // open: two rising
            &[(5, 0, 140), (3, 70, 220)],             // close: two falling
            &[(1, 0, 260), (1, 180, 380)],            // error: low double-knock
            &[(2, 0, 200), (4, 130, 200), (7, 260, 500)], // login: triad up
            &[(8, 0, 130), (9, 110, 260)],            // ping: bright quick two-note
        ],
    },
    SoundTheme {
        name: "Jade Bells",
        events: [
            &[(4, 0, 500), (7, 200, 500), (9, 400, 800)],
            &[(9, 0, 50)],
            &[(5, 0, 180), (8, 90, 260)],
            &[(8, 0, 180), (5, 90, 260)],
            &[(0, 0, 300), (0, 220, 420)],
            &[(4, 0, 240), (7, 150, 240), (9, 300, 600)],
            &[(9, 0, 140), (9, 120, 280)],
        ],
    },
    SoundTheme {
        name: "Silent",
        events: [&[], &[], &[], &[], &[], &[], &[]],
    },
];

static mut SOUND_THEME: usize = 0;

pub fn sound_theme_count() -> usize {
    SOUND_THEMES.len()
}

pub fn sound_theme() -> usize {
    unsafe { SOUND_THEME }
}

pub fn set_sound_theme(i: usize) {
    unsafe { SOUND_THEME = i % SOUND_THEMES.len() };
}

pub fn sound_theme_name(i: usize) -> &'static str {
    SOUND_THEMES.get(i).map(|t| t.name).unwrap_or("")
}

/// Fire one UI sound event through the active sound theme (system stream,
/// shared mode -- a click never needs exclusivity).
pub fn jingle(event: usize) {
    let theme = &SOUND_THEMES[unsafe { SOUND_THEME } % SOUND_THEMES.len()];
    let Some(notes) = theme.events.get(event) else { return };
    for &(pent, delay, dur) in notes.iter() {
        note(APP_SYSTEM, PENTATONIC_CENTIHZ[pent % 10], dur, delay, event == EVENT_BOOT);
    }
}

// -- Tape player (generative pentatonic track) -------------------------------

pub const PLAYER_STOPPED: u32 = 0;
pub const PLAYER_PLAYING: u32 = 1;
pub const PLAYER_PAUSED: u32 = 2;

static mut PLAYER_STATE: u32 = PLAYER_STOPPED;
static mut PLAYER_POS_SAMPLES: u64 = 0;
static mut PLAYER_LEN_SAMPLES: u64 = 0;
static mut PLAYER_SEED: u64 = 0;
static mut PLAYER_NEXT_NOTE_AT: u64 = 0;
static mut PLAYER_RNG: u64 = 0;

/// Load a generative track: `seed` picks the melody, `len_ms` its length.
/// Real state for a real transport -- position advances only while
/// samples are actually rendered into the ring.
pub fn player_load(seed: u64, len_ms: u32) {
    unsafe {
        PLAYER_SEED = seed;
        PLAYER_RNG = seed | 1;
        PLAYER_POS_SAMPLES = 0;
        PLAYER_NEXT_NOTE_AT = 0;
        PLAYER_LEN_SAMPLES = len_ms as u64 * SAMPLE_RATE as u64 / 1000;
        PLAYER_STATE = PLAYER_STOPPED;
    }
}

pub fn player_play() {
    unsafe {
        if PLAYER_LEN_SAMPLES > 0 {
            PLAYER_STATE = PLAYER_PLAYING;
        }
    }
}

pub fn player_pause() {
    unsafe {
        if PLAYER_STATE == PLAYER_PLAYING {
            PLAYER_STATE = PLAYER_PAUSED;
        } else if PLAYER_STATE == PLAYER_PAUSED {
            PLAYER_STATE = PLAYER_PLAYING;
        }
    }
}

pub fn player_stop() {
    unsafe {
        PLAYER_STATE = PLAYER_STOPPED;
        PLAYER_POS_SAMPLES = 0;
        PLAYER_NEXT_NOTE_AT = 0;
        PLAYER_RNG = PLAYER_SEED | 1;
    }
}

pub fn player_state() -> u32 {
    unsafe { PLAYER_STATE }
}

pub fn player_pos_ms() -> u64 {
    unsafe { PLAYER_POS_SAMPLES * 1000 / SAMPLE_RATE as u64 }
}

pub fn player_len_ms() -> u64 {
    unsafe { PLAYER_LEN_SAMPLES * 1000 / SAMPLE_RATE as u64 }
}

static mut LINE_BUF: [u8; 64] = [0; 64];

/// The tape deck's one-line ASCII transport, built kernel-side (a
/// variable-length bar is a loop `.ling` can't write):
/// `[>] |######--------------| 00:12 / 01:00`. State glyph: `>` playing,
/// `=` paused, `.` stopped. Redraw with `\r` for the in-place effect.
pub fn player_line() -> &'static str {
    let pos = player_pos_ms();
    let len = player_len_ms().max(1);
    let filled = ((pos * 20) / len).min(20) as usize;
    let state = player_state();
    unsafe {
        let buf = &mut *&raw mut LINE_BUF;
        let mut n = 0;
        buf[n] = b'[';
        buf[n + 1] = match state {
            PLAYER_PLAYING => b'>',
            PLAYER_PAUSED => b'=',
            _ => b'.',
        };
        buf[n + 2] = b']';
        buf[n + 3] = b' ';
        buf[n + 4] = b'|';
        n += 5;
        for i in 0..20 {
            buf[n + i] = if i < filled { b'#' } else { b'-' };
        }
        n += 20;
        buf[n] = b'|';
        buf[n + 1] = b' ';
        n += 2;
        for (t, sep) in [(pos, true), (len, false)] {
            let total_s = t / 1000;
            let (m, s) = (total_s / 60, total_s % 60);
            buf[n] = b'0' + ((m / 10) % 10) as u8;
            buf[n + 1] = b'0' + (m % 10) as u8;
            buf[n + 2] = b':';
            buf[n + 3] = b'0' + (s / 10) as u8;
            buf[n + 4] = b'0' + (s % 10) as u8;
            n += 5;
            if sep {
                buf[n] = b' ';
                buf[n + 1] = b'/';
                buf[n + 2] = b' ';
                n += 3;
            }
        }
        core::str::from_utf8(&buf[..n]).unwrap_or("")
    }
}

fn player_advance(frames: usize) {
    unsafe {
        if PLAYER_STATE != PLAYER_PLAYING {
            return;
        }
        // Schedule melody notes as playback crosses eighth-note boundaries
        // (120 BPM -> 250ms per eighth). xorshift picks pentatonic steps;
        // same seed = same melody, honestly reproducible.
        let step = SAMPLE_RATE as u64 / 4;
        let end = PLAYER_POS_SAMPLES + frames as u64;
        while PLAYER_NEXT_NOTE_AT < end {
            PLAYER_RNG ^= PLAYER_RNG << 13;
            PLAYER_RNG ^= PLAYER_RNG >> 7;
            PLAYER_RNG ^= PLAYER_RNG << 17;
            let r = PLAYER_RNG;
            // Rest ~1/4 of the time; otherwise a note, longer on beats.
            if r % 4 != 0 {
                let idx = (r >> 8) as usize % 10;
                let on_beat = (PLAYER_NEXT_NOTE_AT / step) % 4 == 0;
                let dur = if on_beat { 420 } else { 200 };
                note(APP_PLAYER, PENTATONIC_CENTIHZ[idx], dur, 0, on_beat);
            }
            PLAYER_NEXT_NOTE_AT += step;
        }
        PLAYER_POS_SAMPLES = end;
        if PLAYER_POS_SAMPLES >= PLAYER_LEN_SAMPLES {
            player_stop();
        }
    }
}

// -- The mix ------------------------------------------------------------------

// -- PCM sample source (real WAV playback) ----------------------------------
// Plays raw PCM from a caller-owned buffer (media.rs's lingfs read buffer),
// resampled to 48 kHz and summed into the mix on the player stream. u8 and
// s16le, mono or stereo. Fractional resampling via a 16.16 cursor.
pub const PCM_STOPPED: u32 = 0;
pub const PCM_PLAYING: u32 = 1;
pub const PCM_PAUSED: u32 = 2;

static mut PCM_PTR: *const u8 = core::ptr::null();
static mut PCM_FRAMES: u64 = 0; // total source frames
static mut PCM_RATE: u32 = 22050;
static mut PCM_CH: u8 = 1;
static mut PCM_BITS: u8 = 8;
static mut PCM_POS: u64 = 0; // 16.16 fixed-point source frame index
static mut PCM_STEP: u64 = 0; // src frames per output frame, 16.16
static mut PCM_STATE: u32 = PCM_STOPPED;
static mut PCM_LOOP: bool = false; // wrap to start at end instead of stopping

/// Point the PCM player at a raw sample buffer. `frames` is the number of
/// whole sample frames (already accounting for channels). The buffer must
/// outlive playback (media.rs keeps it in a static). Does not start.
pub fn pcm_load(ptr: *const u8, frames: u64, rate: u32, channels: u8, bits: u8) {
    unsafe {
        PCM_PTR = ptr;
        PCM_FRAMES = frames;
        PCM_RATE = rate.max(1);
        PCM_CH = channels.max(1);
        PCM_BITS = if bits == 16 { 16 } else { 8 };
        PCM_POS = 0;
        PCM_STEP = ((rate as u64) << 16) / SAMPLE_RATE as u64;
        PCM_STATE = PCM_STOPPED;
        PCM_LOOP = false;
    }
}

/// Enable/disable seamless looping (wrap to the start at end-of-buffer rather
/// than stopping). Used by the boot/startup music.
pub fn pcm_set_loop(on: bool) {
    unsafe { PCM_LOOP = on };
}

pub fn pcm_play() {
    unsafe {
        if !PCM_PTR.is_null() && PCM_FRAMES > 0 {
            PCM_STATE = PCM_PLAYING;
        }
    }
}
pub fn pcm_pause() {
    unsafe {
        PCM_STATE = match PCM_STATE {
            PCM_PLAYING => PCM_PAUSED,
            PCM_PAUSED => PCM_PLAYING,
            s => s,
        };
    }
}
pub fn pcm_stop() {
    unsafe {
        PCM_STATE = PCM_STOPPED;
        PCM_POS = 0;
    }
}
pub fn pcm_state() -> u32 {
    unsafe { PCM_STATE }
}
pub fn pcm_pos_ms() -> u64 {
    unsafe { (PCM_POS >> 16) * 1000 / PCM_RATE as u64 }
}
pub fn pcm_len_ms() -> u64 {
    unsafe { PCM_FRAMES * 1000 / PCM_RATE as u64 }
}

/// One source sample (mono-summed, -1.0..1.0) at the current cursor, or 0
/// if not playing / past the end. Advances the cursor by one output frame.
unsafe fn pcm_next() -> f64 {
    if PCM_STATE != PCM_PLAYING || PCM_PTR.is_null() {
        return 0.0;
    }
    let mut idx = PCM_POS >> 16;
    if idx >= PCM_FRAMES {
        if PCM_LOOP && PCM_FRAMES > 0 {
            PCM_POS = 0;
            idx = 0;
        } else {
            PCM_STATE = PCM_STOPPED;
            PCM_POS = 0;
            return 0.0;
        }
    }
    let ch = PCM_CH as u64;
    let bytes_per_sample = (PCM_BITS / 8) as u64;
    let frame_bytes = ch * bytes_per_sample;
    let base = (idx * frame_bytes) as usize;
    let mut sum = 0.0f64;
    for c in 0..ch as usize {
        let off = base + c * bytes_per_sample as usize;
        let v = if PCM_BITS == 16 {
            let lo = *PCM_PTR.add(off) as i16;
            let hi = *PCM_PTR.add(off + 1) as i16;
            let s = (lo | (hi << 8)) as i16;
            s as f64 / 32768.0
        } else {
            // u8 PCM is unsigned, centered at 128.
            (*PCM_PTR.add(off) as f64 - 128.0) / 128.0
        };
        sum += v;
    }
    PCM_POS += PCM_STEP;
    sum / ch as f64
}

/// Render `buf` (interleaved stereo i16) from all active voices, applying
/// per-stream volume, exclusive mode, and master volume.
fn render(buf: &mut [i16]) {
    let frames = buf.len() / 2;
    unsafe {
        BLOCK_START = RENDERED;
        RENDERED += frames as u64;
    }
    player_advance(frames);
    unsafe {
        let voices = &mut *&raw mut VOICES;
        let excl = EXCLUSIVE;
        let mono = MONO;
        let mut buf_peak_l = 0i16;
        let mut buf_peak_r = 0i16;
        // the LingStation sequencer renders a whole block at once; it is None (free) while nothing plays
        let daw = crate::drivers::daw::block(frames);
        let daw_gain = if excl >= 0 && excl as usize != APP_DAW { 0.0 } else { STREAM_VOL[APP_DAW] as f64 / 100.0 };
        // LingStationX (the compiled app) playing a project through lingdaw;
        // silenced while another app holds exclusive output.
        // the device's queue depth makes the playhead follow what is audible
        let lsx = crate::lingdaw::block_clocked(frames, (ring_buffers() * buffer_frames()) as usize);
        let lsx_gain = if excl >= 0 { 0.0 } else { 1.0 };
        // Programs' sound effects and music (drivers::sfx).
        let mut fx: alloc::vec::Vec<i16> = alloc::vec![0i16; frames * 2];
        if crate::drivers::sfx::active() {
            crate::drivers::sfx::mix_into(&mut fx);
        }
        for f in 0..frames {
            let mut accl = 0.0f64;
            let mut accr = 0.0f64;
            if let Some(d) = lsx {
                accl += d[f * 2] as f64 / 32768.0 * lsx_gain;
                accr += d[f * 2 + 1] as f64 / 32768.0 * lsx_gain;
            }
            accl += fx[f * 2] as f64 / 32768.0 * lsx_gain;
            accr += fx[f * 2 + 1] as f64 / 32768.0 * lsx_gain;
            for v in voices.iter_mut() {
                if !v.active {
                    continue;
                }
                if v.delay > 0 {
                    // Scheduled note not yet due -- burn its delay one
                    // sample at a time so jingle timing is sample-exact.
                    v.delay -= 1;
                    continue;
                }
                let s = if v.sine { fast_sin(v.phase) } else { triangle(v.phase) };
                v.phase += v.phase_inc;
                if v.phase >= 1.0 {
                    v.phase -= 1.0;
                }
                if v.attacking {
                    v.level += v.attack;
                    if v.level >= 1.0 {
                        v.level = 1.0;
                        v.attacking = false;
                    }
                } else if v.hold > 0 {
                    v.hold -= 1;
                } else {
                    v.level *= v.decay;
                    if v.level < 0.001 {
                        v.active = false;
                        continue;
                    }
                }
                let stream_gain = if excl >= 0 && excl as usize != v.stream {
                    0.0
                } else {
                    STREAM_VOL[v.stream] as f64 / 100.0
                };
                let contrib = s * v.level * stream_gain * 0.22; // headroom for ~4 voices
                // Pan: centre stays full-volume on both channels; extremes hard
                // pan. Mono forces centre.
                let pan = if mono { 0.0 } else { v.pan };
                let (lg, rg) = if pan <= 0.0 { (1.0, 1.0 + pan) } else { (1.0 - pan, 1.0) };
                accl += contrib * lg;
                accr += contrib * rg;
            }
            if let Some(d) = daw {
                if f * 2 + 1 < d.len() {
                    accl += d[f * 2] as f64 / 32768.0 * daw_gain;
                    accr += d[f * 2 + 1] as f64 / 32768.0 * daw_gain;
                }
            }
            // Real PCM (WAV) on the player stream, honoring exclusive mode.
            let pcm = pcm_next();
            if pcm != 0.0 {
                let pgain = if excl >= 0 && excl as usize != APP_PLAYER {
                    0.0
                } else {
                    STREAM_VOL[APP_PLAYER] as f64 / 100.0
                };
                accl += pcm * pgain * 0.9;
                accr += pcm * pgain * 0.9;
            }
            let master = MASTER_VOL as f64 / 100.0;
            let sl = (accl * master * 32767.0).clamp(-32767.0, 32767.0) as i16;
            let sr = (accr * master * 32767.0).clamp(-32767.0, 32767.0) as i16;
            buf[f * 2] = sl;
            buf[f * 2 + 1] = sr;
            if sl.abs() > buf_peak_l {
                buf_peak_l = sl.abs();
            }
            if sr.abs() > buf_peak_r {
                buf_peak_r = sr.abs();
            }
        }
        // the screen recorder's audio track: exactly what goes to the device
        #[cfg(target_arch = "x86_64")]
        crate::drivers::screenrec::audio_push(buf);
        // Update the meter: instant rise to this buffer's peak, slow fall.
        let tl = (buf_peak_l as u32 * 100) / 32767;
        let tr = (buf_peak_r as u32 * 100) / 32767;
        PEAK_L = tl.max(PEAK_L.saturating_sub(4));
        PEAK_R = tr.max(PEAK_R.saturating_sub(4));
    }
}

/// Initialize the hardware; true if an AC'97 or HD Audio device (or a USB
/// audio device, see `drivers::usbaudio`) exists.
pub fn init() -> bool {
    prefs_load();
    let saved = out_choice();
    hda::set_preferred(if saved.starts_with("hda:") { Some(saved.as_str()) } else { None });
    let r = init_backend();
    apply_out_choice_at_boot();
    r
}

fn init_backend() -> bool {
    unsafe {
        if BACKEND == BACKEND_NONE {
            if ac97::init() {
                BACKEND = BACKEND_AC97;
            } else if hda::init() {
                BACKEND = BACKEND_HDA;
            }
        }
        let usb = crate::drivers::usbaudio::present();
        let line: &[u8] = match (BACKEND, usb_active()) {
            (_, true) => b"audio: output = USB audio device
",
            (BACKEND_AC97, _) => b"audio: output = AC'97
",
            (BACKEND_HDA, _) => b"audio: output = Intel HD Audio
",
            _ => b"audio: no output device yet (a USB one can be plugged in)
",
        };
        crate::console_write(line);
        BACKEND != BACKEND_NONE || usb
    }
}

// -- Output selection (onboard codec vs. USB audio) ------------------------

/// 0 auto (USB while one is plugged in), 1 onboard codec, 2 USB.
static mut OUT_PREF: u8 = 0;

/// Is the USB device the one being played through?
pub fn usb_active() -> bool {
    crate::drivers::usbaudio::present() && (unsafe { OUT_PREF } != 1 || unsafe { BACKEND } == BACKEND_NONE)
}

/// usbaudio attached a device; true if it is now the active output.
pub fn usb_attached() -> bool {
    usb_active()
}

/// usbaudio lost its device: the onboard codec takes over at the next pump.
pub fn usb_detached() {
    let name = match unsafe { BACKEND } {
        BACKEND_AC97 => "AC'97",
        BACKEND_HDA => hda::label(),
        _ => "none",
    };
    crate::console_write(alloc::format!("audio: output falls back to {name}
").as_bytes());
}

/// Keep the DMA ring fed -- call once per WM frame / shell-loop iteration.
/// Cheap no-op when the ring is already full or no device exists. Only the
/// active output is fed; an idle HD Audio ring runs into silence and its
/// watchdog stops it, an idle USB ring drains (the device plays silence).
/// From inside a long garbage collection (lingrt mark / sweep loops): keep the speaker fed. A
/// collection over a ~1.8 GB heap pauses the app up to ~50 ms, longer than the output queue, and
/// only USB input was pumped there -- every such pause was a dropout. At most every 4 ms, never
/// nested in a pump already running.
pub fn pump_from_gc() {
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    static BUSY: AtomicBool = AtomicBool::new(false);
    static LAST_US: AtomicU64 = AtomicU64::new(0);
    let now = crate::arch::timer::now_us();
    if now.saturating_sub(LAST_US.load(Relaxed)) < 4_000 || PUMPING.load(Relaxed) || BUSY.swap(true, Relaxed) {
        return;
    }
    LAST_US.store(now, Relaxed);
    pump();
    BUSY.store(false, Relaxed);
}

/// A `pump` is running (a collection inside one must not start another).
static PUMPING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn pump() {
    PUMPING.store(true, core::sync::atomic::Ordering::Relaxed);
    pump_inner();
    PUMPING.store(false, core::sync::atomic::Ordering::Relaxed);
}

fn pump_inner() {
    let now = crate::arch::timer::now_us();
    pace_queue(now);
    pump_devices();
    stamp_audible(crate::arch::timer::now_us(), usb_active());
}

fn pump_devices() {
    cap_pump();
    // Bluetooth headphones streaming (A2DP): they are the output while connected
    if crate::drivers::bluetooth::audio_active() {
        crate::drivers::bluetooth::audio_pump(&mut render);
        return;
    }
    let usb = usb_active();
    crate::drivers::usbaudio::pump(&mut render, usb);
    // LingStationX's take in progress: drain what arrived (HD Audio above, USB in its pump)
    crate::lingdaw::rec_service();
    if usb {
        return;
    }
    match unsafe { BACKEND } {
        BACKEND_AC97 => {
            ac97::pump(&mut render);
        }
        BACKEND_HDA => {
            hda::pump(&mut render);
        }
        _ => {}
    }
}

/// `audio` (Terminal / SSH): list the outputs, choose one, test it.
pub fn command(arg: &str) -> alloc::string::String {
    use alloc::format;
    use alloc::string::String;
    let words: alloc::vec::Vec<&str> = arg.split_whitespace().collect();
    let mut msg = String::new();
    match words.as_slice() {
        [] | ["list"] | ["status"] => {},
        ["in", rest @ ..] => return command_in(rest),
        ["latency"] => return latency_report(),
        ["clock"] => return clock_test(),
        ["out", which] => {
            let pref = match *which {
                "0" | "onboard" | "hda" | "ac97" => 1,
                "1" | "usb" => 2,
                "auto" => 0,
                _ => return String::from("usage: audio out <0|1|auto>
"),
            };
            if pref == 1 && unsafe { BACKEND } == BACKEND_NONE {
                return String::from("audio: there is no onboard codec
");
            }
            if pref == 2 && !crate::drivers::usbaudio::present() {
                return String::from("audio: no USB audio device is attached
");
            }
            unsafe {
                OUT_PREF = pref;
                *(&raw mut OUT_CHOICE) = String::from(["auto", "onboard", "usb"][pref as usize]);
            }
            prefs_save();
            msg = format!("audio: output -> {}
", device_name());
            crate::console_write(msg.as_bytes());
        },
        ["tone", rest @ ..] => {
            let hz = rest.first().and_then(|s| s.parse().ok()).unwrap_or(1000u32);
            let ms = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(2000u32);
            tone(hz, ms);
            return format!("audio: {hz} Hz tone for {ms} ms on {}
", device_name());
        },
        ["pins"] => return hda::status(),
        ["use", which] => {
            let n = match *which {
                "auto" => None,
                w => match w.parse::<usize>() {
                    Ok(v) => Some(v),
                    Err(_) => return String::from("usage: audio use <n|auto>   (numbers from `audio pins`)\n"),
                },
            };
            if unsafe { BACKEND } == BACKEND_AC97 {
                return String::from("audio: the onboard device is AC'97 (no HD Audio pins)\n");
            }
            let r = hda::use_output(n);
            if r.is_ok() && unsafe { BACKEND } == BACKEND_NONE {
                // HD Audio came up only now (nothing usable at boot).
                unsafe { BACKEND = BACKEND_HDA };
            }
            msg = match r {
                Ok(name) => {
                    let key = match (n, hda::active_output()) {
                        (Some(_), Some(i)) => hda::output_key(i),
                        _ => String::from("auto"),
                    };
                    unsafe {
                        // a chosen pin means "onboard"
                        OUT_PREF = if n.is_some() { 1 } else { 0 };
                        *(&raw mut OUT_CHOICE) = key;
                    }
                    prefs_save();
                    format!("audio: output -> {name}\n")
                },
                Err(e) => format!("audio: {e}\n"),
            };
            crate::console_write(msg.as_bytes());
        },
        ["reset"] => {
            crate::drivers::usbaudio::reset_stats();
            return String::from("audio: pump statistics reset\n");
        },
        ["jingle"] => {
            jingle(EVENT_BOOT);
            return format!("audio: boot jingle on {}
", device_name());
        },
        ["latency", ms] => {
            let Ok(v) = ms.parse::<u32>() else { return String::from("usage: audio latency <12..100 ms>
") };
            crate::drivers::usbaudio::set_target_ms(v);
            return format!("audio: USB queue target {} ms
", crate::drivers::usbaudio::target_ms());
        },
        _ => {
            return String::from(
                "usage: audio                 list outputs (* = playing)
       audio out <0|1|auto>  choose the output
       audio pins            HD Audio jacks / HDMI pins (presence)
       audio use <n|auto>    play through HD Audio pin n
       audio in              inputs (mic / line in); audio in use <n|auto|none>
       audio in start|stop   capture, to watch level and position
       audio in gain <0..100>
       audio tone [hz] [ms]  test tone
       audio jingle          boot jingle
       audio latency <ms>    USB queue length
",
            )
        },
    }
    let usb = crate::drivers::usbaudio::present();
    let active_usb = usb_active();
    let onboard = match unsafe { BACKEND } {
        BACKEND_AC97 => Some("AC'97"),
        BACKEND_HDA => Some(hda::label()),
        _ => None,
    };
    let mut t = msg;
    t.push_str("audio outputs:
");
    match onboard {
        Some(n) => t.push_str(&format!("{} 0  {n}, 48000 Hz 16-bit stereo
", if active_usb { ' ' } else { '*' })),
        None => t.push_str("  0  (no onboard codec)
"),
    }
    if usb {
        t.push_str(&format!("{} 1  {} (USB)
", if active_usb { '*' } else { ' ' }, crate::drivers::usbaudio::name()));
    } else {
        t.push_str("  1  (no USB audio device)
");
    }
    if unsafe { BACKEND } == BACKEND_HDA {
        t.push_str(&hda::status());
    }
    t.push_str(&crate::drivers::usbaudio::status());
    t.push_str(&format!("saved choice (Settings > Sound): out {}, in {}\n", out_choice(), in_choice()));
    t.push_str(&format!(
        "selection: {}; master volume {}%
",
        match unsafe { OUT_PREF } {
            1 => "onboard (audio out 0)",
            2 => "USB (audio out 1)",
            _ => "auto -- USB while one is plugged in",
        },
        master_volume()
    ));
    t
}

// -- Device choice (Settings > Sound), saved in lingfs `audiodev` -----------
//
// One small system-wide file (not per user: the choice holds from boot on,
// before anyone signs in), lines of `key value`:
//   out auto | onboard | usb | hda:<pci b:d.f>:<codec>:<pin nid>
//   in auto | none | hda:<pci b:d.f>:<codec>:<pin nid>
//   in_gain <0..100>      (50 = 0 dB)
//   volume <0..100>       (master)
// A saved HD Audio pin is played again at boot when it is there (not a jack
// reporting "unplugged"); otherwise playback falls back to automatic, and
// the saved choice is kept for the next boot.

const PREFS_FILE: &str = "audiodev";
static mut OUT_CHOICE: String = String::new();
static mut IN_CHOICE: String = String::new();
/// A changed slider is written out once it has been still for a second
/// (lingfs writes are slow; a drag must not write every frame).
static mut PREFS_DIRTY_MS: u64 = 0;

fn out_choice() -> String {
    let c = unsafe { (*(&raw const OUT_CHOICE)).clone() };
    if c.is_empty() { String::from("auto") } else { c }
}

fn in_choice() -> String {
    let c = unsafe { (*(&raw const IN_CHOICE)).clone() };
    if c.is_empty() { String::from("auto") } else { c }
}

fn prefs_load() {
    if !crate::fs::lingfs::is_mounted() {
        return;
    }
    let mut buf = [0u8; 4096];
    let Ok(Some(n)) = crate::fs::lingfs::read_file(PREFS_FILE, &mut buf) else { return };
    let text = core::str::from_utf8(&buf[..n.min(buf.len())]).unwrap_or("");
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(k), Some(v)) = (it.next(), it.next()) else { continue };
        match k {
            "out" => unsafe { *(&raw mut OUT_CHOICE) = String::from(v) },
            "in" => unsafe { *(&raw mut IN_CHOICE) = String::from(v) },
            "in_gain" => {
                if let Ok(g) = v.parse::<u32>() {
                    hda::capture_set_gain(g);
                }
            },
            "volume" => {
                if let Ok(g) = v.parse::<u32>() {
                    set_master_volume(g);
                }
            },
            _ => {},
        }
    }
}

fn prefs_save() {
    unsafe { PREFS_DIRTY_MS = 0 };
    if !crate::fs::lingfs::is_mounted() {
        return;
    }
    let text = alloc::format!(
        "out {}\nin {}\nin_gain {}\nvolume {}\n",
        out_choice(),
        in_choice(),
        hda::capture_gain(),
        master_volume()
    );
    let _ = crate::fs::lingfs::write_file_any(PREFS_FILE, text.as_bytes());
}

/// Save soon (sliders).
fn prefs_touch() {
    unsafe { PREFS_DIRTY_MS = crate::arch::timer::now_ms().max(1) };
}

/// After init: what the saved output choice means now that the devices are
/// known (a saved HD Audio pin that came up = only that pin, "onboard").
fn apply_out_choice_at_boot() {
    let c = out_choice();
    unsafe {
        OUT_PREF = match c.as_str() {
            "usb" => 2,
            "onboard" => 1,
            k if k.starts_with("hda:") && hda::manual() && hda::active_output().map(hda::output_key).as_deref() == Some(k) => 1,
            _ => 0,
        };
    }
}

/// Switch the output: "auto", "onboard", "usb", or an HD Audio pin key.
/// Takes effect at once (re-routes the codec) and is saved; a short blip
/// confirms it on the new device.
pub fn set_output(key: &str) -> Result<String, String> {
    use alloc::format;
    let backend = unsafe { BACKEND };
    match key {
        "auto" => {
            unsafe { OUT_PREF = 0 };
            if backend == BACKEND_HDA {
                let _ = hda::use_output(None);
            }
        },
        "onboard" => {
            if backend == BACKEND_NONE {
                return Err(String::from("there is no onboard codec"));
            }
            unsafe { OUT_PREF = 1 };
        },
        "usb" => {
            if !crate::drivers::usbaudio::present() {
                return Err(String::from("no USB audio device is plugged in"));
            }
            unsafe { OUT_PREF = 2 };
        },
        k => {
            if backend == BACKEND_AC97 {
                return Err(String::from("the onboard device is AC'97 (no HD Audio pins)"));
            }
            let Some(i) = hda::find_output(k) else {
                return Err(format!("no output {k}"));
            };
            hda::use_output(Some(i)).map_err(String::from)?;
            unsafe {
                if BACKEND == BACKEND_NONE {
                    BACKEND = BACKEND_HDA;
                }
                OUT_PREF = 1;
            }
        },
    }
    unsafe { *(&raw mut OUT_CHOICE) = String::from(key) };
    prefs_save();
    test_tone(0);
    let msg = format!("output -> {}", device_label());
    crate::console_write(format!("audio: {msg}\n").as_bytes());
    Ok(msg)
}

/// The output playing now, by its Settings name.
pub fn device_label() -> String {
    if usb_active() {
        return alloc::format!("USB audio: {}", crate::drivers::usbaudio::name());
    }
    match unsafe { BACKEND } {
        BACKEND_HDA => match hda::active_output() {
            Some(i) => hda::output_name(i),
            None => String::from(hda::label()),
        },
        BACKEND_AC97 => String::from("AC'97"),
        _ => String::from("none"),
    }
}

/// Settings > Sound > Output rows: `key \t name \t state \t selected \t playing`.
pub fn output_rows() -> Vec<String> {
    use alloc::format;
    if unsafe { BACKEND } == BACKEND_HDA {
        hda::refresh_sense();
    }
    let choice = out_choice();
    let usb = usb_active();
    let mut rows = Vec::new();
    let auto_now = if choice == "auto" { device_label() } else { String::new() };
    rows.push(format!("auto\tAutomatic\t{auto_now}\t{}\t0", (choice == "auto") as u8));
    match unsafe { BACKEND } {
        BACKEND_HDA => {
            let active = hda::active_output();
            for i in 0..hda::output_count() {
                if !hda::output_listed(i) {
                    continue;
                }
                let key = hda::output_key(i);
                // a GPU lists one HDMI/DP pin per connector: only those with
                // a display (or that cannot tell) are worth a row
                if hda::output_is_hdmi(i) && hda::output_state(i) == "unplugged" && choice != key && active != Some(i) {
                    continue;
                }
                rows.push(format!(
                    "{key}\t{}\t{}\t{}\t{}",
                    hda::output_name(i),
                    hda::output_state(i),
                    (choice == key) as u8,
                    (!usb && active == Some(i)) as u8
                ));
            }
        },
        BACKEND_AC97 => rows.push(format!("onboard\tAC'97 (onboard)\t\t{}\t{}", (choice == "onboard") as u8, (!usb) as u8)),
        _ => {},
    }
    if crate::drivers::usbaudio::present() {
        rows.push(format!(
            "usb\tUSB audio: {}\tplugged\t{}\t{}",
            crate::drivers::usbaudio::name(),
            (choice == "usb") as u8,
            usb as u8
        ));
    }
    rows
}

// -- Capture (audio input) -------------------------------------------------
//
// Two sources: an HD Audio jack (`hda::capture_*`, polled from here) and a
// USB audio interface's recording stream (`usbaudio::capture_*`: its pump
// decodes the packets and calls `cap_push_f32`). "auto" prefers the USB
// interface -- someone who plugged in a Studio 26c wants to record from it.
//
// What was recorded goes into a 2 s stereo f32 ring (48 kHz) and a peak
// meter. Apps read it with `audio_in_*` (mono: L+R averaged) and
// LingStationX's recorder with `audio_in_read_stereo`; Settings' meter
// starts capture through `os_audio_in_meter`. Capture stops on
// `audio_in_stop` or after CAP_IDLE_MS with nobody reading or metering (an
// app that is suspended or forgot to stop), so the mic is never left open
// behind the user's back; an app that started it gets it back on its next
// read.

const CAP_LEN: usize = 2 * SAMPLE_RATE as usize;
const CAP_IDLE_MS: u64 = 5_000;
/// Stereo frames, interleaved (2 * CAP_LEN floats).
static mut CAP_RING: Vec<f32> = Vec::new();
/// Frames ever written / read (ring positions are these mod CAP_LEN).
static mut CAP_W: u64 = 0;
static mut CAP_R: u64 = 0;
static mut CAP_LEVEL: f32 = 0.0;
static mut CAP_APP: bool = false;
static mut CAP_TOUCH_MS: u64 = 0;
/// When a capture start last failed: the meter asks every frame, a broken
/// input must not re-probe the codec that often.
static mut CAP_FAIL_MS: u64 = 0;
static mut CAP_ERR: String = String::new();
/// An app's own input choice for its capture (LingStationX's input menu),
/// over the saved Settings choice; cleared by `audio_in_stop`.
static mut CAP_OVERRIDE: Option<String> = None;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Src {
    Hda(usize),
    Usb,
}

/// log2 without libm: exponent + linear mantissa (error < 0.09).
fn log2_approx(x: f32) -> f32 {
    let b = x.to_bits();
    let e = ((b >> 23) & 0xFF) as i32 - 127;
    let m = f32::from_bits((b & 0x7F_FFFF) | 0x3F80_0000);
    e as f32 + (m - 1.0)
}

/// HD Audio: 16-bit stereo, the codec's software gain applied.
fn cap_push(stereo: &[i16]) {
    let gain = hda::capture_soft_gain() / 32768.0;
    crate::drivers::screenrec::mic_push(stereo, gain);
    let f: Vec<f32> = stereo.iter().map(|&x| x as f32 * gain).collect();
    cap_store(&f);
}

/// USB audio: stereo f32 (-1..1) straight from the device's 24-bit samples.
pub fn cap_push_f32(stereo: &[f32]) {
    if crate::drivers::screenrec::active() {
        let i: Vec<i16> = stereo.iter().map(|&x| (x.clamp(-1.0, 1.0) * 32767.0) as i16).collect();
        crate::drivers::screenrec::mic_push(&i, 1.0 / 32768.0);
    }
    cap_store(stereo);
}

fn cap_store(stereo: &[f32]) {
    unsafe {
        let ring = &mut *&raw mut CAP_RING;
        if ring.len() != CAP_LEN * 2 {
            ring.clear();
            ring.resize(CAP_LEN * 2, 0.0);
        }
        let mut peak = 0.0f32;
        for f in stereo.chunks_exact(2) {
            let (l, r) = (f[0].clamp(-1.0, 1.0), f[1].clamp(-1.0, 1.0));
            let a = l.abs().max(r.abs());
            if a > peak {
                peak = a;
            }
            let at = (CAP_W % CAP_LEN as u64) as usize * 2;
            ring[at] = l;
            ring[at + 1] = r;
            CAP_W += 1;
        }
        if CAP_W - CAP_R > CAP_LEN as u64 {
            CAP_R = CAP_W - CAP_LEN as u64; // nobody read: drop the oldest
        }
        // meter on a dB scale: -60 dBFS = 0, 0 dBFS = 100; fast up, slow down
        let db = if peak > 0.000_001 { 6.0206 * log2_approx(peak) } else { -120.0 };
        let lvl = ((db + 60.0) * (100.0 / 60.0)).clamp(0.0, 100.0);
        CAP_LEVEL = if lvl > CAP_LEVEL { lvl } else { (CAP_LEVEL - 1.5).max(lvl) };
    }
}

/// The source capturing now, if any.
fn running() -> Option<Src> {
    if crate::drivers::usbaudio::capture_running() {
        return Some(Src::Usb);
    }
    if hda::capture_running() {
        return hda::capture_current().map(Src::Hda);
    }
    None
}

fn src_stop() {
    hda::capture_stop();
    crate::drivers::usbaudio::capture_stop();
}

fn src_name(s: Src) -> String {
    match s {
        Src::Hda(i) => hda::input_name(i),
        Src::Usb => crate::drivers::usbaudio::capture_name(),
    }
}

fn cap_poll() {
    if hda::capture_running() {
        hda::capture_poll(&mut |s: &[i16]| cap_push(s));
    }
    // USB arrives through its pump (`cap_push_f32`)
}

fn usb_in() -> Option<Src> {
    crate::drivers::usbaudio::has_capture().then_some(Src::Usb)
}

fn hda_best() -> Option<Src> {
    if unsafe { BACKEND } != BACKEND_HDA {
        return None;
    }
    hda::input_best().map(Src::Hda)
}

/// The input a choice ("auto", "usb", "none", a jack key) means now.
fn src_for(choice: &str) -> Option<Src> {
    match choice {
        "none" => None,
        "auto" | "" => usb_in().or_else(hda_best),
        // a USB interface that is not plugged in falls back to automatic
        "usb" => usb_in().or_else(hda_best),
        k => {
            let jack = if unsafe { BACKEND } == BACKEND_HDA { hda::find_input(k).map(Src::Hda) } else { None };
            jack.or_else(|| usb_in().or_else(hda_best))
        },
    }
}

/// The input the current choice means now (None = no input / "none").
fn input_src() -> Option<Src> {
    match unsafe { (*(&raw const CAP_OVERRIDE)).clone() } {
        Some(k) => src_for(&k),
        None => src_for(&in_choice()),
    }
}

/// Start capturing from the chosen input (if not already). false = no input.
fn cap_ensure() -> bool {
    unsafe { CAP_TOUCH_MS = crate::arch::timer::now_ms() };
    let Some(want) = input_src() else {
        if running().is_some() {
            src_stop();
        }
        unsafe { *(&raw mut CAP_ERR) = String::from("no input") };
        return false;
    };
    if running() == Some(want) {
        return true;
    }
    let now = crate::arch::timer::now_ms();
    if unsafe { CAP_FAIL_MS } != 0 && now.saturating_sub(unsafe { CAP_FAIL_MS }) < 3_000 {
        return false;
    }
    if running().is_some() {
        src_stop();
    }
    unsafe {
        CAP_R = CAP_W;
        CAP_LEVEL = 0.0;
    }
    let r = match want {
        Src::Hda(i) => hda::capture_start(i).map_err(String::from),
        Src::Usb => crate::drivers::usbaudio::capture_start(),
    };
    let ok = r.is_ok();
    if let Err(e) = r {
        crate::console_write(alloc::format!("audio: capture from {} failed: {e}\n", src_name(want)).as_bytes());
        unsafe { *(&raw mut CAP_ERR) = e };
    }
    unsafe { CAP_FAIL_MS = if ok { 0 } else { now.max(1) } };
    ok
}

fn cap_touch() {
    unsafe { CAP_TOUCH_MS = crate::arch::timer::now_ms() };
    if unsafe { CAP_APP } && running().is_none() {
        cap_ensure();
    }
    cap_poll();
}

/// Called from `pump`: move recorded frames into the ring, stop an idle
/// capture, write out a changed setting.
fn cap_pump() {
    let now = crate::arch::timer::now_ms();
    if running().is_some() {
        if now.saturating_sub(unsafe { CAP_TOUCH_MS }) > CAP_IDLE_MS {
            src_stop();
            unsafe { CAP_LEVEL = 0.0 };
        } else {
            cap_poll();
        }
    }
    let dirty = unsafe { PREFS_DIRTY_MS };
    if dirty != 0 && now.saturating_sub(dirty) > 1_000 {
        prefs_save();
    }
}

/// `audio_in_start()`: an app wants the microphone.
pub fn audio_in_start() -> bool {
    unsafe {
        CAP_APP = true;
        CAP_FAIL_MS = 0;
    }
    cap_ensure()
}

/// `audio_in_start` from a given input (a key from [`input_rows`], or a row's
/// name), without changing the saved choice; "" = the saved choice.
pub fn audio_in_start_from(which: &str) -> bool {
    let key = if which.is_empty() {
        None
    } else {
        input_rows()
            .iter()
            .map(|r| r.split('\t').collect::<Vec<_>>())
            .find(|c| c[0] == which || c.get(1).is_some_and(|n| *n == which))
            .map(|c| String::from(c[0]))
    };
    unsafe { *(&raw mut CAP_OVERRIDE) = key };
    audio_in_start()
}

/// Why the last capture start failed.
pub fn capture_error() -> String {
    unsafe { (*(&raw const CAP_ERR)).clone() }
}

pub fn audio_in_stop() {
    unsafe {
        CAP_APP = false;
        CAP_R = CAP_W;
        CAP_LEVEL = 0.0;
        *(&raw mut CAP_OVERRIDE) = None;
    }
    src_stop();
}

/// Up to `n` mono samples (-1..1, 48 kHz; L+R averaged), oldest first.
pub fn audio_in_read(n: usize) -> Vec<f32> {
    let st = audio_in_read_stereo(n);
    st.chunks_exact(2).map(|f| (f[0] + f[1]) * 0.5).collect()
}

/// Up to `n` stereo frames (interleaved L R, -1..1, 48 kHz), oldest first.
pub fn audio_in_read_stereo(n: usize) -> Vec<f32> {
    cap_touch();
    unsafe {
        let ring = &*&raw const CAP_RING;
        if ring.len() != CAP_LEN * 2 {
            return Vec::new();
        }
        let have = (CAP_W - CAP_R) as usize;
        let k = have.min(n).min(CAP_LEN);
        let mut out = Vec::with_capacity(k * 2);
        for j in 0..k as u64 {
            let at = ((CAP_R + j) % CAP_LEN as u64) as usize * 2;
            out.push(ring[at]);
            out.push(ring[at + 1]);
        }
        CAP_R += k as u64;
        out
    }
}

pub fn audio_in_available() -> usize {
    cap_touch();
    unsafe { (CAP_W - CAP_R) as usize }
}

/// Input level 0..100 (dB scale, -60..0 dBFS).
pub fn audio_in_level() -> u32 {
    cap_touch();
    unsafe { CAP_LEVEL as u32 }
}

/// Settings' meter: capture runs while it is shown. -1 = no input.
pub fn audio_in_meter() -> i32 {
    unsafe { CAP_TOUCH_MS = crate::arch::timer::now_ms() };
    if running().is_none() && !cap_ensure() {
        return -1;
    }
    cap_poll();
    unsafe { CAP_LEVEL as i32 }
}

/// What is being recorded from (Settings / `audio_in_device`).
pub fn input_label() -> String {
    match running().or_else(input_src) {
        Some(s) => src_name(s),
        None => String::new(),
    }
}

pub fn set_input_gain(g: u32) {
    hda::capture_set_gain(g);
    prefs_touch();
}

/// Switch the input: "auto", "none", "usb", or an HD Audio jack key; saved.
pub fn set_input(key: &str) -> Result<String, String> {
    use alloc::format;
    match key {
        "auto" | "none" => {},
        "usb" => {
            if !crate::drivers::usbaudio::has_capture() {
                return Err(String::from("no USB audio interface with an input is plugged in"));
            }
        },
        k => {
            if hda::find_input(k).is_none() {
                return Err(format!("no input {k}"));
            }
        },
    }
    unsafe {
        *(&raw mut IN_CHOICE) = String::from(key);
        CAP_FAIL_MS = 0;
    }
    prefs_save();
    if running().is_some() || unsafe { CAP_APP } {
        if key == "none" {
            src_stop();
        } else {
            cap_ensure();
        }
    }
    let l = input_label();
    Ok(if key == "none" { String::from("input -> none") } else { format!("input -> {}", if l.is_empty() { "nothing plugged in" } else { l.as_str() }) })
}

/// Settings > Sound > Input rows: `key \t name \t state \t selected \t active`.
pub fn input_rows() -> Vec<String> {
    use alloc::format;
    let choice = in_choice();
    let mut rows = Vec::new();
    let now = input_src();
    let auto_now = if choice == "auto" { now.map(src_name).unwrap_or_default() } else { String::new() };
    rows.push(format!("auto\tAutomatic\t{auto_now}\t{}\t0", (choice == "auto") as u8));
    if crate::drivers::usbaudio::has_capture() {
        rows.push(format!(
            "usb\t{}\t{}\t{}\t{}",
            crate::drivers::usbaudio::capture_name(),
            if crate::drivers::usbaudio::capture_running() { "recording" } else { "USB audio" },
            (choice == "usb") as u8,
            (now == Some(Src::Usb)) as u8
        ));
    }
    if unsafe { BACKEND } == BACKEND_HDA {
        for i in 0..hda::input_count() {
            let key = hda::input_key(i);
            rows.push(format!(
                "{key}\t{}\t{}\t{}\t{}",
                hda::input_name(i),
                hda::input_state(i),
                (choice == key) as u8,
                (now == Some(Src::Hda(i))) as u8
            ));
        }
    }
    rows.push(format!("none\tNone\t\t{}\t0", (choice == "none") as u8));
    rows
}

/// `audio in ...` (Terminal / SSH).
fn command_in(words: &[&str]) -> String {
    use alloc::format;
    let mut msg = String::new();
    match words {
        [] | ["list"] => {},
        ["use", which] => {
            let key = match *which {
                "auto" | "none" | "usb" => String::from(*which),
                w => match w.parse::<usize>() {
                    Ok(n) if n < hda::input_count() => hda::input_key(n),
                    _ => return String::from("usage: audio in use <n|usb|auto|none>   (numbers from `audio in`)\n"),
                },
            };
            msg = match set_input(&key) {
                Ok(m) => format!("audio: {m}\n"),
                Err(e) => format!("audio: {e}\n"),
            };
        },
        ["start"] => {
            let ok = audio_in_start();
            // hold it for a minute without reads, for watching `audio in`
            unsafe { CAP_TOUCH_MS = crate::arch::timer::now_ms() + 55_000 };
            msg = if ok {
                format!("audio: capturing from {} (stops after ~60 s unless read)\n", input_label())
            } else {
                format!("audio: no input to capture from ({})\n", capture_error())
            };
        },
        ["stop"] => {
            audio_in_stop();
            msg = String::from("audio: capture stopped\n");
        },
        ["gain", g] => {
            let Ok(v) = g.parse::<u32>() else { return String::from("usage: audio in gain <0..100>   (50 = 0 dB)\n") };
            set_input_gain(v);
            prefs_save();
            msg = format!("audio: input gain {}\n", hda::capture_gain());
        },
        _ => {
            return String::from(
                "usage: audio in               list inputs (* = capturing)\n       audio in use <n|usb|auto|none>\n       audio in start | stop     capture (to watch the level / position)\n       audio in gain <0..100>     (HD Audio jacks)\n",
            )
        },
    }
    let mut t = msg;
    t.push_str(&format!("input choice: {}\n", in_choice()));
    if crate::drivers::usbaudio::has_capture() {
        t.push_str(&format!(
            "  {} usb: {}{}\n",
            if crate::drivers::usbaudio::capture_running() { "*" } else { " " },
            crate::drivers::usbaudio::capture_name(),
            if input_src() == Some(Src::Usb) { "  <- the input now" } else { "" }
        ));
    }
    if unsafe { BACKEND } == BACKEND_HDA {
        hda::refresh_sense();
        t.push_str("HD Audio inputs (audio in use <n|auto|none>):\n");
        t.push_str(&hda::input_status());
    } else {
        t.push_str("  (no HD Audio controller: no jacks)\n");
    }
    if running().is_some() {
        cap_poll();
        t.push_str(&format!(
            "level {} / 100, {} frames waiting\n",
            unsafe { CAP_LEVEL as u32 },
            unsafe { CAP_W - CAP_R }
        ));
    }
    t
}

// -- Builtins for apps and the desktop -------------------------------------

/// `audio_in_*` (any app) and `os_audio_*` (Settings > Sound). Registered
/// with `lingrt::register`; None for other names.
pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    use crate::lingrt::{boolean, list_from, mk_str, num, to_f64, to_string, UNIT};
    if !name.starts_with(b"audio_in_") && !name.starts_with(b"os_audio_") {
        return None;
    }
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).unwrap_or(0.0);
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let rows = |r: Vec<String>| list_from(r.iter().map(|x| mk_str(x)).collect());
    let res = |r: Result<String, String>| mk_str(&r.unwrap_or_else(|e| e));
    Some(match name {
        b"audio_in_start" => boolean(audio_in_start()),
        b"audio_in_stop" => {
            audio_in_stop();
            UNIT
        },
        b"audio_in_read" => {
            let want = if args.is_empty() { 1024.0 } else { n(0) };
            let v = audio_in_read(want.clamp(0.0, CAP_LEN as f64) as usize);
            list_from(v.iter().map(|&x| num(x as f64)).collect())
        },
        b"audio_in_available" => num(audio_in_available() as f64),
        b"audio_in_level" => num(audio_in_level() as f64),
        b"audio_in_rate" => num(SAMPLE_RATE as f64),
        b"audio_in_device" => mk_str(&input_label()),
        b"os_audio_outputs" => rows(output_rows()),
        b"os_audio_output_set" => res(set_output(&s(0))),
        b"os_audio_inputs" => rows(input_rows()),
        b"os_audio_input_set" => res(set_input(&s(0))),
        b"os_audio_in_gain" => num(hda::capture_gain() as f64),
        b"os_audio_in_gain_set" => {
            set_input_gain(n(0).clamp(0.0, 100.0) as u32);
            UNIT
        },
        b"os_audio_in_meter" => num(audio_in_meter() as f64),
        b"os_audio_in_label" => mk_str(&input_label()),
        b"os_audio_master" => num(master_volume() as f64),
        b"os_audio_master_set" => {
            let v = n(0).clamp(0.0, 100.0) as u32;
            if v != master_volume() {
                set_master_volume(v);
                prefs_touch();
            }
            UNIT
        },
        b"os_audio_device" => mk_str(&device_label()),
        b"os_audio_test" => {
            test_tone(0);
            UNIT
        },
        b"os_audio_level" => num(output_level_l().max(output_level_r()) as f64),
        _ => return None,
    })
}
