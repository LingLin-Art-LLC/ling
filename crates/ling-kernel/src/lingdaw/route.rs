//! LingStationX's mixer graph and time-based control on LingOS, with ling-daw semantics:
//!
//! * **automation lanes** (`daw_auto_*`): per track and for the master, a lane moves `vol`, `pan`, an instrument knob or an
//!   effect knob (`fx<i>.<p>`, `fx<i>.wet`); while the transport plays the engine reads them every sub-block
//!   ([`automate`]). With Record Automation on, moving one of those by hand writes points at the playhead ([`record`]); a
//!   lane gives way to the hand for 0.4 s after it was touched,
//! * **routes** (`daw_route_*`, `daw_duck_*`): sends (post-fader audio into another track's input), sidechain keys (the
//!   source's level reaches the target's compressor), duck routes (the source's notes trigger the target's Ducker) and
//!   modulator routes (the source's audio is the Vocoder's modulator); the engine renders tracks in the routes' order,
//! * the **Performance launcher** (`daw_perf_*`): scenes of launcher clips per track, launched / stopped on the
//!   launch-quantize grid of the session clock, looping until stopped; scenes from the arrangement (`daw_perf_auto`).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::{audio, audio_start, fx, AutoDoc, Audio, ClipDoc, Daw, NoteDoc, ProjectDoc, RouteDoc, TrackDoc, SR};
use crate::lingrt::{boolean, is_list, list_from, list_items, mk_str, num, to_f64, to_string, UNIT};

// ── engine side ──────────────────────────────────────────────────────────────

/// Tracks in the order the routes need: every source before the tracks it feeds (engine.rs `route_order`).
pub(super) fn order(p: &ProjectDoc) -> Vec<usize> {
    let n = p.tracks.len();
    let mut indeg = alloc::vec![0usize; n];
    for r in &p.routes {
        if r.from < n && r.to < n && r.from != r.to {
            indeg[r.to] += 1;
        }
    }
    let mut order = Vec::with_capacity(n);
    let mut done = alloc::vec![false; n];
    loop {
        let mut progressed = false;
        for i in 0..n {
            if !done[i] && indeg[i] == 0 {
                done[i] = true;
                order.push(i);
                progressed = true;
                for r in &p.routes {
                    if r.from == i && r.to < n && r.from != r.to {
                        indeg[r.to] = indeg[r.to].saturating_sub(1);
                    }
                }
            }
        }
        if !progressed {
            break;
        }
    }
    for i in 0..n {
        if !done[i] {
            order.push(i);
        }
    }
    order
}

/// Is track `t` a source of some route (it renders even when muted: a muted kick can still key a sidechain)?
pub(super) fn is_source(p: &ProjectDoc, t: usize) -> bool {
    p.routes.iter().any(|r| r.from == t && r.to != t && r.to < p.tracks.len())
}

/// The notes of the duck routes' sources starting in this sub-block, per target track: (frame, velocity x amount)
/// (engine.rs `duck_events`).
pub(super) fn duck_events(p: &ProjectDoc, seg: Option<(f64, f64)>, frames: usize, nt: usize) -> Vec<Vec<(u32, f32)>> {
    let mut out: Vec<Vec<(u32, f32)>> = alloc::vec![Vec::new(); nt];
    let Some((p0, p1)) = seg else { return out };
    if frames == 0 || !(p1 > p0) {
        return out;
    }
    let bpf = (p1 - p0) / frames as f64;
    for r in p.routes.iter().filter(|r| r.kind == "duck" && r.from < nt && r.to < nt && r.from != r.to) {
        for c in &p.tracks[r.from].clips {
            if c.is_audio() || p1 <= c.start || p0 >= c.start + c.len {
                continue;
            }
            let ll = if c.loop_len >= 0.25 && c.loop_len < c.len { c.loop_len } else { 0.0 };
            let mut visit = |base: f64, limit: f64| {
                for x in &c.midi_notes {
                    if x.start >= limit {
                        continue;
                    }
                    let ns = base + x.start;
                    if ns >= p0 && ns < p1 && (r.keys.is_empty() || r.keys.contains(&x.key)) {
                        let fr = (libm::floor((ns - p0) / bpf).max(0.0) as usize).min(frames - 1) as u32;
                        out[r.to].push((fr, (x.vel * r.amount).clamp(0.0, 1.0)));
                    }
                }
            };
            if ll > 0.0 {
                let k0 = (libm::floor((p0 - c.start) / ll) as i64).max(0);
                let k1 = (libm::floor((p1 - c.start) / ll) as i64).max(0);
                for k in k0..=k1 {
                    let off = k as f64 * ll;
                    if off >= c.len {
                        break;
                    }
                    visit(c.start + off, ll.min(c.len - off));
                }
            } else {
                visit(c.start, c.len);
            }
        }
    }
    out
}

/// Lanes touched by hand in the last 0.4 s: (track or None for the master, parameter, beat of the last point, time in ms).
static mut TOUCHED: Vec<(Option<usize>, String, f64, u64)> = Vec::new();
static mut AUTO_REC: bool = false;

fn touched() -> &'static mut Vec<(Option<usize>, String, f64, u64)> {
    unsafe { &mut *(&raw mut TOUCHED) }
}

fn held_by_hand(t: Option<usize>, param: &str) -> bool {
    let now = crate::arch::timer::now_ms();
    touched().iter().any(|x| x.0 == t && x.1 == param && now.saturating_sub(x.3) < 400)
}

