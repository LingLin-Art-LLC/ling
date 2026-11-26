//! `daw_inst_plot` / `daw_inst_info(t, "tables")` on LingOS: the pictures the instrument panels draw (an oscillator's
//! cycle, Nova's wavetable stack, a filter's response, an envelope, an LFO, Cinema's pitch envelope, Prism's scale).
//!
//! The desktop asks its real instruments (ling-daw dsp/*: `Instrument::plot`). LingOS plays every instrument with the
//! built-in voices of ling-osdaw, so these are drawn from the track's own knobs with the same formulas the desktop uses
//! where they are plain maths (Nova's warps, env times, the LFO phase) and generated shapes for the 48 factory tables
//! (by family: basic, harmonic, sync, vowel, growl, fm, metal, digital, dist, organ, phase). All values 0..1, like the PC.

use alloc::string::String;
use alloc::vec::Vec;
use libm::{cosf, expf, floorf, log10f, powf, sinf, sqrtf, tanhf};

use super::TrackDoc;

const TAU: f32 = core::f32::consts::TAU;

/// Nova's factory tables (ling-daw dsp/nova/tables.rs `TABLES`), in selector order: (name, family).
pub const TABLES: [(&str, &str); 48] = [
    ("Basic Shapes", "basic"),
    ("Saw Stack", "basic"),
    ("Pulse Width", "basic"),
    ("Mirror Saw", "basic"),
    ("Harmonic Sweep", "harmonic"),
    ("Odd Harmonics", "harmonic"),
    ("Even Harmonics", "harmonic"),
    ("Sine Partials", "harmonic"),
    ("Saw Octaves", "harmonic"),
    ("Hollow Saw", "harmonic"),
    ("Sync Saw", "sync"),
    ("Sync Sine", "sync"),
    ("Sync Square", "sync"),
    ("Vowel AEIOU", "vowel"),
    ("Talk Box", "vowel"),
    ("Yoi", "vowel"),
    ("Choir Ah", "vowel"),
    ("Growl Formant", "growl"),
    ("Riddim Square", "growl"),
    ("Neuro Grind", "growl"),
    ("Screech", "growl"),
    ("Wub Sweep", "growl"),
    ("FM 1:1", "fm"),
    ("FM 2:1", "fm"),
    ("FM 3:2", "fm"),
    ("FM Feedback", "fm"),
    ("FM Bass", "fm"),
    ("Metallic", "metal"),
    ("Bell Partials", "metal"),
    ("Glass", "metal"),
    ("Spectral Noise", "metal"),
    ("Chip Pulse", "digital"),
    ("4-bit Ramp", "digital"),
    ("Bitcrush Saw", "digital"),
    ("Digital Steps", "digital"),
    ("Fold Sine", "dist"),
    ("Fold Saw", "dist"),
    ("Tanh Drive", "dist"),
    ("Asym Drive", "dist"),
    ("Dirty Square", "dist"),
    ("Organ Drawbars", "organ"),
    ("Additive Pad", "organ"),
    ("Brass Swell", "organ"),
    ("Pluck Decay", "organ"),
    ("Phase Distort", "phase"),
    ("Reso Saw", "phase"),
    ("Comb Saw", "phase"),
    ("Formant Pulse", "phase"),
];

/// The selector's names, the user slot last (ling-daw `names_with_user`).
pub fn table_names() -> String {
    let mut s = String::new();
    for (n, _) in TABLES.iter() {
        s.push_str(n);
        s.push('\n');
    }
    s.push_str("User");
    s
}

