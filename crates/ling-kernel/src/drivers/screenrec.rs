//! Screen recording: the presented frames as an H.264 `.mp4` (the default)
//! or a Motion-JPEG `.avi`, in `home/<user>/Videos/screenrec/screenrec-<n>.<ext>`.
//! Both play in VLC, mpv, ffmpeg, browsers and Windows' Media Player / Photos.
//!
//! Started from the PrtSc overlay ("Record video" / R) or the terminal --
//! also over SSH: `screenrec start [fps] [full|half] [h264|mjpeg]`,
//! `screenrec stop`, `screenrec codec h264|mjpeg`, `screenrec quality <qp>`,
//! `screenrec bitrate <kbit/s>`. While a recording runs, PrtSc stops it. Both
//! presents (the Ling desktop's `lingapp::present` and the native window
//! manager's `framebuffer::present`) hand their frame to [`frame`]; a frame
//! is taken only when its time slot is due.
//!
//! H.264 (`ling_h264::enc`): Constrained Baseline, CAVLC, 30 fps at the
//! screen's own resolution, keyframes every 2 s, constant quality with a
//! bitrate cap (default QP 24, 10 Mbit/s). The present only copies the frame;
//! the slices of the picture are encoded on the worker cores
//! (`smp::batch_start`) while the desktop goes on, and a frame that comes due
//! while the previous one is still encoding is dropped (the previous picture
//! simply lasts longer: every sample carries its real duration). Static
//! screen areas cost almost nothing (skipped macroblocks).
//!
//! Storage: on a data volume (the installed system's Data partition, the
//! Pi's card, else the first LINGDATA disk) the recording streams into a
//! hidden `.screenrec-<n>.part` beside the video -- a fragmented MP4 (or the
//! AVI's frames) -- and stopping writes the real file (a regular "fast start"
//! MP4: `moov` with the sample tables first, then the frames) and deletes the
//! part file. Without a data volume (lingfs only, a few MiB) the frames stay
//! in memory, capped at LINGFS_CAP, and the file is written at the end.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use ling_h264::enc::{EncConfig, Encoder, Source};
use ling_mp4::writer::{self, AudioPart, AudioTrack, SampleInfo, VideoTrack};

const MJPEG_FPS: u32 = 10;
const H264_FPS: u32 = 30;
const QUALITY: u32 = 70;
/// Frames go to disk in pieces this big.
const FLUSH_AT: usize = 1 << 20;
/// lingfs-only systems: the whole recording lives in memory first.
const LINGFS_CAP: u64 = 8 << 20;
/// FAT32's 4 GiB file limit, with room for the header and index.
const MEDIA_CAP: u64 = 3_900 << 20;
/// MP4 track timescale (ticks per second).
const TIMESCALE: u64 = 90_000;
/// An MP4 fragment is written at every keyframe, or once this much is pending.
const FRAG_MAX: usize = 4 << 20;

/// Default codec for new recordings: 0 = H.264, 1 = MJPEG.
static CODEC: AtomicU8 = AtomicU8::new(0);
/// H.264 base quantiser (quality: lower is better) and bitrate cap (kbit/s, 0 = none).
static QP: AtomicU8 = AtomicU8::new(24);
static KBPS: AtomicU32 = AtomicU32::new(10_000);
/// When the last slice of the running H.264 frame finished (us, worker cores write it).
static SLICES_END: AtomicU64 = AtomicU64::new(0);

// -- Audio ------------------------------------------------------------------------
//
// The mixer hands every block it renders for the output device to
// `audio_push` (and the input jack's captured blocks to `mic_push`). While a
// recording's audio runs, the blocks queue here, on a timeline that starts
// with the first video frame: a block rendered at time t belongs at
// t - origin, like a frame captured then (both are what the apps produced at
// that moment; the device's queue delays what is heard, not what was made).
// When the mixer falls behind (a slow device clock, an underrun) or there is
// no output device at all, silence fills the gap; when it runs ahead, the
// newest frames are dropped, so audio and video stay in step.

/// What to record: 0 nothing, 1 the system output, 2 the input jack, 3 both (mixed).
static AUDIO_MODE: AtomicU8 = AtomicU8::new(1);
/// `screenrec flash` (an A/V sync test): frames still to turn white, each with a 1 kHz beep.
static FLASH: AtomicU32 = AtomicU32::new(0);
/// Bits of the running recording's audio (as AUDIO_MODE), 0 = not capturing.
static AUDIO_LIVE: AtomicU8 = AtomicU8::new(0);
const AAC_RATE: u32 = 48_000;
const AAC_BITRATE: u32 = 160_000;
/// Audio frames (1024 samples) one encode job takes at most.
const AAC_BATCH: usize = 16;

struct AudioCap {
    /// queued stereo samples, oldest first, not yet handed to the encoder
    pcm: Vec<i16>,
    /// microphone samples (mono) waiting to be mixed in
    mic: Vec<i16>,
    /// stereo frames placed on the timeline so far (consumed + queued)
    placed: u64,
    origin_us: u64,
    /// how far a burst's last block reaches past its render time (one device buffer), frames
    latency: u64,
    /// silence inserted for underruns, frames
    filled: u64,
    /// when the last block arrived, and the last block time a correction looked at
    last_us: u64,
    checked_us: u64,
    /// frames dropped to pull the timeline back (rendering ran ahead), and still to drop
    dropped: u64,
    debt: u64,
    /// system sound: input frames consumed per output frame (a hair off 1.0 while the
    /// device clock drifts against the timer), the resampler's phase and last input frame
    ratio: f64,
    frac: f64,
    prev: (i16, i16),
    /// frames rendered but not yet heard when the recording started
    l0: i64,
}

static mut ACAP: AudioCap =
    AudioCap {
        pcm: Vec::new(),
        mic: Vec::new(),
        placed: 0,
        origin_us: 0,
        latency: 0,
        filled: 0,
        last_us: 0,
        checked_us: 0,
        dropped: 0,
        debt: 0,
        ratio: 1.0,
        frac: 0.0,
        prev: (0, 0),
        l0: 0,
    };

/// Largest timeline error left alone (frames, ~32 ms); beyond it audio_sync corrects.
const SYNC_SLACK: u64 = 768;

/// Serial log of a timeline correction (the first 20 of a recording).
fn sync_log(how: &str, frames: u64) {
    static N: AtomicU32 = AtomicU32::new(0);
    if N.fetch_add(1, Ordering::Relaxed) < 20 {
        crate::drivers::serial::write(format!("screenrec: audio {how} by {} ms, corrected\n", frames * 1000 / AAC_RATE as u64).as_bytes());
    }
}

/// A block arrives now: note the time; returns how many of its frames to skip
/// (a correction found rendering ahead of the timeline).
fn audio_place(a: &mut AudioCap, n: u64, now: u64) -> u64 {
    a.last_us = now;
    let skip = a.debt.min(n);
    a.debt -= skip;
    a.dropped += skip;
    skip
}

/// Keep the timeline on the wall clock; called between mixer pumps (from the
/// frame path). The device driver renders in bursts, one buffer per block, so
/// right after a burst its newest block ends one buffer past the burst's time:
/// that is where the timeline's end belongs. Behind by more than the slack
/// (the device clock runs slow, an underrun): silence in between; ahead: the
/// newest frames are dropped. When
/// nothing was rendered for 300 ms (no device, or its ring stopped), silence
/// runs up to now.
/// System sound: the recording follows the audible clock (mixer::audible_frame). The
/// mixer's output is one continuous stream, so the blocks are simply laid end to end; the
/// file only has to keep pace with the video's timer. How far the file runs ahead of the
/// wall clock is measured at the frame being heard now (what is queued for the device is
/// not heard yet), so the bursty, variable-depth device queue never shows up as an error.
/// Drift (the codec's crystal against the CPU timer) is taken out by resampling (at most
/// 3%, ~0.01% on real hardware); silence goes in only where the speaker really played silence
/// (an underrun, no device), and frames are dropped only past 100 ms ahead.
fn audio_sync_heard(a: &mut AudioCap, now: u64) {
    let rendered = crate::drivers::mixer::rendered_frames() as i64;
    let heard = crate::drivers::mixer::audible_frame() as i64;
    let k = (a.placed - a.debt) as i64 - (rendered - heard);
    let expected = (now.saturating_sub(a.origin_us) * AAC_RATE as u64 / 1_000_000) as i64 - a.l0;
    let err = k - expected;
    // behind for real (the speaker played silence) stays behind; a long frame that only
    // delayed the pump catches up at the next pumps -- so silence goes in only after the
    // shortfall (over 100 ms) has lasted 300 ms; smaller ones the resampler takes up (checked_us: when it began; unused by this mode otherwise)
    if err < -4800 && a.checked_us == 0 {
        a.checked_us = now.max(1);
    } else if err >= -4800 {
        a.checked_us = 0;
    }
    if err < -4800 && now.saturating_sub(a.checked_us) < 300_000 {
        a.ratio = 1.0 - 0.03;
    } else if err < -4800 {
        a.checked_us = 0;
        sync_log("behind", (-err) as u64);
        let to = a.placed + (-err) as u64;
        audio_fill(a, to);
        a.ratio = 1.0;
    } else if err > 4800 {
        sync_log("ahead", err as u64);
        let mut over = err as u64;
        let k = over.min(a.pcm.len() as u64 / 2);
        a.pcm.truncate(a.pcm.len() - k as usize * 2);
        a.placed -= k;
        a.dropped += k;
        over -= k;
        a.debt += over;
        a.ratio = 1.0;
    } else {
        // pull the error to zero over ~0.5 s. A real codec's crystal is within ~0.01% of
        // the timer, so this stays at ~1.0000; emulated devices (QEMU's HD Audio runs ~2%
        // slow) need up to a few percent, which is still far better than cutting gaps
        a.ratio = 1.0 + (err as f64 / AAC_RATE as f64 * 2.0).clamp(-0.03, 0.03);
    }
}

