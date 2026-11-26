//! Files' extras for the desktop: Properties, a per-drive Trash, and the disk tools.
//!
//! * Properties (`fsx_info`, `fsx_info_many`): what each file system records about a file -- where
//!   its bytes really are (a data volume, lingfs, or the read-only image), its size on disk (whole
//!   clusters on FAT / exFAT, 4 KiB blocks on lingfs), the create / modify / access times FAT and
//!   exFAT keep, the attribute bits, lingfs's owner and group, the Unix mode `chmod` set, the drive,
//!   its file system and free space. A folder's (or a selection's) totals come from a background
//!   walk (`fsx_du_*`) that advances a few milliseconds per presented frame.
//! * The Trash (freedesktop.org layout): a deleted item is *moved*, never erased -- into the trash
//!   on its own drive, so the move is a rename (instant whatever the size): `home/<user>/.Trash` for
//!   anything on the home folder's drive, `media/<vol>/.Trash-<user>` on any other data volume or
//!   USB stick. `files/<name>` holds the item, `info/<name>.trashinfo` where it came from and when
//!   (`Path=`, `DeletionDate=` in local time, plus `Unix=` / `Size=` / `File=`). Nothing ever
//!   empties it on its own: only `fsx_trash_delete` / `fsx_trash_empty` (the Trash app) and
//!   `fsx_purge` (Files' Shift+Delete, after a confirmation) erase anything, in a background job.
//! * Disk tools (the Trash app's Disk tab): every drive's use (`fsx_drives`), a fragmentation
//!   analysis (`fsx_frag_*`: how many FAT / exFAT files are in more than one piece; for lingfs how
//!   many stored blocks nothing refers to any more), a cleanup scan (`fsx_clean_*`: unfinished
//!   screen recordings, leftover temp files, app caches, old logs -- listed first, deleted only when
//!   asked) and "optimize" (`fsx_opt_*`): lingfs's own compaction; on FAT / exFAT each fragmented
//!   file is copied to a fresh run, the copy is compared byte for byte with the original, and only
//!   then does the copy take the original's place -- at every moment one complete copy exists.
//! * The "Burn all trash" animation and the sound effects are in `fsx_burn.rs`.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::fs::{lingfs, media};
use crate::lingrt::{list_from, list_items, mk_str, num, to_f64, to_string, UNIT};
use crate::vfs;

pub(crate) fn sarg(args: &[u64], i: usize) -> String {
    args.get(i).map(|&v| to_string(v)).unwrap_or_default()
}

pub(crate) fn narg(args: &[u64], i: usize, d: f64) -> f64 {
    args.get(i).map(|&v| to_f64(v)).unwrap_or(d)
}

fn now_us() -> u64 {
    crate::arch::timer::now_us()
}

/// The desktop's time zone (Settings > System), seconds east of UTC.
static mut TZ: i64 = 0;

fn tz() -> i64 {
    unsafe { TZ }
}

// ── formatting ───────────────────────────────────────────────────────────────

fn civil(t: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (t / 86400) as i64;
    let secs = (t % 86400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d, secs / 3600, (secs / 60) % 60, secs % 60)
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// "15 Nov 2026, 14:03:09" for a time as stored (no zone applied).
fn fmt_stamp(t: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil(t);
    format!("{d} {} {y}, {hh:02}:{mm:02}:{ss:02}", MONTHS[(m as usize).clamp(1, 12) - 1])
}

/// A unix time in the desktop's zone.
fn fmt_local(t: u64) -> String {
    fmt_stamp((t as i64 + tz()).max(0) as u64)
}

/// "2026-11-15T14:03:09" (the trashinfo DeletionDate, local time).
fn iso_local(t: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil((t as i64 + tz()).max(0) as u64);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}")
}