fn fract(x: f32) -> f32 {
    x - floorf(x)
}
fn saw(x: f32) -> f32 {
    2.0 * fract(x) - 1.0
}
fn tri(x: f32) -> f32 {
    1.0 - 4.0 * (fract(x) - 0.5).abs()
}
fn sq(x: f32, w: f32) -> f32 {
    if fract(x) < w { 1.0 } else { -1.0 }
}
fn sine(x: f32) -> f32 {
    sinf(TAU * x)
}
fn fold(y: f32) -> f32 {
    // the wave folder (tables::fold): reflect at +-1
    let mut v = y;
    for _ in 0..8 {
        if v > 1.0 {
            v = 2.0 - v;
        } else if v < -1.0 {
            v = -2.0 - v;
        } else {
            break;
        }
    }
    v.clamp(-1.0, 1.0)
}
/// a sum of harmonics h (1-based) with amplitude f(h)
fn additive(x: f32, hmax: usize, f: impl Fn(usize) -> f32) -> f32 {
    let mut y = 0.0;
    for h in 1..=hmax {
        let a = f(h);
        if a != 0.0 {
            y += a * sinf(TAU * x * h as f32);
        }
    }
    y
}
/// a formant bump: harmonic h of a ~110 Hz note near centre frequency c (Hz)
fn formant(h: usize, c: f32) -> f32 {
    let f = 110.0 * h as f32;
    expf(-sqr((f - c) / (c * 0.25)))
}
fn sqr(x: f32) -> f32 {
    x * x
}
fn hash(i: u32) -> f32 {
    let mut x = i.wrapping_mul(2654435761) ^ 0x9e37_79b9;
    x ^= x >> 15;
    x = x.wrapping_mul(0x85eb_ca6b);
    x ^= x >> 13;
    (x & 0xffff) as f32 / 32767.5 - 1.0
}

