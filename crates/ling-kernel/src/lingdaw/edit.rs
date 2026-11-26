//! LingStationX editing on LingOS, the parts beyond the plain reads/sets of
//! `lingdaw.rs`, with ling-daw `api.rs` semantics:
//!
//! * **undo / redo** — whole-project JSON snapshots exactly as on the PC: the
//!   app calls `daw_undo_push` after an edit; the snapshot before it goes on the
//!   undo stack (100 deep) when the project changed,
//! * **clips** — `daw_clip_add_midi`, `_add_audio`, `_split`, `_set_notes`,
//!   `_trim_left`, `_to_track`, `_file`, `_wave`, `_peaks` and the clip
//!   clipboard (`daw_clipboard_copy/count/info/paste`, real clip data, so a cut
//!   and paste survives edits),
//! * **live notes** — `daw_live` (the piano roll's keyboard, the tracker's
//!   note entry): the track's instrument sounds while the key is held,
//! * the small reads the views use: `daw_sample_rate`, `daw_bpm_at`,
//!   `daw_tempo_clear`, `daw_limiter_gr`.

use alloc::string::String;
use alloc::vec::Vec;

use super::{ClipDoc, Daw, NoteDoc, ProjectDoc, SR};
use crate::lingrt::{boolean, is_list, list_from, list_items, mk_str, num, to_f64, to_string, UNIT};

struct Undo {
    undo: Vec<String>,
    redo: Vec<String>,
    last: String,
    clipboard: Vec<(usize, ClipDoc)>,
}

static mut UNDO: Undo = Undo { undo: Vec::new(), redo: Vec::new(), last: String::new(), clipboard: Vec::new() };

fn st() -> &'static mut Undo {
    unsafe { &mut *(&raw mut UNDO) }
}

fn snap(p: &ProjectDoc) -> String {
    serde_json::to_string(p).unwrap_or_default()
}

/// How deep the undo and redo stacks are (for `daw state`).
pub(super) fn depth() -> (usize, usize) {
    (st().undo.len(), st().redo.len())
}

/// A project was opened or a new one started: the history starts again from it.
pub(super) fn reset(d: &Daw) {
    let u = st();
    u.undo.clear();
    u.redo.clear();
    u.last = snap(&d.project);
}

fn restore(d: &mut Daw, s: &str) -> bool {
    let Ok(doc) = serde_json::from_str::<ProjectDoc>(s) else { return false };
    d.project = doc.normalised();
    d.dirty = true;
    d.rev += 1;
    true
}

/// A key held from `daw_live`: (track, key).
static mut LIVE: Vec<(usize, u8)> = Vec::new();

// ── recording what is played (daw_rec_arm + daw_live while playing) ─────────
static mut REC_ARM: Option<usize> = None;
/// the clip being recorded into (track, clip) and the keys held: (key, start beat, velocity)
static mut REC_CLIP: Option<(usize, usize)> = None;
static mut REC_HELD: Vec<(u8, f64, f32)> = Vec::new();

fn record_note(d: &mut Daw, track: usize, key: u8, vel: f32, on: bool) {
    let pos = d.position;
    let held = unsafe { &mut *(&raw mut REC_HELD) };
    if unsafe { REC_CLIP }.is_none() {
        if !on {
            return;
        }
        let start = libm::floor(pos / 4.0) * 4.0;
        let Some(t) = d.project.tracks.get_mut(track) else { return };
        t.clips.push(ClipDoc { name: String::from("Recording"), start, len: 4.0, ..ClipDoc::default() });
        unsafe { REC_CLIP = Some((track, t.clips.len() - 1)) };
        d.rev += 1;
    }
    let Some((rt, rc)) = (unsafe { REC_CLIP }) else { return };
    if rt != track {
        return;
    }
    if on {
        held.push((key, pos, vel));
    } else if let Some(i) = held.iter().position(|h| h.0 == key) {
        let (k, start, v) = held.remove(i);
        push_recorded(d, rt, rc, k, start, pos, v);
    }
}

fn push_recorded(d: &mut Daw, t: usize, c: usize, key: u8, start: f64, end: f64, vel: f32) {
    if let Some(clip) = d.project.tracks.get_mut(t).and_then(|t| t.clips.get_mut(c)) {
        let rel = (start - clip.start).max(0.0);
        let len = (end - start).max(0.06);
        clip.midi_notes.push(NoteDoc::new(rel, len, key, vel));
        let need = libm::ceil((rel + len) / 4.0) * 4.0;
        if need > clip.len {
            clip.len = need;
        }
    }
    d.dirty = true;
    d.rev += 1;
}

