//! The DAW engine behind LingStationX's `daw_*` builtins on LingOS, step 1:
//! the project model. On the desktop these are served by `ling-daw` (a std
//! engine with cpal audio, threads and file dialogs); in the kernel this
//! module holds the same project — parsed from the very same `project.json`
//! format (`ProjectDoc`/`TrackDoc`/`ClipDoc`/`NoteDoc`, field for field from
//! ling-daw's `project.rs`, with its serde defaults) — and answers the reads
//! and simple edits with ling-daw `api.rs` semantics, so the Arranger, track
//! list and clip views show a real project.
//!
//! Not here yet: instruments/effects DSP and audio output (the transport
//! clock runs, so the playhead moves, but nothing sounds), audio clips'
//! samples (no storage driver to read stems from), note editing, undo.
//!
//! LingStationX itself opens its "Anthem of Ling Country" starter template on
//! first run; the image embeds that template (and FunKey's project file,
//! listed as a template too — see `expose_projects_as_templates`).

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::lingrt::{boolean, list_from, mk_str, num, to_f64, to_string, UNIT};

/// Audio recording (`daw_audio_rec_*`) and audio-clip playback.
mod rec;
pub use rec::rec_service;
/// Effects: insert chains + master chain (ling-daw's DSP via ling-fxcore).
mod fx;
/// Undo / redo, clip edits, the clip clipboard, live notes.
mod edit;
/// Automation lanes, routes (send / sidechain / duck / mod) and the Performance launcher.
mod route;
/// Factory presets from text, Growl preset chains, the arpeggiator, BPM detection.
mod tools;
/// Standard MIDI files: scan, import, export.
mod midi;
/// The instrument panels' pictures (`daw_inst_plot`, Nova's table names).
mod plot;
/// Decorate Clip, Theme and Variations.
mod deco;
/// Make Medley from a folder of MIDI files (the desktop's medley engine, ported).
mod medley;
/// The vocal editor's signal processing and note math (ling-daw vocal.rs + vedit_api.rs, ported; host-tested for parity).
mod vocal;
/// The inline vocal editor (`daw_ve_*`, `daw_vm_scales` / `daw_vm_temperaments`).
mod vedit;

// ── project.json (ling-daw project.rs) ───────────────────────────────────────

fn half() -> f32 {
    0.5
}
fn one() -> f32 {
    1.0
}
fn four() -> f64 {
    4.0
}
fn yes() -> bool {
    true
}

#[derive(Deserialize, Serialize, Clone, Default)]
struct FxDoc {
    #[serde(default)]
    name: String,
    #[serde(default)]
    params: Vec<(String, f32)>,
    #[serde(default)]
    bypass: bool,
    /// dry/wet mix, 0..1 (projects from before the mix knob have none: fully wet)
    #[serde(default = "full_wet")]
    wet: f32,
    /// a file the effect works from (the growl's spectral sample), empty for none
    #[serde(default)]
    source: String,
    /// the whole state of an effect that has more than named knobs (the Ringstorm's pedal graph) as JSON
    #[serde(default)]
    config: String,
    /// runtime identity of the effect (not saved): ties it to its running instance, see `fx`
    #[serde(skip)]
    id: u64,
}

fn full_wet() -> f32 {
    1.0
}

#[derive(Deserialize, Serialize, Clone)]
struct NoteDoc {
    start: f64,
    len: f64,
    key: u8,
    vel: f32,
    #[serde(default)]
    pan: f32,
    #[serde(default = "half")]
    cutoff: f32,
    #[serde(default = "half")]
    reso: f32,
    // kept so a project from the PC saves back whole (ling-daw project.rs NoteDoc)
    #[serde(default)]
    lyric: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    bend: Vec<(f32, f32)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    press: Vec<(f32, f32)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    timbre: Vec<(f32, f32)>,
}

impl NoteDoc {
    fn new(start: f64, len: f64, key: u8, vel: f32) -> NoteDoc {
        NoteDoc { start, len, key, vel, pan: 0.0, cutoff: 0.5, reso: 0.5, lyric: String::new(), bend: Vec::new(), press: Vec::new(), timbre: Vec::new() }
    }
}

#[derive(Deserialize, Serialize, Clone)]
struct ClipDoc {
    #[serde(default)]
    name: String,
    start: f64,
    len: f64,
    #[serde(default)]
    midi_notes: Vec<NoteDoc>,
    #[serde(default)]
    audio_path: String,
    #[serde(default)]
    offset_s: f64,
    #[serde(default = "one")]
    gain: f32,
    #[serde(default)]
    pitch: f32,
    #[serde(default)]
    warp: f32,
    #[serde(default)]
    loop_len: f64,
    #[serde(default)]
    lyric_text: String,
    #[serde(default)]
    lyric_lang: String,
    #[serde(default)]
    tag: String,
}

impl Default for ClipDoc {
    fn default() -> ClipDoc {
        ClipDoc {
            name: String::new(),
            start: 0.0,
            len: 4.0,
            midi_notes: Vec::new(),
            audio_path: String::new(),
            offset_s: 0.0,
            gain: 1.0,
            pitch: 0.0,
            warp: 0.0,
            loop_len: 0.0,
            lyric_text: String::new(),
            lyric_lang: String::new(),
            tag: String::new(),
        }
    }
}

impl ClipDoc {
    fn is_audio(&self) -> bool {
        !self.audio_path.is_empty()
    }
    fn end(&self) -> f64 {
        self.start + self.len
    }
}

#[derive(Deserialize, Serialize, Clone)]
struct TrackDoc {
    #[serde(default)]
    name: String,
    #[serde(default)]
    color: u32,
    #[serde(default)]
    kind: String, // instrument | audio
    #[serde(default)]
    instrument: String, // synth | drums | ...
    #[serde(default)]
    patch: String,
    #[serde(default = "one")]
    level: f32,
    #[serde(default)]
    pan: f32,
    #[serde(default)]
    muted: bool,
    #[serde(default)]
    solo: bool,
    #[serde(default)]
    effects: Vec<FxDoc>,
    #[serde(default)]
    clips: Vec<ClipDoc>,
    /// The instrument's knobs, (name, 0..1), exactly the PC's `state` field.
    #[serde(default)]
    state: Vec<(String, f32)>,
    #[serde(default)]
    config: String,
    /// the Performance launcher's clips (the scenes' slots index them)
    #[serde(default)]
    session: Vec<ClipDoc>,
    /// automation lanes: vol, pan, an instrument knob, fx<i>.<p> / fx<i>.wet
    #[serde(default)]
    auto: Vec<AutoDoc>,
}

/// An automation lane (ling-daw project.rs AutoDoc / model.rs AutoLane).
#[derive(Deserialize, Serialize, Clone)]
struct AutoDoc {
    param: String,
    #[serde(default)]
    points: Vec<(f64, f32)>,
}

impl AutoDoc {
    /// The lane's value at `beat` (linear between points, held before the first / after the last), None when empty.
    fn value_at(&self, beat: f64) -> Option<f32> {
        let p = &self.points;
        if p.is_empty() {
            return None;
        }
        if beat <= p[0].0 {
            return Some(p[0].1);
        }
        if beat >= p[p.len() - 1].0 {
            return Some(p[p.len() - 1].1);
        }
        let i = p.partition_point(|x| x.0 <= beat);
        if i == 0 || i >= p.len() {
            return Some(p[p.len() - 1].1);
        }
        let (a, b) = (p[i - 1], p[i]);
        let t = ((beat - a.0) / (b.0 - a.0).max(1e-9)) as f32;
        Some(a.1 + (b.1 - a.1) * t)
    }
}

/// A track-to-track route (ling-daw project.rs RouteDoc): "send" (post-fader audio), "sidechain" (the source's level keys the
/// target's compressor), "duck" (the source's notes trigger the target's Ducker), "mod" (the source's audio is the modulator input
/// of the target's effects, the Vocoder).
#[derive(Deserialize, Serialize, Clone)]
struct RouteDoc {
    kind: String,
    from: usize,
    to: usize,
    amount: f32,
    #[serde(default)]
    keys: Vec<u8>,
}

impl TrackDoc {
    fn is_audio(&self) -> bool {
        self.kind == "audio"
    }

    /// The knobs the Parameters view shows: the saved state, else the
    /// instrument kind's defaults (the PC synth's parameter list).
    fn params(&self) -> Vec<(String, f32)> {
        if !self.state.is_empty() {
            return self.state.clone();
        }
        let defaults: &[(&str, f32)] = if self.instrument == "drums" || self.instrument == "drum" {
            &[("tune", 0.5), ("decay", 0.5), ("tone", 0.5), ("snap", 0.5), ("room", 0.2), ("level", 0.8)]
        } else {
            &[
                ("wave", 0.0), ("sub", 0.3), ("add", 0.0), ("fm", 0.0), ("cutoff", 0.6), ("reso", 0.2), ("filt env", 0.3),
                ("partials", 0.5), ("rolloff", 0.5), ("odd/even", 0.5), ("fm ratio", 0.25), ("fm index", 0.0), ("detune", 0.15),
                ("noise", 0.0), ("attack", 0.1), ("decay", 0.4), ("sustain", 0.7), ("release", 0.3), ("lfo rate", 0.3),
                ("lfo cut", 0.0), ("lfo pitch", 0.0), ("fm env", 0.0),
            ]
        };
        defaults.iter().map(|(n, v)| (String::from(*n), *v)).collect()
    }

    fn param(&self, name: &str) -> Option<f32> {
        self.params().into_iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    fn set_param(&mut self, name: &str, v: f32) {
        if self.state.is_empty() {
            self.state = self.params();
        }
        let v = if v.is_finite() { v.clamp(0.0, 1.0) } else { return };
        match self.state.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = v,
            None => self.state.push((String::from(name), v)),
        }
    }
}

/// Load a LingStationX patch file (`type = synth`, `wave=0.66 sub=1 ...`,
/// `fx ...` effect lines skipped) onto a track: its kind and its knobs, named
/// the way the project keeps them (`filt_env` -> `filt env`).
fn apply_patch(t: &mut TrackDoc, text: &str, path: &str) -> bool {
    let mut kind = String::new();
    let mut state: Vec<(String, f32)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("name") || line.starts_with("fx ") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("type") {
            kind = String::from(rest.trim().trim_start_matches('=').trim().split(' ').next().unwrap_or(""));
            continue;
        }
        for tok in line.split_whitespace() {
            if let Some((k, v)) = tok.split_once('=') {
                if let Ok(v) = v.parse::<f32>() {
                    let k = k.replace('_', " ");
                    match state.iter_mut().find(|(n, _)| *n == k) {
                        Some(slot) => slot.1 = v,
                        None => state.push((k, v)),
                    }
                }
            }
        }
    }
    if state.is_empty() {
        return false;
    }
    if !kind.is_empty() {
        t.instrument = kind;
    }
    t.state = state;
    t.patch = String::from(path);
    true
}

/// The `.ling` patches in a folder as (display name, path): the file name
/// without `.ling`, its kind prefix (`Chip_`) or GM number, `_` as spaces.
fn patch_files(dir: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, is_dir, _) in crate::vfs::list(dir) {
        let Some(stem) = name.strip_suffix(".ling") else { continue };
        if is_dir {
            continue;
        }
        let mut label = String::from(stem);
        for pre in ["Chip_", "Cinema_", "Nova_", "Growl_", "Voice_", "Prism_"] {
            if let Some(r) = label.strip_prefix(pre) {
                label = String::from(r);
            }
        }
        // "001 Acoustic Grand Piano"
        if label.len() > 4 && label.as_bytes()[..3].iter().all(|b| b.is_ascii_digit()) && label.as_bytes()[3] == b' ' {
            label = String::from(&label[4..]);
        }
        out.push((label.replace('_', " "), alloc::format!("{dir}/{name}")));
    }
    out
}

/// The presets an instrument has built in. On the PC the engine carries them;
/// here they are the app's own patch folders for the instruments whose
/// presets the chooser does not list as files.
fn builtin_presets(kind: &str) -> Vec<(String, String)> {
    match kind {
        "chip" => patch_files("instruments/Chip"),
        "cinema" => patch_files("instruments/Cinema"),
        "nova" => patch_files("instruments/Nova"),
        // the PC engine's own factory sets (ling-daw *_presets.txt)
        other => tools::factory_presets(other),
    }
}

#[derive(Deserialize, Serialize, Clone)]
struct ProjectDoc {
    #[serde(default)]
    name: String,
    #[serde(default)]
    tempo_bpm: f64,
    #[serde(default)]
    tempo_map: Vec<(f64, f64)>,
    #[serde(default = "one")]
    master_level: f32,
    #[serde(default = "yes")]
    limiter: bool,
    #[serde(default)]
    master_effects: Vec<FxDoc>,
    #[serde(default)]
    master_auto: Vec<AutoDoc>,
    #[serde(default)]
    loop_on: bool,
    #[serde(default)]
    loop_start: f64,
    #[serde(default)]
    loop_end: f64,
    #[serde(default)]
    meta: Vec<(String, String)>,
    // routes, scenes and launcher slots: kept as the PC wrote them (saved back whole)
    #[serde(default)]
    routes: Vec<RouteDoc>,
    #[serde(default)]
    scenes: Vec<String>,
    #[serde(default)]
    slots: Vec<Vec<i32>>,
    #[serde(default)]
    session_only: bool,
    #[serde(default = "four")]
    #[allow(dead_code)]
    performance_launch_quantize_beats: f64,
    #[serde(default)]
    tracks: Vec<TrackDoc>,
}

impl ProjectDoc {
    fn empty(name: &str, bpm: f64) -> ProjectDoc {
        ProjectDoc {
            name: String::from(name),
            tempo_bpm: bpm,
            tempo_map: Vec::new(),
            master_level: 0.9,
            limiter: true,
            master_effects: Vec::new(),
            master_auto: Vec::new(),
            loop_on: false,
            loop_start: 0.0,
            loop_end: 16.0,
            meta: Vec::new(),
            routes: Vec::new(),
            scenes: Vec::new(),
            slots: Vec::new(),
            session_only: false,
            performance_launch_quantize_beats: 4.0,
            tracks: Vec::new(),
        }
    }

    /// ling-daw `apply_doc`'s clean-ups: clamped tempo, sane loop.
    fn normalised(mut self) -> ProjectDoc {
        self.tempo_bpm = if self.tempo_bpm.is_finite() && self.tempo_bpm > 0.0 { self.tempo_bpm.clamp(20.0, 400.0) } else { 120.0 };
        self.tempo_map.retain(|(b, v)| b.is_finite() && v.is_finite() && *b >= 0.0);
        for p in self.tempo_map.iter_mut() {
            p.1 = p.1.clamp(20.0, 400.0);
        }
        self.tempo_map.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
        if !(self.loop_end > self.loop_start) {
            self.loop_end = self.loop_start + 16.0;
        }
        self.master_level = if self.master_level.is_finite() { self.master_level.clamp(0.0, 2.0) } else { 0.9 };
        self
    }

    fn bpm_at(&self, beat: f64) -> f64 {
        let mut b = self.tempo_bpm;
        for (pb, v) in &self.tempo_map {
            if *pb <= beat {
                b = *v;
            } else {
                break;
            }
        }
        b
    }

    /// Seconds between two beats, following the tempo map (model.rs).
    fn beats_to_secs(&self, a: f64, b: f64) -> f64 {
        if b <= a {
            return 0.0;
        }
        let (mut cur, mut bpm, mut secs) = (a, self.bpm_at(a), 0.0);
        for (pb, v) in &self.tempo_map {
            if *pb > cur && *pb < b {
                secs += (pb - cur) * 60.0 / bpm;
                cur = *pb;
                bpm = *v;
            }
        }
        secs + (b - cur) * 60.0 / bpm
    }

    fn end_beat(&self) -> f64 {
        self.tracks.iter().flat_map(|t| t.clips.iter().map(|c| c.end())).fold(0.0, f64::max)
    }

    /// The launcher clip in a scene's slot for a track, -1 for none.
    fn slot(&self, scene: usize, track: usize) -> i32 {
        self.slots.get(scene).and_then(|r| r.get(track)).copied().unwrap_or(-1)
    }

    fn set_slot(&mut self, scene: usize, track: usize, v: i32) {
        if scene >= 1024 || track >= 256 {
            return;
        }
        while self.slots.len() <= scene {
            self.slots.push(Vec::new());
        }
        let row = &mut self.slots[scene];
        while row.len() <= track {
            row.push(-1);
        }
        row[track] = v;
    }

    /// Would a route from -> to make a loop (model.rs `would_cycle`)?
    fn would_cycle(&self, from: usize, to: usize) -> bool {
        if from == to {
            return true;
        }
        let mut stack = alloc::vec![to];
        let mut seen = alloc::vec![false; self.tracks.len().max(from.max(to) + 1)];
        while let Some(n) = stack.pop() {
            if n == from {
                return true;
            }
            if n >= seen.len() || seen[n] {
                continue;
            }
            seen[n] = true;
            for r in &self.routes {
                if r.from == n {
                    stack.push(r.to);
                }
            }
        }
        false
    }

    /// Remove track `i`, keeping routes and launcher slots consistent (model.rs `remove_track`).
    fn remove_track(&mut self, i: usize) {
        if i >= self.tracks.len() {
            return;
        }
        self.tracks.remove(i);
        self.routes.retain(|r| r.from != i && r.to != i);
        for r in self.routes.iter_mut() {
            if r.from > i {
                r.from -= 1;
            }
            if r.to > i {
                r.to -= 1;
            }
        }
        for row in self.slots.iter_mut() {
            if i < row.len() {
                row.remove(i);
            }
        }
    }

    /// Insert a track at `at`: routes and slot columns after it move along.
    fn insert_track(&mut self, at: usize, t: TrackDoc) {
        let at = at.min(self.tracks.len());
        self.tracks.insert(at, t);
        for r in self.routes.iter_mut() {
            if r.from >= at {
                r.from += 1;
            }
            if r.to >= at {
                r.to += 1;
            }
        }
        for row in self.slots.iter_mut() {
            if at <= row.len() {
                row.insert(at, -1);
            }
        }
    }

    /// Move track `i` to `j` (model.rs `move_track`).
    fn move_track(&mut self, i: usize, j: usize) {
        let n = self.tracks.len();
        if i >= n || j >= n || i == j {
            return;
        }
        let map = |old: usize| -> usize {
            if old == i {
                j
            } else if i < j && old > i && old <= j {
                old - 1
            } else if i > j && old >= j && old < i {
                old + 1
            } else {
                old
            }
        };
        let t = self.tracks.remove(i);
        self.tracks.insert(j, t);
        for r in self.routes.iter_mut() {
            r.from = map(r.from);
            r.to = map(r.to);
        }
        for row in self.slots.iter_mut() {
            if row.len() < n {
                row.resize(n, -1);
            }
            let v = row.remove(i);
            row.insert(j, v);
        }
    }
}