/// Append `n` stereo frames of `x` to the queue, resampled by `a.ratio` (linear
/// interpolation, continuous across blocks); returns the frames produced.
fn resample_into(a: &mut AudioCap, x: &[i16]) -> u64 {
    let n = x.len() / 2;
    if n == 0 {
        return 0;
    }
    let at = |i: isize, c: usize, prev: (i16, i16)| -> f64 {
        if i < 0 {
            (if c == 0 { prev.0 } else { prev.1 }) as f64
        } else {
            x[i as usize * 2 + c] as f64
        }
    };
    let mut pos = a.frac - 1.0;
    let last = (n - 1) as f64;
    let mut made = 0u64;
    while pos < last {
        let i = libm::floor(pos);
        let t = pos - i;
        let i = i as isize;
        for c in 0..2 {
            let v = at(i, c, a.prev) * (1.0 - t) + at(i + 1, c, a.prev) * t;
            a.pcm.push(v.clamp(-32767.0, 32767.0) as i16);
        }
        made += 1;
        pos += a.ratio;
    }
    a.frac = pos - last;
    a.prev = (x[(n - 1) * 2], x[(n - 1) * 2 + 1]);
    made
}

fn audio_sync(a: &mut AudioCap, now: u64) {
    if AUDIO_LIVE.load(Ordering::Relaxed) & 1 != 0 {
        audio_sync_heard(a, now);
        return;
    }
    if a.last_us > a.checked_us {
        a.checked_us = a.last_us;
        let target = audio_due(a, a.last_us);
        let eff = a.placed - a.debt;
        if eff + SYNC_SLACK < target {
            sync_log("behind", target - eff);
            audio_fill(a, target + a.debt);
        } else if eff > target + SYNC_SLACK {
            sync_log("ahead", eff - target);
            let mut over = eff - target;
            let k = over.min(a.pcm.len() as u64 / 2);
            a.pcm.truncate(a.pcm.len() - k as usize * 2);
            a.placed -= k;
            a.dropped += k;
            over -= k;
            a.debt += over;
        }
    } else if now.saturating_sub(a.last_us.max(a.origin_us)) > 300_000 {
        let to = now.saturating_sub(a.origin_us) * AAC_RATE as u64 / 1_000_000;
        audio_fill(a, to + a.debt);
    }
}

fn acap() -> &'static mut AudioCap {
    unsafe { &mut *(&raw mut ACAP) }
}

/// Timeline position (frames) that "now" corresponds to.
fn audio_due(a: &AudioCap, now: u64) -> u64 {
    now.saturating_sub(a.origin_us) * AAC_RATE as u64 / 1_000_000 + a.latency
}

/// Fill silence up to `to` frames (an underrun, or no audio device).
fn audio_fill(a: &mut AudioCap, to: u64) {
    if to > a.placed {
        let n = (to - a.placed) as usize;
        a.pcm.resize(a.pcm.len() + n * 2, 0);
        a.placed = to;
        a.filled += n as u64;
    }
}

/// The mixer rendered `stereo` (interleaved 16-bit, 48 kHz) for the output device.
pub fn audio_push(stereo: &[i16]) {
    let live = AUDIO_LIVE.load(Ordering::Acquire);
    if live == 0 {
        return;
    }
    let a = acap();
    let n = (stereo.len() / 2) as u64;
    if live & 1 == 0 {
        return; // the input jack drives the timeline (see mic_push)
    }
    let skip = audio_place(a, n, crate::arch::timer::now_us());
    let start = a.pcm.len();
    let n = resample_into(a, &stereo[skip as usize * 2..]);
    if live & 2 != 0 {
        // mix the microphone in
        let take = a.mic.len().min(n as usize);
        for i in 0..take {
            let m = a.mic[i] as i32;
            for c in 0..2 {
                let s = &mut a.pcm[start + i * 2 + c];
                *s = (*s as i32 + m).clamp(-32767, 32767) as i16;
            }
        }
        a.mic.drain(..take);
        if a.mic.len() > 9600 {
            let d = a.mic.len() - 9600;
            a.mic.drain(..d); // the input runs ahead: keep 200 ms
        }
    }
    a.placed += n;
}

/// The input jack captured `stereo` (16-bit, 48 kHz); `gain` = the input level setting.
pub fn mic_push(stereo: &[i16], gain: f32) {
    let live = AUDIO_LIVE.load(Ordering::Acquire);
    if live & 2 == 0 {
        return;
    }
    let a = acap();
    if live & 1 != 0 {
        for f in stereo.chunks_exact(2) {
            a.mic.push((((f[0] as f32 + f[1] as f32) * 0.5) * gain * 32767.0).clamp(-32767.0, 32767.0) as i16);
        }
        return;
    }
    let n = (stereo.len() / 2) as u64;
    let skip = audio_place(a, n, crate::arch::timer::now_us());
    let n = n - skip;
    for f in stereo[skip as usize * 2..].chunks_exact(2) {
        let m = (((f[0] as f32 + f[1] as f32) * 0.5) * gain * 32767.0).clamp(-32767.0, 32767.0) as i16;
        a.pcm.push(m);
        a.pcm.push(m);
    }
    a.placed += n;
}

fn audio_mode_name(m: u8) -> &'static str {
    match m {
        0 => "off",
        1 => "system",
        2 => "mic",
        _ => "both",
    }
}

enum Sink {
    Media { part: String, file: crate::fs::media::MediaFile, buf: Vec<u8>, written: u64 },
    Memory(Vec<u8>),
}

impl Sink {
    /// Bytes in the sink so far (also the offset the next write lands at).
    fn len(&self) -> u64 {
        match self {
            Sink::Media { written, .. } => *written,
            Sink::Memory(v) => v.len() as u64,
        }
    }

    fn write(&mut self, d: &[u8]) -> bool {
        match self {
            Sink::Memory(v) => {
                v.extend_from_slice(d);
                true
            },
            Sink::Media { file, buf, written, .. } => {
                buf.extend_from_slice(d);
                *written += d.len() as u64;
                if buf.len() >= FLUSH_AT {
                    let ok = crate::fs::media::file_write(file, buf);
                    buf.clear();
                    ok
                } else {
                    true
                }
            },
        }
    }
}

/// Motion-JPEG state.
struct Mj {
    /// (offset from the `movi` fourcc, size) of every chunk, in order
    index: Vec<(u32, u32)>,
    movi_len: u64,
    max_chunk: u32,
    enc: jpeg::Encoder,
    planes: jpeg::Planes,
}

/// What the worker cores need for one frame's slices (and its audio).
struct JobCtx {
    enc: *const Encoder,
    px: *const u32,
    len: usize,
    w: usize,
    h: usize,
    /// video slices; task `slices` (when `frames` > 0) encodes the audio
    slices: usize,
    audio: *mut AudioJob,
}

/// Audio frames handed to a worker with a video frame.
struct AudioJob {
    enc: ling_aac::enc::AacEncoder,
    /// up to AAC_BATCH frames of interleaved stereo PCM, -1..1
    input: Vec<f32>,
    frames: usize,
    /// their access units: AAC_BATCH slots of `slot` bytes, and the lengths
    out: Vec<u8>,
    slot: usize,
    sizes: [u32; AAC_BATCH],
}

impl AudioJob {
    fn run(&mut self) {
        for f in 0..self.frames {
            let (i, o) = (f * 2048, f * self.slot);
            let n = self.enc.encode_frame(&self.input[i..i + 2048], &mut self.out[o..o + self.slot]);
            self.sizes[f] = n as u32;
        }
    }
}

/// The MP4 side of an H.264 recording.
struct Mux {
    track: VideoTrack,
    /// (capture time in TIMESCALE ticks from the first frame, size, keyframe) of every frame
    samples: Vec<(u64, u32, bool)>,
    /// frames not written yet: samples[pend_from..], their bytes
    pend: Vec<u8>,
    pend_from: usize,
    /// the AAC track: its config, every access unit's size, the ones not written yet
    audio: Option<AudioTrack>,
    asamples: Vec<u32>,
    apend: Vec<u8>,
    apend_from: usize,
    /// written chunks in file order: (offset in the sink, bytes, samples, audio?)
    chunks: Vec<(u64, u64, u32, bool)>,
    seq: u32,
    /// the memory sink holds bare frames; the media one a fragmented MP4
    fragmented: bool,
}

