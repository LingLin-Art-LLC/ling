//! Make Medley on LingOS: `daw_song_medley_scan`, `daw_song_medley_build`, `daw_song_medley_decode`, `daw_song_medley_mix` --
//! the folder-of-MIDI medley / mashup of LingStationX's Medley window (ling/medley.ling).
//!
//! A port of the desktop engine (ling-daw medley.rs, with the pieces it stands on: midi_import.rs's part split, articulate.rs's
//! phrase_map, theory.rs's key finding, register.rs, the story arcs of emotion.rs / songgen.rs), no_std + libm. The phrases,
//! the common key, the smart / random / cipher order, the HMAC-SHA-256 keyed permutation, the message encoding, the key path,
//! the layered mashup (energy-driven layers, chord-aware fitting, clash swaps, call and response, register guard and octave
//! allocation, breakdown, loop rolls) and the clip names (the spine's `file #n · id`, so Decode reads either side's medleys)
//! follow the desktop code line by line.
//!
//! Not here (they need the rest of the desktop engine): the sound design per section, Sound Recipes, the feel / emotion
//! plan, vocal chops and acapellas, the Ling Cinema transitions, fills / crashes and Mix AI. The instruments are the app's
//! General MIDI patches chosen by the layer's role; the build's sentence says what was left out.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::{apply_patch, ClipDoc, Daw, NoteDoc, TrackDoc};
use crate::lingrt::{list_from, mk_str, num, to_f64, to_string};

// ── float maths (no std) ─────────────────────────────────────────────────────

trait F {
    fn sin_(self) -> Self;
    fn pow_(self, e: Self) -> Self;
    fn log2_(self) -> Self;
    fn floor_(self) -> Self;
    fn ceil_(self) -> Self;
    fn round_(self) -> Self;
}
impl F for f64 {
    fn sin_(self) -> f64 {
        libm::sin(self)
    }
    fn pow_(self, e: f64) -> f64 {
        libm::pow(self, e)
    }
    fn log2_(self) -> f64 {
        libm::log2(self)
    }
    fn floor_(self) -> f64 {
        libm::floor(self)
    }
    fn ceil_(self) -> f64 {
        libm::ceil(self)
    }
    fn round_(self) -> f64 {
        libm::round(self)
    }
}
impl F for f32 {
    fn sin_(self) -> f32 {
        libm::sinf(self)
    }
    fn pow_(self, e: f32) -> f32 {
        libm::powf(self, e)
    }
    fn log2_(self) -> f32 {
        libm::log2f(self)
    }
    fn floor_(self) -> f32 {
        libm::floorf(self)
    }
    fn ceil_(self) -> f32 {
        libm::ceilf(self)
    }
    fn round_(self) -> f32 {
        libm::roundf(self)
    }
}
const PI: f64 = core::f64::consts::PI;

/// a note (model.rs Note: the fields the medley uses)
#[derive(Clone, Debug)]
pub struct Note {
    pub start: f64,
    pub len: f64,
    pub key: u8,
    pub vel: f32,
    pub pan: f32,
}
impl Note {
    fn new(start: f64, len: f64, key: u8, vel: f32) -> Note {
        Note { start, len, key, vel, pan: 0.0 }
    }
}

// ── a MIDI file as the desktop's midi_import::parse splits it ───────────────────

pub struct ImportedTrack {
    pub program: Option<u8>,
    pub channel: u8,
    pub notes: Vec<Note>,
    pub percussive: bool,
}
pub struct ImportedMidi {
    pub tracks: Vec<ImportedTrack>,
    pub bpm: Option<f64>,
    pub length_beats: f64,
}

fn vlq(b: &[u8], i: &mut usize, end: usize) -> Option<u32> {
    let mut v = 0u32;
    for _ in 0..4 {
        if *i >= end {
            return None;
        }
        let x = b[*i];
        *i += 1;
        v = (v << 7) | (x & 0x7f) as u32;
        if x & 0x80 == 0 {
            return Some(v);
        }
    }
    Some(v)
}

/// Parts per (channel, program, bank) of every track, notes pushed when they end (stable-sorted by start after), the sustain
/// pedal holding notes, first-in first-out note-offs per key -- midi_import.rs `parse` (the expression lanes left out).
pub fn parse_midi(b: &[u8]) -> Result<ImportedMidi, String> {
    if b.len() < 14 || &b[0..4] != b"MThd" {
        return Err(String::from("not a MIDI file"));
    }
    let be16 = |o: usize| u16::from_be_bytes([b[o], b[o + 1]]);
    let be32 = |o: usize| u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let hlen = be32(4) as usize;
    let ntracks = be16(10) as usize;
    let div = be16(12);
    let tpqn = if div & 0x8000 != 0 {
        let fps = (-((div >> 8) as i8) as i32).max(1) as f64;
        (fps * (div & 0xff) as f64 / 2.0).max(1.0)
    } else {
        div.max(1) as f64
    };
    let mut tempos: Vec<(f64, f64)> = Vec::new();
    let mut out: Vec<ImportedTrack> = Vec::new();
    let mut off = 8 + hlen;
    for _ in 0..ntracks {
        if off + 8 > b.len() || &b[off..off + 4] != b"MTrk" {
            break;
        }
        let len = be32(off + 4) as usize;
        let (start, end) = (off + 8, (off + 8 + len).min(b.len()));
        off = end;
        struct Seg {
            chan: u8,
            program: Option<u8>,
            bank: u16,
            notes: Vec<Note>,
        }
        // channel state: program, bank, pedal, pending note-ons per key (tick, vel, seg), held by the pedal
        struct St {
            chan: u8,
            program: u8,
            bank: u16,
            pedal: bool,
            pending: Vec<(u8, Vec<(u64, u8, usize)>)>,
            held: Vec<(u8, u64, u8, usize)>,
        }
        let mut segs: Vec<Seg> = Vec::new();
        let mut states: Vec<St> = Vec::new();
        let mut i = start;
        let mut tick = 0u64;
        let mut status = 0u8;
        while i < end {
            let Some(dt) = vlq(b, &mut i, end) else { break };
            tick += dt as u64;
            let beat = tick as f64 / tpqn;
            let Some(&first) = b.get(i) else { break };
            let st = if first & 0x80 != 0 {
                i += 1;
                if first < 0xf0 {
                    status = first;
                }
                first
            } else {
                status
            };
            if st == 0 {
                i += 1;
                continue;
            }
            match st {
                0xff => {
                    let Some(&ty) = b.get(i) else { break };
                    i += 1;
                    let Some(l) = vlq(b, &mut i, end) else { break };
                    let l = l as usize;
                    let data = &b[i.min(end)..(i + l).min(end)];
                    if ty == 0x51 && data.len() == 3 {
                        let us = ((data[0] as u32) << 16) | ((data[1] as u32) << 8) | data[2] as u32;
                        if us > 0 {
                            tempos.push((beat, 60_000_000.0 / us as f64));
                        }
                    }
                    i += l;
                },
                0xf0 | 0xf7 => {
                    let Some(l) = vlq(b, &mut i, end) else { break };
                    i += l as usize;
                },
                0xf2 => i += 2,
                0xf1 | 0xf3 => i += 1,
                0xf4..=0xfe => {},
                _ => {
                    let c = st & 0x0f;
                    let kind = st & 0xf0;
                    let nb = if kind == 0xc0 || kind == 0xd0 { 1 } else { 2 };
                    if i + nb > end {
                        break;
                    }
                    let (d1, d2) = (b[i], if nb > 1 { b[i + 1] } else { 0 });
                    i += nb;
                    let si = match states.iter().position(|s| s.chan == c) {
                        Some(x) => x,
                        None => {
                            states.push(St { chan: c, program: 0, bank: 0, pedal: false, pending: Vec::new(), held: Vec::new() });
                            states.len() - 1
                        },
                    };
                    let seg_of = |segs: &mut Vec<Seg>, program: u8, bank: u16| -> usize {
                        let prog = if c == 9 { None } else { Some(program) };
                        match segs.iter().position(|s| s.chan == c && s.program == prog && s.bank == bank) {
                            Some(x) => x,
                            None => {
                                segs.push(Seg { chan: c, program: prog, bank, notes: Vec::new() });
                                segs.len() - 1
                            },
                        }
                    };
                    match kind {
                        0xc0 => states[si].program = d1,
                        0xb0 => match d1 {
                            0 => states[si].bank = d2 as u16,
                            32 => {
                                if states[si].bank == 0 {
                                    states[si].bank = d2 as u16;
                                }
                            },
                            64 => {
                                let down = d2 >= 64;
                                if states[si].pedal && !down {
                                    for (key, s0, vel, seg) in core::mem::take(&mut states[si].held) {
                                        let len = (tick.saturating_sub(s0) as f64 / tpqn).max(0.05);
                                        segs[seg].notes.push(Note::new(s0 as f64 / tpqn, len, key, vel as f32 / 127.0));
                                    }
                                }
                                states[si].pedal = down;
                            },
                            _ => {},
                        },
                        0x90 if d2 > 0 => {
                            let (p, bk) = (states[si].program, states[si].bank);
                            let seg = seg_of(&mut segs, p, bk);
                            match states[si].pending.iter_mut().find(|e| e.0 == d1) {
                                Some(e) => e.1.push((tick, d2, seg)),
                                None => states[si].pending.push((d1, vec![(tick, d2, seg)])),
                            }
                        },
                        0x80 | 0x90 => {
                            let started = states[si].pending.iter_mut().find(|e| e.0 == d1).and_then(|e| if e.1.is_empty() { None } else { Some(e.1.remove(0)) });
                            if let Some((s0, vel, seg)) = started {
                                if states[si].pedal && c != 9 {
                                    states[si].held.push((d1, s0, vel, seg));
                                } else {
                                    let len = (tick.saturating_sub(s0) as f64 / tpqn).max(0.05);
                                    segs[seg].notes.push(Note::new(s0 as f64 / tpqn, len, d1, vel as f32 / 127.0));
                                }
                            }
                        },
                        _ => {},
                    }
                },
            }
        }
        // notes still open at the end of the track
        for st in states.iter_mut() {
            let mut open: Vec<(u8, u64, u8, usize)> = st.pending.iter().flat_map(|(k, v)| v.iter().map(move |(s, vel, seg)| (*k, *s, *vel, *seg))).collect();
            open.extend(core::mem::take(&mut st.held));
            for (key, s0, vel, seg) in open {
                let len = (tick.saturating_sub(s0) as f64 / tpqn).max(0.05);
                segs[seg].notes.push(Note::new(s0 as f64 / tpqn, len, key, vel as f32 / 127.0));
            }
        }
        for seg in segs {
            if seg.notes.is_empty() {
                continue;
            }
            let mut notes = seg.notes;
            notes.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(core::cmp::Ordering::Equal));
            out.push(ImportedTrack { program: seg.program, channel: seg.chan, notes, percussive: false });
        }
    }
    let length_beats = out.iter().flat_map(|t| t.notes.iter()).map(|n| n.start + n.len).fold(0.0, f64::max);
    tempos.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
    let mut map: Vec<(f64, f64)> = Vec::new();
    for (bt, v) in tempos {
        if let Some(last) = map.last_mut() {
            if (last.0 - bt).abs() < 1e-6 {
                last.1 = v;
                continue;
            }
            if (last.1 - v).abs() < 0.01 {
                continue;
            }
        }
        map.push((bt, v));
    }
    Ok(ImportedMidi { tracks: out, bpm: map.first().map(|f| f.1), length_beats })
}