const PALETTE: [u32; 7] = [0xED4A37, 0xF7941E, 0xF7D846, 0x45C870, 0x3BAAEB, 0x4A64D8, 0x9A53D6];

// ── engine state ─────────────────────────────────────────────────────────────

struct Daw {
    project: ProjectDoc,
    path: String,
    status: String,
    dirty: bool,
    /// Bumped on every change (ling-daw's `daw_rev`: the UI re-reads on change).
    rev: u64,
    playing: bool,
    /// Beat position when the transport last started / was seeked.
    base_beat: f64,
    base_ms: u64,
    position: f64,
}

static mut DAW: Option<Daw> = None;

fn daw() -> &'static mut Daw {
    let d = unsafe { &mut *(&raw mut DAW) };
    if d.is_none() {
        let mut fresh = Daw {
            project: ProjectDoc::empty("Untitled", 120.0),
            path: String::new(),
            status: String::new(),
            dirty: false,
            rev: 0,
            playing: false,
            base_beat: 0.0,
            base_ms: 0,
            position: 0.0,
        };
        expose_projects_as_templates();
        ling_fxcore::host::set_audio_loader(rec::fx_loader);
        fresh.rev = 1;
        *d = Some(fresh);
    }
    d.as_mut().unwrap()
}

/// Every embedded `projects/<name>/project.json` also appears as
/// `assets/templates/<name>/project.json`, so LingStationX's own
/// File > New From Template menu lists it (there's no file dialog on LingOS
/// to open a project with yet).
fn expose_projects_as_templates() {
    let mut names: Vec<String> = Vec::new();
    crate::appfs::list("projects", |name, is_dir| {
        if is_dir {
            names.push(String::from(name));
        }
    });
    for name in names {
        let src = alloc::format!("projects/{name}/project.json");
        if let Some(bytes) = crate::appfs::get(&src) {
            let alias = alloc::format!("assets/templates/{name}/project.json");
            if !crate::appfs::exists(&alias) {
                crate::appfs::register(alloc::boxed::Box::leak(alias.into_boxed_str()), bytes);
            }
        }
    }
}

fn load_into(d: &mut Daw, path: &str) -> Result<(), String> {
    let Some(bytes) = crate::vfs::read(path) else {
        return Err(alloc::format!("{path}: not found"));
    };
    let doc: ProjectDoc = serde_json::from_slice(&bytes).map_err(|e| alloc::format!("{path}: {e}"))?;
    d.project = doc.normalised();
    d.path = String::from(path);
    d.dirty = false;
    d.rev += 1;
    d.playing = false;
    d.position = 0.0;
    d.base_beat = 0.0;
    d.status = alloc::format!("Opened {path}");
    edit::reset(d);
    Ok(())
}

/// Advance the transport clock: while playing, the position follows real time
/// at the tempo map's speed, wrapping at the loop end when the loop is on.
fn tick(d: &mut Daw) {
    if !d.playing {
        return;
    }
    let now = crate::arch::timer::now_ms();
    if let Some(beat) = audible_beat(&d.project) {
        // the speaker is the clock: the playhead shows what is being heard
        d.position = beat;
        d.base_beat = beat;
        d.base_ms = now;
        return;
    }
    {
        // Play was just pressed and the device hasn't pulled its first block:
        // hold the playhead (up to 250 ms) instead of running ahead on the wall
        // clock and then stepping back when the audio clock takes over.
        let a = audio();
        if a.running && a.clock_us == 0 && now.saturating_sub(d.base_ms) < 250 {
            d.position = d.base_beat;
            return;
        }
    }
    let secs = now.saturating_sub(d.base_ms) as f64 / 1000.0;
    // Walk forward through the tempo map from base_beat for `secs` seconds.
    let p = &d.project;
    let (mut beat, mut left) = (d.base_beat, secs);
    loop {
        let bpm = p.bpm_at(beat);
        let next = p.tempo_map.iter().map(|(b, _)| *b).find(|b| *b > beat);
        match next {
            Some(nb) if (nb - beat) * 60.0 / bpm < left => {
                left -= (nb - beat) * 60.0 / bpm;
                beat = nb;
            },
            _ => {
                beat += left * bpm / 60.0;
                break;
            },
        }
    }
    if p.loop_on && p.loop_end > p.loop_start && beat >= p.loop_end {
        let span = p.loop_end - p.loop_start;
        beat = p.loop_start + (beat - p.loop_start) % span;
        d.base_beat = beat;
        d.base_ms = now;
    } else if !p.loop_on && beat > p.end_beat().max(4.0) + 4.0 && !rec::recording() {
        d.playing = false;
        beat = 0.0;
    }
    d.position = beat;
}

// ── Tracker view (daw_trk_*): the arrangement as rows and channels ─────────
// Every instrument track becomes as many channels as it has notes sounding at
// once (up to 8): notes are dealt to the lowest free channel, so a chord reads
// as a row of notes across its channels. Audio tracks get one channel whose
// cells mark where clips start. A row is 1 / rows-per-beat beats. Ported from
// ling-daw's tracker_api.rs; the layout is rebuilt when the project changes.
const TRK_VOICES: usize = 8;
const AUDIO_KEY: i64 = 128;

#[derive(Clone)]
struct TrkEv {
    s: f64,
    e: f64,
    key: i64,
    vel: f32,
    clip: usize,
    note: usize,
}

struct TrkChan {
    track: usize,
    voice: usize,
    evs: Vec<TrkEv>,
}

struct TrkLayout {
    rev: u64,
    chans: Vec<TrkChan>,
    end: f64,
}

static mut TRK: Option<TrkLayout> = None;

fn trk_track_events(t: &TrackDoc) -> Vec<TrkEv> {
    let mut out: Vec<TrkEv> = Vec::new();
    for (ci, c) in t.clips.iter().enumerate() {
        if c.is_audio() {
            out.push(TrkEv { s: c.start, e: c.start + c.len, key: AUDIO_KEY, vel: 1.0, clip: ci, note: usize::MAX });
            continue;
        }
        let ll = if c.loop_len >= 0.25 && c.loop_len < c.len { c.loop_len } else { 0.0 };
        let reps = if ll > 0.0 { (libm::ceil(c.len / ll) as usize).min(4096) } else { 1 };
        for k in 0..reps {
            let base = c.start + k as f64 * ll;
            let limit = if ll > 0.0 { ll.min(c.len - k as f64 * ll) } else { c.len };
            for (ni, n) in c.midi_notes.iter().enumerate() {
                if n.start >= limit {
                    continue;
                }
                let s = base + n.start;
                out.push(TrkEv { s, e: s + n.len.min(limit - n.start).max(0.02), key: n.key as i64, vel: n.vel, clip: ci, note: ni });
            }
        }
    }
    out.sort_by(|a, b| a.s.total_cmp(&b.s).then(a.key.cmp(&b.key)));
    out
}

fn trk_build(p: &ProjectDoc, rev: u64) -> TrkLayout {
    let mut chans = Vec::new();
    let mut end = 0f64;
    for (ti, t) in p.tracks.iter().enumerate() {
        let evs = trk_track_events(t);
        end = end.max(evs.iter().map(|e| e.e).fold(0.0, f64::max));
        let mut lanes: Vec<Vec<TrkEv>> = alloc::vec![Vec::new()];
        let mut last_end: Vec<f64> = alloc::vec![f64::MIN];
        if t.is_audio() {
            lanes[0] = evs;
        } else {
            for ev in evs {
                let free = last_end.iter().position(|le| *le <= ev.s + 1e-6);
                let v = match free {
                    Some(v) => v,
                    None if lanes.len() < TRK_VOICES => {
                        lanes.push(Vec::new());
                        last_end.push(f64::MIN);
                        lanes.len() - 1
                    },
                    None => (0..lanes.len()).min_by(|a, b| last_end[*a].total_cmp(&last_end[*b])).unwrap_or(0),
                };
                last_end[v] = ev.e;
                lanes[v].push(ev);
            }
        }
        for (v, evs) in lanes.into_iter().enumerate() {
            chans.push(TrkChan { track: ti, voice: v, evs });
        }
    }
    TrkLayout { rev, chans, end }
}

fn trk_layout(p: &ProjectDoc, rev: u64) -> &'static TrkLayout {
    let slot = unsafe { &mut *(&raw mut TRK) };
    if slot.as_ref().map(|l| l.rev != rev).unwrap_or(true) {
        *slot = Some(trk_build(p, rev));
    }
    slot.as_ref().unwrap()
}

/// Cells of a block of rows: (key, velocity 0..1, hold) row by row for the
/// given channels. key -1 none, 128 an audio clip start; hold 0 none, 1 an
/// earlier note still sounds, 2 an earlier note ended in this row.
fn trk_page_cells(l: &TrkLayout, beat0: f64, rows: usize, rpb: f64, chans: &[usize]) -> Vec<(i64, f32, u8)> {
    let cn = chans.len();
    let rl = 1.0 / rpb;
    let mut cells = alloc::vec![(-1i64, 0f32, 0u8); rows * cn];
    for (ci, chi) in chans.iter().enumerate() {
        let Some(ch) = l.chans.get(*chi) else { continue };
        let i0 = ch.evs.partition_point(|e| e.e <= beat0 - 1e-9);
        for ev in &ch.evs[i0..] {
            if ev.s >= beat0 + rows as f64 * rl {
                break;
            }
            let on_row = libm::floor((ev.s - beat0) / rl + 1e-6);
            let off_row = libm::ceil((ev.e - beat0) / rl - 1e-6);
            if on_row >= 0.0 && (on_row as usize) < rows {
                let c = &mut cells[on_row as usize * cn + ci];
                c.0 = ev.key;
                c.1 = ev.vel;
            }
            let from = (on_row + 1.0).max(0.0) as usize;
            let to = (off_row.max(0.0) as usize).min(rows);
            for r in from..to {
                let c = &mut cells[r * cn + ci];
                if c.0 < 0 && c.2 == 0 {
                    c.2 = 1;
                }
            }
            if off_row >= 0.0 && (off_row as usize) < rows && ev.key != AUDIO_KEY {
                let c = &mut cells[off_row as usize * cn + ci];
                if c.0 < 0 {
                    c.2 = 2;
                }
            }
        }
    }
    cells
}

/// A list argument's numbers (a non-list gives an empty vector).
fn nums_arg(args: &[u64], i: usize) -> Vec<f64> {
    use crate::lingrt::{is_list, list_items};
    match args.get(i) {
        Some(&v) if is_list(v) => list_items(v).iter().map(|&x| to_f64(x)).collect(),
        _ => Vec::new(),
    }
}

// ── Sheet view (daw_sheet_*): a compact score layout of our own ──────────────
// Page 990 x 1400 (portrait). Every `bars`-bar stretch of every chosen
// instrument track with notes in it is one system: one staff (treble, bass by
// the track's average pitch, percussion for drums), five lines 10 apart. The
// geometry list is the PC's: [pw, ph, nsys, (track, b0, b1, x0, bar_w, y top,
// y bottom, nstaves, (clef, y bottom line, half-step)*)*, nnotes, (track, key,
// start, x, y)*] in page units.
const SH_PW: f64 = 990.0;
const SH_PH: f64 = 1400.0;
const SH_LEFT: f64 = 60.0;
const SH_RIGHT: f64 = 40.0;
const SH_TOP: f64 = 90.0;
const SH_BOTTOM: f64 = 60.0;
const SH_STEP: f64 = 5.0;
const SH_SYS_H: f64 = 110.0;

struct ShSys {
    track: usize,
    b0: f64,
    b1: f64,
    x0: f64,
    bar_w: f64,
    y0: f64,
    y1: f64,
    clef: u8,
    yb: f64,
}

struct ShNote {
    track: usize,
    key: u8,
    start: f64,
    x: f64,
    y: f64,
}

struct ShPage {
    systems: Vec<ShSys>,
    notes: Vec<ShNote>,
}

struct ShSheet {
    rev: u64,
    opts: String,
    pages: Vec<ShPage>,
}

static mut SHEET: Option<ShSheet> = None;

fn sh_opt<'a>(opts: &'a str, key: &str) -> &'a str {
    for kv in opts.split(';') {
        if let Some((k, v)) = kv.split_once('=') {
            if k.trim() == key {
                return v.trim();
            }
        }
    }
    ""
}

/// Diatonic number of a key (C-major letters; a sharp sits on the natural below).
fn sh_dn(key: u8) -> i32 {
    const LETTER: [i32; 12] = [0, 0, 1, 1, 2, 3, 3, 4, 4, 5, 5, 6];
    (key as i32 / 12) * 7 + LETTER[(key % 12) as usize]
}

