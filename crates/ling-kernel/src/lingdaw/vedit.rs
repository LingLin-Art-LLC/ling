//! The inline vocal editor on LingOS (`daw_ve_*`, plus `daw_vm_scales` / `daw_vm_temperaments` for its menus): ling-daw's
//! `api/vedit_api.rs` over the kernel's project model. A recorded voice is cut into its sung notes ("blobs"); every blob can be
//! moved in pitch, have its drift flattened, be moved and stretched in time and change loudness; Correct pitch (scale +
//! temperament), Quantize, Align to a guide (a MIDI part or another edited vocal), Write MIDI, Compare and Reset work as on the PC.
//!
//! The same storage as the desktop, so a project edited on either opens on the other: the original recording is kept
//! (`ved.<id>.src`), the edit lives in the project meta (`ved.<id>` options, `ved.<id>.blobs`, `ved.<id>.out`) and the clip's tag
//! (`ved:<id>`). A render (vocal.rs `render_segments`, PSOLA: the formants stay) is written next to the project as
//! `audio/vocal_edit_<id>_<rev>.wav` and the clip's `audio_path` points at it, so playback, the arranger's waveform and the
//! offline render all hear the edit. The signal processing and the note math are in [`super::vocal`] (host-tested against
//! ling-daw's code).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use super::vocal::{self, Blob, EditOpts, Geo, Pitch};
use super::{rec, ClipDoc, Daw, NoteDoc, ProjectDoc, TrackDoc, PALETTE};
use crate::lingrt::{boolean, list_from, mk_str, num, to_f64, to_string};

/// renders of one vocal kept on disk (so undo can step back through them)
const KEEP_RENDERS: u64 = 8;
/// ling-daw model.rs MAX_TRACKS
const MAX_TRACKS: usize = 256;

#[derive(Clone, Debug)]
struct Edit {
    src: String,
    blobs: Vec<Blob>,
    vdepth: f64,
    vrate: f64,
    retune: f64,
    temp: usize,
    troot: i32,
    bypass: bool,
    rendered: String,
    rev: u64,
}

/// pitch and level of an original recording, by path (analysing is the slow part, so it is done once per recording)
struct Ana {
    pitch: Pitch,
    level: Vec<f32>,
    top: f32,
}

static mut ANA: Vec<(String, Box<Ana>)> = Vec::new();

fn ana_cache() -> &'static mut Vec<(String, Box<Ana>)> {
    unsafe { &mut *(&raw mut ANA) }
}

fn kv_num(text: &str, key: &str, d: f64) -> f64 {
    text.split(';').find_map(|p| p.split_once('=').filter(|(k, _)| *k == key).and_then(|(_, v)| v.parse().ok())).unwrap_or(d)
}
fn meta_get(p: &ProjectDoc, key: &str) -> Option<String> {
    p.meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}
fn meta_put(p: &mut ProjectDoc, key: &str, v: String) {
    if let Some(e) = p.meta.iter_mut().find(|(k, _)| k == key) {
        e.1 = v;
    } else {
        p.meta.push((String::from(key), v));
    }
}
fn edit_id(c: &ClipDoc) -> Option<String> {
    c.tag.strip_prefix("ved:").map(String::from)
}

fn load_edit(p: &ProjectDoc, id: &str) -> Option<Edit> {
    let opts = meta_get(p, &alloc::format!("ved.{id}"))?;
    let src = meta_get(p, &alloc::format!("ved.{id}.src")).unwrap_or_default();
    let blobs = meta_get(p, &alloc::format!("ved.{id}.blobs"))
        .unwrap_or_default()
        .split('|')
        .filter_map(|b| {
            let v: Vec<f64> = b.split(',').filter_map(|x| x.parse().ok()).collect();
            (v.len() >= 9).then(|| Blob { src_on: v[0], src_off: v[1], dst_on: v[2], dst_off: v[3], midi: v[4], shift: v[5], drift: v[6], gain: v[7], vel: v[8] })
        })
        .collect();
    Some(Edit {
        src,
        blobs,
        vdepth: kv_num(&opts, "vdepth", 0.0),
        vrate: kv_num(&opts, "vrate", 5.5),
        retune: kv_num(&opts, "retune", 12.0),
        temp: kv_num(&opts, "temp", 0.0) as usize,
        troot: kv_num(&opts, "troot", 0.0) as i32,
        bypass: kv_num(&opts, "bypass", 0.0) > 0.5,
        rendered: meta_get(p, &alloc::format!("ved.{id}.out")).unwrap_or_default(),
        rev: kv_num(&opts, "rev", 0.0) as u64,
    })
}