// ── keys (theory.rs) ─────────────────────────────────────────────────────────

const SHARP: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
const FLAT: [&str; 12] = ["C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B"];
const MAJOR: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
const MINOR: [u8; 7] = [0, 2, 3, 5, 7, 8, 10];
const PROF_MAJOR: [f64; 12] = [6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88];
const PROF_MINOR: [f64; 12] = [6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17];

#[derive(Clone, Copy)]
pub struct Key {
    pub tonic: u8,
    pub minor: bool,
}
impl Key {
    pub fn new(tonic: u8, minor: bool) -> Key {
        Key { tonic: tonic % 12, minor }
    }
    fn flats(&self) -> bool {
        if self.minor { matches!(self.tonic, 2 | 5 | 7 | 10 | 3 | 0) } else { matches!(self.tonic, 5 | 10 | 3 | 8 | 1 | 6) }
    }
    pub fn name(&self) -> String {
        format!("{} {}", if self.flats() { FLAT[self.tonic as usize] } else { SHARP[self.tonic as usize] }, if self.minor { "minor" } else { "major" })
    }
    pub fn scale(&self) -> [u8; 7] {
        let s = if self.minor { MINOR } else { MAJOR };
        let mut out = [0u8; 7];
        for (i, d) in s.iter().enumerate() {
            out[i] = (self.tonic + d) % 12;
        }
        out
    }
    pub fn in_scale(&self, pc: u8) -> bool {
        self.scale().contains(&(pc % 12))
    }
}

fn corr(a: &[f64; 12], b: &[f64; 12]) -> f64 {
    let ma = a.iter().sum::<f64>() / 12.0;
    let mb = b.iter().sum::<f64>() / 12.0;
    let (mut nm, mut da, mut db) = (0.0, 0.0, 0.0);
    for i in 0..12 {
        nm += (a[i] - ma) * (b[i] - mb);
        da += (a[i] - ma) * (a[i] - ma);
        db += (b[i] - mb) * (b[i] - mb);
    }
    if da <= 1e-12 || db <= 1e-12 { 0.0 } else { nm / libm::sqrt(da * db) }
}

fn detect_key_hist(h: &[f64; 12]) -> Key {
    let mut s = [0.0; 24];
    for t in 0..12 {
        let mut rot = [0.0; 12];
        for i in 0..12 {
            rot[(i + t) % 12] = PROF_MAJOR[i];
        }
        s[t] = corr(h, &rot);
        let mut rot = [0.0; 12];
        for i in 0..12 {
            rot[(i + t) % 12] = PROF_MINOR[i];
        }
        s[12 + t] = corr(h, &rot);
    }
    let mut best = 0;
    for i in 1..24 {
        if s[i] > s[best] {
            best = i;
        }
    }
    Key::new((best % 12) as u8, best >= 12)
}

// ── phrases (articulate.rs phrase_map) ──────────────────────────────────────

struct PhraseSpan {
    b0: f64,
    b1: f64,
}

const PHRASE_BARS: [f64; 5] = [4.0, 8.0, 16.0, 32.0, 64.0];

fn print(notes: &[(f64, f64)], b0: f64, b1: f64) -> Vec<u32> {
    let mut v: Vec<u32> = notes.iter().filter(|n| n.0 >= b0 - 1e-6 && n.0 < b1 - 1e-6).map(|n| ((n.0 - b0) * 4.0).round_().max(0.0) as u32).collect();
    v.sort_unstable();
    v.dedup();
    v
}

fn similar(a: &[u32], b: &[u32]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let (mut i, mut j, mut both) = (0, 0, 0usize);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            both += 1;
            i += 1;
            j += 1;
        } else if a[i] < b[j] {
            i += 1;
        } else {
            j += 1;
        }
    }
    both as f64 / (a.len() + b.len() - both) as f64
}

fn boundary(notes: &[(f64, f64)], t: f64) -> f64 {
    if notes.is_empty() {
        return 0.5;
    }
    let (mut held, mut onset, mut last_end) = (false, false, f64::MIN);
    for &(on, len) in notes {
        if on > t + 0.3 {
            break;
        }
        if on < t - 0.05 {
            last_end = last_end.max(on + len);
            if on + len > t + 0.05 {
                held = true;
            }
        }
        if (on - t).abs() <= 0.25 {
            onset = true;
        }
    }
    let on = if onset { 0.2 } else { 0.0 };
    if held {
        return 0.1 + on;
    }
    let rest = if last_end == f64::MIN { 1.0 } else { ((t - last_end) / 2.0).clamp(0.0, 1.0) };
    (0.4 + 0.4 * rest + on).min(1.0)
}

fn auto_len(notes: &[(f64, f64)], b0: f64, b1: f64) -> f64 {
    let len = b1 - b0;
    let mut best = (f64::MIN, 16.0f64.min(len));
    for bars in PHRASE_BARS {
        let p = bars * 4.0;
        if p > len + 1e-9 && bars > 4.0 {
            continue;
        }
        let n = (len / p).floor_().max(1.0);
        let (mut q, mut c) = (0.0, 0usize);
        let mut t = b0 + p;
        while t < b1 - 1e-9 {
            q += boundary(notes, t);
            c += 1;
            t += p;
        }
        let q = if c == 0 { 0.5 } else { q / c as f64 };
        let mut prints: Vec<Vec<u32>> = vec![];
        let mut t = b0;
        while t + p <= b1 + 1e-9 && prints.len() < 32 {
            prints.push(print(notes, t, t + p));
            t += p;
        }
        let rep = if prints.len() < 2 { 0.0 } else { (1..prints.len()).filter(|&k| (0..k).any(|j| similar(&prints[k], &prints[j]) >= 0.8)).count() as f64 / (prints.len() - 1) as f64 };
        let score = q + 0.4 * rep - 0.08 * (bars / 8.0).log2_().abs() - if n > 16.0 { 0.3 } else { 0.0 };
        if score > best.0 + 1e-9 {
            best = (score, p.min(len));
        }
    }
    best.1
}

/// the phrase cuts of b0..b1 (the letters and energies of articulate.rs are not needed by the medley)
fn phrase_map(b0: f64, b1: f64, notes: &[(f64, f64)], bars: f64) -> Vec<PhraseSpan> {
    let len = b1 - b0;
    if !(len > 0.5) {
        return vec![];
    }
    let notes: Vec<(f64, f64)> = notes.iter().copied().filter(|n| n.0 >= b0 - 1e-6 && n.0 < b1).collect();
    let plen = if bars >= 1.0 { (bars.round_() * 4.0).clamp(4.0, 256.0) } else { auto_len(&notes, b0, b1) }.min(len.max(1.0));
    let mut cuts = vec![b0];
    let mut t = b0 + plen;
    while t < b1 - 3.99 && cuts.len() < 512 {
        cuts.push(t);
        t += plen;
    }
    cuts.push(b1);
    (0..cuts.len() - 1).map(|k| PhraseSpan { b0: cuts[k], b1: cuts[k + 1] }).collect()
}