fn grouped(n: u64) -> String {
    let s = format!("{n}");
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub(crate) fn size_short(n: u64) -> String {
    let f = n as f64;
    if n < 1024 {
        format!("{n} B")
    } else if n < 1 << 20 {
        format!("{:.1} KB", f / 1024.0)
    } else if n < 1 << 30 {
        format!("{:.1} MB", f / 1048576.0)
    } else {
        format!("{:.2} GB", f / 1073741824.0)
    }
}

fn size_long(n: u64) -> String {
    if n < 1024 {
        format!("{} bytes", grouped(n))
    } else {
        format!("{} ({} bytes)", size_short(n), grouped(n))
    }
}

// ── where a path's bytes are ─────────────────────────────────────────────────

pub(crate) enum Place {
    /// on a FAT / exFAT volume, at this `media/<vol>/...` path
    Media(String),
    /// in lingfs, at this resolved path
    Lingfs(String),
    /// built into the LingOS image (read-only)
    Image(String),
    Missing,
}

/// The order `vfs::read` looks in: a media path itself, the data volume the path maps to, lingfs, the image.
pub(crate) fn locate(p: &str) -> Place {
    let n = vfs::norm(p);
    if media::is_media_path(&n) {
        return if media::exists(&n) { Place::Media(n) } else { Place::Missing };
    }
    let r = vfs::resolve(&n);
    if let Some(c) = vfs::storage_path(&n) {
        if media::exists(&c) {
            return Place::Media(c);
        }
    }
    if vfs::ready() {
        if lingfs::path_kind(&r).is_some() {
            return Place::Lingfs(r);
        }
        if lingfs::path_kind(&n).is_some() {
            return Place::Lingfs(n);
        }
    }
    if crate::appfs::exists(&n) || crate::appfs::is_dir(&n) {
        return Place::Image(n);
    }
    if crate::appfs::exists(&r) || crate::appfs::is_dir(&r) {
        return Place::Image(r);
    }
    Place::Missing
}

/// `media/<vol>/...` -> `<vol>`
fn vol_of(media_path: &str) -> String {
    media_path.trim_matches('/').strip_prefix("media/").map(|r| String::from(r.split('/').next().unwrap_or(""))).unwrap_or_default()
}

/// Which drive a path is on: "media/<vol>", "system" (lingfs), "image", or "" (nowhere).
pub(crate) fn drive_key(p: &str) -> String {
    match locate(p) {
        Place::Media(c) => format!("media/{}", vol_of(&c)),
        Place::Lingfs(_) => String::from("system"),
        Place::Image(_) => String::from("image"),
        Place::Missing => {
            // a path that does not exist yet: the drive it would be written to
            let n = vfs::norm(p);
            if media::is_media_path(&n) {
                return format!("media/{}", vol_of(&n));
            }
            match vfs::storage_path(&n) {
                Some(c) => format!("media/{}", vol_of(&c)),
                None if vfs::ready() => String::from("system"),
                None => String::new(),
            }
        },
    }
}

/// Space cache: a FAT / exFAT free count is one pass over the allocation table.
static mut SPACE: Vec<(String, u64, (u64, u64))> = Vec::new();

/// (total, free) bytes of a drive key.
pub(crate) fn drive_space(key: &str) -> Option<(u64, u64)> {
    if key == "system" {
        if !vfs::ready() {
            return None;
        }
        let bs = lingfs::BLOCK_SIZE as u64;
        return Some((lingfs::MAX_OBJECTS as u64 * bs, lingfs::free_objects() as u64 * bs));
    }
    if !key.starts_with("media/") {
        return None;
    }
    let now = crate::arch::timer::now_ms();
    let cache = unsafe { &mut *(&raw mut SPACE) };
    if let Some((_, t, v)) = cache.iter().find(|(k, _, _)| k == key) {
        if now.saturating_sub(*t) < 15_000 {
            return Some(*v);
        }
    }
    let v = media::space(key)?;
    cache.retain(|(k, _, _)| k != key);
    cache.push((String::from(key), now, v));
    Some(v)
}

fn space_dirty(key: &str) {
    unsafe { (*(&raw mut SPACE)).retain(|(k, _, _)| k != key) };
}

/// Bytes per allocation unit of a drive (lingfs: its 4 KiB blocks).
fn drive_unit(key: &str) -> u64 {
    if let Some(v) = key.strip_prefix("media/") {
        return media::vol_info(v).map(|i| i.2).unwrap_or(4096).max(1);
    }
    lingfs::BLOCK_SIZE as u64
}

fn on_disk(size: u64, key: &str) -> u64 {
    if key == "system" {
        return if size == 0 { 0 } else { lingfs::objects_for(size as usize) as u64 * lingfs::BLOCK_SIZE as u64 };
    }
    if key == "image" {
        return size;
    }
    let u = drive_unit(key);
    size.div_ceil(u) * u
}

fn drive_label(key: &str) -> String {
    if key == "system" {
        return String::from("System storage (lingfs, inside the LingOS disk)");
    }
    if key == "image" {
        return String::from("The LingOS image (built in, read-only)");
    }
    if let Some(v) = key.strip_prefix("media/") {
        if let Some((label, kind, _)) = media::vol_info(v) {
            return format!("{key} -- \"{label}\" ({kind})");
        }
    }
    String::from(key)
}

fn drive_fs(key: &str) -> String {
    if key == "system" {
        return String::from("lingfs (content-addressed, copy-on-write)");
    }
    if key == "image" {
        return String::from("appfs (read-only, embedded in the kernel)");
    }
    key.strip_prefix("media/").and_then(media::vol_info).map(|i| String::from(i.1)).unwrap_or_default()
}

/// A lingfs entry's kind and metadata (owner, group, mode, the commit that last changed it).
fn lingfs_entry(r: &str) -> Option<(bool, lingfs::Meta)> {
    let name = vfs::basename(r);
    if name.is_empty() {
        return None;
    }
    let (entries, count) = lingfs::lookup_dir(&vfs::parent(r)).ok()??;
    (0..count).find(|&i| lingfs::name_eq(&entries[i].0, &name)).map(|i| (entries[i].2 == lingfs::Kind::Tree, entries[i].3))
}

fn owner_str(b: &[u8; lingfs::OWNER_LEN]) -> String {
    String::from(core::str::from_utf8(lingfs::tree::trim_owner(b)).unwrap_or("?"))
}

fn kind_str(name: &str, is_dir: bool) -> String {
    if is_dir {
        return String::from("Folder");
    }
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => format!("{} file", name[i + 1..].to_ascii_uppercase()),
        _ => String::from("File"),
    }
}

fn size_in_listing(p: &str) -> u64 {
    let name = vfs::basename(p);
    vfs::list(&vfs::parent(p)).into_iter().find(|(n, _, _)| *n == name).map(|(_, _, s)| s as u64).unwrap_or(0)
}

fn push2(out: &mut Vec<u64>, k: &str, v: &str) {
    out.push(mk_str(k));
    out.push(mk_str(v));
}

/// Properties of one path as label / value pairs. A folder's size rows come from the du job.
fn info(p: &str) -> Vec<u64> {
    let mut out = Vec::new();
    let n = vfs::norm(p);
    let name = vfs::basename(&n);
    let is_dir = vfs::is_dir(&n);
    let place = locate(&n);
    let key = drive_key(&n);
    push2(&mut out, "Name", if name.is_empty() { "/" } else { &name });
    push2(&mut out, "Type", &kind_str(&name, is_dir));
    let parent = vfs::parent(&n);
    push2(&mut out, "Location", if parent.is_empty() { "/" } else { &parent });
    match &place {
        Place::Media(c) if *c != n => push2(&mut out, "Stored at", c),
        Place::Lingfs(r) if *r != n => push2(&mut out, "Stored at", r),
        Place::Missing => push2(&mut out, "Stored at", "(not found -- it may have just moved)"),
        _ => {},
    }
    if !is_dir {
        let size = size_in_listing(&n);
        push2(&mut out, "Size", &size_long(size));
        let od = on_disk(size, &key);
        let how = match key.as_str() {
            "system" => " (4 KiB blocks; identical blocks are stored once)",
            "image" => " (inside the kernel image)",
            _ => "",
        };
        push2(&mut out, "Size on disk", &format!("{}{how}", size_long(od)));
    }
    // times and attribute bits
    let mut ro = !crate::modes::writable(&vfs::resolve(&n));
    let mut hidden = name.starts_with('.');
    let mut extra: Vec<&str> = Vec::new();
    match &place {
        Place::Media(c) => {
            let fsn = drive_fs(&key);
            match media::entry_meta(c) {
                Some(m) => {
                    let fat = fsn.starts_with("FAT");
                    let t = |v: u64, what: &str| if v == 0 { format!("not recorded ({fsn} keeps no {what} here)") } else { fmt_stamp(v) };
                    push2(&mut out, "Created", &t(m.created, "creation time"));
                    push2(&mut out, "Modified", &t(m.modified, "modification time"));
                    let acc = if m.accessed == 0 {
                        format!("not recorded ({fsn} keeps no access date here)")
                    } else if fat {
                        let s = fmt_stamp(m.accessed);
                        format!("{} (FAT keeps the date only)", s.split(',').next().unwrap_or(&s))
                    } else {
                        fmt_stamp(m.accessed)
                    };
                    push2(&mut out, "Accessed", &acc);
                    ro |= m.attr & 0x01 != 0;
                    hidden |= m.attr & 0x02 != 0;
                    if m.attr & 0x04 != 0 {
                        extra.push("system");
                    }
                    if m.attr & 0x20 != 0 {
                        extra.push("archive");
                    }
                },
                None => push2(&mut out, "Times", "the volume's root folder has no directory entry"),
            }
        },
        Place::Lingfs(r) => {
            push2(&mut out, "Times", "lingfs keeps no clock times; it numbers its changes");
            if let Some((_, meta)) = lingfs_entry(r) {
                push2(&mut out, "Last change", &format!("change #{} of this volume", meta.modified_seq));
                push2(&mut out, "Owner", &format!("{} (group {})", owner_str(&meta.owner), owner_str(&meta.group)));
                if meta.mode != 0 {
                    push2(&mut out, "lingfs mode", &format!("{:o}", meta.mode));
                }
            }
        },
        Place::Image(_) => {
            push2(&mut out, "Times", "built into this LingOS build");
            ro = true;
        },
        Place::Missing => {},
    }
    let mut attrs: Vec<&str> = Vec::new();
    attrs.push(if ro { "read-only" } else { "writable" });
    if hidden {
        attrs.push("hidden");
    }
    attrs.extend(extra);
    push2(&mut out, "Attributes", &attrs.join(", "));
    if n.starts_with("home/") {
        let mode = crate::modes::get(&vfs::resolve(&n), is_dir);
        push2(&mut out, "Permissions", &format!("{} ({:04o})", crate::modes::symbolic(mode, is_dir), mode));
    }
    drive_rows(&mut out, &key);
    out
}