/// Notes of a track in beats of the arrangement (clip loops unrolled).
fn sh_track_notes(t: &TrackDoc) -> Vec<(f64, f64, u8)> {
    let mut out = Vec::new();
    for c in &t.clips {
        if c.is_audio() {
            continue;
        }
        let ll = if c.loop_len >= 0.25 && c.loop_len < c.len { c.loop_len } else { 0.0 };
        let reps = if ll > 0.0 { (libm::ceil(c.len / ll) as usize).min(512) } else { 1 };
        for k in 0..reps {
            let base = c.start + k as f64 * ll;
            let limit = if ll > 0.0 { ll.min(c.len - k as f64 * ll) } else { c.len };
            for n in &c.midi_notes {
                if n.start < limit {
                    out.push((base + n.start, n.len.min(limit - n.start), n.key));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

fn sh_build(p: &ProjectDoc, opts: &str) -> Vec<ShPage> {
    let bars = sh_opt(opts, "bars").parse::<f64>().unwrap_or(4.0).clamp(1.0, 8.0);
    let from_bar = sh_opt(opts, "from").parse::<f64>().unwrap_or(1.0).max(1.0);
    let to_bar = sh_opt(opts, "to").parse::<f64>().unwrap_or(0.0);
    let chosen: Vec<usize> = sh_opt(opts, "tracks").split(',').filter_map(|s| s.trim().parse::<usize>().ok()).collect();
    let mut pages: Vec<ShPage> = Vec::new();
    let mut page = ShPage { systems: Vec::new(), notes: Vec::new() };
    let mut y = SH_TOP;
    let end = p.end_beat().max(4.0);
    let last_bar = if to_bar > 0.0 { to_bar } else { libm::ceil(end / 4.0) };
    let mut bar = from_bar;
    let x0 = SH_LEFT + 30.0;
    let bar_w = (SH_PW - x0 - SH_RIGHT) / bars;
    while bar <= last_bar {
        let b0 = (bar - 1.0) * 4.0;
        let b1 = b0 + bars * 4.0;
        for (ti, t) in p.tracks.iter().enumerate() {
            if t.is_audio() || (!chosen.is_empty() && !chosen.contains(&ti)) {
                continue;
            }
            let notes = sh_track_notes(t);
            let here: Vec<&(f64, f64, u8)> = notes.iter().filter(|n| n.0 >= b0 - 1e-9 && n.0 < b1 - 1e-9).collect();
            if here.is_empty() {
                continue;
            }
            if y + SH_SYS_H > SH_PH - SH_BOTTOM {
                pages.push(core::mem::replace(&mut page, ShPage { systems: Vec::new(), notes: Vec::new() }));
                y = SH_TOP;
            }
            let drums = t.instrument == "drums" || t.instrument == "drum";
            let avg = here.iter().map(|n| n.2 as f64).sum::<f64>() / here.len() as f64;
            let clef: u8 = if drums { 2 } else if avg < 57.0 { 1 } else { 0 };
            let yb = y + 20.0 + 8.0 * SH_STEP;
            let reference = if clef == 1 { sh_dn(43) } else { sh_dn(64) };
            for n in &here {
                let x = x0 + 14.0 + (n.0 - b0) / 4.0 * bar_w;
                let ny = if drums {
                    yb - ((n.2 as i32 % 8) as f64) * SH_STEP
                } else {
                    yb - ((sh_dn(n.2) - reference) as f64) * SH_STEP
                };
                page.notes.push(ShNote { track: ti, key: n.2, start: n.0, x, y: ny });
            }
            page.systems.push(ShSys { track: ti, b0, b1, x0, bar_w, y0: y, y1: y + SH_SYS_H - 20.0, clef, yb });
            y += SH_SYS_H;
        }
        bar += bars;
    }
    if !page.systems.is_empty() || pages.is_empty() {
        pages.push(page);
    }
    pages
}

fn sh_sheet(p: &ProjectDoc, rev: u64, opts: &str) -> &'static ShSheet {
    let slot = unsafe { &mut *(&raw mut SHEET) };
    if slot.as_ref().map(|s| s.rev != rev || s.opts != opts).unwrap_or(true) {
        *slot = Some(ShSheet { rev, opts: String::from(opts), pages: sh_build(p, opts) });
    }
    slot.as_ref().unwrap()
}

// ── dispatch ─────────────────────────────────────────────────────────────────

/// The `daw_*` builtins this engine implements; `None` for the rest (logged
/// once by the runtime as not implemented).
pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    if !name.starts_with(b"daw_") {
        return None;
    }
    // the calling app drives the DAW: its parking / closing pauses the transport
    crate::lingapp::note_daw_use();
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).unwrap_or(0.0);
    let u = |i: usize| n(i).max(0.0) as usize;
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let d = daw();
    tick(d);
    rec::ensure_samples(d);
    if let Some(v) = rec::dispatch(name, args, d) {
        return Some(v);
    }
    fx::ensure_ids(&mut d.project);
    route::apply_pending(d);
    if let Some(v) = fx::dispatch(name, args, d) {
        return Some(v);
    }
    if let Some(v) = edit::dispatch(name, args, d) {
        return Some(v);
    }
    if let Some(v) = route::dispatch(name, args, d) {
        return Some(v);
    }
    if let Some(v) = tools::dispatch(name, args, d) {
        return Some(v);
    }
    if let Some(v) = midi::dispatch(name, args, d) {
        return Some(v);
    }
    if let Some(v) = deco::dispatch(name, args, d) {
        return Some(v);
    }
    if let Some(v) = medley::dispatch(name, args, d) {
        return Some(v);
    }
    if let Some(v) = vedit::dispatch(name, args, d) {
        return Some(v);
    }
    let p = &mut d.project;
    let track = |p: &ProjectDoc, i: usize| p.tracks.get(i).cloned();
    Some(match name {
        // ── project ──
        b"daw_project_load" => {
            let path = s(0).replace('\\', "/");
            match load_into(d, &path) {
                Ok(()) => boolean(true),
                Err(e) => {
                    d.status = alloc::format!("Open failed: {e}");
                    num(0.0)
                },
            }
        },
        b"daw_new" => {
            d.project = ProjectDoc::empty("Untitled", 120.0);
            // a new project starts with the PC's gentle master compressor (ling-daw `ensure_master_comp`)
            if let Some(mut c) = fx::new_doc("Compressor") {
                for (k, v) in [("threshold", 0.833f32), ("ratio", 0.0526), ("attack", 0.146), ("release", 0.192), ("makeup", 0.5417)] {
                    if let Some(slot) = c.params.iter_mut().find(|x| x.0 == k) {
                        slot.1 = v;
                    }
                }
                d.project.master_effects.insert(0, c);
            }
            d.path.clear();
            d.dirty = false;
            d.rev += 1;
            d.playing = false;
            d.position = 0.0;
            edit::reset(d);
            UNIT
        },
        b"daw_project_name" => mk_str(&p.name),
        b"daw_set_project_name" => {
            p.name = s(0);
            d.dirty = true;
            d.rev += 1;
            UNIT
        },
        b"daw_project_path" => mk_str(&d.path),
        b"daw_set_project_path" => {
            d.path = s(0);
            UNIT
        },
        b"daw_status" => mk_str(&core::mem::take(&mut d.status)),
        b"daw_dirty" => boolean(d.dirty),
        b"daw_set_dirty" => {
            d.dirty = n(0) > 0.5;
            UNIT
        },
        b"daw_rev" => num(d.rev as f64),
        // No MIDI devices on LingOS yet (audio inputs + recording: `rec`).
        b"daw_device" => mk_str("LingOS audio"),
        b"daw_midi_inputs" => list_from(Vec::new()),
        b"daw_midi_target" => num(-1.0),
        b"daw_midi_channel"
        | b"daw_audio_config" | b"daw_set_device" | b"daw_midi_open" | b"daw_gm_sf2_set" => UNIT,
        // Saving needs a writable disk (no storage driver yet).
        // Save: `daw_project_save("")` to the project's own path,
        // `_save_as(path)` moves the project there, `_save_copy(path)` writes a
        // copy. project.json as ling-daw writes it, through crate::vfs.
        b"daw_project_save" | b"daw_project_save_copy" | b"daw_project_save_as" => {
            let arg = args.first().map(|&v| to_string(v)).unwrap_or_default();
            let target = if arg.is_empty() { d.path.clone() } else { arg };
            if target.is_empty() {
                d.status = String::from("No project path yet -- use Save As");
                boolean(false)
            } else {
                match serde_json::to_vec_pretty(&d.project) {
                    Ok(json) if crate::vfs::write(&target, &json) => {
                        if name != b"daw_project_save_copy" {
                            d.path = target.clone();
                            d.dirty = false;
                        }
                        d.status = alloc::format!("Saved {target}");
                        boolean(true)
                    },
                    _ => {
                        d.status = if crate::vfs::ready() {
                            alloc::format!("Could not write {target}")
                        } else {
                            String::from("Saving needs storage (no SD card driver on the Pi yet)")
                        };
                        boolean(false)
                    },
                }
            }
        },

        // ── transport ──
        b"daw_play" => {
            if !d.playing {
                d.playing = true;
                d.base_beat = d.position;
                d.base_ms = crate::arch::timer::now_ms();
                audio_start(d.position);
            }
            UNIT
        },
        b"daw_pause" => {
            route::finish_all(d.position);
            edit::finish_recording(d);
            route::apply_pending(d);
            audio_stop();
            d.playing = false;
            UNIT
        },
        b"daw_stop" => {
            route::finish_all(d.position);
            edit::finish_recording(d);
            route::apply_pending(d);
            audio_stop();
            {
                let a = audio();
                a.spos = 0.0;
                a.sess_play.iter_mut().for_each(|x| *x = None);
                a.sess_queue.iter_mut().for_each(|x| *x = None);
            }
            d.playing = false;
            d.position = 0.0;
            UNIT
        },
        b"daw_seek" => {
            d.position = n(0).max(0.0);
            if d.playing {
                audio_start(d.position);
            }
            d.base_beat = d.position;
            d.base_ms = crate::arch::timer::now_ms();
            UNIT
        },
        b"daw_playing" => boolean(d.playing),
        b"daw_position" => {
            clock_note(d);
            num(d.position)
        },
        b"daw_bpm" => num(p.tempo_bpm),
        b"daw_set_bpm" => {
            p.tempo_bpm = n(0).clamp(20.0, 400.0);
            d.dirty = true;
            d.rev += 1;
            UNIT
        },
        b"daw_tempo_points" => {
            list_from(p.tempo_map.iter().flat_map(|(b, v)| [num(*b), num(*v)]).collect())
        },
        b"daw_secs_at" => num(p.beats_to_secs(0.0, n(0).max(0.0))),
        b"daw_loop_on" => boolean(p.loop_on),
        b"daw_loop_start" => num(p.loop_start),
        b"daw_loop_end" => num(p.loop_end),
        b"daw_set_loop" => {
            p.loop_on = n(0) > 0.5;
            if args.len() >= 3 {
                p.loop_start = n(1).max(0.0);
                p.loop_end = n(2).max(n(1) + 0.25);
            }
            UNIT
        },
        b"daw_end_beat" => num(p.end_beat()),

        // ── master ──
        b"daw_master_get" => num(match s(0).as_str() {
            "vol" => p.master_level as f64,
            _ => 0.0,
        }),
        b"daw_master_set" => {
            if s(0) == "vol" {
                route::record(d, None, "vol", n(1).clamp(0.0, 2.0) as f32);
                let p = &mut d.project;
                p.master_level = n(1).clamp(0.0, 2.0) as f32;
                d.dirty = true;
            d.rev += 1;
            }
            UNIT
        },
        b"daw_limiter_get" => boolean(p.limiter),
        b"daw_limiter_set" => {
            p.limiter = n(0) > 0.5;
            UNIT
        },
        b"daw_arr_tags" => {
            // [start, end, tag]* from the project's "sections" meta
            let text = p.meta.iter().find(|(k, _)| k == "sections").map(|(_, v)| v.clone()).unwrap_or_default();
            let mut v = Vec::new();
            for part in text.split(';') {
                let f: Vec<&str> = part.split(':').collect();
                if f.len() >= 3 {
                    if let (Ok(a), Ok(b)) = (f[0].trim().parse::<f64>(), f[1].trim().parse::<f64>()) {
                        v.push(num(a));
                        v.push(num(b));
                        v.push(mk_str(f[2]));
                    }
                }
            }
            list_from(v)
        },

        // ── tracks ──
        b"daw_track_count" => num(p.tracks.len() as f64),
        b"daw_track_name" => mk_str(&track(p, u(0)).map(|t| t.name).unwrap_or_default()),
        b"daw_track_set_name" => {
            if let Some(t) = p.tracks.get_mut(u(0)) {
                t.name = s(1);
                d.dirty = true;
            d.rev += 1;
            }
            UNIT
        },
        b"daw_track_kind" => mk_str(
            &track(p, u(0)).map(|t| if t.is_audio() { String::from("audio") } else { t.instrument }).unwrap_or_default(),
        ),
        b"daw_track_inst" => mk_str(&match track(p, u(0)) {
            // the instrument's own name, as ling-daw's `Instrument::name()` gives it on the desktop
            Some(t) if !t.is_audio() => String::from(match t.instrument.as_str() {
                "synth" | "" => "Ling Synth X",
                "drums" | "drum" => "Ling Membrane",
                "prism" => "Ling Prism",
                "nova" => "Ling Nova",
                "growl" => "Ling Growl",
                "voice" => "Ling Voice",
                "chip" => "Ling Chip",
                "cinema" => "Ling Cinema",
                "pluck" => "Ling Pluck",
                "slicer" => "Ling Slicer",
                "sampler" => "Sampler",
                "sf2" => "SoundFont",
                "script" => "Script Synth",
                other => other,
            }),
            _ => String::from("Audio"),
        }),
        b"daw_track_patch" => mk_str(&track(p, u(0)).map(|t| t.patch).unwrap_or_default()),
        b"daw_track_get" => {
            let Some(t) = p.tracks.get(u(0)) else { return Some(num(0.0)) };
            num(match s(1).as_str() {
                "vol" => t.level as f64,
                "pan" => t.pan as f64,
                "mute" => t.muted as i32 as f64,
                "solo" => t.solo as i32 as f64,
                "audio" => t.is_audio() as i32 as f64,
                "cr" => ((t.color >> 16) & 255) as f64,
                "cg" => ((t.color >> 8) & 255) as f64,
                "cb" => (t.color & 255) as f64,
                "clips" => t.clips.len() as f64,
                "fx" => t.effects.len() as f64,
                _ => 0.0,
            })
        },
        b"daw_track_set" => {
            let v = n(2);
            if matches!(s(1).as_str(), "vol" | "pan") {
                let fv = if s(1) == "vol" { v.clamp(0.0, 2.0) } else { v.clamp(-1.0, 1.0) };
                route::record(d, Some(u(0)), &s(1), fv as f32);
            }
            let p = &mut d.project;
            if let Some(t) = p.tracks.get_mut(u(0)) {
                match s(1).as_str() {
                    "vol" => t.level = v.clamp(0.0, 2.0) as f32,
                    "pan" => t.pan = v.clamp(-1.0, 1.0) as f32,
                    "mute" => t.muted = v > 0.5,
                    "solo" => t.solo = v > 0.5,
                    "color" => t.color = PALETTE[(v.max(0.0) as usize) % 7],
                    _ => {},
                }
                d.dirty = true;
            d.rev += 1;
            }
            UNIT
        },
        b"daw_palette" => list_from(PALETTE.iter().map(|c| num(*c as f64)).collect()),

        // ── offline render (Render Audio): runs a slice per poll, see `render` ──
        b"daw_render_start" => boolean(render::start(p, &s(0), n(1), n(2), n(3), n(4) as u32, &s(5), n(7) > 0.5, n(8) > 0.5)),
        b"daw_render_state" => num(render::poll() as f64),
        b"daw_render_progress" => {
            render::poll();
            num(render::progress())
        },
        b"daw_render_elapsed" => num(render::elapsed()),
        b"daw_render_message" => mk_str(&render::message()),
        b"daw_render_ack" => {
            render::ack();
            UNIT
        },
        b"daw_render_cancel" => {
            render::cancel();
            UNIT
        },

        // ── clips ──
        b"daw_clip_count" => num(p.tracks.get(u(0)).map(|t| t.clips.len()).unwrap_or(0) as f64),
        b"daw_clip_get" => {
            let Some(c) = p.tracks.get(u(0)).and_then(|t| t.clips.get(u(1))) else { return Some(num(0.0)) };
            if s(2) == "seconds" {
                // the audio file's length (the arranger's waveform needs it: without it no waveform was drawn)
                let path = if c.is_audio() { c.audio_path.clone() } else { String::new() };
                return Some(num(if path.is_empty() { 0.0 } else { rec::sample_seconds(d, &path).unwrap_or(0.0) }));
            }
            num(match s(2).as_str() {
                "start" => c.start,
                "len" => c.len,
                "loop" => c.loop_len,
                "end" => c.end(),
                "audio" => c.is_audio() as i32 as f64,
                "notes" if !c.is_audio() => c.midi_notes.len() as f64,
                "gain" if c.is_audio() => c.gain as f64,
                "pitch" if c.is_audio() => c.pitch as f64,
                "warp" if c.is_audio() => c.warp as f64,
                "offset" if c.is_audio() => c.offset_s,
                _ => 0.0,
            })
        },
        b"daw_clip_set" => {
            let v = n(3);
            if let Some(c) = p.tracks.get_mut(u(0)).and_then(|t| t.clips.get_mut(u(1))) {
                let audio = c.is_audio();
                match s(2).as_str() {
                    "start" => c.start = v.max(0.0),
                    "len" => c.len = v.max(0.25),
                    "loop" => c.loop_len = if v < 0.25 { 0.0 } else { v },
                    "gain" if audio => c.gain = v.clamp(0.0, 4.0) as f32,
                    "pitch" if audio => c.pitch = v.clamp(-24.0, 24.0) as f32,
                    "offset" if audio => c.offset_s = v.max(0.0),
                    _ => {},
                }
                d.dirty = true;
            d.rev += 1;
            }
            UNIT
        },
        b"daw_clip_name" => mk_str(&p.tracks.get(u(0)).and_then(|t| t.clips.get(u(1))).map(|c| c.name.clone()).unwrap_or_default()),
        b"daw_clip_set_name" => {
            if let Some(c) = p.tracks.get_mut(u(0)).and_then(|t| t.clips.get_mut(u(1))) {
                c.name = s(2);
                d.dirty = true;
            d.rev += 1;
            }
            UNIT
        },
        b"daw_clip_notes" => {
            // flat [start,len,key,vel,pan,cutoff,reso] * N
            let Some(c) = p.tracks.get(u(0)).and_then(|t| t.clips.get(u(1))) else { return Some(list_from(Vec::new())) };
            if c.is_audio() {
                return Some(list_from(Vec::new()));
            }
            let mut v = Vec::with_capacity(c.midi_notes.len() * 7);
            for x in &c.midi_notes {
                v.extend([
                    num(x.start),
                    num(x.len),
                    num(x.key as f64),
                    num(x.vel as f64),
                    num(x.pan as f64),
                    num(x.cutoff as f64),
                    num(x.reso as f64),
                ]);
            }
            list_from(v)
        },
        b"daw_clip_remove" => {
            if let Some(t) = p.tracks.get_mut(u(0)) {
                if u(1) < t.clips.len() {
                    t.clips.remove(u(1));
                    d.dirty = true;
            d.rev += 1;
                }
            }
            UNIT
        },
        b"daw_clip_dup" => {
            let Some(t) = p.tracks.get_mut(u(0)) else { return Some(num(-1.0)) };
            let Some(mut c) = t.clips.get(u(1)).cloned() else { return Some(num(-1.0)) };
            c.start = n(2).max(0.0);
            t.clips.push(c);
            d.dirty = true;
            d.rev += 1;
            num((t.clips.len() - 1) as f64)
        },
        // ── project meta ──
        b"daw_meta_get" => mk_str(&p.meta.iter().find(|(k, _)| *k == s(0)).map(|(_, v)| v.clone()).unwrap_or_default()),
        b"daw_meta_set" => {
            let (key, val) = (s(0), s(1));
            match p.meta.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = val,
                None => p.meta.push((key, val)),
            }
            d.dirty = true;
            d.rev += 1;
            UNIT
        },
        // ── notes ──
        b"daw_note_add" => {
            // (t, c, start, len, key, vel) -> idx
            let Some(c) = p.tracks.get_mut(u(0)).and_then(|t| t.clips.get_mut(u(1))) else { return Some(num(-1.0)) };
            if c.is_audio() {
                return Some(num(-1.0));
            }
            c.midi_notes.push(NoteDoc::new(n(2).max(0.0), n(3).max(0.03), n(4).clamp(0.0, 127.0) as u8, n(5).clamp(0.01, 1.0) as f32));
            d.dirty = true;
            d.rev += 1;
            num((c.midi_notes.len() - 1) as f64)
        },
        b"daw_note_set" => {
            // (t, c, idx, field, v)
            if let Some(x) = p.tracks.get_mut(u(0)).and_then(|t| t.clips.get_mut(u(1))).and_then(|c| c.midi_notes.get_mut(u(2))) {
                let v = n(4);
                match s(3).as_str() {
                    "start" => x.start = v.max(0.0),
                    "len" => x.len = v.max(0.03),
                    "key" => x.key = v.clamp(0.0, 127.0) as u8,
                    "vel" => x.vel = v.clamp(0.01, 1.0) as f32,
                    "pan" => x.pan = v.clamp(-1.0, 1.0) as f32,
                    "cutoff" => x.cutoff = v.clamp(0.0, 1.0) as f32,
                    "reso" => x.reso = v.clamp(0.0, 1.0) as f32,
                    _ => {},
                }
            }
            d.dirty = true;
            d.rev += 1;
            UNIT
        },
        b"daw_note_remove" => {
            if let Some(c) = p.tracks.get_mut(u(0)).and_then(|t| t.clips.get_mut(u(1))) {
                if u(2) < c.midi_notes.len() {
                    c.midi_notes.remove(u(2));
                }
            }
            d.dirty = true;
            d.rev += 1;
            UNIT
        },
        b"daw_notes_apply" => {
            // (t, c, mode, selected indices, origin [start, key]* by note, origin lengths by note, p1, p2)
            // "move": p1 beats, p2 semitones  "resize": p1 extra beats, p2 shortest  "stretch": p1 anchor, p2 factor
            let (sel, orig, olen) = (nums_arg(args, 3), nums_arg(args, 4), nums_arg(args, 5));
            let (mode, p1, p2) = (s(2), n(6), n(7));
            if p1.is_finite() && p2.is_finite() {
                if let Some(c) = p.tracks.get_mut(u(0)).and_then(|t| t.clips.get_mut(u(1))) {
                    for si in sel {
                        let i = si.max(0.0) as usize;
                        let Some(x) = c.midi_notes.get_mut(i) else { continue };
                        let o_start = orig.get(i * 2).copied();
                        match mode.as_str() {
                            "move" => {
                                if let (Some(os), Some(ok)) = (o_start, orig.get(i * 2 + 1)) {
                                    x.start = (os + p1).max(0.0);
                                    x.key = (ok + p2).clamp(0.0, 127.0) as u8;
                                }
                            },
                            "resize" => {
                                if let Some(ol) = olen.get(i) {
                                    x.len = (ol + p1).max(p2.max(0.03));
                                }
                            },
                            "stretch" => {
                                if let (Some(os), Some(ol)) = (o_start, olen.get(i)) {
                                    x.start = (p1 + (os - p1) * p2).max(0.0);
                                    x.len = (ol * p2).max(0.06);
                                }
                            },
                            _ => {},
                        }
                    }
                }
                d.dirty = true;
                d.rev += 1;
            }
            UNIT
        },
        b"daw_note_lyrics" => {
            // one string per note; the Pi engine keeps no per-note lyrics yet
            let count = p.tracks.get(u(0)).and_then(|t| t.clips.get(u(1))).map(|c| c.midi_notes.len()).unwrap_or(0);
            list_from((0..count).map(|_| mk_str("")).collect())
        },
        // ── instrument parameters (the knobs of the Parameters view) ──
        b"daw_inst_params" => list_from(
            p.tracks.get(u(0)).map(|t| t.params()).unwrap_or_default().iter().flat_map(|(k, v)| [mk_str(k), num(*v as f64)]).collect(),
        ),
        b"daw_inst_get" => num(p.tracks.get(u(0)).and_then(|t| t.param(&s(1))).map(|v| v as f64).unwrap_or(-1.0)),
        b"daw_inst_get_many" => {
            use crate::lingrt::{is_list, list_items};
            let names: Vec<String> = match args.get(1) {
                Some(&v) if is_list(v) => list_items(v).iter().map(|&x| to_string(x)).collect(),
                _ => Vec::new(),
            };
            let t = p.tracks.get(u(0));
            list_from(names.iter().map(|nm| num(t.and_then(|t| t.param(nm)).map(|v| v as f64).unwrap_or(-1.0))).collect())
        },
        b"daw_inst_set" => {
            route::record(d, Some(u(0)), &s(1), n(2) as f32);
            let p = &mut d.project;
            if let Some(t) = p.tracks.get_mut(u(0)) {
                t.set_param(&s(1), n(2) as f32);
                d.dirty = true;
                d.rev += 1;
            }
            UNIT
        },
        // ── tracks: add, remove, duplicate, move ──
        b"daw_track_add" => {
            // (kind "synth"|"drums"|"audio"|..., optional patch path) -> new index
            let kind = s(0);
            let kind = if kind.is_empty() { String::from("synth") } else { kind };
            let audio = kind == "audio";
            let mut name = String::from(match kind.as_str() {
                "drums" => "Drums",
                "audio" => "Audio",
                "chip" => "Chip",
                "growl" => "Growl",
                "voice" => "Voice",
                "prism" => "Prism",
                "pluck" => "Pluck",
                "cinema" => "Cinema",
                "nova" => "Nova",
                _ => "Synth",
            });
            name.push_str(&alloc::format!(" {}", p.tracks.len() + 1));
            const COLORS: [u32; 8] = [0x4F9DFF, 0xFF6B6B, 0x6BD968, 0xFFB648, 0xB57BFF, 0x4FD1C5, 0xFF7AC6, 0xE5E05A];
            let mut t = TrackDoc {
                name,
                color: COLORS[p.tracks.len() % COLORS.len()],
                kind: String::from(if audio { "audio" } else { "instrument" }),
                instrument: if audio { String::new() } else { kind },
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
            let path = s(1).replace('\\', "/");
            if !audio && !path.is_empty() {
                if let Some(bytes) = crate::vfs::read(&path) {
                    apply_patch(&mut t, core::str::from_utf8(&bytes).unwrap_or(""), &path);
                }
            }
            p.tracks.push(t);
            d.dirty = true;
            d.rev += 1;
            num((p.tracks.len() - 1) as f64)
        },
        b"daw_track_remove" => {
            if u(0) < p.tracks.len() {
                p.remove_track(u(0));
                d.dirty = true;
                d.rev += 1;
            }
            UNIT
        },
        b"daw_track_dup" => {
            // (i, with_clips) -> new index, right after the original
            let Some(mut t) = p.tracks.get(u(0)).cloned() else { return Some(num(-1.0)) };
            if n(1) < 0.5 {
                t.clips.clear();
            }
            t.name.push_str(" copy");
            let at = u(0) + 1;
            p.insert_track(at, t);
            d.dirty = true;
            d.rev += 1;
            num(at as f64)
        },
        b"daw_track_move" => {
            // (i, delta) -> new index
            let i = u(0);
            let len = p.tracks.len();
            if len == 0 || i >= len {
                return Some(num(i as f64));
            }
            let j = (i as i64 + n(1) as i64).clamp(0, len as i64 - 1) as usize;
            if i != j {
                p.move_track(i, j);
                d.dirty = true;
                d.rev += 1;
            }
            num(j as f64)
        },
        b"daw_track_kind_drums" => boolean(track(p, u(0)).map(|t| t.instrument == "drums" || t.instrument == "drum").unwrap_or(false)),
        // the track's sound as a patch file (a user preset)
        b"daw_inst_save" => {
            let Some(t) = track(p, u(0)) else { return Some(num(0.0)) };
            let mut text = alloc::format!("# {} patch, saved on LingOS\nname = \"{}\"\ntype = {}\n", t.instrument, t.name, t.instrument);
            for (k, v) in t.params() {
                text.push_str(&alloc::format!("{}={:.4}\n", k.replace(' ', "_"), v));
            }
            num(crate::vfs::write(&s(1).replace('\\', "/"), text.as_bytes()) as i32 as f64)
        },
        // (track, what): "tables" = Nova's wavetable names (one per line, User last); other questions have no answer here
        b"daw_inst_info" => match (track(p, u(0)), s(1).as_str()) {
            (Some(t), "tables") if t.instrument == "nova" => mk_str(&plot::table_names()),
            _ => mk_str(""),
        },
        // ── instruments: switch the plugin, load a patch file, the built-in presets ──
        b"daw_track_set_kind" => {
            let Some(t) = p.tracks.get_mut(u(0)) else { return Some(num(0.0)) };
            let kind = s(1);
            if kind.is_empty() || t.is_audio() {
                return Some(num(0.0));
            }
            t.instrument = kind;
            t.state.clear();
            t.patch = s(2);
            d.dirty = true;
            d.rev += 1;
            num(1.0)
        },
        b"daw_inst_load" => {
            let path = s(1).replace('\\', "/");
            let ok = match (p.tracks.get_mut(u(0)), crate::vfs::read(&path)) {
                (Some(t), Some(bytes)) if !t.is_audio() => apply_patch(t, core::str::from_utf8(&bytes).unwrap_or(""), &path),
                _ => false,
            };
            if ok {
                d.dirty = true;
                d.rev += 1;
            }
            num(ok as i32 as f64)
        },
        b"daw_inst_presets" => {
            let kind = track(p, u(0)).map(|t| t.instrument).unwrap_or_default();
            list_from(builtin_presets(&kind).iter().map(|(name, _)| mk_str(name)).collect())
        },
        b"daw_inst_preset" => {
            let want = s(1);
            let kind = track(p, u(0)).map(|t| t.instrument).unwrap_or_default();
            let found = builtin_presets(&kind).into_iter().find(|(name, _)| name.eq_ignore_ascii_case(&want)).or_else(|| {
                // a sound asked for by name (the song generator's "Sitar"): any patch folder of the same kind
                ["instruments/GM", "instruments/Electronic GM", "instruments"]
                    .iter()
                    .flat_map(|dir| patch_files(dir))
                    .find(|(name, _)| name.eq_ignore_ascii_case(&want))
            });
            let ok = match (found, p.tracks.get_mut(u(0))) {
                (Some((_, path)), Some(t)) => match tools::factory_text(&path) {
                    Some(text) => apply_patch(t, text, &path),
                    None => match crate::vfs::read(&path) {
                        Some(bytes) => apply_patch(t, core::str::from_utf8(&bytes).unwrap_or(""), &path),
                        None => false,
                    },
                },
                _ => false,
            };
            if ok {
                d.dirty = true;
                d.rev += 1;
            }
            boolean(ok)
        },
        b"daw_inst_plot" => {
            // (track, what, points) -> 0..1 values: the cycle, Nova's table stack, filter, env, lfo, pitch, scale
            let pts = n(2).max(2.0) as usize;
            match p.tracks.get(u(0)) {
                Some(t) => list_from(plot::plot(t, &s(1), pts).into_iter().map(|v| num(v as f64)).collect()),
                None => list_from(Vec::new()),
            }
        },
        b"daw_inst_categories" | b"daw_inst_parts" => list_from(Vec::new()),
        b"daw_inst_preset_info" => mk_str(""),
        b"daw_inst_part" => num(0.0),
        b"daw_inst_ab_side" => num(-1.0),
        b"daw_inst_select_part" | b"daw_inst_ab" => UNIT,
        b"daw_inst_default" => {
            // (track, knob) -> what a fresh instrument of the kind has, -1 when unknown
            let Some(t) = track(p, u(0)) else { return Some(num(-1.0)) };
            let fresh = TrackDoc { state: Vec::new(), ..t };
            num(fresh.param(&s(1)).map(|v| v as f64).unwrap_or(-1.0))
        },
        b"daw_inst_random" => {
            // (track, seed): new knob values near the defaults (the PC's Growl / Nova / Chip / Cinema roll a new sound)
            let Some(t) = p.tracks.get_mut(u(0)) else { return Some(boolean(false)) };
            if t.is_audio() {
                return Some(boolean(false));
            }
            let mut rng = (n(1).abs() as u32).wrapping_mul(2654435761).wrapping_add(0x9e3779b9) | 1;
            let ps = t.params();
            for (k, v) in ps {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                if k == "level" || k.ends_with(".level") || k == "volume" {
                    continue;
                }
                let r = (rng & 0xffff) as f32 / 65535.0;
                t.set_param(&k, v + (r - v) * 0.6);
            }
            d.dirty = true;
            d.rev += 1;
            boolean(true)
        },
        // ── not on the Pi yet: theory analysis, sends / sidechains ──
        b"daw_th_keys" | b"daw_th_chords" | b"daw_th_phrases" | b"daw_th_roles" | b"daw_th_vocab" => list_from(Vec::new()),
        // ── Tracker view ──
        b"daw_trk_channels" => {
            let l = trk_layout(p, d.rev);
            list_from(l.chans.iter().flat_map(|c| [num(c.track as f64), num(c.voice as f64)]).collect())
        },
        b"daw_trk_length" => num(trk_layout(p, d.rev).end),
        b"daw_trk_page" => {
            // (first beat, rows, rows per beat, first channel, channels) -> [key, velocity 0..127, hold]* row by row
            let l = trk_layout(p, d.rev);
            let (beat0, rows, rpb) = (n(0), u(1).min(256), n(2).clamp(1.0, 16.0));
            let (c0, cn) = (u(3), u(4).min(64));
            let idx: Vec<usize> = (0..cn).map(|ci| c0 + ci).collect();
            let cells = trk_page_cells(l, beat0, rows, rpb, &idx);
            list_from(cells.into_iter().flat_map(|(k, v, h)| [num(k as f64), num(libm::round((v * 127.0) as f64).clamp(0.0, 127.0)), num(h as f64)]).collect())
        },
        b"daw_trk_ring" => {
            // (track, first beat, beats, slots) -> loudest note start (0..1) in each slot
            let l = trk_layout(p, d.rev);
            let (t, beat0, beats, slots) = (u(0), n(1), n(2).max(0.25), u(3).clamp(1, 128));
            let mut out = alloc::vec![0f64; slots];
            for ch in l.chans.iter().filter(|c| c.track == t) {
                let i0 = ch.evs.partition_point(|e| e.e <= beat0 - 1e-9);
                for ev in &ch.evs[i0..] {
                    if ev.s >= beat0 + beats {
                        break;
                    }
                    if ev.s >= beat0 - 1e-9 {
                        let k = (((ev.s - beat0) / beats) * slots as f64) as usize;
                        if k < slots {
                            out[k] = out[k].max(ev.vel as f64);
                        }
                    }
                }
            }
            list_from(out.into_iter().map(num).collect())
        },
        b"daw_trk_set" => {
            // (channel, beat, row length, key (-1 clears), velocity 0..127, length in beats (0 = one row)) -> 1 when changed
            let (chi, beat, rl, key) = (u(0), n(1), n(2).max(0.01), n(3) as i64);
            let vel = (n(4) / 127.0).clamp(0.02, 1.0) as f32;
            let len = if n(5) > 0.0 { n(5) } else { rl };
            let (track, kill) = {
                let l = trk_layout(p, d.rev);
                let Some(ch) = l.chans.get(chi) else { return Some(boolean(false)) };
                let mut kill: Vec<(usize, usize)> =
                    ch.evs.iter().filter(|e| e.s >= beat - 1e-6 && e.s < beat + rl - 1e-6 && e.note != usize::MAX).map(|e| (e.clip, e.note)).collect();
                kill.sort_unstable();
                kill.dedup();
                (ch.track, kill)
            };
            let Some(t) = p.tracks.get_mut(track) else { return Some(boolean(false)) };
            if t.is_audio() {
                return Some(boolean(false));
            }
            let mut changed = !kill.is_empty();
            for (ci, ni) in kill.into_iter().rev() {
                if let Some(c) = t.clips.get_mut(ci) {
                    if ni < c.midi_notes.len() {
                        c.midi_notes.remove(ni);
                    }
                }
            }
            if (0..128).contains(&key) {
                let ci = match t.clips.iter().position(|c| !c.is_audio() && c.start <= beat + 1e-9 && beat < c.start + c.len - 1e-9) {
                    Some(i) => i,
                    None => {
                        let start = libm::floor(beat / 4.0) * 4.0;
                        t.clips.push(ClipDoc {
                            name: String::from("Pattern"),
                            start,
                            ..ClipDoc::default()
                        });
                        t.clips.len() - 1
                    },
                };
                let c = &mut t.clips[ci];
                let mut local = beat - c.start;
                if c.loop_len >= 0.25 && c.loop_len < c.len {
                    local %= c.loop_len;
                }
                c.midi_notes.push(NoteDoc::new(local, len, key as u8, vel));
                changed = true;
            }
            if changed {
                d.dirty = true;
                d.rev += 1;
            }
            boolean(changed)
        },
        b"daw_trk_pdf" | b"daw_trk_export" => list_from(Vec::new()),
        // ── Sheet view ──
        b"daw_sheet_scan" => {
            // -> [bars, then (track, name, notes, drums 0/1, has lyrics 0/1)*] for every instrument track with notes
            let mut v = alloc::vec![num(libm::ceil(p.end_beat().max(4.0) / 4.0))];
            for (i, t) in p.tracks.iter().enumerate() {
                if t.is_audio() {
                    continue;
                }
                let count: usize = t.clips.iter().map(|c| c.midi_notes.len()).sum();
                if count == 0 {
                    continue;
                }
                let drums = t.instrument == "drums" || t.instrument == "drum";
                v.extend([num(i as f64), mk_str(&t.name), num(count as f64), num(drums as u8 as f64), num(0.0)]);
            }
            list_from(v)
        },
        b"daw_sheet_png" => {
            // (options, fonts folder, page, page width on screen) -> [pages, 0, 0, 0, page height on screen, generation, ""]:
            // no picture on the Pi (the view draws the page from daw_sheet_geo), the rest as the PC reports it
            let opts = s(0);
            let pw = n(3).max(100.0);
            let sheet = sh_sheet(p, d.rev, &opts);
            let pages = sheet.pages.iter().filter(|pg| !pg.systems.is_empty()).count();
            if pages == 0 {
                list_from(Vec::new())
            } else {
                list_from(alloc::vec![num(pages as f64), num(0.0), num(0.0), num(0.0), num(SH_PH * pw / SH_PW), num(d.rev as f64), mk_str("")])
            }
        },
        b"daw_sheet_geo" => {
            // (options, page) -> the geometry list (see sh_build); [] for no such page
            let opts = s(0);
            let sheet = sh_sheet(p, d.rev, &opts);
            let Some(pg) = sheet.pages.get(u(1)) else { return Some(list_from(Vec::new())) };
            if pg.systems.is_empty() {
                return Some(list_from(Vec::new()));
            }
            let mut v = alloc::vec![num(SH_PW), num(SH_PH), num(pg.systems.len() as f64)];
            for sy in &pg.systems {
                v.extend([num(sy.track as f64), num(sy.b0), num(sy.b1), num(sy.x0), num(sy.bar_w), num(sy.y0), num(sy.y1), num(1.0)]);
                v.extend([num(sy.clef as f64), num(sy.yb), num(SH_STEP)]);
            }
            v.push(num(pg.notes.len() as f64));
            for q in &pg.notes {
                v.extend([num(q.track as f64), num(q.key as f64), num(q.start), num(q.x), num(q.y)]);
            }
            list_from(v)
        },
        b"daw_sheet_edit" => {
            // (track, op, key, start beat, value) -> 1 when something changed. op: 0 delete, 1 transpose by value,
            // 2 move by value beats, 3 set length, 4 add a note of length value at that beat
            let (ti, op, key, start, val) = (u(0), n(1) as i64, n(2).clamp(0.0, 127.0) as u8, n(3).max(0.0), n(4));
            if !start.is_finite() || !val.is_finite() {
                return Some(num(0.0));
            }
            let mut changed = false;
            if let Some(t) = p.tracks.get_mut(ti) {
                if op == 4 {
                    let ci = match t.clips.iter().position(|c| !c.is_audio() && start >= c.start && start < c.start + c.len) {
                        Some(i) => i,
                        None => {
                            let s0 = libm::floor(start / 4.0) * 4.0;
                            t.clips.push(ClipDoc {
                                name: String::from("Pattern"),
                                start: s0,
                                ..ClipDoc::default()
                            });
                            t.clips.len() - 1
                        },
                    };
                    let c = &mut t.clips[ci];
                    let rel = start - c.start;
                    let len = val.max(0.0625);
                    if rel + len > c.len {
                        c.len = libm::ceil((rel + len) / 4.0) * 4.0;
                    }
                    c.midi_notes.push(NoteDoc::new(rel, len, key, 0.8));
                    changed = true;
                } else {
                    'find: for c in t.clips.iter_mut() {
                        if c.is_audio() {
                            continue;
                        }
                        let (cs, clen) = (c.start, c.len);
                        let ll = if c.loop_len >= 0.25 && c.loop_len < c.len { c.loop_len } else { 0.0 };
                        let reps = if ll > 0.0 { (libm::ceil(clen / ll) as usize).min(512) } else { 1 };
                        for k in 0..reps {
                            let base = cs + k as f64 * ll;
                            let rel = start - base;
                            for i in 0..c.midi_notes.len() {
                                let nn = &c.midi_notes[i];
                                if nn.key == key && rel >= nn.start - 0.01 && rel < nn.start + nn.len.max(0.05) + 0.01 {
                                    match op {
                                        0 => {
                                            c.midi_notes.remove(i);
                                        },
                                        1 => c.midi_notes[i].key = (nn.key as f64 + libm::round(val)).clamp(0.0, 127.0) as u8,
                                        2 => c.midi_notes[i].start = (nn.start + val).max(0.0),
                                        3 => c.midi_notes[i].len = val.max(0.0625),
                                        _ => break 'find,
                                    }
                                    changed = true;
                                    break 'find;
                                }
                            }
                        }
                    }
                }
            }
            if changed {
                d.dirty = true;
                d.rev += 1;
            }
            num(changed as u8 as f64)
        },
        b"daw_sheet_write" | b"daw_sheet_xml" => list_from(Vec::new()),
        b"daw_sheet_musescore_find" | b"daw_sheet_musescore_open" => mk_str(""),
        _ => return None,
    })
}

// ── step 2: sound ────────────────────────────────────────────────────────────
// A polyphonic synth + note scheduler rendering the loaded project into the
// kernel mixer (x86: AC97 via `drivers::mixer`, which pulls `block`). It
// follows the project's own clock: notes of every audible track (mute/solo,
// clip loops) are scheduled sample-accurately per block, synth tracks play
// detuned saws + a square sub through a resonant low-pass with an envelope,
// drum tracks a synthesized General MIDI kit. Track level/pan, note velocity/
// pan/cutoff/resonance, master level and the limiter are honoured. It is not
// ling-daw's instrument/effect DSP (Prism, the sampler, reverb...) — a stand-in
// so a project plays recognisably until those are ported.

const SR: f64 = 48_000.0;
const MAX_VOICES: usize = 64;

#[derive(Clone, Copy, PartialEq)]
enum VKind {
    Synth,
    Kick,
    Snare,
    Hat,
    OpenHat,
    Tom,
    Cymbal,
    Click,
}

#[derive(Clone, Copy)]
struct Voice {
    track: usize,
    kind: VKind,
    freq: f64,
    vel: f64,
    pan: f64,
    cutoff: f64,
    reso: f64,
    /// seconds since note-on / since release
    t: f64,
    rel_t: f64,
    gate: bool,
    off_beat: f64,
    phase: [f64; 3],
    lp: [f64; 2],
    noise: u32,
    env: f64,
    sp: SynthP,
    /// a `daw_live` key held from the keyboard (-1: a note of the song)
    live: i16,
    /// started by a launcher clip (a stop of the track's launcher releases it)
    sess: bool,
}

/// A track's knobs as the Pi voice uses them, read once at note-on. Every
/// LingStationX instrument has its own knob names (Synth X "cutoff", Prism
/// "cut", Nova "e1 a" ...); they map onto one subtractive voice with a
/// character per instrument (Chip: bare pulse, Pluck: no sustain, Growl:
/// driven, Voice: vibrato and breath, Cinema: noise).
#[derive(Clone, Copy)]
struct SynthP {
    a: f64,
    d: f64,
    s: f64,
    r: f64,
    cutoff: f64,
    reso: f64,
    fenv: f64,
    wave: f64,
    duty: f64,
    sub: f64,
    detune: f64,
    noise: f64,
    drive: f64,
    lfo_hz: f64,
    lfo_cut: f64,
    lfo_pitch: f64,
    chip: bool,
    /// drums: pitch and decay multipliers and level for the piece
    tune: f64,
    decay: f64,
    level: f64,
}

fn synth_params(t: &TrackDoc, piece: Option<&str>, auto: &[(String, f32)]) -> SynthP {
    let mut ps = t.params();
    // knobs automation is moving right now
    for (k, v) in auto {
        match ps.iter_mut().find(|(n, _)| n == k) {
            Some(slot) => slot.1 = *v,
            None => ps.push((k.clone(), *v)),
        }
    }
    let get = |names: &[&str], def: f64| -> f64 {
        names.iter().find_map(|nm| ps.iter().find(|(n, _)| n == nm).map(|(_, v)| *v as f64)).unwrap_or(def).clamp(0.0, 1.0)
    };
    let secs = |x: f64, lo: f64, span: f64| lo + x * x * span;
    let kind = t.instrument.as_str();
    let mut p = SynthP {
        a: secs(get(&["attack", "amp a", "e1 a"], 0.1), 0.002, 2.0),
        d: secs(get(&["decay", "amp d", "e1 d"], 0.4), 0.03, 3.0),
        s: get(&["sustain", "amp s", "e1 s"], 0.7),
        r: secs(get(&["release", "amp r", "e1 r"], 0.3), 0.01, 3.0),
        cutoff: get(&["cutoff", "cut", "bright"], 0.6),
        reso: get(&["reso", "res"], 0.2),
        fenv: get(&["filt env", "fenv"], 0.3),
        wave: get(&["wave", "a wave"], 0.0),
        duty: 0.5,
        sub: get(&["sub", "sub level"], 0.3),
        detune: get(&["detune", "a det"], 0.15),
        noise: get(&["noise", "breath"], 0.0),
        drive: get(&["drive", "drv"], 0.0),
        lfo_hz: secs(get(&["lfo rate", "vib rate"], 0.3), 0.1, 12.0),
        lfo_cut: get(&["lfo cut"], 0.0),
        lfo_pitch: get(&["lfo pitch", "vib depth"], 0.0),
        chip: false,
        tune: 1.0,
        decay: 1.0,
        level: 1.0,
    };
    match kind {
        "chip" => {
            p.chip = true;
            p.wave = 1.0;
            p.duty = 0.08 + 0.42 * get(&["duty"], 0.5);
            p.sub = 0.0;
            p.detune = 0.0;
            p.a = 0.002;
        },
        "pluck" => {
            p.s = 0.0;
            p.d = secs(get(&["str decay", "decay"], 0.4), 0.08, 1.5);
            p.fenv = p.fenv.max(0.5);
        },
        "growl" => {
            p.drive = p.drive.max(0.4);
            p.detune = p.detune.max(0.5);
            p.wave = 0.3;
        },
        "voice" => {
            p.cutoff = 0.45;
            p.reso = 0.55;
            p.lfo_hz = 4.0 + 3.0 * get(&["vib rate"], 0.5);
            p.lfo_pitch = get(&["vib depth"], 0.3) * 1.5;
            p.noise = get(&["breath"], 0.2) * 0.3;
            p.sub = 0.0;
        },
        "cinema" => {
            p.noise = 0.5 + 0.5 * get(&["noise"], 0.5);
            p.r = secs(get(&["length"], 0.5), 0.3, 4.0);
            p.a = p.a.max(0.02);
        },
        _ => {},
    }
    // a string patch (GM guitars, harps) plucks
    if get(&["string"], 0.0) > 0.5 {
        p.s = 0.0;
        p.d = secs(get(&["str decay"], 0.4), 0.08, 2.0);
    }
    if let Some(pc) = piece {
        let g = |f: &str, def: f64| {
            let key = alloc::format!("{pc}.{f}");
            ps.iter().find(|(n, _)| *n == key).map(|(_, v)| *v as f64).unwrap_or(def).clamp(0.0, 1.0)
        };
        p.tune = libm::pow(2.0, (g("tune", 0.5) - 0.5) * 1.5);
        p.decay = libm::pow(2.0, (0.5 - g("decay", 0.5)) * 2.5);
        p.level = g("level", 0.8) / 0.8;
    }
    p
}

struct Audio {
    running: bool,
    beat: f64,
    voices: Vec<Voice>,
    buf: Vec<i16>,
    /// Scheduling stops here (an offline render's end); live playback: infinity.
    stop_beat: f64,
    /// The audio clock: when the last block was rendered by a real output
    /// device (0 = never / not a device), and how many frames that device
    /// keeps queued ahead of the speaker.
    clock_us: u64,
    lat_frames: f64,
    /// Last playhead handed to the UI (kept monotonic between seeks/loops).
    shown: f64,
    shown_us: u64,
    /// the running effect instances of this engine (see `fx`)
    racks: fx::Racks,
    /// an offline render: the launcher does not play (as ling-daw's export)
    offline: bool,
    /// the launcher's clock (beats since play, never loops) and per track: the clip playing (clip, scene, start on that
    /// clock) and a queued launch (clip or -1 = stop, scene, when)
    spos: f64,
    sess_play: Vec<Option<(usize, i32, f64)>>,
    sess_queue: Vec<Option<(i64, i32, f64)>>,
    /// per track: instrument knobs as automation has them now (new notes start with these)
    inst_auto: Vec<Vec<(String, f32)>>,
}

impl Audio {
    const fn new() -> Audio {
        Audio { running: false, beat: 0.0, voices: Vec::new(), buf: Vec::new(), stop_beat: f64::INFINITY, clock_us: 0, lat_frames: 0.0, shown: -1.0, shown_us: 0, racks: fx::Racks::new(), offline: false, spos: 0.0, sess_play: Vec::new(), sess_queue: Vec::new(), inst_auto: Vec::new() }
    }
}

static mut AUDIO: Audio = Audio::new();

fn audio() -> &'static mut Audio {
    unsafe { &mut *(&raw mut AUDIO) }
}

