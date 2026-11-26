//! The vocal editor's signal processing and note math on LingOS: ling-daw's `vocal.rs` (the pitch tracker, note detection, the
//! level track and the PSOLA re-render of an edited vocal) and the blob math of `api/vedit_api.rs`, ported line for line to
//! `no_std` (`f64::log2` / `powf` / `exp` / `sin` -> libm, `HashMap` -> `BTreeMap`). Nothing here touches the kernel, so the
//! host parity test (`live/tests/vedit-parity`, a scratch crate that includes this file and ling-daw's `vocal.rs` side by side)
//! runs it against the desktop's code on the same audio.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

/// The voice's pitch, one entry per `hop` seconds (ling-daw `vocal::Pitch`).
pub struct Pitch {
    /// seconds per f0 entry
    pub hop: f64,
    /// Hz, 0 = not voiced; entry j describes the time j * hop
    pub f0: Vec<f32>,
}
impl Pitch {
    pub fn at(&self, t: f64) -> f32 {
        let j = libm::round(t / self.hop);
        if j < 0.0 {
            return 0.0;
        }
        self.f0.get(j as usize).copied().unwrap_or(0.0)
    }
}

pub fn cents(a: f64, b: f64) -> f64 {
    1200.0 * libm::log2(a / b)
}

pub fn midi_hz(k: f64) -> f64 {
    440.0 * libm::pow(2.0, (k - 69.0) / 12.0)
}

/// Hz -> fractional MIDI
pub fn hz_midi(f: f64) -> f64 {
    69.0 + 12.0 * libm::log2(f / 440.0)
}

pub fn mono_of(stereo: &[f32]) -> Vec<f32> {
    stereo.chunks_exact(2).map(|f| (f[0] + f[1]) * 0.5).collect()
}

// ── pitch tracking (YIN on a 12 kHz copy, every 5 ms) ───────────────────────
pub fn track_pitch(mono: &[f32], sr: f32) -> Pitch {
    let dec = (libm::roundf(sr / 12000.0) as usize).max(1);
    let sd = sr as f64 / dec as f64;
    let d: Vec<f32> = mono.chunks(dec).map(|c| c.iter().sum::<f32>() / c.len() as f32).collect();
    let hop_s = 0.005;
    let hop = ((hop_s * sd) as usize).max(1);
    let w = (0.036 * sd) as usize;
    let tau_min = (sd / 800.0).max(2.0) as usize;
    let tau_max = ((sd / 70.0) as usize).min(w);
    let nfr = d.len() / hop + 1;
    let global_rms = libm::sqrtf(d.iter().map(|x| x * x).sum::<f32>() / d.len().max(1) as f32);
    let mut f0 = vec![0.0f32; nfr];
    let mut diff = vec![0.0f32; tau_max + 2];
    let mut cm = vec![0.0f32; tau_max + 2];
    for j in 0..nfr {
        let centre = j * hop;
        let start = centre as isize - (w / 2) as isize;
        let at = |i: usize| -> f32 {
            let k = start + i as isize;
            if k < 0 || k as usize >= d.len() {
                0.0
            } else {
                d[k as usize]
            }
        };
        let rms = libm::sqrtf((0..w).step_by(2).map(|i| at(i) * at(i)).sum::<f32>() / (w / 2).max(1) as f32);
        if rms < global_rms * 0.06 || rms < 1e-4 {
            continue;
        }
        for tau in 1..=tau_max {
            let mut s = 0.0f32;
            for i in 0..w {
                let a = at(i) - at(i + tau);
                s += a * a;
            }
            diff[tau] = s;
        }
        let mut run = 0.0f32;
        cm[0] = 1.0;
        for tau in 1..=tau_max {
            run += diff[tau];
            cm[tau] = if run > 0.0 { diff[tau] * tau as f32 / run } else { 1.0 };
        }
        let mut pick = 0usize;
        let mut tau = tau_min;
        while tau < tau_max {
            if cm[tau] < 0.16 {
                while tau + 1 < tau_max && cm[tau + 1] < cm[tau] {
                    tau += 1;
                }
                pick = tau;
                break;
            }
            tau += 1;
        }
        if pick == 0 {
            // no clear dip: take the deepest one if it is reasonably periodic
            let (bt, bv) = (tau_min..tau_max).map(|t| (t, cm[t])).fold((0, 1.0), |a, b| if b.1 < a.1 { b } else { a });
            if bv < 0.28 {
                pick = bt;
            }
        }
        if pick > 0 && pick + 1 <= tau_max {
            let (a, b, c) = (cm[pick - 1], cm[pick], cm[pick + 1]);
            let den = a - 2.0 * b + c;
            let shift = if den.abs() > 1e-9 { 0.5 * (a - c) / den } else { 0.0 };
            f0[j] = (sd / (pick as f64 + shift as f64)) as f32;
        }
    }
    // a 5-point median over voiced runs removes octave glitches
    let src = f0.clone();
    for j in 2..nfr.saturating_sub(2) {
        if src[j] > 0.0 {
            let mut v: Vec<f32> = (j - 2..=j + 2).map(|k| src[k]).filter(|x| *x > 0.0).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            f0[j] = v[v.len() / 2];
        }
    }
    // a lone voiced frame is noise
    let src = f0.clone();
    for j in 1..nfr.saturating_sub(1) {
        if src[j] > 0.0 && src[j - 1] == 0.0 && src[j + 1] == 0.0 {
            f0[j] = 0.0;
        }
    }
    Pitch { hop: hop as f64 / sd, f0 }
}

