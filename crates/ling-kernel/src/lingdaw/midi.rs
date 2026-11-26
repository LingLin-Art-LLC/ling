//! Standard MIDI files for LingStationX on LingOS (ling-daw midi_import.rs / midi_io.rs, the parts a user meets):
//!
//! * `daw_midi_scan(path)`: the parts of a .mid file, as the import dialog lists them,
//! * `daw_midi_import(path, beat)`: every part (MIDI track x channel) becomes a track with one clip; a General MIDI program
//!   gets the app's GM patch (`instruments/GM/NNN Name.ling`), channel 10 the drum kit; an empty project takes the file's tempo,
//! * `daw_midi_export_project(path)`: a type-1 file at 480 PPQ, a tempo track plus one track per instrument track (drums on
//!   channel 10), the same layout as the Windows export.

use alloc::string::String;
use alloc::vec::Vec;

use super::{apply_patch, ClipDoc, Daw, NoteDoc, TrackDoc};
use crate::lingrt::{list_from, mk_str, num, to_f64, to_string};

struct Part {
    name: String,
    channel: u8,
    program: Option<u8>,
    notes: Vec<(f64, f64, u8, f32)>,
}

struct Song {
    parts: Vec<Part>,
    bpm: Option<f64>,
}

fn vlq(b: &[u8], i: &mut usize) -> Option<u32> {
    let mut v = 0u32;
    for _ in 0..4 {
        let x = *b.get(*i)?;
        *i += 1;
        v = (v << 7) | (x & 0x7f) as u32;
        if x & 0x80 == 0 {
            return Some(v);
        }
    }
    Some(v)
}

fn parse(b: &[u8]) -> Result<Song, String> {
    if b.len() < 14 || &b[0..4] != b"MThd" {
        return Err(String::from("not a MIDI file"));
    }
    let be16 = |o: usize| u16::from_be_bytes([b[o], b[o + 1]]);
    let be32 = |o: usize| u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let hlen = be32(4) as usize;
    let ntracks = be16(10) as usize;
    let div = be16(12);
    if div & 0x8000 != 0 {
        return Err(String::from("SMPTE timing is not supported"));
    }
    let ppq = div.max(1) as f64;
    let mut off = 8 + hlen;
    let mut parts: Vec<Part> = Vec::new();
    let mut bpm = None;
    for ti in 0..ntracks {
        if off + 8 > b.len() || &b[off..off + 4] != b"MTrk" {
            break;
        }
        let len = be32(off + 4) as usize;
        let (start, end) = (off + 8, (off + 8 + len).min(b.len()));
        off = end;
        let mut i = start;
        let mut tick: u64 = 0;
        let mut status = 0u8;
        let mut tname = String::new();
        // per channel: program, open notes (key -> start tick, vel), finished notes
        let mut prog: [Option<u8>; 16] = [None; 16];
        let mut open: Vec<(u8, u8, u64, u8)> = Vec::new();
        let mut done: [Vec<(f64, f64, u8, f32)>; 16] = Default::default();
        while i < end {
            let Some(dt) = vlq(b, &mut i) else { break };
            tick += dt as u64;
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
                // a data byte with no status to run on (a damaged file): skip it and look for the next event
                i += 1;
                continue;
            }
            match st {
                0xff => {
                    let Some(&ty) = b.get(i) else { break };
                    i += 1;
                    let Some(l) = vlq(b, &mut i) else { break };
                    let l = l as usize;
                    let data = &b[i.min(end)..(i + l).min(end)];
                    if ty == 0x51 && data.len() == 3 && bpm.is_none() {
                        let us = ((data[0] as u32) << 16) | ((data[1] as u32) << 8) | data[2] as u32;
                        if us > 0 {
                            bpm = Some(60_000_000.0 / us as f64);
                        }
                    } else if ty == 0x03 && tname.is_empty() {
                        tname = String::from_utf8_lossy(data).into_owned();
                    }
                    i += l;
                },
                0xf0 | 0xf7 => {
                    let Some(l) = vlq(b, &mut i) else { break };
                    i += l as usize;
                },
                // other system messages (rare in files): song position 2 data bytes, MTC / song select 1, the rest none
                0xf2 => i += 2,
                0xf1 | 0xf3 => i += 1,
                0xf4..=0xfe => {},
                _ => {
                    let ch = (st & 0x0f) as usize;
                    let kind = st & 0xf0;
                    let nb = if kind == 0xc0 || kind == 0xd0 { 1 } else { 2 };
                    if i + nb > end {
                        break;
                    }
                    let (d1, d2) = (b[i], if nb > 1 { b[i + 1] } else { 0 });
                    i += nb;
                    match kind {
                        0x90 if d2 > 0 => open.push((ch as u8, d1, tick, d2)),
                        0x80 | 0x90 => {
                            if let Some(k) = open.iter().position(|o| o.0 == ch as u8 && o.1 == d1) {
                                let o = open.remove(k);
                                done[ch].push((o.2 as f64 / ppq, ((tick - o.2) as f64 / ppq).max(0.03), d1, o.3 as f32 / 127.0));
                            }
                        },
                        0xc0 => {
                            if prog[ch].is_none() {
                                prog[ch] = Some(d1);
                            }
                        },
                        _ => {},
                    }
                },
            }
        }
        // notes never released end with the track
        for o in open {
            done[o.0 as usize].push((o.2 as f64 / ppq, ((tick.saturating_sub(o.2)) as f64 / ppq).max(0.25), o.1, o.3 as f32 / 127.0));
        }
        // a type-0 file (or a track playing several channels) splits into one part per channel: tell them apart
        let channels = done.iter().filter(|n| !n.is_empty()).count();
        for (ch, notes) in done.iter_mut().enumerate() {
            if notes.is_empty() {
                continue;
            }
            notes.sort_by(|a, b| a.0.total_cmp(&b.0));
            let name = if tname.is_empty() {
                alloc::format!("Track {} ch {}", ti + 1, ch + 1)
            } else if channels > 1 {
                alloc::format!("{} ch {}", tname, ch + 1)
            } else {
                tname.clone()
            };
            parts.push(Part { name, channel: ch as u8, program: if ch == 9 { None } else { Some(prog[ch].unwrap_or(0)) }, notes: core::mem::take(notes) });
        }
    }
    Ok(Song { parts, bpm })
}

