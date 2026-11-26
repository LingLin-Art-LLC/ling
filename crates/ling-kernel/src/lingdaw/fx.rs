//! LingStationX's effects on LingOS: insert chains on every track and the
//! master chain, with the very DSP the Windows build runs (ling-daw's effects,
//! compiled no_std as `ling-fxcore` — `cargo test -p ling-fxcore` renders all 34
//! on both and checks they agree).
//!
//! The project (`FxDoc`: name, knobs, bypass, dry/wet, source file, config
//! JSON) is the truth and is what saves; the running effect instances live in
//! the engine's [`Racks`] and follow it: [`sync`] creates an instance for every
//! new effect (knobs, config and source applied), drops removed ones, follows
//! moves (instances keep their state, a reverb tail survives a reorder) and
//! hands knob changes on. Each effect has a runtime id (`FxDoc::id`, not saved)
//! so a moved or duplicated effect is told apart from a new one.
//!
//! Builtins (ling-daw `api.rs` semantics): `daw_fx_types/add/remove/move/
//! bypass/bypassed/set/set_wet/wet/params/default/reset/random/preset/presets/
//! view/info/command/load/source`, `daw_rig_view/catalog/presets` (the
//! Ringstorm pedalboard).

use alloc::string::String;
use alloc::vec::Vec;

use super::{Daw, FxDoc, ProjectDoc, TrackDoc, SR};
use crate::lingrt::{boolean, is_list, list_from, list_items, mk_str, num, to_f64, to_string, UNIT};
use ling_fxcore::Slot;

pub(super) struct Live {
    id: u64,
    /// None: an effect this build does not know (a script effect from a PC project) -- passes the sound through
    slot: Option<Slot>,
}

/// The running effect instances of one engine (live playback, or an offline render's own).
#[derive(Default)]
pub(super) struct Racks {
    tracks: Vec<Vec<Live>>,
    master: Vec<Live>,
}

impl Racks {
    pub(super) const fn new() -> Racks {
        Racks { tracks: Vec::new(), master: Vec::new() }
    }

    /// Whether track `t` runs any effect (bypassed ones do not count).
    pub(super) fn active(&self, t: usize) -> bool {
        self.tracks.get(t).is_some_and(|c| c.iter().any(|l| l.slot.as_ref().is_some_and(|s| !s.bypass)))
    }

    pub(super) fn master_active(&self) -> bool {
        self.master.iter().any(|l| l.slot.as_ref().is_some_and(|s| !s.bypass))
    }

    fn chain(&mut self, t: i64) -> Option<&mut Vec<Live>> {
        if t < 0 {
            Some(&mut self.master)
        } else {
            self.tracks.get_mut(t as usize)
        }
    }
}

static mut NEXT_ID: u64 = 1;

fn next_id() -> u64 {
    unsafe {
        let v = NEXT_ID;
        NEXT_ID += 1;
        v
    }
}

/// Give every effect a unique runtime id (new ones, and copies made by a track duplicate or an undo).
pub(super) fn ensure_ids(p: &mut ProjectDoc) {
    let mut seen: Vec<u64> = Vec::new();
    let chains = p.tracks.iter_mut().map(|t| &mut t.effects).chain(core::iter::once(&mut p.master_effects));
    for chain in chains {
        for f in chain.iter_mut() {
            if f.id == 0 || seen.contains(&f.id) {
                f.id = next_id();
            }
            seen.push(f.id);
        }
    }
}

fn create(doc: &FxDoc) -> Live {
    let slot = Slot::new(&doc.name, SR as f32).map(|mut s| {
        if !doc.config.is_empty() {
            s.fx.command("load_config", &[], &doc.config);
        }
        if !doc.source.is_empty() {
            s.fx.load_source(&doc.source);
        }
        s
    });
    let mut l = Live { id: doc.id, slot };
    apply(&mut l, doc);
    l
}

