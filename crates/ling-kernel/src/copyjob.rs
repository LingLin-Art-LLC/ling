//! Background copy / move for Files (and anything else that pastes big things).
//!
//! `vfs::copy` reads each whole file and writes it back in one call: a 200 MB
//! paste from a USB stick held the desktop -- and with it a USB mouse -- for
//! minutes. Here a paste is a queued job that advances in time slices from
//! every presented frame (`tick`, called by `lingapp::present_inner`), so the
//! screen keeps drawing and the pointer keeps moving.
//!
//! Every file is streamed in steps of at most 1 MiB, whatever the two ends are:
//! a removable / data volume is read with ranged reads and written with a
//! stream (`fs::media`); a path that only *looks* like lingfs -- `home/...` on
//! an installed PC lives on its data partition -- is mapped to that volume
//! first (`vfs::storage_path`); a lingfs destination is written block by block
//! (`lingfs::ContentWriter`) and linked in at the end, after a room check so a
//! video too big for lingfs fails at once instead of after filling it. Only
//! lingfs sources (a few MiB at most) and keyring-sealed files (sealed in one
//! piece) are read whole. Folders are listed one per step as the job reaches
//! them, so a tree of thousands of files starts at once. A move that only
//! relinks (inside one volume) is done at once (`vfs::rename_in_place`).
//!
//! Builtins (lingapp): fs_copy_start(src, dst, cut) queues one item;
//! fs_copy_state() 0 idle / 1 running / 2 done / 3 failed (until acked);
//! fs_copy_progress() 0..1; fs_copy_message(); fs_copy_ack(); fs_copy_cancel().

use alloc::string::String;
use alloc::vec::Vec;

/// Disk work per frame: the frame stays responsive (the pointer is moved by
/// its interrupt anyway on PS/2; USB mice are serviced between frames).
const SLICE_US: u64 = 9_000;
/// Bytes per step: adapts to the device (a USB stick or an emulated IDE disk
/// moves 4 KiB in the time an NVMe drive moves 1 MiB: emulated IDE is port I/O,
/// about a millisecond a sector under QEMU in WSL), so one step never
/// holds a frame for long.
const CHUNK_MIN: usize = 4 << 10;
const CHUNK_MAX: usize = 1 << 20;
static mut CHUNK: usize = CHUNK_MIN * 4;
/// Object slots kept free in lingfs after a copied file (its commits, the
/// desktop's own small writes).
const LINGFS_SPARE_OBJECTS: usize = 64;

enum Step {
    /// a folder: make it at the destination, then list it (its entries become the next steps)
    Scan(String, String),
    Mkdir(String),
    /// copy one file (src, dst, size from the listing)
    File(String, String, u64),
    /// a cut item, once everything under it is copied: delete the original
    DeleteSrc(String),
    /// the last of a cut folder: the folder itself, its entries deleted by the steps before
    DeleteDir(String),
}

/// Where a file's bytes come from.
enum Src {
    Range(crate::fs::media::MediaRange),
    Mem(Vec<u8>),
}

/// Where they go.
enum Out {
    Media(crate::fs::media::MediaFile),
    /// streamed into lingfs blocks; linked in at the (resolved) path when done
    Lingfs(crate::fs::lingfs::ContentWriter, String),
    /// collected and written in one `vfs::write` (keyring-sealed files)
    Whole(Vec<u8>),
}

struct Stream {
    src: Src,
    out: Out,
    /// the destination as Files names it
    dst: String,
    off: u64,
    size: u64,
}

struct Job {
    steps: Vec<Step>,
    next: usize,
    stream: Option<Stream>,
    total: u64,
    done: u64,
    items: usize,
    failed: Option<String>,
    cut: bool,
}

struct State {
    state: u8,
    msg: String,
    job: Option<Job>,
    cancel: bool,
    busy: bool,
    buf: Vec<u8>,
}

static mut ST: State = State { state: 0, msg: String::new(), job: None, cancel: false, busy: false, buf: Vec::new() };

fn st() -> &'static mut State {
    unsafe { &mut *(&raw mut ST) }
}

fn sealed(n: &str) -> bool {
    crate::keyring::is_private_path(&crate::vfs::resolve(n))
}