/// Whether the engine is producing sound (the transport runs or notes ring out).
pub fn engine_running() -> bool {
    let a = unsafe { &*(&raw const AUDIO) };
    a.running || !a.voices.is_empty()
}

/// Pause the transport (the desktop does this when LingStationX is suspended
/// or closed, if the "pause music" setting is on). Nothing if no project is up.
pub fn pause_transport() {
    if let Some(d) = unsafe { (*(&raw mut DAW)).as_mut() } {
        if d.playing {
            audio_stop();
            d.playing = false;
        }
    }
}

fn audio_start(beat: f64) {
    let a = audio();
    a.voices.clear();
    a.beat = beat;
    a.running = true;
    // the clock restarts with the first block the device pulls
    a.clock_us = 0;
    a.shown = -1.0;
    a.shown_us = 0;
}

fn audio_stop() {
    let a = audio();
    a.running = false;
    for v in a.voices.iter_mut() {
        v.gate = false;
    }
}

fn drum_kind(key: u8) -> (VKind, f64) {
    match key {
        35 | 36 => (VKind::Kick, 55.0),
        37 | 39 => (VKind::Click, 1800.0),
        38 | 40 => (VKind::Snare, 190.0),
        42 | 44 => (VKind::Hat, 0.0),
        46 => (VKind::OpenHat, 0.0),
        41 | 43 | 45 | 47 | 48 | 50 => (VKind::Tom, 70.0 + (key as f64 - 41.0) * 14.0),
        49 | 51 | 52 | 53 | 55 | 57 | 59 => (VKind::Cymbal, 0.0),
        _ => (VKind::Click, 900.0),
    }
}