/// One sample (-1..1, before normalising) of factory table `i` at frame position `pos` (0..1), phase `x` (cycles).
fn table_sample(i: usize, pos: f32, x: f32) -> f32 {
    let name = TABLES.get(i).map(|t| t.0).unwrap_or("Basic Shapes");
    let p = pos.clamp(0.0, 1.0);
    match name {
        "Basic Shapes" => {
            // sine -> triangle -> saw -> square across the table
            let s = p * 3.0;
            let (a, b, f) = if s < 1.0 {
                (sine(x), tri(x), s)
            } else if s < 2.0 {
                (tri(x), saw(x), s - 1.0)
            } else {
                (saw(x), sq(x, 0.5), s - 2.0)
            };
            a + (b - a) * f
        },
        "Saw Stack" => (0..(1 + (p * 6.0) as usize)).map(|k| saw(x * (1.0 + 0.004 * k as f32) + 0.13 * k as f32)).sum::<f32>(),
        "Pulse Width" => sq(x, 0.5 - p * 0.45),
        "Mirror Saw" => saw(x) * (1.0 - p) + tri(x) * p,
        "Harmonic Sweep" => additive(x, 2 + (p * 30.0) as usize, |h| 1.0 / h as f32),
        "Odd Harmonics" => additive(x, 1 + (p * 31.0) as usize, |h| if h % 2 == 1 { 1.0 / h as f32 } else { 0.0 }),
        "Even Harmonics" => additive(x, 2 + (p * 30.0) as usize, |h| if h == 1 || h % 2 == 0 { 1.0 / h as f32 } else { 0.0 }),
        "Sine Partials" => additive(x, 8, |h| if h == 1 { 1.0 } else { p * 0.6 / h as f32 * (1.0 + sinf(h as f32 * 1.7)) }),
        "Saw Octaves" => (0..5).map(|k| saw(x * powf(2.0, k as f32)) * powf(p, k as f32)).sum::<f32>(),
        "Hollow Saw" => saw(x) - saw(x + 0.5 * p) * 0.8,
        "Sync Saw" => saw(x * (1.0 + p * 7.0)),
        "Sync Sine" => sine(x * (1.0 + p * 7.0)),
        "Sync Square" => sq(x * (1.0 + p * 7.0), 0.5),
        "Vowel AEIOU" | "Talk Box" | "Yoi" | "Choir Ah" => {
            // formants that move through the vowels as the position moves
            let (f1, f2) = match name {
                "Choir Ah" => (700.0 + p * 100.0, 1150.0 + p * 150.0),
                "Yoi" => (300.0 + p * 400.0, 2200.0 - p * 1200.0),
                "Talk Box" => (400.0 + p * 500.0, 900.0 + p * 1300.0),
                _ => (750.0 - p * 450.0, 1100.0 + sinf(p * 6.0) * 900.0),
            };
            additive(x, 40, |h| (formant(h, f1) + 0.7 * formant(h, f2) + 0.05) / sqrtf(h as f32))
        },
        "Growl Formant" => tanhf(3.0 * (saw(x) + sine(x * (2.0 + p * 6.0)) * p)),
        "Riddim Square" => sq(x, 0.5) * (1.0 - p) + sq(x * 2.0, 0.25 + p * 0.5) * p,
        "Neuro Grind" => fold((saw(x) + sine(x * 3.0) * p) * (1.0 + p * 4.0)),
        "Screech" => sinf(TAU * x + (2.0 + p * 10.0) * sine(x * 3.0)),
        "Wub Sweep" => additive(x, 24, |h| expf(-sqr((h as f32 - (1.0 + p * 20.0)) / 3.0)) / sqrtf(h as f32)),
        "FM 1:1" => sinf(TAU * x + p * 5.0 * sine(x)),
        "FM 2:1" => sinf(TAU * x + p * 5.0 * sine(x * 2.0)),
        "FM 3:2" => sinf(TAU * x + p * 5.0 * sine(x * 1.5)),
        "FM Feedback" => {
            let mut y = 0.0;
            for _ in 0..4 {
                y = sinf(TAU * x + p * 1.6 * y);
            }
            y
        },
        "FM Bass" => sinf(TAU * x + p * 3.0 * sine(x) * (1.0 - fract(x))),
        "Metallic" => [1.0f32, 2.76, 5.4, 8.93].iter().enumerate().map(|(k, r)| sinf(TAU * x * floorf(r * (1.0 + p))) / (k + 1) as f32).sum(),
        "Bell Partials" => [1.0f32, 2.0, 3.0, 4.2, 5.4, 6.8].iter().enumerate().map(|(k, r)| sinf(TAU * x * floorf(r + p * k as f32)) * powf(0.7, k as f32)).sum(),
        "Glass" => additive(x, 16, |h| if h == 1 || h == 4 + (p * 8.0) as usize || h == 9 + (p * 6.0) as usize { 1.0 / sqrtf(h as f32) } else { 0.0 }),
        "Spectral Noise" => sine(x) * (1.0 - p) + hash((x * 256.0) as u32) * p,
        "Chip Pulse" => sq(x, [0.5f32, 0.25, 0.125, 0.75][((p * 3.99) as usize).min(3)]),
        "4-bit Ramp" => {
            let q = powf(2.0, 4.0 - p * 2.0);
            floorf(fract(x) * q) / q * 2.0 - 1.0
        },
        "Bitcrush Saw" => {
            let q = powf(2.0, 8.0 - p * 6.0);
            floorf(saw(x) * q) / q
        },
        "Digital Steps" => {
            let q = 2.0 + floorf(p * 14.0);
            floorf(sine(x) * q) / q
        },
        "Fold Sine" => fold(sine(x) * (1.0 + p * 5.0)),
        "Fold Saw" => fold(saw(x) * (1.0 + p * 5.0)),
        "Tanh Drive" => tanhf(sine(x) * (1.0 + p * 9.0)),
        "Asym Drive" => tanhf(sine(x) * (1.0 + p * 6.0) + p * 0.8),
        "Dirty Square" => tanhf((sq(x, 0.5) + saw(x * 3.0) * p * 0.5) * 2.0),
        "Organ Drawbars" => [0.5f32, 1.0, 1.5, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0]
            .iter()
            .enumerate()
            .map(|(k, r)| sinf(TAU * x * r * 2.0) * if (k as f32) <= 2.0 + p * 7.0 { 1.0 / (1.0 + k as f32 * 0.4) } else { 0.0 })
            .sum(),
        "Additive Pad" => additive(x, 12, |h| (0.5 + 0.5 * cosf(h as f32 * (1.0 + p * 2.0))) / h as f32),
        "Brass Swell" => additive(x, 20, |h| powf(1.0 / h as f32, 1.6 - p)),
        "Pluck Decay" => additive(x, 24, |h| expf(-(h as f32) * (1.0 - p * 0.9) * 0.4) / sqrtf(h as f32)),
        "Phase Distort" => {
            // Casio-style: the phase runs fast to a knee, then slow
            let k = 0.5 - p * 0.45;
            let ph = if fract(x) < k { fract(x) * 0.5 / k } else { 0.5 + (fract(x) - k) * 0.5 / (1.0 - k) };
            cosf(TAU * ph)
        },
        "Reso Saw" => (1.0 - fract(x)) * sinf(TAU * x * (1.0 + p * 12.0)),
        "Comb Saw" => saw(x) - saw(x + 0.05 + p * 0.4),
        "Formant Pulse" => sq(x, 0.5) * sinf(TAU * x * (2.0 + p * 8.0)).abs(),
        _ => sine(x),
    }
}