fn drive_rows(out: &mut Vec<u64>, key: &str) {
    if key.is_empty() {
        return;
    }
    push2(out, "Drive", &drive_label(key));
    push2(out, "File system", &drive_fs(key));
    if key != "image" {
        let u = drive_unit(key);
        push2(out, if key == "system" { "Block size" } else { "Cluster size" }, &size_short(u));
    }
    if let Some((total, free)) = drive_space(key) {
        push2(out, "Free space", &format!("{} free of {} ({}% used)", size_short(free), size_short(total), if total > 0 { 100 - free * 100 / total } else { 0 }));
    }
}

/// Properties of a selection: counts, types, the folder(s) and drive(s).
fn info_many(paths: &[String]) -> Vec<u64> {
    let mut out = Vec::new();
    let (mut files, mut dirs) = (0usize, 0usize);
    let mut kinds: Vec<String> = Vec::new();
    let mut parents: Vec<String> = Vec::new();
    let mut drives: Vec<String> = Vec::new();
    for p in paths {
        let d = vfs::is_dir(p);
        if d {
            dirs += 1;
        } else {
            files += 1;
        }
        let k = kind_str(&vfs::basename(p), d);
        if !kinds.contains(&k) {
            kinds.push(k);
        }
        let par = vfs::parent(p);
        if !parents.contains(&par) {
            parents.push(par);
        }
        let dk = drive_key(p);
        if !drives.contains(&dk) {
            drives.push(dk);
        }
    }
    let w = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    push2(&mut out, "Selected", &format!("{} ({}, {})", w(paths.len(), "item", "items"), w(files, "file", "files"), w(dirs, "folder", "folders")));
    let shown: Vec<&str> = kinds.iter().take(5).map(|s| s.as_str()).collect();
    push2(&mut out, "Types", &format!("{}{}", shown.join(", "), if kinds.len() > 5 { ", ..." } else { "" }));
    if parents.len() == 1 {
        push2(&mut out, "Location", if parents[0].is_empty() { "/" } else { &parents[0] });
    } else {
        push2(&mut out, "Location", &format!("{} folders", parents.len()));
    }
    if drives.len() == 1 {
        drive_rows(&mut out, &drives[0]);
    } else {
        let labels: Vec<String> = drives.iter().map(|d| drive_label(d)).collect();
        push2(&mut out, "Drives", &labels.join("; "));
    }
    out
}

// ── background jobs: one time slice per presented frame ─────────────────────

const SLICE_US: u64 = 5_000;

/// Folder sizes: a walk over everything under the given paths.
struct Du {
    stack: Vec<(String, String)>, // (path, drive key)
    files: u64,
    dirs: u64,
    bytes: u64,
    disk: u64,
    done: bool,
    gen: u32,
}

static mut DU: Option<Du> = None;
static mut DU_GEN: u32 = 0;

fn du_start(paths: &[String]) -> u32 {
    let gen = unsafe {
        DU_GEN = DU_GEN.wrapping_add(1);
        DU_GEN
    };
    let mut d = Du { stack: Vec::new(), files: 0, dirs: 0, bytes: 0, disk: 0, done: false, gen };
    let many = paths.len() > 1;
    for p in paths {
        let key = drive_key(p);
        if vfs::is_dir(p) {
            if many {
                d.dirs += 1;
            }
            d.stack.push((vfs::norm(p), key));
        } else {
            let s = size_in_listing(p);
            d.files += 1;
            d.bytes += s;
            d.disk += on_disk(s, &key);
        }
    }
    unsafe { DU = Some(d) };
    gen
}

fn du_tick(t0: u64) {
    let Some(d) = (unsafe { (*(&raw mut DU)).as_mut() }) else { return };
    while !d.done && now_us().saturating_sub(t0) < SLICE_US {
        let Some((p, key)) = d.stack.pop() else {
            d.done = true;
            break;
        };
        for (name, is_dir, size) in vfs::list(&p) {
            let c = vfs::join(&p, &name);
            if is_dir {
                d.dirs += 1;
                if d.stack.len() < 100_000 {
                    d.stack.push((c, key.clone()));
                }
            } else {
                d.files += 1;
                d.bytes += size as u64;
                d.disk += on_disk(size as u64, &key);
            }
        }
    }
}

/// Permanent deletes (the Trash's Delete / Empty / Burn, Files' Shift+Delete, the cleanup).
enum Del {
    /// a file, or a folder with everything in it
    Tree(String),
    /// a folder whose entries were queued after it: the folder itself, now empty
    Dir(String),
}

struct DelJob {
    stack: Vec<Del>,
    removed: u64,
    failed: u64,
    last_fail: String,
    items: u64,
}

static mut DEL: DelJob = DelJob { stack: Vec::new(), removed: 0, failed: 0, last_fail: String::new(), items: 0 };