fn note_on(a: &mut Audio, track: usize, t: &TrackDoc, drums: bool, n: &NoteDoc, off_beat: f64) {
    let (kind, freq) = if drums {
        drum_kind(n.key)
    } else {
        (VKind::Synth, 440.0 * libm::pow(2.0, (n.key as f64 - 69.0) / 12.0))
    };
    let piece = match kind {
        VKind::Synth => None,
        VKind::Kick => Some("kick"),
        VKind::Snare => Some("snare"),
        VKind::Hat => Some("closed hat"),
        VKind::OpenHat => Some("open hat"),
        VKind::Tom => Some("tom"),
        VKind::Cymbal => Some("crash"),
        VKind::Click => Some("clap"),
    };
    let sp = synth_params(t, piece, a.inst_auto.get(track).map(|v| v.as_slice()).unwrap_or(&[]));
    let freq = if drums { freq * sp.tune } else { freq };
    let v = Voice {
        track,
        kind,
        freq,
        vel: (n.vel as f64).clamp(0.0, 1.0),
        pan: (n.pan as f64).clamp(-1.0, 1.0),
        cutoff: (n.cutoff as f64).clamp(0.0, 1.0),
        reso: (n.reso as f64).clamp(0.0, 1.0),
        t: 0.0,
        rel_t: 0.0,
        gate: true,
        off_beat,
        phase: [0.0; 3],
        lp: [0.0; 2],
        noise: 0x1234_5678 ^ (n.key as u32).wrapping_mul(2654435761),
        env: 0.0,
        sp,
        live: -1,
        sess: false,
    };
    if a.voices.len() >= MAX_VOICES {
        // steal the oldest released voice, else the oldest
        let i = a.voices.iter().position(|v| !v.gate).unwrap_or(0);
        a.voices.remove(i);
    }
    a.voices.push(v);
}

#[inline]
fn noise(s: &mut u32) -> f64 {
    let mut x = *s;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *s = x;
    (x as f64 / u32::MAX as f64) * 2.0 - 1.0
}

/// One sample of a voice; false once it has died away.
fn voice_sample(v: &mut Voice, dt: f64) -> Option<f64> {
    // a drum's kit decay plays its envelope faster or slower
    v.t += if matches!(v.kind, VKind::Synth) { dt } else { dt * v.sp.decay };
    if !v.gate {
        v.rel_t += dt;
    }
    let s = match v.kind {
        VKind::Synth => {
            let p = v.sp;
            if v.gate {
                v.env = if v.t < p.a { v.t / p.a } else { p.s + (1.0 - p.s) * libm::exp(-(v.t - p.a) / p.d) };
            } else {
                v.env *= libm::exp(-dt * 3.0 / p.r);
            }
            // released, or a pluck that has rung out
            if v.env < 0.0005 && (!v.gate || (p.s < 0.001 && v.t > p.a)) {
                return None;
            }
            let lfo = libm::sin(2.0 * core::f64::consts::PI * p.lfo_hz * v.t);
            let f = v.freq * (1.0 + p.lfo_pitch * 0.02 * lfo);
            let det = 1.0 + p.detune * 0.012;
            let mut osc = 0.0;
            for (i, m) in [1.0f64, det].iter().enumerate() {
                v.phase[i] += f * m * dt;
                v.phase[i] -= libm::floor(v.phase[i]);
                let ph = v.phase[i];
                let pulse = if ph < p.duty { 1.0 } else { -1.0 };
                osc += (2.0 * ph - 1.0) * (1.0 - p.wave) + pulse * p.wave;
            }
            osc *= 0.5;
            v.phase[2] += f * 0.5 * dt;
            v.phase[2] -= libm::floor(v.phase[2]);
            osc += p.sub * 0.6 * if v.phase[2] < 0.5 { 1.0 } else { -1.0 };
            if p.noise > 0.0 {
                osc += p.noise * 0.6 * noise(&mut v.noise);
            }
            if p.drive > 0.0 {
                osc = libm::tanh(osc * (1.0 + p.drive * 5.0));
            }
            if p.chip {
                osc * 0.45 * v.env
            } else {
                // two-pole low-pass: the track's cutoff (+ the note's), its envelope and LFO
                let cut = (p.cutoff + (v.cutoff - 0.5) + p.lfo_cut * 0.3 * lfo).clamp(0.0, 1.0);
                let fe = p.fenv * libm::exp(-v.t / (0.05 + p.d * 0.5));
                let fc = ((60.0 + 14_000.0 * cut * cut) * (1.0 + 4.0 * fe)).min(18_000.0);
                let g = 1.0 - libm::exp(-2.0 * core::f64::consts::PI * fc * dt);
                let fb = (p.reso + (v.reso - 0.5) * 0.5).clamp(0.0, 1.0) * 0.85;
                v.lp[0] += g * (osc - v.lp[0] - fb * (v.lp[1] - v.lp[0]));
                v.lp[1] += g * (v.lp[0] - v.lp[1]);
                v.lp[1] * v.env
            }
        },
        VKind::Kick => {
            let f = v.freq + 110.0 * libm::exp(-v.t * 30.0);
            v.phase[0] += f * dt;
            let e = libm::exp(-v.t * 9.0);
            if e < 0.0005 {
                return None;
            }
            libm::sin(2.0 * core::f64::consts::PI * v.phase[0]) * e * 1.2
        },
        VKind::Snare => {
            v.phase[0] += v.freq * dt;
            let tone = libm::sin(2.0 * core::f64::consts::PI * v.phase[0]) * libm::exp(-v.t * 25.0);
            let nz = noise(&mut v.noise) * libm::exp(-v.t * 14.0);
            if v.t > 0.5 {
                return None;
            }
            (tone * 0.5 + nz * 0.7) * 0.8
        },
        VKind::Hat | VKind::OpenHat | VKind::Cymbal => {
            let decay = match v.kind {
                VKind::Hat => 45.0,
                VKind::OpenHat => 9.0,
                _ => 3.5,
            };
            let e = libm::exp(-v.t * decay);
            if e < 0.0005 {
                return None;
            }
            // high-passed noise
            let nz = noise(&mut v.noise);
            let hp = nz - v.lp[0];
            v.lp[0] += 0.35 * (nz - v.lp[0]);
            hp * e * 0.45
        },
        VKind::Tom => {
            let f = v.freq * (1.0 + 0.5 * libm::exp(-v.t * 20.0));
            v.phase[0] += f * dt;
            let e = libm::exp(-v.t * 7.0);
            if e < 0.0005 {
                return None;
            }
            libm::sin(2.0 * core::f64::consts::PI * v.phase[0]) * e * 0.9
        },
        VKind::Click => {
            v.phase[0] += v.freq * dt;
            let e = libm::exp(-v.t * 40.0);
            if e < 0.0005 {
                return None;
            }
            (libm::sin(2.0 * core::f64::consts::PI * v.phase[0]) * 0.5 + noise(&mut v.noise) * 0.3) * e
        },
    };
    let lvl = if matches!(v.kind, VKind::Synth) { 1.0 } else { v.sp.level };
    Some(s * lvl * (0.35 + 0.65 * v.vel))
}

/// Schedule every note starting in [b0, b1) on audible tracks.
fn schedule(a: &mut Audio, p: &ProjectDoc, b0: f64, b1: f64) {
    let any_solo = p.tracks.iter().any(|t| t.solo);
    for (ti, t) in p.tracks.iter().enumerate() {
        if t.is_audio() || ((t.muted || (any_solo && !t.solo)) && !route::is_source(p, ti)) {
            continue;
        }
        let drums = t.instrument == "drums" || t.instrument == "drum";
        for c in &t.clips {
            if c.is_audio() || c.end() <= b0 || c.start >= b1 {
                continue;
            }
            let span = if c.loop_len > 0.0 { c.loop_len } else { c.len };
            let reps = if c.loop_len > 0.0 { libm::ceil(c.len / c.loop_len).max(1.0) as usize } else { 1 };
            for k in 0..reps {
                let off = c.start + k as f64 * span;
                if off >= b1 || off + span <= b0 {
                    continue;
                }
                for n in &c.midi_notes {
                    if n.start >= span {
                        continue;
                    }
                    let on = off + n.start;
                    if on >= c.end() || on < b0 || on >= b1 {
                        continue;
                    }
                    let off_beat = (on + n.len.max(0.02)).min(c.end());
                    note_on(a, ti, t, drums, n, off_beat);
                }
            }
        }
    }
}

/// Render `frames` stereo frames of the playing project, or None when silent
/// (nothing playing and every voice has died away). Not an audio clock: a
/// caller that pulls blocks on its own schedule (the Pi's per-frame fallback
/// with no HDMI sink) must not drive the playhead.
pub fn block(frames: usize) -> Option<&'static [i16]> {
    block_inner(frames, None)
}

/// `block` for a real output device that keeps `queued_frames` ahead of the
/// speaker: the playhead then follows the samples actually being heard,
/// instead of a separate wall clock that drifts from the audio.
pub fn block_clocked(frames: usize, queued_frames: usize) -> Option<&'static [i16]> {
    block_inner(frames, Some(queued_frames))
}