pub(super) fn finish_recording(d: &mut Daw) {
    let pos = d.position;
    if let Some((t, c)) = unsafe { REC_CLIP.take() } {
        let held = core::mem::take(unsafe { &mut *(&raw mut REC_HELD) });
        for (k, s, v) in held {
            push_recorded(d, t, c, k, s, pos, v);
        }
    }
    unsafe { (*(&raw mut REC_HELD)).clear() };
}

// ── per-note expression (`daw_nx_*`, ling-daw api/noteexpr_api.rs) ───────────
// The curves live on the project's notes (bend / press / timbre, saved as on the PC); the edits are ling-daw's
// gesture.rs (generated into ling-fxcore), run on copies of the clip's notes and copied back.

fn to_fx(x: &NoteDoc) -> ling_fxcore::model::Note {
    ling_fxcore::model::Note {
        start: x.start,
        len: x.len,
        key: x.key,
        vel: x.vel,
        pan: x.pan,
        cutoff: x.cutoff,
        reso: x.reso,
        lyric: x.lyric.clone(),
        bend: x.bend.clone(),
        press: x.press.clone(),
        timbre: x.timbre.clone(),
    }
}

fn from_fx(x: &mut NoteDoc, f: &ling_fxcore::model::Note) {
    x.start = f.start;
    x.len = f.len;
    x.vel = f.vel;
    x.bend = f.bend.clone();
    x.press = f.press.clone();
    x.timbre = f.timbre.clone();
}

/// ling-daw `Note::cut` on a project note: keep `a..b` beats of it, the curves keep what sounded there.
fn cut_note(x: &mut NoteDoc, a: f64, b: f64) {
    let mut f = to_fx(x);
    f.cut(a, b);
    from_fx(x, &f);
}

fn which_of(s: &str) -> usize {
    match s {
        "press" | "pressure" | "at" | "aftertouch" => 1,
        "timbre" | "slide" | "y" => 2,
        _ => 0,
    }
}

fn curve(f: &mut ling_fxcore::model::Note, w: usize) -> &mut Vec<(f32, f32)> {
    match w {
        1 => &mut f.press,
        2 => &mut f.timbre,
        _ => &mut f.bend,
    }
}

fn nums(args: &[u64], i: usize) -> Vec<f64> {
    match args.get(i) {
        Some(&v) if is_list(v) => list_items(v).iter().map(|&x| to_f64(x)).collect(),
        Some(&v) if crate::lingrt::is_num(v) => alloc::vec![to_f64(v)],
        _ => Vec::new(),
    }
}