fn save_edit(p: &mut ProjectDoc, id: &str, e: &Edit) {
    let opts = alloc::format!("vdepth={};vrate={};retune={};temp={};troot={};bypass={};rev={}", e.vdepth, e.vrate, e.retune, e.temp, e.troot, e.bypass as u8, e.rev);
    meta_put(p, &alloc::format!("ved.{id}"), opts);
    meta_put(p, &alloc::format!("ved.{id}.src"), e.src.clone());
    meta_put(p, &alloc::format!("ved.{id}.out"), e.rendered.clone());
    let blobs: Vec<String> = e
        .blobs
        .iter()
        .map(|b| alloc::format!("{:.5},{:.5},{:.5},{:.5},{:.4},{:.4},{:.3},{:.4},{:.3}", b.src_on, b.src_off, b.dst_on, b.dst_off, b.midi, b.shift, b.drift, b.gain, b.vel))
        .collect();
    meta_put(p, &alloc::format!("ved.{id}.blobs"), blobs.join("|"));
}

fn geo_of(clip: &ClipDoc, bpm: f64) -> Option<Geo> {
    if !clip.is_audio() {
        return None;
    }
    let spb = if clip.warp > 1.0 { 60.0 / clip.warp as f64 } else { 60.0 / bpm.max(20.0) };
    Some(Geo { offset_s: clip.offset_s, spb, start: clip.start, len: clip.len })
}

/// the notes of a MIDI clip in absolute beats (start, end, key), with the loop unrolled (ling-daw theory_api `expand`)
fn expand(c: &ClipDoc) -> Vec<(f64, f64, u8)> {
    if c.is_audio() {
        return Vec::new();
    }
    let ll = if c.loop_len >= 0.25 && c.loop_len < c.len { c.loop_len } else { 0.0 };
    let mut out = Vec::new();
    for n in &c.midi_notes {
        if ll > 0.0 {
            if n.start >= ll {
                continue;
            }
            let mut s = n.start;
            while s < c.len {
                let len = n.len.min(c.len - s);
                out.push((c.start + s, c.start + s + len, n.key));
                s += ll;
            }
        } else if n.start < c.len {
            let len = n.len.min(c.len - n.start).max(0.03);
            out.push((c.start + n.start, c.start + n.start + len, n.key));
        }
    }
    out
}

fn is_drums(t: &TrackDoc) -> bool {
    t.instrument == "drums" || t.instrument == "drum"
}

fn touch(d: &mut Daw) {
    d.dirty = true;
    d.rev += 1;
}

/// The recording's analysis (cached by path).
fn ve_ana(d: &Daw, path: &str) -> Result<&'static Ana, String> {
    let cache = ana_cache();
    if let Some(i) = cache.iter().position(|(p, _)| p == path) {
        return Ok(&cache[i].1);
    }
    let (stereo, rate) = rec::sample_data(d, path).ok_or_else(|| alloc::format!("cannot read {path}"))?;
    let mono = vocal::mono_of(stereo);
    let pitch = vocal::track_pitch(&mono, rate as f32);
    let level = vocal::level_track(&mono, rate as f32, pitch.hop);
    let top = level.iter().cloned().fold(1e-6, f32::max);
    if cache.len() >= 16 {
        cache.remove(0);
    }
    cache.push((String::from(path), Box::new(Ana { pitch, level, top })));
    Ok(&cache.last().unwrap().1)
}