/// Apply every lane at `beat` (engine.rs `apply_lane`): the mixer gains of this sub-block, the instrument knobs new notes
/// start with, and the effect knobs (onto the running instances; the project keeps the hand-set values).
pub(super) fn automate(a: &mut Audio, p: &ProjectDoc, beat: f64, gains: &mut [(f64, f64)], master: &mut f64) {
    for (ti, t) in p.tracks.iter().enumerate() {
        if t.auto.is_empty() {
            if let Some(v) = a.inst_auto.get_mut(ti) {
                v.clear();
            }
            continue;
        }
        for l in &t.auto {
            if l.points.is_empty() || held_by_hand(Some(ti), &l.param) {
                continue;
            }
            let Some(v) = l.value_at(beat) else { continue };
            match l.param.as_str() {
                "vol" => gains[ti].0 = v.clamp(0.0, 2.0) as f64,
                "pan" => gains[ti].1 = v.clamp(-1.0, 1.0) as f64,
                "bend" => {},
                pn if pn.starts_with("fx") => {
                    if let Some((i, k)) = pn[2..].split_once('.') {
                        if let Ok(fi) = i.parse::<usize>() {
                            fx::set_live(&mut a.racks, ti as i64, fi, k, v);
                        }
                    }
                },
                name => {
                    if let Some(ia) = a.inst_auto.get_mut(ti) {
                        match ia.iter_mut().find(|(n, _)| n == name) {
                            Some(slot) => slot.1 = v.clamp(0.0, 1.0),
                            None => ia.push((String::from(name), v.clamp(0.0, 1.0))),
                        }
                    }
                },
            }
        }
    }
    for l in &p.master_auto {
        if l.points.is_empty() || held_by_hand(None, &l.param) {
            continue;
        }
        let Some(v) = l.value_at(beat) else { continue };
        if l.param == "vol" {
            *master = v.clamp(0.0, 2.0) as f64;
        } else if let Some((i, k)) = l.param.strip_prefix("fx").and_then(|r| r.split_once('.')) {
            if let Ok(fi) = i.parse::<usize>() {
                fx::set_live(&mut a.racks, -1, fi, k, v);
            }
        }
    }
}

/// With Record Automation on and the transport playing, a knob moved by hand writes a point at the playhead
/// (engine.rs `record_auto`): the stretch the hand passed over since its last point is replaced.
pub(super) fn record(d: &mut Daw, track: Option<usize>, param: &str, value: f32) {
    if !(unsafe { AUTO_REC } && d.playing) || !value.is_finite() {
        return;
    }
    let beat = d.position;
    let now = crate::arch::timer::now_ms();
    let mut from = beat;
    match touched().iter_mut().find(|x| x.0 == track && x.1 == param) {
        Some(x) => {
            if beat >= x.2 && beat - x.2 < 2.0 {
                from = x.2;
            }
            x.2 = beat;
            x.3 = now;
        },
        None => touched().push((track, String::from(param), beat, now)),
    }
    let lanes = match track {
        Some(t) => match d.project.tracks.get_mut(t) {
            Some(t) => &mut t.auto,
            None => return,
        },
        None => &mut d.project.master_auto,
    };
    let li = match lanes.iter().position(|l| l.param == param) {
        Some(i) => i,
        None => {
            lanes.push(AutoDoc { param: String::from(param), points: Vec::new() });
            lanes.len() - 1
        },
    };
    let pts = &mut lanes[li].points;
    pts.retain(|p| libm::fabs(p.0 - beat) > 0.04 && !(p.0 > from && p.0 < beat));
    let at = pts.partition_point(|p| p.0 < beat);
    pts.insert(at, (beat, value));
}

// ── recording launches into the arrangement (daw_perf_rec, engine.rs `finish_perf`) ─────
static mut PERF_REC: bool = false;
/// per track: the launcher clip being recorded and the arrangement beat it began at
static mut OPEN: Vec<Option<(usize, f64)>> = Vec::new();
/// finished passes waiting to become arrangement clips: (track, launcher clip, start beat, end beat)
static mut PENDING: Vec<(usize, usize, f64, f64)> = Vec::new();

fn open() -> &'static mut Vec<Option<(usize, f64)>> {
    unsafe { &mut *(&raw mut OPEN) }
}

fn close_pass(t: usize, end: f64) {
    if let Some(Some((ci, start))) = open().get_mut(t).map(|o| o.take()) {
        if end > start + 0.01 {
            unsafe { (*(&raw mut PENDING)).push((t, ci, start, end)) };
        }
    }
}

/// The transport stopped / paused at `end`: every pass being recorded ends there.
pub(super) fn finish_all(end: f64) {
    for t in 0..open().len() {
        close_pass(t, end);
    }
}

/// Turn the finished passes into arrangement clips, one copy per loop of the launcher clip (from the dispatcher: the
/// engine side cannot change the project).
pub(super) fn apply_pending(d: &mut Daw) {
    let pend = core::mem::take(unsafe { &mut *(&raw mut PENDING) });
    if pend.is_empty() {
        return;
    }
    for (t, ci, start, end) in pend {
        let Some(tr) = d.project.tracks.get_mut(t) else { continue };
        let Some(src) = tr.session.get(ci).cloned() else { continue };
        let len = src.len.max(0.25);
        let mut k = 0.0;
        while start + k * len < end - 0.01 {
            let mut c = src.clone();
            c.start = start + k * len;
            c.len = len.min(end - c.start);
            tr.clips.push(c);
            k += 1.0;
        }
    }
    d.dirty = true;
    d.rev += 1;
}