struct H264 {
    enc: Box<Encoder>,
    /// the audio encoder and its job (None: no audio)
    audio: Option<Box<AudioJob>>,
    /// time zero of both tracks (us): the first captured frame's slot start, and that slot
    origin_us: u64,
    slot0: u64,
    /// the captured frame being encoded
    cap: Vec<u32>,
    cw: usize,
    ch: usize,
    job: JobCtx,
    busy: bool,
    workers: bool,
    job_tick: u64,
    job_t0: u64,
    last_t: u64,
    mux: Mux,
    dropped: u32,
    encoded: u32,
    keys: u32,
    enc_us: u64,
    enc_max: u64,
}

enum Kind {
    Mjpeg(Mj),
    H264(Box<H264>),
}

struct Rec {
    fps: u32,
    w: usize,
    h: usize,
    start_us: u64,
    next_slot: u64,
    sink: Sink,
    /// where the video goes: the real (media or lingfs) path and the one shown
    out_real: String,
    out_shown: String,
    kind: Kind,
}

static mut REC: Option<Rec> = None;
/// Held by whoever touches REC (a present encoding a frame, a terminal stop).
static BUSY: AtomicBool = AtomicBool::new(false);
/// The last recording's result line, for `screenrec` status after an auto-stop.
static mut LAST: Option<String> = None;

fn lock() -> bool {
    BUSY.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok()
}

fn unlock() {
    BUSY.store(false, Ordering::Release);
}

fn rec() -> &'static mut Option<Rec> {
    unsafe { &mut *(&raw mut REC) }
}

pub fn active() -> bool {
    unsafe { (*(&raw const REC)).is_some() }
}

fn codec_name(h264: bool) -> &'static str {
    if h264 {
        "H.264"
    } else {
        "MJPEG"
    }
}

/// "recording 12.3 s, ... -> path" / the last result.
pub fn status() -> String {
    if let Some(r) = unsafe { (*(&raw const REC)).as_ref() } {
        let secs = crate::arch::timer::now_us().saturating_sub(r.start_us) as f64 / 1e6;
        let detail = match &r.kind {
            Kind::Mjpeg(m) => format!("MJPEG, {} frames", m.index.len()),
            Kind::H264(h) => format!(
                "H.264 qp {}, {} frames ({} dropped), audio {} ({:.1} s queued + coded, {:.1} s silence filled), encode avg {:.1} ms max {:.1} ms",
                h.enc.current_qp(),
                h.encoded,
                h.dropped,
                if h.audio.is_some() { audio_mode_name(AUDIO_LIVE.load(Ordering::Relaxed)) } else { "off" },
                acap().placed as f64 / AAC_RATE as f64,
                acap().filled as f64 / AAC_RATE as f64,
                h.enc_us as f64 / 1000.0 / h.encoded.max(1) as f64,
                h.enc_max as f64 / 1000.0
            ),
        };
        return format!("recording {:.1} s, {}x{} @ {} fps, {}, {:.1} MiB -> {}", secs, r.w, r.h, r.fps, detail, r.sink.len() as f64 / 1048576.0, r.out_shown);
    }
    let codec = codec_name(CODEC.load(Ordering::Relaxed) == 0);
    let settings = format!(
        "codec {codec}, quality (qp) {}, bitrate cap {} kbit/s, audio {}",
        QP.load(Ordering::Relaxed),
        KBPS.load(Ordering::Relaxed),
        audio_mode_name(AUDIO_MODE.load(Ordering::Relaxed))
    );
    match unsafe { (*(&raw const LAST)).as_ref() } {
        Some(s) => format!("not recording (last: {s}); {settings}"),
        None => format!("not recording; {settings}"),
    }
}

const USAGE: &str = "usage: screenrec start [fps 1-60] [full|half] [h264|mjpeg] | stop | status\n       screenrec codec h264|mjpeg | quality <qp 10-51, default 24> | bitrate <kbit/s, 0 = no cap>\n       screenrec audio off|system|mic|both (default system: what you hear, as AAC in the .mp4)\n  records the screen to ~/Videos/screenrec/screenrec-<n>.mp4 (.avi for MJPEG); PrtSc > Record video does the same";

/// `screenrec [start [fps] [full|half] [h264|mjpeg] | stop | status | codec | quality | bitrate]` (terminal and SSH).
pub fn command(arg: &str) -> String {
    let mut words = arg.split_whitespace();
    let line = match words.next().unwrap_or("status") {
        "start" | "on" => {
            let mut fps = 0;
            let mut size = None;
            let mut codec = None;
            for w in words {
                match w {
                    "full" => size = Some(true),
                    "half" => size = Some(false),
                    "h264" | "h.264" | "mp4" => codec = Some(true),
                    "mjpeg" | "avi" => codec = Some(false),
                    _ => {
                        if let Ok(n) = w.parse::<u32>() {
                            fps = n;
                        }
                    },
                }
            }
            start_with(fps, size, codec).unwrap_or_else(|e| format!("screenrec: {e}"))
        },
        "stop" | "off" => match stop() {
            Ok(p) => format!("saved {p}"),
            Err(e) => format!("screenrec: {e}"),
        },
        "status" => status(),
        "codec" => match words.next() {
            Some("h264" | "h.264" | "mp4") => {
                CODEC.store(0, Ordering::Relaxed);
                String::from("screenrec: new recordings use H.264 (.mp4)")
            },
            Some("mjpeg" | "avi") => {
                CODEC.store(1, Ordering::Relaxed);
                String::from("screenrec: new recordings use Motion-JPEG (.avi)")
            },
            _ => format!("screenrec: codec {} (choose h264 or mjpeg)", codec_name(CODEC.load(Ordering::Relaxed) == 0)),
        },
        "quality" | "qp" => match words.next().and_then(|w| w.parse::<u8>().ok()) {
            Some(q) if (10..=51).contains(&q) => {
                QP.store(q, Ordering::Relaxed);
                format!("screenrec: H.264 quality (qp) {q} for new recordings (lower = better and bigger; 24 default)")
            },
            _ => format!("screenrec: quality (qp) {} -- give 10..51, lower is better", QP.load(Ordering::Relaxed)),
        },
        "bitrate" | "kbps" => match words.next().and_then(|w| w.parse::<u32>().ok()) {
            Some(k) => {
                KBPS.store(k, Ordering::Relaxed);
                if k == 0 {
                    String::from("screenrec: no bitrate cap (constant quality)")
                } else {
                    format!("screenrec: bitrate cap {k} kbit/s for new recordings")
                }
            },
            None => format!("screenrec: bitrate cap {} kbit/s (0 = none)", KBPS.load(Ordering::Relaxed)),
        },
        "flash" => {
            // A/V sync test: the next captured frame turns white as a 1 kHz beep starts
            FLASH.fetch_add(1, Ordering::Relaxed);
            String::from("screenrec: the next recorded frame is white, with a 1 kHz beep at the same moment")
        },
        "audio" | "sound" => {
            let m = match words.next() {
                Some("off" | "none") => Some(0),
                Some("system" | "on" | "out") => Some(1),
                Some("mic" | "in" | "input") => Some(2),
                Some("both" | "all") => Some(3),
                _ => None,
            };
            match m {
                Some(m) => {
                    AUDIO_MODE.store(m, Ordering::Relaxed);
                    format!("screenrec: audio {} for new H.264 recordings", audio_mode_name(m))
                },
                None => format!("screenrec: audio {} (off | system | mic | both)", audio_mode_name(AUDIO_MODE.load(Ordering::Relaxed))),
            }
        },
        _ => String::from(USAGE),
    };
    format!("{line}\n")
}

fn user() -> Option<String> {
    crate::keyring::user().or_else(|| {
        let c = crate::fs::lingfs::current_user();
        (!c.is_empty() && c != "root").then(|| String::from(c))
    })
}

/// The highest `screenrec-<n>.avi` / `.mp4` number among `names`.
fn highest(names: &[(String, bool, usize)]) -> u32 {
    let mut n = 0;
    for (name, _, _) in names {
        let base = name.rsplit('/').next().unwrap_or(name);
        if let Some(k) = base
            .strip_prefix("screenrec-")
            .and_then(|s| s.strip_suffix(".avi").or_else(|| s.strip_suffix(".mp4")))
            .and_then(|s| s.parse::<u32>().ok())
        {
            n = n.max(k);
        }
    }
    n
}

/// Start recording the screen with the default codec. `fps` 0 = default,
/// `full` keeps the screen's resolution (else the codec's default size).
pub fn start(fps: u32, full: bool) -> Result<String, String> {
    start_with(fps, full.then_some(true), None)
}