/// (edit id, edit, clip geometry) of an edited audio clip
fn ve_get(d: &Daw, t: usize, c: usize) -> Option<(String, Edit, Geo)> {
    let clip = d.project.tracks.get(t)?.clips.get(c)?;
    let geo = geo_of(clip, d.project.tempo_bpm)?;
    let id = edit_id(clip)?;
    let e = load_edit(&d.project, &id)?;
    Some((id, e, geo))
}

fn ve_put(d: &mut Daw, id: &str, e: &Edit) {
    save_edit(&mut d.project, id, e);
    touch(d);
}

/// point the clip at `path` (the render or the original)
fn ve_set_audio(d: &mut Daw, t: usize, c: usize, path: &str) {
    if let Some(clip) = d.project.tracks.get_mut(t).and_then(|x| x.clips.get_mut(c)) {
        if clip.is_audio() {
            clip.audio_path = String::from(path);
        }
    }
}

fn ve_render(d: &mut Daw, t: usize, c: usize, id: &str, e: &mut Edit) -> Result<usize, String> {
    let ana = ve_ana(d, &e.src)?;
    if vocal::untouched(&e.blobs, e.vdepth) {
        // nothing to do: the clip plays the recording itself
        e.rendered.clear();
        e.bypass = false;
        ve_set_audio(d, t, c, &e.src.clone());
        return Ok(0);
    }
    let (orig, rate) = rec::sample_data(d, &e.src).ok_or_else(|| alloc::format!("cannot read {}", e.src))?;
    let segs: Vec<vocal::Seg> = e.blobs.iter().map(Blob::seg).collect();
    let opts = EditOpts { vib_depth_cents: e.vdepth.clamp(0.0, 100.0), vib_rate: e.vrate.clamp(2.0, 9.0), retune_ms: e.retune.clamp(1.0, 200.0) };
    let out = vocal::render_segments(orig, rate as f32, &ana.pitch, &segs, &opts);
    let dir = rec::audio_dir(d);
    e.rev += 1;
    let path = alloc::format!("{dir}/vocal_edit_{id}_{}.wav", e.rev);
    let saved = rec::save_audio_file(&path, &rec::wav_f32(&out, rate));
    // older renders of this vocal beyond the last few go (from disk and from memory)
    if e.rev > KEEP_RENDERS {
        let old = alloc::format!("{dir}/vocal_edit_{id}_{}.wav", e.rev - KEEP_RENDERS);
        rec::forget(&old);
        if crate::vfs::exists(&old) {
            crate::vfs::delete(&old);
        }
    }
    rec::put_audio(&path, rate, out);
    if !saved {
        d.status = alloc::format!("Vocal editor: the render plays from memory (no room to save {path})");
    }
    e.rendered = path.clone();
    e.bypass = false;
    ve_set_audio(d, t, c, &path);
    Ok(e.blobs.len())
}