/// The knobs, bypass and dry/wet of the project's effect onto the instance.
fn apply(l: &mut Live, doc: &FxDoc) {
    let Some(s) = l.slot.as_mut() else { return };
    for (n, v) in &doc.params {
        if let Some(i) = s.params.iter().position(|(pn, _)| pn == n) {
            if s.params[i].1 != *v {
                s.set(i, *v);
            }
        }
    }
    s.bypass = doc.bypass;
    s.wet = doc.wet.clamp(0.0, 1.0);
}

fn sync_chain(lives: &mut Vec<Live>, docs: &[FxDoc], pool: &mut Vec<Live>) {
    let same = lives.len() == docs.len() && lives.iter().zip(docs).all(|(l, d)| l.id == d.id);
    if same {
        for (l, d) in lives.iter_mut().zip(docs) {
            apply(l, d);
        }
        return;
    }
    pool.extend(lives.drain(..));
    for d in docs {
        let l = match pool.iter().position(|l| l.id == d.id && d.id != 0) {
            Some(i) => {
                let mut l = pool.swap_remove(i);
                apply(&mut l, d);
                l
            },
            None => create(d),
        };
        lives.push(l);
    }
}

/// Bring the running instances in line with the project.
pub(super) fn sync(r: &mut Racks, p: &ProjectDoc) {
    let n = p.tracks.len();
    let same_shape = r.tracks.len() == n
        && r.tracks.iter().zip(&p.tracks).all(|(c, t)| c.len() == t.effects.len() && c.iter().zip(&t.effects).all(|(l, d)| l.id == d.id))
        && r.master.len() == p.master_effects.len()
        && r.master.iter().zip(&p.master_effects).all(|(l, d)| l.id == d.id);
    let mut pool: Vec<Live> = Vec::new();
    if !same_shape {
        // everything not in its place goes to the pool first, so a moved effect is found wherever it went
        for c in r.tracks.iter_mut() {
            pool.extend(c.drain(..));
        }
        pool.extend(r.master.drain(..));
        r.tracks.resize_with(n, Vec::new);
        r.tracks.truncate(n);
    }
    for (c, t) in r.tracks.iter_mut().zip(&p.tracks) {
        sync_chain(c, &t.effects, &mut pool);
    }
    sync_chain(&mut r.master, &p.master_effects, &mut pool);
}

// ── processing ───────────────────────────────────────────────────────────────

/// The keys of track `t` sounding at `beat` as a bit set (fxctx.rs `sounding`); `until` drops to where that can change.
fn sounding(t: &TrackDoc, beat: f64, until: &mut f64) -> u128 {
    let mut set = 0u128;
    for c in &t.clips {
        if c.is_audio() || beat >= c.start + c.len {
            continue;
        }
        if beat < c.start {
            *until = until.min(c.start);
            continue;
        }
        let ll = if c.loop_len >= 0.25 && c.loop_len < c.len { c.loop_len } else { 0.0 };
        let (base, limit) = if ll > 0.0 {
            let b = c.start + libm::floor((beat - c.start) / ll) * ll;
            (b, ll.min(c.start + c.len - b))
        } else {
            (c.start, c.len)
        };
        *until = until.min(base + limit);
        let local = beat - base;
        for n in &c.midi_notes {
            if n.start >= limit {
                continue;
            }
            let end = (n.start + n.len).min(limit);
            if n.start <= local + 1e-9 && end > local + 1e-9 {
                set |= 1u128 << (n.key & 127);
                *until = until.min(base + end);
            } else if n.start > local {
                *until = until.min(base + n.start);
            }
        }
    }
    set
}

fn pitched(t: &TrackDoc) -> bool {
    !t.is_audio() && t.instrument != "drums" && t.instrument != "drum" && !t.clips.is_empty()
}