/// Start recording. `size`: Some(true) full resolution, Some(false) half,
/// None = H.264 full when worker cores can encode, else half above 1280 wide.
/// `codec`: Some(true) H.264, Some(false) MJPEG, None = the default (`screenrec codec`).
pub fn start_with(fps: u32, size: Option<bool>, codec: Option<bool>) -> Result<String, String> {
    if active() {
        return Err(format!("already {}", status()));
    }
    let user = user().ok_or_else(|| String::from("sign in first: recordings go to your Videos/screenrec folder"))?;
    let (sw, sh) = (crate::drivers::framebuffer::width() as usize, crate::drivers::framebuffer::height() as usize);
    if sw == 0 || sh == 0 {
        return Err(String::from("no screen"));
    }
    let h264 = codec.unwrap_or(CODEC.load(Ordering::Relaxed) == 0);
    let workers = crate::par::cores() > 1 && crate::arch::smp::has_workers();
    let fps = if fps == 0 {
        if h264 {
            H264_FPS
        } else {
            MJPEG_FPS
        }
    } else {
        fps.clamp(1, if h264 { 60 } else { 30 })
    };
    let full = size.unwrap_or(h264 && workers);
    let (w, h) = if full || sw <= 1280 { (sw, sh) } else { (sw / 2, sh / 2) };
    // H.264 wants even sizes
    let (w, h) = if h264 { (w & !1, h & !1) } else { (w, h) };
    let ext = if h264 { "mp4" } else { "avi" };
    let shown_dir = format!("home/{user}/Videos/screenrec");
    // a data volume when there is one: the home folder's own, else LINGDATA
    let media_dir = crate::vfs::storage_path(&shown_dir).or_else(|| {
        crate::fs::media::has("lingdata").then(|| format!("media/lingdata/home/{user}/Videos/screenrec"))
    });
    let (sink, out_real, out_shown) = match media_dir {
        Some(dir) => {
            if !crate::fs::media::mkdir_p(&dir) {
                return Err(format!("cannot create {dir}"));
            }
            let n = highest(&crate::fs::media::list(&dir)) + 1;
            let part = format!("{dir}/.screenrec-{n}.part");
            let _ = crate::fs::media::delete(&part);
            let file = crate::fs::media::file_create(&part).ok_or_else(|| format!("cannot write {part}"))?;
            let real = format!("{dir}/screenrec-{n}.{ext}");
            let shown = if dir.ends_with(&shown_dir) && !dir.starts_with("media/lingdata") { format!("{shown_dir}/screenrec-{n}.{ext}") } else { real.clone() };
            (Sink::Media { part, file, buf: Vec::with_capacity(FLUSH_AT + (256 << 10)), written: 0 }, real, shown)
        },
        None => {
            if !crate::vfs::ready() {
                return Err(String::from("no storage mounted"));
            }
            let n = highest(&crate::vfs::list(&shown_dir)) + 1;
            let p = format!("{shown_dir}/screenrec-{n}.{ext}");
            (Sink::Memory(Vec::new()), p.clone(), p)
        },
    };
    let mut sink = sink;
    let kind = if h264 {
        let mbh = h.div_ceil(16);
        // two slices per worker core balance uneven content; one without workers compresses best
        let slices = if workers { (2 * (crate::par::cores() - 1)).clamp(1, (mbh / 2).max(1)) } else { 1 };
        let enc = Box::new(Encoder::new(EncConfig {
            width: w,
            height: h,
            fps,
            qp: QP.load(Ordering::Relaxed),
            max_qp: 40,
            bitrate_kbps: KBPS.load(Ordering::Relaxed),
            gop: fps * 2,
            slices,
        }));
        let track = VideoTrack { width: w as u32, height: h as u32, timescale: TIMESCALE as u32, avcc: enc.avcc(), bt709: true };
        // audio: AAC-LC stereo 48 kHz from the mixer (and / or the input jack)
        let amode = AUDIO_MODE.load(Ordering::Relaxed);
        let audio = (amode != 0).then(|| {
            let enc = ling_aac::enc::AacEncoder::new(AAC_RATE, 2, AAC_BITRATE);
            let slot = enc.max_frame_bytes();
            Box::new(AudioJob { enc, input: alloc::vec![0.0; AAC_BATCH * 2048], frames: 0, out: alloc::vec![0; AAC_BATCH * slot], slot, sizes: [0; AAC_BATCH] })
        });
        let atrack = audio.as_ref().map(|a| AudioTrack { sample_rate: AAC_RATE, channels: 2, asc: a.enc.asc().to_vec(), bitrate: AAC_BITRATE, priming: 1024 });
        if amode & 2 != 0 {
            // keep the input jack recording (a no-op when there is none)
            crate::drivers::mixer::audio_in_start();
        }
        let fragmented = matches!(sink, Sink::Media { .. });
        if fragmented {
            // the part file is a playable fragmented MP4 from the start
            let mut head = writer::ftyp();
            head.extend_from_slice(&writer::moov_fragmented_av(&track, atrack.as_ref()));
            sink.write(&head);
        }
        Kind::H264(Box::new(H264 {
            enc,
            cap: Vec::new(),
            cw: 0,
            ch: 0,
            job: JobCtx { enc: core::ptr::null(), px: core::ptr::null(), len: 0, w: 0, h: 0, slices: 0, audio: core::ptr::null_mut() },
            audio,
            origin_us: 0,
            slot0: 0,
            busy: false,
            workers,
            job_tick: 0,
            job_t0: 0,
            last_t: 0,
            mux: Mux {
                track,
                samples: Vec::new(),
                pend: Vec::new(),
                pend_from: 0,
                audio: atrack,
                asamples: Vec::new(),
                apend: Vec::new(),
                apend_from: 0,
                chunks: Vec::new(),
                seq: 1,
                fragmented,
            },
            dropped: 0,
            encoded: 0,
            keys: 0,
            enc_us: 0,
            enc_max: 0,
        }))
    } else {
        Kind::Mjpeg(Mj { index: Vec::new(), movi_len: 0, max_chunk: 0, enc: jpeg::Encoder::new(QUALITY), planes: jpeg::Planes::new(w, h) })
    };
    while !lock() {
        crate::proc::sched::yield_now();
    }
    *rec() = Some(Rec { fps, w, h, start_us: crate::arch::timer::now_us(), next_slot: 0, sink, out_real, out_shown: out_shown.clone(), kind });
    unlock();
    let codec = codec_name(h264);
    let sound = if h264 { audio_mode_name(AUDIO_MODE.load(Ordering::Relaxed)) } else { "off" };
    crate::drivers::serial::write(format!("screenrec: start {codec} {w}x{h} @ {fps} fps, audio {sound} -> {out_shown}\n").as_bytes());
    Ok(format!("recording the screen ({codec}, {w}x{h}, {fps} fps, audio {sound}) -> {out_shown}"))
}

/// One presented frame (`px[y * stride + x]` = 0xRRGGBB, `w` x `h`). Cheap
/// when no recording runs or its next slot is not due yet.
pub fn frame(px: &[u32], stride: usize, w: usize, h: usize) {
    if !active() || w == 0 || h == 0 || !lock() {
        return;
    }
    let now = crate::arch::timer::now_us();
    let mut stop_reason: Option<&str> = None;
    if let Some(r) = rec().as_mut() {
        let slot = now.saturating_sub(r.start_us) * r.fps as u64 / 1_000_000;
        let Rec { kind, sink, next_slot, fps, start_us, .. } = r;
        match kind {
            Kind::Mjpeg(m) => {
                if slot >= *next_slot {
                    // the slots a slow frame skipped: empty chunks repeat the last picture
                    while *next_slot < slot {
                        push_chunk(sink, m, None);
                        *next_slot += 1;
                    }
                    m.planes.load(px, stride, w, h);
                    let jpg = m.enc.encode(&m.planes);
                    if !push_chunk(sink, m, Some(&jpg)) {
                        stop_reason = Some("the disk is full");
                    }
                    *next_slot = slot + 1;
                }
            },
            Kind::H264(hs) => stop_reason = h264_frame(sink, hs, next_slot, *fps, *start_us, slot, now, px, stride, w, h),
        }
        let cap = if matches!(r.sink, Sink::Memory(_)) { LINGFS_CAP } else { MEDIA_CAP };
        if stop_reason.is_none() && r.sink.len() >= cap {
            stop_reason = Some("it reached the size limit");
        }
    }
    unlock();
    if let Some(why) = stop_reason {
        let msg = match stop() {
            Ok(p) => format!("recording stopped ({why}), saved {p}"),
            Err(e) => format!("recording stopped ({why}): {e}"),
        };
        crate::drivers::serial::write(format!("screenrec: {msg}\n").as_bytes());
        crate::drivers::screenshot::notice(String::from("Screen recording stopped"), msg);
    }
}

