//! LingStationX tools on LingOS that the Windows engine computes, ported with the same results:
//!
//! * the **factory presets** of the Growl, Prism and Voice instruments (ling-daw's `*_presets.txt`, embedded here: the
//!   chooser lists them and `daw_inst_preset` loads them),
//! * the **Growl presets' effect chains** (`daw_inst_preset_has_fx` / `daw_inst_preset_fx`: the `fx Name k=v` lines),
//! * the piano roll's **arpeggiator** (`daw_roll_arp`, compose.rs `arpeggiate`),
//! * **BPM detection** of an audio clip (`daw_clip_detect_bpm`, ling-music analysis.rs: spectral-flux onsets, autocorrelation).

use alloc::string::String;
use alloc::vec::Vec;

use super::{fx, Daw, NoteDoc};
use crate::lingrt::{is_list, list_items, num, to_f64, to_string};

static GROWL: &str = include_str!("../../../ling-daw/src/growl_presets.txt");
static PRISM: &str = include_str!("../../../ling-daw/src/prism_presets.txt");
static VOICE: &str = include_str!("../../../ling-daw/src/voice_presets.txt");

fn text_of(kind: &str) -> Option<&'static str> {
    match kind {
        "growl" => Some(GROWL),
        "prism" => Some(PRISM),
        "voice" => Some(VOICE),
        _ => None,
    }
}

/// The `[Name]` sections of an instrument's factory preset text, in order: (name, section text).
fn sections(text: &'static str) -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&'static str, &'static str)> = Vec::new();
    let mut cur: Option<(&'static str, usize)> = None;
    let mut pos = 0usize;
    for line in text.split_inclusive('\n') {
        let l = line.trim_end();
        if l.starts_with('[') && l.ends_with(']') && l.len() > 2 {
            if let Some((n, st)) = cur.take() {
                out.push((n, &text[st..pos]));
            }
            cur = Some((&l[1..l.len() - 1], pos + line.len()));
        }
        pos += line.len();
    }
    if let Some((n, st)) = cur {
        out.push((n, &text[st..]));
    }
    out
}

/// Factory presets of an instrument kind as (name, marker path `preset:<kind>:<name>`), [] for kinds without a text table.
pub(super) fn factory_presets(kind: &str) -> Vec<(String, String)> {
    let Some(t) = text_of(kind) else { return Vec::new() };
    sections(t).into_iter().filter(|(n, _)| !n.starts_with('_')).map(|(n, _)| (String::from(n), alloc::format!("preset:{kind}:{n}"))).collect()
}

/// The patch text behind a `preset:<kind>:<name>` marker.
pub(super) fn factory_text(path: &str) -> Option<&'static str> {
    let rest = path.strip_prefix("preset:")?;
    let (kind, name) = rest.split_once(':')?;
    sections(text_of(kind)?).into_iter().find(|(n, _)| *n == name).map(|(_, t)| t)
}

/// growl.rs `preset_fx`: the chain a Growl preset suggests, (effect, [(knob, value)]).
fn growl_fx(name: &str) -> Vec<(String, Vec<(String, f32)>)> {
    let Some((_, t)) = sections(GROWL).into_iter().find(|(n, _)| *n == name) else { return Vec::new() };
    let mut out = Vec::new();
    for line in t.lines() {
        let Some(rest) = line.trim().strip_prefix("fx ") else { continue };
        let mut it = rest.split_whitespace();
        let Some(fxn) = it.next() else { continue };
        let ps = it.filter_map(|t| t.split_once('=')).filter_map(|(k, v)| v.parse::<f32>().ok().map(|v| (k.replace('_', " "), v))).collect();
        out.push((fxn.replace('_', " "), ps));
    }
    out
}

// ── arpeggiator (compose.rs) ─────────────────────────────────────────────────

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) ^ 0xD1B54A32D192ED03 | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

#[derive(Clone, Copy)]
struct N {
    start: f64,
    len: f64,
    key: u8,
    vel: f32,
}

