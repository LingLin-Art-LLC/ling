//! Video frame timing shared by the local player (mediaplay.rs) and the YouTube player
//! (videoplay.rs): which decoded picture to show this frame, and how even the result is.
//!
//! Two fixes over "the newest frame at or before the clock, sampled while drawing":
//!  * the clock is sampled for the moment the picture will be ON SCREEN -- the next present
//!    slot of the frame pacer (lingapp::next_present_us), not the moment the app happens to
//!    draw. Draw times jitter by milliseconds; slots are evenly spaced, so 24 fps content on a
//!    60 Hz present gets a steady 3:2 pull-down instead of a pattern that wobbles whenever a
//!    draw lands near a frame boundary;
//!  * the media clock is the audible audio clock (sfx::music_pos), lightly smoothed (a phase
//!    lock on the wall clock, snapped if it is ever off by more than 60 ms): the audible clock
//!    advances in pump-sized steps and pauses when the pump is late, and picking frames on it
//!    raw doubled or skipped pictures on those steps. Sync stays on the sound.
//!
//! The stats (`media` / `video` status): intervals between picture changes in present slots
//! (p50 / p95 / max), late pictures (shown more than one slot after their time), and cadence
//! errors (an interval that is neither floor nor ceil of slots-per-frame).

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const KEEP: usize = 600;

/// `media pace old`: pick pictures the previous way (raw clock when drawing) -- A/B tests only.
static LEGACY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn legacy() -> bool {
    LEGACY.load(core::sync::atomic::Ordering::Relaxed)
}

pub fn set_legacy(on: bool) {
    LEGACY.store(on, core::sync::atomic::Ordering::Relaxed);
}

#[derive(Default)]
pub struct Pace {
    ck_t: f64,
    ck_us: u64,
    /// The present slot (us) the current picture first went out in.
    last_slot: u64,
    /// The previous shown picture's pts (for the content frame duration).
    last_pts: f64,
    /// Intervals between picture changes (us) and the content frame durations (s).
    ivals: VecDeque<u32>,
    durs: VecDeque<f32>,
    pub late: u64,
    pub cadence_err: u64,
}

fn now_us() -> u64 {
    crate::arch::timer::now_us()
}

/// The next present slot (us); now when the pacer is off.
fn slot_us() -> u64 {
    let s = crate::lingapp::next_present_us();
    let n = now_us();
    if s > n && s < n + 100_000 { s } else { n }
}

fn slot_len_us() -> u64 {
    let hz = crate::drivers::framebuffer::present_hz() as u64;
    if hz == 0 { 16_667 } else { 1_000_000 / hz }
}

impl Pace {
    pub fn reset(&mut self) {
        *self = Pace::default();
    }

    /// The media time to pick a picture for: `t` (the player's clock, audio-driven when the
    /// media has sound) smoothed, then moved on to when this frame reaches the screen.
    pub fn target(&mut self, t: f64, playing: bool) -> f64 {
        let n = now_us();
        if !playing || self.ck_us == 0 {
            self.ck_t = t;
            self.ck_us = n;
            return t;
        }
        let mut pred = self.ck_t + n.saturating_sub(self.ck_us) as f64 / 1e6;
        let err = t - pred;
        if err.abs() > 0.06 {
            pred = t; // a seek, a stall, a stream switch: follow the sound at once
        } else {
            pred += err * 0.1;
        }
        self.ck_t = pred;
        self.ck_us = n;
        pred + slot_us().saturating_sub(n) as f64 / 1e6
    }

    /// A new picture (pts) goes out this frame, for target time `tp`.
    pub fn shown(&mut self, pts: f64, tp: f64) {
        let s = slot_us();
        let slot = slot_len_us() as f64 / 1e6;
        let fdur = if self.last_pts > 0.0 && pts > self.last_pts && pts - self.last_pts < 0.5 { pts - self.last_pts } else { 0.0 };
        // late: its time passed more than a slot (and a frame) before it could be shown
        if tp - pts > slot + fdur.max(slot) + 0.002 {
            self.late += 1;
        }
        if self.last_slot > 0 && s > self.last_slot {
            let iv = s - self.last_slot;
            if fdur > 0.0 {
                // cadence: the interval must be the floor or ceil of frame time in slots
                let want = fdur / slot;
                // libm: no f64 round/floor/ceil in this no_std kernel
                let got = libm::round(iv as f64 / 1e6 / slot);
                if got < libm::floor(want) - 0.01 || got > libm::ceil(want) + 0.01 {
                    self.cadence_err += 1;
                }
                self.durs.push_back(fdur as f32);
                if self.durs.len() > KEEP {
                    self.durs.pop_front();
                }
            }
            self.ivals.push_back(iv.min(u32::MAX as u64) as u32);
            if self.ivals.len() > KEEP {
                self.ivals.pop_front();
            }
        }
        self.last_slot = s;
        self.last_pts = pts;
    }

    /// "cadence p50 41.7 p95 50.0 max 66.7 ms over 300, content 24.0 fps, late 0, cadence errors 0"
    pub fn report(&self) -> String {
        if self.ivals.is_empty() {
            return String::from("cadence: no picture changes yet");
        }
        let mut v: Vec<u32> = self.ivals.iter().copied().collect();
        v.sort_unstable();
        let q = |f: f64| v[((v.len() - 1) as f64 * f) as usize] as f64 / 1000.0;
        let fps = if self.durs.is_empty() {
            0.0
        } else {
            let mut d: Vec<f32> = self.durs.iter().copied().collect();
            d.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
            1.0 / d[d.len() / 2].max(1e-3) as f64
        };
        format!(
            "cadence p50 {:.1} p95 {:.1} max {:.1} ms over {}, content {:.1} fps on {} Hz, late {}, cadence errors {}",
            q(0.5),
            q(0.95),
            q(1.0),
            v.len(),
            fps,
            crate::drivers::framebuffer::present_hz(),
            self.late,
            self.cadence_err
        )
    }
}