/// The removable / data-volume file holding `src`'s bytes, if it is on one (the order
/// `vfs::read` looks in: the data volume, lingfs, the package volume's home copy).
fn media_source(src: &str) -> Option<String> {
    use crate::fs::media;
    let n = crate::vfs::norm(src);
    if media::is_media_path(&n) {
        return Some(n);
    }
    if sealed(&n) {
        return None;
    }
    let r = crate::vfs::resolve(&n);
    let stored = crate::vfs::storage_path(&n);
    if let Some(c) = &stored {
        if media::exists(c) {
            return Some(c.clone());
        }
    }
    if crate::vfs::ready() && (crate::fs::lingfs::path_kind(&r).is_some() || crate::fs::lingfs::path_kind(&n).is_some()) {
        return None;
    }
    if r.starts_with("home/") && stored.is_none() {
        if let Some(v) = crate::vfs::package_volume() {
            let c = alloc::format!("media/{v}/{r}");
            if media::exists(&c) {
                return Some(c);
            }
        }
    }
    None
}

/// A file's size from its folder's listing (one listing per folder, cached by vfs).
fn size_from_listing(p: &str) -> u64 {
    let name = crate::vfs::basename(p);
    crate::vfs::list(&crate::vfs::parent(p)).into_iter().find(|(n, _, _)| *n == name).map(|(_, _, s)| s as u64).unwrap_or(0)
}

/// Queue one item. A cut that only relinks (one volume) is done now.
pub fn start(src: &str, dst: &str, cut: bool) -> bool {
    let (src, dst) = (crate::vfs::norm(src), crate::vfs::norm(dst));
    if src.is_empty() || dst.is_empty() || src == dst || dst.starts_with(&alloc::format!("{src}/")) {
        return false;
    }
    if !crate::vfs::exists(&src) {
        return false;
    }
    if cut && crate::vfs::rename_in_place(&src, &dst) {
        return true;
    }
    let r = st();
    if r.state >= 2 && r.job.is_none() {
        r.state = 0; // a finished job nobody acked: start fresh
    }
    if r.job.is_none() {
        // a new job may be on another device: start small, the steps grow to fit the frame
        unsafe { CHUNK = CHUNK_MIN * 4 };
    }
    let job = r.job.get_or_insert_with(|| Job { steps: Vec::new(), next: 0, stream: None, total: 0, done: 0, items: 0, failed: None, cut });
    let parent = crate::vfs::parent(&dst);
    if !parent.is_empty() {
        job.steps.push(Step::Mkdir(parent));
    }
    if crate::vfs::is_dir(&src) {
        job.steps.push(Step::Scan(src.clone(), dst.clone()));
    } else {
        let size = size_from_listing(&src).max(1);
        job.total += size;
        job.steps.push(Step::File(src.clone(), dst.clone(), size));
    }
    if cut {
        job.steps.push(Step::DeleteSrc(src.clone()));
    }
    job.items += 1;
    job.cut |= cut;
    r.state = 1;
    r.cancel = false;
    r.msg = alloc::format!("{} {}", if cut { "moving" } else { "copying" }, crate::vfs::basename(&src));
    true
}

/// Advance the queue by one time slice (from every presented frame).
pub fn tick() {
    let r = st();
    if r.state != 1 || r.busy {
        return;
    }
    r.busy = true;
    let t = crate::arch::timer::now_us();
    crate::hitch::timed(b"copy job", tick_inner);
    unsafe {
        STATS.slices += 1;
        STATS.slice_us += crate::arch::timer::now_us().saturating_sub(t);
    }
    st().busy = false;
}

fn finish(ok: bool, msg: String) {
    let r = st();
    r.job = None;
    r.state = if ok { 2 } else { 3 };
    r.msg = msg;
    r.buf = Vec::new();
}

fn abort_out(out: Out) {
    match out {
        Out::Media(m) => crate::fs::media::file_abort(m),
        // lingfs never frees a block: what was written stays until a compaction
        Out::Lingfs(..) | Out::Whole(_) => {},
    }
}