/// The app's General MIDI patches: (program, display name, path), from instruments/GM ("001 Acoustic Grand Piano.ling").
pub(super) fn gm_patches() -> Vec<(u8, String, String)> {
    let mut out = Vec::new();
    // the app's bundled files (also when no app is in front: an SSH session), else the folder on disk
    let mut files: Vec<String> = Vec::new();
    // relative to the running app's folder, else LingStationX's own bundle (the kernel image
    // keeps it under apps/lingstation/ -- what an SSH session, with no app in front, sees)
    let mut dir = "instruments/GM";
    for d in ["instruments/GM", "apps/lingstation/instruments/GM"] {
        crate::appfs::list(d, |n, is_dir| {
            if !is_dir {
                files.push(String::from(n));
            }
        });
        if !files.is_empty() {
            dir = d;
            break;
        }
    }
    if files.is_empty() {
        files = crate::vfs::list("instruments/GM").into_iter().filter(|x| !x.1).map(|x| x.0).collect();
    }
    for name in files {
        let Some(stem) = name.strip_suffix(".ling") else { continue };
        if stem.len() > 4 && stem.as_bytes()[..3].iter().all(|c| c.is_ascii_digit()) {
            if let Ok(nr) = stem[..3].parse::<u16>() {
                if (1..=128).contains(&nr) {
                    out.push(((nr - 1) as u8, String::from(&stem[4..]).replace('_', " "), alloc::format!("{dir}/{name}")));
                }
            }
        }
    }
    out
}

fn put_vlq(o: &mut Vec<u8>, mut v: u32) {
    let mut buf = [0u8; 5];
    let mut n = 0;
    loop {
        buf[n] = (v & 0x7f) as u8;
        n += 1;
        v >>= 7;
        if v == 0 {
            break;
        }
    }
    for k in (0..n).rev() {
        o.push(buf[k] | if k > 0 { 0x80 } else { 0 });
    }
}

fn chunk(o: &mut Vec<u8>, body: &[u8]) {
    o.extend_from_slice(b"MTrk");
    o.extend_from_slice(&(body.len() as u32).to_be_bytes());
    o.extend_from_slice(body);
}