fn nx_dispatch(name: &[u8], args: &[u64], d: &mut Daw) -> Option<u64> {
    use ling_fxcore::gesture;
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).unwrap_or(0.0);
    let u = |i: usize| n(i).max(0.0) as usize;
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let (t, c) = (u(0), u(1));
    let clip = d.project.tracks.get_mut(t).and_then(|x| x.clips.get_mut(c)).filter(|c| !c.is_audio());
    let Some(clip) = clip else {
        return Some(match name {
            b"daw_nx_flags" | b"daw_nx_get" => list_from(Vec::new()),
            b"daw_nx_text" => mk_str(""),
            b"daw_nx_shapes" => list_from(gesture::SHAPES.iter().map(|x| mk_str(x)).collect()),
            _ => boolean(false),
        });
    };
    let mut fx: Vec<ling_fxcore::model::Note> = clip.midi_notes.iter().map(to_fx).collect();
    let mut changed = true;
    let r = match name {
        b"daw_nx_shapes" => {
            changed = false;
            list_from(gesture::SHAPES.iter().map(|x| mk_str(x)).collect())
        },
        b"daw_nx_flags" => {
            changed = false;
            list_from(fx.iter().map(|x| num((!x.bend.is_empty()) as u8 as f64 + 2.0 * (!x.press.is_empty()) as u8 as f64 + 4.0 * (!x.timbre.is_empty()) as u8 as f64)).collect())
        },
        b"daw_nx_get" => {
            changed = false;
            let w = which_of(&s(2));
            let mut v = Vec::new();
            for x in fx.iter_mut() {
                let cv = curve(x, w);
                v.push(num(cv.len() as f64));
                for p in cv.iter() {
                    v.push(num(p.0 as f64));
                    v.push(num(p.1 as f64));
                }
            }
            list_from(v)
        },
        b"daw_nx_at" => {
            changed = false;
            let w = which_of(&s(3));
            let rest = if w == 2 { 0.5 } else { 0.0 };
            match fx.get_mut(u(2)) {
                Some(x) => {
                    let cv = curve(x, w);
                    num(if cv.is_empty() { rest } else { gesture::at(cv, n(4) as f32) as f64 })
                },
                None => num(rest),
            }
        },
        b"daw_nx_shape" => {
            let (shape, depth) = (s(3).to_lowercase(), n(4) as f32);
            let mut k = 0;
            for i in nums(args, 2) {
                if i >= 0.0 && gesture::apply_shape(&mut fx, i as usize, &shape, depth) {
                    k += 1;
                }
            }
            num(k as f64)
        },
        b"daw_nx_depth" => {
            for i in nums(args, 2) {
                if let Some(x) = fx.get_mut(i.max(0.0) as usize) {
                    x.bend = gesture::with_depth(&x.bend, n(3) as f32);
                }
            }
            boolean(true)
        },
        b"daw_nx_draw" => {
            let w = which_of(&s(3));
            if let Some(x) = fx.get_mut(u(2)) {
                let lim = if w == 0 { gesture::MAX_SEMIS } else { 1.0 };
                let lo = if w == 0 { -lim } else { 0.0 };
                let rest = if w == 2 { 0.5 } else { 0.0 };
                let cv = curve(x, w);
                *cv = gesture::draw(cv, rest, n(4) as f32, (n(5) as f32).clamp(lo, lim), n(6) as f32, (n(7) as f32).clamp(lo, lim));
            }
            boolean(true)
        },
        b"daw_nx_clear" => {
            let all = s(3) == "all";
            let w = which_of(&s(3));
            for i in nums(args, 2) {
                if let Some(x) = fx.get_mut(i.max(0.0) as usize) {
                    if all {
                        x.bend.clear();
                        x.press.clear();
                        x.timbre.clear();
                    } else {
                        curve(x, w).clear();
                    }
                }
            }
            boolean(true)
        },
        b"daw_nx_tidy" => {
            if let Some(x) = fx.get_mut(u(2)) {
                gesture::prune(&mut x.bend, 0.0);
                gesture::prune(&mut x.press, 0.0);
                gesture::prune(&mut x.timbre, 0.5);
            }
            boolean(true)
        },
        b"daw_nx_text" => {
            changed = false;
            mk_str(&fx.get(u(2)).map(|x| if x.has_expr() { alloc::format!("{}|{}|{}", gesture::to_text(&x.bend), gesture::to_text(&x.press), gesture::to_text(&x.timbre)) } else { String::new() }).unwrap_or_default())
        },
        b"daw_nx_set_text" => {
            let text = s(3);
            if let Some(x) = fx.get_mut(u(2)) {
                let mut parts = text.split('|');
                x.bend = gesture::from_text(parts.next().unwrap_or(""));
                x.press = gesture::from_text(parts.next().unwrap_or(""));
                x.timbre = gesture::from_text(parts.next().unwrap_or(""));
            }
            boolean(true)
        },
        b"daw_nx_vel_set" => {
            for (i, v) in nums(args, 2).iter().zip(nums(args, 3).iter()) {
                if let Some(x) = fx.get_mut(i.max(0.0) as usize) {
                    x.vel = (*v as f32).clamp(0.02, 1.0);
                }
            }
            boolean(true)
        },
        b"daw_nx_vel_line" => {
            let (idx, old, line) = (nums(args, 2), nums(args, 3), nums(args, 4));
            let o: Vec<f32> = old.iter().map(|x| *x as f32).collect();
            let l: Vec<f32> = line.iter().map(|x| *x as f32).collect();
            let new = gesture::velocity_line(&o, &l, n(5) as f32, n(6) > 0.5);
            for (i, v) in idx.iter().zip(new.iter()) {
                if let Some(x) = fx.get_mut(i.max(0.0) as usize) {
                    x.vel = *v;
                }
            }
            list_from(new.iter().map(|v| num(*v as f64)).collect())
        },
        _ => return None,
    };
    if changed {
        for (x, f) in clip.midi_notes.iter_mut().zip(fx.iter()) {
            from_fx(x, f);
        }
        d.dirty = true;
        d.rev += 1;
    }
    Some(r)
}