/// `n` points of a table frame, normalised to its peak, -1..1
fn table_frame(i: usize, pos: f32, n: usize) -> Vec<f32> {
    let v: Vec<f32> = (0..n).map(|k| table_sample(i, pos, k as f32 / n as f32)).collect();
    let pk = v.iter().fold(1e-6f32, |m, x| m.max(x.abs()));
    v.into_iter().map(|x| x / pk).collect()
}

fn get(t: &TrackDoc, name: &str, d: f32) -> f32 {
    t.param(name).unwrap_or(d)
}

/// the first knob whose name is one of `names` (instruments name the same thing differently)
fn any(t: &TrackDoc, names: &[&str], d: f32) -> f32 {
    names.iter().find_map(|n| t.param(n)).unwrap_or(d)
}

/// 0..1 selector knob -> index of `count` choices (ling-daw `sel`)
fn sel(v: f32, count: usize) -> usize {
    ((v.clamp(0.0, 1.0) * (count.max(2) - 1) as f32) + 0.5) as usize
}

const WARPS: usize = 15; // off sync bend+ bend- pwm asym mirror remap quantize crush fold fm pm am rm

/// Nova: the table index of oscillator `o` ("a" / "b" / "c")
fn nova_table(t: &TrackDoc, o: &str) -> usize {
    sel(get(t, &alloc::format!("{o} table"), 0.0), TABLES.len() + 1).min(TABLES.len() - 1)
}

/// Nova "wave <o>": the position's frame through the two warps (the same maths as ling-daw nova `plot`)
fn nova_wave(t: &TrackDoc, o: &str, n: usize) -> Vec<f32> {
    let ti = nova_table(t, o);
    let pos = get(t, &alloc::format!("{o} pos"), 0.0);
    let wt: [usize; 2] = [sel(get(t, &alloc::format!("{o} w1 type"), 0.0), WARPS), sel(get(t, &alloc::format!("{o} w2 type"), 0.0), WARPS)];
    let wa: [f32; 2] = [get(t, &alloc::format!("{o} w1 amt"), 0.0), get(t, &alloc::format!("{o} w2 amt"), 0.0)];
    let frame = table_frame(ti, pos, 512);
    let read = |ph: f32| frame[((fract(ph) * 512.0) as usize).min(511)];
    (0..n)
        .map(|k| {
            let mut p = k as f32 / n as f32;
            for w in 0..2 {
                let a = wa[w];
                match wt[w] {
                    1 => p = fract(p * (1.0 + a * 7.0)),
                    2 => {
                        let y = 2.0 * p - 1.0;
                        p = (y.signum() * powf(y.abs(), 1.0 + a * 3.0) + 1.0) * 0.5;
                    },
                    3 => {
                        let y = 2.0 * p - 1.0;
                        p = (y.signum() * powf(y.abs(), 1.0 / (1.0 + a * 3.0)) + 1.0) * 0.5;
                    },
                    4 => {
                        let wd = 1.0 - a * 0.95;
                        p = if p < wd { p / wd } else { -1.0 };
                    },
                    5 => {
                        let m = 0.5 - a * 0.45;
                        p = if p < m { p * 0.5 / m } else { 0.5 + (p - m) * 0.5 / (1.0 - m) };
                    },
                    6 => {
                        let mi = 1.0 - (2.0 * p - 1.0).abs();
                        p += (mi * 0.5 - p) * a;
                    },
                    8 => {
                        let s = powf(2.0, 2.0 + (1.0 - a) * 9.0);
                        p = floorf(p * s) / s;
                    },
                    _ => {},
                }
                if p < 0.0 {
                    break;
                }
            }
            if p < 0.0 {
                return 0.5;
            }
            let mut y = read(p);
            for w in 0..2 {
                let a = wa[w];
                match wt[w] {
                    10 => y = fold(y * (1.0 + a * 6.0)),
                    9 => {
                        let q = powf(2.0, 1.0 + (1.0 - a) * 11.0);
                        y = libm::roundf(y * q) / q;
                    },
                    _ => {},
                }
            }
            (0.5 + 0.5 * y).clamp(0.0, 1.0)
        })
        .collect()
}

/// Nova "stack <o>": 8 frames across the table, n/8 points each
fn nova_stack(t: &TrackDoc, o: &str, n: usize) -> Vec<f32> {
    let ti = nova_table(t, o);
    let per = (n / 8).max(2);
    let mut out = Vec::with_capacity(per * 8);
    for f in 0..8 {
        for y in table_frame(ti, f as f32 / 7.0, per) {
            out.push((0.5 + 0.5 * y).clamp(0.0, 1.0));
        }
    }
    out
}