/// A worker core encodes one slice of the captured frame (or, as the last
/// task, the audio frames that came with it).
unsafe fn slice_task(ctx: usize, i: usize) {
    let c = &*(ctx as *const JobCtx);
    if i >= c.slices {
        if !c.audio.is_null() {
            (*c.audio).run();
        }
        return;
    }
    let px = core::slice::from_raw_parts(c.px, c.len);
    let src = Source { px, stride: c.w, width: c.w, height: c.h };
    (*c.enc).encode_slice(i, &src);
    SLICES_END.fetch_max(crate::arch::timer::now_us(), Ordering::AcqRel);
}

/// The H.264 side of [`frame`]: collect a finished frame, start the next one when due.
fn h264_frame(sink: &mut Sink, hs: &mut H264, next_slot: &mut u64, fps: u32, start_us: u64, slot: u64, now: u64, px: &[u32], stride: usize, w: usize, h: usize) -> Option<&'static str> {
    if hs.busy && crate::arch::smp::batch_done() {
        hs.busy = false;
        if !h264_collect(sink, hs, SLICES_END.load(Ordering::Acquire)) {
            return Some("the disk is full");
        }
    }
    if slot < *next_slot {
        return None;
    }
    *next_slot = slot + 1;
    if hs.busy {
        // the encoder is behind: this slot repeats the previous picture
        hs.dropped += 1;
        if hs.dropped % 60 == 1 {
            crate::drivers::serial::write(format!("screenrec: encoder busy, {} frame(s) dropped so far\n", hs.dropped).as_bytes());
        }
        return None;
    }
    if stride < w || px.len() < (h - 1) * stride + w {
        return None;
    }
    // capture: one copy of the frame; the workers convert and encode it
    if hs.cw != w || hs.ch != h {
        hs.cap = alloc::vec![0; w * h];
        hs.cw = w;
        hs.ch = h;
    }
    for y in 0..h {
        hs.cap[y * w..(y + 1) * w].copy_from_slice(&px[y * stride..y * stride + w]);
    }
    if hs.origin_us != 0 && FLASH.load(Ordering::Relaxed) > 0 {
        // sync test: this picture white, and a beep rendered from now on
        FLASH.fetch_sub(1, Ordering::Relaxed);
        hs.cap.fill(0xFF_FFFF);
        crate::drivers::mixer::tone(1000, 60);
    }
    if hs.origin_us == 0 {
        // the first frame's slot is time zero of both tracks: the audio starts there
        hs.slot0 = slot;
        hs.origin_us = (start_us + slot * 1_000_000 / fps as u64).max(1);
        if hs.audio.is_some() {
            let a = acap();
            a.pcm.clear();
            a.mic.clear();
            a.placed = 0;
            a.filled = 0;
            a.last_us = 0;
            a.checked_us = 0;
            a.dropped = 0;
            a.debt = 0;
            a.origin_us = hs.origin_us;
            // sound goes where the mixer made it (the app's intent, as with the picture):
            // the stream is laid on the audible clock less the queue it started with (l0),
            // see audio_sync_heard
            a.latency = (crate::drivers::mixer::buffer_frames() as u64).min(AAC_RATE as u64 / 10);
            a.ratio = 1.0;
            a.frac = 0.0;
            a.prev = (0, 0);
            crate::drivers::mixer::hold_depth(true);
            a.l0 = crate::drivers::mixer::rendered_frames() as i64 - crate::drivers::mixer::audible_frame() as i64;
            AUDIO_LIVE.store(AUDIO_MODE.load(Ordering::Relaxed), Ordering::Release);
        }
    }
    let dt = if hs.last_t == 0 { 1_000_000 / fps as u64 } else { now.saturating_sub(hs.last_t) };
    hs.last_t = now;
    // the video sample's time: its slot on the frame grid (the frame was taken
    // within one present of the slot's start), distinct for every frame
    hs.job_tick = (slot - hs.slot0) * TIMESCALE / fps as u64;
    hs.job_t0 = now;
    hs.enc.begin_frame(dt);
    let audio = audio_take(hs, now);
    let n = hs.enc.slice_count();
    hs.job = JobCtx { enc: &*hs.enc as *const Encoder, px: hs.cap.as_ptr(), len: hs.cap.len(), w, h, slices: n, audio };
    let tasks = n + (!audio.is_null()) as usize;
    SLICES_END.store(now, Ordering::Release);
    if hs.workers && crate::arch::smp::batch_start(slice_task, &hs.job as *const JobCtx as usize, tasks) {
        hs.busy = true;
        return None;
    }
    // no worker cores (or `cores 1`): encode right here
    for i in 0..tasks {
        unsafe { slice_task(&hs.job as *const JobCtx as usize, i) };
    }
    if !h264_collect(sink, hs, crate::arch::timer::now_us()) {
        return Some("the disk is full");
    }
    None
}

/// Hand the whole audio frames captured so far (up to AAC_BATCH) to the next
/// job; null when there are none. Fills silence where the mixer fell behind.
fn audio_take(hs: &mut H264, now: u64) -> *mut AudioJob {
    let live = AUDIO_LIVE.load(Ordering::Acquire);
    if live & 2 != 0 {
        // keep the input jack's capture running (pulls its frames into mic_push)
        let _ = crate::drivers::mixer::audio_in_available();
    }
    let Some(aj) = hs.audio.as_deref_mut() else { return core::ptr::null_mut() };
    let a = acap();
    if live != 0 {
        audio_sync(a, now);
    }
    let k = (a.pcm.len() / 2048).min(AAC_BATCH);
    if k == 0 {
        return core::ptr::null_mut();
    }
    for i in 0..k * 2048 {
        aj.input[i] = a.pcm[i] as f32 / 32768.0;
    }
    a.pcm.drain(..k * 2048);
    aj.frames = k;
    aj as *mut AudioJob
}

/// Move the audio frames the last job encoded into the file's queue.
fn audio_collect(aj: &mut AudioJob, m: &mut Mux) {
    for f in 0..aj.frames {
        let n = aj.sizes[f] as usize;
        if n == 0 {
            continue;
        }
        m.asamples.push(n as u32);
        m.apend.extend_from_slice(&aj.out[f * aj.slot..f * aj.slot + n]);
    }
    aj.frames = 0;
}

/// Finish the frame the slices encoded (and its audio) and queue it for the file.
fn h264_collect(sink: &mut Sink, hs: &mut H264, end_us: u64) -> bool {
    let src = Source { px: &hs.cap, stride: hs.cw, width: hs.cw, height: hs.ch };
    let pkt = hs.enc.end_frame(&src);
    let us = end_us.saturating_sub(hs.job_t0);
    hs.enc_us += us;
    hs.enc_max = hs.enc_max.max(us);
    hs.encoded += 1;
    hs.keys += pkt.key as u32;
    let m = &mut hs.mux;
    let mut ok = true;
    if pkt.key && m.pend_from < m.samples.len() {
        ok = flush_fragment(sink, m, Some(hs.job_tick));
    }
    m.samples.push((hs.job_tick, pkt.data.len() as u32, pkt.key));
    m.pend.extend_from_slice(pkt.data);
    if let Some(aj) = hs.audio.as_deref_mut() {
        audio_collect(aj, m);
    }
    if m.pend.len() + m.apend.len() >= FRAG_MAX {
        ok &= flush_fragment(sink, m, None);
    }
    ok
}

/// Video sample infos for `samples[from..]`; `next` = the time after the last
/// (None: give it one nominal frame).
fn video_infos(samples: &[(u64, u32, bool)], from: usize, next: Option<u64>) -> Vec<SampleInfo> {
    let mut v = Vec::with_capacity(samples.len() - from);
    for i in from..samples.len() {
        let (t, size, key) = samples[i];
        let end = samples.get(i + 1).map(|x| x.0).or(next).unwrap_or(t + TIMESCALE / 30).max(t + 1);
        v.push(SampleInfo { size, duration: (end - t) as u32, key });
    }
    v
}

fn audio_infos(sizes: &[u32]) -> Vec<SampleInfo> {
    sizes.iter().map(|&size| SampleInfo { size, duration: 1024, key: true }).collect()
}

/// Write the pending video frames and audio units as one fragment.
fn flush_fragment(sink: &mut Sink, m: &mut Mux, next: Option<u64>) -> bool {
    let (vf, af) = (m.pend_from, m.apend_from);
    let (nv, na) = (m.samples.len() - vf, m.asamples.len() - af);
    if nv == 0 && na == 0 {
        return true;
    }
    let mut ok = true;
    if m.fragmented {
        let vi = video_infos(&m.samples, vf, next);
        let ai = audio_infos(&m.asamples[af..]);
        let mut runs: Vec<(u32, u64, &[SampleInfo])> = Vec::new();
        if nv > 0 {
            runs.push((1, m.samples[vf].0, &vi));
        }
        if na > 0 {
            runs.push((2, af as u64 * 1024, &ai));
        }
        ok &= sink.write(&writer::fragment_header_av(m.seq, &runs));
    }
    if nv > 0 {
        let off = sink.len();
        ok &= sink.write(&m.pend);
        m.chunks.push((off, m.pend.len() as u64, nv as u32, false));
    }
    if na > 0 {
        let off = sink.len();
        ok &= sink.write(&m.apend);
        m.chunks.push((off, m.apend.len() as u64, na as u32, true));
    }
    m.seq += 1;
    m.pend.clear();
    m.apend.clear();
    m.pend_from = m.samples.len();
    m.apend_from = m.asamples.len();
    ok
}