fn del() -> &'static mut DelJob {
    unsafe { &mut *(&raw mut DEL) }
}

/// Paths a permanent delete never takes: a drive's root, the home folder and what holds it.
fn deletable(p: &str) -> bool {
    let n = vfs::norm(p);
    let segs = n.split('/').count();
    if n.is_empty() || n == "home" || n == "media" || vfs::is_account_secret(&n) {
        return false;
    }
    if n.starts_with("media/") && segs < 3 {
        return false;
    }
    if n.starts_with("home/") && segs < 3 {
        return false;
    }
    let h = vfs::home();
    !(n == h || h.starts_with(&format!("{n}/")))
}

fn del_queue(p: &str) -> bool {
    if !deletable(p) || !vfs::exists(p) {
        return false;
    }
    let j = del();
    if j.stack.is_empty() {
        j.removed = 0;
        j.failed = 0;
        j.items = 0;
        j.last_fail.clear();
    }
    // LIFO: the first queued goes last -- callers queue an info record before its item
    j.stack.push(Del::Tree(vfs::norm(p)));
    j.items += 1;
    true
}

fn del_tick(t0: u64) {
    let j = del();
    while now_us().saturating_sub(t0) < SLICE_US {
        let Some(step) = j.stack.pop() else { return };
        match step {
            Del::Tree(p) => {
                if vfs::is_dir(&p) {
                    let kids = vfs::list(&p);
                    if !kids.is_empty() {
                        j.stack.push(Del::Dir(p.clone()));
                        for (name, _, _) in kids {
                            j.stack.push(Del::Tree(vfs::join(&p, &name)));
                        }
                        continue;
                    }
                }
                if vfs::delete(&p) || !vfs::exists(&p) {
                    j.removed += 1;
                } else {
                    j.failed += 1;
                    j.last_fail = p;
                }
            },
            Del::Dir(p) => {
                // a dotfile inside (vfs::list hides them) keeps a media folder non-empty
                if p.starts_with("media/") {
                    for (name, _, _) in media::list(&p) {
                        let _ = media::delete_tree(&format!("{p}/{name}"));
                    }
                }
                if vfs::delete(&p) || !vfs::exists(&p) {
                    j.removed += 1;
                } else {
                    j.failed += 1;
                    j.last_fail = p;
                }
            },
        }
        if j.stack.is_empty() {
            unsafe { (*(&raw mut SPACE)).clear() };
            vfs::note_change("");
        }
    }
}

static mut BUSY: bool = false;

/// One slice of every running job (from each presented frame, like the copy job).
pub fn tick() {
    unsafe {
        if BUSY {
            return;
        }
        BUSY = true;
    }
    let t0 = now_us();
    del_tick(t0);
    du_tick(now_us());
    frag_tick(now_us());
    clean_tick(now_us());
    opt_tick(now_us());
    crate::fsx_burn::tick();
    unsafe { BUSY = false };
}

// ── the Trash ────────────────────────────────────────────────────────────────

fn user() -> String {
    let u = lingfs::current_user();
    if u.is_empty() || !vfs::ready() {
        String::from("ling")
    } else {
        String::from(u)
    }
}

/// Every trash folder that may hold something: the home one, and each data volume's.
fn trash_roots() -> Vec<String> {
    let mut out = alloc::vec![vfs::join(&vfs::home(), ".Trash")];
    let u = user();
    for v in media::names() {
        let r = format!("media/{v}/.Trash-{u}");
        // the home trash may itself live on this volume: never list one place twice
        if vfs::storage_path(&out[0]).as_deref() == Some(r.as_str()) {
            continue;
        }
        out.push(r);
    }
    out
}

/// The trash for `p`: (trash folder, true when it is on the same drive -- the move is a rename).
fn trash_root_for(p: &str) -> Option<(String, bool)> {
    let d = drive_key(p);
    if d.is_empty() || d == "image" {
        return None;
    }
    let hd = drive_key(&vfs::home());
    if d == hd {
        return Some((vfs::join(&vfs::home(), ".Trash"), true));
    }
    if d.starts_with("media/") {
        return Some((format!("{d}/.Trash-{}", user()), true));
    }
    Some((vfs::join(&vfs::home(), ".Trash"), false))
}

fn make_dir(d: &str) -> bool {
    vfs::is_dir(d) || if media::is_media_path(d) { media::mkdir_p(d) } else { vfs::mkdir(d) }
}

/// Is `p` inside a trash folder already?
fn in_trash(p: &str) -> bool {
    let n = vfs::norm(p);
    n.split('/').any(|s| s == ".Trash" || s.starts_with(".Trash-"))
}

static mut SEQ: u32 = 0;

/// Move `p` into its drive's trash. Ok(where it went) or Err(why not).
fn trash(p: &str) -> Result<String, String> {
    let n = vfs::norm(p);
    let name = vfs::basename(&n);
    if !deletable(&n) {
        return Err(format!("{} can't be deleted", if name.is_empty() { "/" } else { &name }));
    }
    if in_trash(&n) {
        return Err(format!("{name} is in the Trash already -- delete it there"));
    }
    if !vfs::exists(&n) {
        return Err(format!("{name} is not there any more"));
    }
    let Some((root, same)) = trash_root_for(&n) else {
        return Err(format!("{name} is built into the LingOS image and can't be deleted"));
    };
    if !crate::modes::writable(&vfs::resolve(&n)) {
        return Err(format!("{name} is read-only (chmod)"));
    }
    let (fdir, idir) = (vfs::join(&root, "files"), vfs::join(&root, "info"));
    if !make_dir(&fdir) || !make_dir(&idir) {
        return Err(format!("could not make the Trash folder {root}"));
    }
    let target = crate::drivers::clipboard::unique_path(&vfs::join(&fdir, &name));
    let tname = vfs::basename(&target);
    let now = crate::clock::unix_now();
    // lingfs names stop at 55 bytes: a long name's record gets a short one (File= says whose it is)
    let mut iname = format!("{tname}.trashinfo");
    if iname.len() > 50 || vfs::exists(&vfs::join(&idir, &iname)) {
        let s = unsafe {
            SEQ = SEQ.wrapping_add(1);
            SEQ
        };
        iname = format!("t{now}-{s}.trashinfo");
    }
    let is_dir = vfs::is_dir(&n);
    let size: i64 = if is_dir { -1 } else { size_in_listing(&n) as i64 };
    let rec = format!("[Trash Info]\nPath={n}\nDeletionDate={}\nFile={tname}\nSize={size}\nUnix={now}\nDir={}\n", iso_local(now), is_dir as u8);
    let ipath = vfs::join(&idir, &iname);
    if !vfs::write(&ipath, rec.as_bytes()) {
        return Err(String::from("could not write to the Trash folder (is the drive full or read-only?)"));
    }
    if same && vfs::rename_in_place(&n, &target) {
        space_dirty(&drive_key(&target));
        return Ok(target);
    }
    // another drive (or sealed files): the copy job moves it in the background
    if crate::copyjob::start(&n, &target, true) {
        return Ok(target);
    }
    vfs::delete(&ipath);
    Err(format!("could not move {name} to the Trash (a folder full? lingfs folders hold 32 entries)"))
}