fn block_inner(frames: usize, clock: Option<usize>) -> Option<&'static [i16]> {
    let a = audio();
    if !a.running && a.voices.is_empty() {
        return None;
    }
    let d = unsafe { (*(&raw mut DAW)).as_mut() }?;
    fx::ensure_ids(&mut d.project);
    let mut fbuf: Vec<f32> = alloc::vec![0.0; frames * 2];
    synth(a, &d.project, &mut fbuf);
    if a.buf.len() < frames * 2 {
        a.buf.resize(frames * 2, 0);
    }
    for (o, x) in a.buf.iter_mut().zip(fbuf.iter()) {
        *o = (*x * 32000.0) as i16;
    }
    if let Some(q) = clock {
        a.clock_us = crate::arch::timer::now_us().max(1);
        a.lat_frames = q as f64;
    }
    Some(&a.buf[..frames * 2])
}

/// The project's sound: `out.len() / 2` stereo frames, after the master
/// level and the soft limiter (-1..1). Shared by playback and offline render.
fn synth(a: &mut Audio, p: &ProjectDoc, out: &mut [f32]) {
    // as ling-daw's render thread: denormals flushed to zero while the engine runs, so a reverb / delay / filter tail
    // decaying towards silence does not cost 10-100x the CPU (scoped: the caller's float mode comes back after)
    let saved = ftz_on();
    synth_inner(a, p, out);
    ftz_restore(saved);
}

#[cfg(target_arch = "x86_64")]
fn ftz_on() -> u64 {
    let mut mx: u32 = 0;
    unsafe {
        core::arch::asm!("stmxcsr [{}]", in(reg) &mut mx, options(nostack));
        let on = mx | 0x8040;
        core::arch::asm!("ldmxcsr [{}]", in(reg) &on, options(nostack, readonly));
    }
    mx as u64
}
#[cfg(target_arch = "x86_64")]
fn ftz_restore(saved: u64) {
    let mx = saved as u32;
    unsafe { core::arch::asm!("ldmxcsr [{}]", in(reg) &mx, options(nostack, readonly)) };
}
#[cfg(target_arch = "aarch64")]
fn ftz_on() -> u64 {
    let fpcr: u64;
    unsafe {
        core::arch::asm!("mrs {}, fpcr", out(reg) fpcr);
        core::arch::asm!("msr fpcr, {}", in(reg) fpcr | (1 << 24));
    }
    fpcr
}
#[cfg(target_arch = "aarch64")]
fn ftz_restore(saved: u64) {
    unsafe { core::arch::asm!("msr fpcr, {}", in(reg) saved) };
}
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn ftz_on() -> u64 {
    0
}
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn ftz_restore(_: u64) {}

fn synth_inner(a: &mut Audio, p: &ProjectDoc, out: &mut [f32]) {
    let frames = out.len() / 2;
    let dt = 1.0 / SR;
    let nt = p.tracks.len();
    // the running effects follow the project (new / removed / moved effects, knobs, bypass, dry/wet)
    fx::sync(&mut a.racks, p);
    if a.inst_auto.len() != nt {
        a.inst_auto.resize_with(nt, Vec::new);
    }
    route::ensure_sess_len(a, nt);
    // Tracks that run effects or take part in a route render into a buffer of their own: voices and audio clips,
    // the sends coming in, the effect chain (with its sidechain key, modulator input and duck triggers), then the
    // fader and pan (ling-daw's order); every other track mixes straight in as before. Routed tracks go in the
    // routes' order, sources first.
    let routed: Vec<bool> = (0..nt).map(|i| a.racks.active(i) || p.routes.iter().any(|r| (r.from == i || r.to == i) && r.from < nt && r.to < nt && r.from != r.to)).collect();
    let any_routed = routed.iter().any(|x| *x);
    let order: Vec<usize> = if any_routed { route::order(p) } else { Vec::new() };
    let master_fx = a.racks.master_active();
    let any_solo = p.tracks.iter().any(|t| t.solo);
    let audible: Vec<bool> = p.tracks.iter().map(|t| !t.muted && !(any_solo && !t.solo)).collect();
    let mut tbuf: Vec<Vec<f32>> = if any_routed { (0..nt).map(|i| if routed[i] { alloc::vec![0.0f32; 128] } else { Vec::new() }).collect() } else { Vec::new() };
    let mut outb: Vec<Vec<f32>> = if any_routed { (0..nt).map(|i| if routed[i] { alloc::vec![0.0f32; 128] } else { Vec::new() }).collect() } else { Vec::new() };
    let mut key_pk: Vec<f32> = alloc::vec![0.0; nt];
    let mut f = 0usize;
    while f < frames {
        // Sub-blocks of 64 frames: schedule notes, then render.
        let n = (frames - f).min(64);
        // this sub-block's beats, for the audio clips (None: transport stopped)
        let mut seg: Option<(f64, f64)> = None;
        if a.running && a.beat >= a.stop_beat {
            // an offline render's range is over: let the notes ring out
            a.running = false;
            for v in a.voices.iter_mut() {
                v.gate = false;
            }
        }
        // the mixer this sub-block: levels and pans (automation moves them), the master level
        let mut gains: Vec<(f64, f64)> = p.tracks.iter().map(|t| (t.level as f64, (t.pan as f64).clamp(-1.0, 1.0))).collect();
        let mut master = p.master_level as f64;
        if a.running {
            let bpm = p.bpm_at(a.beat);
            let b0 = a.beat;
            let mut b1 = b0 + n as f64 * dt * bpm / 60.0;
            seg = Some((b0, b1));
            // automation (ling-daw `apply_automation`): lanes move the mixer, instrument knobs and effect knobs
            route::automate(a, p, b0, &mut gains, &mut master);
            // the launcher: queued launches / stops that fall in this sub-block start, playing clips schedule their notes
            if !a.offline {
                route::session(a, p, b0, b1 - b0);
            }
            if !(p.session_only && !a.offline) {
                if p.loop_on && p.loop_end > p.loop_start && b1 >= p.loop_end && b0 < p.loop_end {
                    schedule(a, p, b0, p.loop_end);
                    for v in a.voices.iter_mut() {
                        if v.live < 0 && !v.sess {
                            v.gate = false;
                        }
                    }
                    b1 = p.loop_start + (b1 - p.loop_end);
                    schedule(a, p, p.loop_start, b1);
                } else {
                    schedule(a, p, b0, b1.min(a.stop_beat));
                }
            } else if p.loop_on && p.loop_end > p.loop_start && b1 >= p.loop_end && b0 < p.loop_end {
                b1 = p.loop_start + (b1 - p.loop_end);
            }
            a.spos += (seg.unwrap().1 - b0).max(0.0);
            a.beat = b1;
            for v in a.voices.iter_mut() {
                if v.gate && a.beat >= v.off_beat {
                    v.gate = false;
                }
            }
        }
        // audio clips (recorded takes, imported stems) for this sub-block
        let mut clip = [0.0f64; 128];
        if let Some((c0, c1)) = seg {
            if !(p.session_only && !a.offline) {
                rec::clip_mix_gains(p, c0, c1, n, &mut clip[..n * 2], &routed, &gains);
            }
        }
        // every voice's samples for the sub-block (on every core when there are enough)
        let nv = a.voices.len();
        let mut contrib: Vec<f64> = alloc::vec![0.0; nv * n];
        let mut alive: Vec<usize> = alloc::vec![n; nv];
        render_voices(a, dt, n, &mut contrib, &mut alive);
        let mut mix = [0.0f64; 128];
        for i in 0..n {
            let (mut l, mut r) = (0.0f64, 0.0f64);
            for k in 0..nv {
                if i >= alive[k] {
                    continue;
                }
                let v = &a.voices[k];
                let s = contrib[k * n + i];
                if any_routed && routed.get(v.track).copied().unwrap_or(false) {
                    // into the track's own buffer: the note's own pan only (unity at the centre); fader + track pan after the effects
                    let x = (v.pan.clamp(-1.0, 1.0) + 1.0) * core::f64::consts::FRAC_PI_4;
                    let tb = &mut tbuf[v.track];
                    tb[i * 2] += (s * libm::cos(x) * core::f64::consts::SQRT_2) as f32;
                    tb[i * 2 + 1] += (s * libm::sin(x) * core::f64::consts::SQRT_2) as f32;
                } else {
                    let (lvl, tpan) = gains.get(v.track).copied().unwrap_or((1.0, 0.0));
                    let pan = (tpan + v.pan).clamp(-1.0, 1.0);
                    let x = (pan + 1.0) * core::f64::consts::FRAC_PI_4;
                    l += s * lvl * libm::cos(x);
                    r += s * lvl * libm::sin(x);
                }
            }
            mix[i * 2] = l;
            mix[i * 2 + 1] = r;
        }
        let mut k = 0;
        a.voices.retain(|_| {
            let keep = alive[k] == n;
            k += 1;
            keep
        });
        // the routed tracks, sources first (the engine's mix runs at 0.35 of the voices' scale: the chains get the level the master sees)
        if any_routed {
            let duck = route::duck_events(p, seg, n, nt);
            for &ti in &order {
                if !routed[ti] {
                    continue;
                }
                let mut tb = core::mem::take(&mut tbuf[ti]);
                if let Some((c0, c1)) = seg {
                    let mut cl = [0.0f64; 128];
                    if !(p.session_only && !a.offline) && rec::clip_track(p, ti, c0, c1, n, &mut cl[..n * 2]) {
                        for j in 0..n * 2 {
                            tb[j] += cl[j] as f32;
                        }
                    }
                }
                // what comes in from other tracks
                let mut sc: Option<f32> = None;
                let mut aux: Option<Vec<f32>> = None;
                for r in p.routes.iter().filter(|r| r.to == ti && r.from < nt && r.from != ti) {
                    match r.kind.as_str() {
                        "duck" => {},
                        "mod" => {
                            // the modulator input: audible or not, the source feeds it
                            let src = &outb[r.from];
                            let ab = aux.get_or_insert_with(|| alloc::vec![0.0f32; n * 2]);
                            for j in 0..n * 2 {
                                ab[j] += src[j] * 0.35 * r.amount;
                            }
                        },
                        "sidechain" => {
                            // keyed pre-fader / pre-mute, so a muted trigger track still keys
                            let pk = key_pk[r.from] * r.amount;
                            sc = Some(sc.map_or(pk, |c: f32| c.max(pk)));
                        },
                        _ => {
                            // a send: the source's post-fader sound into this track's input
                            if audible[r.from] {
                                let src = &outb[r.from];
                                for j in 0..n * 2 {
                                    tb[j] += src[j] * r.amount;
                                }
                            }
                        },
                    }
                }
                for x in tb[..n * 2].iter_mut() {
                    *x *= 0.35;
                }
                fx::process(&mut a.racks, p, ti as i64, &mut tb[..n * 2], seg.map(|s| s.0), sc, aux.as_deref(), &duck[ti]);
                key_pk[ti] = tb[..n * 2].iter().fold(0.0f32, |m, x| m.max(libm::fabsf(*x)));
                let (lvl, tpan) = gains[ti];
                let x = (tpan + 1.0) * core::f64::consts::FRAC_PI_4;
                let (gl, gr) = (lvl * libm::cos(x) / 0.35, lvl * libm::sin(x) / 0.35);
                let ob = &mut outb[ti];
                for i in 0..n {
                    ob[i * 2] = (tb[i * 2] as f64 * gl) as f32;
                    ob[i * 2 + 1] = (tb[i * 2 + 1] as f64 * gr) as f32;
                }
                if audible[ti] {
                    for j in 0..n * 2 {
                        mix[j] += ob[j] as f64;
                    }
                }
                for x in tb.iter_mut() {
                    *x = 0.0;
                }
                tbuf[ti] = tb;
            }
        }
        if master_fx {
            // master effects, then the master level and the soft limiter
            let mut mb = [0.0f32; 128];
            for j in 0..n * 2 {
                mb[j] = ((mix[j] + clip[j]) * 0.35) as f32;
            }
            fx::process(&mut a.racks, p, -1, &mut mb[..n * 2], seg.map(|s| s.0), None, None, &[]);
            for j in 0..n * 2 {
                out[f * 2 + j] = libm::tanh(mb[j] as f64 * master) as f32;
            }
        } else {
            // master + soft limiter
            for j in 0..n * 2 {
                out[f * 2 + j] = libm::tanh((mix[j] + clip[j]) * master * 0.35) as f32;
            }
        }
        f += n;
    }
}

/// From this many sounding voices a sub-block's voices render on every core.
const PAR_VOICES: usize = 6;

/// `n` samples of every voice into `contrib[k * n ..]` (a voice touches only
/// itself, so they can run side by side on the cores); `alive[k]` is where a
/// voice that died mid-block stopped. The mixing is done by the caller in voice
/// order, the same additions in the same order on one core or many.
fn render_voices(a: &mut Audio, dt: f64, n: usize, contrib: &mut [f64], alive: &mut [usize]) {
    let nv = a.voices.len();
    if nv >= PAR_VOICES && crate::par::cores() > 1 {
        let vp = crate::par::Shared(a.voices.as_mut_ptr());
        let cp = crate::par::Shared(contrib.as_mut_ptr());
        let ap = crate::par::Shared(alive.as_mut_ptr());
        crate::par::for_each(nv, &|k: usize| {
            let v = unsafe { &mut *vp.get().add(k) };
            for i in 0..n {
                match voice_sample(v, dt) {
                    Some(s) => unsafe { *cp.get().add(k * n + i) = s },
                    None => {
                        unsafe { *ap.get().add(k) = i };
                        break;
                    },
                }
            }
        });
    } else {
        for (k, v) in a.voices.iter_mut().enumerate() {
            for i in 0..n {
                match voice_sample(v, dt) {
                    Some(s) => contrib[k * n + i] = s,
                    None => {
                        alive[k] = i;
                        break;
                    },
                }
            }
        }
    }
}

/// `daw_live`: the track's instrument starts `key` now (the piano roll's keys, the tracker's note entry), held until [`live_note_off`].
fn live_note_on(track: usize, t: &TrackDoc, key: u8, vel: f32) {
    let a = audio();
    let drums = t.instrument == "drums" || t.instrument == "drum";
    note_on(a, track, t, drums, &NoteDoc::new(0.0, 1.0, key, vel), f64::INFINITY);
    if let Some(v) = a.voices.last_mut() {
        v.live = key as i16;
    }
}

fn live_note_off(track: usize, key: u8) {
    for v in audio().voices.iter_mut() {
        if v.track == track && v.live == key as i16 {
            v.gate = false;
            v.live = -1;
        }
    }
}

/// The beat being heard right now, from the audio clock: the last rendered
/// block's end beat, minus what the device still has queued, plus the time
/// since that block was rendered. None when no device is pulling blocks (then
/// the transport's wall clock is used).
fn audible_beat(p: &ProjectDoc) -> Option<f64> {
    let a = audio();
    if !a.running || a.clock_us == 0 {
        return None;
    }
    let now = crate::arch::timer::now_us();
    let since = now.saturating_sub(a.clock_us) as f64 / 1e6;
    if since > 0.25 {
        return None; // the device stopped pulling: fall back to the wall clock
    }
    let behind = (a.lat_frames / SR - since).max(0.0);
    let mut beat = a.beat - behind * p.bpm_at(a.beat) / 60.0;
    if p.loop_on && p.loop_end > p.loop_start && beat < p.loop_start && a.beat >= p.loop_start {
        // the rendered beat has wrapped but the speaker is still before the loop end
        beat += p.loop_end - p.loop_start;
    }
    let target = beat.max(0.0);
    // The device takes whole buffers (~21 ms), so the raw estimate jitters by a
    // buffer. Shown beat = a phase-locked loop: it advances at the tempo with
    // the real frame time and pulls 15% of the remaining error each frame, so
    // every frame steps evenly and the playhead stays locked to the sound. A
    // seek, a loop wrap or a stall (error > 120 ms) snaps straight to it.
    let bps = p.bpm_at(target) / 60.0;
    let snap = 0.12 * bps;
    let shown = if a.shown < 0.0 || a.shown_us == 0 {
        target
    } else {
        let dt = now.saturating_sub(a.shown_us) as f64 / 1e6;
        let free = a.shown + dt.min(0.1) * bps;
        let err = target - free;
        if err > snap {
            target // behind the sound (a stall): catch up at once
        } else if err < -snap {
            // ahead of the sound: a seek or loop wrap jumps back; a stalled
            // device just holds the playhead until the sound catches up
            if a.shown - target > 1.0 { target } else { a.shown }
        } else {
            // never step backward between seeks
            (free + err * 0.15).max(a.shown)
        }
    };
    a.shown = shown;
    a.shown_us = now;
    Some(shown)
}

// ── offline render (Render Audio) ────────────────────────────────────────────
// LingStationX's Render dialog: the project (or each track alone, for stems)
// through the same synth as playback, from `start` to `end` beats plus a tail,
// into WAV (16/24-bit PCM, 32-bit float) or FLAC (16/24-bit). It runs in time
// slices from the dialog's per-frame polls (daw_render_state/progress), so the
// desktop keeps drawing and the mouse keeps moving while it renders. States as
// ling-daw: 0 idle, 1 running, 2 done, 3 failed, 4 cancelled.
mod render {
    use super::{Audio, ProjectDoc, SR};
    use alloc::string::String;
    use alloc::vec::Vec;

    /// Work per poll: the dialog polls every frame, so this is the frame-time
    /// cost (the pointer is an interrupt-driven overlay, so it stays smooth).
    const SLICE_US: u64 = 12_000;
    const CHUNK: usize = 1024;
    /// encoded bytes gathered before they go to the disk
    const FLUSH: usize = 512 * 1024;
    /// FLAC block size (frames)
    const FLAC_BS: usize = 4096;
    /// the most a part may build in memory (a small file for the system disk)
    const MAX_BUFFERED: u64 = 48 * 1024 * 1024;

    struct Part {
        path: String,
        /// None = the full mix; Some(i) = track i alone (a stem)
        track: Option<usize>,
    }

    struct Job {
        base: ProjectDoc,
        proj: ProjectDoc,
        start: f64,
        end: f64,
        frames: usize,
        bits: u32,
        flac: bool,
        parts: Vec<Part>,
        cur: usize,
        a: Audio,
        done_frames: usize,
        summary: String,
        // The render is encoded as it goes and streamed to its file: a whole song never sits in
        // memory (a float buffer for it was one allocation, and the allocator's biggest block is
        // 64 MiB -- anything over ~3 minutes failed with "not enough memory").
        /// encoded bytes not on disk yet (the whole file when `file` is None)
        out: Vec<u8>,
        /// FLAC: samples waiting for a whole block
        pend: Vec<f32>,
        fno: u64,
        /// where this part really goes, and its open stream when that is a data drive
        dest: String,
        file: Option<crate::fs::media::MediaFile>,
        written: u64,
        /// a system-disk path moved to the data drive (too big for lingfs)
        moved: bool,
    }

