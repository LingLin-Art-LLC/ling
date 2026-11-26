//! Live music engine: its own cpal output stream mixing decoded-track playback
//! with the polyphonic [`Synth`]. Separate from `ling-audio`'s `AudioEngine` so
//! music and sound-effects run independently.

use crate::synth::{Patch, Synth};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct TrackPlayer {
    stereo: Arc<Vec<f32>>, // interleaved L,R at src_rate
    src_rate: u32,
    pos: f64, // fractional stereo-pair index
    playing: bool,
    volume: f32,
}

struct MusicState {
    track: Option<TrackPlayer>,
    synth: Synth,
    master: f32,
    out_rate: u32,
    /// The track position (seconds) at the start of the last buffer handed to
    /// the device, when that buffer starts coming out of the speaker, and
    /// whether the track was playing: `position` runs on this, so it reports
    /// what is heard rather than the render cursor a device buffer ahead.
    heard_pos: f64,
    heard_at: Option<Instant>,
    heard_playing: bool,
}

impl MusicState {
    fn note_buffer(&mut self, latency: Duration) {
        match &self.track {
            Some(t) => {
                self.heard_pos = t.pos / t.src_rate.max(1) as f64;
                self.heard_playing = t.playing;
                self.heard_at = Some(Instant::now() + latency);
            }
            None => self.heard_at = None,
        }
    }

    #[inline]
    fn next(&mut self) -> (f32, f32) {
        let (mut l, mut r) = (0.0f32, 0.0f32);

        if let Some(tp) = &mut self.track {
            if tp.playing {
                let n = tp.stereo.len() / 2;
                if n >= 2 {
                    let ratio = tp.src_rate as f64 / self.out_rate as f64;
                    let idx = tp.pos as usize;
                    if idx + 1 < n {
                        let frac = (tp.pos - idx as f64) as f32;
                        let bl = tp.stereo[idx * 2]
                            + (tp.stereo[(idx + 1) * 2] - tp.stereo[idx * 2]) * frac;
                        let br = tp.stereo[idx * 2 + 1]
                            + (tp.stereo[(idx + 1) * 2 + 1] - tp.stereo[idx * 2 + 1]) * frac;
                        l += bl * tp.volume;
                        r += br * tp.volume;
                        tp.pos += ratio;
                    } else {
                        tp.playing = false; // reached the end
                    }
                }
            }
        }

        let s = self.synth.next_sample();
        l += s;
        r += s;

        let m = self.master;
        ((l * m).tanh(), (r * m).tanh())
    }
}

/// The live music engine. Construct once; keep alive for the program duration.
pub struct MusicEngine {
    state: Arc<Mutex<MusicState>>,
    _stream: cpal::Stream,
    pub out_rate: u32,
}

impl MusicEngine {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no output device")?;
        let supported = device.default_output_config()?;
        let channels = supported.channels() as usize;
        let out_rate = supported.sample_rate().0;
        let fmt = supported.sample_format();
        let config = supported.config();