struct Item {
    root: String,
    file: String,
    info: String,
    path: String,
    unix: u64,
    size: i64,
    is_dir: bool,
}

fn field<'a>(rec: &'a str, k: &str) -> Option<&'a str> {
    rec.lines().find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix('='))).map(|v| v.trim_end_matches('\r'))
}

/// Parse "YYYY-MM-DDTHH:MM:SS" (local) back to unix (for records other systems wrote).
fn parse_iso(s: &str) -> u64 {
    let num = |a: usize, b: usize| s.get(a..b).and_then(|x| x.parse::<u32>().ok());
    let (Some(y), Some(m), Some(d)) = (num(0, 4), num(5, 7), num(8, 10)) else { return 0 };
    let (hh, mm, ss) = (num(11, 13).unwrap_or(0), num(14, 16).unwrap_or(0), num(17, 19).unwrap_or(0));
    let t = crate::fs::fat::days_from_civil(y as i64, m, d) * 86400 + (hh * 3600 + mm * 60 + ss) as i64 - tz();
    t.max(0) as u64
}

fn trash_items() -> Vec<Item> {
    let mut out = Vec::new();
    for root in trash_roots() {
        let (fdir, idir) = (vfs::join(&root, "files"), vfs::join(&root, "info"));
        if !vfs::is_dir(&fdir) {
            continue;
        }
        let present = vfs::list(&fdir);
        let mut claimed: Vec<String> = Vec::new();
        for (iname, d, _) in vfs::list(&idir) {
            if d || !iname.ends_with(".trashinfo") {
                continue;
            }
            let Some(bytes) = vfs::read(&vfs::join(&idir, &iname)) else { continue };
            let rec = String::from_utf8_lossy(&bytes);
            let file = field(&rec, "File").map(String::from).unwrap_or_else(|| String::from(iname.trim_end_matches(".trashinfo")));
            let Some((_, is_dir, lsize)) = present.iter().find(|(nm, _, _)| *nm == file) else { continue };
            let unix = field(&rec, "Unix").and_then(|v| v.parse::<u64>().ok()).unwrap_or_else(|| field(&rec, "DeletionDate").map(parse_iso).unwrap_or(0));
            let size = if *is_dir { -1 } else { *lsize as i64 };
            claimed.push(file.clone());
            out.push(Item { root: root.clone(), file, info: iname.clone(), path: String::from(field(&rec, "Path").unwrap_or("")), unix, size, is_dir: *is_dir });
        }
        // something in files/ without a record (another system's, or a crash between the two writes)
        for (nm, d, s) in present {
            if !claimed.contains(&nm) {
                out.push(Item { root: root.clone(), file: nm, info: String::new(), path: String::new(), unix: 0, size: if d { -1 } else { s as i64 }, is_dir: d });
            }
        }
    }
    out.sort_by(|a, b| b.unix.cmp(&a.unix));
    out
}

fn item_id(it: &Item) -> String {
    format!("{}\n{}\n{}", it.root, it.file, it.info)
}

fn item_of(id: &str) -> Option<(String, String, String)> {
    let mut p = id.split('\n');
    Some((String::from(p.next()?), String::from(p.next()?), String::from(p.next().unwrap_or(""))))
}

/// Put a trashed item back where it was ("name (2)" when that name is taken now).
fn restore(id: &str) -> Result<String, String> {
    let (root, file, info) = item_of(id).ok_or("no such item")?;
    let src = vfs::join(&vfs::join(&root, "files"), &file);
    if !vfs::exists(&src) {
        return Err(format!("{file} is not in the Trash any more"));
    }
    let ipath = vfs::join(&vfs::join(&root, "info"), &info);
    let orig = if info.is_empty() { String::new() } else { vfs::read(&ipath).map(|b| String::from(field(&String::from_utf8_lossy(&b), "Path").unwrap_or(""))).unwrap_or_default() };
    let mut dst = if orig.is_empty() { vfs::join(&vfs::home(), &file) } else { orig };
    let parent = vfs::parent(&dst);
    if !parent.is_empty() && !make_dir(&parent) {
        // the folder it came from is gone for good (an unplugged stick): the home folder takes it
        dst = vfs::join(&vfs::home(), &vfs::basename(&dst));
    }
    dst = crate::drivers::clipboard::unique_path(&dst);
    if !vfs::rename_in_place(&src, &dst) && !crate::copyjob::start(&src, &dst, true) {
        return Err(format!("could not move {file} back to {}", vfs::parent(&dst)));
    }
    if !info.is_empty() {
        vfs::delete(&ipath);
    }
    space_dirty(&drive_key(&dst));
    Ok(dst)
}

fn trash_delete(id: &str) -> bool {
    let Some((root, file, info)) = item_of(id) else { return false };
    if !info.is_empty() {
        del_queue(&vfs::join(&vfs::join(&root, "info"), &info));
    }
    del_queue(&vfs::join(&vfs::join(&root, "files"), &file))
}

/// Queue every trashed item for deletion; how many were queued.
fn trash_empty() -> usize {
    let items = trash_items();
    let mut n = 0;
    for it in &items {
        if trash_delete(&item_id(it)) {
            n += 1;
        }
    }
    // stray records whose file is already gone
    for root in trash_roots() {
        let idir = vfs::join(&root, "info");
        for (iname, d, _) in vfs::list(&idir) {
            if !d && !items.iter().any(|it| it.root == root && it.info == iname) {
                del_queue(&vfs::join(&idir, &iname));
            }
        }
    }
    n
}

// ── drives, fragmentation, cleanup, optimize ─────────────────────────────────

fn drive_keys() -> Vec<String> {
    let mut out = Vec::new();
    if vfs::ready() {
        out.push(String::from("system"));
    }
    for v in media::names() {
        out.push(format!("media/{v}"));
    }
    out
}