/// The `daw_ve_*` / `daw_vm_*` builtins; None for other names.
pub(super) fn dispatch(name: &[u8], args: &[u64], d: &mut Daw) -> Option<u64> {
    if !(name.starts_with(b"daw_ve_") || name.starts_with(b"daw_vm_")) {
        return None;
    }
    // ling-daw api.rs `A::n` / `A::u`: a missing or non-finite number is 0
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).filter(|x| x.is_finite()).unwrap_or(0.0);
    let u = |i: usize| n(i).max(0.0) as usize;
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let nums = |v: Vec<f64>| list_from(v.into_iter().map(num).collect());
    let empty = || list_from(Vec::new());
    let (t, c) = (u(0), u(1));
    Some(match name {
        b"daw_vm_scales" => list_from(vocal::SCALES.iter().map(|x| mk_str(x.0)).collect()),
        b"daw_vm_temperaments" => list_from(vocal::TEMPERAMENT_NAMES.iter().map(|x| mk_str(x)).collect()),
        b"daw_ve_info" => {
            // (t, c) -> [] (not audio) | [edited 0/1, blobs, offset s, seconds per beat, recording seconds, comparing original 0/1,
            //            vibrato cents, vibrato Hz, glide ms, temperament, tonic, rendered 0/1]
            let Some(clip) = d.project.tracks.get(t).and_then(|x| x.clips.get(c)).cloned() else { return Some(empty()) };
            let Some(g) = geo_of(&clip, d.project.tempo_bpm) else { return Some(empty()) };
            let e = edit_id(&clip).and_then(|id| load_edit(&d.project, &id));
            let secs = rec::sample_seconds(d, &clip.audio_path).unwrap_or(0.0);
            nums(match e {
                Some(e) => alloc::vec![1.0, e.blobs.len() as f64, g.offset_s, g.spb, secs, e.bypass as u8 as f64, e.vdepth, e.vrate, e.retune, e.temp as f64, e.troot as f64, (!e.rendered.is_empty()) as u8 as f64],
                None => alloc::vec![0.0, 0.0, g.offset_s, g.spb, secs, 0.0, 0.0, 5.5, 12.0, 0.0, 0.0, 0.0],
            })
        },
        b"daw_ve_analyze" => {
            // (t, c) -> number of notes found (0 = none; see the status line). Starts the edit over from the original recording.
            let Some(clip) = d.project.tracks.get(t).and_then(|x| x.clips.get(c)) else { return Some(num(0.0)) };
            if !clip.is_audio() {
                d.status = String::from("Vocal editor: that is not an audio clip");
                return Some(num(0.0));
            }
            let old = edit_id(clip).and_then(|id| load_edit(&d.project, &id).map(|e| (id, e.src)));
            let (id, src) = match old {
                Some((id, src)) if !src.is_empty() => (id, src),
                _ => {
                    // the desktop's id is the clock's milliseconds; here also never one a saved project already uses
                    let mut k = crate::arch::timer::now_ms() % 100_000_000;
                    while meta_get(&d.project, &alloc::format!("ved.{k}")).is_some() {
                        k = (k + 1) % 100_000_000;
                    }
                    (alloc::format!("{k}"), clip.audio_path.clone())
                },
            };
            let ana = match ve_ana(d, &src) {
                Ok(a) => a,
                Err(m) => {
                    d.status = alloc::format!("Vocal editor: {m}");
                    return Some(num(0.0));
                },
            };
            let blobs = vocal::blobs_of(&ana.pitch, &ana.level, ana.top);
            if blobs.is_empty() {
                d.status = String::from("Vocal editor: no sung notes found in that clip");
                return Some(num(0.0));
            }
            let nb = blobs.len();
            // a later render continues the numbering, so undo never meets a file that was overwritten
            let rev = meta_get(&d.project, &alloc::format!("ved.{id}")).map(|o| kv_num(&o, "rev", 0.0) as u64).unwrap_or(0);
            let e = Edit { src: src.clone(), blobs, vdepth: 0.0, vrate: 5.5, retune: 12.0, temp: 0, troot: 0, bypass: false, rendered: String::new(), rev };
            if let Some(clip) = d.project.tracks.get_mut(t).and_then(|x| x.clips.get_mut(c)) {
                clip.tag = alloc::format!("ved:{id}");
                clip.audio_path = src;
            }
            save_edit(&mut d.project, &id, &e);
            touch(d);
            d.status = alloc::format!("Vocal editor: {nb} notes");
            num(nb as f64)
        },
        b"daw_ve_blobs" => {
            // (t, c) -> flat [src on, src off, dst on, dst off (s), detected pitch, shift (semitones), drift 0..1, gain, level 0..1] * N
            let Some((_, e, _)) = ve_get(d, t, c) else { return Some(empty()) };
            let mut v = Vec::with_capacity(e.blobs.len() * 9);
            for b in &e.blobs {
                v.extend([b.src_on, b.src_off, b.dst_on, b.dst_off, b.midi, b.shift, b.drift, b.gain, b.vel]);
            }
            nums(v)
        },
        b"daw_ve_curve" => {
            // (t, c, blob, n) -> flat [pitch after the edit (fractional MIDI, -1 = unpitched), level 0..1] * n across the blob
            let Some((_, e, _)) = ve_get(d, t, c) else { return Some(empty()) };
            let Some(b) = e.blobs.get(u(2)).cloned() else { return Some(empty()) };
            let Ok(ana) = ve_ana(d, &e.src) else { return Some(empty()) };
            nums(vocal::curve(&b, &ana.pitch, &ana.level, ana.top, u(3).clamp(2, 400)))
        },
        b"daw_ve_set" => {
            // (t, c, selection "i,j,..", key, value) -> true. Keys: shift (semitones, absolute), dshift (added), drift 0..1, gain
            // (linear), dgain (dB added), move (seconds added to the place, length kept), on / off (one blob's start / end, seconds)
            let Some((id, mut e, _)) = ve_get(d, t, c) else { return Some(boolean(false)) };
            let sel = vocal::sel_of(&s(2), e.blobs.len());
            if !vocal::set_key(&mut e.blobs, &sel, &s(3), n(4)) {
                return Some(boolean(false));
            }
            ve_put(d, &id, &e);
            boolean(true)
        },
        b"daw_ve_split" => {
            // (t, c, blob, time s on the edited time line) -> true when the blob was cut in two there
            let Some((id, mut e, _)) = ve_get(d, t, c) else { return Some(boolean(false)) };
            let (i, at) = (u(2), n(3));
            match e.blobs.get(i) {
                Some(b) if at >= b.dst_on + vocal::MIN_LEN && at <= b.dst_off - vocal::MIN_LEN => {},
                _ => return Some(boolean(false)),
            }
            let Ok(ana) = ve_ana(d, &e.src) else { return Some(boolean(false)) };
            if !vocal::split(&mut e.blobs, &ana.pitch, i, at) {
                return Some(boolean(false));
            }
            ve_put(d, &id, &e);
            boolean(true)
        },
        b"daw_ve_merge" => {
            // (t, c, blob) -> true when the blob and the one after it became one note
            let Some((id, mut e, _)) = ve_get(d, t, c) else { return Some(boolean(false)) };
            if u(2) + 1 >= e.blobs.len() {
                return Some(boolean(false));
            }
            let Ok(ana) = ve_ana(d, &e.src) else { return Some(boolean(false)) };
            if !vocal::merge(&mut e.blobs, &ana.pitch, u(2)) {
                return Some(boolean(false));
            }
            ve_put(d, &id, &e);
            boolean(true)
        },
        b"daw_ve_correct" => {
            // (t, c, selection, centre 0..1, drift 0..1 (-1 = leave), scale index, tonic 0..11) -> notes changed.
            let Some((id, mut e, _)) = ve_get(d, t, c) else { return Some(num(0.0)) };
            let sel = vocal::sel_of(&s(2), e.blobs.len());
            let allowed = vocal::allowed_of(u(5), (n(6) as i64).rem_euclid(12) as i32);
            let (temp, troot) = (e.temp, e.troot);
            let changed = vocal::correct(&mut e.blobs, &sel, n(3).clamp(0.0, 1.0), n(4), &allowed, temp, troot);
            ve_put(d, &id, &e);
            num(changed as f64)
        },
        b"daw_ve_quantize" => {
            // (t, c, selection, grid in beats, strength 0..1) -> notes moved: starts go toward the grid, lengths stay
            let Some((id, mut e, g)) = ve_get(d, t, c) else { return Some(num(0.0)) };
            let sel = vocal::sel_of(&s(2), e.blobs.len());
            let moved = vocal::quantize(&mut e.blobs, &sel, &g, n(3).max(1.0 / 64.0), n(4).clamp(0.0, 1.0));
            ve_put(d, &id, &e);
            num(moved as f64)
        },
        b"daw_ve_guides" => {
            // (t, c) -> [track, name, kind (0 MIDI part, 1 edited vocal)] * N: what this vocal can be aligned to
            let Some(clip) = d.project.tracks.get(t).and_then(|x| x.clips.get(c)) else { return Some(empty()) };
            let (cs, ce) = (clip.start, clip.end());
            let mut v = Vec::new();
            for (ti, tr) in d.project.tracks.iter().enumerate() {
                if ti == t || is_drums(tr) {
                    continue;
                }
                if tr.is_audio() {
                    if tr.clips.iter().any(|k| k.start < ce && k.end() > cs && edit_id(k).is_some()) {
                        v.extend([num(ti as f64), mk_str(&tr.name), num(1.0)]);
                    }
                } else if tr.clips.iter().flat_map(expand).any(|nt| nt.1 > cs && nt.0 < ce) {
                    v.extend([num(ti as f64), mk_str(&tr.name), num(0.0)]);
                }
            }
            list_from(v)
        },
        b"daw_ve_align" => {
            // (t, c, guide track, pitch 0/1, timing 0/1, strength 0..1) -> notes changed, or -1 when the guide has nothing here.
            let Some((id, mut e, g)) = ve_get(d, t, c) else { return Some(num(-1.0)) };
            let (do_pitch, do_time, strength) = (n(3) > 0.5, n(4) > 0.5, n(5).clamp(0.0, 1.0));
            let Some(gt) = d.project.tracks.get(u(2)) else { return Some(num(-1.0)) };
            // guide notes in beats: (start, end, pitch)
            let guide: Vec<(f64, f64, f64)> = if gt.is_audio() {
                let mut v = Vec::new();
                for k in &gt.clips {
                    if let (Some(gid), Some(gg)) = (edit_id(k), geo_of(k, d.project.tempo_bpm)) {
                        if let Some(ge) = load_edit(&d.project, &gid) {
                            v.extend(ge.blobs.iter().map(|b| (gg.beat(b.dst_on), gg.beat(b.dst_off), b.midi + b.shift)).filter(|nt| nt.1 > k.start && nt.0 < k.end()));
                        }
                    }
                }
                v
            } else {
                gt.clips.iter().flat_map(expand).map(|nt| (nt.0, nt.1, nt.2 as f64)).collect()
            };
            let (temp, troot) = (e.temp, e.troot);
            let Some(changed) = vocal::align(&mut e.blobs, &g, guide, do_pitch, do_time, strength, temp, troot) else { return Some(num(-1.0)) };
            ve_put(d, &id, &e);
            num(changed as f64)
        },
        b"daw_ve_opt" => {
            // (t, c, key, value): vdepth (cents), vrate (Hz), retune (glide ms), temp (0..4), troot (0..11)
            let Some((id, mut e, _)) = ve_get(d, t, c) else { return Some(boolean(false)) };
            let v = n(3);
            match s(2).as_str() {
                "vdepth" => e.vdepth = v.clamp(0.0, 100.0),
                "vrate" => e.vrate = v.clamp(2.0, 9.0),
                "retune" => e.retune = v.clamp(1.0, 200.0),
                "temp" => e.temp = (v.max(0.0) as usize).min(vocal::TEMPERAMENTS.len() - 1),
                "troot" => e.troot = (v as i64).rem_euclid(12) as i32,
                _ => return Some(boolean(false)),
            }
            ve_put(d, &id, &e);
            boolean(true)
        },
        b"daw_ve_render" => {
            // (t, c) -> [1, notes, milliseconds] | [0, reason]
            let fail = |m: String| list_from(alloc::vec![num(0.0), mk_str(&m)]);
            let Some((id, mut e, _)) = ve_get(d, t, c) else { return Some(fail(String::from("analyse the vocal first"))) };
            let t0 = crate::arch::timer::now_ms();
            match ve_render(d, t, c, &id, &mut e) {
                Ok(nb) => {
                    ve_put(d, &id, &e);
                    let ms = crate::arch::timer::now_ms().saturating_sub(t0) as f64;
                    if d.status.is_empty() || !d.status.starts_with("Vocal editor: the render plays") {
                        d.status = alloc::format!("Vocal editor: rendered {nb} notes in {ms:.0} ms");
                    }
                    list_from(alloc::vec![num(1.0), num(nb as f64), num(ms)])
                },
                Err(m) => fail(alloc::format!("could not render ({m})")),
            }
        },
        b"daw_ve_compare" => {
            // (t, c, on 0/1) -> true: play the original recording (on) or the edit again (off), the edit is kept
            let Some((id, mut e, _)) = ve_get(d, t, c) else { return Some(boolean(false)) };
            if n(2) > 0.5 {
                if rec::sample_seconds(d, &e.src).is_none() {
                    d.status = alloc::format!("Vocal editor: cannot read {}", e.src);
                    return Some(boolean(false));
                }
                ve_set_audio(d, t, c, &e.src.clone());
                e.bypass = true;
            } else {
                e.bypass = false;
                let back = !e.rendered.is_empty() && rec::sample_seconds(d, &e.rendered).is_some();
                if back {
                    ve_set_audio(d, t, c, &e.rendered.clone());
                } else {
                    let _ = ve_render(d, t, c, &id, &mut e);
                }
            }
            ve_put(d, &id, &e);
            boolean(true)
        },
        b"daw_ve_reset" => {
            // (t, c, selection) -> true: the notes go back to where and how they were sung (every note when the selection is empty)
            let Some((id, mut e, _)) = ve_get(d, t, c) else { return Some(boolean(false)) };
            let sel = vocal::sel_of(&s(2), e.blobs.len());
            vocal::reset(&mut e.blobs, &sel);
            ve_put(d, &id, &e);
            boolean(true)
        },
        b"daw_ve_to_midi" => {
            // (t, c) -> index of a new MIDI track with the notes as edited (pitch rounded, level as velocity), or -1
            let Some((_, e, g)) = ve_get(d, t, c) else { return Some(num(-1.0)) };
            let notes: Vec<NoteDoc> = e
                .blobs
                .iter()
                .filter_map(|b| {
                    let (st, en) = (g.beat(b.dst_on) - g.start, g.beat(b.dst_off) - g.start);
                    (en > 0.0 && st < g.len).then(|| {
                        let key = libm::round(b.midi + b.shift).clamp(0.0, 127.0) as u8;
                        NoteDoc::new(st.max(0.0), (en.min(g.len) - st.max(0.0)).max(0.05), key, (0.35 + 0.65 * (b.vel * b.gain).clamp(0.0, 1.0)) as f32)
                    })
                })
                .collect();
            if notes.is_empty() {
                d.status = String::from("Vocal editor: no notes inside the clip");
                return Some(num(-1.0));
            }
            let n_tr = d.project.tracks.len();
            if n_tr >= MAX_TRACKS {
                d.status = String::from("Vocal editor: no room for another track");
                return Some(num(-1.0));
            }
            let (tname, cname) = {
                let tr = &d.project.tracks[t];
                (tr.name.clone(), tr.clips.get(c).map(|k| k.name.clone()).unwrap_or_default())
            };
            let nn = notes.len();
            let clip = ClipDoc { name: alloc::format!("{cname} notes"), start: g.start, len: g.len, midi_notes: notes, ..ClipDoc::default() };
            d.project.tracks.push(TrackDoc {
                name: alloc::format!("{tname} notes"),
                color: PALETTE[n_tr % 7],
                kind: String::from("instrument"),
                instrument: String::from("synth"),
                patch: String::new(),
                level: 1.0,
                pan: 0.0,
                muted: false,
                solo: false,
                effects: Vec::new(),
                clips: alloc::vec![clip],
                state: Vec::new(),
                config: String::new(),
                session: Vec::new(),
                auto: Vec::new(),
            });
            touch(d);
            d.status = alloc::format!("Vocal editor: {nn} notes written to a new MIDI track");
            num(n_tr as f64)
        },
        _ => return None,
    })
}
