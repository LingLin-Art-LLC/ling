//! Decorate Clip and Theme and Variations on LingOS (`daw_deco_clip`, `daw_deco_variations`).
//!
//! The desktop's decorator (ling-daw decorate.rs over theory.rs) is large; this is a smaller one with the same promise:
//! the clip's own key (pitch-class profile), melody notes keep their onsets and every added note is a tone of the key --
//! long melody notes get a passing tone toward the next note or a neighbour turn, chords are rolled and get the root an
//! octave up. `amount` 0..1 is how much is added, `seed` makes it repeatable.

use alloc::string::String;
use alloc::vec::Vec;

use super::{Daw, NoteDoc};
use crate::lingrt::{num, to_f64};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// (tonic pc, minor) by the Krumhansl-Kessler profiles, weighted by note length
fn key_of(notes: &[NoteDoc]) -> (u8, bool) {
    const MAJ: [f64; 12] = [6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88];
    const MIN: [f64; 12] = [6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17];
    let mut h = [0f64; 12];
    for n in notes {
        h[(n.key % 12) as usize] += n.len.min(4.0);
    }
    let mut best = (f64::MIN, 0u8, false);
    for t in 0..12 {
        for minor in [false, true] {
            let p = if minor { &MIN } else { &MAJ };
            let s: f64 = (0..12).map(|q| h[q] * p[(q + 12 - t) % 12]).sum();
            if s > best.0 {
                best = (s, t as u8, minor);
            }
        }
    }
    (best.1, best.2)
}

fn in_key(k: i32, tonic: u8, minor: bool) -> bool {
    const MAJ: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
    const MIN: [u8; 7] = [0, 2, 3, 5, 7, 8, 10];
    let pc = ((k - tonic as i32).rem_euclid(12)) as u8;
    if minor { MIN.contains(&pc) } else { MAJ.contains(&pc) }
}

/// the next key-tone up (dir 1) or down (dir -1) from `k`
fn step(k: i32, dir: i32, tonic: u8, minor: bool) -> i32 {
    let mut x = k + dir;
    for _ in 0..3 {
        if in_key(x, tonic, minor) {
            return x;
        }
        x += dir;
    }
    k + dir * 2
}

const NAMES: [&str; 12] = ["C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"];

/// The decorated notes of `src` (sorted by start), within `len` beats.
pub fn decorate(src: &[NoteDoc], len: f64, amount: f64, seed: u64) -> (Vec<NoteDoc>, String) {
    let (tonic, minor) = key_of(src);
    let mut notes: Vec<NoteDoc> = src.to_vec();
    notes.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.key.cmp(&b.key)));
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let a = amount.clamp(0.0, 1.0);
    let mut out: Vec<NoteDoc> = Vec::with_capacity(notes.len() * 2);
    let mut i = 0;
    while i < notes.len() {
        // the notes that start together: one is melody, several are a chord
        let mut j = i + 1;
        while j < notes.len() && (notes[j].start - notes[i].start).abs() < 0.04 {
            j += 1;
        }
        let group = &notes[i..j];
        let next_start = notes.get(j).map(|n| n.start).unwrap_or(len);
        if group.len() >= 3 {
            // a chord: rolled upward, the root doubled an octave up
            let roll = rng.next() < a;
            for (k, n) in group.iter().enumerate() {
                let mut m = n.clone();
                if roll {
                    let d = 0.03 * k as f64;
                    m.start += d;
                    m.len = (m.len - d).max(0.05);
                }
                out.push(m);
            }
            if rng.next() < a * 0.6 {
                let root = group.iter().min_by_key(|n| n.key).unwrap();
                if root.key <= 115 && !group.iter().any(|n| n.key == root.key + 12) {
                    let mut o = root.clone();
                    o.key += 12;
                    o.vel = (o.vel * 0.7).max(0.05);
                    out.push(o);
                }
            }
        } else {
            let top = group.iter().max_by_key(|n| n.key).unwrap();
            for n in group {
                out.push(n.clone());
            }
            // a long melody note: a passing tone toward the next note, or a neighbour turn
            let room = (next_start - top.start).min(top.len);
            if room >= 1.0 && rng.next() < a {
                let next_key = notes.get(j).map(|n| n.key as i32);
                let k0 = top.key as i32;
                let (mut nk, at) = match next_key {
                    Some(nk) if (nk - k0).abs() >= 3 => (step(k0, if nk > k0 { 1 } else { -1 }, tonic, minor), room * 0.5),
                    _ => (step(k0, if rng.next() < 0.5 { 1 } else { -1 }, tonic, minor), room * 0.5),
                };
                nk = nk.clamp(0, 127);
                // the held note gives up its second half to the ornament (and comes back for a neighbour turn)
                if let Some(held) = out.iter_mut().rev().find(|m| m.start == top.start && m.key == top.key) {
                    held.len = at;
                }
                let mut orn = top.clone();
                orn.start = top.start + at;
                orn.key = nk as u8;
                orn.len = if next_key.is_some_and(|x| (x - k0).abs() >= 3) { room - at } else { (room - at) * 0.5 };
                orn.vel = (top.vel * 0.85).max(0.05);
                let neighbour = !next_key.is_some_and(|x| (x - k0).abs() >= 3);
                out.push(orn.clone());
                if neighbour && rng.next() < 0.7 {
                    let mut back = top.clone();
                    back.start = orn.start + orn.len;
                    back.len = room - at - orn.len;
                    if back.len > 0.05 {
                        out.push(back);
                    }
                }
            }
        }
        i = j;
    }
    out.retain(|n| n.start < len && n.len > 0.01);
    out.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.key.cmp(&b.key)));
    (out, alloc::format!("{} {}", NAMES[tonic as usize], if minor { "minor" } else { "major" }))
}