/// the cycle of a non-Nova instrument's oscillator: its kind's shape, morphed by the "wave"-like knob
fn plain_wave(t: &TrackDoc, second: bool, n: usize) -> Vec<f32> {
    let kind = t.instrument.as_str();
    let w = if second { any(t, &["wave b", "b wave", "osc2 wave", "wave2"], 0.5) } else { any(t, &["wave", "a wave", "osc1 wave", "shape", "wave a"], 0.0) };
    let duty = any(t, &["duty", "pulse width", "pw", "width"], 0.5);
    let v: Vec<f32> = (0..n)
        .map(|k| {
            let x = k as f32 / n as f32;
            match kind {
                // the chip: pulse with its duty, triangle at the top of the wave knob
                "chip" => {
                    if w > 0.66 {
                        tri(x)
                    } else {
                        sq(x, 0.1 + duty.clamp(0.0, 1.0) * 0.8)
                    }
                },
                "growl" => fold((saw(x) + sine(x * 2.0) * w) * (1.0 + w * 3.0)),
                _ => {
                    // synth / prism: sine -> saw -> square across the knob
                    let s = w.clamp(0.0, 1.0) * 2.0;
                    if s < 1.0 { sine(x) + (saw(x) - sine(x)) * s } else { saw(x) + (sq(x, 0.5) - saw(x)) * (s - 1.0) }
                },
            }
        })
        .collect();
    let pk = v.iter().fold(1e-6f32, |m, x| m.max(x.abs()));
    v.into_iter().map(|y| (0.5 + 0.5 * y / pk).clamp(0.0, 1.0)).collect()
}

/// a low-pass response (12 dB, resonant) over 20 Hz..20 kHz on a log axis, -36..+12 dB -> 0..1
fn filter_curve(t: &TrackDoc, k: usize, n: usize) -> Vec<f32> {
    let fx = alloc::format!("f{k}");
    let cut = any(t, &[&alloc::format!("{fx} cut"), "cutoff", "filter cutoff", "cut", "filter"], 0.6);
    let res = any(t, &[&alloc::format!("{fx} res"), "reso", "resonance", "res", "q"], 0.2);
    let on = t.param(&alloc::format!("{fx} on")).unwrap_or(1.0) > 0.5;
    let fc = 20.0 * powf(1000.0, cut.clamp(0.0, 1.0));
    let q = 0.5 + res.clamp(0.0, 1.0) * 9.5;
    (0..n)
        .map(|i| {
            let f = 20.0 * powf(1000.0, i as f32 / (n - 1).max(1) as f32);
            let db = if on {
                let r = f / fc;
                let mag = 1.0 / sqrtf(sqr(1.0 - r * r) + sqr(r / q));
                20.0 * log10f(mag.max(1e-5))
            } else {
                0.0
            };
            ((db + 36.0) / 48.0).clamp(0.0, 1.0)
        })
        .collect()
}

/// ADSR (env times like the desktop's: 0.5 ms * 30000^v), sustain held then released over the last quarter
fn env_curve(t: &TrackDoc, k: usize, n: usize) -> Vec<f32> {
    let e = alloc::format!("e{k}");
    let a = any(t, &[&alloc::format!("{e} a"), "attack", "amp a", "a"], 0.1);
    let d = any(t, &[&alloc::format!("{e} d"), "decay", "amp d", "d"], 0.4);
    let s = any(t, &[&alloc::format!("{e} s"), "sustain", "amp s", "s"], 0.7);
    let r = any(t, &[&alloc::format!("{e} r"), "release", "amp r", "r"], 0.3);
    let time = |v: f32| 0.0005 * powf(30000.0, v.clamp(0.0, 1.0));
    let (ta, td, tr) = (time(a), time(d), time(r));
    let hold = (ta + td) * 1.3 + 0.05;
    let total = hold / 0.75;
    (0..n)
        .map(|i| {
            let tt = i as f32 / n as f32 * total;
            let y = if tt < ta {
                tt / ta.max(1e-4)
            } else if tt < ta + td {
                1.0 - (1.0 - s) * (1.0 - expf(-4.0 * (tt - ta) / td.max(1e-4))) / (1.0 - expf(-4.0))
            } else if tt < hold {
                s
            } else {
                // the release drawn over the last quarter at its own speed
                let rt = (tt - hold) / (total - hold).max(1e-4) * tr;
                s * expf(-4.0 * rt / tr.max(1e-4))
            };
            y.clamp(0.0, 1.0)
        })
        .collect()
}