fn parse_rate(r: &str) -> f64 {
    let t = r.trim().to_ascii_lowercase();
    if let Some((a, b)) = t.split_once('/') {
        if let (Ok(a), Ok(b)) = (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
            if b > 0.0 {
                return (4.0 * a / b).clamp(0.0625, 4.0);
            }
        }
    }
    if let Ok(x) = t.parse::<f64>() {
        return x.clamp(0.0625, 4.0);
    }
    0.0
}

fn euclid(hits: usize, steps: usize, rot: usize) -> Vec<bool> {
    let steps = steps.clamp(1, 64);
    let hits = hits.min(steps);
    let mut v = alloc::vec![false; steps];
    if hits > 0 {
        for i in 0..hits {
            v[((i * steps + hits - 1) / hits + rot) % steps] = true;
        }
    }
    v
}

fn rhythm_mask(r: &str) -> Vec<bool> {
    let t = r.trim().to_ascii_lowercase();
    if let Some(rest) = t.strip_prefix("euclid:") {
        let n: Vec<usize> = rest.split(|c: char| c == ',' || c == '/' || c == ' ').filter_map(|x| x.trim().parse().ok()).collect();
        if n.len() >= 2 {
            return euclid(n[0], n[1], n.get(2).copied().unwrap_or(0));
        }
    }
    let lit = |m: &str| m.chars().map(|c| c == 'x' || c == 'X').collect::<Vec<bool>>();
    match t.as_str() {
        "" | "16ths" | "8ths" | "all" | "straight" | "triplet" => Vec::new(),
        "offbeat" => lit("..x...x...x...x."),
        "gallop" => lit("x.xxx.xxx.xxx.xx"),
        "syncopated" => lit("x..x..x...x..x.."),
        "dotted" => lit("x..x..x.x..x..x."),
        "pulse" => lit("x...x...x...x..."),
        "charleston" => lit("x..x............"),
        s if s.len() >= 2 && s.chars().all(|c| c == 'x' || c == '.' || c == '-') => lit(&s.replace('-', ".")),
        _ => Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn arp_notes(pitches: &[u8], start: f64, len: f64, rate: f64, order: &str, gate: f64, mask: &[bool], rng: &mut Rng) -> Vec<N> {
    let mut out = Vec::new();
    if pitches.is_empty() || rate <= 0.0 {
        return out;
    }
    let n = pitches.len();
    let seq: Vec<usize> = match order {
        "down" => (0..n).rev().collect(),
        "updown" => (0..n).chain((1..n.saturating_sub(1)).rev()).collect(),
        "pingpong" => (0..n).chain((0..n).rev()).collect(),
        "random" => (0..16).map(|_| (rng.next() % n as u64) as usize).collect(),
        _ => (0..n).collect(),
    };
    let count = (len / rate + 1e-6) as usize;
    let mut hit = 0usize;
    for i in 0..count {
        if !mask.is_empty() && !mask[i % mask.len()] {
            continue;
        }
        let t = i as f64 * rate;
        let on_beat = libm::fabs(t % 1.0) < 0.01;
        out.push(N { start: start + t, len: (rate * gate).max(0.05), key: pitches[seq[hit % seq.len()]], vel: if on_beat { 0.9 } else { 0.7 } });
        hit += 1;
    }
    out
}

fn arpeggiate(notes: &[N], rate: f64, order: &str, octaves: u32, gate: f64, rhythm: &str, seed: u64) -> Vec<N> {
    let mut rng = Rng::new(seed);
    let mask = rhythm_mask(rhythm);
    let mut idx: Vec<usize> = (0..notes.len()).collect();
    idx.sort_by(|a, b| notes[*a].start.partial_cmp(&notes[*b].start).unwrap_or(core::cmp::Ordering::Equal).then(notes[*a].key.cmp(&notes[*b].key)));
    let mut out = Vec::new();
    let mut i = 0;
    while i < idx.len() {
        let mut j = i + 1;
        while j < idx.len() && libm::fabs(notes[idx[j]].start - notes[idx[i]].start) < 0.03 {
            j += 1;
        }
        let group: Vec<N> = idx[i..j].iter().map(|k| notes[*k]).collect();
        if group.len() >= 2 {
            let start = group[0].start;
            let len = group.iter().map(|n| n.len).fold(0.0, f64::max).max(rate);
            let mut pitches: Vec<u8> = Vec::new();
            for o in 0..octaves.max(1) {
                for n in &group {
                    let k = n.key as u32 + 12 * o;
                    if k <= 127 && !pitches.contains(&(k as u8)) {
                        pitches.push(k as u8);
                    }
                }
            }
            pitches.sort();
            let vel = group.iter().map(|n| n.vel).fold(0.0, f32::max);
            for mut n in arp_notes(&pitches, start, len, rate, order, gate, &mask, &mut rng) {
                n.vel = (n.vel * vel / 0.9).clamp(0.1, 1.0);
                out.push(n);
            }
        } else {
            out.extend(group);
        }
        i = j;
    }
    out
}

// ── BPM (ling-music analysis.rs) ─────────────────────────────────────────────

const FRAME: usize = 1024;
const HOP: usize = 512;

/// In-place radix-2 FFT of (re, im).
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
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
        let ang = -2.0 * core::f64::consts::PI / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (wr, wi) = (libm::cos(ang * k as f64) as f32, libm::sin(ang * k as f64) as f32);
                let (a, b) = (start + k, start + k + len / 2);
                let (xr, xi) = (re[b] * wr - im[b] * wi, re[b] * wi + im[b] * wr);
                re[b] = re[a] - xr;
                im[b] = im[a] - xi;
                re[a] += xr;
                im[a] += xi;
            }
        }
        len <<= 1;
    }
}