pub(super) fn ensure_sess_len(a: &mut Audio, n: usize) {
    if a.sess_play.len() != n {
        a.sess_play.resize(n, None);
        a.sess_queue.resize(n, None);
    }
}

/// One sub-block of the launcher on the session clock `a.spos` (`db` beats long): queued launches and stops that fall in it
/// take effect, then every playing launcher clip schedules its notes (looping by the clip's length).
pub(super) fn session(a: &mut Audio, p: &ProjectDoc, beat: f64, db: f64) {
    let (s0, s1) = (a.spos, a.spos + db);
    ensure_sess_len(a, p.tracks.len());
    for t in 0..p.tracks.len() {
        if let Some(q) = a.sess_queue[t] {
            if q.2 < s1 {
                // where on the arrangement clock this happens (for recording the pass)
                let at_beat = beat + (q.2 - s0).max(0.0);
                if unsafe { PERF_REC } {
                    if open().len() <= t {
                        open().resize(t + 1, None);
                    }
                    close_pass(t, at_beat);
                    if q.0 >= 0 {
                        open()[t] = Some((q.0 as usize, at_beat));
                    }
                }
                if q.0 >= 0 {
                    a.sess_play[t] = Some((q.0 as usize, q.1, q.2));
                } else {
                    a.sess_play[t] = None;
                    for v in a.voices.iter_mut().filter(|v| v.track == t && v.sess) {
                        v.gate = false;
                    }
                }
                a.sess_queue[t] = None;
            }
        }
    }
    let any_solo = p.tracks.iter().any(|t| t.solo);
    for t in 0..p.tracks.len() {
        let Some((ci, _, start)) = a.sess_play[t] else { continue };
        let tr = &p.tracks[t];
        if tr.is_audio() || ((tr.muted || (any_solo && !tr.solo)) && !is_source(p, t)) {
            continue;
        }
        let Some(c) = tr.session.get(ci) else { continue };
        if c.is_audio() {
            continue;
        }
        let len = c.len.max(0.25);
        let (l0, l1) = ((s0 - start).max(0.0), (s1 - start).max(0.0));
        if !(l1 > l0) {
            continue;
        }
        let drums = tr.instrument == "drums" || tr.instrument == "drum";
        let k0 = (libm::floor((l0 - 1e-6) / len) as i64).max(0);
        let k1 = (libm::floor((l1 + 1e-6) / len) as i64).max(0);
        for k in k0..=k1 {
            let base = start + k as f64 * len;
            for x in &c.midi_notes {
                if x.start >= len {
                    continue;
                }
                let on = base + x.start;
                if on < s0.max(start) || on >= s1 {
                    continue;
                }
                let off_s = (on + x.len.max(0.02)).min(base + len);
                // the voice's note-off is on the arrangement clock, which runs alongside
                super::note_on(a, t, tr, drums, x, beat + (off_s - s0));
                if let Some(v) = a.voices.last_mut() {
                    v.sess = true;
                }
            }
        }
    }
}

fn quant_at(a: &Audio, p: &ProjectDoc, playing: bool) -> f64 {
    let q = p.performance_launch_quantize_beats;
    if !playing || q <= 0.0 {
        a.spos
    } else {
        libm::ceil(a.spos / q - 1e-9).max(0.0) * q
    }
}

/// Launching from a stopped transport starts it (as on the PC).
fn start_transport(d: &mut Daw) {
    if !d.playing {
        d.playing = true;
        d.base_beat = d.position;
        d.base_ms = crate::arch::timer::now_ms();
        audio_start(d.position);
    }
}

// ── auto scenes (perf.rs) ────────────────────────────────────────────────────

fn slice_track(t: &TrackDoc, a: f64, b: f64, bpm: f64) -> Option<ClipDoc> {
    let len = b - a;
    let mut notes: Vec<NoteDoc> = Vec::new();
    let mut audio_c: Option<ClipDoc> = None;
    for c in &t.clips {
        if c.end() <= a || c.start >= b {
            continue;
        }
        if c.is_audio() {
            if audio_c.is_none() && c.start <= a + 0.01 {
                let lead = (a - c.start).max(0.0);
                let spb = if c.warp > 1.0 { 60.0 / c.warp as f64 } else { 60.0 / bpm.max(1.0) };
                let mut y = c.clone();
                y.start = 0.0;
                y.len = len;
                y.loop_len = 0.0;
                y.offset_s = c.offset_s + lead * spb;
                audio_c = Some(y);
            }
            continue;
        }
        for x in &c.midi_notes {
            let s = c.start + x.start;
            let e = s + x.len;
            if s < b && e > a && x.start < c.len {
                let ns = (s - a).max(0.0);
                let ne = (e.min(c.end()).min(b) - a).max(ns + 0.03);
                let mut y = x.clone();
                y.start = ns;
                y.len = ne - ns;
                notes.push(y);
            }
        }
    }
    if audio_c.is_some() {
        return audio_c;
    }
    if notes.is_empty() {
        return None;
    }
    Some(ClipDoc { name: String::from("Slice"), start: 0.0, len, midi_notes: notes, ..ClipDoc::default() })
}