/// Close a finished stream. Err(why) when the destination refused it.
fn finish_out(out: Out, dst: &str) -> Result<(), String> {
    let r = match out {
        Out::Media(m) => {
            if crate::fs::media::file_finish(m) {
                Ok(())
            } else {
                let why = unsafe { crate::fs::fat::STREAM_ERR };
                Err(String::from(if why.is_empty() { "the volume refused it" } else { why }))
            }
        },
        Out::Lingfs(w, path) => match w.finish() {
            Ok(h) => crate::fs::lingfs::link_file(&path, h).map_err(|_| String::from("the system storage refused it")),
            Err(_) => Err(String::from("the system storage is full")),
        },
        Out::Whole(data) => {
            if crate::vfs::write(dst, &data) {
                Ok(())
            } else {
                Err(String::from("the write was refused"))
            }
        },
    };
    crate::vfs::note_change(dst);
    r
}

/// Open the two ends of one file copy. Err(message) when it cannot start.
fn open_stream(src: &str, dst: &str) -> Result<Stream, String> {
    use crate::fs::{lingfs, media};
    let name = crate::vfs::basename(src);
    // the source
    let (src_io, size) = match media_source(src) {
        Some(c) => {
            let rng = media::open_range(&c).ok_or_else(|| alloc::format!("could not read {name}"))?;
            let size = rng.size();
            (Src::Range(rng), size)
        },
        // lingfs (a few MiB at most), the image, a sealed file: in one piece
        None => {
            let data = crate::vfs::read(src).ok_or_else(|| alloc::format!("could not read {name}"))?;
            let size = data.len() as u64;
            (Src::Mem(data), size)
        },
    };
    // the destination
    let n = crate::vfs::norm(dst);
    let target = if media::is_media_path(&n) {
        Some(n.clone())
    } else if sealed(&n) {
        None
    } else {
        crate::vfs::storage_path(&n)
    };
    let out = if let Some(c) = target {
        if !media::is_media_path(&n) && !crate::modes::writable(&crate::vfs::resolve(&n)) {
            return Err(alloc::format!("{dst} is read-only"));
        }
        let p = crate::vfs::parent(&c);
        if !p.is_empty() && !media::is_dir(&p) {
            media::mkdir_p(&p);
        }
        if size > media::max_file(&c) {
            return Err(alloc::format!("{name} is too big for that drive (FAT holds files up to 4 GiB)"));
        }
        let _ = media::delete(&c);
        Out::Media(media::file_create(&c).ok_or_else(|| alloc::format!("could not create {dst}"))?)
    } else if sealed(&n) {
        Out::Whole(Vec::with_capacity(size as usize))
    } else {
        if !crate::vfs::ready() {
            return Err(alloc::format!("could not create {dst}"));
        }
        let r = crate::vfs::resolve(&n);
        if r.is_empty() || !crate::modes::writable(&r) {
            return Err(alloc::format!("{dst} is read-only"));
        }
        // lingfs is a few MiB and never frees a block: check the room before the first byte
        let need = lingfs::objects_for(size as usize) + LINGFS_SPARE_OBJECTS;
        if size as usize > lingfs::MAX_FILE_BYTES || need > lingfs::free_objects() {
            let free = lingfs::free_objects().saturating_sub(LINGFS_SPARE_OBJECTS) * lingfs::BLOCK_SIZE;
            return Err(alloc::format!(
                "{name} ({} MiB) does not fit in the system storage ({} MiB free) -- paste it onto a data drive (media/...)",
                size.div_ceil(1 << 20),
                free >> 20
            ));
        }
        Out::Lingfs(lingfs::ContentWriter::new(), r)
    };
    Ok(Stream { src: src_io, out, dst: String::from(dst), off: 0, size })
}

/// Every missing folder on the way (a removable volume's mkdir is one level).
fn make_dir(d: &str) -> bool {
    crate::vfs::is_dir(d) || if crate::fs::media::is_media_path(d) { crate::fs::media::mkdir_p(d) } else { crate::vfs::mkdir(d) }
}