fn onset_envelope(mono: &[f32]) -> Vec<f32> {
    if mono.len() < FRAME {
        return Vec::new();
    }
    let window: Vec<f32> = (0..FRAME).map(|i| 0.5 * (1.0 - libm::cosf(2.0 * core::f32::consts::PI * i as f32 / (FRAME - 1) as f32))).collect();
    let mut prev: Vec<f32> = Vec::new();
    let mut env: Vec<f32> = Vec::new();
    let (mut re, mut im) = (alloc::vec![0.0f32; FRAME], alloc::vec![0.0f32; FRAME]);
    let mut pos = 0;
    while pos + FRAME <= mono.len() {
        for i in 0..FRAME {
            re[i] = mono[pos + i] * window[i];
            im[i] = 0.0;
        }
        fft(&mut re, &mut im);
        let mag: Vec<f32> = (0..FRAME / 2).map(|b| libm::sqrtf(re[b] * re[b] + im[b] * im[b])).collect();
        let mut flux = 0.0f32;
        if !prev.is_empty() {
            for (c, p) in mag.iter().zip(prev.iter()) {
                if c > p {
                    flux += c - p;
                }
            }
        }
        env.push(flux);
        prev = mag;
        pos += HOP;
    }
    if env.len() < 2 {
        return Vec::new();
    }
    let max = env.iter().cloned().fold(0.0f32, f32::max);
    if max > 0.0 {
        for e in env.iter_mut() {
            *e /= max;
        }
    }
    env
}

pub(super) fn bpm(mono: &[f32], rate: u32) -> f32 {
    let env = onset_envelope(mono);
    if env.len() < 16 {
        return 0.0;
    }
    let env_rate = rate as f32 / HOP as f32;
    let (min_bpm, max_bpm) = (60.0f32, 200.0f32);
    let min_lag = libm::floorf(env_rate * 60.0 / max_bpm).max(1.0) as usize;
    let max_lag = libm::ceilf(env_rate * 60.0 / min_bpm).min((env.len() / 2) as f32) as usize;
    if max_lag <= min_lag {
        return 0.0;
    }
    let mean: f32 = env.iter().sum::<f32>() / env.len() as f32;
    let c: Vec<f32> = env.iter().map(|e| e - mean).collect();
    let (mut best_lag, mut best) = (min_lag, f32::MIN);
    for lag in min_lag..=max_lag {
        let mut ac = 0.0f32;
        for i in 0..(c.len() - lag) {
            ac += c[i] * c[i + lag];
        }
        let weight = 1.0 - 0.15 * (libm::fabsf(lag as f32 - (min_lag + max_lag) as f32 * 0.5) / (max_lag - min_lag) as f32);
        let score = ac * weight;
        if score > best {
            best = score;
            best_lag = lag;
        }
    }
    let mut b = 60.0 * env_rate / best_lag as f32;
    while b < 70.0 {
        b *= 2.0;
    }
    while b > 170.0 {
        b /= 2.0;
    }
    libm::roundf(b * 10.0) / 10.0
}