struct Frag {
    key: String,
    stack: Vec<String>,
    files: u64,
    frag: u64,
    pieces: u64,
    frag_bytes: u64,
    bytes: u64,
    list: Vec<(String, u64, usize)>,
    done: bool,
    note: String,
}

static mut FRAG: Option<Frag> = None;

fn frag_start(key: &str) -> bool {
    let mut f = Frag { key: String::from(key), stack: Vec::new(), files: 0, frag: 0, pieces: 0, frag_bytes: 0, bytes: 0, list: Vec::new(), done: false, note: String::new() };
    if key == "system" {
        match lingfs::object_stats() {
            Some((live, total)) => {
                let bs = lingfs::BLOCK_SIZE as u64;
                f.files = live as u64;
                f.frag = total.saturating_sub(live) as u64;
                f.note = format!(
                    "lingfs stores 4 KiB blocks by their hash, so files are never 'fragmented' in the FAT sense. {} blocks in use, {} hold old versions nothing refers to ({} reclaimable). The check found every block the folders refer to.",
                    live,
                    total.saturating_sub(live),
                    size_short(total.saturating_sub(live) as u64 * bs)
                );
            },
            None => f.note = String::from("lingfs check FAILED: a folder refers to a block that is missing. Nothing was changed."),
        }
        f.done = true;
    } else if key.starts_with("media/") && media::exists(key) {
        f.stack.push(String::from(key));
    } else {
        return false;
    }
    unsafe { FRAG = Some(f) };
    true
}

fn frag_tick(t0: u64) {
    let Some(f) = (unsafe { (*(&raw mut FRAG)).as_mut() }) else { return };
    while !f.done && now_us().saturating_sub(t0) < SLICE_US {
        let Some(dir) = f.stack.pop() else {
            f.done = true;
            let pct = if f.files > 0 { f.frag * 100 / f.files } else { 0 };
            f.note = format!(
                "{} of {} files ({pct}%) are in more than one piece: {} extra pieces, {} of data. A fragmented file reads slower on a hard disk; on flash (USB sticks, SD cards, NVMe) it hardly matters.",
                f.frag,
                f.files,
                f.pieces,
                size_short(f.frag_bytes)
            );
            break;
        };
        for (name, is_dir, size) in media::list(&dir) {
            let p = format!("{dir}/{name}");
            if is_dir {
                f.stack.push(p);
                continue;
            }
            f.files += 1;
            f.bytes += size as u64;
            if let Some(k) = media::pieces(&p) {
                if k > 1 {
                    f.frag += 1;
                    f.pieces += (k - 1) as u64;
                    f.frag_bytes += size as u64;
                    if f.list.len() < 4000 {
                        f.list.push((p, size as u64, k));
                    }
                }
            }
        }
    }
}

/// Cleanup candidates: (path, bytes, why).
struct Clean {
    stack: Vec<(String, u32)>,
    found: Vec<(String, u64, String)>,
    /// a cache folder being summed: index into `found` and its path prefix
    sums: Vec<(usize, String)>,
    done: bool,
    seen: u64,
}

static mut CLEAN: Option<Clean> = None;

fn clean_start() {
    let mut c = Clean { stack: Vec::new(), found: Vec::new(), sums: Vec::new(), done: false, seen: 0 };
    let home = vfs::home();
    // the home folder as stored: a data volume's folder lists dotfiles (unfinished recordings)
    let root = vfs::storage_path(&home).filter(|p| media::is_dir(p)).unwrap_or(home.clone());
    c.stack.push((root, 0));
    if media::has("lingdata") {
        let rec = format!("media/lingdata/home/{}/Videos/screenrec", user());
        if media::is_dir(&rec) {
            c.stack.push((rec, 9));
        }
    }
    unsafe { CLEAN = Some(c) };
}

fn list_any(p: &str) -> Vec<(String, bool, usize)> {
    if media::is_media_path(p) {
        media::list(p)
    } else {
        vfs::list(p)
    }
}

fn clean_rule(dir: &str, name: &str, is_dir: bool, siblings: &[(String, bool, usize)]) -> Option<&'static str> {
    let l = name.to_ascii_lowercase();
    if is_dir {
        let in_app = dir.contains("/Programs/") || dir.ends_with("/Programs") || dir.contains("/Projects/");
        if in_app && (l == "cache" || l == ".cache") {
            return Some("app cache (rebuilt when the app needs it)");
        }
        return None;
    }
    #[cfg(target_arch = "x86_64")]
    let recording = crate::drivers::screenrec::active();
    #[cfg(not(target_arch = "x86_64"))]
    let recording = false;
    if l.starts_with(".screenrec-") && l.ends_with(".part") && !recording {
        return Some("unfinished screen recording (the recorder stopped without finishing it)");
    }
    if l.starts_with(".lingopt-") && l.ends_with(".tmp") {
        return Some("leftover copy from an interrupted optimize");
    }
    if let Some(base) = name.strip_suffix(".tmp") {
        // a database snapshot that never replaced its file: only when the file itself is there
        if !base.is_empty() && siblings.iter().any(|(n, d, _)| !d && n == base) {
            return Some("leftover temporary file (the real one is next to it)");
        }
    }
    if dir.ends_with("/Downloads") && (l.ends_with(".part") || l.ends_with(".crdownload") || l.ends_with(".download")) {
        return Some("unfinished download");
    }
    None
}

fn clean_tick(t0: u64) {
    let Some(c) = (unsafe { (*(&raw mut CLEAN)).as_mut() }) else { return };
    while !c.done && now_us().saturating_sub(t0) < SLICE_US {
        let Some((dir, depth)) = c.stack.pop() else {
            c.done = true;
            break;
        };
        let kids = list_any(&dir);
        for (name, is_dir, size) in &kids {
            c.seen += 1;
            let p = format!("{dir}/{name}");
            // inside a cache folder being summed: just count
            if let Some(&(i, _)) = c.sums.iter().find(|(_, pre)| p.starts_with(pre.as_str())) {
                if *is_dir {
                    c.stack.push((p, depth + 1));
                } else {
                    c.found[i].1 += *size as u64;
                }
                continue;
            }
            if let Some(why) = clean_rule(&dir, name, *is_dir, &kids) {
                c.found.push((p.clone(), if *is_dir { 0 } else { *size as u64 }, String::from(why)));
                if *is_dir {
                    c.sums.push((c.found.len() - 1, format!("{p}/")));
                    c.stack.push((p, depth + 1));
                }
                continue;
            }
            // .Trash is the Trash app's business, the account stores the kernel's
            if *is_dir && depth < 10 && !name.starts_with(".Trash") && c.seen < 60_000 {
                c.stack.push((p, depth + 1));
            }
        }
    }
}