/// an LFO cycle: its shape knob (sine, triangle, saw up, saw down, square, ...) from its phase
fn lfo_curve(t: &TrackDoc, k: usize, n: usize) -> Vec<f32> {
    let l = alloc::format!("l{k}");
    let shape = sel(any(t, &[&alloc::format!("{l} shape"), "lfo shape"], 0.0), 8);
    let ph0 = any(t, &[&alloc::format!("{l} phase"), "lfo phase"], 0.0);
    (0..n)
        .map(|i| {
            let x = fract(i as f32 / n as f32 + ph0);
            let y = match shape {
                1 => tri(x),
                2 => saw(x),
                3 => -saw(x),
                4 => sq(x, 0.5),
                5 => hash((x * 8.0) as u32 + 1),
                6 => sinf(TAU * x) * sinf(TAU * x).abs(),
                7 => 1.0 - 2.0 * expf(-5.0 * x),
                _ => sinf(TAU * x),
            };
            (0.5 + 0.5 * y).clamp(0.0, 1.0)
        })
        .collect()
}

/// Cinema's pitch envelope: a drop or rise of "pitch env" depth over "pitch time"
fn pitch_curve(t: &TrackDoc, n: usize) -> Vec<f32> {
    let depth = any(t, &["pitch env", "pitch amt", "pitch depth", "pitch"], 0.5);
    let time = any(t, &["pitch time", "pitch decay", "pitch t"], 0.3);
    (0..n)
        .map(|i| {
            let x = i as f32 / n as f32;
            (0.5 + (depth - 0.5) * expf(-x / (0.05 + time * 0.5))).clamp(0.0, 1.0)
        })
        .collect()
}

/// Prism's scale: the scale knob's steps as a staircase over two octaves
fn scale_curve(t: &TrackDoc, n: usize) -> Vec<f32> {
    let scales: [&[u8]; 6] = [&[0, 2, 4, 5, 7, 9, 11], &[0, 2, 3, 5, 7, 8, 10], &[0, 2, 4, 7, 9], &[0, 3, 5, 7, 10], &[0, 2, 3, 5, 7, 9, 10], &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]];
    let sc = scales[sel(any(t, &["scale", "scale type"], 0.0), scales.len()).min(scales.len() - 1)];
    let steps = sc.len() * 2;
    (0..n)
        .map(|i| {
            let k = (i * steps / n.max(1)).min(steps - 1);
            let semis = (k / sc.len()) as f32 * 12.0 + sc[k % sc.len()] as f32;
            (semis / 24.0).clamp(0.0, 1.0)
        })
        .collect()
}

/// `daw_inst_plot(track, what, points)`
pub fn plot(t: &TrackDoc, what: &str, n: usize) -> Vec<f32> {
    if t.is_audio() {
        return Vec::new();
    }
    let n = n.clamp(2, 4096);
    let nova = t.instrument == "nova";
    let tail = what.rsplit(' ').next().unwrap_or("");
    let osc = if matches!(tail, "a" | "b" | "c") { tail } else { "a" };
    let num = |s: &str, d: usize| s.trim().parse::<usize>().unwrap_or(d);
    if what.starts_with("stack") {
        return if nova { nova_stack(t, osc, n) } else { Vec::new() };
    }
    if what.starts_with("wave") {
        if nova {
            return nova_wave(t, osc, n);
        }
        return plain_wave(t, what == "wave_b" || what.ends_with(" b"), n);
    }
    if let Some(k) = what.strip_prefix("filter") {
        return filter_curve(t, num(k, 1).clamp(1, 2), n);
    }
    if let Some(k) = what.strip_prefix("env") {
        return env_curve(t, num(k, 1).clamp(1, 4), n);
    }
    if let Some(k) = what.strip_prefix("lfo") {
        return lfo_curve(t, num(k, 1).clamp(1, 4), n);
    }
    if what.starts_with("pitch") {
        return pitch_curve(t, n);
    }
    if what.starts_with("scale") {
        return scale_curve(t, n);
    }
    if what.starts_with("pos") {
        return alloc::vec![get(t, &alloc::format!("{osc} pos"), 0.0)];
    }
    Vec::new()
}