pub(super) fn dispatch(name: &[u8], args: &[u64], d: &mut Daw) -> Option<u64> {
    if name.starts_with(b"daw_nx_") {
        return nx_dispatch(name, args, d);
    }
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).unwrap_or(0.0);
    let u = |i: usize| n(i).max(0.0) as usize;
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let touch = |d: &mut Daw| {
        d.dirty = true;
        d.rev += 1;
    };
    Some(match name {
        // ── undo / redo ──
        b"daw_undo_push" => {
            let u = st();
            let now = snap(&d.project);
            if now != u.last {
                let prev = core::mem::replace(&mut u.last, now);
                u.undo.push(prev);
                if u.undo.len() > 100 {
                    u.undo.remove(0);
                }
                u.redo.clear();
            }
            UNIT
        },
        b"daw_can_undo" => boolean(!st().undo.is_empty()),
        b"daw_can_redo" => boolean(!st().redo.is_empty()),
        b"daw_undo" => {
            let u = st();
            // commit pending edits first so redo comes back to the present
            let now = snap(&d.project);
            if now != u.last {
                let prev = core::mem::replace(&mut u.last, now);
                u.undo.push(prev);
            }
            while let Some(sn) = u.undo.pop() {
                if restore(d, &sn) {
                    let cur = core::mem::replace(&mut u.last, sn);
                    u.redo.push(cur);
                    break;
                }
            }
            UNIT
        },
        b"daw_redo" => {
            let u = st();
            while let Some(sn) = u.redo.pop() {
                if restore(d, &sn) {
                    let cur = core::mem::replace(&mut u.last, sn);
                    u.undo.push(cur);
                    break;
                }
            }
            UNIT
        },

        // ── clips ──
        b"daw_clip_add_midi" => {
            // (track, start, len) -> new clip index
            let Some(t) = d.project.tracks.get_mut(u(0)) else { return Some(num(-1.0)) };
            t.clips.push(ClipDoc { name: String::from("Pattern"), start: n(1).max(0.0), len: n(2).max(0.25), ..ClipDoc::default() });
            let i = t.clips.len() - 1;
            touch(d);
            num(i as f64)
        },
        b"daw_clip_add_audio" => {
            // (track, file, start) -> new clip index, -1 when the file cannot be read
            let path = s(1).replace('\\', "/");
            let Some(secs) = super::rec::sample_seconds(d, &path) else {
                d.status = alloc::format!("Import failed: cannot read {path}");
                return Some(num(-1.0));
            };
            let bpm = d.project.tempo_bpm;
            let Some(t) = d.project.tracks.get_mut(u(0)) else { return Some(num(-1.0)) };
            let base = path.rsplit('/').next().unwrap_or(&path);
            let stem = base.rsplit_once('.').map(|(a, _)| a).unwrap_or(base);
            t.clips.push(ClipDoc { name: String::from(stem), start: n(2).max(0.0), len: (secs * bpm / 60.0).max(0.25), audio_path: path, ..ClipDoc::default() });
            let i = t.clips.len() - 1;
            touch(d);
            num(i as f64)
        },
        b"daw_clip_file" => mk_str(&d.project.tracks.get(u(0)).and_then(|t| t.clips.get(u(1))).map(|c| c.audio_path.clone()).unwrap_or_default()),
        b"daw_clip_split" => {
            // (t, c, beat) -> index of the right half, -1 when the beat is not inside the clip
            let spb = 60.0 / d.project.tempo_bpm;
            let Some(t) = d.project.tracks.get_mut(u(0)) else { return Some(num(-1.0)) };
            let (ci, b) = (u(1), n(2));
            let Some(c) = t.clips.get(ci).cloned() else { return Some(num(-1.0)) };
            if !(b > c.start + 0.01 && b < c.end() - 0.01) {
                return Some(num(-1.0));
            }
            let cut = b - c.start;
            let (mut left, mut right) = (c.clone(), c.clone());
            left.len = cut;
            right.start = b;
            right.len = c.len - cut;
            if c.is_audio() {
                right.offset_s += if c.warp > 1.0 { cut * 60.0 / c.warp as f64 } else { cut * spb };
            } else {
                left.midi_notes.retain(|x| x.start < cut);
                for x in left.midi_notes.iter_mut() {
                    if x.start + x.len > cut {
                        let l = cut - x.start;
                        cut_note(x, 0.0, l);
                    }
                }
                right.midi_notes.retain(|x| x.start + x.len > cut);
                for x in right.midi_notes.iter_mut() {
                    let st = x.start - cut;
                    if st < 0.0 {
                        // a note across the cut: the right half keeps the rest of its curves
                        let l0 = x.len;
                        cut_note(x, -st, l0);
                        x.start = 0.0;
                    } else {
                        x.start = st;
                    }
                }
            }
            right.name = alloc::format!("{} b", c.name);
            t.clips[ci] = left;
            t.clips.push(right);
            let i = t.clips.len() - 1;
            touch(d);
            num(i as f64)
        },
        b"daw_clip_set_notes" => {
            // (t, c, flat [start,len,key,vel,pan,cutoff,reso] * N): every note of a MIDI clip at once
            let flat: Vec<f64> = match args.get(2) {
                Some(&v) if is_list(v) => list_items(v).iter().map(|&x| to_f64(x)).collect(),
                _ => return Some(boolean(false)),
            };
            let mut notes = Vec::with_capacity(flat.len() / 7);
            for ch in flat.chunks(7) {
                if ch.len() < 4 {
                    break;
                }
                let mut x = NoteDoc::new(ch[0].max(0.0), ch[1].max(0.02), libm::round(ch[2]).clamp(0.0, 127.0) as u8, (ch[3] as f32).clamp(0.01, 1.0));
                if ch.len() >= 7 {
                    x.pan = (ch[4] as f32).clamp(-1.0, 1.0);
                    x.cutoff = (ch[5] as f32).clamp(0.0, 1.0);
                    x.reso = (ch[6] as f32).clamp(0.0, 1.0);
                }
                notes.push(x);
            }
            let Some(c) = d.project.tracks.get_mut(u(0)).and_then(|t| t.clips.get_mut(u(1))) else { return Some(boolean(false)) };
            if c.is_audio() {
                return Some(boolean(false));
            }
            c.midi_notes = notes;
            touch(d);
            boolean(true)
        },
        b"daw_clip_trim_left" => {
            // (t, c, new start): the content stays in place, only the clip's start moves
            let spb = 60.0 / d.project.tempo_bpm;
            let Some(c) = d.project.tracks.get_mut(u(0)).and_then(|t| t.clips.get_mut(u(1))) else { return Some(num(0.0)) };
            let (old, end) = (c.start, c.end());
            let mut ns = n(2).clamp(0.0, (end - 0.25).max(0.0));
            if c.is_audio() {
                ns = ns.max(old - c.offset_s / spb);
            }
            let dl = ns - old;
            if libm::fabs(dl) < 1e-9 {
                return Some(num(0.0));
            }
            if c.is_audio() {
                c.offset_s = (c.offset_s + dl * spb).max(0.0);
            } else {
                for x in c.midi_notes.iter_mut() {
                    x.start -= dl;
                }
                if dl > 0.0 {
                    c.midi_notes.retain(|x| x.start + x.len > 1e-9);
                    for x in c.midi_notes.iter_mut() {
                        if x.start < 0.0 {
                            let (s0, l0) = (x.start, x.len);
                            cut_note(x, -s0, l0);
                            x.start = 0.0;
                        }
                    }
                }
            }
            c.start = ns;
            c.len = end - ns;
            touch(d);
            boolean(true)
        },
        b"daw_clip_to_track" => {
            // (t, c, new track) -> the clip's index there, -1 for an audio clip onto an instrument track or back
            let (t, c, nt) = (u(0), u(1), u(2));
            if t == nt {
                return Some(num(c as f64));
            }
            let p = &mut d.project;
            if t >= p.tracks.len() || nt >= p.tracks.len() || c >= p.tracks[t].clips.len() {
                return Some(num(-1.0));
            }
            if p.tracks[t].clips[c].is_audio() != p.tracks[nt].is_audio() {
                return Some(num(-1.0));
            }
            let clip = p.tracks[t].clips.remove(c);
            p.tracks[nt].clips.push(clip);
            let i = p.tracks[nt].clips.len() - 1;
            touch(d);
            num(i as f64)
        },
        b"daw_clipboard_copy" => {
            // (track, clip, 1 = clear first) -> how many clips it holds
            let cb = &mut st().clipboard;
            if n(2) > 0.5 {
                cb.clear();
            }
            if let Some(c) = d.project.tracks.get(u(0)).and_then(|t| t.clips.get(u(1))) {
                cb.push((u(0), c.clone()));
            }
            num(cb.len() as f64)
        },
        b"daw_clipboard_count" => num(st().clipboard.len() as f64),
        b"daw_clipboard_info" => {
            // i -> [track, start, len, is audio]
            let Some((t, c)) = st().clipboard.get(u(0)) else { return Some(list_from(Vec::new())) };
            list_from(alloc::vec![num(*t as f64), num(c.start), num(c.len), boolean(c.is_audio())])
        },
        b"daw_clipboard_paste" => {
            // (i, track, start) -> new clip index, -1 when the kinds differ
            let Some((_, mut clip)) = st().clipboard.get(u(0)).cloned() else { return Some(num(-1.0)) };
            let Some(t) = d.project.tracks.get_mut(u(1)) else { return Some(num(-1.0)) };
            if t.is_audio() != clip.is_audio() {
                return Some(num(-1.0));
            }
            clip.start = n(2).max(0.0);
            t.clips.push(clip);
            let i = t.clips.len() - 1;
            touch(d);
            num(i as f64)
        },
        b"daw_clip_peaks" => {
            // (t, c, bins) -> peak 0..1 per bin over the whole sample (audio clips), [] for MIDI
            let path = d.project.tracks.get(u(0)).and_then(|t| t.clips.get(u(1))).filter(|c| c.is_audio()).map(|c| c.audio_path.clone());
            let bins = u(2).clamp(1, 4096);
            match path {
                Some(p) => list_from(super::rec::peaks(&p, 0.0, 1.0, bins).into_iter().map(|x| num(x as f64)).collect()),
                None => list_from(Vec::new()),
            }
        },
        b"daw_clip_wave" => {
            // (t, c, frac0, frac1, bins): the peaks of part of the sample
            let path = d.project.tracks.get(u(0)).and_then(|t| t.clips.get(u(1))).filter(|c| c.is_audio()).map(|c| c.audio_path.clone());
            let bins = u(4).clamp(1, 4096);
            match path {
                Some(p) => list_from(super::rec::peaks(&p, n(2), n(3), bins).into_iter().map(|x| num(x as f64)).collect()),
                None => list_from(Vec::new()),
            }
        },

        // ── live notes ──
        b"daw_live" => {
            // (track, key, velocity, on)
            let (ti, key, vel, on) = (u(0), n(1).clamp(0.0, 127.0) as u8, n(2).clamp(0.01, 1.0) as f32, n(3) > 0.5);
            let live = unsafe { &mut *(&raw mut LIVE) };
            if on {
                if let Some(t) = d.project.tracks.get(ti) {
                    if !t.is_audio() {
                        super::live_note_on(ti, t, key, vel);
                        live.push((ti, key));
                    }
                }
            } else {
                super::live_note_off(ti, key);
                live.retain(|&(a, b)| !(a == ti && b == key));
            }
            // the armed track records what is played while the transport runs (engine.rs `record_note`)
            if d.playing && unsafe { REC_ARM } == Some(ti) {
                record_note(d, ti, key, vel, on);
            }
            UNIT
        },
        b"daw_rec_arm" => {
            // (track, or -1 to disarm: the notes still held are closed at the playhead)
            let was = unsafe { REC_ARM };
            unsafe { REC_ARM = if n(0) >= 0.0 { Some(u(0)) } else { None } };
            if unsafe { REC_ARM }.is_none() && was.is_some() {
                finish_recording(d);
            }
            UNIT
        },

        // ── small reads ──
        b"daw_sample_rate" => num(SR),
        b"daw_bpm_at" => num(d.project.bpm_at(n(0).max(0.0))),
        b"daw_tempo_clear" => {
            d.project.tempo_map.clear();
            touch(d);
            UNIT
        },
        // the kernel's limiter is a soft clip: no gain reduction to show
        b"daw_limiter_gr" => num(0.0),
        // the audio device as the Settings dialog shows it: the kernel mixer runs at 48 kHz
        b"daw_device_rate" => num(SR),
        b"daw_audio_cfg_get" => num(if s(0) == "rate" { SR } else { 0.0 }),
        b"daw_audio_stats" => num(0.0),
        b"daw_audio_stats_reset" => UNIT,
        b"daw_midi_channel_get" => num(0.0),
        b"daw_midi_current" | b"daw_gm_sf2_get" => mk_str(""),
        b"daw_devices" => list_from(alloc::vec![mk_str("LingOS audio")]),
        b"daw_test_tone" => {
            // a short 440 Hz tone on the first instrument track's sound would need a track; the PC plays left then right:
            // here the kernel mixer's own test tone (Settings > Audio) does that job
            d.status = String::from("Test tone: use Settings > Audio on LingOS");
            UNIT
        },
        _ => return None,
    })
}