        let state = Arc::new(Mutex::new(MusicState {
            track: None,
            synth: Synth::new(out_rate as f32),
            master: 0.8,
            out_rate,
            heard_pos: 0.0,
            heard_at: None,
            heard_playing: false,
        }));
        let stream = build_stream(&device, &config, channels, Arc::clone(&state), fmt)?;
        stream.play()?;
        Ok(Self { state, _stream: stream, out_rate })
    }

    // ── track playback ─────────────────────────────────────────────────────
    pub fn set_track(&self, stereo: Vec<f32>, src_rate: u32) {
        if let Ok(mut s) = self.state.lock() {
            s.track = Some(TrackPlayer {
                stereo: Arc::new(stereo),
                src_rate,
                pos: 0.0,
                playing: false,
                volume: 1.0,
            });
        }
    }

    pub fn play(&self) {
        if let Ok(mut s) = self.state.lock() {
            if let Some(t) = &mut s.track {
                t.playing = true;
            }
        }
    }

    pub fn pause(&self) {
        if let Ok(mut s) = self.state.lock() {
            if let Some(t) = &mut s.track {
                t.playing = false;
            }
        }
    }

    pub fn stop(&self) {
        if let Ok(mut s) = self.state.lock() {
            if let Some(t) = &mut s.track {
                t.playing = false;
                t.pos = 0.0;
            }
        }
    }

    pub fn seek(&self, sec: f32) {
        if let Ok(mut s) = self.state.lock() {
            if let Some(t) = &mut s.track {
                t.pos = (sec.max(0.0) as f64) * t.src_rate as f64;
            }
            s.heard_at = None;
        }
    }

    /// Current playback position in seconds: what is coming out of the speaker
    /// now (the last buffer's start, carried forward in real time), never past
    /// the render cursor.
    pub fn position(&self) -> f32 {
        let Ok(s) = self.state.lock() else { return 0.0 };
        let Some(t) = s.track.as_ref() else { return 0.0 };
        let raw = t.pos / t.src_rate.max(1) as f64;
        let Some(at) = s.heard_at else { return raw as f32 };
        if !s.heard_playing {
            return s.heard_pos.min(raw).max(0.0) as f32;
        }
        let now = Instant::now();
        let p = if now >= at {
            s.heard_pos + (now - at).as_secs_f64()
        } else {
            s.heard_pos - (at - now).as_secs_f64()
        };
        p.min(raw).max(0.0) as f32
    }

    pub fn set_volume(&self, v: f32) {
        if let Ok(mut s) = self.state.lock() {
            s.master = v.clamp(0.0, 2.0);
        }
    }

    pub fn set_track_volume(&self, v: f32) {
        if let Ok(mut s) = self.state.lock() {
            if let Some(t) = &mut s.track {
                t.volume = v.clamp(0.0, 2.0);
            }
        }
    }

    // ── synth ──────────────────────────────────────────────────────────────
    pub fn add_patch(&self, p: Patch) -> usize {
        self.state
            .lock()
            .map(|mut s| s.synth.add_patch(p))
            .unwrap_or(0)
    }

    pub fn note(&self, patch: usize, midi: i32, vel: f32, dur: f32) -> u32 {
        self.state
            .lock()
            .map(|mut s| s.synth.note(patch, midi, vel, dur))
            .unwrap_or(0)
    }

    pub fn note_on(&self, patch: usize, midi: i32, vel: f32) -> u32 {
        self.state
            .lock()
            .map(|mut s| s.synth.note_on(patch, midi, vel))
            .unwrap_or(0)
    }

    pub fn note_off(&self, patch: usize, midi: i32) {
        if let Ok(mut s) = self.state.lock() {
            s.synth.note_off_pitch(patch, midi);
        }
    }

    pub fn all_notes_off(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.synth.all_notes_off();
        }
    }
}

fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    state: Arc<Mutex<MusicState>>,
    fmt: cpal::SampleFormat,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    let err_fn = |e: cpal::StreamError| eprintln!("ling-music stream error: {e}");
    Ok(match fmt {
        cpal::SampleFormat::F32 => {
            let st = Arc::clone(&state);
            device.build_output_stream(
                config,
                move |d: &mut [f32], info: &cpal::OutputCallbackInfo| fill_f32(d, channels, &st, out_latency(info)),
                err_fn,
                None,
            )?
        },
        _ => {
            let st = Arc::clone(&state);
            device.build_output_stream::<i16, _, _>(
                config,
                move |d: &mut [i16], info: &cpal::OutputCallbackInfo| fill_i16(d, channels, &st, out_latency(info)),
                err_fn,
                None,
            )?
        },
    })
}

/// How long until the buffer being filled is heard (cpal's playback - callback instants).
fn out_latency(info: &cpal::OutputCallbackInfo) -> Duration {
    let ts = info.timestamp();
    ts.playback.duration_since(&ts.callback).unwrap_or(Duration::ZERO)
}

fn fill_f32(data: &mut [f32], ch: usize, state: &Arc<Mutex<MusicState>>, latency: Duration) {
    let ch = ch.max(1);
    if let Ok(mut s) = state.try_lock() {
        s.note_buffer(latency);
        for frame in data.chunks_mut(ch) {
            let (l, r) = s.next();
            frame[0] = l;
            if ch > 1 {
                frame[1] = r;
            }
            for x in frame.iter_mut().skip(2) {
                *x = 0.0;
            }
        }
    } else {
        for s in data.iter_mut() {
            *s = 0.0;
        }
    }
}

fn fill_i16(data: &mut [i16], ch: usize, state: &Arc<Mutex<MusicState>>, latency: Duration) {
    let ch = ch.max(1);
    if let Ok(mut s) = state.try_lock() {
        s.note_buffer(latency);
        for frame in data.chunks_mut(ch) {
            let (l, r) = s.next();
            frame[0] = (l * 32767.0) as i16;
            if ch > 1 {
                frame[1] = (r * 32767.0) as i16;
            }
            for x in frame.iter_mut().skip(2) {
                *x = 0;
            }
        }
    } else {
        for s in data.iter_mut() {
            *s = 0;
        }
    }
}