/// Optimize: lingfs compaction at once; FAT / exFAT fragmented files one at a time.
enum OptStage {
    Pick,
    Copy { src: media::MediaRange, out: Option<media::MediaFile>, off: u64 },
    Verify { a: media::MediaRange, b: media::MediaRange, off: u64 },
}

struct Opt {
    list: Vec<(String, u64, usize)>,
    next: usize,
    cur: String,
    tmp: String,
    stage: OptStage,
    improved: u64,
    skipped: u64,
    failed: u64,
    budget: u64,
    done: bool,
    msg: String,
    buf: Vec<u8>,
    buf2: Vec<u8>,
}

static mut OPT: Option<Opt> = None;
const OPT_CHUNK: usize = 256 << 10;
const OPT_MAX_FILE: u64 = 1 << 30;

fn opt_start(key: &str) -> String {
    if crate::copyjob::state() == 1 || !del().stack.is_empty() {
        return String::from("wait for the copy / delete in progress to finish first");
    }
    if key == "system" {
        return match lingfs::compact_now() {
            Ok(true) => String::from("lingfs compacted: the blocks of old versions are free again"),
            Ok(false) => String::from("nothing worth compacting (lingfs compacts by itself once it fills up)"),
            Err(_) => String::from("the compaction could not run; nothing was changed"),
        };
    }
    let list = unsafe { (*(&raw const FRAG)).as_ref() }.filter(|f| f.done && f.key == key).map(|f| f.list.clone());
    let Some(list) = list else { return String::from("analyze the drive first") };
    if list.is_empty() {
        return String::from("no fragmented files on this drive -- nothing to do");
    }
    let free = drive_space(key).map(|s| s.1).unwrap_or(0);
    unsafe {
        OPT = Some(Opt {
            list,
            next: 0,
            cur: String::new(),
            tmp: String::new(),
            stage: OptStage::Pick,
            improved: 0,
            skipped: 0,
            failed: 0,
            budget: free.saturating_sub(64 << 20),
            done: false,
            msg: String::new(),
            buf: alloc::vec![0u8; OPT_CHUNK],
            buf2: alloc::vec![0u8; OPT_CHUNK],
        })
    };
    String::from("optimizing")
}

fn opt_tick(t0: u64) {
    let Some(o) = (unsafe { (*(&raw mut OPT)).as_mut() }) else { return };
    while !o.done && now_us().saturating_sub(t0) < SLICE_US * 2 {
        let stage = core::mem::replace(&mut o.stage, OptStage::Pick);
        match stage {
            OptStage::Pick => {
                let Some((p, size, pieces)) = o.list.get(o.next).cloned() else {
                    o.done = true;
                    o.msg = format!(
                        "done: {} files now in one piece (or fewer pieces), {} could not be improved or were skipped, {} failed safely (the originals are untouched)",
                        o.improved, o.skipped, o.failed
                    );
                    unsafe { (*(&raw mut SPACE)).clear() };
                    break;
                };
                o.next += 1;
                // still there, still in pieces, and room for a second copy?
                if size > OPT_MAX_FILE || size + (8 << 20) > o.budget || media::pieces(&p) != Some(pieces) {
                    o.skipped += 1;
                    continue;
                }
                let dir = &p[..p.rfind('/').unwrap_or(0)];
                o.tmp = format!("{dir}/.lingopt-{}.tmp", o.next);
                let _ = media::delete(&o.tmp);
                let (Some(src), Some(out)) = (media::open_range(&p), media::file_create(&o.tmp)) else {
                    o.failed += 1;
                    continue;
                };
                o.cur = p;
                o.stage = OptStage::Copy { src, out: Some(out), off: 0 };
            },
            OptStage::Copy { src, mut out, off } => {
                let size = src.size();
                let n = core::cmp::min(OPT_CHUNK as u64, size - off) as usize;
                let ok = n == 0 || (src.read_at(off, &mut o.buf[..n]) && out.as_mut().is_some_and(|m| media::file_write(m, &o.buf[..n])));
                if !ok {
                    if let Some(m) = out.take() {
                        media::file_abort(m);
                    }
                    o.failed += 1;
                    continue;
                }
                if off + n as u64 >= size {
                    let fin = out.take().is_some_and(media::file_finish);
                    let before = media::pieces(&o.cur).unwrap_or(1);
                    let after = media::pieces(&o.tmp).unwrap_or(usize::MAX);
                    if !fin || after >= before {
                        // no better: the copy goes, the original stays as it was
                        let _ = media::delete(&o.tmp);
                        if fin {
                            o.skipped += 1;
                        } else {
                            o.failed += 1;
                        }
                        continue;
                    }
                    match (media::open_range(&o.cur), media::open_range(&o.tmp)) {
                        (Some(a), Some(b)) if a.size() == b.size() => o.stage = OptStage::Verify { a, b, off: 0 },
                        _ => {
                            let _ = media::delete(&o.tmp);
                            o.failed += 1;
                        },
                    }
                } else {
                    o.stage = OptStage::Copy { src, out, off: off + n as u64 };
                }
            },
            OptStage::Verify { a, b, off } => {
                let size = a.size();
                let n = core::cmp::min(OPT_CHUNK as u64, size - off) as usize;
                let same = n == 0 || (a.read_at(off, &mut o.buf[..n]) && b.read_at(off, &mut o.buf2[..n]) && o.buf[..n] == o.buf2[..n]);
                if !same {
                    let _ = media::delete(&o.tmp);
                    o.failed += 1;
                    crate::console_write(format!("fsx: optimize: {} copy did not verify; original kept\n", o.cur).as_bytes());
                    continue;
                }
                if off + (n as u64) < size {
                    o.stage = OptStage::Verify { a, b, off: off + n as u64 };
                    continue;
                }
                drop((a, b));
                // the verified copy takes the original's place
                if !media::delete(&o.cur) {
                    let _ = media::delete(&o.tmp);
                    o.failed += 1;
                    continue;
                }
                if media::rename(&o.tmp, &o.cur) {
                    o.improved += 1;
                } else {
                    o.failed += 1;
                    crate::console_write(format!("fsx: optimize: could not rename {} back to {} -- the data is safe in the .tmp file\n", o.tmp, o.cur).as_bytes());
                    o.msg = format!("{} kept its data in {} (rename failed)", o.cur, o.tmp);
                }
                vfs::note_change(&o.cur);
            },
        }
    }
}