fn export(d: &Daw) -> Vec<u8> {
    const PPQ: f64 = 480.0;
    let p = &d.project;
    let mut o = Vec::new();
    let tracks: Vec<&TrackDoc> = p.tracks.iter().filter(|t| !t.is_audio()).collect();
    o.extend_from_slice(b"MThd");
    o.extend_from_slice(&6u32.to_be_bytes());
    o.extend_from_slice(&1u16.to_be_bytes());
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    // the tempo track
    let mut t0 = Vec::new();
    let us = (60_000_000.0 / p.tempo_bpm) as u32;
    t0.extend_from_slice(&[0, 0xff, 0x51, 3, (us >> 16) as u8, (us >> 8) as u8, us as u8]);
    t0.extend_from_slice(&[0, 0xff, 0x03]);
    put_vlq(&mut t0, p.name.len() as u32);
    t0.extend_from_slice(p.name.as_bytes());
    t0.extend_from_slice(&[0, 0xff, 0x2f, 0]);
    bodies.push(t0);
    let mut chan_no = 0u8;
    for t in tracks {
        let mut evs: Vec<(u32, u8, [u8; 3])> = Vec::new();
        let drums = t.instrument == "drums" || t.instrument == "drum";
        for c in t.clips.iter().filter(|c| !c.is_audio()) {
            for x in &c.midi_notes {
                if x.start >= c.len {
                    continue;
                }
                let len = x.len.min(c.len - x.start).max(0.03);
                let s = libm::round((c.start + x.start) * PPQ) as u32;
                let e = (libm::round((c.start + x.start + len) * PPQ) as u32).max(s + 1);
                evs.push((s, 3, [0x90, x.key, (x.vel * 127.0).clamp(1.0, 127.0) as u8]));
                evs.push((e, 0, [0x80, x.key, 0]));
            }
        }
        if evs.is_empty() {
            continue;
        }
        evs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let chan = if drums {
            9
        } else {
            let c = chan_no % 16;
            chan_no += 1;
            if c == 9 { 10 } else { c }
        };
        let mut body = Vec::new();
        body.extend_from_slice(&[0, 0xff, 0x03]);
        put_vlq(&mut body, t.name.len() as u32);
        body.extend_from_slice(t.name.as_bytes());
        let mut last = 0u32;
        for (tick, _, m) in evs {
            put_vlq(&mut body, tick - last);
            last = tick;
            body.extend_from_slice(&[m[0] | chan, m[1], m[2]]);
        }
        body.extend_from_slice(&[0, 0xff, 0x2f, 0]);
        bodies.push(body);
    }
    o.extend_from_slice(&(bodies.len() as u16).to_be_bytes());
    o.extend_from_slice(&480u16.to_be_bytes());
    for b in &bodies {
        chunk(&mut o, b);
    }
    o
}

/// The file `daw_import_prepare` was given (the desktop reads and decodes it on a thread first; here the import
/// builtins read the file themselves, so preparing is only checking it can be read).
static mut PREPARED: Option<(String, Result<(), String>)> = None;