// ── registers (register.rs) ─────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Voice {
    Bass,
    Pad,
    Chords,
    Lead,
    Arp,
}
impl Voice {
    fn home(self) -> (f64, f64) {
        match self {
            Voice::Bass => (33.0, 48.0),
            Voice::Pad => (50.0, 70.0),
            Voice::Chords => (52.0, 72.0),
            Voice::Lead => (60.0, 82.0),
            Voice::Arp => (64.0, 90.0),
        }
    }
    fn rank(self) -> i32 {
        match self {
            Voice::Bass => 0,
            Voice::Pad | Voice::Chords => 1,
            Voice::Lead => 2,
            Voice::Arp => 3,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Reg {
    pub lo: u8,
    pub hi: u8,
    pub median: f64,
    pub centre: f64,
    pub octaves: [f32; 11],
    pub rate: f64,
    pub avg_len: f64,
    pub n: usize,
}
impl Reg {
    fn moved(&self, oct: i32) -> Reg {
        let d = oct * 12;
        let mut o = [0f32; 11];
        for (i, v) in self.octaves.iter().enumerate() {
            let j = i as i32 + oct;
            if (0..11).contains(&j) {
                o[j as usize] += v;
            }
        }
        Reg { lo: (self.lo as i32 + d).clamp(0, 127) as u8, hi: (self.hi as i32 + d).clamp(0, 127) as u8, median: self.median + d as f64, centre: self.centre + d as f64, octaves: o, ..self.clone() }
    }
    fn busy(&self) -> f64 {
        let r = ((self.rate - 1.5) / 2.5).clamp(0.0, 1.0);
        let l = ((0.75 - self.avg_len) / 0.5).clamp(0.0, 1.0);
        (r * 0.7 + l * 0.3).clamp(0.0, 1.0)
    }
}

fn analyze(notes: &[Note], beats: f64) -> Option<Reg> {
    if notes.is_empty() {
        return None;
    }
    let mut keys: Vec<u8> = notes.iter().map(|n| n.key).collect();
    keys.sort();
    let median = if keys.len() % 2 == 1 { keys[keys.len() / 2] as f64 } else { (keys[keys.len() / 2 - 1] as f64 + keys[keys.len() / 2] as f64) / 2.0 };
    let mut o = [0f32; 11];
    let (mut wsum, mut w) = (0.0, 0.0);
    for n in notes {
        let d = n.len.max(0.05);
        wsum += n.key as f64 * d;
        w += d;
        o[(n.key / 12).min(10) as usize] += d as f32;
    }
    for v in o.iter_mut() {
        *v /= w.max(1e-9) as f32;
    }
    let mut onsets: Vec<i64> = notes.iter().map(|n| (n.start * 48.0).round_() as i64).collect();
    onsets.sort();
    onsets.dedup();
    Some(Reg { lo: keys[0], hi: *keys.last().unwrap(), median, centre: wsum / w.max(1e-9), octaves: o, rate: onsets.len() as f64 / beats.max(1.0), avg_len: w / notes.len() as f64, n: notes.len() })
}

fn brightness(name: &str) -> f32 {
    let r = name.to_lowercase();
    let has = |w: &[&str]| w.iter().any(|x| r.contains(x));
    if has(&["sub", "sine", "808"]) {
        0.1
    } else if has(&["growl", "wobble", "reese", "saw", "screech", "laser", "tearout", "riddim", "fm ", "neuro", "distort"]) {
        0.9
    } else if has(&["square", "chip", "bell", "brass", "organ", "stab"]) {
        0.7
    } else if has(&["rolling", "pluck", "keys", "piano", "lead", "arp"]) {
        0.5
    } else if has(&["pad", "string", "warm", "ambient", "choir", "flute"]) {
        0.3
    } else {
        0.5
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    floor: f64,
    ceil: f64,
    lo_min: f64,
    hi_max: f64,
}

fn limits(v: Voice, bright: f32, busy: f64) -> Limits {
    let b = bright.clamp(0.0, 1.0) as f64;
    let busy = busy.clamp(0.0, 1.0);
    match v {
        Voice::Bass => {
            let floor = 26.0 + 7.0 * b + 5.0 * busy;
            Limits { floor, ceil: 55.0, lo_min: floor - 7.0, hi_max: 74.0 }
        },
        Voice::Pad | Voice::Chords => Limits { floor: 45.0 + 2.0 * busy, ceil: 84.0 - 6.0 * b, lo_min: 33.0, hi_max: 100.0 - 6.0 * b },
        Voice::Lead => Limits { floor: 50.0, ceil: 96.0 - 10.0 * b, lo_min: 38.0, hi_max: 105.0 - 8.0 * b },
        Voice::Arp => Limits { floor: 52.0, ceil: 100.0 - 10.0 * b, lo_min: 40.0, hi_max: 108.0 - 8.0 * b },
    }
}

fn violation(r: &Reg, l: &Limits) -> f64 {
    let m = (l.floor - r.median).max(0.0) + (r.median - l.ceil).max(0.0);
    let e = (l.lo_min - r.lo as f64).max(0.0) + (r.hi as f64 - l.hi_max).max(0.0);
    2.0 * m + e
}

fn reason(v: Voice, oct: i32) -> &'static str {
    match (v, oct.signum()) {
        (Voice::Bass, 1) => "was muddy",
        (Voice::Bass, -1) => "too high for a bass",
        (_, -1) => "was shrill",
        (Voice::Pad | Voice::Chords, 1) => "was boomy",
        (_, 1) => "sat too low",
        _ => "",
    }
}

fn guard(r: &Reg, l: &Limits) -> i32 {
    let order = [0, 1, -1, 2, -2, 3, -3, 4, -4];
    let viol: Vec<Option<f64>> = order
        .iter()
        .map(|k| {
            let m = r.moved(*k);
            if r.lo as i32 + 12 * k < 0 || r.hi as i32 + 12 * k > 127 {
                return None;
            }
            Some(violation(&m, l))
        })
        .collect();
    let min = viol.iter().flatten().cloned().fold(f64::MAX, f64::min);
    order.iter().zip(&viol).find(|(_, v)| v.map(|v| v <= min + 3.0).unwrap_or(false)).map(|(k, _)| *k).unwrap_or(0)
}

struct Placing {
    reg: Reg,
    voice: Voice,
    limits: Limits,
    weight: f64,
    keep: bool,
}
#[derive(Clone)]
struct Placed {
    octave: i32,
    behind: bool,
    thinned: bool,
}

fn overlap(a: f64, b: f64) -> bool {
    (a / 12.0).floor_() == (b / 12.0).floor_() || (a - b).abs() < 9.0
}

fn allocate(parts: &[Placing]) -> Vec<Placed> {
    let n = parts.len();
    if n == 0 {
        return vec![];
    }
    let base: Vec<i32> = parts.iter().map(|p| guard(&p.reg, &p.limits)).collect();
    let cands: Vec<Vec<i32>> = parts
        .iter()
        .zip(&base)
        .map(|(p, b)| {
            let v0 = violation(&p.reg.moved(*b), &p.limits);
            let mut c: Vec<i32> = (-2..=2).map(|d| b + d).filter(|k| violation(&p.reg.moved(*k), &p.limits) <= v0 + 1e-9).filter(|k| (p.reg.lo as i32 + 12 * k) >= 0 && (p.reg.hi as i32 + 12 * k) <= 127).collect();
            if c.is_empty() {
                c.push(*b);
            }
            c.sort_by_key(|k| (k - b).abs());
            c
        })
        .collect();
    let cost_of = |ks: &[i32]| -> f64 {
        let mut c = 0.0;
        let cen: Vec<f64> = (0..ks.len()).map(|i| parts[i].reg.centre + 12.0 * ks[i] as f64).collect();
        for i in 0..ks.len() {
            let p = &parts[i];
            c += ks[i].abs() as f64 * p.weight;
            let (h0, h1) = p.voice.home();
            c += 0.1 * ((h0 - cen[i]).max(0.0) + (cen[i] - h1).max(0.0));
            for j in 0..i {
                let q = &parts[j];
                if overlap(cen[i], cen[j]) {
                    c += if p.voice == Voice::Pad || q.voice == Voice::Pad { 2.5 } else { 6.0 };
                }
                let (lo, hi) = if p.voice.rank() < q.voice.rank() { (i, j) } else { (j, i) };
                if parts[lo].voice.rank() != parts[hi].voice.rank() {
                    if parts[lo].voice == Voice::Bass && cen[hi] < cen[lo] + 7.0 {
                        c += 8.0;
                    } else if cen[hi] < cen[lo] {
                        c += 1.0;
                    }
                }
            }
        }
        c
    };
    let mut best_k = base.clone();
    let mut best_c = cost_of(&base);
    let combos: usize = cands.iter().map(|c| c.len()).product();
    if combos <= 200_000 {
        let mut idx = vec![0usize; n];
        loop {
            let ks: Vec<i32> = (0..n).map(|i| cands[i][idx[i]]).collect();
            let c = cost_of(&ks);
            if c + 1e-9 < best_c {
                best_c = c;
                best_k = ks;
            }
            let mut d = 0;
            while d < n {
                idx[d] += 1;
                if idx[d] < cands[d].len() {
                    break;
                }
                idx[d] = 0;
                d += 1;
            }
            if d == n {
                break;
            }
        }
    } else {
        for _ in 0..3 {
            for i in 0..n {
                for k in &cands[i] {
                    let mut t = best_k.clone();
                    t[i] = *k;
                    let c = cost_of(&t);
                    if c + 1e-9 < best_c {
                        best_c = c;
                        best_k = t;
                    }
                }
            }
        }
    }
    let cen: Vec<f64> = (0..n).map(|i| parts[i].reg.centre + 12.0 * best_k[i] as f64).collect();
    let mut out: Vec<Placed> = best_k.iter().map(|k| Placed { octave: *k, behind: false, thinned: false }).collect();
    for i in 0..n {
        for j in 0..i {
            if out[i].thinned || out[j].thinned || !overlap(cen[i], cen[j]) {
                continue;
            }
            if parts[i].voice == Voice::Pad {
                out[i].behind = true;
            } else if parts[j].voice == Voice::Pad {
                out[j].behind = true;
            } else if !parts[i].keep {
                out[i].thinned = true;
            } else if !parts[j].keep {
                out[j].thinned = true;
            }
        }
    }
    out
}

fn harsh(a: u8, b: u8) -> bool {
    matches!((a as i32 - b as i32).rem_euclid(12), 1 | 6 | 11)
}

fn clash(notes: &[Note], against: &[Note]) -> f64 {
    let (mut bad, mut over) = (0.0, 0.0);
    for n in notes {
        for a in against {
            let o = (n.start + n.len).min(a.start + a.len) - n.start.max(a.start);
            if o > 0.0 {
                over += o;
                if harsh(n.key, a.key) {
                    bad += o;
                }
            }
        }
    }
    if over > 0.0 { bad / over } else { 0.0 }
}

// ── the medley (medley.rs) ──────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Part {
    Lead,
    Chords,
    Bass,
    Drums,
}

#[derive(Clone, Debug)]
pub struct Phrase {
    pub file: String,
    pub index: usize,
    pub beats: f64,
    pub parts: Vec<(Part, u8, Vec<Note>)>,
    pub tonic: u8,
    pub minor: bool,
    pub bpm: f64,
    pub density: f32,
    pub first: Option<u8>,
    pub last: Option<u8>,
    pub id: u64,
}

fn classify(t: &ImportedTrack) -> Part {
    if t.channel == 9 || t.percussive {
        return Part::Drums;
    }
    if t.notes.is_empty() {
        return Part::Lead;
    }
    let mean = t.notes.iter().map(|n| n.key as f64).sum::<f64>() / t.notes.len() as f64;
    if mean < 50.0 {
        return Part::Bass;
    }
    let poly = t.notes.iter().map(|a| t.notes.iter().filter(|b| b.start <= a.start + 1e-6 && b.start + b.len > a.start + 1e-6).count()).sum::<usize>() as f64 / t.notes.len() as f64;
    if poly > 1.6 { Part::Chords } else { Part::Lead }
}

fn fnv(bytes: &[u8], mut h: u64) -> u64 {
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

pub fn chop(m: &ImportedMidi, file: &str, bars: f64) -> Vec<Phrase> {
    let tracks: Vec<(Part, u8, &Vec<Note>)> = m.tracks.iter().filter(|t| !t.notes.is_empty()).map(|t| (classify(t), t.program.unwrap_or(0), &t.notes)).collect();
    if tracks.is_empty() {
        return vec![];
    }
    let end = tracks.iter().flat_map(|t| t.2.iter().map(|n| n.start + n.len)).fold(m.length_beats, f64::max);
    let end = (end / 4.0).ceil_() * 4.0;
    let mut onsets: Vec<(f64, f64)> = tracks.iter().filter(|t| t.0 != Part::Drums).flat_map(|t| t.2.iter().map(|n| (n.start, n.len))).collect();
    if onsets.is_empty() {
        onsets = tracks.iter().flat_map(|t| t.2.iter().map(|n| (n.start, n.len))).collect();
    }
    onsets.sort_by(|a, b| a.0.total_cmp(&b.0));
    let phrases = phrase_map(0.0, end.max(4.0), &onsets, bars);
    let mut h = [0f64; 12];
    for t in tracks.iter().filter(|t| t.0 != Part::Drums) {
        for n in t.2.iter() {
            h[(n.key % 12) as usize] += n.len.max(0.1);
        }
    }
    let key = detect_key_hist(&h);
    let bpm = m.bpm.unwrap_or(120.0);
    let mut out = vec![];
    for (i, ph) in phrases.iter().enumerate() {
        let mut parts: Vec<(Part, u8, Vec<Note>)> = vec![];
        for (part, prog, notes) in &tracks {
            let sel: Vec<Note> = notes
                .iter()
                .filter(|n| n.start >= ph.b0 - 1e-6 && n.start < ph.b1 - 1e-6)
                .map(|n| {
                    let mut c = n.clone();
                    c.start = n.start - ph.b0;
                    c.len = n.len.min(ph.b1 - n.start).max(0.05);
                    c
                })
                .collect();
            if sel.is_empty() {
                continue;
            }
            match parts.iter_mut().find(|p| p.0 == *part) {
                Some(p) => p.2.extend(sel),
                None => parts.push((*part, *prog, sel)),
            }
        }
        let total: usize = parts.iter().map(|p| p.2.len()).sum();
        if total < 4 {
            continue;
        }
        for p in parts.iter_mut() {
            p.2.sort_by(|a, b| a.start.total_cmp(&b.start));
        }
        parts.sort_by_key(|p| p.0);
        let melodic = parts.iter().find(|p| p.0 == Part::Lead).or_else(|| parts.iter().find(|p| p.0 != Part::Drums));
        let first = melodic.and_then(|p| p.2.first().map(|n| n.key));
        let last = melodic.and_then(|p| p.2.last().map(|n| n.key));
        let mut id = fnv(file.as_bytes(), 0xcbf2_9ce4_8422_2325);
        id = fnv(&(i as u32).to_le_bytes(), id);
        for p in &parts {
            for n in &p.2 {
                id = fnv(&[p.0 as u8, n.key, ((n.start * 16.0).round_() as i64 & 0xff) as u8], id);
            }
        }
        let beats = ph.b1 - ph.b0;
        out.push(Phrase { file: file.to_string(), index: i, beats, parts, tonic: key.tonic, minor: key.minor, bpm, density: total as f32 / beats.max(1.0) as f32, first, last, id });
    }
    out
}

pub fn common_key(ph: &[Phrase]) -> (u8, bool) {
    let mut votes = [[0usize; 2]; 12];
    for p in ph {
        votes[p.tonic as usize % 12][p.minor as usize] += 1;
    }
    let mut best = (0u8, false, 0usize);
    for t in 0..12 {
        for m in 0..2 {
            if votes[t][m] > best.2 {
                best = (t as u8, m == 1, votes[t][m]);
            }
        }
    }
    (best.0, best.1)
}

pub fn shift_to(p: &Phrase, tonic: u8, minor: bool) -> i32 {
    let own = if p.minor == minor { p.tonic as i32 } else if p.minor { (p.tonic as i32 + 3) % 12 } else { (p.tonic as i32 + 9) % 12 };
    let d = (tonic as i32 - own).rem_euclid(12);
    if d > 5 { d - 12 } else { d }
}

struct KeyStream {
    key: Vec<u8>,
    ctr: u64,
    buf: Vec<u64>,
}
impl KeyStream {
    fn new(pass: &str) -> KeyStream {
        KeyStream { key: pass.as_bytes().to_vec(), ctr: 0, buf: vec![] }
    }
    fn next(&mut self) -> u64 {
        if self.buf.is_empty() {
            let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.key).expect("any key length");
            mac.update(b"ling-medley");
            mac.update(&self.ctr.to_le_bytes());
            self.ctr += 1;
            let out = mac.finalize().into_bytes();
            self.buf = out.chunks(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).rev().collect();
        }
        self.buf.pop().unwrap()
    }
    fn below(&mut self, n: u64) -> u64 {
        let lim = u64::MAX - u64::MAX % n;
        loop {
            let x = self.next();
            if x < lim {
                return x % n;
            }
        }
    }
}

pub fn keyed_permutation(n: usize, pass: &str) -> Vec<usize> {
    let mut ks = KeyStream::new(pass);
    let mut v: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        let j = ks.below(i as u64 + 1) as usize;
        v.swap(i, j);
    }
    v
}

pub fn encode(message: &str, n: usize, pass: &str) -> Vec<usize> {
    if n < 2 {
        return vec![];
    }
    let mut bytes: Vec<u8> = message.as_bytes().iter().take(255).copied().collect();
    bytes.insert(0, bytes.len() as u8);
    let ndig = ((bytes.len() * 8) as f64 / (n as f64).log2_()).ceil_() as usize;
    let mut nm = bytes;
    let mut digits = vec![0usize; ndig];
    for d in (0..ndig).rev() {
        let mut rem = 0u32;
        for b in nm.iter_mut() {
            let cur = rem * 256 + *b as u32;
            *b = (cur / n as u32) as u8;
            rem = cur % n as u32;
        }
        digits[d] = rem as usize;
    }
    let perm = keyed_permutation(n, pass);
    digits.into_iter().map(|d| perm[d]).collect()
}

pub fn decode(indices: &[usize], n: usize, pass: &str) -> Option<String> {
    if n < 2 || indices.is_empty() {
        return None;
    }
    let perm = keyed_permutation(n, pass);
    let mut inv = vec![0usize; n];
    for (d, p) in perm.iter().enumerate() {
        inv[*p] = d;
    }
    let mut nm: Vec<u32> = vec![0];
    for i in indices {
        let d = *inv.get(*i)? as u32;
        let mut carry = d;
        for b in nm.iter_mut().rev() {
            let cur = *b * n as u32 + carry;
            *b = cur % 256;
            carry = cur / 256;
        }
        while carry > 0 {
            nm.insert(0, carry % 256);
            carry /= 256;
        }
    }
    let bytes: Vec<u8> = nm.into_iter().map(|b| b as u8).collect();
    for s in 0..bytes.len() {
        let len = bytes[s] as usize;
        if s + 1 + len == bytes.len() {
            return String::from_utf8(bytes[s + 1..].to_vec()).ok();
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Smart,
    Random,
    Cipher,
}

pub fn order_to(ph: &[Phrase], count: usize, mode: Mode, seed: u64, pass: &str, key: (u8, bool), curve: Option<&[f64]>) -> Vec<usize> {
    let n = ph.len();
    if n == 0 || count == 0 {
        return vec![];
    }
    match mode {
        Mode::Cipher => {
            let perm = keyed_permutation(n, if pass.is_empty() { "ling" } else { pass });
            (0..count).map(|i| perm[i % n]).collect()
        },
        Mode::Random => {
            let perm = keyed_permutation(n, &format!("seed-{seed}"));
            (0..count).map(|i| perm[i % n]).collect()
        },
        Mode::Smart => {
            let dmax = ph.iter().map(|p| p.density).fold(0.0f32, f32::max).max(0.01);
            let mut ks = KeyStream::new(&format!("smart-{seed}"));
            let mut used = vec![0usize; n];
            let mut out: Vec<usize> = vec![];
            for k in 0..count {
                let pos = k as f32 / (count.max(2) - 1) as f32;
                let want = match curve {
                    Some(c) => c.get(k).copied().unwrap_or(0.5) as f32,
                    None => 0.3 + 0.7 * (core::f32::consts::PI * pos.pow_(1.9)).sin_(),
                };
                let prev = out.last().map(|i| &ph[*i]);
                let mut best = (f32::MAX, 0usize);
                for (i, p) in ph.iter().enumerate() {
                    let mut cost = (used[i] as f32) * 10.0 + (p.density / dmax - want).abs() * 3.0;
                    if let Some(q) = prev {
                        if let (Some(l), Some(f)) = (q.last, p.first) {
                            let a = l as i32 + shift_to(q, key.0, key.1);
                            let b = f as i32 + shift_to(p, key.0, key.1);
                            cost += ((a - b).abs() as f32 / 2.0).min(6.0);
                        }
                        cost += 0.25 * camelot_steps((q.tonic, q.minor), (p.tonic, p.minor)).min(6) as f32;
                        if q.file == p.file {
                            cost += 1.5;
                        }
                        if q.id == p.id {
                            cost += 20.0;
                        }
                    }
                    cost += (ks.next() % 1000) as f32 / 1000.0 * 0.8;
                    if cost < best.0 {
                        best = (cost, i);
                    }
                }
                used[best.1] += 1;
                out.push(best.1);
            }
            out
        },
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Cat {
    Drums,
    Bass,
    Pad,
    Stab,
    Lead,
    Arp,
}
impl Cat {
    pub fn name(self) -> &'static str {
        match self {
            Cat::Drums => "Drums",
            Cat::Bass => "Bass",
            Cat::Pad => "Pad",
            Cat::Stab => "Stab",
            Cat::Lead => "Lead",
            Cat::Arp => "Arp",
        }
    }
}

pub fn category(part: Part, notes: &[Note], beats: f64) -> Cat {
    let avg = notes.iter().map(|n| n.len).sum::<f64>() / notes.len().max(1) as f64;
    match part {
        Part::Drums => Cat::Drums,
        Part::Bass => Cat::Bass,
        Part::Chords => {
            if avg >= 1.5 { Cat::Pad } else { Cat::Stab }
        },
        Part::Lead => {
            let mut keys: Vec<u8> = notes.iter().map(|n| n.key).collect();
            keys.sort();
            keys.dedup();
            if notes.len() as f64 / beats.max(1.0) >= 2.5 && avg <= 0.3 && keys.len() <= 8 { Cat::Arp } else { Cat::Lead }
        },
    }
}

const STORY_ARCS: [&str; 6] = ["rise", "fall", "journey", "dream", "battle", "hero"];
const ARCS: [&str; 7] = ["release", "slow burn", "bittersweet", "triumph", "melancholy", "storm", "lullaby"];
const LEXICON: &[(&str, f32, f32)] = &[
    ("love", 0.6, 0.1), ("light", 0.4, 0.0), ("alive", 0.6, 0.4), ("tonight", 0.2, 0.3), ("dance", 0.5, 0.5), ("fly", 0.4, 0.3), ("free", 0.6, 0.3),
    ("shine", 0.5, 0.2), ("smile", 0.6, 0.1), ("home", 0.3, -0.1), ("sun", 0.4, 0.1), ("dream", 0.2, -0.1), ("hope", 0.5, 0.1), ("forever", 0.3, 0.1),
    ("fire", 0.1, 0.6), ("run", 0.0, 0.5), ("fight", -0.2, 0.7), ("rise", 0.3, 0.5), ("burn", -0.1, 0.6), ("scream", -0.3, 0.8),
    ("alone", -0.6, -0.2), ("cry", -0.6, 0.1), ("tears", -0.6, 0.0), ("dark", -0.5, 0.0), ("cold", -0.4, -0.1), ("lost", -0.5, 0.0), ("goodbye", -0.5, -0.1),
    ("broken", -0.6, 0.1), ("pain", -0.6, 0.3), ("rain", -0.3, -0.1), ("night", -0.1, 0.0), ("fall", -0.3, 0.1), ("gone", -0.5, -0.2), ("miss", -0.5, -0.1),
    ("hurt", -0.6, 0.2), ("die", -0.7, 0.2), ("empty", -0.6, -0.3), ("sorry", -0.4, -0.2), ("quiet", -0.1, -0.4), ("sleep", 0.0, -0.5),
];

/// emotion.rs lyric_lean: (valence, arousal) of the words
fn lyric_lean(text: &str) -> (f32, f32) {
    let t = text.to_lowercase();
    let (mut v, mut a, mut n) = (0.0f32, 0.0f32, 0);
    for w in t.split(|c: char| !c.is_alphabetic()) {
        if let Some((_, lv, la)) = LEXICON.iter().find(|(k, _, _)| *k == w || (w.len() > 4 && w.starts_with(k))) {
            v += lv;
            a += la;
            n += 1;
        }
    }
    if n == 0 {
        return (0.0, 0.0);
    }
    let k = 1.0 / libm::sqrtf(n as f32);
    ((v * k * 0.5).clamp(-0.4, 0.4), (a * k * 0.5).clamp(-0.4, 0.4))
}

/// songgen.rs story_arc
fn story_arc(text: &str) -> String {
    let t = text.to_lowercase();
    let has = |ws: &[&str]| ws.iter().any(|w| t.contains(w));
    let arc = if has(&["hero", "quest", "returns", "comes back", "homecoming", "redemption"]) {
        "hero"
    } else if has(&["tragedy", "tragic", "falls", "loses", "loss", "fades", "sunset", "farewell", "goodbye", "decline"]) {
        "fall"
    } else if has(&["battle", "war", "storm", "chase", "fight", "escape", "danger"]) {
        "battle"
    } else if has(&["dream", "sleep", "lullaby", "float", "calm", "meditat", "drift"]) {
        "dream"
    } else if has(&["journey", "travel", "voyage", "road", "adventure", "wander"]) {
        "journey"
    } else if has(&["rise", "rising", "triumph", "victory", "climb", "sunrise", "awaken", "hope"]) {
        "rise"
    } else {
        ""
    };
    arc.to_string()
}

pub fn arc_point(arc: &str, x: f64) -> (f64, f64) {
    match arc {
        "release" => (0.3 + 0.7 * (PI * x.pow_(1.4)).sin_().max(0.0), -0.2 + 0.8 * x),
        "slow burn" | "rise" => (0.2 + 0.8 * x.pow_(1.3), 0.3 * x),
        "bittersweet" => (0.45 + 0.3 * (PI * x).sin_(), if ((x * 3.99) as i32) % 2 == 0 { 0.35 } else { -0.35 }),
        "triumph" => (0.3 + 0.7 * x.pow_(0.8), -0.2 + x),
        "melancholy" => (0.5 - 0.25 * x + 0.15 * (PI * x).sin_(), -0.6 - 0.2 * (PI * x).sin_()),
        "storm" | "battle" => (0.65 + 0.35 * (PI * 3.0 * x).sin_().abs(), -0.5 + 0.2 * (PI * 2.0 * x).sin_()),
        "lullaby" | "dream" => (0.4 - 0.2 * x + 0.1 * (2.0 * PI * x).sin_(), 0.3),
        "fall" => (0.9 - 0.7 * x, 0.4 - x),
        "journey" => (0.4 + 0.3 * (2.0 * PI * x).sin_().abs() + 0.2 * x, 0.45 * (2.0 * PI * x).sin_()),
        "hero" => (if x < 0.4 { 0.5 - 0.5 * x } else { 0.3 + 0.7 * ((x - 0.4) / 0.6) }, if x < 0.45 { -0.4 } else { -0.4 + 1.2 * (x - 0.45) / 0.55 }),
        _ => (0.3 + 0.7 * (PI * x.pow_(1.9)).sin_(), 0.0),
    }
}

pub fn resolve_arc(feel: &str, story: &str, seed: u64) -> String {
    let f = feel.trim().to_lowercase();
    if ARCS.contains(&f.as_str()) || STORY_ARCS.contains(&f.as_str()) {
        return f;
    }
    let s = story_arc(story);
    if !s.is_empty() {
        return s;
    }
    if !story.trim().is_empty() {
        let (v, a) = lyric_lean(story);
        return if v < -0.15 {
            if a > 0.15 { "storm" } else { "melancholy" }
        } else if v > 0.15 {
            if a > 0.15 { "triumph" } else { "release" }
        } else {
            "bittersweet"
        }
        .into();
    }
    ARCS[(seed % ARCS.len() as u64) as usize].to_string()
}

pub fn movement(arc: &str, n: usize, lean: f64) -> Vec<(f64, f64)> {
    (0..n)
        .map(|i| {
            let x = if n < 2 { 0.5 } else { i as f64 / (n - 1) as f64 };
            let (e, v) = arc_point(arc, x);
            (e.clamp(0.05, 1.0), (v + lean).clamp(-1.0, 1.0))
        })
        .collect()
}

pub fn key_path(mv: &[(f64, f64)], home: (u8, bool), seed: u64) -> Vec<(u8, bool, &'static str)> {
    let n = mv.len();
    let (mut t, mut m) = home;
    let v0 = mv.first().map(|x| x.1).unwrap_or(0.0);
    if v0 < -0.3 && !m {
        t = (t + 9) % 12;
        m = true;
    } else if v0 > 0.3 && m {
        t = (t + 3) % 12;
        m = false;
    }
    let start = (t, m);
    let mean_e = mv.iter().map(|x| x.0).sum::<f64>() / n.max(1) as f64;
    let mut peak = mv.first().map(|x| x.0).unwrap_or(0.0);
    let mut fifths = 0i32;
    let mut boosts = 0;
    let mut ks = KeyStream::new(&format!("keys-{seed}"));
    let mut out = vec![];
    for i in 0..n {
        let mut label = if i == 0 { "home" } else { "stay" };
        if i > 0 {
            let (e, v) = mv[i];
            let want_minor = if v < -0.2 { true } else if v > 0.2 { false } else { m };
            let want_f = ((e - mean_e) * 5.0 + v * 1.5).round_().clamp(-2.0, 2.0) as i32;
            let roll = (ks.next() % 1000) as f64 / 1000.0;
            if e >= 0.85 && e > peak + 0.02 && boosts < 2 && i * 2 >= n {
                t = (t + 2) % 12;
                boosts += 1;
                label = "boost";
            } else if want_minor != m {
                if m {
                    t = (t + 3) % 12;
                    label = "relative major";
                } else {
                    t = (t + 9) % 12;
                    label = "relative minor";
                }
                m = want_minor;
            } else if want_f != fifths && roll < 0.75 {
                if want_f > fifths {
                    t = (t + 7) % 12;
                    fifths += 1;
                    label = "a fifth up";
                } else {
                    t = (t + 5) % 12;
                    fifths -= 1;
                    label = "a fifth down";
                }
            }
            peak = peak.max(e);
            if i + 1 == n && n > 2 && boosts == 0 && (t, m) != start {
                (t, m) = start;
                label = "home";
            }
        }
        out.push((t, m, label));
    }
    out
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Roll {
    None,
    Shrink,
    Grow,
}
pub const ROLL_BEATS: f64 = 32.0;

pub fn roll_steps(r: Roll) -> Vec<f64> {
    match r {
        Roll::Shrink => vec![4.0, 2.0, 1.0, 0.5, 0.5],
        Roll::Grow => vec![0.5, 0.5, 1.0, 2.0, 4.0],
        Roll::None => vec![],
    }
}

#[derive(Clone, Debug)]
pub struct Layer {
    pub phrase: usize,
    pub part: usize,
    pub cat: Cat,
    pub shift: i32,
    pub spine: bool,
    pub octave: i32,
    pub behind: bool,
    pub mask: Vec<(f64, f64)>,
    pub role: &'static str,
    pub pick: usize,
}
impl Layer {
    fn new(phrase: usize, part: usize, cat: Cat, shift: i32, spine: bool) -> Layer {
        Layer { phrase, part, cat, shift, spine, octave: 0, behind: false, mask: vec![], role: "", pick: 0 }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Registers {
    Off,
    Guard,
    Layer,
}
impl Registers {
    fn parse(s: &str) -> Registers {
        match s.trim() {
            "off" | "0" | "none" => Registers::Off,
            "guard" => Registers::Guard,
            _ => Registers::Layer,
        }
    }
}

pub struct Smarts {
    pub registers: Registers,
    pub genre: String,
    pub align: bool,
    pub breakdown: bool,
    pub answer: bool,
    pub clash: bool,
}

fn voice(c: Cat) -> Option<Voice> {
    match c {
        Cat::Drums => None,
        Cat::Bass => Some(Voice::Bass),
        Cat::Pad => Some(Voice::Pad),
        Cat::Stab => Some(Voice::Chords),
        Cat::Lead => Some(Voice::Lead),
        Cat::Arp => Some(Voice::Arp),
    }
}

fn aligned_beats(beats: f64) -> f64 {
    let bars = (beats / 4.0).max(0.25);
    let mut best = (f64::MAX, 4.0);
    for c in [1.0f64, 2.0, 4.0, 8.0, 16.0] {
        let d = (bars / c).log2_().abs();
        if d < best.0 - 1e-9 || ((d - best.0).abs() < 1e-9 && c > best.1) {
            best = (d, c);
        }
    }
    best.1 * 4.0
}

pub fn camelot_steps(a: (u8, bool), b: (u8, bool)) -> i32 {
    let pos = |k: (u8, bool)| ((if k.1 { (k.0 as i32 + 3) % 12 } else { k.0 as i32 % 12 }) * 7) % 12;
    let d = (pos(a) - pos(b)).rem_euclid(12);
    d.min(12 - d) + (a.1 != b.1) as i32
}

fn summarize(log: &[String]) -> String {
    let mut seen: Vec<(String, usize)> = vec![];
    for l in log {
        match seen.iter_mut().find(|s| s.0 == *l) {
            Some(s) => s.1 += 1,
            None => seen.push((l.clone(), 1)),
        }
    }
    seen.iter().map(|(t, n)| if *n > 1 { format!("{t} x{n}") } else { t.clone() }).collect::<Vec<_>>().join(", ")
}

pub struct Slot {
    pub beats: f64,
    pub key: (u8, bool),
    pub energy: f64,
    pub spine: usize,
    pub layers: Vec<Layer>,
    pub roll_after: Roll,
}

pub struct Mashup {
    pub arc: String,
    pub slots: Vec<Slot>,
    pub log: Vec<String>,
}

fn tile(notes: &[Note], src_beats: f64, beats: f64) -> Vec<Note> {
    let src = src_beats.max(0.25);
    let mut out = vec![];
    let mut off = 0.0;
    while off < beats - 1e-6 {
        for n in notes.iter().filter(|n| n.start < src - 1e-6) {
            let s = n.start + off;
            if s >= beats - 1e-6 {
                continue;
            }
            let mut m = n.clone();
            m.start = s;
            m.len = m.len.min(beats - s).max(0.03);
            out.push(m);
        }
        off += src;
    }
    out
}

pub fn layer_notes(ph: &[Phrase], l: &Layer, beats: f64) -> Vec<Note> {
    let p = &ph[l.phrase];
    let Some((_, _, notes)) = p.parts.get(l.part) else { return vec![] };
    let mut v = tile(notes, p.beats, beats);
    if !l.mask.is_empty() {
        v.retain(|n| l.mask.iter().any(|(a, b)| n.start >= a - 1e-6 && n.start < b - 1e-6));
        for n in v.iter_mut() {
            if let Some((_, b)) = l.mask.iter().find(|(a, b)| n.start >= a - 1e-6 && n.start < b - 1e-6) {
                n.len = n.len.min(b - n.start).max(0.03);
            }
        }
    }
    if l.cat != Cat::Drums {
        for n in v.iter_mut() {
            n.key = (n.key as i32 + l.shift + 12 * l.octave).clamp(0, 127) as u8;
        }
    }
    if l.behind {
        for n in v.iter_mut() {
            n.vel *= 0.72;
            n.pan = if n.key % 2 == 0 { -0.4 } else { 0.4 };
        }
    }
    v
}

fn bass_fold(notes: &[Note], shift: i32) -> i32 {
    let Some(r) = analyze(notes, 1.0) else { return 0 };
    let m = r.median + shift as f64;
    let mut k = 0;
    while m + 12.0 * (k as f64) < 28.0 && k < 6 {
        k += 1;
    }
    while m + 12.0 * (k as f64) > 64.0 && k > -6 {
        k -= 1;
    }
    k
}

pub fn riser_notes(key: (u8, bool), beats: f64) -> Vec<Note> {
    let sc = Key::new(key.0, key.1).scale();
    let base = if key.0 > 6 { 48 + key.0 as i32 } else { 60 + key.0 as i32 };
    let end = (beats - 0.5).max(1.0);
    let mut out = vec![];
    let mut t = 0.0;
    while t < end - 1e-6 {
        let frac = t / end;
        let step: f64 = if frac < 0.45 { 2.0 } else if frac < 0.72 { 1.0 } else if frac < 0.9 { 0.5 } else { 0.25 };
        let deg = (frac * 14.0).round_() as usize;
        let k = base + (sc[deg % 7] as i32 - key.0 as i32).rem_euclid(12) + 12 * (deg / 7) as i32;
        out.push(Note::new(t, (step * 1.05).min(end - t), k.clamp(0, 127) as u8, (0.3 + 0.6 * frac) as f32));
        t += step;
    }
    out
}

pub fn roll_notes(notes: &[Note], src_beats: f64, r: Roll) -> Vec<Note> {
    let src = tile(notes, src_beats, 16.0);
    let mut out = vec![];
    let mut at = 0.0;
    for b in roll_steps(r) {
        let len = b * 4.0;
        for n in src.iter().filter(|n| n.start < len - 1e-6) {
            let mut m = n.clone();
            m.start = n.start + at;
            m.len = n.len.min(len - n.start).max(0.03);
            m.vel = (n.vel * (0.72 + 0.4 * (m.start / ROLL_BEATS) as f32)).min(1.0);
            out.push(m);
        }
        at += len;
    }
    out
}

pub fn role_for(cat: Cat, energy: f64, minor: bool, genre: &str, k: usize) -> &'static str {
    let pick = |v: &[&'static str]| v[k % v.len()];
    let bassm = matches!(genre, "dubstep" | "drum and bass");
    match cat {
        Cat::Drums => "drums",
        Cat::Bass if bassm && energy > 0.6 => pick(&["growl bass", "wobble bass"]),
        Cat::Bass if energy > 0.6 => pick(&["rolling bass", "pluck bass", "sub bass"]),
        Cat::Bass => pick(&["sub bass", "pluck bass"]),
        Cat::Pad if minor => pick(&["dark pad", "warm pad", "ambient pad"]),
        Cat::Pad => pick(&["warm pad", "ambient pad", "strings"]),
        Cat::Stab => pick(&["stab chord", "keys", "piano", "organ"]),
        Cat::Lead if energy > 0.7 && bassm => pick(&["supersaw lead", "laser lead", "synth lead"]),
        Cat::Lead if energy > 0.7 => pick(&["supersaw lead", "synth lead", "pluck"]),
        Cat::Lead => pick(&["synth lead", "pluck", "keys"]),
        Cat::Arp => pick(&["arp", "pluck"]),
    }
}

fn fit_score(notes: &[Note], shift: i32, key: Key, against: &[Note]) -> f64 {
    let (mut inside, mut total) = (0.0, 0.0);
    let (mut cons, mut over) = (0.0, 0.0);
    for n in notes {
        let k = n.key as i32 + shift;
        let d = n.len.max(0.05);
        total += d;
        if key.in_scale(k.rem_euclid(12) as u8) {
            inside += d;
        }
        for a in against {
            let o = (n.start + n.len).min(a.start + a.len) - n.start.max(a.start);
            if o > 0.0 {
                over += o;
                if matches!((k - a.key as i32).rem_euclid(12), 0 | 3 | 4 | 5 | 7 | 8 | 9) {
                    cons += o;
                }
            }
        }
    }
    if total <= 0.0 {
        return 0.0;
    }
    inside / total + 0.5 * if over > 0.0 { cons / over } else { 1.0 }
}

fn fit_shift(notes: &[Note], base: i32, key: (u8, bool), against: &[Note]) -> i32 {
    let (lo, hi) = (-6, 5);
    let k = Key::new(key.0, key.1);
    let base = base.clamp(lo, hi);
    let mut best = (f64::MIN, base);
    for s in lo..=hi {
        let sc = fit_score(notes, s, k, against) - 0.012 * s.abs() as f64 + if s == base { 0.02 } else { 0.0 };
        if sc > best.0 + 1e-9 {
            best = (sc, s);
        }
    }
    best.1
}

fn slot_tag(i: usize, n: usize, energy: f64) -> &'static str {
    if i == 0 && energy < 0.6 {
        "intro"
    } else if i + 1 == n && energy < 0.6 {
        "outro"
    } else if energy >= 0.75 {
        "drop"
    } else if energy >= 0.5 {
        "verse"
    } else {
        "break"
    }
}

pub fn mashup_with(ph: &[Phrase], spine: &[usize], arc: &str, lean: f64, layering: f64, rolls: f64, seed: u64, home: (u8, bool), sm: &Smarts) -> Mashup {
    let n = spine.len();
    let mut mv = movement(arc, n, lean);
    let keys = key_path(&mv, home, seed);
    let layered = layering >= 0.05;
    let mut log: Vec<String> = vec![];
    let mut breakdown = None;
    if layered && sm.breakdown && n >= 6 {
        if let Some(l) = (n / 2..n).rev().find(|i| mv[*i].0 >= 0.75) {
            if l >= 2 && mv[l - 1].0 >= 0.6 {
                mv[l - 1].0 = 0.3;
                breakdown = Some(l - 1);
                log.push("a breakdown before the last drop".into());
            }
        }
    }
    let regs = if layered { sm.registers } else if sm.registers == Registers::Layer { Registers::Guard } else { sm.registers };
    let mut cells: Vec<(usize, usize, Cat, f64)> = vec![];
    for (i, p) in ph.iter().enumerate() {
        for (j, (part, _, notes)) in p.parts.iter().enumerate() {
            cells.push((i, j, category(*part, notes, p.beats), notes.len() as f64 / p.beats.max(1.0)));
        }
    }
    let mut dmax = [0.01f64; 6];
    for c in &cells {
        dmax[c.2 as usize] = dmax[c.2 as usize].max(c.3);
    }
    let mut used = vec![0usize; cells.len()];
    let mut uses = [0usize; 6];
    let mut ks = KeyStream::new(&format!("mashup-{seed}"));
    let mut slots = vec![];
    let shifted = |ns: &[Note], s: i32| -> Vec<Note> { ns.iter().map(|x| Note { key: (x.key as i32 + s).clamp(0, 127) as u8, ..x.clone() }).collect() };
    for (si, &pi) in spine.iter().enumerate() {
        let p = &ph[pi];
        let sb = if layered && sm.align { aligned_beats(p.beats) } else { p.beats };
        let (e, _v) = mv[si];
        let (t, m, _label) = keys[si];
        let shift_of = |q: usize, cat: Cat| if cat == Cat::Drums { 0 } else { shift_to(&ph[q], t, m) };
        let own: Vec<(usize, Cat)> = cells.iter().filter(|c| c.0 == pi).map(|c| (c.1, c.2)).collect();
        let pref = [Cat::Lead, Cat::Stab, Cat::Pad, Cat::Arp, Cat::Bass, Cat::Drums];
        let sp = pref.iter().find_map(|c| own.iter().find(|o| o.1 == *c)).copied().unwrap_or((0, Cat::Lead));
        let mut layers = vec![Layer::new(pi, sp.0, sp.1, shift_of(pi, sp.1), true)];
        if !layered {
            for o in own.iter().filter(|o| o.0 != sp.0) {
                layers.push(Layer::new(pi, o.0, o.1, shift_of(pi, o.1), false));
            }
        } else {
            let chord = if e >= 0.55 { vec![Cat::Stab, Cat::Pad] } else { vec![Cat::Pad, Cat::Stab] };
            let mut want: Vec<Vec<Cat>> = vec![chord];
            if e >= 0.35 {
                want.push(vec![Cat::Bass]);
            }
            if e >= 0.55 {
                want.push(vec![Cat::Drums]);
            }
            if e >= 0.75 {
                want.push(vec![Cat::Arp, Cat::Lead]);
            }
            for alts in want {
                let alts: Vec<Cat> = alts.into_iter().filter(|c| !layers.iter().any(|l| l.cat == *c)).collect();
                if alts.is_empty() {
                    continue;
                }
                let r = (ks.next() % 1000) as f64 / 1000.0;
                if let Some(o) = own.iter().find(|o| alts.contains(&o.1) && o.0 != sp.0) {
                    if r >= layering {
                        layers.push(Layer::new(pi, o.0, o.1, shift_of(pi, o.1), false));
                        continue;
                    }
                }
                let mut best: Option<(f64, usize)> = None;
                for (ci, c) in cells.iter().enumerate() {
                    if !alts.contains(&c.2) {
                        continue;
                    }
                    let q = &ph[c.0];
                    let mut cost = (c.3 / dmax[c.2 as usize] - e).abs() * 2.0 + used[ci] as f64 * 1.5;
                    cost += alts.iter().position(|a| *a == c.2).unwrap_or(0) as f64 * 0.4;
                    if c.2 != Cat::Drums {
                        if q.minor != m {
                            cost += 0.8;
                        }
                        cost += 0.12 * camelot_steps((q.tonic, q.minor), (t, m)).min(5) as f64;
                    }
                    if q.file != p.file {
                        cost -= 0.8 * layering;
                    }
                    let ratio = sb / q.beats.max(0.25);
                    if ratio < 0.99 || (ratio - ratio.round_()).abs() > 0.01 {
                        cost += 0.5;
                    }
                    cost += (ks.next() % 1000) as f64 / 1000.0 * 0.6;
                    if best.map(|b| cost < b.0).unwrap_or(true) {
                        best = Some((cost, ci));
                    }
                }
                if let Some((_, ci)) = best {
                    used[ci] += 1;
                    let c = cells[ci];
                    layers.push(Layer::new(c.0, c.1, c.2, shift_of(c.0, c.2), false));
                }
            }
        }
        let tiles = |l: &Layer| -> Vec<Note> {
            let q = &ph[l.phrase];
            q.parts.get(l.part).map(|x| tile(&x.2, q.beats, sb)).unwrap_or_default()
        };
        let mut against: Vec<Note> = vec![];
        let mut fitted: Vec<Option<Vec<Note>>> = vec![None; layers.len()];
        let mut li = 0;
        while li < layers.len() {
            if layers[li].cat == Cat::Drums {
                li += 1;
                continue;
            }
            let raw = tiles(&layers[li]);
            let s = fit_shift(&raw, layers[li].shift, (t, m), &against);
            layers[li].shift = s;
            let mut ns = shifted(&raw, s);
            let mut drop_it = false;
            if sm.clash && layered && !layers[li].spine && !against.is_empty() {
                let cl = clash(&ns, &against);
                if cl > 0.3 {
                    let mut cand: Vec<(usize, u64, usize)> =
                        (0..cells.len()).filter(|ci| cells[*ci].2 == layers[li].cat && !(cells[*ci].0 == layers[li].phrase && cells[*ci].1 == layers[li].part)).map(|ci| (used[ci], ks.next() % 1000, ci)).collect();
                    cand.sort();
                    let mut best: Option<(f64, usize, i32, Vec<Note>)> = None;
                    for (_, _, ci) in cand.into_iter().take(6) {
                        let c = cells[ci];
                        let q = &ph[c.0];
                        let raw2 = tile(&q.parts[c.1].2, q.beats, sb);
                        let s2 = fit_shift(&raw2, shift_to(q, t, m), (t, m), &against);
                        let ns2 = shifted(&raw2, s2);
                        let cl2 = clash(&ns2, &against);
                        if best.as_ref().map(|b| cl2 < b.0).unwrap_or(true) {
                            best = Some((cl2, ci, s2, ns2));
                        }
                    }
                    let name = layers[li].cat.name().to_lowercase();
                    match best {
                        Some((cl2, ci, s2, ns2)) if cl2 < cl - 0.1 => {
                            used[ci] += 1;
                            layers[li].phrase = cells[ci].0;
                            layers[li].part = cells[ci].1;
                            layers[li].shift = s2;
                            ns = ns2;
                            if cl2 > 0.45 {
                                drop_it = true;
                            } else {
                                log.push(format!("{name} swapped for one that does not clash"));
                            }
                        },
                        _ => drop_it = cl > 0.45,
                    }
                    if drop_it {
                        log.push(format!("{name} left out (clashed)"));
                    }
                }
            }
            if drop_it {
                layers.remove(li);
                fitted.remove(li);
                continue;
            }
            against.extend(ns.iter().cloned());
            fitted[li] = Some(ns);
            li += 1;
        }
        if layered && sm.answer && layers[0].cat == Cat::Lead {
            if let Some(hook) = fitted[0].clone() {
                let bars = (sb / 4.0).ceil_().max(1.0) as usize;
                let act: Vec<f64> = (0..bars)
                    .map(|b| {
                        let (b0, b1) = (b as f64 * 4.0, b as f64 * 4.0 + 4.0);
                        hook.iter().map(|x| ((x.start + x.len).min(b1) - x.start.max(b0)).max(0.0)).sum::<f64>() / 4.0
                    })
                    .collect();
                for l in layers.iter_mut().skip(1).filter(|l| l.cat == Cat::Lead) {
                    let mut on: Vec<usize> = (0..bars).filter(|b| act[*b] < 0.25).collect();
                    if on.len() * 4 < bars || on.len() * 4 > bars * 3 {
                        on = (0..bars).filter(|b| if bars >= 4 { (b / 2) % 2 == 1 } else { b % 2 == 1 }).collect();
                    }
                    if !on.is_empty() && on.len() < bars {
                        l.mask = on.iter().map(|b| (*b as f64 * 4.0, (*b as f64 * 4.0 + 4.0).min(sb))).collect();
                        log.push("a second lead answers the hook".into());
                    }
                }
            }
        }
        for l in layers.iter_mut() {
            let k = uses[l.cat as usize];
            uses[l.cat as usize] += 1;
            l.pick = k;
            l.role = role_for(l.cat, e, m, &sm.genre, k);
        }
        let pitched: Vec<usize> = (0..layers.len()).filter(|i| layers[*i].cat != Cat::Drums && fitted.get(*i).and_then(|f| f.as_ref()).map(|f| !f.is_empty()).unwrap_or(false)).collect();
        if regs == Registers::Off {
            for i in pitched.iter().copied() {
                if layers[i].cat == Cat::Bass {
                    layers[i].octave = bass_fold(fitted[i].as_deref().unwrap_or(&[]), 0);
                }
            }
        } else if !pitched.is_empty() {
            let places: Vec<Placing> = pitched
                .iter()
                .filter_map(|i| {
                    let l = &layers[*i];
                    let reg = analyze(fitted[*i].as_deref().unwrap_or(&[]), sb)?;
                    let vc = voice(l.cat)?;
                    let busy = reg.busy();
                    Some(Placing { reg, voice: vc, limits: limits(vc, brightness(l.role), busy), weight: if l.spine { 1.5 } else { 1.0 }, keep: l.spine || l.cat == Cat::Bass })
                })
                .collect();
            let placed: Vec<Placed> = if regs == Registers::Layer && places.len() >= 2 {
                allocate(&places)
            } else {
                places.iter().map(|pl| Placed { octave: guard(&pl.reg, &pl.limits), behind: false, thinned: false }).collect()
            };
            let mut gone: Vec<usize> = vec![];
            for ((i, pl), pd) in pitched.iter().zip(&places).zip(&placed) {
                let l = &mut layers[*i];
                let name = l.cat.name().to_lowercase();
                l.octave = pd.octave;
                l.behind = pd.behind;
                if pd.octave != 0 {
                    let g = guard(&pl.reg, &pl.limits);
                    let why = if g != 0 && g.signum() == pd.octave.signum() { reason(pl.voice, g) } else { "own octave" };
                    let o = pd.octave.abs();
                    log.push(format!("{name} {} {o} octave{} ({why})", if pd.octave > 0 { "up" } else { "down" }, if o > 1 { "s" } else { "" }));
                }
                if pd.behind {
                    log.push(format!("{name} behind (shares an octave)"));
                }
                if pd.thinned && !l.spine {
                    log.push(format!("{name} left out (no octave free)"));
                    gone.push(*i);
                }
            }
            gone.sort();
            for i in gone.into_iter().rev() {
                layers.remove(i);
            }
        }
        slots.push(Slot { beats: sb, key: (t, m), energy: e, spine: pi, layers, roll_after: Roll::None });
    }
    if rolls > 0.01 && n >= 2 {
        let thr = 0.4 - 0.3 * rolls.clamp(0.0, 1.0);
        for i in 0..n - 1 {
            let de = mv[i + 1].0 - mv[i].0;
            let (into, out) = (mv[i].0 < 0.75 && mv[i + 1].0 >= 0.75, mv[i].0 >= 0.75 && mv[i + 1].0 < 0.75);
            slots[i].roll_after = if de > thr || (rolls >= 0.3 && into) {
                Roll::Shrink
            } else if de < -thr || (rolls >= 0.3 && out) {
                Roll::Grow
            } else {
                Roll::None
            };
        }
        if rolls >= 0.5 && n >= 3 {
            let peak = (1..n).max_by(|a, b| mv[*a].0.total_cmp(&mv[*b].0)).unwrap_or(1);
            if slots[peak - 1].roll_after == Roll::None {
                slots[peak - 1].roll_after = Roll::Shrink;
            }
        }
        if let Some(b) = breakdown {
            slots[b].roll_after = Roll::Shrink;
        }
    }
    Mashup { arc: arc.to_string(), slots, log }
}

pub fn short_id(id: u64) -> String {
    format!("{:06x}", id & 0xff_ffff)
}

// ── the folder (song_api.rs medley_phrases) ─────────────────────────────────

fn medley_phrases(dir: &str, bars: f64) -> (Vec<Phrase>, usize) {
    let dir = dir.trim_end_matches('/');
    let mut files: Vec<String> = crate::vfs::list(dir)
        .into_iter()
        .filter(|(n, is_dir, _)| {
            let lo = n.to_lowercase();
            !*is_dir && ["mid", "midi"].iter().any(|e| lo.rsplit_once('.').map(|(_, x)| x == *e).unwrap_or(false))
        })
        .map(|(n, _, _)| n)
        .collect();
    files.sort();
    let mut out = vec![];
    let mut used = 0;
    for f in &files {
        let Some(data) = crate::vfs::read(&format!("{dir}/{f}")) else { continue };
        let Ok(m) = parse_midi(&data) else { continue };
        let name = f.rsplit_once('.').map(|(a, _)| a).unwrap_or(f);
        let ph = chop(&m, name, bars);
        if !ph.is_empty() {
            used += 1;
        }
        out.extend(ph);
    }
    (out, used)
}

// ── the instruments: a role -> a General MIDI patch of the app (the desktop picks from its genre libraries) ──

fn role_program(cat: Cat, role: &str, k: usize) -> u8 {
    let pick = |v: &[u8]| v[k % v.len()];
    match role {
        "sub bass" => pick(&[38, 33]),
        "pluck bass" => pick(&[34, 33]),
        "rolling bass" => pick(&[39, 38]),
        "growl bass" | "wobble bass" => pick(&[39, 87]),
        "dark pad" => pick(&[94, 95]),
        "warm pad" => pick(&[89, 88]),
        "ambient pad" => pick(&[99, 91]),
        "strings" => pick(&[48, 49]),
        "stab chord" => pick(&[62, 61]),
        "keys" => pick(&[4, 5]),
        "piano" => 0,
        "organ" => pick(&[16, 17]),
        "supersaw lead" => pick(&[81, 90]),
        "laser lead" => pick(&[84, 80]),
        "synth lead" => pick(&[80, 81]),
        "pluck" => pick(&[45, 46]),
        "arp" => pick(&[80, 11]),
        _ => match cat {
            Cat::Bass => 33,
            Cat::Pad => 89,
            Cat::Stab => 4,
            Cat::Arp => 80,
            _ => 80,
        },
    }
}

fn to_doc(n: &Note) -> NoteDoc {
    let mut d = NoteDoc::new(n.start, n.len.max(0.03), n.key, n.vel.clamp(0.01, 1.0));
    d.pan = n.pan;
    d
}

/// `daw_song_medley_build`: (folder, phrase bars 0 = auto, phrases, mode, passphrase, message, seed, bpm 0 = the files', options)
fn build(d: &mut Daw, args: &[u64]) -> String {
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).unwrap_or(0.0);
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let (ph, files) = medley_phrases(&s(0).replace('\\', "/"), n(1));
    if ph.len() < 2 {
        return String::from("Not enough phrases: the folder needs MIDI files with notes");
    }
    let mode = match s(3).as_str() {
        "random" => Mode::Random,
        "cipher" => Mode::Cipher,
        _ => Mode::Smart,
    };
    let pass = s(4);
    let message = s(5);
    let seed = n(6).abs() as u64;
    let opts = s(8);
    let (head, story) = match opts.find("story=") {
        Some(i) => (String::from(&opts[..i]), String::from(opts[i + 6..].trim())),
        None => (opts.clone(), String::new()),
    };
    let opt = |k: &str| head.split(';').filter_map(|kv| kv.split_once('=')).find(|(kk, _)| kk.trim() == k).map(|(_, v)| String::from(v.trim()));
    let num = |k: &str| opt(k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0).clamp(0.0, 1.0);
    let flag = |k: &str| opt(k).map(|v| !matches!(v.as_str(), "0" | "off" | "false" | "no")).unwrap_or(true);
    let (layering, rolls) = (num("layering"), num("rolls"));
    let genre = opt("genre").filter(|x| !x.is_empty()).unwrap_or_else(|| String::from("pop"));
    let mut bpms: Vec<f64> = ph.iter().map(|p| p.bpm).collect();
    bpms.sort_by(f64::total_cmp);
    let bpm = if n(7) > 20.0 { n(7) } else { bpms[bpms.len() / 2] };
    let key = common_key(&ph);
    let arc_opt = opt("arc").unwrap_or_default();
    let feel_opt = opt("feelarc").unwrap_or_default();
    let arc_in = if !arc_opt.is_empty() && arc_opt != "none" { arc_opt } else { feel_opt };
    let arc = resolve_arc(&arc_in, &story, seed);
    let lean = if story.is_empty() { 0.0 } else { lyric_lean(&story).0 as f64 * 0.5 };
    let count = (n(2).max(2.0) as usize).min(512);
    let order: Vec<usize> = if mode == Mode::Cipher && !message.is_empty() {
        encode(&message, ph.len(), &pass)
    } else {
        let curve: Vec<f64> = movement(&arc, count, lean).iter().map(|x| x.0).collect();
        order_to(&ph, count, mode, seed, &pass, key, Some(&curve))
    };
    let smarts = Smarts { registers: Registers::parse(&opt("registers").unwrap_or_default()), genre: genre.clone(), align: flag("align"), breakdown: flag("breakdown"), answer: flag("answer"), clash: flag("clash") };
    let mash = mashup_with(&ph, &order, &arc, lean, layering, rolls, seed, key, &smarts);
    let p = &mut d.project;
    let end0 = p.tracks.iter().flat_map(|t| t.clips.iter().map(|c| c.start + c.len)).fold(0.0, f64::max);
    let start0 = (end0 / 4.0).ceil_() * 4.0;
    let empty = p.tracks.iter().all(|t| t.clips.is_empty());
    if empty {
        p.tempo_bpm = bpm.clamp(40.0, 240.0);
    }
    // the tracks: one per (category, patch), at most 24 (then the category's first)
    let gm = super::midi::gm_patches();
    let mut wanted: Vec<(Cat, u8)> = vec![];
    let mut assign: Vec<Vec<usize>> = vec![];
    for sl in &mash.slots {
        let mut row = vec![];
        for l in &sl.layers {
            let prog = if l.cat == Cat::Drums { 255 } else { role_program(l.cat, l.role, l.pick / 3) };
            let idx = match wanted.iter().position(|w| w.0 == l.cat && w.1 == prog) {
                Some(i) => i,
                None if wanted.len() < 24 => {
                    wanted.push((l.cat, prog));
                    wanted.len() - 1
                },
                None => match wanted.iter().position(|w| w.0 == l.cat) {
                    Some(i) => i,
                    None => {
                        wanted.push((l.cat, prog));
                        wanted.len() - 1
                    },
                },
            };
            row.push(idx);
        }
        assign.push(row);
    }
    let first_track = p.tracks.len();
    for (cat, prog) in &wanted {
        let drums = *cat == Cat::Drums;
        let mut t = TrackDoc {
            name: String::new(),
            color: super::PALETTE[p.tracks.len() % 7],
            kind: String::from("instrument"),
            instrument: String::from(if drums { "drums" } else { "synth" }),
            patch: String::new(),
            level: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            effects: Vec::new(),
            clips: Vec::new(),
            state: Vec::new(),
            config: String::new(),
            session: Vec::new(),
            auto: Vec::new(),
        };
        let mut stem = String::from(if drums { "drums" } else { "synth" });
        if !drums {
            if let Some(g) = gm.iter().find(|g| g.0 == *prog) {
                let bytes = crate::appfs::get(&g.2).map(|b| b.to_vec()).or_else(|| crate::vfs::read(&g.2));
                if let Some(bytes) = bytes {
                    apply_patch(&mut t, core::str::from_utf8(&bytes).unwrap_or(""), &g.2);
                }
                stem = g.1.clone();
            }
        }
        t.name = format!("{} · {}", cat.name(), stem);
        p.tracks.push(t);
    }
    // the sections: every slot, and a loop roll after it where the plan has one
    let ns = mash.slots.len();
    let mut at = start0;
    let mut clips = 0usize;
    let mut foreign = 0usize;
    let mut rolled = 0usize;
    let risers = flag("risers") && rolls > 0.01;
    let mut riser_t: Option<usize> = None;
    let mut keys_said: Vec<String> = vec![];
    for (si, sl) in mash.slots.iter().enumerate() {
        let k = Key::new(sl.key.0, sl.key.1).name();
        if keys_said.last() != Some(&k) {
            keys_said.push(k);
        }
        for (li, l) in sl.layers.iter().enumerate() {
            let t = first_track + assign[si][li];
            let q = &ph[l.phrase];
            let name = if l.spine { format!("{} #{} · {}", q.file, q.index + 1, short_id(q.id)) } else { format!("{} #{} {}", q.file, q.index + 1, l.cat.name()) };
            let notes: Vec<NoteDoc> = layer_notes(&ph, l, sl.beats).iter().map(to_doc).collect();
            if let Some(tr) = p.tracks.get_mut(t) {
                tr.clips.push(ClipDoc { name, start: at, len: sl.beats, midi_notes: notes, ..ClipDoc::default() });
                clips += 1;
                if q.file != ph[sl.spine].file {
                    foreign += 1;
                }
            }
        }
        at += sl.beats;
        let src = match sl.roll_after {
            Roll::Shrink => Some(si),
            Roll::Grow => Some(si + 1).filter(|x| *x < ns),
            Roll::None => None,
        };
        if let Some(ri) = src {
            let rs = &mash.slots[ri];
            for (li, l) in rs.layers.iter().enumerate() {
                let t = first_track + assign[ri][li];
                let q = &ph[l.phrase];
                let notes: Vec<NoteDoc> = roll_notes(&layer_notes(&ph, l, rs.beats), rs.beats, sl.roll_after).iter().map(to_doc).collect();
                if let Some(tr) = p.tracks.get_mut(t) {
                    tr.clips.push(ClipDoc { name: format!("{} #{} roll", q.file, q.index + 1), start: at, len: ROLL_BEATS, midi_notes: notes, ..ClipDoc::default() });
                }
            }
            if risers && sl.roll_after == Roll::Shrink {
                let ti = *riser_t.get_or_insert_with(|| {
                    let mut t = TrackDoc {
                        name: String::from("Riser"),
                        color: super::PALETTE[p.tracks.len() % 7],
                        kind: String::from("instrument"),
                        instrument: String::from("synth"),
                        patch: String::new(),
                        level: 0.7,
                        pan: 0.0,
                        muted: false,
                        solo: false,
                        effects: Vec::new(),
                        clips: Vec::new(),
                        state: Vec::new(),
                        config: String::new(),
                        session: Vec::new(),
                        auto: Vec::new(),
                    };
                    if let Some(g) = gm.iter().find(|g| g.0 == 81) {
                        if let Some(bytes) = crate::appfs::get(&g.2).map(|b| b.to_vec()).or_else(|| crate::vfs::read(&g.2)) {
                            apply_patch(&mut t, core::str::from_utf8(&bytes).unwrap_or(""), &g.2);
                        }
                    }
                    t.name = String::from("Riser");
                    p.tracks.push(t);
                    p.tracks.len() - 1
                });
                let into = mash.slots[(si + 1).min(ns - 1)].key;
                let notes: Vec<NoteDoc> = riser_notes(into, ROLL_BEATS).iter().map(to_doc).collect();
                p.tracks[ti].clips.push(ClipDoc { name: String::from("riser"), start: at, len: ROLL_BEATS, midi_notes: notes, ..ClipDoc::default() });
            }
            at += ROLL_BEATS;
            rolled += 1;
        }
    }
    d.dirty = true;
    d.rev += 1;
    let tracks = wanted.len() + riser_t.is_some() as usize;
    let _ = slot_tag;
    format!(
        "{}: {} phrases from {} files ({} in all) at {:.0} bpm, {} bars; story: {}; keys: {}; {} clips ({} layered from other files) on {} instruments; {} loop rolls{}; order: {}{}; on LingOS: General MIDI sounds by role (no sound design, recipes, feel, vocal chops or Mix AI)",
        if layering > 0.05 { "Mashup" } else { "Medley" },
        order.len(),
        files,
        ph.len(),
        bpm,
        libm::round((at - start0) / 4.0),
        mash.arc,
        keys_said.join(" > "),
        clips,
        foreign,
        tracks,
        rolled,
        if mash.log.is_empty() { String::new() } else { format!("; arrangement: {}", summarize(&mash.log)) },
        s(3),
        if mode == Mode::Cipher && !message.is_empty() { format!(", the order spells a {}-character message (decode with the same folder and passphrase)", message.chars().count()) } else { String::new() }
    )
}

pub(super) fn dispatch(name: &[u8], args: &[u64], d: &mut Daw) -> Option<u64> {
    if !name.starts_with(b"daw_song_medley_") {
        return None;
    }
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).unwrap_or(0.0);
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default().replace('\\', "/");
    Some(match name {
        b"daw_song_medley_scan" => {
            // (folder, phrase bars 0 = auto) -> [files used, phrases, key name, median bpm]
            let (ph, files) = medley_phrases(&s(0), n(1));
            if ph.is_empty() {
                return Some(list_from(vec![num(0.0), num(0.0), mk_str(""), num(0.0)]));
            }
            let (t, m) = common_key(&ph);
            let mut b: Vec<f64> = ph.iter().map(|p| p.bpm).collect();
            b.sort_by(f64::total_cmp);
            list_from(vec![num(files as f64), num(ph.len() as f64), mk_str(&Key::new(t, m).name()), num(libm::round(b[b.len() / 2]))])
        },
        b"daw_song_medley_build" => {
            let r = build(d, args);
            mk_str(&r)
        },
        b"daw_song_medley_mix" => {
            // (genre) -> [loudness LUFS, brightness]: Mix AI's target (LingOS has no Mix AI; the app skips it when daw_mix_start says no)
            list_from(vec![num(-9.0), num(0.0)])
        },
        b"daw_song_medley_decode" => {
            // (folder, phrase bars, passphrase) -> the message the project's medley clips spell ("" when none)
            let (ph, _) = medley_phrases(&s(0), n(1));
            let mut seen: Vec<(f64, String)> = vec![];
            for t in &d.project.tracks {
                for c in &t.clips {
                    if let Some((_, id)) = c.name.rsplit_once("· ") {
                        if !seen.iter().any(|x| (x.0 - c.start).abs() < 1e-6) {
                            seen.push((c.start, String::from(id.trim())));
                        }
                    }
                }
            }
            seen.sort_by(|x, y| x.0.total_cmp(&y.0));
            let idx: Vec<usize> = seen.iter().filter_map(|(_, id)| ph.iter().position(|p| short_id(p.id) == *id)).collect();
            let pass = args.get(2).map(|&v| to_string(v)).unwrap_or_default();
            mk_str(&decode(&idx, ph.len(), &pass).unwrap_or_default())
        },
        _ => return None,
    })
}