// ── builtins ─────────────────────────────────────────────────────────────────

fn strs(v: u64) -> Vec<String> {
    list_items(v).iter().map(|&x| to_string(x)).filter(|s| !s.is_empty()).collect()
}

pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    if name.starts_with(b"fsx_burn") || name == b"fsx_sound" || name == b"fsx_mute" || name == b"fsx_muted" {
        return crate::fsx_burn::dispatch(name, args);
    }
    Some(match name {
        b"fsx_tz" => {
            unsafe { TZ = (narg(args, 0, 0.0) * 3600.0) as i64 };
            UNIT
        },
        b"fsx_date" => mk_str(&fmt_local(narg(args, 0, 0.0).max(0.0) as u64)),
        b"fsx_size" => mk_str(&size_short(narg(args, 0, 0.0).max(0.0) as u64)),
        b"fsx_info" => list_from(info(&sarg(args, 0))),
        b"fsx_info_many" => list_from(info_many(&strs(args.first().copied().unwrap_or(UNIT)))),
        b"fsx_drive" => mk_str(&drive_key(&sarg(args, 0))),
        // the folder-size walk: start over a list of paths, then poll [state, files, folders, bytes, on disk, id]
        b"fsx_du_start" => num(du_start(&strs(args.first().copied().unwrap_or(UNIT))) as f64),
        b"fsx_du" => match unsafe { (*(&raw const DU)).as_ref() } {
            Some(d) => list_from(alloc::vec![num(if d.done { 2.0 } else { 1.0 }), num(d.files as f64), num(d.dirs as f64), num(d.bytes as f64), num(d.disk as f64), num(d.gen as f64)]),
            None => list_from(alloc::vec![num(0.0)]),
        },
        // the Trash
        b"fsx_trash" => mk_str(&match trash(&sarg(args, 0)) {
            Ok(_) => String::new(),
            Err(e) => e,
        }),
        b"fsx_trash_list" => {
            let mut out = Vec::new();
            for it in trash_items() {
                out.push(mk_str(&item_id(&it)));
                out.push(mk_str(&it.file));
                out.push(mk_str(&it.path));
                out.push(num(it.unix as f64));
                out.push(num(it.size as f64));
                out.push(num(it.is_dir as u8 as f64));
                out.push(mk_str(&drive_key(&it.root)));
            }
            list_from(out)
        },
        b"fsx_trash_restore" => mk_str(&match restore(&sarg(args, 0)) {
            Ok(p) => format!("ok:{p}"),
            Err(e) => e,
        }),
        b"fsx_trash_delete" => num(trash_delete(&sarg(args, 0)) as u8 as f64),
        b"fsx_trash_empty" => num(trash_empty() as f64),
        b"fsx_trash_paths" => {
            // the trashed items' own paths (the Trash app sizes their folders with fsx_du_start)
            list_from(trash_items().iter().map(|it| mk_str(&vfs::join(&vfs::join(&it.root, "files"), &it.file))).collect())
        },
        b"fsx_purge" => num(del_queue(&sarg(args, 0)) as u8 as f64),
        // [queued + running (1) or idle (0), removed, failed, items, last failure]
        b"fsx_del" => {
            let j = del();
            list_from(alloc::vec![num(if j.stack.is_empty() { 0.0 } else { 1.0 }), num(j.removed as f64), num(j.failed as f64), num(j.items as f64), mk_str(&j.last_fail)])
        },
        // drives: [key, label, file system, total, free, unit]*
        b"fsx_drives" => {
            let mut out = Vec::new();
            for k in drive_keys() {
                let (total, free) = drive_space(&k).unwrap_or((0, 0));
                let label = if k == "system" { String::from("System storage") } else { k.strip_prefix("media/").and_then(media::vol_info).map(|i| i.0).unwrap_or_default() };
                out.push(mk_str(&k));
                out.push(mk_str(&label));
                out.push(mk_str(&drive_fs(&k)));
                out.push(num(total as f64));
                out.push(num(free as f64));
                out.push(num(drive_unit(&k) as f64));
            }
            list_from(out)
        },
        b"fsx_frag_start" => num(frag_start(&sarg(args, 0)) as u8 as f64),
        // [state 0 none / 1 running / 2 done, files, fragmented, extra pieces, fragmented bytes, key, summary]
        b"fsx_frag" => match unsafe { (*(&raw const FRAG)).as_ref() } {
            Some(f) => list_from(alloc::vec![num(if f.done { 2.0 } else { 1.0 }), num(f.files as f64), num(f.frag as f64), num(f.pieces as f64), num(f.frag_bytes as f64), mk_str(&f.key), mk_str(&f.note)]),
            None => list_from(alloc::vec![num(0.0)]),
        },
        b"fsx_clean_start" => {
            clean_start();
            UNIT
        },
        // [state, entries looked at, then path, bytes, why ...]
        b"fsx_clean" => match unsafe { (*(&raw const CLEAN)).as_ref() } {
            Some(c) => {
                let mut out = alloc::vec![num(if c.done { 2.0 } else { 1.0 }), num(c.seen as f64)];
                for (p, s, why) in &c.found {
                    out.push(mk_str(p));
                    out.push(num(*s as f64));
                    out.push(mk_str(why));
                }
                list_from(out)
            },
            None => list_from(alloc::vec![num(0.0), num(0.0)]),
        },
        // delete the cleanup candidates given (only ones the scan found)
        b"fsx_clean_run" => {
            let want = strs(args.first().copied().unwrap_or(UNIT));
            let mut n = 0;
            if let Some(c) = unsafe { (*(&raw const CLEAN)).as_ref() } {
                for (p, _, _) in &c.found {
                    if want.contains(p) && del_queue(p) {
                        n += 1;
                    }
                }
            }
            num(n as f64)
        },
        b"fsx_opt_start" => mk_str(&opt_start(&sarg(args, 0))),
        // [state, done, of, improved, skipped, failed, message, current file]
        b"fsx_opt" => match unsafe { (*(&raw const OPT)).as_ref() } {
            Some(o) => list_from(alloc::vec![
                num(if o.done { 2.0 } else { 1.0 }),
                num(o.next as f64),
                num(o.list.len() as f64),
                num(o.improved as f64),
                num(o.skipped as f64),
                num(o.failed as f64),
                mk_str(&o.msg),
                mk_str(&o.cur)
            ]),
            None => list_from(alloc::vec![num(0.0)]),
        },
        _ => return None,
    })
}