fn auto_scenes(p: &mut ProjectDoc, mode: u8) {
    let end = libm::ceil(p.end_beat()).min(4096.0);
    if end <= 0.0 {
        return;
    }
    let mut cuts: Vec<f64> = alloc::vec![0.0];
    match mode {
        1 | 2 => {
            let step = if mode == 1 { 4.0 } else { 16.0 };
            let mut b = step;
            while b < end {
                cuts.push(b);
                b += step;
            }
        },
        3 => {
            let text = p.meta.iter().find(|(k, _)| k == "sections").map(|(_, v)| v.clone()).unwrap_or_default();
            let mut starts: Vec<f64> = text
                .split(';')
                .filter_map(|part| {
                    let f: Vec<&str> = part.split(':').collect();
                    if f.len() == 3 && f[2] != "fill" {
                        f[0].parse::<f64>().ok()
                    } else {
                        None
                    }
                })
                .collect();
            starts.sort_by(|a, b| a.total_cmp(b));
            for x in starts {
                let x = libm::round(x / 4.0) * 4.0;
                if x > *cuts.last().unwrap() + 7.9 && x < end - 3.9 {
                    cuts.push(x);
                }
            }
        },
        _ => {
            let mut pts: Vec<f64> = Vec::new();
            for t in &p.tracks {
                for c in &t.clips {
                    pts.push(c.start);
                    pts.push(c.end());
                }
            }
            pts.retain(|x| x.is_finite());
            pts.sort_by(|a, b| a.total_cmp(b));
            for x in pts {
                let x = libm::round(x / 4.0) * 4.0;
                if x > *cuts.last().unwrap() + 3.9 && x < end - 3.9 {
                    cuts.push(x);
                }
            }
        },
    }
    cuts.push(end);
    for t in p.tracks.iter_mut() {
        t.session.clear();
    }
    p.slots.clear();
    p.scenes.clear();
    let bpm = p.tempo_bpm;
    for (si, w) in cuts.windows(2).enumerate() {
        let (a, b) = (w[0], w[1]);
        p.scenes.push(alloc::format!("Scene {}", si + 1));
        for ti in 0..p.tracks.len() {
            if let Some(c) = slice_track(&p.tracks[ti], a, b, bpm) {
                p.tracks[ti].session.push(c);
                let idx = (p.tracks[ti].session.len() - 1) as i32;
                p.set_slot(si, ti, idx);
            }
        }
    }
}

// ── automation shapes (api.rs `auto_shape_points`) ──────────────────────────

fn shape_points(shape: &str, b0: f64, b1: f64, period: f64, lo: f32, hi: f32, seed: u32) -> Vec<(f64, f32)> {
    let mut out: Vec<(f64, f32)> = Vec::new();
    let span = hi - lo;
    let mut rng = seed.wrapping_mul(2654435761).wrapping_add(12345);
    let mut rnd = move || {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        (rng & 0xffff) as f32 / 65535.0
    };
    let mut t = b0;
    while t < b1 - 1e-9 && out.len() < 6000 {
        let end = (t + period).min(b1);
        let full = (end - t) >= period - 1e-9;
        let at = |f: f64| t + (end - t) * f;
        match shape {
            "sine" => {
                for k in 0..=8 {
                    let f = k as f64 / 8.0;
                    out.push((at(f), lo + span * (0.5 - 0.5 * libm::cos(core::f64::consts::TAU * f)) as f32));
                }
            },
            "tri" => {
                out.push((at(0.0), lo));
                out.push((at(0.5), hi));
                out.push((at(1.0), lo));
            },
            "up" => {
                out.push((at(0.0), lo));
                out.push((at(1.0) - 0.01, hi));
            },
            "down" => {
                out.push((at(0.0), hi));
                out.push((at(1.0) - 0.01, lo));
            },
            "pulse" => {
                out.push((at(0.0), hi));
                out.push((at(0.5) - 0.01, hi));
                out.push((at(0.5), lo));
                out.push((at(1.0) - 0.01, lo));
            },
            "steps" => {
                for k in 0..8 {
                    let v = lo + span * k as f32 / 7.0;
                    out.push((at(k as f64 / 8.0), v));
                    out.push((at((k + 1) as f64 / 8.0) - 0.01, v));
                }
            },
            _ => {
                for k in 0..4 {
                    let v = lo + span * rnd();
                    out.push((at(k as f64 / 4.0), v));
                    out.push((at((k + 1) as f64 / 4.0) - 0.01, v));
                }
            },
        }
        if !full {
            break;
        }
        t += period;
    }
    out.retain(|p| p.0 >= b0 - 1e-9 && p.0 <= b1 + 1e-9);
    out.dedup_by(|a, b| libm::fabs(a.0 - b.0) < 1e-6);
    out
}

/// May a lane on this track (None = the master) move `param` (api.rs `daw_auto_lane_add` / `_set_param`)?
fn lane_valid(p: &ProjectDoc, t: Option<usize>, param: &str, more: bool) -> bool {
    let fx_ok = |effects: &Vec<super::FxDoc>| {
        param.strip_prefix("fx").and_then(|r| r.split_once('.')).is_some_and(|(i, k)| {
            i.parse::<usize>().ok().and_then(|i| effects.get(i)).is_some_and(|e| k == "wet" || k.parse::<usize>().is_ok_and(|k| k < e.params.len()))
        })
    };
    match t {
        None => param == "vol" || fx_ok(&p.master_effects),
        Some(t) => match p.tracks.get(t) {
            None => false,
            Some(tr) => {
                param == "vol"
                    || param == "pan"
                    || (more && (param == "bend" || param.starts_with("fl:")))
                    || (!tr.is_audio() && tr.params().iter().any(|(n, _)| n == param))
                    || fx_ok(&tr.effects)
            },
        },
    }
}

// ── duck helpers (api/duck_api.rs) ───────────────────────────────────────────

