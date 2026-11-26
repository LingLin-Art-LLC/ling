//! What an app leaves behind goes with it: the per-app resource ledger.
//!
//! The kernel's resource tables (sound samples and voices, g3d textures /
//! meshes / colliders / particles, glTF models, pictures, the net_* lobby,
//! WebSockets, HTTP jobs) are global and indexed by handle, and an app's
//! handles used to live forever: every launch of a game loaded its assets
//! again on top of the last one's, its looping music played on after it
//! quit, its sockets stayed open. Each table now keeps an [`Owners`] column
//! -- which app made each entry -- and when an app ends (its entry returned,
//! Task Manager's End, or its background turn ended it) [`app_ended`] walks
//! every table and frees that app's entries, then logs one line saying what
//! went.
//!
//! Owner ids: every launch gets a fresh `uid` (lingapp's AppCtx); 0 is the
//! desktop / the system (boot services, kernel-side players), which is never
//! freed this way. A green thread counts for the app that started it.
//!
//! Freed entries keep their index and become empty (a 1x1 texture, a silent
//! sample, a mesh with no triangles): ids are never handed out again, so a
//! number some cache or another program still holds can only ever point at
//! nothing, not at somebody else's new resource. An empty slot costs a few
//! dozen bytes.

use alloc::string::String;
use alloc::vec::Vec;

/// An entry that belonged to an app that ended (empty now).
pub const FREED: u32 = u32::MAX;

/// Who owns new resources made right now: the running app (or the app whose
/// green thread is running), 0 for the desktop and the system.
pub fn owner() -> u32 {
    crate::lingapp::res_owner()
}

/// One table's owner column, index-aligned with the table.
pub struct Owners(Vec<u32>);

impl Owners {
    pub const fn new() -> Owners {
        Owners(Vec::new())
    }

    /// The table has `len` entries now: entries the column has not seen yet
    /// were made by `who` (call it right after the code that may push).
    pub fn sync(&mut self, len: usize, who: u32) {
        if self.0.len() > len {
            self.0.truncate(len);
        }
        while self.0.len() < len {
            self.0.push(who);
        }
    }

    /// Entry `i` (made or replaced in place) belongs to `who`.
    pub fn set(&mut self, i: usize, who: u32) {
        if self.0.len() <= i {
            self.0.resize(i + 1, 0);
        }
        self.0[i] = who;
    }

    pub fn of(&self, i: usize) -> u32 {
        self.0.get(i).copied().unwrap_or(0)
    }

    /// The indices `uid` owns; they are marked FREED (the caller empties them).
    pub fn take(&mut self, uid: u32) -> Vec<usize> {
        let mut out = Vec::new();
        if uid == 0 || uid == FREED {
            return out;
        }
        for (i, o) in self.0.iter_mut().enumerate() {
            if *o == uid {
                *o = FREED;
                out.push(i);
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// What one app's end gave back (the exit log line).
#[derive(Default)]
pub struct Freed {
    /// voices / tones stopped, sound samples freed
    pub voices: u32,
    pub sounds: u32,
    /// net_* lobby peers, WebSockets, HTTP jobs, TCP sockets
    pub sockets: u32,
    pub jobs: u32,
    /// g3d textures, meshes, colliders, glTF models, particles
    pub g3d: u32,
    pub particles: u32,
    /// pictures (image_load / run-time images)
    pub images: u32,
    /// media players / decoders
    pub media: u32,
    /// green threads stopped, locks they held
    pub threads: u32,
    pub locks: u32,
    /// bytes the tables let go of (PCM, texels, vertices, pixels)
    pub bytes: u64,
}

fn free_kib() -> u64 {
    crate::mm::frame::free_frame_count() as u64 * 4
}

/// `name` (owner id `uid`) ended: free everything it made. `how`: "quit",
/// "ended by Task Manager", ... From the desktop's stack (never from inside
/// the app or a green thread). `coros`: what coro::kill_owner did (green
/// threads stopped, locks they held, sockets they had open).
pub fn app_ended(uid: u32, name: &str, how: &str, coros: (usize, u32, u32), stack_kib: u64) {
    let before = free_kib();
    let (threads, locks, socks) = coros;
    let mut f = Freed { threads: threads as u32, locks, sockets: socks, ..Freed::default() };
    if uid != 0 {
        let sounds = crate::drivers::sfx::app_ended(uid, &mut f);
        crate::compat::app_ended(uid, &sounds, &mut f);
        crate::drivers::g3d::app_ended(uid, &mut f);
        crate::lingsock::app_ended(uid, &mut f);
        // its net_* lobby and HTTP jobs
        crate::netapi::app_ended(uid, &mut f);
    }
    crate::lingapp::fonts_trim();
    let after = free_kib();
    let back_kib = after.saturating_sub(before) + stack_kib;
    let mut s = String::new();
    use core::fmt::Write;
    let _ = write!(
        s,
        "app: {name} {how} -- freed {} sound(s) + {} voice(s), {} socket(s) + {} http job(s), {} g3d object(s) + {} particle(s), {} picture(s), {} thread(s) ({} lock(s)); {} MiB returned, {} MiB free\n",
        f.sounds,
        f.voices,
        f.sockets,
        f.jobs,
        f.g3d,
        f.particles,
        f.images,
        f.threads,
        f.locks,
        (back_kib + 512) / 1024,
        after / 1024,
    );
    crate::console_write(s.as_bytes());
}