/// Encode what the capture queue still holds (at stop, on this core), plus
/// one silent frame that flushes the encoder's delay.
fn audio_drain(hs: &mut H264) {
    let Some(aj) = hs.audio.as_deref_mut() else { return };
    let a = acap();
    let rem = a.pcm.len() % 2048;
    if rem != 0 {
        a.pcm.resize(a.pcm.len() + 2048 - rem, 0);
    }
    a.pcm.resize(a.pcm.len() + 2048, 0);
    while !a.pcm.is_empty() {
        let k = (a.pcm.len() / 2048).min(AAC_BATCH);
        for i in 0..k * 2048 {
            aj.input[i] = a.pcm[i] as f32 / 32768.0;
        }
        a.pcm.drain(..k * 2048);
        aj.frames = k;
        aj.run();
        audio_collect(aj, &mut hs.mux);
    }
    a.pcm = Vec::new();
    a.mic = Vec::new();
}

/// Append a `00dc` chunk (None = empty: repeat the previous frame).
fn push_chunk(sink: &mut Sink, m: &mut Mj, data: Option<&[u8]>) -> bool {
    let d = data.unwrap_or(&[]);
    let off = 4 + m.movi_len as u32;
    let mut head = [0u8; 8];
    head[..4].copy_from_slice(b"00dc");
    head[4..].copy_from_slice(&(d.len() as u32).to_le_bytes());
    let pad = d.len() & 1;
    let mut ok = sink.write(&head) && sink.write(d);
    if pad == 1 {
        ok &= sink.write(&[0]);
    }
    m.index.push((off, d.len() as u32));
    m.movi_len += 8 + d.len() as u64 + pad as u64;
    m.max_chunk = m.max_chunk.max(d.len() as u32);
    ok
}

/// Stop and write the video; returns the path it was saved to.
pub fn stop() -> Result<String, String> {
    while !lock() {
        crate::proc::sched::yield_now();
    }
    let r = rec().take();
    unlock();
    let Some(mut r) = r else { return Err(String::from("not recording")) };
    // the mixer stops queueing audio for us
    AUDIO_LIVE.store(0, Ordering::Release);
    crate::drivers::mixer::hold_depth(false);
    // a frame still on the worker cores: wait for it (they must be done with its buffers)
    if let Kind::H264(hs) = &mut r.kind {
        if hs.busy {
            while !crate::arch::smp::batch_done() {
                crate::proc::sched::yield_now();
            }
            hs.busy = false;
            let ok = h264_collect(&mut r.sink, hs, SLICES_END.load(Ordering::Acquire));
            let _ = ok;
        }
        // the sound up to now (the timeline's tail is silence if the mixer was idle)
        if hs.audio.is_some() && hs.origin_us != 0 {
            let a = acap();
            let end = crate::arch::timer::now_us().saturating_sub(hs.origin_us) * AAC_RATE as u64 / 1_000_000;
            audio_fill(a, end);
        }
        audio_drain(hs);
    }
    let res = finish(r);
    let line = match &res {
        Ok(p) => format!("saved {p}"),
        Err(e) => format!("failed: {e}"),
    };
    crate::drivers::serial::write(format!("screenrec: {line}\n").as_bytes());
    unsafe { *(&raw mut LAST) = Some(line) };
    res
}

fn finish(r: Rec) -> Result<String, String> {
    let Rec { fps, w, h, sink, out_real, out_shown, kind, .. } = r;
    let (header, ranges, trailer, summary) = match kind {
        Kind::Mjpeg(m) => {
            let frames = m.index.len() as u32;
            let header = avi_header(fps, w, h, &m);
            let mut idx = Vec::with_capacity(8 + 16 * m.index.len());
            idx.extend_from_slice(b"idx1");
            idx.extend_from_slice(&(16 * frames).to_le_bytes());
            for &(off, size) in &m.index {
                idx.extend_from_slice(b"00dc");
                idx.extend_from_slice(&(if size > 0 { 0x10u32 } else { 0 }).to_le_bytes());
                idx.extend_from_slice(&off.to_le_bytes());
                idx.extend_from_slice(&size.to_le_bytes());
            }
            let summary = format!("MJPEG, {} frames, {:.1} s", frames, frames as f64 / fps as f64);
            (header, alloc::vec![(0u64, m.movi_len)], idx, summary)
        },
        Kind::H264(mut hs) => {
            let end_tick = crate::arch::timer::now_us().saturating_sub(hs.origin_us) * TIMESCALE / 1_000_000;
            let mut sink = sink;
            let _ = flush_fragment(&mut sink, &mut hs.mux, Some(end_tick));
            let m = &hs.mux;
            if m.samples.is_empty() {
                drop_sink(sink);
                return Err(String::from("no frames were recorded"));
            }
            let vinfos = video_infos(&m.samples, 0, Some(end_tick));
            let ainfos = audio_infos(&m.asamples);
            // chunk offsets within the copied data, in file order
            let (mut vrel, mut arel) = (Vec::new(), Vec::new());
            let mut at = 0u64;
            for &(_, len, n, audio) in &m.chunks {
                if audio {
                    arel.push((at, n));
                } else {
                    vrel.push((at, n));
                }
                at += len;
            }
            let apart = m.audio.as_ref().filter(|_| !ainfos.is_empty()).map(|t| AudioPart { track: t, samples: &ainfos, chunks: &arel });
            let header = writer::layout_av(&m.track, &vinfos, &vrel, apart);
            let secs = vinfos.iter().map(|s| s.duration as u64).sum::<u64>() as f64 / TIMESCALE as f64;
            let sound = match m.audio {
                Some(_) => {
                    let filled = acap().filled as f64 / AAC_RATE as f64;
                    let dropped = acap().dropped as f64 * 1000.0 / AAC_RATE as f64;
                    format!("AAC {} kbit/s ({:.1} s silence filled in, {:.0} ms dropped to keep sync)", AAC_BITRATE / 1000, filled, dropped)
                },
                None => String::from("no audio"),
            };
            let summary = format!(
                "H.264 + {sound}, {} frames ({} dropped, {} keyframes), {:.1} s, encode avg {:.1} ms",
                hs.encoded,
                hs.dropped,
                hs.keys,
                secs,
                hs.enc_us as f64 / 1000.0 / hs.encoded.max(1) as f64
            );
            let ranges: Vec<(u64, u64)> = m.chunks.iter().map(|&(o, l, _, _)| (o, l)).collect();
            return write_out(sink, &out_real, &out_shown, &header, &ranges, &[], summary);
        },
    };
    write_out(sink, &out_real, &out_shown, &header, &ranges, &trailer, summary)
}

fn drop_sink(sink: Sink) {
    if let Sink::Media { part, file, .. } = sink {
        crate::fs::media::file_abort(file);
        let _ = crate::fs::media::delete(&part);
    }
}

/// header + the sink's byte `ranges` + trailer -> the video file.
fn write_out(sink: Sink, out_real: &str, out_shown: &str, header: &[u8], ranges: &[(u64, u64)], trailer: &[u8], summary: String) -> Result<String, String> {
    let body: u64 = ranges.iter().map(|r| r.1).sum();
    let mib = (header.len() as u64 + body + trailer.len() as u64) as f64 / 1048576.0;
    match sink {
        Sink::Memory(data) => {
            let mut all = Vec::with_capacity(header.len() + body as usize + trailer.len());
            all.extend_from_slice(header);
            for &(o, l) in ranges {
                all.extend_from_slice(&data[o as usize..(o + l) as usize]);
            }
            drop(data);
            all.extend_from_slice(trailer);
            if !crate::vfs::write(out_real, &all) {
                return Err(format!("could not write {out_real}"));
            }
        },
        Sink::Media { part, mut file, buf, .. } => {
            if !buf.is_empty() && !crate::fs::media::file_write(&mut file, &buf) {
                crate::fs::media::file_abort(file);
                let _ = crate::fs::media::delete(&part);
                return Err(String::from("the disk is full"));
            }
            drop(buf);
            if !crate::fs::media::file_finish(file) {
                let _ = crate::fs::media::delete(&part);
                return Err(format!("could not finish {part}"));
            }
            let res = assemble(&part, out_real, header, ranges, trailer);
            let _ = crate::fs::media::delete(&part);
            res?;
        },
    }
    crate::vfs::note_change(out_shown);
    Ok(format!("{out_shown} ({summary}, {mib:.1} MiB)"))
}