fn tick_inner() {
    let t0 = crate::arch::timer::now_us();
    let r = st();
    if r.buf.len() < CHUNK_MAX {
        r.buf = alloc::vec![0u8; CHUNK_MAX];
    }
    loop {
        let r = st();
        if r.cancel {
            if let Some(s) = r.job.as_mut().and_then(|j| j.stream.take()) {
                abort_out(s.out);
            }
            finish(false, String::from("cancelled -- what was already copied stays"));
            return;
        }
        if crate::arch::timer::now_us().saturating_sub(t0) >= SLICE_US {
            return;
        }
        let Some(j) = r.job.as_mut() else { return };
        // a file being streamed: one chunk
        if let Some(s) = j.stream.as_mut() {
            let chunk = unsafe { CHUNK };
            let n = core::cmp::min(chunk as u64, s.size - s.off) as usize;
            let t = crate::arch::timer::now_us();
            let ok = n == 0 || {
                let data: &[u8] = match &s.src {
                    Src::Range(rng) => {
                        let buf = &mut r.buf[..n];
                        let tr = crate::arch::timer::now_us();
                        let got = rng.read_at(s.off, buf);
                        unsafe { STATS.read_us += crate::arch::timer::now_us().saturating_sub(tr) };
                        if got {
                            buf
                        } else {
                            &[]
                        }
                    },
                    Src::Mem(v) => &v[s.off as usize..s.off as usize + n],
                };
                let tw = crate::arch::timer::now_us();
                let wrote = data.len() == n
                    && match &mut s.out {
                        Out::Media(m) => crate::fs::media::file_write(m, data),
                        Out::Lingfs(w, _) => w.write(data).is_ok(),
                        Out::Whole(v) => {
                            v.extend_from_slice(data);
                            true
                        },
                    };
                unsafe { STATS.write_us += crate::arch::timer::now_us().saturating_sub(tw) };
                wrote
            };
            unsafe {
                STATS.steps += 1;
                STATS.bytes += n as u64;
                STATS.max_step_us = STATS.max_step_us.max(crate::arch::timer::now_us().saturating_sub(t));
            }
            // steer the step size toward a quarter of the frame budget
            let dt = crate::arch::timer::now_us().saturating_sub(t);
            unsafe {
                if dt > SLICE_US / 2 && CHUNK > CHUNK_MIN {
                    CHUNK /= 2;
                } else if dt < SLICE_US / 8 && n == chunk && CHUNK < CHUNK_MAX {
                    CHUNK *= 2;
                }
            }
            if !ok {
                let s = j.stream.take().unwrap();
                let name = crate::vfs::basename(&s.dst);
                abort_out(s.out);
                finish(false, alloc::format!("could not copy {name} (is the drive full or read-only?)"));
                return;
            }
            s.off += n as u64;
            j.done += n as u64;
            if s.off >= s.size {
                let s = j.stream.take().unwrap();
                if let Err(why) = finish_out(s.out, &s.dst) {
                    let m = alloc::format!("could not finish writing {}: {why}", s.dst);
                    crate::console_write(alloc::format!("copyjob: {m}\n").as_bytes());
                    finish(false, m);
                    return;
                }
            }
            continue;
        }
        if j.next >= j.steps.len() {
            let (items, cut) = (j.items, j.cut);
            if let Some(f) = j.failed.take() {
                finish(false, f);
                return;
            }
            finish(true, alloc::format!("{} {} item{}", if cut { "moved" } else { "copied" }, items, if items == 1 { "" } else { "s" }));
            return;
        }
        let step = j.next;
        j.next += 1;
        match &j.steps[step] {
            Step::Mkdir(d) => {
                if !make_dir(d) {
                    let d = d.clone();
                    finish(false, alloc::format!("could not make the folder {d}"));
                    return;
                }
            },
            Step::Scan(src, dst) => {
                let (src, dst) = (src.clone(), dst.clone());
                if !make_dir(&dst) {
                    finish(false, alloc::format!("could not make the folder {dst}"));
                    return;
                }
                // this folder's entries run next (before a cut's DeleteSrc of a parent)
                let mut add: Vec<Step> = Vec::new();
                for (name, is_dir, size) in crate::vfs::list(&src) {
                    let (a, b) = (crate::vfs::join(&src, &name), crate::vfs::join(&dst, &name));
                    if is_dir {
                        add.push(Step::Scan(a, b));
                    } else {
                        let size = (size as u64).max(1);
                        j.total += size;
                        add.push(Step::File(a, b, size));
                    }
                }
                j.steps.splice(step + 1..step + 1, add);
            },
            Step::DeleteSrc(p) => {
                let p = p.clone();
                // a folder goes one entry a step (its entries first, then the emptied folder):
                // deleting 300 files in one go held a frame for most of a second
                if crate::vfs::is_dir(&p) {
                    let kids = crate::vfs::list(&p);
                    if !kids.is_empty() {
                        let mut add: Vec<Step> = kids.into_iter().map(|(name, _, _)| Step::DeleteSrc(crate::vfs::join(&p, &name))).collect();
                        add.push(Step::DeleteDir(p));
                        j.steps.splice(step + 1..step + 1, add);
                        continue;
                    }
                }
                let ok = crate::vfs::delete(&p);
                if !ok {
                    j.failed = Some(alloc::format!("copied, but could not remove the original {}", crate::vfs::basename(&p)));
                }
            },
            Step::DeleteDir(p) => {
                let p = p.clone();
                if !crate::vfs::delete(&p) {
                    j.failed = Some(alloc::format!("copied, but could not remove the original {}", crate::vfs::basename(&p)));
                }
            },
            Step::File(src, dst, planned) => {
                let (src, dst, planned) = (src.clone(), dst.clone(), *planned);
                match open_stream(&src, &dst) {
                    Ok(s) => {
                        // the listing's size was the plan; the real one counts now
                        j.total = j.total.saturating_sub(planned) + s.size.max(1);
                        if s.size == 0 {
                            j.done += 1;
                        }
                        j.stream = Some(s);
                    },
                    Err(m) => {
                        crate::console_write(alloc::format!("copyjob: {m}\n").as_bytes());
                        finish(false, m);
                        return;
                    },
                }
            },
        }
    }
}