/// fxctx.rs `keys_at`: the keys an effect on track `own` (None = the master) following `src` hears.
fn keys_at(p: &ProjectDoc, own: Option<usize>, src: ling_fxcore::dsp::fx::KeySource, beat: f64, out: &mut [u8; 32]) -> (usize, f64) {
    use ling_fxcore::dsp::fx::KeySource;
    let mut until = beat + 4.0;
    let own_set = |until: &mut f64| own.and_then(|o| p.tracks.get(o)).map(|t| sounding(t, beat, until)).unwrap_or(0);
    let set = match src {
        KeySource::Own => own_set(&mut until),
        KeySource::Track(k) => p.tracks.get(k).map(|t| sounding(t, beat, &mut until)).unwrap_or(0),
        KeySource::Auto => {
            let (mut chordal, mut all) = (0u128, 0u128);
            for (i, t) in p.tracks.iter().enumerate() {
                if Some(i) == own || !pitched(t) {
                    continue;
                }
                let s = sounding(t, beat, &mut until);
                if s.count_ones() >= 2 {
                    chordal |= s;
                }
                all |= s;
            }
            let set = if chordal != 0 { chordal | (all & all.wrapping_neg()) } else { all };
            if set != 0 {
                set
            } else {
                own_set(&mut until)
            }
        },
    };
    let (mut n, mut s) = (0, set);
    while s != 0 && n < 32 {
        out[n] = s.trailing_zeros() as u8;
        n += 1;
        s &= s - 1;
    }
    (n, until.max(beat + 1e-6))
}

/// Run chain `t` (-1 = the master) over `buf` (interleaved stereo, in place). `beat`: where the block starts, None while
/// the transport is stopped (key-following effects keep what they last heard, as on the PC).
#[allow(clippy::too_many_arguments)]
pub(super) fn process(r: &mut Racks, p: &ProjectDoc, t: i64, buf: &mut [f32], beat: Option<f64>, sc: Option<f32>, aux: Option<&[f32]>, trig: &[(u32, f32)]) {
    let own = if t < 0 { None } else { Some(t as usize) };
    let bpm = p.bpm_at(beat.unwrap_or(0.0));
    let Some(chain) = r.chain(t) else { return };
    for l in chain.iter_mut() {
        let Some(s) = l.slot.as_mut() else { continue };
        // what the routes bring: a sidechain key level, the modulator input, the duck triggers of this block
        s.fx.set_sidechain(sc);
        s.fx.set_aux(aux);
        s.fx.set_triggers(trig);
        s.fx.set_transport(bpm, beat.unwrap_or(0.0));
        if let (Some(kf), Some(b)) = (s.fx.key_follow(), beat) {
            if !(b >= kf.valid.0 && b < kf.valid.1) {
                let mut keys = [0u8; 32];
                let (n, until) = keys_at(p, own, kf.src, b, &mut keys);
                s.fx.set_keys(&keys[..n], b, until);
            }
        }
        if !s.bypass {
            s.process(buf);
            ling_fxcore::scrub(buf);
        }
    }
}

/// Automation moving a running effect: knob `k` ("wet" or a parameter index) of effect `fi` on chain `t` to `v`.
pub(super) fn set_live(r: &mut Racks, t: i64, fi: usize, k: &str, v: f32) {
    let Some(s) = r.chain(t).and_then(|c| c.get_mut(fi)).and_then(|l| l.slot.as_mut()) else { return };
    if k == "wet" {
        s.wet = v.clamp(0.0, 1.0);
    } else if let Ok(pi) = k.parse::<usize>() {
        if s.params.get(pi).is_some_and(|p| p.1 != v.clamp(0.0, 1.0)) {
            s.set(pi, v);
        }
    }
}

// ── builtins ─────────────────────────────────────────────────────────────────

fn docs_mut(p: &mut ProjectDoc, t: i64) -> Option<&mut Vec<FxDoc>> {
    if t < 0 {
        Some(&mut p.master_effects)
    } else {
        p.tracks.get_mut(t as usize).map(|t| &mut t.effects)
    }
}

/// A new effect as the project keeps it (its defaults), None for a name the engine does not have.
pub(super) fn new_doc(name: &str) -> Option<FxDoc> {
    let s = Slot::new(name, SR as f32)?;
    Some(FxDoc {
        name: String::from(name),
        params: s.params.iter().map(|(n, v)| (String::from(*n), *v)).collect(),
        bypass: false,
        wet: 1.0,
        source: String::new(),
        config: String::new(),
        id: next_id(),
    })
}