/// loudness (RMS) of `mono` per `hop` seconds
pub fn level_track(mono: &[f32], sr: f32, hop: f64) -> Vec<f32> {
    let step = ((hop * sr as f64) as usize).max(1);
    let win = step * 2;
    (0..mono.len() / step + 1)
        .map(|i| {
            let a = (i * step).saturating_sub(step / 2).min(mono.len());
            let b = (a + win).min(mono.len());
            if b <= a { 0.0 } else { libm::sqrtf((a..b).map(|k| mono[k] * mono[k]).sum::<f32>() / (b - a) as f32) }
        })
        .collect()
}

/// a note found in a recording
#[derive(Clone, Debug)]
pub struct DNote {
    /// seconds in the recording
    pub on: f64,
    pub off: f64,
    /// the note's pitch (median over its length, so vibrato does not count) in fractional MIDI
    pub midi: f64,
}

/// Cut a pitch track into sung notes: a note ends when the pitch leaves it for a few frames or the voice stops for 60 ms; notes under
/// 70 ms are dropped; neighbours on the same semitone with a hair of a gap are joined.
pub fn detect_notes(p: &Pitch) -> Vec<DNote> {
    let nj = p.f0.len();
    let m: Vec<f64> = p.f0.iter().map(|f| if *f > 0.0 { hz_midi(*f as f64) } else { f64::NAN }).collect();
    let median = |v: &mut Vec<f64>| -> f64 {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let gap_frames = ((0.06 / p.hop) as usize).max(2);
    let mut out: Vec<DNote> = vec![];
    let mut j = 0;
    while j < nj {
        if m[j].is_nan() {
            j += 1;
            continue;
        }
        let j0 = j;
        let mut vals: Vec<f64> = vec![m[j]];
        let mut last = j;
        let mut k = j + 1;
        while k < nj {
            if m[k].is_nan() {
                if k - last > gap_frames {
                    break;
                }
            } else {
                let mut r: Vec<f64> = vals.iter().rev().take(8).cloned().collect();
                let med = median(&mut r);
                if (m[k] - med).abs() > 0.7 {
                    let stay = (k..(k + 4).min(nj)).filter(|q| !m[*q].is_nan() && (m[*q] - med).abs() > 0.7).count();
                    if stay >= 3 {
                        break;
                    }
                } else {
                    vals.push(m[k]);
                    last = k;
                }
            }
            k += 1;
        }
        let j1 = last + 1;
        let dur = (j1 - j0) as f64 * p.hop;
        if dur >= 0.07 && vals.len() >= 4 {
            let mut v = vals.clone();
            let c = median(&mut v);
            out.push(DNote { on: j0 as f64 * p.hop, off: j1 as f64 * p.hop, midi: c });
        }
        j = k.max(j1);
    }
    // join neighbours that are one note with a hiccup between
    let mut joined: Vec<DNote> = vec![];
    for n in out {
        if let Some(l) = joined.last_mut() {
            if n.on - l.off < 0.05 && (libm::round(n.midi) - libm::round(l.midi)).abs() < 0.5 {
                l.off = n.off;
                continue;
            }
        }
        joined.push(n);
    }
    joined
}

// ── the time map ────────────────────────────────────────────────────────────
struct TimeMap {
    pts: Vec<(f64, f64)>, // (output time, input time)
}
impl TimeMap {
    fn map(&self, t: f64) -> f64 {
        if self.pts.is_empty() {
            return t;
        }
        if t <= self.pts[0].0 {
            return self.pts[0].1 + (t - self.pts[0].0);
        }
        for w in self.pts.windows(2) {
            if t <= w[1].0 {
                let f = (t - w[0].0) / (w[1].0 - w[0].0).max(1e-9);
                return w[0].1 + f * (w[1].1 - w[0].1);
            }
        }
        let l = self.pts.last().unwrap();
        l.1 + (t - l.0)
    }
}

// ── PSOLA ───────────────────────────────────────────────────────────────────
/// analysis marks (sample positions): one per period inside the voiced stretches
fn pitch_marks(mono: &[f32], p: &Pitch, sr: f32) -> Vec<usize> {
    let mut marks: Vec<usize> = vec![];
    let n = mono.len();
    let mut pos = 0usize;
    let mut j = 0usize;
    while j < p.f0.len() {
        if p.f0[j] <= 0.0 {
            j += 1;
            continue;
        }
        // a voiced run
        let mut e = j;
        while e < p.f0.len() && p.f0[e] > 0.0 {
            e += 1;
        }
        let t0 = (j as f64 * p.hop * sr as f64) as usize;
        let t1 = ((e as f64 * p.hop * sr as f64) as usize).min(n);
        let per0 = (sr / p.f0[j]) as usize;
        let mut cur = t0.max(pos);
        // one polarity for the whole run: marks sit on the strongest peak of the side that is larger
        let probe_hi = (cur + per0 * 4).min(t1.max(cur + 1)).min(n);
        let (mx, mn) = mono[cur.min(n.saturating_sub(1))..probe_hi.max(cur.min(n.saturating_sub(1)))].iter().fold((0.0f32, 0.0f32), |m, x| (m.0.max(*x), m.1.min(*x)));
        let pol = if mx >= -mn { 1.0f32 } else { -1.0 };
        let hi = (cur + per0).min(n);
        if hi > cur {
            cur = (cur..hi).max_by(|a, b| (mono[*a] * pol).partial_cmp(&(mono[*b] * pol)).unwrap()).unwrap_or(cur);
        }
        while cur < t1 {
            marks.push(cur);
            let f = p.at(cur as f64 / sr as f64).max(60.0) as f64;
            let per = (sr as f64 / f) as usize;
            let centre = cur + per.max(8);
            let w = (per / 5).max(2);
            let lo = centre.saturating_sub(w);
            let hi = (centre + w).min(n.saturating_sub(1));
            if hi <= lo {
                break;
            }
            cur = (lo..=hi).max_by(|a, b| (mono[*a] * pol).partial_cmp(&(mono[*b] * pol)).unwrap()).unwrap_or(centre);
        }
        pos = t1;
        j = e;
    }
    marks.dedup();
    marks
}

fn nearest_mark(marks: &[usize], t: usize) -> usize {
    let i = marks.partition_point(|m| *m < t);
    if i == 0 {
        return marks[0];
    }
    if i >= marks.len() {
        return marks[marks.len() - 1];
    }
    if t - marks[i - 1] <= marks[i] - t {
        marks[i - 1]
    } else {
        marks[i]
    }
}

struct Windows(BTreeMap<usize, Vec<f32>>);
impl Windows {
    fn get(&mut self, len: usize) -> &Vec<f32> {
        self.0.entry(len).or_insert_with(|| (0..len).map(|i| 0.5 - 0.5 * libm::cosf(core::f32::consts::TAU * (i as f32 + 0.5) / len as f32)).collect())
    }
}

/// Re-synthesise `stereo` with pitch-synchronous overlap-add into `n_out` frames: pitch moves by `ratio` (one value per `p.hop` of
/// output time), time follows `tm`.
fn resynth_n(stereo: &[f32], mono: &[f32], sr: f32, p: &Pitch, tm: &TimeMap, ratio: &[f32], n_out: usize) -> Vec<f32> {
    let n = mono.len();
    if n == 0 || n_out == 0 {
        return vec![0.0; n_out * 2];
    }
    let marks = pitch_marks(mono, p, sr);
    let mut out = vec![0.0f32; n_out * 2];
    let mut wsum = vec![0.0f32; n_out];
    let mut wins = Windows(BTreeMap::new());
    let grid_hop = p.hop * sr as f64;
    let mut cur = 0.0f64;
    let un_len = ((0.012 * sr) as usize) | 1;
    let un_hop = 0.006 * sr as f64;
    let mut guard = 0usize;
    while cur < n_out as f64 && guard < n_out * 4 {
        guard += 1;
        let t_out = cur / sr as f64;
        let t_in = tm.map(t_out);
        let in_pos = (t_in * sr as f64).clamp(0.0, (n - 1) as f64);
        let f = p.at(t_in) as f64;
        let (centre, len, hop_out) = if f > 0.0 && !marks.is_empty() {
            let per_in = (sr as f64 / f).max(8.0);
            let r = ratio.get((cur / grid_hop) as usize).copied().unwrap_or(1.0) as f64;
            let m = nearest_mark(&marks, in_pos as usize);
            (m, ((2.0 * per_in) as usize) | 1, per_in / r.clamp(0.5, 2.0))
        } else {
            (in_pos as usize, un_len, un_hop)
        };
        let half = len / 2;
        let win = wins.get(len).clone();
        let oc = cur as isize;
        for (k, w) in win.iter().enumerate() {
            let src = centre as isize + k as isize - half as isize;
            let dst = oc + k as isize - half as isize;
            if src < 0 || src as usize >= n || dst < 0 || dst as usize >= n_out {
                continue;
            }
            let (s, d) = (src as usize, dst as usize);
            out[d * 2] += stereo[s * 2] * w;
            out[d * 2 + 1] += stereo[s * 2 + 1] * w;
            wsum[d] += w;
        }
        cur += hop_out.max(2.0);
    }
    for i in 0..n_out {
        let g = 1.0 / wsum[i].max(0.25);
        out[i * 2] *= g;
        out[i * 2 + 1] *= g;
        if !out[i * 2].is_finite() {
            out[i * 2] = 0.0;
        }
        if !out[i * 2 + 1].is_finite() {
            out[i * 2 + 1] = 0.0;
        }
    }
    out
}

// ── the inline vocal editor's render ────────────────────────────────────────

/// One sung note of an edited vocal: where it is in the recording (`src_*`, seconds), where the edit puts it (`dst_*`, seconds on the
/// same time line), its detected pitch (fractional MIDI), how far it is moved (semitones), how much of its pitch drift is flattened
/// toward its centre (0..1) and its loudness (linear gain).
#[derive(Clone, Debug)]
pub struct Seg {
    pub src_on: f64,
    pub src_off: f64,
    pub dst_on: f64,
    pub dst_off: f64,
    pub midi: f64,
    pub shift: f64,
    pub drift: f64,
    pub gain: f64,
}

#[derive(Clone, Debug)]
pub struct EditOpts {
    /// vibrato added to notes of 0.3 s and longer (0 = none)
    pub vib_depth_cents: f64,
    pub vib_rate: f64,
    /// how fast the pitch moves from one note's correction to the next (ms; the glide between notes)
    pub retune_ms: f64,
}

/// The time map an edit makes: both ends of every note, in order (a point that would run backwards is skipped).
fn seg_time_map(segs: &[Seg]) -> TimeMap {
    let mut pts: Vec<(f64, f64)> = vec![];
    for g in segs {
        for (o, i) in [(g.dst_on, g.src_on), (g.dst_off, g.src_off)] {
            if let Some(l) = pts.last() {
                if o <= l.0 + 1e-4 || i <= l.1 + 1e-4 {
                    continue;
                }
            }
            pts.push((o, i));
        }
    }
    TimeMap { pts }
}

/// where the recording time `t_in` lands after the edit (the inverse of the time map)
pub fn segs_out_time(segs: &[Seg], t_in: f64) -> f64 {
    let tm = seg_time_map(segs);
    if tm.pts.is_empty() {
        return t_in;
    }
    if t_in <= tm.pts[0].1 {
        return tm.pts[0].0 + (t_in - tm.pts[0].1);
    }
    for w in tm.pts.windows(2) {
        if t_in <= w[1].1 {
            let f = (t_in - w[0].1) / (w[1].1 - w[0].1).max(1e-9);
            return w[0].0 + f * (w[1].0 - w[0].0);
        }
    }
    let l = tm.pts.last().unwrap();
    l.0 + (t_in - l.1)
}

/// Render an edited vocal: every note gets its own pitch move, drift correction, timing / length (time-slice stretch) and gain, with
/// pitch-synchronous resynthesis (the formants stay). `p` is the pitch track of `stereo`. The output may be longer or shorter than the input.
pub fn render_segments(stereo: &[f32], sr: f32, p: &Pitch, segs: &[Seg], o: &EditOpts) -> Vec<f32> {
    let mono = mono_of(stereo);
    let n = mono.len();
    let mut s: Vec<Seg> = segs.iter().filter(|g| g.dst_off > g.dst_on && g.src_off > g.src_on).cloned().collect();
    s.sort_by(|a, b| a.dst_on.total_cmp(&b.dst_on));
    let tm = seg_time_map(&s);
    let end_out = segs_out_time(&s, n as f64 / sr as f64).max(0.05);
    let n_out = (end_out * sr as f64) as usize;
    let hop = p.hop;
    let nj = (n_out as f64 / sr as f64 / hop) as usize + 2;
    let mut desired = vec![0.0f64; nj];
    let mut active = vec![false; nj];
    let mut k = 0usize;
    for j in 0..nj {
        let t = j as f64 * hop;
        while k < s.len() && s[k].dst_off + 0.01 < t {
            k += 1;
        }
        let Some(g) = s.get(k).filter(|g| t >= g.dst_on - 0.01) else { continue };
        let f = p.at(tm.map(t)) as f64;
        if f <= 0.0 {
            continue;
        }
        let mut c = g.shift * 100.0 + g.drift.clamp(0.0, 1.0) * cents(midi_hz(g.midi), f);
        let len = g.dst_off - g.dst_on;
        if o.vib_depth_cents > 0.0 && len >= 0.3 {
            let delay = 0.15_f64.min(len * 0.3);
            let fade = ((t - g.dst_on - delay) / 0.25).clamp(0.0, 1.0);
            let tail = ((g.dst_off - t) / 0.1).clamp(0.0, 1.0);
            c += o.vib_depth_cents * fade * tail * libm::sin(core::f64::consts::TAU * o.vib_rate * (t - g.dst_on - delay).max(0.0));
        }
        desired[j] = c;
        active[j] = true;
    }
    // glide between notes; an unpitched stretch keeps the last correction (its grains do not use it, and the next note starts from there)
    let alpha = 1.0 - libm::exp(-hop / (o.retune_ms / 1000.0).max(0.001));
    let mut corr = vec![0.0f64; nj];
    let mut prev = 0.0f64;
    let mut was = false;
    for j in 0..nj {
        if active[j] {
            prev = if was { prev + alpha * (desired[j] - prev) } else { desired[j] };
            was = true;
        } else {
            was = false;
        }
        corr[j] = prev;
    }
    let ratio: Vec<f32> = corr.iter().map(|c| libm::pow(2.0, c / 1200.0) as f32).collect();
    let mut out = resynth_n(stereo, &mono, sr, p, &tm, &ratio, n_out);
    // loudness of every note, with 10 ms ramps so a gain change never clicks
    let ramp = 0.01;
    for g in &s {
        if (g.gain - 1.0).abs() < 1e-3 {
            continue;
        }
        let i0 = (((g.dst_on - ramp) * sr as f64).max(0.0) as usize).min(n_out);
        let i1 = (((g.dst_off + ramp) * sr as f64) as usize).min(n_out);
        for i in i0..i1 {
            let t = i as f64 / sr as f64;
            let w = ((t - (g.dst_on - ramp)) / ramp).clamp(0.0, 1.0).min(((g.dst_off + ramp - t) / ramp).clamp(0.0, 1.0));
            let gain = (1.0 + (g.gain.max(0.0) - 1.0) * w) as f32;
            out[i * 2] *= gain;
            out[i * 2 + 1] *= gain;
        }
    }
    out
}

// ── scales and temperaments ─────────────────────────────────────────────────

/// ling-daw `vocmidi_api::SCALES` (the Correct pitch menu)
pub const SCALES: [(&str, [bool; 12]); 9] = [
    ("major", [true, false, true, false, true, true, false, true, false, true, false, true]),
    ("minor", [true, false, true, true, false, true, false, true, true, false, true, false]),
    ("harmonic minor", [true, false, true, true, false, true, false, true, true, false, false, true]),
    ("dorian", [true, false, true, true, false, true, false, true, false, true, true, false]),
    ("mixolydian", [true, false, true, false, true, true, false, true, false, true, true, false]),
    ("pentatonic major", [true, false, true, false, true, false, false, true, false, true, false, false]),
    ("pentatonic minor", [true, false, false, true, false, true, false, true, false, false, true, false]),
    ("blues", [true, false, false, true, false, true, true, true, false, false, true, false]),
    ("chromatic", [true; 12]),
];

/// Deviations in cents from equal temperament of the twelve notes counted from the key's tonic
/// (0 equal, 1 just intonation, 2 Pythagorean, 3 quarter-comma meantone, 4 Werckmeister III).
pub const TEMPERAMENT_NAMES: [&str; 5] = ["Equal", "Just", "Pythagorean", "Meantone", "Werckmeister"];
pub const TEMPERAMENTS: [[f64; 12]; 5] = [
    [0.0; 12],
    [0.0, 11.73, 3.91, 15.64, -13.69, -1.96, -9.78, 1.96, 13.69, -15.64, 17.60, -11.73],
    [0.0, -9.78, 3.91, -5.87, 7.82, -1.96, 11.73, 1.96, -7.82, 5.87, -3.91, 9.78],
    [0.0, -23.95, -6.84, 10.26, -13.69, 3.42, -20.53, -3.42, -27.37, -10.26, 6.84, -17.11],
    [0.0, -9.78, -7.82, -5.87, -9.78, -1.96, -11.73, -3.91, -7.82, -11.73, -3.91, -7.82],
];

/// The nearest allowed pitch (a MIDI note number) to the fractional note `m`; `prefer` keeps the last note when it is nearly as close
/// (so a sung slide does not flip between two neighbours).
pub fn nearest_allowed(m: f64, allowed: &[bool; 12], prefer: Option<i32>) -> i32 {
    let base = libm::round(m) as i32;
    let mut best = base;
    let mut bd = f64::MAX;
    for k in base - 3..=base + 3 {
        if allowed[k.rem_euclid(12) as usize] {
            let d = (k as f64 - m).abs();
            if d < bd {
                bd = d;
                best = k;
            }
        }
    }
    if let Some(p) = prefer {
        if allowed[p.rem_euclid(12) as usize] && (p as f64 - m).abs() <= bd + 0.18 {
            return p;
        }
    }
    best
}

// ── the editor's notes ("blobs", ling-daw api/vedit_api.rs) ─────────────────

pub const MIN_LEN: f64 = 0.03;

/// `v` within [lo, hi]; when the bounds cross (a neighbour closer than the minimum length) the lower bound wins.
pub fn clamp_ok(v: f64, lo: f64, hi: f64) -> f64 {
    v.max(lo).min(hi.max(lo))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Blob {
    pub src_on: f64,
    pub src_off: f64,
    pub dst_on: f64,
    pub dst_off: f64,
    pub midi: f64,
    pub shift: f64,
    pub drift: f64,
    pub gain: f64,
    pub vel: f64,
}

impl Blob {
    pub fn seg(&self) -> Seg {
        Seg { src_on: self.src_on, src_off: self.src_off, dst_on: self.dst_on, dst_off: self.dst_off, midi: self.midi, shift: self.shift, drift: self.drift, gain: self.gain }
    }
}

/// the selection the script passes: "3,4,7" (empty = every blob)
pub fn sel_of(s: &str, n: usize) -> Vec<usize> {
    let v: Vec<usize> = s.split(',').filter_map(|x| x.trim().parse::<usize>().ok()).filter(|i| *i < n).collect();
    if v.is_empty() && s.trim().is_empty() { (0..n).collect() } else { v }
}

pub fn median_midi(p: &Pitch, a: f64, b: f64, fallback: f64) -> f64 {
    let mut v: Vec<f64> = (((a / p.hop) as usize)..((b / p.hop) as usize).min(p.f0.len())).map(|j| p.f0[j]).filter(|f| *f > 0.0).map(|f| hz_midi(f as f64)).collect();
    if v.len() < 3 {
        return fallback;
    }
    v.sort_by(|x, y| x.total_cmp(y));
    v[v.len() / 2]
}

/// keep the notes in order and apart: no note starts before the one before it ends, and none is shorter than MIN_LEN
pub fn tidy(blobs: &mut [Blob]) {
    blobs.sort_by(|a, b| a.dst_on.total_cmp(&b.dst_on));
    for i in 0..blobs.len() {
        blobs[i].dst_on = blobs[i].dst_on.max(0.0);
        if i > 0 && blobs[i].dst_on < blobs[i - 1].dst_off {
            // the earlier note gives way (it is shortened), unless that would make it too short
            let lo = blobs[i - 1].dst_on + MIN_LEN;
            if blobs[i].dst_on >= lo {
                blobs[i - 1].dst_off = blobs[i].dst_on;
            } else {
                blobs[i - 1].dst_off = lo;
                let len = blobs[i].dst_off - blobs[i].dst_on;
                blobs[i].dst_on = lo;
                blobs[i].dst_off = blobs[i].dst_off.max(lo + len.max(MIN_LEN));
            }
        }
        if blobs[i].dst_off < blobs[i].dst_on + MIN_LEN {
            blobs[i].dst_off = blobs[i].dst_on + MIN_LEN;
        }
    }
}

pub fn allowed_of(scale: usize, root: i32) -> [bool; 12] {
    let mut allowed = [false; 12];
    for (k, on) in SCALES[scale.min(SCALES.len() - 1)].1.iter().enumerate() {
        allowed[(k as i32 + root).rem_euclid(12) as usize] = *on;
    }
    allowed
}

/// a whole-number key in a temperament (with its tonic), as a fractional MIDI pitch
pub fn tempered(temp: usize, troot: i32, key: i32) -> f64 {
    let t = TEMPERAMENTS[temp.min(TEMPERAMENTS.len() - 1)];
    key as f64 + t[(key - troot).rem_euclid(12) as usize] / 100.0
}

/// `daw_ve_set`'s edits on the blobs (selection `sel`), true when the key is known
pub fn set_key(blobs: &mut [Blob], sel: &[usize], key: &str, v: f64) -> bool {
    let n = blobs.len();
    match key {
        "shift" => sel.iter().for_each(|i| blobs[*i].shift = v.clamp(-36.0, 36.0)),
        "dshift" => sel.iter().for_each(|i| blobs[*i].shift = (blobs[*i].shift + v).clamp(-36.0, 36.0)),
        "drift" => sel.iter().for_each(|i| blobs[*i].drift = v.clamp(0.0, 1.0)),
        "gain" => sel.iter().for_each(|i| blobs[*i].gain = v.clamp(0.0, 4.0)),
        "dgain" => sel.iter().for_each(|i| blobs[*i].gain = (blobs[*i].gain * libm::pow(10.0, v / 20.0)).clamp(0.0, 4.0)),
        "move" => {
            // the selection moves as one block and stops at the first unselected neighbour on either side
            let mut d = v;
            let insel = |i: usize| sel.contains(&i);
            for &i in sel {
                let b = &blobs[i];
                d = d.max(-b.dst_on);
                if let Some(p) = (0..i).rev().find(|j| !insel(*j)) {
                    d = d.max(blobs[p].dst_off - b.dst_on);
                }
                if let Some(q) = (i + 1..n).find(|j| !insel(*j)) {
                    d = d.min(blobs[q].dst_on - b.dst_off);
                }
            }
            for &i in sel {
                blobs[i].dst_on += d;
                blobs[i].dst_off += d;
            }
        },
        "on" | "off" => {
            let Some(&i) = sel.first() else { return false };
            let prev_end = if i > 0 { blobs[i - 1].dst_off } else { 0.0 };
            let next_on = blobs.get(i + 1).map(|b| b.dst_on).unwrap_or(f64::MAX);
            let b = &mut blobs[i];
            if key == "on" {
                b.dst_on = clamp_ok(v, prev_end, b.dst_off - MIN_LEN);
            } else {
                b.dst_off = clamp_ok(v, b.dst_on + MIN_LEN, next_on);
            }
        },
        _ => return false,
    }
    tidy(blobs);
    true
}

/// `daw_ve_split`: blob `i` cut in two at `at` (seconds on the edited time line), false when that is too near an end
pub fn split(blobs: &mut Vec<Blob>, p: &Pitch, i: usize, at: f64) -> bool {
    let Some(b) = blobs.get(i).cloned() else { return false };
    if at < b.dst_on + MIN_LEN || at > b.dst_off - MIN_LEN {
        return false;
    }
    let f = (at - b.dst_on) / (b.dst_off - b.dst_on);
    let ts = b.src_on + f * (b.src_off - b.src_on);
    let mut l = b.clone();
    let mut r = b.clone();
    l.src_off = ts;
    l.dst_off = at;
    r.src_on = ts;
    r.dst_on = at;
    l.midi = median_midi(p, l.src_on, l.src_off, b.midi);
    r.midi = median_midi(p, r.src_on, r.src_off, b.midi);
    blobs[i] = l;
    blobs.insert(i + 1, r);
    true
}

/// `daw_ve_merge`: blob `i` and the one after it become one note
pub fn merge(blobs: &mut Vec<Blob>, p: &Pitch, i: usize) -> bool {
    if i + 1 >= blobs.len() {
        return false;
    }
    let r = blobs.remove(i + 1);
    let l = &mut blobs[i];
    let (ll, rl) = (l.dst_off - l.dst_on, r.dst_off - r.dst_on);
    l.src_off = r.src_off;
    l.dst_off = r.dst_off;
    if rl > ll {
        l.shift = r.shift;
    }
    l.drift = l.drift.max(r.drift);
    l.gain = (l.gain * ll + r.gain * rl) / (ll + rl).max(1e-6);
    l.vel = l.vel.max(r.vel);
    l.midi = median_midi(p, l.src_on, l.src_off, l.midi);
    true
}

/// `daw_ve_correct`: every selected note's centre goes `centre` of the way to the nearest note `allowed` allows (in the temperament);
/// `drift` >= 0 also sets the drift flattening. The number of notes changed.
pub fn correct(blobs: &mut [Blob], sel: &[usize], centre: f64, drift: f64, allowed: &[bool; 12], temp: usize, troot: i32) -> usize {
    let mut prev: Option<i32> = None;
    let mut changed = 0usize;
    for &i in sel {
        let b = &mut blobs[i];
        let cur = b.midi + b.shift;
        let want = nearest_allowed(cur, allowed, prev);
        prev = Some(want);
        let target = tempered(temp, troot, want);
        let ns = b.shift + (target - cur) * centre;
        if (ns - b.shift).abs() > 1e-4 || (drift >= 0.0 && (drift - b.drift).abs() > 1e-4) {
            changed += 1;
        }
        b.shift = ns;
        if drift >= 0.0 {
            b.drift = drift.clamp(0.0, 1.0);
        }
    }
    changed
}

/// A clip's place on the time line: the recording's seconds <-> the project's beats (ling-daw vedit_api `Geo`).
#[derive(Clone, Copy, Debug)]
pub struct Geo {
    pub offset_s: f64,
    pub spb: f64,
    pub start: f64,
    pub len: f64,
}
impl Geo {
    pub fn beat(&self, s: f64) -> f64 {
        self.start + (s - self.offset_s) / self.spb
    }
    pub fn secs(&self, b: f64) -> f64 {
        self.offset_s + (b - self.start) * self.spb
    }
}

/// `daw_ve_quantize`: starts go `strength` of the way to the grid (in beats), lengths stay. The number of notes moved.
pub fn quantize(blobs: &mut [Blob], sel: &[usize], g: &Geo, grid: f64, strength: f64) -> usize {
    let mut moved = 0usize;
    for &i in sel {
        let b = &mut blobs[i];
        let beat = g.beat(b.dst_on);
        let d = (g.secs(libm::round(beat / grid) * grid) - b.dst_on) * strength;
        if d.abs() > 1e-4 {
            b.dst_on += d;
            b.dst_off += d;
            moved += 1;
        }
    }
    tidy(blobs);
    moved
}

/// `daw_ve_align`: each sung note is matched to the guide note (start beat, end beat, pitch) it overlaps most; the notes on one
/// guide note keep their shares of it. The number of notes changed, None when the guide has nothing near the clip.
pub fn align(blobs: &mut [Blob], g: &Geo, guide: Vec<(f64, f64, f64)>, do_pitch: bool, do_time: bool, strength: f64, temp: usize, troot: i32) -> Option<usize> {
    let window = (g.start, g.start + g.len);
    let guide: Vec<(f64, f64, f64)> = guide.into_iter().filter(|n| n.1 > window.0 - 1.0 && n.0 < window.1 + 1.0).collect();
    if guide.is_empty() {
        return None;
    }
    // match every blob to a guide note
    let pick: Vec<Option<usize>> = blobs
        .iter()
        .map(|b| {
            let (bs, be) = (g.beat(b.dst_on), g.beat(b.dst_off));
            let over = |n: &(f64, f64, f64)| (be.min(n.1) - bs.max(n.0)).max(0.0);
            let best = guide.iter().enumerate().max_by(|x, y| over(x.1).total_cmp(&over(y.1)));
            match best {
                Some((k, n)) if over(n) > 0.0 => Some(k),
                _ => guide.iter().enumerate().filter(|(_, n)| (n.0 - bs).abs() < 0.5).min_by(|x, y| (x.1 .0 - bs).abs().total_cmp(&(y.1 .0 - bs).abs())).map(|(k, _)| k),
            }
        })
        .collect();
    let mut changed = 0usize;
    let mut i = 0;
    while i < blobs.len() {
        let Some(k) = pick[i] else {
            i += 1;
            continue;
        };
        let mut j = i;
        while j + 1 < blobs.len() && pick[j + 1] == Some(k) {
            j += 1;
        }
        let (gs, ge, gp) = guide[k];
        if do_time {
            // the group [i..=j] is remapped linearly onto the guide note
            let (o0, o1) = (blobs[i].dst_on, blobs[j].dst_off);
            let (n0, n1) = (o0 + (g.secs(gs) - o0) * strength, o1 + (g.secs(ge) - o1) * strength);
            if n1 > n0 + MIN_LEN && o1 > o0 {
                for b in &mut blobs[i..=j] {
                    let f0 = (b.dst_on - o0) / (o1 - o0);
                    let f1 = (b.dst_off - o0) / (o1 - o0);
                    b.dst_on = n0 + f0 * (n1 - n0);
                    b.dst_off = n0 + f1 * (n1 - n0);
                }
            }
        }
        if do_pitch {
            for b in &mut blobs[i..=j] {
                let cur = b.midi + b.shift;
                // the guide's pitch in the singer's octave
                let key = libm::round(gp + 12.0 * libm::round((cur - gp) / 12.0)) as i32;
                let target = if (gp - libm::trunc(gp)).abs() < 1e-6 { tempered(temp, troot, key) } else { gp + 12.0 * libm::round((cur - gp) / 12.0) };
                b.shift += (target - cur) * strength;
            }
        }
        changed += j - i + 1;
        i = j + 1;
    }
    tidy(blobs);
    Some(changed)
}

/// `daw_ve_reset`: the selected notes go back to where and how they were sung
pub fn reset(blobs: &mut [Blob], sel: &[usize]) {
    for &i in sel {
        let b = &mut blobs[i];
        b.dst_on = b.src_on;
        b.dst_off = b.src_off;
        b.shift = 0.0;
        b.drift = 0.0;
        b.gain = 1.0;
    }
    tidy(blobs);
}

/// `daw_ve_curve`: [pitch after the edit (fractional MIDI, -1 = unpitched), level 0..1] * n across blob `b`
pub fn curve(b: &Blob, p: &Pitch, level: &[f32], top: f32, n: usize) -> Vec<f64> {
    let mut v = Vec::with_capacity(n * 2);
    for k in 0..n {
        let f = (k as f64 + 0.5) / n as f64;
        let ts = b.src_on + f * (b.src_off - b.src_on);
        let hz = p.at(ts) as f64;
        let m = if hz > 0.0 {
            let m = hz_midi(hz);
            m + b.drift * (b.midi - m) + b.shift
        } else {
            -1.0
        };
        let j = ((ts / p.hop) as usize).min(level.len().saturating_sub(1));
        let l = level.get(j).copied().unwrap_or(0.0) / top;
        v.push(m);
        v.push((l as f64 * b.gain).clamp(0.0, 1.5));
    }
    v
}

/// `daw_ve_analyze`'s notes: the detected ones with their loudness (0.05..1, against the loudest moment)
pub fn blobs_of(p: &Pitch, level: &[f32], top: f32) -> Vec<Blob> {
    let lv = |a0: f64, a1: f64| -> f64 {
        let (j0, j1) = ((a0 / p.hop) as usize, ((a1 / p.hop) as usize).min(level.len()));
        if j1 <= j0 { 0.0 } else { (level[j0..j1].iter().cloned().fold(0.0, f32::max) / top) as f64 }
    };
    detect_notes(p)
        .iter()
        .map(|n| Blob { src_on: n.on, src_off: n.off, dst_on: n.on, dst_off: n.off, midi: n.midi, shift: 0.0, drift: 0.0, gain: 1.0, vel: lv(n.on, n.off).clamp(0.05, 1.0) })
        .collect()
}

/// Is the edit a no-op (the clip can play the recording itself)?
pub fn untouched(blobs: &[Blob], vdepth: f64) -> bool {
    blobs.iter().all(|b| b.shift.abs() < 1e-4 && b.drift < 1e-4 && (b.gain - 1.0).abs() < 1e-4 && (b.dst_on - b.src_on).abs() < 1e-5 && (b.dst_off - b.src_off).abs() < 1e-5) && vdepth <= 0.0
}