pub(super) fn dispatch(name: &[u8], args: &[u64], d: &mut Daw) -> Option<u64> {
    if name == b"daw_import_prepare" {
        // (path): the app then polls; an unreadable file fails the poll with the reason
        let path = args.first().map(|&v| to_string(v)).unwrap_or_default().replace('\\', "/");
        let r = if crate::vfs::exists(&path) { Ok(()) } else { Err(alloc::format!("{path}: not found")) };
        unsafe { PREPARED = Some((path, r)) };
        return Some(crate::lingrt::UNIT);
    }
    if name == b"daw_import_poll" {
        // [state, progress 0..1, message]: 2 ready, -1 failed, 0 nothing prepared (the desktop's 1 = still working)
        let v = match unsafe { (*(&raw const PREPARED)).clone() } {
            Some((_, Ok(()))) => alloc::vec![num(2.0), num(1.0), mk_str("ready")],
            Some((_, Err(e))) => alloc::vec![num(-1.0), num(0.0), mk_str(&e)],
            None => alloc::vec![num(0.0), num(0.0), mk_str("")],
        };
        return Some(list_from(v));
    }
    if !name.starts_with(b"daw_midi_") {
        return None;
    }
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).unwrap_or(0.0);
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default().replace('\\', "/");
    Some(match name {
        b"daw_midi_scan" => {
            // path -> [name, program (-1 drums), channel 1..16, notes, bank, GM name, percussive, own samples]* in import order
            let Some(bytes) = crate::vfs::read(&s(0)) else { return Some(list_from(Vec::new())) };
            let Ok(song) = parse(&bytes) else { return Some(list_from(Vec::new())) };
            let gm = gm_patches();
            let mut v = Vec::new();
            for pt in &song.parts {
                let gname = match pt.program {
                    Some(pr) => gm.iter().find(|g| g.0 == pr).map(|g| g.1.clone()).unwrap_or_else(|| alloc::format!("Program {}", pr + 1)),
                    None => String::from("Drums"),
                };
                v.extend([mk_str(&pt.name), num(pt.program.map(|x| x as f64).unwrap_or(-1.0)), num(pt.channel as f64 + 1.0), num(pt.notes.len() as f64), num(0.0), mk_str(&gname), num((pt.program.is_none()) as u8 as f64), num(0.0)]);
            }
            list_from(v)
        },
        b"daw_midi_import" => {
            // (path, beat) -> tracks made, -1 on error (the reason in the status)
            let path = s(0);
            let song = match crate::vfs::read(&path).ok_or_else(|| alloc::format!("{path}: not found")).and_then(|b| parse(&b)) {
                Ok(x) => x,
                Err(e) => {
                    d.status = alloc::format!("MIDI import failed: {e}");
                    return Some(num(-1.0));
                },
            };
            let start = n(1).max(0.0);
            let p = &mut d.project;
            if p.tracks.iter().all(|t| t.clips.is_empty()) {
                if let Some(b) = song.bpm {
                    p.tempo_bpm = b.clamp(20.0, 300.0);
                }
            }
            let gm = gm_patches();
            // optional third argument, one choice per part (the import dialog): "" / "auto" / "gm" = the GM patch,
            // a .ling path = play the part with that patch, "skip" = leave the part out
            let choices: Vec<String> = match args.get(2) {
                Some(&v) if crate::lingrt::is_list(v) => crate::lingrt::list_items(v).iter().map(|&x| to_string(x).replace('\\', "/")).collect(),
                _ => Vec::new(),
            };
            let mut made = 0usize;
            for (pi, pt) in song.parts.into_iter().enumerate() {
                let choice = choices.get(pi).cloned().unwrap_or_default();
                if choice == "skip" {
                    continue;
                }
                if p.tracks.len() >= 256 {
                    break;
                }
                let drums = pt.program.is_none();
                let end = pt.notes.iter().map(|x| x.0 + x.1).fold(0.0, f64::max);
                let len = libm::ceil(end.max(1.0) / 4.0) * 4.0;
                let notes: Vec<NoteDoc> = pt.notes.iter().map(|x| NoteDoc::new(x.0, x.1, x.2, x.3.clamp(0.01, 1.0))).collect();
                let mut t = TrackDoc {
                    name: pt.name.clone(),
                    color: super::PALETTE[p.tracks.len() % 7],
                    kind: String::from("instrument"),
                    instrument: String::from(if drums { "drums" } else { "synth" }),
                    patch: String::new(),
                    level: 1.0,
                    pan: 0.0,
                    muted: false,
                    solo: false,
                    effects: Vec::new(),
                    clips: alloc::vec![ClipDoc { name: pt.name.clone(), start, len, midi_notes: notes, ..ClipDoc::default() }],
                    state: Vec::new(),
                    config: String::new(),
                    session: Vec::new(),
                    auto: Vec::new(),
                };
                if let Some(pr) = pt.program {
                    if let Some(g) = gm.iter().find(|g| g.0 == pr) {
                        let bytes = crate::appfs::get(&g.2).map(|b| b.to_vec()).or_else(|| crate::vfs::read(&g.2));
                        if let Some(bytes) = bytes {
                            apply_patch(&mut t, core::str::from_utf8(&bytes).unwrap_or(""), &g.2);
                        }
                        if t.name.starts_with("Track ") {
                            t.name = g.1.clone();
                        }
                    }
                }
                // a patch the user picked for this part wins over the GM one
                if choice.ends_with(".ling") {
                    let bytes = crate::appfs::get(&choice).map(|b| b.to_vec()).or_else(|| crate::vfs::read(&choice));
                    if let Some(bytes) = bytes {
                        let keep = t.name.clone();
                        apply_patch(&mut t, core::str::from_utf8(&bytes).unwrap_or(""), &choice);
                        t.name = keep;
                    }
                }
                p.tracks.push(t);
                made += 1;
            }
            d.dirty = true;
            d.rev += 1;
            d.status = alloc::format!("Imported {made} part(s) from {path}");
            num(made as f64)
        },
        b"daw_midi_export_project" => {
            let path = s(0);
            let ok = crate::vfs::write(&path, &export(d));
            if !ok {
                d.status = alloc::format!("MIDI export failed: cannot write {path}");
            }
            crate::lingrt::boolean(ok)
        },
        b"daw_midi_close" => {
            d.status = String::from("MIDI input closed");
            crate::lingrt::UNIT
        },
        _ => return None,
    })
}