fn parse_keys(s: &str) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for part in s.split(|c: char| c == ',' || c == ' ' || c == ';').filter(|p| !p.is_empty()) {
        let nums: Vec<u8> = part.split('-').filter_map(|x| x.trim().parse::<u8>().ok()).collect();
        match nums.as_slice() {
            [a] if *a < 128 => out.push(*a),
            [a, b] if *a < 128 && *b < 128 => out.extend(*a.min(b)..=*a.max(b)),
            _ => {},
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn keys_string(keys: &[u8]) -> String {
    let mut s = String::new();
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&k.to_string());
    }
    s
}

fn depth_n(db: f64) -> f32 {
    (db / 30.0).clamp(0.0, 1.0) as f32
}
fn attack_n(ms: f64) -> f32 {
    libm::sqrt(((ms / 1000.0 - 0.0005) / 0.04).clamp(0.0, 1.0)) as f32
}
fn hold_n(ms: f64) -> f32 {
    (ms / 200.0).clamp(0.0, 1.0) as f32
}
fn release_n(ms: f64) -> f32 {
    libm::sqrt(((ms / 1000.0 - 0.03) / 1.17).clamp(0.0, 1.0)) as f32
}

fn ducker_index(t: &TrackDoc) -> Option<usize> {
    t.effects.iter().position(|e| e.name == "Ducker")
}

fn note_count(t: &TrackDoc, keys: &[u8]) -> usize {
    t.clips
        .iter()
        .filter(|c| !c.is_audio())
        .map(|c| {
            let per = c.midi_notes.iter().filter(|x| keys.is_empty() || keys.contains(&x.key)).count();
            let reps = if c.loop_len >= 0.25 && c.loop_len < c.len { libm::ceil(c.len / c.loop_len) as usize } else { 1 };
            per * reps.min(512)
        })
        .sum()
}

// ── builtins ─────────────────────────────────────────────────────────────────

fn lanes_mut(p: &mut ProjectDoc, t: f64) -> Option<&mut Vec<AutoDoc>> {
    if t < 0.0 {
        Some(&mut p.master_auto)
    } else {
        p.tracks.get_mut(t as usize).map(|t| &mut t.auto)
    }
}