/// header + the part file's byte `ranges` + trailer -> `out`.
fn assemble(part: &str, out: &str, header: &[u8], ranges: &[(u64, u64)], trailer: &[u8]) -> Result<(), String> {
    let src = crate::fs::media::open_range(part).ok_or_else(|| format!("cannot read {part}"))?;
    let need = ranges.iter().map(|r| r.0 + r.1).max().unwrap_or(0);
    if src.size() < need {
        return Err(format!("{part} is short ({} of {need} bytes)", src.size()));
    }
    let _ = crate::fs::media::delete(out);
    let mut dst = crate::fs::media::file_create(out).ok_or_else(|| format!("cannot write {out}"))?;
    let mut ok = crate::fs::media::file_write(&mut dst, header);
    let mut chunk = alloc::vec![0u8; FLUSH_AT];
    for &(start, len) in ranges {
        let mut at = 0u64;
        while ok && at < len {
            let n = (len - at).min(chunk.len() as u64) as usize;
            ok = src.read_at(start + at, &mut chunk[..n]) && crate::fs::media::file_write(&mut dst, &chunk[..n]);
            at += n as u64;
            crate::proc::sched::yield_now();
        }
    }
    ok = ok && (trailer.is_empty() || crate::fs::media::file_write(&mut dst, trailer));
    if !ok {
        crate::fs::media::file_abort(dst);
        return Err(format!("could not write {out} (disk full?)"));
    }
    if !crate::fs::media::file_finish(dst) {
        return Err(format!("could not finish {out}"));
    }
    Ok(())
}

/// RIFF/AVI header up to and including the `movi` list's fourcc.
fn avi_header(fps: u32, w: usize, h: usize, m: &Mj) -> Vec<u8> {
    let frames = m.index.len() as u32;
    let (w, h) = (w as u32, h as u32);
    let movi_size = 4 + m.movi_len as u32;
    let riff_size = 4 + (8 + 192) + (8 + movi_size) + (8 + 16 * frames);
    let rate_bytes = (m.movi_len.max(1) * fps as u64 / frames.max(1) as u64) as u32;
    let mut v = Vec::with_capacity(224);
    let mut put = |b: &[u8]| v.extend_from_slice(b);
    let le = |x: u32| x.to_le_bytes();
    put(b"RIFF");
    put(&le(riff_size));
    put(b"AVI ");
    put(b"LIST");
    put(&le(192));
    put(b"hdrl");
    put(b"avih");
    put(&le(56));
    put(&le(1_000_000 / fps));
    put(&le(rate_bytes));
    put(&le(0)); // padding granularity
    put(&le(0x10)); // AVIF_HASINDEX
    put(&le(frames));
    put(&le(0)); // initial frames
    put(&le(1)); // streams
    put(&le(m.max_chunk + 8));
    put(&le(w));
    put(&le(h));
    put(&[0u8; 16]);
    put(b"LIST");
    put(&le(116));
    put(b"strl");
    put(b"strh");
    put(&le(56));
    put(b"vids");
    put(b"MJPG");
    put(&le(0)); // flags
    put(&le(0)); // priority + language
    put(&le(0)); // initial frames
    put(&le(1)); // scale
    put(&le(fps)); // rate: fps / 1
    put(&le(0)); // start
    put(&le(frames)); // length
    put(&le(m.max_chunk + 8));
    put(&le(u32::MAX)); // quality: default
    put(&le(0)); // sample size: varies
    put(&0u16.to_le_bytes());
    put(&0u16.to_le_bytes());
    put(&(w as u16).to_le_bytes());
    put(&(h as u16).to_le_bytes());
    put(b"strf");
    put(&le(40));
    put(&le(40)); // BITMAPINFOHEADER
    put(&le(w));
    put(&le(h));
    put(&1u16.to_le_bytes());
    put(&24u16.to_le_bytes());
    put(b"MJPG");
    put(&le(w * h * 3));
    put(&[0u8; 16]);
    put(b"LIST");
    put(&le(movi_size));
    put(b"movi");
    v
}

/// A baseline (sequential, Huffman) JPEG encoder with 4:2:0 chroma and the
/// standard tables -- everything a Motion-JPEG frame needs.
pub mod jpeg {
    use alloc::vec::Vec;

    const ZIGZAG: [usize; 64] = [
        0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42,
        49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
    ];
    const LUM_Q: [u32; 64] = [
        16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56, 14, 17, 22, 29, 51, 87, 80,
        62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113, 92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98,
        112, 100, 103, 99,
    ];
    const CHR_Q: [u32; 64] = [
        17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99, 47, 66, 99, 99, 99, 99, 99,
        99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
        99, 99,
    ];
    const DC_LUM_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
    const DC_CHR_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
    const DC_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    const AC_LUM_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
    const AC_LUM_VALS: [u8; 162] = [
        0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32,
        0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16,
        0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45,
        0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
        0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94,
        0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6,
        0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8,
        0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
        0xf9, 0xfa,
    ];
    const AC_CHR_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
    const AC_CHR_VALS: [u8; 162] = [
        0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22, 0x32, 0x81,
        0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34,
        0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44,
        0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
        0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92,
        0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4,
        0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6,
        0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
        0xf9, 0xfa,
    ];
    /// cos((2x + 1) u pi / 16) * (u == 0 ? sqrt(1/8) : 1/2), [u][x]
    const DCT: [[f32; 8]; 8] = {
        // cos(k pi / 16) for k = 0..=8
        const C: [f32; 9] = [1.0, 0.980_785_3, 0.923_879_5, 0.831_469_6, 0.707_106_8, 0.555_570_2, 0.382_683_43, 0.195_090_32, 0.0];
        let mut t = [[0f32; 8]; 8];
        let mut u = 0;
        while u < 8 {
            let mut x = 0;
            while x < 8 {
                // reduce (2x+1)u mod 32 to cos(k pi/16) with a sign
                let k = ((2 * x + 1) * u) % 32;
                let (k, s) = if k <= 8 {
                    (k, 1.0)
                } else if k <= 16 {
                    (16 - k, -1.0)
                } else if k <= 24 {
                    (k - 16, -1.0)
                } else {
                    (32 - k, 1.0)
                };
                let norm = if u == 0 { 0.353_553_4 } else { 0.5 };
                t[u][x] = s * C[k] * norm;
                x += 1;
            }
            u += 1;
        }
        t
    };

    /// Y, Cb, Cr planes of one frame at the recording's size.
    pub struct Planes {
        pub w: usize,
        pub h: usize,
        y: Vec<u8>,
        cb: Vec<u8>,
        cr: Vec<u8>,
    }

    impl Planes {
        pub fn new(w: usize, h: usize) -> Self {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            Planes { w, h, y: alloc::vec![0; w * h], cb: alloc::vec![128; cw * ch], cr: alloc::vec![128; cw * ch] }
        }

        /// Scale `px` (sw x sh, 0xRRGGBB) to this size and convert. An exact
        /// half averages 2x2 (readable text); other sizes sample.
        pub fn load(&mut self, px: &[u32], stride: usize, sw: usize, sh: usize) {
            let (w, h) = (self.w, self.h);
            let half = sw == w * 2 && sh == h * 2;
            let at = |x: usize, y: usize| -> u32 { px.get(y * stride + x).copied().unwrap_or(0) };
            let mut rgb_row: Vec<(i32, i32, i32)> = alloc::vec![(0, 0, 0); w];
            let cw = w.div_ceil(2);
            let mut acc: Vec<(i32, i32, i32)> = alloc::vec![(0, 0, 0); cw];
            for oy in 0..h {
                for (ox, d) in rgb_row.iter_mut().enumerate() {
                    *d = if half {
                        let (x, y) = (ox * 2, oy * 2);
                        let mut s = (0, 0, 0);
                        for c in [at(x, y), at(x + 1, y), at(x, y + 1), at(x + 1, y + 1)] {
                            s.0 += (c >> 16 & 0xFF) as i32;
                            s.1 += (c >> 8 & 0xFF) as i32;
                            s.2 += (c & 0xFF) as i32;
                        }
                        (s.0 >> 2, s.1 >> 2, s.2 >> 2)
                    } else {
                        let c = at((ox * sw / w).min(sw - 1), (oy * sh / h).min(sh - 1));
                        ((c >> 16 & 0xFF) as i32, (c >> 8 & 0xFF) as i32, (c & 0xFF) as i32)
                    };
                }
                let yrow = &mut self.y[oy * w..(oy + 1) * w];
                for (ox, &(r, g, b)) in rgb_row.iter().enumerate() {
                    // BT.601 full range, fixed point (x 2^16)
                    yrow[ox] = ((19595 * r + 38470 * g + 7471 * b + 32768) >> 16).clamp(0, 255) as u8;
                    let a = &mut acc[ox / 2];
                    a.0 += r;
                    a.1 += g;
                    a.2 += b;
                }
                if oy % 2 == 1 || oy == h - 1 {
                    let rows = if oy % 2 == 1 { 2 } else { 1 };
                    let cy = oy / 2;
                    for (cx, a) in acc.iter_mut().enumerate() {
                        let n = rows * if cx * 2 + 1 < w { 2 } else { 1 };
                        let (r, g, b) = (a.0 / n, a.1 / n, a.2 / n);
                        self.cb[cy * cw + cx] = ((-11059 * r - 21709 * g + 32768 * b + (128 << 16) + 32768) >> 16).clamp(0, 255) as u8;
                        self.cr[cy * cw + cx] = ((32768 * r - 27439 * g - 5329 * b + (128 << 16) + 32768) >> 16).clamp(0, 255) as u8;
                        *a = (0, 0, 0);
                    }
                }
            }
        }
    }