    struct State {
        state: u8,
        msg: String,
        t0_us: u64,
        t1_us: u64,
        progress: f64,
        cancel: bool,
        /// A poll is in progress: writing the file waits on the disk, and a
        /// background task (an SSH `daw render`) yields there -- the app's own
        /// per-frame poll must not re-enter and drop the job under it.
        busy: bool,
        job: Option<Job>,
    }

    static mut ST: State = State { state: 0, msg: String::new(), t0_us: 0, t1_us: 0, progress: 0.0, cancel: false, busy: false, job: None };

    fn st() -> &'static mut State {
        unsafe { &mut *(&raw mut ST) }
    }

    fn split_name(path: &str) -> (&str, &str) {
        let slash = path.rfind('/').map(|i| i + 1).unwrap_or(0);
        let file = &path[slash..];
        match file.rfind('.') {
            Some(i) if i > 0 => (&path[..slash], &file[..i]),
            _ => (&path[..slash], file),
        }
    }

    fn fail(r: &mut State, m: String) -> bool {
        r.state = 3;
        r.msg = m;
        r.job = None;
        // the dialog reads the failure on its next poll
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start(p: &ProjectDoc, path: &str, s: f64, e: f64, tail: f64, bits: u32, fmt: &str, stems: bool, bundle: bool) -> bool {
        let r = st();
        if r.state == 1 {
            return false; // one render at a time, as on the desktop
        }
        let path = path.replace('\\', "/");
        let start = if s.is_finite() { s.max(0.0) } else { 0.0 };
        let end = if e.is_finite() && e > start { e } else { p.end_beat().max(start + 1.0) };
        let tail = if tail.is_finite() { tail.clamp(0.0, 30.0) } else { 2.0 };
        let secs = p.beats_to_secs(start, end) + tail;
        let frames = (secs * SR) as usize;
        r.t0_us = crate::arch::timer::now_us().max(1);
        r.t1_us = 0;
        r.progress = 0.0;
        r.cancel = false;
        r.msg.clear();
        // Ogg Vorbis has no encoder in the kernel: the same audio goes out as FLAC (lossless) instead
        let (fmt, path, note) = if fmt == "ogg" {
            let (dir, stem) = split_name(&path);
            (String::from("flac"), alloc::format!("{dir}{stem}.flac"), " (as FLAC: LingOS has no Ogg encoder yet)")
        } else {
            (String::from(fmt), path.clone(), "")
        };
        let flac = fmt == "flac";
        let bits = match (flac, bits) {
            (true, b) if b >= 24 => 24,
            (true, _) => 16,
            (false, b) if b >= 32 => 32,
            (false, b) if b >= 24 => 24,
            _ => 16,
        };
        if frames == 0 || secs > 6.0 * 3600.0 {
            return fail(r, String::from("the range is empty or longer than six hours"));
        }
        if part_bytes(frames, bits) >= 0xFFF0_0000 {
            return fail(
                r,
                alloc::format!(
                    "{:.0} minutes at {}-bit is over 4 GiB, more than a WAV/FLAC file (or a FAT drive) can hold: choose 16-bit or a shorter range",
                    secs / 60.0,
                    bits
                ),
            );
        }
        let mut parts = Vec::new();
        let summary;
        if stems || bundle {
            let (dir, stem) = split_name(&path);
            let dir = if bundle { alloc::format!("{dir}{stem} bundle/audio/") } else { String::from(dir) };
            let ext = if flac { "flac" } else { "wav" };
            for (i, t) in p.tracks.iter().enumerate() {
                if t.clips.is_empty() || t.is_audio() {
                    continue;
                }
                let safe: String = t.name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == ' ' { c } else { '_' }).collect();
                parts.push(Part { path: alloc::format!("{dir}{stem}_{:02}_{}.{ext}", i + 1, safe.trim()), track: Some(i) });
            }
            if parts.is_empty() {
                return fail(r, String::from("no instrument tracks with clips to render"));
            }
            summary = alloc::format!(
                "{} stems in {}{}{}",
                parts.len(),
                dir,
                if bundle { " (audio only: the MIDI/patch bundle needs the desktop app)" } else { "" },
                note
            );
        } else {
            summary = alloc::format!("{path}{note}");
            parts.push(Part { path, track: None });
        }
        // the render works on its own copy: editing during a render can't tear it
        let mut base = p.clone();
        base.loop_on = false;
        let mut job = Job {
            proj: base.clone(),
            base,
            start,
            end,
            frames,
            bits,
            flac,
            parts,
            cur: 0,
            a: Audio::new(),
            done_frames: 0,
            summary,
            out: Vec::new(),
            pend: Vec::new(),
            fno: 0,
            dest: String::new(),
            file: None,
            written: 0,
            moved: false,
        };
        if let Err(m) = begin_part(&mut job) {
            return fail(r, m);
        }
        r.job = Some(job);
        r.state = 1;
        true
    }

    /// A part's file size (header + samples + FLAC frame overhead), an upper bound.
    fn part_bytes(frames: usize, bits: u32) -> u64 {
        44 + frames as u64 * 2 * (bits / 8) as u64 + (frames as u64 / FLAC_BS as u64 + 1) * 24
    }

    /// Where a part's file really goes. A data-drive path (media/..., or home/ on the card or the
    /// data partition) is streamed there while it renders; a system-disk (lingfs) path is built in
    /// memory and written at the end, which only suits small files -- a bigger one moves to the
    /// data drive (true = moved).
    fn destination(path: &str, size: u64) -> (String, bool) {
        let n = crate::vfs::norm(path);
        if crate::fs::media::is_media_path(&n) {
            return (n, false);
        }
        // private folders are sealed by vfs::write, so they take the in-memory path
        if !crate::keyring::is_private_path(&crate::vfs::resolve(path)) {
            if let Some(c) = crate::vfs::storage_path(path) {
                return (c, false);
            }
        }
        if size > 8 * 1024 * 1024 {
            if let Some(v) = crate::vfs::package_volume() {
                let file = &path[path.rfind('/').map(|i| i + 1).unwrap_or(0)..];
                return (alloc::format!("media/{v}/LingOS Renders/{file}"), true);
            }
        }
        (String::from(path), false)
    }

    fn begin_part(j: &mut Job) -> Result<(), String> {
        // the mix keeps the user's mute/solo; a stem is its track alone
        j.proj = j.base.clone();
        if let Some(ti) = j.parts[j.cur].track {
            for (i, t) in j.proj.tracks.iter_mut().enumerate() {
                t.muted = i != ti;
                t.solo = false;
            }
        }
        j.a = Audio::new();
        j.a.offline = true;
        j.a.beat = j.start;
        j.a.running = true;
        j.a.stop_beat = j.end;
        j.done_frames = 0;
        j.pend.clear();
        j.fno = 0;
        j.written = 0;
        let path = j.parts[j.cur].path.clone();
        let size = part_bytes(j.frames, j.bits);
        let (dest, moved) = destination(&path, size);
        j.dest = dest;
        j.moved = moved;
        j.out = Vec::new();
        if crate::fs::media::is_media_path(&j.dest) {
            if let Some(i) = j.dest.rfind('/') {
                crate::fs::media::mkdir_p(&j.dest[..i]);
            }
            match crate::fs::media::file_create(&j.dest) {
                Some(f) => j.file = Some(f),
                None => return Err(alloc::format!("could not create {}: is the drive full or read-only?", j.dest)),
            }
        } else {
            // in memory, written at the end: reserve it all now (growing a Vec doubles it, and a
            // failed allocation in the kernel is a panic, not an error)
            if size > MAX_BUFFERED || j.out.try_reserve_exact(size as usize).is_err() {
                return Err(alloc::format!(
                    "{path} would be {:.0} MB: too big for the system disk -- choose a folder on a data drive (media/...)",
                    size as f64 / (1024.0 * 1024.0)
                ));
            }
        }
        if j.flac {
            flac_header(&mut j.out, j.frames, j.bits);
        } else {
            wav_header(&mut j.out, j.frames, j.bits);
        }
        Ok(())
    }

    /// Encode rendered samples onto `out` (FLAC keeps a partial block in `pend`).
    fn encode(j: &mut Job, s: &[f32]) {
        if !j.flac {
            wav_put(&mut j.out, s, j.bits);
            return;
        }
        j.pend.extend_from_slice(s);
        let mut at = 0;
        while j.pend.len() - at >= FLAC_BS * 2 {
            flac_block(&mut j.out, &j.pend[at..at + FLAC_BS * 2], j.fno, j.bits);
            j.fno += 1;
            at += FLAC_BS * 2;
        }
        j.pend.drain(..at);
    }

    /// A streamed part that will not finish: give its clusters back.
    fn abort_file(j: &mut Job) {
        if let Some(f) = j.file.take() {
            crate::fs::media::file_abort(f);
        }
    }

    /// The summary names where a moved part really went.
    fn note_moved(j: &mut Job, path: &str, a: &str) {
        j.summary = j.summary.replace(path, a);
        if !j.summary.contains(a) {
            j.summary.push_str(&alloc::format!(" (saved to {a}: too big for the system disk)"));
        }
    }

    /// Advance the running render by one time slice; returns the state.
    pub fn poll() -> u8 {
        let r = st();
        if r.state != 1 || r.busy {
            return r.state;
        }
        r.busy = true;
        let s = poll_inner();
        st().busy = false;
        s
    }

    fn poll_inner() -> u8 {
        let r = st();
        let now = crate::arch::timer::now_us();
        if r.t1_us != 0 && now.saturating_sub(r.t1_us) < 2_000 {
            return 1; // already worked this frame (the dialog polls more than once)
        }
        if r.cancel {
            if let Some(j) = r.job.as_mut() {
                abort_file(j);
            }
            r.job = None;
            r.state = 4;
            r.msg = String::from("Render cancelled.");
            return 4;
        }
        let Some(j) = r.job.as_mut() else {
            r.state = 3;
            return 3;
        };
        let mut chunk = [0f32; CHUNK * 2];
        while j.done_frames < j.frames && crate::arch::timer::now_us().saturating_sub(now) < SLICE_US {
            let n = (j.frames - j.done_frames).min(CHUNK);
            super::synth(&mut j.a, &j.proj, &mut chunk[..n * 2]);
            encode(j, &chunk[..n * 2]);
            j.done_frames += n;
        }
        let finished = j.done_frames >= j.frames;
        if finished && !j.pend.is_empty() {
            let rest = core::mem::take(&mut j.pend);
            flac_block(&mut j.out, &rest, j.fno, j.bits);
            j.fno += 1;
        }
        if let Some(f) = j.file.as_mut() {
            if !j.out.is_empty() && (finished || j.out.len() >= FLUSH) {
                if !crate::fs::media::file_write(f, &j.out) {
                    let m = alloc::format!(
                        "could not write {} after {:.1} MB: is the drive full?",
                        j.dest,
                        j.written as f64 / (1024.0 * 1024.0)
                    );
                    abort_file(j);
                    fail(r, m);
                    return 3;
                }
                j.written += j.out.len() as u64;
                j.out.clear();
            }
        }
        let parts = j.parts.len() as f64;
        r.progress = ((j.cur as f64 + j.done_frames as f64 / j.frames.max(1) as f64) / parts * 0.98).min(0.98);
        r.t1_us = crate::arch::timer::now_us();
        if !finished {
            return 1;
        }
        // this part is rendered: close its file (or write the small in-memory one)
        let path = j.parts[j.cur].path.clone();
        if let Some(f) = j.file.take() {
            if !crate::fs::media::file_finish(f) {
                let m = alloc::format!("could not finish {} (the drive is full or a write failed)", j.dest);
                fail(r, m);
                return 3;
            }
            crate::vfs::note_change(&j.dest);
            if j.moved {
                let a = j.dest.clone();
                note_moved(j, &path, &a);
            }
            j.cur += 1;
            if j.cur < j.parts.len() {
                if let Err(m) = begin_part(j) {
                    fail(r, m);
                    return 3;
                }
                return 1;
            }
            r.state = 2;
            r.progress = 1.0;
            r.msg = alloc::format!("Rendered {}", j.summary);
            r.job = None;
            return 2;
        }
        let bytes = core::mem::take(&mut j.out);
        if let Some(i) = path.rfind('/') {
            crate::vfs::mkdir(&path[..i]);
        }
        // the system disk (lingfs) holds only ~16 MiB, and a write too big for it takes minutes
        // to fail: a big render aimed at it goes straight to the data drive
        let on_fat = crate::fs::media::is_media_path(&crate::vfs::norm(&path)) || crate::vfs::storage_path(&path).is_some();
        let too_big = !on_fat && bytes.len() > 8 * 1024 * 1024 && crate::vfs::package_volume().is_some();
        if too_big || !crate::vfs::write(&path, &bytes) {
            // the system disk (lingfs) holds only ~16 MiB: a song goes to the data drive instead
            let mb = bytes.len() as f64 / (1024.0 * 1024.0);
            let file = &path[path.rfind('/').map(|i| i + 1).unwrap_or(0)..];
            let alt = crate::vfs::package_volume().map(|v| alloc::format!("media/{v}/LingOS Renders/{file}"));
            let wrote = match &alt {
                Some(a) => {
                    crate::vfs::mkdir(&a[..a.rfind('/').unwrap_or(0)]);
                    crate::vfs::write(a, &bytes)
                },
                None => false,
            };
            if !wrote {
                let m = match alt {
                    Some(a) => alloc::format!("could not write {path} or {a} ({mb:.1} MB): is the drive full or read-only?"),
                    None => alloc::format!("could not write {path} ({mb:.1} MB): the system disk only holds small files -- choose a folder on a data drive (media/...)"),
                };
                fail(r, m);
                return 3;
            }
            let a = alt.unwrap_or_default();
            note_moved(j, &path, &a);
        }
        j.cur += 1;
        if j.cur < j.parts.len() {
            if let Err(m) = begin_part(j) {
                fail(r, m);
                return 3;
            }
            return 1;
        }
        r.state = 2;
        r.progress = 1.0;
        r.msg = alloc::format!("Rendered {}", j.summary);
        r.job = None;
        2
    }

    pub fn progress() -> f64 {
        st().progress
    }
    pub fn elapsed() -> f64 {
        let r = st();
        if r.t0_us == 0 {
            return 0.0;
        }
        crate::arch::timer::now_us().saturating_sub(r.t0_us) as f64 / 1e6
    }
    pub fn message() -> String {
        st().msg.clone()
    }
    pub fn ack() {
        let r = st();
        if r.state != 1 {
            r.state = 0;
        }
    }
    pub fn cancel() {
        st().cancel = true;
    }

    /// Run a whole render now, without the dialog (the `daw render` command).
    pub fn run_to_end(budget_ms: u64) -> u8 {
        let t0 = crate::arch::timer::now_ms();
        let mut last_yield = t0;
        loop {
            st().t1_us = 0;
            let s = poll();
            // an SSH-started render must not freeze the desktop: give it a
            // frame every ~40 ms of rendering
            if crate::arch::timer::now_ms().saturating_sub(last_yield) >= 40 {
                crate::proc::sched::yield_now();
                last_yield = crate::arch::timer::now_ms();
            }
            if s != 1 || crate::arch::timer::now_ms().saturating_sub(t0) > budget_ms {
                return s;
            }
        }
    }

    fn sample_i(x: f32, bits: u32) -> i32 {
        let x = x.clamp(-1.0, 1.0) as f64;
        let m = ((1i64 << (bits - 1)) - 1) as f64;
        libm::round(x * m) as i32
    }

    fn wav_header(o: &mut Vec<u8>, frames: usize, bits: u32) {
        let float = bits == 32;
        let bps = bits / 8;
        let len = (frames as u64 * 2 * bps as u64).min(u32::MAX as u64 - 36) as u32;
        o.extend_from_slice(b"RIFF");
        o.extend_from_slice(&(36 + len).to_le_bytes());
        o.extend_from_slice(b"WAVEfmt ");
        o.extend_from_slice(&16u32.to_le_bytes());
        o.extend_from_slice(&(if float { 3u16 } else { 1u16 }).to_le_bytes());
        o.extend_from_slice(&2u16.to_le_bytes());
        o.extend_from_slice(&(SR as u32).to_le_bytes());
        o.extend_from_slice(&(SR as u32 * 2 * bps).to_le_bytes());
        o.extend_from_slice(&((2 * bps) as u16).to_le_bytes());
        o.extend_from_slice(&(bits as u16).to_le_bytes());
        o.extend_from_slice(b"data");
        o.extend_from_slice(&len.to_le_bytes());
    }

    fn wav_put(o: &mut Vec<u8>, data: &[f32], bits: u32) {
        let bps = (bits / 8) as usize;
        for &x in data {
            if bits == 32 {
                o.extend_from_slice(&x.to_le_bytes());
            } else {
                let v = sample_i(x, bits);
                o.extend_from_slice(&v.to_le_bytes()[..bps]);
            }
        }
    }

    fn crc8(d: &[u8]) -> u8 {
        let mut c = 0u8;
        for &b in d {
            c ^= b;
            for _ in 0..8 {
                c = if c & 0x80 != 0 { (c << 1) ^ 0x07 } else { c << 1 };
            }
        }
        c
    }
    fn crc16(d: &[u8]) -> u16 {
        let mut c = 0u16;
        for &b in d {
            c ^= (b as u16) << 8;
            for _ in 0..8 {
                c = if c & 0x8000 != 0 { (c << 1) ^ 0x8005 } else { c << 1 };
            }
        }
        c
    }
    /// FLAC's UTF-8-style frame number.
    fn utf8_num(o: &mut Vec<u8>, v: u64) {
        if v < 0x80 {
            o.push(v as u8);
            return;
        }
        let mut n = 2u32;
        while v >= (1u64 << (5 * n + 1)) {
            n += 1;
        }
        let lead: u8 = !(0xFFu8 >> n);
        o.push(lead | (v >> (6 * (n - 1))) as u8);
        for k in (0..n - 1).rev() {
            o.push(0x80 | ((v >> (6 * k)) & 0x3F) as u8);
        }
    }

    /// FLAC with verbatim subframes: lossless and readable by every decoder,
    /// but not compressed (the kernel has no LPC encoder yet). The header knows
    /// the total length up front, so the frames can follow as they render.
    fn flac_header(o: &mut Vec<u8>, frames: usize, bits: u32) {
        o.extend_from_slice(b"fLaC");
        o.extend_from_slice(&[0x80, 0, 0, 34]); // the last metadata block: STREAMINFO, 34 bytes
        o.extend_from_slice(&(FLAC_BS as u16).to_be_bytes());
        o.extend_from_slice(&(FLAC_BS as u16).to_be_bytes());
        o.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // min/max frame size: unknown
        // 20 bits rate, 3 bits channels-1, 5 bits bps-1, 36 bits total samples
        let sr = SR as u64;
        let packed: u64 = (sr << 44) | (1u64 << 41) | (((bits - 1) as u64) << 36) | (frames as u64 & 0xF_FFFF_FFFF);
        o.extend_from_slice(&packed.to_be_bytes());
        o.extend_from_slice(&[0u8; 16]); // MD5 not computed (all zero is allowed)
    }

    /// One FLAC frame of `data.len() / 2` stereo frames (at most FLAC_BS).
    fn flac_block(o: &mut Vec<u8>, data: &[f32], fno: u64, bits: u32) {
        let n = data.len() / 2;
        if n == 0 {
            return;
        }
        let bps = (bits / 8) as usize;
        let ss_code: u8 = if bits == 24 { 0b110 } else { 0b100 };
        let f0 = o.len();
        o.extend_from_slice(&[0xFF, 0xF8]);
        o.push((0b0111 << 4) | 0b1010); // 16-bit blocksize-1 at the header end; 48 kHz
        o.push((0b0001 << 4) | (ss_code << 1)); // L/R independent
        utf8_num(o, fno);
        o.extend_from_slice(&((n - 1) as u16).to_be_bytes());
        let c8 = crc8(&o[f0..]);
        o.push(c8);
        for ch in 0..2 {
            o.push(0b0000_0010); // verbatim subframe, no wasted bits
            for k in 0..n {
                let b = sample_i(data[k * 2 + ch], bits).to_be_bytes();
                o.extend_from_slice(&b[4 - bps..]);
            }
        }
        let c16 = crc16(&o[f0..]);
        o.extend_from_slice(&c16.to_be_bytes());
    }
}