pub(super) fn dispatch(name: &[u8], args: &[u64], d: &mut Daw) -> Option<u64> {
    if !name.starts_with(b"daw_deco_") {
        return None;
    }
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).unwrap_or(0.0);
    let u = |i: usize| n(i).max(0.0) as usize;
    Some(match name {
        b"daw_deco_clip" => {
            // (track, clip, amount, seed) -> notes after
            let (t, c, amount, seed) = (u(0), u(1), n(2), n(3).max(0.0) as u64);
            let Some(clip) = d.project.tracks.get_mut(t).and_then(|tr| tr.clips.get_mut(c)) else { return Some(num(0.0)) };
            if clip.is_audio() || clip.midi_notes.is_empty() {
                return Some(num(0.0));
            }
            let (notes, key) = decorate(&clip.midi_notes, clip.len, amount, seed);
            let count = notes.len();
            clip.midi_notes = notes;
            d.status = alloc::format!("Decorated: {count} notes, in {key}");
            d.dirty = true;
            d.rev += 1;
            num(count as f64)
        },
        b"daw_deco_variations" => {
            // (track, clip, count, amount, seed) -> clips added: the theme, then variations that grow, one clip each after it
            let (t, c, count, amount, seed) = (u(0), u(1), u(2).clamp(2, 7), n(3), n(4).max(0.0) as u64);
            let Some(tr) = d.project.tracks.get_mut(t) else { return Some(num(0.0)) };
            let Some(theme) = tr.clips.get(c).cloned() else { return Some(num(0.0)) };
            if theme.is_audio() || theme.midi_notes.is_empty() {
                return Some(num(0.0));
            }
            let mut made = 0;
            for v in 1..count {
                let amt = (amount * (0.5 + v as f64 / count as f64)).min(1.0);
                let (notes, _) = decorate(&theme.midi_notes, theme.len, amt, seed.wrapping_add(v as u64 * 7919));
                let mut nc = theme.clone();
                nc.start = theme.start + theme.len * v as f64;
                nc.midi_notes = notes;
                nc.name = alloc::format!("{} var {v}", theme.name);
                tr.clips.push(nc);
                made += 1;
            }
            d.status = alloc::format!("Theme and {made} variations");
            d.dirty = true;
            d.rev += 1;
            num(made as f64)
        },
        _ => return None,
    })
}