    struct Huff {
        code: [u16; 256],
        len: [u8; 256],
    }

    impl Huff {
        fn new(bits: &[u8; 16], vals: &[u8]) -> Self {
            let mut h = Huff { code: [0; 256], len: [0; 256] };
            let (mut code, mut k) = (0u16, 0usize);
            for (l, &n) in bits.iter().enumerate() {
                for _ in 0..n {
                    h.code[vals[k] as usize] = code;
                    h.len[vals[k] as usize] = l as u8 + 1;
                    code += 1;
                    k += 1;
                }
                code <<= 1;
            }
            h
        }
    }

    struct Bits<'a> {
        out: &'a mut Vec<u8>,
        acc: u32,
        n: u32,
    }

    impl Bits<'_> {
        fn put(&mut self, v: u32, len: u32) {
            if len == 0 {
                return;
            }
            self.acc = (self.acc << len) | (v & ((1 << len) - 1));
            self.n += len;
            while self.n >= 8 {
                let b = (self.acc >> (self.n - 8)) as u8;
                self.out.push(b);
                if b == 0xFF {
                    self.out.push(0);
                }
                self.n -= 8;
            }
            self.acc &= (1 << self.n) - 1;
        }

        fn flush(&mut self) {
            if self.n > 0 {
                self.put(0x7F, 8 - self.n);
            }
        }
    }

    pub struct Encoder {
        /// divisors in natural order, and the tables as written (zigzag)
        lq: [f32; 64],
        cq: [f32; 64],
        lq_zz: [u8; 64],
        cq_zz: [u8; 64],
        dc_l: Huff,
        ac_l: Huff,
        dc_c: Huff,
        ac_c: Huff,
    }

    fn scale_q(base: &[u32; 64], quality: u32) -> [u32; 64] {
        let q = quality.clamp(1, 100);
        let s = if q < 50 { 5000 / q } else { 200 - 2 * q };
        let mut t = [0u32; 64];
        for i in 0..64 {
            t[i] = ((base[i] * s + 50) / 100).clamp(1, 255);
        }
        t
    }

    impl Encoder {
        pub fn new(quality: u32) -> Self {
            let (l, c) = (scale_q(&LUM_Q, quality), scale_q(&CHR_Q, quality));
            let mut e = Encoder {
                lq: [0.0; 64],
                cq: [0.0; 64],
                lq_zz: [0; 64],
                cq_zz: [0; 64],
                dc_l: Huff::new(&DC_LUM_BITS, &DC_VALS),
                ac_l: Huff::new(&AC_LUM_BITS, &AC_LUM_VALS),
                dc_c: Huff::new(&DC_CHR_BITS, &DC_VALS),
                ac_c: Huff::new(&AC_CHR_BITS, &AC_CHR_VALS),
            };
            for i in 0..64 {
                e.lq[i] = 1.0 / l[i] as f32;
                e.cq[i] = 1.0 / c[i] as f32;
                e.lq_zz[i] = l[ZIGZAG[i]] as u8;
                e.cq_zz[i] = c[ZIGZAG[i]] as u8;
            }
            e
        }

        fn headers(&self, out: &mut Vec<u8>, w: usize, h: usize) {
            out.extend_from_slice(&[0xFF, 0xD8]);
            out.extend_from_slice(&[0xFF, 0xDB, 0, 132, 0]);
            out.extend_from_slice(&self.lq_zz);
            out.push(1);
            out.extend_from_slice(&self.cq_zz);
            out.extend_from_slice(&[0xFF, 0xC0, 0, 17, 8, (h >> 8) as u8, h as u8, (w >> 8) as u8, w as u8, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
            let tables: [(u8, &[u8; 16], &[u8]); 4] =
                [(0x00, &DC_LUM_BITS, &DC_VALS), (0x10, &AC_LUM_BITS, &AC_LUM_VALS), (0x01, &DC_CHR_BITS, &DC_VALS), (0x11, &AC_CHR_BITS, &AC_CHR_VALS)];
            let len: usize = 2 + tables.iter().map(|t| 17 + t.2.len()).sum::<usize>();
            out.extend_from_slice(&[0xFF, 0xC4, (len >> 8) as u8, len as u8]);
            for (id, bits, vals) in tables {
                out.push(id);
                out.extend_from_slice(bits);
                out.extend_from_slice(vals);
            }
            out.extend_from_slice(&[0xFF, 0xDA, 0, 12, 3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);
        }

        pub fn encode(&self, p: &Planes) -> Vec<u8> {
            let (w, h) = (p.w, p.h);
            let mut out = Vec::with_capacity(w * h / 6 + 1024);
            self.headers(&mut out, w, h);
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let mut bits = Bits { out: &mut out, acc: 0, n: 0 };
            let (mut dy, mut dcb, mut dcr) = (0i32, 0i32, 0i32);
            let mut blk = [0f32; 64];
            for my in (0..h).step_by(16) {
                for mx in (0..w).step_by(16) {
                    for (bx, by) in [(0, 0), (8, 0), (0, 8), (8, 8)] {
                        fetch(&p.y, w, h, mx + bx, my + by, &mut blk);
                        dy = self.block(&mut bits, &blk, &self.lq, dy, &self.dc_l, &self.ac_l);
                    }
                    fetch(&p.cb, cw, ch, mx / 2, my / 2, &mut blk);
                    dcb = self.block(&mut bits, &blk, &self.cq, dcb, &self.dc_c, &self.ac_c);
                    fetch(&p.cr, cw, ch, mx / 2, my / 2, &mut blk);
                    dcr = self.block(&mut bits, &blk, &self.cq, dcr, &self.dc_c, &self.ac_c);
                }
            }
            bits.flush();
            out.extend_from_slice(&[0xFF, 0xD9]);
            out
        }

        /// DCT, quantize and code one block; returns its DC for the next diff.
        fn block(&self, bits: &mut Bits, blk: &[f32; 64], q: &[f32; 64], prev_dc: i32, dc: &Huff, ac: &Huff) -> i32 {
            let mut tmp = [0f32; 64];
            // rows
            for y in 0..8 {
                for u in 0..8 {
                    let mut s = 0.0;
                    for x in 0..8 {
                        s += blk[y * 8 + x] * DCT[u][x];
                    }
                    tmp[y * 8 + u] = s;
                }
            }
            let mut coef = [0i32; 64];
            // columns, quantized
            for u in 0..8 {
                for v in 0..8 {
                    let mut s = 0.0;
                    for y in 0..8 {
                        s += tmp[y * 8 + u] * DCT[v][y];
                    }
                    let f = s * q[v * 8 + u];
                    coef[v * 8 + u] = if f >= 0.0 { (f + 0.5) as i32 } else { (f - 0.5) as i32 };
                }
            }
            let d = coef[0] - prev_dc;
            let (n, b) = category(d);
            bits.put(dc.code[n as usize] as u32, dc.len[n as usize] as u32);
            bits.put(b, n);
            let mut run = 0u32;
            for k in 1..64 {
                let c = coef[ZIGZAG[k]];
                if c == 0 {
                    run += 1;
                    continue;
                }
                while run >= 16 {
                    bits.put(ac.code[0xF0] as u32, ac.len[0xF0] as u32);
                    run -= 16;
                }
                let (n, b) = category(c);
                let sym = (run << 4 | n) as usize;
                bits.put(ac.code[sym] as u32, ac.len[sym] as u32);
                bits.put(b, n);
                run = 0;
            }
            if run > 0 {
                bits.put(ac.code[0] as u32, ac.len[0] as u32);
            }
            coef[0]
        }
    }

    /// (bit count, the bits) of a coefficient, JPEG's magnitude category code.
    fn category(v: i32) -> (u32, u32) {
        let a = v.unsigned_abs();
        let n = 32 - a.leading_zeros();
        let b = if v < 0 { (v - 1) as u32 & ((1u32 << n) - 1) } else { v as u32 };
        (n, b)
    }

    /// An 8x8 block at (x0, y0), edge pixels repeated past the plane, level-shifted.
    fn fetch(plane: &[u8], w: usize, h: usize, x0: usize, y0: usize, out: &mut [f32; 64]) {
        for y in 0..8 {
            let row = (y0 + y).min(h - 1) * w;
            for x in 0..8 {
                out[y * 8 + x] = plane[row + (x0 + x).min(w - 1)] as f32 - 128.0;
            }
        }
    }
}