/// `daw render <out.wav|out.flac> [16|24|32]` (Terminal / SSH): render the open
/// project start to end without the dialog -- the feature test's hook.
pub fn render_command(arg: &str) -> String {
    let mut w = arg.split_whitespace();
    let Some(path) = w.next() else { return String::from("usage: daw render <path.wav|path.flac> [16|24|32]\n") };
    let bits: u32 = w.next().and_then(|b| b.parse().ok()).unwrap_or(16);
    let fmt = if path.ends_with(".flac") { "flac" } else { "wav" };
    let d = daw();
    let t0 = crate::arch::timer::now_ms();
    if !render::start(&d.project, path, 0.0, 0.0, 1.0, bits, fmt, false, false) {
        return String::from("a render is already running\n");
    }
    let st = render::run_to_end(600_000);
    let ms = crate::arch::timer::now_ms().saturating_sub(t0);
    let secs = d.project.beats_to_secs(0.0, d.project.end_beat().max(1.0)) + 1.0;
    let msg = render::message();
    render::ack();
    alloc::format!("render: state {st} in {ms} ms ({:.1}x real time): {msg}\n", secs * 1000.0 / ms.max(1) as f64)
}

// ── `daw` console / SSH command: the LingStationX feature tests' hooks ───────

/// Playhead statistics, gathered from the UI's own `daw_position` reads (one
/// per drawn frame): how far each frame's step is from the ideal step for the
/// frame's real duration, and how far the shown beat is from the audio clock.
struct ClockStats {
    last_us: u64,
    last_pos: f64,
    frames: u64,
    err_sum: f64,
    err_max: f64,
    back_steps: u64,
    audio_driven: u64,
}

static mut CLOCK: ClockStats = ClockStats { last_us: 0, last_pos: 0.0, frames: 0, err_sum: 0.0, err_max: 0.0, back_steps: 0, audio_driven: 0 };

fn clock_note(d: &Daw) {
    let c = unsafe { &mut *(&raw mut CLOCK) };
    let now = crate::arch::timer::now_us();
    if !d.playing {
        c.last_us = 0;
        return;
    }
    if c.last_us != 0 {
        let dt = now.saturating_sub(c.last_us) as f64 / 1e6;
        // the same beat read twice in one frame is not a step
        if dt > 0.002 && dt < 0.25 {
            let ideal = dt * d.project.bpm_at(d.position) / 60.0;
            let step = d.position - c.last_pos;
            if step < -0.001 && !(d.project.loop_on && d.position < c.last_pos - 0.5) {
                c.back_steps += 1;
            }
            if step >= 0.0 {
                // error of this frame's step, in milliseconds of music
                let err_ms = libm::fabs(step - ideal) * 60.0 / d.project.bpm_at(d.position) * 1000.0;
                c.err_sum += err_ms;
                c.err_max = c.err_max.max(err_ms);
                c.frames += 1;
                if audio().clock_us != 0 {
                    c.audio_driven += 1;
                }
            }
        }
    }
    c.last_us = now;
    c.last_pos = d.position;
}

pub fn command(arg: &str) -> String {
    let arg = arg.trim();
    let (sub, rest) = arg.split_once(' ').unwrap_or((arg, ""));
    match sub {
        "render" => render_command(rest),
        "selftest" => selftest(),
        "call" => call_command(rest),
        "state" => state_command(),
        "bench" => bench(rest),
        "rec" => rec::command(rest),
        "clock" => {
            let c = unsafe { &mut *(&raw mut CLOCK) };
            let d = daw();
            tick(d);
            let a = audio();
            let out = alloc::format!(
                "clock: playing {} position {:.3} rendered {:.3} latency {:.1} ms source {}\n\
                 playhead: {} frames, step error avg {:.2} ms max {:.2} ms, backward steps {}, audio-clocked frames {}\n",
                d.playing,
                d.position,
                a.beat,
                a.lat_frames / SR * 1000.0,
                if a.clock_us != 0 { "audio" } else { "wall" },
                c.frames,
                if c.frames > 0 { c.err_sum / c.frames as f64 } else { 0.0 },
                c.err_max,
                c.back_steps,
                c.audio_driven
            );
            if rest.trim() == "reset" {
                *c = ClockStats { last_us: 0, last_pos: 0.0, frames: 0, err_sum: 0.0, err_max: 0.0, back_steps: 0, audio_driven: 0 };
            }
            out
        },
        "open" => {
            let d = daw();
            match load_into(d, rest.trim()) {
                Ok(()) => alloc::format!("opened {} ({} tracks, {:.1} beats)\n", d.path, d.project.tracks.len(), d.project.end_beat()),
                Err(e) => alloc::format!("open failed: {e}\n"),
            }
        },
        "play" | "stop" => {
            let d = daw();
            if sub == "play" {
                d.playing = true;
                d.base_beat = d.position;
                d.base_ms = crate::arch::timer::now_ms();
                audio_start(d.position);
            } else {
                audio_stop();
                d.playing = false;
                d.position = 0.0;
            }
            alloc::format!("{sub}: position {:.3}\n", d.position)
        },
        _ => String::from(
            "usage: daw render <out.wav|out.flac> [16|24|32]   render the open project\n\
             \x20      daw selftest                              every instrument kind: sound, pitch, decay\n\
             \x20      daw clock [reset]                         playhead vs audio clock statistics\n\
             \x20      daw open <project.json> | play | stop\n\
             \x20      daw call <daw_builtin> [args]              call a daw_* builtin as the app does (numbers, words (~ = space), [1,2])\n\
             \x20      daw state                                 tracks, clips, notes and effect chains of the open project\n\
             \x20      daw rec inputs | start [input] | status | stop [Hz]   record from the audio input\n",
        ),
    }
}

/// `daw call <name> <args>`: one `daw_*` builtin through the same dispatch the
/// app uses (the feature tests drive effects, clips, notes and undo with it).
/// Arguments: numbers, "quoted text" or bare words, [n,n,...] number lists.
fn call_command(arg: &str) -> String {
    use crate::lingrt::{is_list, is_num, is_str, list_items};
    let arg = arg.trim();
    let (name, mut rest) = arg.split_once(' ').unwrap_or((arg, ""));
    if !name.starts_with("daw_") {
        return String::from("usage: daw call daw_<name> [args]\n");
    }
    let mut args: Vec<u64> = Vec::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        if let Some(r) = rest.strip_prefix('"') {
            let end = r.find('"').unwrap_or(r.len());
            args.push(mk_str(&r[..end]));
            rest = r.get(end + 1..).unwrap_or("");
        } else if let Some(r) = rest.strip_prefix('[') {
            let end = r.find(']').unwrap_or(r.len());
            let items: Vec<u64> = r[..end].split(',').filter_map(|x| x.trim().parse::<f64>().ok()).map(num).collect();
            args.push(list_from(items));
            rest = r.get(end + 1..).unwrap_or("");
        } else {
            let end = rest.find(' ').unwrap_or(rest.len());
            let tok = &rest[..end];
            args.push(match tok.parse::<f64>() {
                Ok(v) => num(v),
                Err(_) => mk_str(&tok.replace('~', " ")),
            });
            rest = &rest[end..];
        }
    }
    fn show(v: u64, out: &mut String, depth: usize) {
        if is_list(v) && depth < 4 {
            out.push('[');
            for (i, &x) in list_items(v).iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                show(x, out, depth + 1);
            }
            out.push(']');
        } else if is_str(v) {
            out.push('"');
            out.push_str(&to_string(v));
            out.push('"');
        } else if is_num(v) {
            let x = to_f64(v);
            out.push_str(&alloc::format!("{}", x));
        } else {
            out.push_str(&to_string(v));
        }
    }
    match dispatch(name.as_bytes(), &args) {
        Some(v) => {
            let mut out = String::new();
            show(v, &mut out, 0);
            out.push('\n');
            out
        },
        None => alloc::format!("{name}: not a builtin of the LingOS engine\n"),
    }
}

/// `daw state`: the open project as the tests check it -- one line per track
/// (kind, mixer, clips with note counts, its effect chain) and the master chain.
fn state_command() -> String {
    let d = daw();
    tick(d);
    let p = &d.project;
    let fxs = |v: &Vec<FxDoc>| -> String {
        let mut o = String::new();
        for (i, f) in v.iter().enumerate() {
            if i > 0 {
                o.push_str(", ");
            }
            o.push_str(&alloc::format!("{}{} wet {:.2} [", f.name, if f.bypass { " (bypassed)" } else { "" }, f.wet));
            for (j, (k, x)) in f.params.iter().enumerate() {
                if j > 0 {
                    o.push(' ');
                }
                o.push_str(&alloc::format!("{k}={x:.3}"));
            }
            o.push(']');
        }
        o
    };
    let mut out = alloc::format!(
        "project \"{}\" path {} bpm {:.2} tracks {} end {:.3} dirty {} rev {} undo {} redo {}\n",
        p.name,
        if d.path.is_empty() { "-" } else { &d.path },
        p.tempo_bpm,
        p.tracks.len(),
        p.end_beat(),
        d.dirty,
        d.rev,
        edit::depth().0,
        edit::depth().1
    );
    for (i, t) in p.tracks.iter().enumerate() {
        out.push_str(&alloc::format!(
            "track {i} \"{}\" {} vol {:.3} pan {:.3} mute {} solo {} clips {}:",
            t.name,
            if t.is_audio() { "audio" } else { &t.instrument },
            t.level,
            t.pan,
            t.muted as u8,
            t.solo as u8,
            t.clips.len()
        ));
        for c in &t.clips {
            out.push_str(&alloc::format!(" [{:.3}+{:.3} {}]", c.start, c.len, if c.is_audio() { String::from("audio") } else { alloc::format!("{} notes", c.midi_notes.len()) }));
        }
        out.push_str(&alloc::format!(" fx: {}\n", fxs(&t.effects)));
    }
    out.push_str(&alloc::format!("master vol {:.3} fx: {}\n", p.master_level, fxs(&p.master_effects)));
    out
}

/// Energy of `freq` in a mono signal (Goertzel).
fn goertzel(x: &[f64], freq: f64) -> f64 {
    let w = 2.0 * core::f64::consts::PI * freq / SR;
    let c = 2.0 * libm::cos(w);
    let (mut s1, mut s2) = (0.0, 0.0);
    for &v in x {
        let s0 = v + c * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - c * s1 * s2
}

/// Every LingStationX instrument kind plays one note through the kernel synth:
/// it must sound (RMS), stay finite and unclipped, sit on the right pitch
/// (melodic kinds: 440 Hz louder than a semitone either side) and die away
/// after release. Drums: kick, snare and hat each sound and decay.
/// `daw bench [tracks]`: a dense project (every track a 4-note chord held for
/// 4 beats, then released) rendered for 4 s on one core and on all of them;
/// prints both times and whether the samples match bit for bit.
fn bench(arg: &str) -> String {
    let tracks: usize = arg.trim().parse().unwrap_or(8).clamp(1, 32);
    const KINDS: [&str; 6] = ["synth", "prism", "nova", "growl", "voice", "cinema"];
    let mut t = String::new();
    for i in 0..tracks {
        if i > 0 {
            t.push(',');
        }
        let base = 48 + (i * 5) % 24;
        let notes: Vec<String> = [0usize, 4, 7, 11].iter().map(|o| alloc::format!(r#"{{"start":0,"len":4,"key":{},"vel":0.8}}"#, base + o)).collect();
        t.push_str(&alloc::format!(
            r#"{{"name":"t{i}","kind":"instrument","instrument":"{}","clips":[{{"start":0,"len":8,"midi_notes":[{}]}}]}}"#,
            KINDS[i % KINDS.len()],
            notes.join(",")
        ));
    }
    let json = alloc::format!(r#"{{"name":"bench","tempo_bpm":120,"tracks":[{t}]}}"#);
    let p: ProjectDoc = match serde_json::from_str::<ProjectDoc>(&json) {
        Ok(p) => p.normalised(),
        Err(e) => return alloc::format!("daw bench: project: {e}\n"),
    };
    let frames = (4.0 * SR) as usize;
    let run = || -> (u64, Vec<f32>) {
        let mut a = Audio::new();
        a.running = true;
        a.stop_beat = 8.0;
        let mut buf = alloc::vec![0f32; frames * 2];
        let t0 = crate::arch::timer::now_us();
        // in 512-frame blocks, the way playback pulls them
        for c in buf.chunks_mut(1024) {
            synth(&mut a, &p, c);
        }
        (crate::arch::timer::now_us().saturating_sub(t0), buf)
    };
    #[cfg(target_arch = "x86_64")]
    let all = crate::arch::smp::cores();
    #[cfg(target_arch = "x86_64")]
    crate::arch::smp::set_cores(1);
    let (us1, b1) = run();
    #[cfg(target_arch = "x86_64")]
    crate::arch::smp::set_cores(all);
    let (usn, bn) = run();
    let same = b1.iter().zip(bn.iter()).all(|(x, y)| x.to_bits() == y.to_bits());
    alloc::format!(
        "daw bench: {tracks} tracks x 4 voices, 4 s of audio: 1 core {:.1} ms, {} cores {:.1} ms (x{:.2}); output {}\n",
        us1 as f64 / 1000.0,
        crate::par::cores(),
        usn as f64 / 1000.0,
        us1 as f64 / usn.max(1) as f64,
        if same { "bit-identical" } else { "DIFFERS" }
    )
}

fn selftest() -> String {
    const KINDS: [&str; 12] = ["synth", "prism", "nova", "chip", "pluck", "growl", "voice", "cinema", "sampler", "sf2", "slicer", "script"];
    let mut out = String::new();
    let (mut pass, mut fail) = (0, 0);
    let run = |label: &str, kind: &str, key: u8, melodic: bool, out: &mut String| {
        let json = alloc::format!(
            r#"{{"name":"selftest","tempo_bpm":120,"tracks":[{{"name":"t","kind":"instrument","instrument":"{kind}","clips":[{{"start":0,"len":4,"midi_notes":[{{"start":0,"len":1,"key":{key},"vel":0.9}}]}}]}}]}}"#
        );
        let p: ProjectDoc = match serde_json::from_str::<ProjectDoc>(&json) {
            Ok(p) => p.normalised(),
            Err(e) => {
                out.push_str(&alloc::format!("  FAIL {label}: project: {e}\n"));
                return false;
            },
        };
        let mut a = Audio::new();
        a.running = true;
        a.stop_beat = 4.0;
        // 2 s at 120 bpm = 4 beats: 0.5 s held, then release
        let frames = (2.0 * SR) as usize;
        let mut buf = alloc::vec![0f32; frames * 2];
        let t0 = crate::arch::timer::now_us();
        synth(&mut a, &p, &mut buf);
        let us = crate::arch::timer::now_us().saturating_sub(t0);
        let mono: Vec<f64> = buf.chunks(2).map(|c| (c[0] as f64 + c[1] as f64) * 0.5).collect();
        let finite = mono.iter().all(|v| v.is_finite());
        let peak = mono.iter().fold(0.0f64, |m, v| m.max(libm::fabs(*v)));
        let rms = |s: &[f64]| libm::sqrt(s.iter().map(|v| v * v).sum::<f64>() / s.len().max(1) as f64);
        // a drum hit is over in tens of ms: judge it on its first 100 ms
        let held = if melodic { rms(&mono[(0.05 * SR) as usize..(0.45 * SR) as usize]) } else { rms(&mono[..(0.1 * SR) as usize]) };
        let tail = rms(&mono[(1.8 * SR) as usize..]);
        let mut why = String::new();
        if !finite {
            why.push_str(" non-finite samples;");
        }
        if held < 0.01 {
            why.push_str(&alloc::format!(" silent (rms {held:.4});"));
        }
        if peak > 0.999 {
            why.push_str(" clipped;");
        }
        if tail > held * 0.25 {
            why.push_str(&alloc::format!(" does not decay (tail {tail:.3} vs {held:.3});"));
        }
        let mut pitch = String::new();
        if melodic && held >= 0.01 {
            let seg = &mono[(0.05 * SR) as usize..(0.45 * SR) as usize];
            let (e, lo, hi) = (goertzel(seg, 440.0), goertzel(seg, 415.3), goertzel(seg, 466.2));
            pitch = alloc::format!(" 440Hz/neighbours {:.1}x", e / lo.max(hi).max(1e-12));
            if !(e > lo && e > hi) {
                why.push_str(" off pitch;");
            }
        }
        let ok = why.is_empty();
        out.push_str(&alloc::format!(
            "  {} {label:<14} rms {held:.3} peak {peak:.3}{pitch}  ({:.1} ms for 2 s){}\n",
            if ok { "PASS" } else { "FAIL" },
            us as f64 / 1000.0,
            if ok { String::new() } else { alloc::format!(" --{why}") }
        ));
        ok
    };
    for k in KINDS {
        if run(k, k, 69, true, &mut out) { pass += 1 } else { fail += 1 }
    }
    for (label, key) in [("drums kick", 36u8), ("drums snare", 38), ("drums hat", 42)] {
        if run(label, "drums", key, false, &mut out) { pass += 1 } else { fail += 1 }
    }
    alloc::format!("daw selftest: {pass} passed, {fail} failed\n{out}")
}