// ── builtins ─────────────────────────────────────────────────────────────────

pub(super) fn dispatch(name: &[u8], args: &[u64], d: &mut Daw) -> Option<u64> {
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).unwrap_or(0.0);
    let u = |i: usize| n(i).max(0.0) as usize;
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    Some(match name {
        b"daw_roll_arp" => {
            // (t, c, [indices] (empty = every note), rate, order, octaves, gate, rhythm, seed) -> notes made from the chords
            let want: Vec<usize> = match args.get(2) {
                Some(&v) if is_list(v) => list_items(v).iter().map(|&x| to_f64(x).max(0.0) as usize).collect(),
                _ => Vec::new(),
            };
            let rate = parse_rate(&s(3)).max(0.0625);
            let rate = if rate <= 0.0625 && n(3) > 0.0 { n(3).clamp(0.0625, 4.0) } else { rate };
            let (order, oct, gate, rhythm, seed) = (s(4).to_ascii_lowercase(), u(5).clamp(1, 4) as u32, n(6).clamp(0.05, 1.5), s(7), u(8) as u64);
            let Some(c) = d.project.tracks.get_mut(u(0)).and_then(|t| t.clips.get_mut(u(1))).filter(|c| !c.is_audio()) else { return Some(num(0.0)) };
            let dst = &mut c.midi_notes;
            let picked: Vec<usize> = if want.is_empty() { (0..dst.len()).collect() } else { want.into_iter().filter(|i| *i < dst.len()).collect() };
            let src: Vec<N> = picked.iter().map(|i| N { start: dst[*i].start, len: dst[*i].len, key: dst[*i].key, vel: dst[*i].vel }).collect();
            let out = arpeggiate(&src, rate, &order, oct, gate, &rhythm, seed);
            let mut drop: Vec<usize> = picked.clone();
            drop.sort_unstable();
            drop.dedup();
            for i in drop.into_iter().rev() {
                dst.remove(i);
            }
            for x in &out {
                dst.push(NoteDoc::new(x.start, x.len, x.key, x.vel));
            }
            d.dirty = true;
            d.rev += 1;
            num(out.len() as f64)
        },
        b"daw_clip_detect_bpm" => {
            let path = d.project.tracks.get(u(0)).and_then(|t| t.clips.get(u(1))).filter(|c| c.is_audio()).map(|c| c.audio_path.clone());
            match path.and_then(|p| super::rec::mono_of(d, &p)) {
                Some((mono, rate)) => num(bpm(&mono, rate) as f64),
                None => num(0.0),
            }
        },
        b"daw_inst_preset_has_fx" => num(growl_fx(&s(0)).len() as f64),
        b"daw_inst_preset_fx" => {
            // (track, preset) -> effects loaded: the Growl preset's chain replaces the track's effects (-1 = it has none)
            let chain = growl_fx(&s(1));
            if chain.is_empty() {
                return Some(num(-1.0));
            }
            let Some(t) = d.project.tracks.get_mut(u(0)) else { return Some(num(-1.0)) };
            t.effects.clear();
            for (fxn, ps) in chain {
                if let Some(mut doc) = fx::new_doc(&fxn) {
                    for (k, v) in ps {
                        if let Some(p) = doc.params.iter_mut().find(|p| p.0 == k) {
                            p.1 = v.clamp(0.0, 1.0);
                        }
                    }
                    t.effects.push(doc);
                }
            }
            let k = t.effects.len();
            d.dirty = true;
            d.rev += 1;
            num(k as f64)
        },
        _ => return None,
    })
}