/// What the job's slices did (`bgcp` prints it): where a slow copy spends its time.
#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub slices: u64,
    pub slice_us: u64,
    pub steps: u64,
    pub bytes: u64,
    pub read_us: u64,
    pub write_us: u64,
    pub max_step_us: u64,
}
static mut STATS: Stats = Stats { slices: 0, slice_us: 0, steps: 0, bytes: 0, read_us: 0, write_us: 0, max_step_us: 0 };

/// The job's counters since the last call, as one line.
pub fn stats_line() -> String {
    let s = unsafe { core::mem::take(&mut *(&raw mut STATS)) };
    alloc::format!(
        "copy job: {} slices ({} ms), {} steps of {} KiB avg, read {} ms, write {} ms, longest step {} ms, step size now {} KiB",
        s.slices,
        s.slice_us / 1000,
        s.steps,
        s.bytes / s.steps.max(1) / 1024,
        s.read_us / 1000,
        s.write_us / 1000,
        s.max_step_us / 1000,
        unsafe { CHUNK } / 1024
    )
}

pub fn state() -> u8 {
    st().state
}
pub fn progress() -> f64 {
    let r = st();
    match &r.job {
        Some(j) if j.total > 0 => (j.done as f64 / j.total as f64).min(1.0),
        _ => {
            if r.state == 2 {
                1.0
            } else {
                0.0
            }
        },
    }
}
pub fn message() -> String {
    let r = st();
    match &r.job {
        Some(j) => {
            let cur = j.steps.get(j.next.saturating_sub(1)).and_then(|s| match s {
                Step::File(a, _, _) => Some(crate::vfs::basename(a)),
                _ => None,
            });
            let mb = |b: u64| b as f64 / (1u64 << 20) as f64;
            alloc::format!(
                "{} {:.0}%  ({:.1} of {:.1} MB){}",
                if j.cut { "moving" } else { "copying" },
                progress() * 100.0,
                mb(j.done),
                mb(j.total),
                cur.map(|c| alloc::format!("  {c}")).unwrap_or_default()
            )
        },
        None => r.msg.clone(),
    }
}
pub fn ack() {
    let r = st();
    if r.state != 1 {
        r.state = 0;
        r.msg.clear();
    }
}
pub fn cancel() {
    st().cancel = true;
}