/// The live instance of effect `f` on chain `t` (made now if playback has not run yet).
fn live<'a>(d: &mut Daw, t: i64, f: usize) -> Option<&'a mut Slot> {
    let a = super::audio();
    ensure_ids(&mut d.project);
    sync(&mut a.racks, &d.project);
    a.racks.chain(t)?.get_mut(f)?.slot.as_mut()
}

fn num_list(args: &[u64], i: usize) -> Vec<f64> {
    match args.get(i) {
        Some(&v) if is_list(v) => list_items(v).iter().map(|&x| to_f64(x)).collect(),
        _ => Vec::new(),
    }
}

pub(super) fn dispatch(name: &[u8], args: &[u64], d: &mut Daw) -> Option<u64> {
    if !(name.starts_with(b"daw_fx_") || name.starts_with(b"daw_rig_") || name == b"daw_script_fx_add") {
        return None;
    }
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).unwrap_or(0.0);
    let u = |i: usize| n(i).max(0.0) as usize;
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let t = n(0) as i64;
    let touch = |d: &mut Daw| {
        d.dirty = true;
        d.rev += 1;
    };
    Some(match name {
        b"daw_fx_types" => list_from(ling_fxcore::EFFECT_NAMES.iter().map(|s| mk_str(s)).collect()),
        b"daw_fx_count" => num(docs_mut(&mut d.project, t).map(|v| v.len()).unwrap_or(0) as f64),
        b"daw_fx_name" => mk_str(&docs_mut(&mut d.project, t).and_then(|v| v.get(u(1))).map(|f| f.name.clone()).unwrap_or_default()),
        b"daw_fx_bypassed" => boolean(docs_mut(&mut d.project, t).and_then(|v| v.get(u(1))).is_some_and(|f| f.bypass)),
        b"daw_fx_wet" => num(docs_mut(&mut d.project, t).and_then(|v| v.get(u(1))).map(|f| f.wet).unwrap_or(1.0) as f64),
        b"daw_fx_params" => list_from(
            docs_mut(&mut d.project, t)
                .and_then(|v| v.get(u(1)))
                .map(|f| f.params.iter().flat_map(|(k, v)| [mk_str(k), num(*v as f64)]).collect())
                .unwrap_or_default(),
        ),
        b"daw_fx_add" => {
            // (target, name) -> index, -1 for an unknown effect or target
            let Some(doc) = new_doc(&s(1)) else { return Some(num(-1.0)) };
            let Some(v) = docs_mut(&mut d.project, t) else { return Some(num(-1.0)) };
            v.push(doc);
            let i = v.len() - 1;
            touch(d);
            num(i as f64)
        },
        b"daw_fx_remove" => {
            if let Some(v) = docs_mut(&mut d.project, t) {
                if u(1) < v.len() {
                    v.remove(u(1));
                }
            }
            touch(d);
            UNIT
        },
        b"daw_fx_move" => {
            if let Some(v) = docs_mut(&mut d.project, t) {
                let i = u(1);
                if i < v.len() {
                    let j = (i as i64 + n(2) as i64).clamp(0, v.len() as i64 - 1) as usize;
                    let f = v.remove(i);
                    v.insert(j, f);
                }
            }
            touch(d);
            UNIT
        },
        b"daw_fx_bypass" => {
            if let Some(f) = docs_mut(&mut d.project, t).and_then(|v| v.get_mut(u(1))) {
                f.bypass = n(2) > 0.5;
            }
            touch(d);
            UNIT
        },
        b"daw_fx_set_wet" => {
            let w = n(2) as f32;
            super::route::record(d, if t >= 0 { Some(t as usize) } else { None }, &alloc::format!("fx{}.wet", u(1)), w.clamp(0.0, 1.0));
            if let Some(f) = docs_mut(&mut d.project, t).and_then(|v| v.get_mut(u(1))) {
                if w.is_finite() {
                    f.wet = w.clamp(0.0, 1.0);
                }
            }
            touch(d);
            UNIT
        },
        b"daw_fx_set" => {
            // (target, idx, param index, 0..1)
            let v = n(3) as f32;
            super::route::record(d, if t >= 0 { Some(t as usize) } else { None }, &alloc::format!("fx{}.{}", u(1), u(2)), v);
            if let Some(f) = docs_mut(&mut d.project, t).and_then(|c| c.get_mut(u(1))) {
                if let Some(p) = f.params.get_mut(u(2)) {
                    if v.is_finite() {
                        p.1 = v.clamp(0.0, 1.0);
                    }
                }
            }
            touch(d);
            UNIT
        },
        b"daw_fx_default" => {
            let nm = docs_mut(&mut d.project, t).and_then(|v| v.get(u(1))).map(|f| f.name.clone()).unwrap_or_default();
            num(ling_fxcore::defaults(&nm).get(u(2)).map(|p| p.1 as f64).unwrap_or(-1.0))
        },
        b"daw_fx_reset" | b"daw_fx_random" | b"daw_fx_preset" => {
            // reset: every knob to its default; random (seed): near the defaults, never the levels; preset (index): defaults + the preset
            let mut done = false;
            if let Some(f) = docs_mut(&mut d.project, t).and_then(|v| v.get_mut(u(1))) {
                let defs = ling_fxcore::defaults(&f.name);
                let set = |f: &mut FxDoc, pn: &str, v: f32| {
                    if let Some(p) = f.params.iter_mut().find(|p| p.0 == pn) {
                        p.1 = v.clamp(0.0, 1.0);
                    }
                };
                match name {
                    b"daw_fx_reset" => {
                        for (pn, dv) in defs {
                            set(f, pn, *dv);
                        }
                        done = true;
                    },
                    b"daw_fx_random" => {
                        let mut rng = (n(2) as u32).wrapping_mul(2654435761).wrapping_add(0x9e3779b9) | 1;
                        for (pn, dv) in defs {
                            rng ^= rng << 13;
                            rng ^= rng >> 17;
                            rng ^= rng << 5;
                            if matches!(*pn, "mix" | "level" | "width" | "gain" | "ceiling" | "trigger" | "push" | "input" | "output" | "slot")
                                || pn.ends_with(" solo")
                                || pn.ends_with(" gain")
                            {
                                continue;
                            }
                            let r = (rng & 0xffff) as f32 / 65535.0;
                            set(f, pn, dv + (r - dv) * 0.85);
                        }
                        done = true;
                    },
                    _ => {
                        if let Some(pr) = ling_fxcore::presets(&f.name).get(u(2)) {
                            for (pn, dv) in defs {
                                set(f, pn, *dv);
                            }
                            for (pn, v) in pr.1 {
                                set(f, pn, *v);
                            }
                            done = true;
                        }
                    },
                }
            }
            touch(d);
            if name == b"daw_fx_preset" {
                num(done as u8 as f64)
            } else {
                UNIT
            }
        },
        b"daw_fx_presets" => list_from(ling_fxcore::presets(&s(0)).iter().map(|p| mk_str(p.0)).collect()),
        b"daw_fx_view" => list_from(live(d, t, u(1)).map(|s| s.fx.view()).unwrap_or_default().into_iter().map(|x| num(x as f64)).collect()),
        b"daw_fx_info" => mk_str(&live(d, t, u(1)).map(|s| s.fx.info(&s_owned(args, 2))).unwrap_or_default()),
        b"daw_fx_source" => mk_str(&docs_mut(&mut d.project, t).and_then(|v| v.get(u(1))).map(|f| f.source.clone()).unwrap_or_default()),
        b"daw_fx_load" => {
            let path = s(2).replace('\\', "/");
            let ok = live(d, t, u(1)).is_some_and(|s| s.fx.load_source(&path));
            if ok {
                if let Some(f) = docs_mut(&mut d.project, t).and_then(|v| v.get_mut(u(1))) {
                    f.source = path;
                }
                touch(d);
            }
            boolean(ok)
        },
        b"daw_fx_command" => {
            // (target, idx, command, [numbers], text) -> [numbers]; the effect's whole state goes back into the project
            let (cmd, text) = (s(2), s(4));
            let nums = num_list(args, 3);
            let mut cfg = None;
            let r = match live(d, t, u(1)) {
                Some(sl) => {
                    let r = sl.fx.command(&cmd, &nums, &text);
                    if !matches!(cmd.as_str(), "meter" | "get_param" | "count" | "kinds") {
                        cfg = Some(sl.fx.info("config"));
                    }
                    r
                },
                None => Vec::new(),
            };
            if let Some(c) = cfg {
                if let Some(f) = docs_mut(&mut d.project, t).and_then(|v| v.get_mut(u(1))) {
                    f.config = c;
                }
                touch(d);
            }
            list_from(r.into_iter().map(num).collect())
        },
        b"daw_rig_view" => {
            // [kind, input, out 0/1, bypass 0/1, level, [name, value]*, peak] per pedal
            let Some(sl) = live(d, t, u(1)) else { return Some(list_from(Vec::new())) };
            let nodes: Vec<serde_json::Value> = serde_json::from_str(&sl.fx.info("nodes")).unwrap_or_default();
            let mut out = Vec::new();
            for (i, nd) in nodes.iter().enumerate() {
                let mut ps = Vec::new();
                if let Some(o) = nd["params"].as_object() {
                    for (k, v) in o {
                        ps.push(mk_str(k));
                        ps.push(num(v.as_f64().unwrap_or(0.0)));
                    }
                }
                let peak = sl.fx.command("meter", &[i as f64], "").first().copied().unwrap_or(0.0);
                out.push(list_from(alloc::vec![
                    mk_str(nd["kind"].as_str().unwrap_or("")),
                    num(nd["input"].as_f64().unwrap_or(-1.0)),
                    num(if nd["out"].as_bool().unwrap_or(false) { 1.0 } else { 0.0 }),
                    num(if nd["bypass"].as_bool().unwrap_or(false) { 1.0 } else { 0.0 }),
                    num(nd["level"].as_f64().unwrap_or(1.0)),
                    list_from(ps),
                    num(peak),
                ]));
            }
            list_from(out)
        },
        b"daw_rig_catalog" => {
            let Some(sl) = live(d, t, u(1)) else { return Some(list_from(Vec::new())) };
            let kinds: Vec<serde_json::Value> = serde_json::from_str(&sl.fx.info("kinds")).unwrap_or_default();
            let mut out = Vec::new();
            for k in &kinds {
                let mut ps = Vec::new();
                if let Some(arr) = k["params"].as_array() {
                    for p in arr {
                        ps.push(mk_str(p["name"].as_str().unwrap_or("")));
                        ps.push(num(p["def"].as_f64().unwrap_or(0.5)));
                    }
                }
                out.push(list_from(alloc::vec![mk_str(k["kind"].as_str().unwrap_or("")), list_from(ps)]));
            }
            list_from(out)
        },
        b"daw_rig_presets" => {
            let Some(sl) = live(d, t, u(1)) else { return Some(list_from(Vec::new())) };
            let names: Vec<String> = serde_json::from_str(&sl.fx.info("presets")).unwrap_or_default();
            list_from(names.iter().map(|x| mk_str(x)).collect())
        },
        // script effects run a Ling script per sample on the PC (ScriptFx); the kernel has no per-sample script DSP
        b"daw_script_fx_add" => {
            d.status = String::from("Script effects need LingStationX on Windows / Linux (no script DSP on LingOS yet)");
            num(-1.0)
        },
        _ => return None,
    })
}

fn s_owned(args: &[u64], i: usize) -> String {
    args.get(i).map(|&v| to_string(v)).unwrap_or_default()
}