pub(super) fn dispatch(name: &[u8], args: &[u64], d: &mut Daw) -> Option<u64> {
    if !(name.starts_with(b"daw_auto_") || name.starts_with(b"daw_route_") || name.starts_with(b"daw_duck_") || name.starts_with(b"daw_perf_")) {
        return None;
    }
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).unwrap_or(0.0);
    let u = |i: usize| n(i).max(0.0) as usize;
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let touch = |d: &mut Daw| {
        d.dirty = true;
        d.rev += 1;
    };
    let nt = d.project.tracks.len();
    Some(match name {
        // ── automation ──
        b"daw_auto_rec" => {
            unsafe { AUTO_REC = n(0) > 0.5 };
            UNIT
        },
        b"daw_auto_lanes" => {
            let p = &mut d.project;
            list_from(lanes_mut(p, n(0)).map(|l| l.iter().map(|x| mk_str(&x.param)).collect()).unwrap_or_default())
        },
        b"daw_auto_lane_add" => {
            let param = s(1);
            let t = if n(0) < 0.0 { None } else { Some(u(0)) };
            if !lane_valid(&d.project, t, &param, false) {
                return Some(num(-1.0));
            }
            let Some(l) = lanes_mut(&mut d.project, n(0)) else { return Some(num(-1.0)) };
            if let Some(i) = l.iter().position(|x| x.param == param) {
                return Some(num(i as f64));
            }
            l.push(AutoDoc { param, points: Vec::new() });
            let i = l.len() - 1;
            touch(d);
            num(i as f64)
        },
        b"daw_auto_lane_set_param" => {
            let param = s(2);
            if !lane_valid(&d.project, Some(u(0)), &param, true) {
                return Some(boolean(false));
            }
            let li = u(1);
            let Some(t) = d.project.tracks.get_mut(u(0)) else { return Some(boolean(false)) };
            if li >= t.auto.len() {
                return Some(boolean(false));
            }
            t.auto[li].param = param.clone();
            if let Some(j) = (0..t.auto.len()).find(|&j| j != li && t.auto[j].param == param) {
                t.auto.remove(j);
            }
            touch(d);
            boolean(true)
        },
        b"daw_auto_lane_remove" => {
            if let Some(l) = lanes_mut(&mut d.project, n(0)) {
                if u(1) < l.len() {
                    l.remove(u(1));
                }
            }
            touch(d);
            UNIT
        },
        b"daw_auto_points" => {
            let p = &mut d.project;
            let pts = lanes_mut(p, n(0)).and_then(|l| l.get(u(1))).map(|l| l.points.clone()).unwrap_or_default();
            list_from(pts.iter().flat_map(|(b, v)| [num(*b), num(*v as f64)]).collect())
        },
        b"daw_auto_points_range" => {
            // (target, lane, from, to) -> [index of the first point, beat, value, ...]: the points inside plus a neighbour each side
            let (b0, b1) = (n(2), n(3));
            let p = &mut d.project;
            let mut v = alloc::vec![num(0.0)];
            if let Some(l) = lanes_mut(p, n(0)).and_then(|l| l.get(u(1))) {
                let pts = &l.points;
                let lo = pts.partition_point(|x| x.0 < b0).saturating_sub(1);
                let hi = (pts.partition_point(|x| x.0 <= b1) + 1).min(pts.len());
                v[0] = num(lo as f64);
                for (b, x) in pts.iter().take(hi).skip(lo) {
                    v.push(num(*b));
                    v.push(num(*x as f64));
                }
            }
            list_from(v)
        },
        b"daw_auto_lane_empty" => {
            let p = &mut d.project;
            boolean(lanes_mut(p, n(0)).and_then(|l| l.get(u(1))).is_none_or(|l| l.points.is_empty()))
        },
        b"daw_auto_set_points" => {
            // (target, lane, flat [beat, value, ...])
            let flat: Vec<f64> = match args.get(2) {
                Some(&v) if is_list(v) => list_items(v).iter().map(|&x| to_f64(x)).collect(),
                _ => return Some(UNIT),
            };
            let mut pts: Vec<(f64, f32)> = flat.chunks_exact(2).map(|c| (c[0], c[1] as f32)).filter(|p| p.0.is_finite() && p.1.is_finite()).collect();
            pts.sort_by(|x, y| x.0.total_cmp(&y.0));
            if let Some(l) = lanes_mut(&mut d.project, n(0)).and_then(|l| l.get_mut(u(1))) {
                l.points = pts;
            }
            touch(d);
            UNIT
        },
        b"daw_auto_shape" => {
            // (target, lane, shape, start, end, period, lo, hi, seed): a repeating shape replaces the lane inside the range
            let (b0, b1) = (n(3).max(0.0), n(4));
            if !(b1 > b0) || !b1.is_finite() {
                return Some(num(0.0));
            }
            let pts = shape_points(&s(2), b0, b1.min(b0 + 4096.0), n(5).max(0.125), n(6).clamp(0.0, 1.0) as f32, n(7).clamp(0.0, 1.0) as f32, n(8) as u32);
            let k = pts.len();
            if let Some(l) = lanes_mut(&mut d.project, n(0)).and_then(|l| l.get_mut(u(1))) {
                l.points.retain(|p| p.0 < b0 - 1e-9 || p.0 > b1 + 1e-9);
                l.points.extend(pts);
                l.points.sort_by(|x, y| x.0.total_cmp(&y.0));
            }
            touch(d);
            num(k as f64)
        },

        // ── routes ──
        b"daw_route_count" => num(d.project.routes.len() as f64),
        b"daw_route_add" => {
            // (kind 0 send / 1 sidechain / 2 duck / 3 mod, from, to) -> index, -1 when it would loop
            let (from, to) = (u(1), u(2));
            let kind = if n(0) > 2.5 { "mod" } else if n(0) > 1.5 { "duck" } else if n(0) > 0.5 { "sidechain" } else { "send" };
            if from >= nt || to >= nt || d.project.would_cycle(from, to) {
                return Some(num(-1.0));
            }
            if let Some(i) = d.project.routes.iter().position(|r| r.kind == kind && r.from == from && r.to == to) {
                return Some(num(i as f64));
            }
            d.project.routes.push(RouteDoc { kind: String::from(kind), from, to, amount: if kind == "send" { 0.5 } else { 1.0 }, keys: Vec::new() });
            let i = d.project.routes.len() - 1;
            touch(d);
            num(i as f64)
        },
        b"daw_route_get" => {
            let Some(r) = d.project.routes.get(u(0)) else { return Some(num(0.0)) };
            num(match s(1).as_str() {
                "from" => r.from as f64,
                "to" => r.to as f64,
                "kind" => match r.kind.as_str() {
                    "sidechain" => 1.0,
                    "duck" => 2.0,
                    "mod" => 3.0,
                    _ => 0.0,
                },
                "amount" => r.amount as f64,
                _ => 0.0,
            })
        },
        b"daw_route_set" => {
            if let Some(r) = d.project.routes.get_mut(u(0)) {
                if s(1) == "amount" {
                    r.amount = n(2).clamp(0.0, 2.0) as f32;
                }
            }
            touch(d);
            UNIT
        },
        b"daw_route_remove" => {
            if u(0) < d.project.routes.len() {
                d.project.routes.remove(u(0));
            }
            touch(d);
            UNIT
        },
        b"daw_duck_keys" => {
            let Some(t) = d.project.tracks.get(u(0)) else { return Some(list_from(Vec::new())) };
            let sug: &[(&str, &str)] = if t.instrument == "drums" || t.instrument == "drum" {
                &[("Kick", "35,36"), ("Snare", "37,38,40"), ("Clap", "39"), ("Closed hat", "42,44"), ("Open hat", "46"), ("Toms", "41,43,45,47,48,50"), ("Every hit", "")]
            } else {
                &[("Every note", ""), ("Low notes (below C3)", "0-47"), ("High notes (C4 and up)", "60-127")]
            };
            let mut v = Vec::new();
            for (label, keys) in sug {
                v.extend([mk_str(label), mk_str(keys), num(note_count(t, &parse_keys(keys)) as f64)]);
            }
            list_from(v)
        },
        b"daw_duck_add" => {
            // (source, target, notes, amount) -> [route, effect index]
            let (from, to) = (u(0), u(1));
            if from >= nt || to >= nt || from == to || d.project.would_cycle(from, to) {
                return Some(list_from(Vec::new()));
            }
            let keys = parse_keys(&s(2));
            let amount = if args.len() > 3 { n(3).clamp(0.0, 2.0) as f32 } else { 1.0 };
            let p = &mut d.project;
            let ri = match p.routes.iter().position(|r| r.kind == "duck" && r.from == from && r.to == to) {
                Some(i) => {
                    p.routes[i].keys = keys;
                    p.routes[i].amount = amount;
                    i
                },
                None => {
                    p.routes.push(RouteDoc { kind: String::from("duck"), from, to, amount, keys });
                    p.routes.len() - 1
                },
            };
            let fi = match ducker_index(&p.tracks[to]) {
                Some(i) => i,
                None => match fx::new_doc("Ducker") {
                    Some(doc) => {
                        p.tracks[to].effects.push(doc);
                        p.tracks[to].effects.len() - 1
                    },
                    None => return Some(list_from(Vec::new())),
                },
            };
            touch(d);
            list_from(alloc::vec![num(ri as f64), num(fi as f64)])
        },
        b"daw_duck_list" => {
            let p = &d.project;
            let mut v = Vec::new();
            for (i, r) in p.routes.iter().enumerate().filter(|(_, r)| r.kind == "duck") {
                let fi = p.tracks.get(r.to).and_then(ducker_index).map(|x| x as f64).unwrap_or(-1.0);
                v.extend([num(i as f64), num(r.from as f64), num(r.to as f64), mk_str(&keys_string(&r.keys)), num(fi)]);
            }
            list_from(v)
        },
        b"daw_duck_remove" => {
            let i = u(0);
            if d.project.routes.get(i).map(|r| r.kind == "duck") != Some(true) {
                return Some(boolean(false));
            }
            let to = d.project.routes[i].to;
            d.project.routes.remove(i);
            if n(1) > 0.5 && !d.project.routes.iter().any(|r| r.to == to && r.kind == "duck") {
                if let Some(t) = d.project.tracks.get_mut(to) {
                    t.effects.retain(|e| e.name != "Ducker");
                }
            }
            touch(d);
            boolean(true)
        },
        b"daw_duck_set" => {
            // (target, depth dB, attack ms, hold ms, release ms, curve) on the target's Ducker
            let vals = [depth_n(n(1)), attack_n(n(2)), hold_n(n(3)), release_n(n(4)), n(5).clamp(0.0, 1.0) as f32];
            let Some(t) = d.project.tracks.get_mut(u(0)) else { return Some(boolean(false)) };
            let Some(fi) = ducker_index(t) else { return Some(boolean(false)) };
            for (k, v) in vals.iter().enumerate() {
                if let Some(p) = t.effects[fi].params.get_mut(k) {
                    p.1 = *v;
                }
            }
            touch(d);
            boolean(true)
        },
        b"daw_duck_get" => {
            let Some(t) = d.project.tracks.get(u(0)) else { return Some(list_from(Vec::new())) };
            let Some(fi) = ducker_index(t) else { return Some(list_from(Vec::new())) };
            let p = &t.effects[fi].params;
            if p.len() < 5 {
                return Some(list_from(Vec::new()));
            }
            let v = |i: usize| p[i].1 as f64;
            list_from(alloc::vec![
                num(v(0) * 30.0),
                num((0.0005 + v(1) * v(1) * 0.04) * 1000.0),
                num(v(2) * 200.0),
                num((0.03 + v(3) * v(3) * 1.17) * 1000.0),
                num(v(4))
            ])
        },
        b"daw_duck_shape" => {
            let mut dk = ling_fxcore::dsp::fx::Ducker::new(SR as f32);
            use ling_fxcore::Effect;
            dk.set_param("depth", depth_n(n(0)));
            dk.set_param("attack", attack_n(n(1)));
            dk.set_param("hold", hold_n(n(2)));
            dk.set_param("release", release_n(n(3)));
            dk.set_param("curve", n(4).clamp(0.0, 1.0) as f32);
            list_from(dk.shape(u(5).clamp(2, 256)).into_iter().map(|x| num(x as f64)).collect())
        },

        // ── the Performance launcher ──
        b"daw_perf_scenes" => num(d.project.scenes.len() as f64),
        b"daw_perf_scene_add" => {
            if d.project.scenes.len() >= 1024 {
                return Some(num(-1.0));
            }
            let nm = if s(0).is_empty() { alloc::format!("Scene {}", d.project.scenes.len() + 1) } else { s(0) };
            d.project.scenes.push(nm);
            let i = d.project.scenes.len() - 1;
            touch(d);
            num(i as f64)
        },
        b"daw_perf_scene_remove" => {
            let i = u(0);
            if i < d.project.scenes.len() {
                d.project.scenes.remove(i);
                if i < d.project.slots.len() {
                    d.project.slots.remove(i);
                }
            }
            touch(d);
            UNIT
        },
        b"daw_perf_scene_name" => mk_str(&d.project.scenes.get(u(0)).cloned().unwrap_or_default()),
        b"daw_perf_scene_set_name" => {
            if let Some(x) = d.project.scenes.get_mut(u(0)) {
                *x = s(1);
            }
            touch(d);
            UNIT
        },
        b"daw_perf_slot" => num(d.project.slot(u(0), u(1)) as f64),
        b"daw_perf_slot_info" => {
            let c = d.project.slot(u(0), u(1));
            match (c >= 0).then(|| d.project.tracks.get(u(1)).and_then(|t| t.session.get(c as usize))).flatten() {
                Some(cl) => list_from(alloc::vec![mk_str(&cl.name), num(cl.len), boolean(cl.is_audio())]),
                None => list_from(Vec::new()),
            }
        },
        b"daw_perf_slot_from_clip" | b"daw_perf_slot_new" | b"daw_perf_slot_copy" => {
            let p = &mut d.project;
            let (sc, t, clip) = match name {
                b"daw_perf_slot_from_clip" => {
                    let Some(mut c) = p.tracks.get(u(1)).and_then(|x| x.clips.get(u(2))).cloned() else { return Some(num(-1.0)) };
                    c.start = 0.0;
                    (u(0), u(1), c)
                },
                b"daw_perf_slot_new" => {
                    if p.tracks.get(u(1)).is_none_or(|t| t.is_audio()) {
                        return Some(num(-1.0));
                    }
                    (u(0), u(1), ClipDoc { name: String::from("Pattern"), start: 0.0, len: n(2).max(1.0), ..ClipDoc::default() })
                },
                _ => {
                    let c = p.slot(u(0), u(1));
                    let Some(cl) = (c >= 0).then(|| p.tracks.get(u(1)).and_then(|t| t.session.get(c as usize)).cloned()).flatten() else { return Some(num(-1.0)) };
                    if p.tracks.get(u(3)).is_none_or(|t| t.is_audio() != cl.is_audio()) {
                        return Some(num(-1.0));
                    }
                    (u(2), u(3), cl)
                },
            };
            if sc >= 1024 || t >= p.tracks.len() {
                return Some(num(-1.0));
            }
            p.tracks[t].session.push(clip);
            let idx = (p.tracks[t].session.len() - 1) as i32;
            while p.scenes.len() <= sc {
                let nm = alloc::format!("Scene {}", p.scenes.len() + 1);
                p.scenes.push(nm);
            }
            p.set_slot(sc, t, idx);
            touch(d);
            num(idx as f64)
        },
        b"daw_perf_slot_clear" => {
            let (sc, t) = (u(0), u(1));
            let c = d.project.slot(sc, t);
            if c >= 0 {
                let p = &mut d.project;
                if let Some(tr) = p.tracks.get_mut(t) {
                    if (c as usize) < tr.session.len() {
                        tr.session.remove(c as usize);
                    }
                }
                for row in p.slots.iter_mut() {
                    if let Some(v) = row.get_mut(t) {
                        if *v == c {
                            *v = -1;
                        } else if *v > c {
                            *v -= 1;
                        }
                    }
                }
                p.set_slot(sc, t, -1);
            }
            touch(d);
            UNIT
        },
        b"daw_perf_launch" => {
            // (scene, track)
            let c = d.project.slot(u(0), u(1));
            if c >= 0 && u(1) < nt {
                let was = d.playing;
                start_transport(d);
                let a = audio();
                ensure_sess_len(a, nt);
                let at = quant_at(a, &d.project, was);
                a.sess_queue[u(1)] = Some((c as i64, u(0) as i32, at));
            }
            UNIT
        },
        b"daw_perf_launch_scene" => {
            let was = d.playing;
            start_transport(d);
            let a = audio();
            ensure_sess_len(a, nt);
            let at = quant_at(a, &d.project, was);
            for t in 0..nt {
                let c = d.project.slot(u(0), t);
                a.sess_queue[t] = Some(if c >= 0 { (c as i64, u(0) as i32, at) } else { (-1, -1, at) });
            }
            UNIT
        },
        b"daw_perf_stop_track" => {
            let a = audio();
            ensure_sess_len(a, nt);
            let at = quant_at(a, &d.project, d.playing);
            if u(0) < nt {
                a.sess_queue[u(0)] = Some((-1, -1, at));
            }
            UNIT
        },
        b"daw_perf_stop_all" => {
            let a = audio();
            ensure_sess_len(a, nt);
            for x in a.sess_play.iter_mut() {
                *x = None;
            }
            for x in a.sess_queue.iter_mut() {
                *x = None;
            }
            for v in a.voices.iter_mut() {
                v.gate = false;
            }
            UNIT
        },
        b"daw_perf_state" => {
            // (track) -> [scene playing or -1, queued (-1 none, -2 stop, scene), progress 0..1]
            let a = audio();
            ensure_sess_len(a, nt);
            let t = u(0);
            let q = a.sess_queue.get(t).copied().flatten();
            let queued = match q {
                None => -1.0,
                Some((c, sc, _)) => {
                    if c < 0 {
                        -2.0
                    } else {
                        sc as f64
                    }
                },
            };
            match a.sess_play.get(t).copied().flatten() {
                Some((ci, sc, start)) => {
                    let len = d.project.tracks.get(t).and_then(|x| x.session.get(ci)).map(|c| c.len).unwrap_or(4.0).max(0.25);
                    list_from(alloc::vec![num(sc as f64), num(queued), num(((a.spos - start).max(0.0) % len) / len)])
                },
                None => list_from(alloc::vec![num(-1.0), num(queued), num(0.0)]),
            }
        },
        b"daw_perf_only" => boolean(d.project.session_only),
        b"daw_perf_set_only" => {
            d.project.session_only = n(0) > 0.5;
            touch(d);
            UNIT
        },
        b"daw_perf_quant" => num(d.project.performance_launch_quantize_beats),
        b"daw_perf_set_quant" => {
            d.project.performance_launch_quantize_beats = n(0).clamp(0.0, 64.0);
            touch(d);
            UNIT
        },
        b"daw_perf_rec" => {
            unsafe { PERF_REC = n(0) > 0.5 };
            if n(0) <= 0.5 {
                finish_all(d.position);
                apply_pending(d);
            }
            UNIT
        },
        b"daw_perf_auto" => {
            auto_scenes(&mut d.project, n(0) as u8);
            touch(d);
            num(d.project.scenes.len() as f64)
        },
        _ => return None,
    })
}
