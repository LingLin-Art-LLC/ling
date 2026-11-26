//! Files for programs: lingfs (the disk on x86; the SD card on the Pi once it
//! is mounted) in front of the image's read-only `appfs`. Paths are
//! `/`-separated from the filesystem root. A program's relative paths resolve
//! under its own data folder (`set_root`): LingStationX's `projects/…` and
//! `settings.txt` land in `home/<user>/Programs/LingStationX/`, while its
//! bundled assets still come from the image by their embedded names, and a
//! `ling run` script works beside its own file.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

static mut ROOT: Option<String> = None;
/// The running app's folder in the image (`apps/<name>`): its embedded files
/// by their own relative names, beside any unprefixed ones.
static mut IMG: Option<String> = None;
/// Where the running app's `projects/...` paths live (`home/<user>/Projects/<App>`).
static mut PROJECTS: Option<String> = None;

/// A program's whole path scope (data root, image folder, projects folder).
pub struct Scope(Option<String>, Option<String>, Option<String>);

/// Is there writable storage (a mounted lingfs volume)?
pub fn ready() -> bool {
    crate::fs::lingfs::is_mounted()
}

/// The signed-in user's home folder ("ling", the image's built-in home, until
/// storage is mounted and a user signed in).
/// With the SD card's data partition mounted (`media/card`), the files under
/// `home/` live there: `home/ling/Music/x` is `media/card/home/ling/Music/x`.
/// Reads fall back to lingfs (anything stored before the partition existed),
/// listings merge both. Accounts, the keyring and settings stay in lingfs.
fn card_path(r: &str) -> Option<String> {
    if (r == "home" || r.starts_with("home/")) && crate::fs::media::has("card") {
        return Some(format!("media/card/{r}"));
    }
    // an installed x86 system: home/ lives on its "LingOS Data" partition
    if (r == "home" || r.starts_with("home/")) && crate::fs::media::has("data") {
        return Some(format!("media/data/{r}"));
    }
    // Installed packages (`pkg-<name>/...`, the `packages/<name>` version
    // records) live on the package volume when there is one -- lingfs is a
    // few MiB, packages are often 100+ MB. Same fallback as home/: reads
    // still find a package lingfs got before the volume existed.
    if is_package_path(r) {
        if let Some(v) = package_volume() {
            return Some(format!("media/{v}/{r}"));
        }
    }
    None
}

/// Where `p` is really stored when that is a data volume (`media/...`), else None (lingfs).
pub fn storage_path(p: &str) -> Option<String> {
    card_path(&resolve(p))
}

fn is_package_path(r: &str) -> bool {
    r.starts_with("pkg-") || r == "packages" || r.starts_with("packages/")
}

/// The FAT volume packages install to: the Pi's SD-card data partition
/// (`media/card`), else x86's second disk (`media/disk1`); None = lingfs.
pub fn package_volume() -> Option<&'static str> {
    if crate::fs::media::has("card") {
        Some("card")
    } else if crate::fs::media::has("data") {
        // the installed system's "LingOS Data" partition (install-disk)
        Some("data")
    } else if crate::fs::media::has("disk1") {
        Some("disk1")
    } else {
        None
    }
}

pub fn home() -> String {
    let u = crate::fs::lingfs::current_user();
    if u.is_empty() || !ready() {
        String::from("home/ling")
    } else {
        format!("home/{u}")
    }
}

/// The running program's data folder (relative paths resolve under it).
pub fn root() -> String {
    unsafe { (*(&raw const ROOT)).clone() }.unwrap_or_else(home)
}

pub fn set_root(r: &str) {
    unsafe { *(&raw mut ROOT) = if r.is_empty() { None } else { Some(norm(r)) } };
}

/// `apps/<name>` when the image holds that app's files, else nothing.
pub fn set_image_root(r: &str) {
    let r = norm(r);
    unsafe { *(&raw mut IMG) = if !r.is_empty() && crate::appfs::is_dir(&r) { Some(r) } else { None } };
}

pub fn set_projects_root(r: &str) {
    unsafe { *(&raw mut PROJECTS) = if r.is_empty() { None } else { Some(norm(r)) } };
}

/// The projects folder of the running app, if it has one.
pub fn projects_root() -> Option<String> {
    unsafe { (*(&raw const PROJECTS)).clone() }
}

/// Clear the scope for a launched program, returning the caller's to restore.
pub fn take_scope() -> Scope {
    unsafe { Scope((*(&raw mut ROOT)).take(), (*(&raw mut IMG)).take(), (*(&raw mut PROJECTS)).take()) }
}

pub fn restore_scope(s: Scope) {
    unsafe {
        *(&raw mut ROOT) = s.0;
        *(&raw mut IMG) = s.1;
        *(&raw mut PROJECTS) = s.2;
    }
}

/// `n` without the running program's data root (`<root>/x` -> `x`): apps
/// build paths from their `fs_cwd()`, which must still find bundled files.
fn under_root(n: &str) -> Option<String> {
    let r = root();
    if r.is_empty() {
        return None;
    }
    n.strip_prefix(&format!("{r}/")).map(String::from)
}

/// The image path of `n` (or of `n` relative to the data root) inside the
/// running app's folder, when it exists.
fn img(n: &str) -> Option<String> {
    let base = unsafe { (*(&raw const IMG)).as_ref()? };
    let mut cands: Vec<String> = alloc::vec![join(base, n)];
    if let Some(rel) = under_root(n) {
        cands.push(join(base, &rel));
        cands.push(rel);
    }
    cands.into_iter().find(|p| crate::appfs::exists(p) || crate::appfs::is_dir(p))
}

/// `projects/...` of the running app, moved to its Projects folder.
fn projects_path(n: &str) -> Option<String> {
    let base = unsafe { (*(&raw const PROJECTS)).as_ref()? };
    if n == "projects" {
        Some(base.clone())
    } else {
        n.strip_prefix("projects/").map(|rest| join(base, rest))
    }
}

/// A package folder (`pkg-*` at the root) is being changed: the start menus' cached app list
/// (`lingfu::installed_apps`) is redone on its next use.
pub fn note_change(p: &str) {
    // folder listings may be stale now
    unsafe { (*(&raw mut LIST_CACHE)).clear() };
    if norm(p).starts_with("pkg-") {
        crate::drivers::lingfu::apps_changed();
    }
}

/// Strip `./` and leading `/`, collapse `.` and `..` segments, `\` → `/`.
/// The account stores at the top of lingfs -- password hashes, authenticator secrets, security-key
/// and approved-SSH-key records. Only the kernel's own sign-in code reads or writes them: SSH
/// commands, SFTP and the terminal file commands are refused (`cat /twofa/<user>` would hand out
/// the TOTP secret; deleting `seckey/<user>` would switch the key requirement off).
pub fn is_account_secret(p: &str) -> bool {
    let n = norm(p);
    let top = n.split('/').next().unwrap_or("");
    // secrets/ + lingsched/: LingScheduler's sealed keyring secrets, pinned host keys, armed jobs, run logs
    matches!(top, "twofa" | "seckey" | "sshkeys" | "users" | "secrets" | "lingsched")
}

pub fn norm(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split(['/', '\\']) {
        match seg {
            "" | "." => {},
            ".." => {
                parts.pop();
            },
            s => parts.push(s),
        }
    }
    parts.join("/")
}

pub fn join(a: &str, b: &str) -> String {
    if a.is_empty() {
        norm(b)
    } else if b.is_empty() {
        norm(a)
    } else {
        norm(&format!("{a}/{b}"))
    }
}

pub fn parent(p: &str) -> String {
    let n = norm(p);
    match n.rfind('/') {
        Some(i) => String::from(&n[..i]),
        None => String::new(),
    }
}

/// Where a program's path lives: the home tree, packages and any existing
/// lingfs path as given; the image's own files by their embedded name;
/// anything else under the program's data root.
pub fn resolve(p: &str) -> String {
    let n = norm(p);
    // A leading '/' is lingfs-root absolute (the convention lingfs's own
    // "/services" / "/hostname" files use): never re-rooted under the app's
    // folder. The console's `miao` alias leans on this to run the temple's
    // rites in the shell's working directory, not inside the package folder.
    if p.starts_with('/') {
        return n;
    }
    if n.is_empty() {
        return root();
    }
    // `<data root>/projects/...` is the app's Projects folder too (apps build
    // project paths from their `fs_cwd()`).
    if let Some(rel) = under_root(&n) {
        if let Some(p) = projects_path(&rel) {
            return p;
        }
    }
    if n == "home" || n.starts_with("home/") || n.starts_with("pkg-") || n == "packages" || n.starts_with("packages/") {
        return n;
    }
    if let Some(p) = projects_path(&n) {
        return p;
    }
    if ready() && crate::fs::lingfs::path_kind(&n).is_some() {
        return n;
    }
    if crate::appfs::exists(&n) || crate::appfs::is_dir(&n) {
        return n;
    }
    if let Some(p) = img(&n) {
        return p;
    }
    let r = root();
    if r.is_empty() || n == r || n.starts_with(&format!("{r}/")) {
        n
    } else {
        format!("{r}/{n}")
    }
}

fn lingfs_read(path: &str) -> Option<Vec<u8>> {
    use crate::fs::lingfs;
    if lingfs::path_kind(path) != Some(false) {
        return None;
    }
    let size = lingfs::file_size(path)?;
    let mut buf = alloc::vec![0u8; size];
    match lingfs::read_file_all(path, &mut buf) {
        Ok(Some(n)) => {
            buf.truncate(n);
            Some(buf)
        },
        _ => None,
    }
}

/// A file's bytes: the program's copy on storage first, then the image's.
pub fn read(p: &str) -> Option<Vec<u8>> {
    let n = norm(p);
    if crate::fs::media::is_media_path(&n) {
        return crate::fs::media::read(&n);
    }
    let r = resolve(&n);
    if let Some(c) = card_path(&r) {
        if let Some(v) = crate::fs::media::read(&c) {
            return unseal_private(&r, v);
        }
    }
    if ready() {
        if let Some(v) = lingfs_read(&r) {
            return unseal_private(&r, v);
        }
        if r != n {
            if let Some(v) = lingfs_read(&n) {
                return unseal_private(&n, v);
            }
        }
    }
    // x86 has no SD data partition: a home file lingfs (a few MiB) doesn't
    // hold may sit on the FAT data disk under the same path -- an app's big
    // data (FLY for FUN's assets) copied to media/disk1/home/<user>/...
    if r.starts_with("home/") && card_path(&r).is_none() {
        if let Some(v) = package_volume() {
            if let Some(b) = crate::fs::media::read(&format!("media/{v}/{r}")) {
                return Some(b);
            }
        }
    }
    if let Some(b) = crate::appfs::get(&n).or_else(|| crate::appfs::get(&r)).or_else(|| img(&n).and_then(|p| crate::appfs::get(&p))) {
        return Some(b.to_vec());
    }
    app_data_elsewhere(&r).and_then(|c| crate::fs::media::read(&c))
}

// ── an app's big data stored under another folder ───────────────────────────

/// `home/<user>/Programs/<App>/<rest>` -> (`home/<user>/Programs/<App>`, `<App>`, `<rest>`).
fn split_app_data(r: &str) -> Option<(&str, &str, &str)> {
    let after_home = r.strip_prefix("home/")?;
    let u = after_home.find('/')?;
    let after = after_home[u + 1..].strip_prefix("Programs/")?;
    let a = after.find('/')?;
    let (app, rest) = (&after[..a], &after[a + 1..]);
    if u == 0 || app.is_empty() || rest.is_empty() {
        return None;
    }
    Some((&r[.."home/".len() + u + 1 + "Programs/".len() + a], app, rest))
}

/// Where an app's data folder really is when `home/<user>/Programs/<App>` (the data root the
/// program reads its relative paths under) does not hold the file -- the assets were copied for
/// another account (`lingforfun-assets.py --user live` on a PC whose account is `ling`), or to
/// the volume's root as `Programs/<App>`, `<App>` or a checkout's `<App>/client`. Any mounted
/// volume (media/data, media/card, media/disk1, a USB stick) is searched for a folder holding
/// `<rest>`'s first component (`assets`), so a settings-only data root does not count. Answers
/// are remembered (a miss for 10 s: games retry missing models every frame).
fn app_data_elsewhere(r: &str) -> Option<String> {
    let (base, app, rest) = split_app_data(r)?;
    let first = rest.split('/').next().unwrap_or(rest);
    // a single file at the data root (settings.txt) is the app's own, not shared data
    if first == rest {
        return None;
    }
    let key = format!("{base}/{first}");
    static mut SEEN: Vec<(String, Option<String>, u64)> = Vec::new();
    let seen = unsafe { &mut *(&raw mut SEEN) };
    let now = crate::arch::timer::now_ms();
    let alt = match seen.iter().position(|e| e.0 == key) {
        Some(i) if seen[i].1.is_some() || now.wrapping_sub(seen[i].2) < 10_000 => seen[i].1.clone(),
        found => {
            let alt = find_app_data(base, app, first);
            let first_time = found.is_none();
            if let Some(i) = found {
                seen.remove(i);
            }
            if seen.len() >= 16 {
                seen.remove(0);
            }
            match &alt {
                Some(a) => crate::console_write(format!("vfs: {key} is not on storage; {app} reads {a}/{first} instead\n").as_bytes()),
                // (only an app's assets folder is worth a word: games also probe optional folders -- patch/, content/)
                // -- and only when the data root really has none: a game probing one optional file
                // (FLY for FUN's ../assets/raw/Sound/*.wav, then its own copy) is not "missing files"
                None if first_time
                    && first == "assets"
                    && !card_path(base).into_iter().chain(package_volume().map(|v| format!("media/{v}/{base}"))).any(|t| crate::fs::media::is_dir(&format!("{t}/{first}"))) =>
                {
                    let vols: Vec<String> = crate::fs::media::names().iter().map(|v| format!("media/{v}")).collect();
                    let want = card_path(base).unwrap_or_else(|| String::from(base));
                    crate::console_write(
                        format!("vfs: {app}: no assets folder at {want} or on any volume ({}); copy it there\n", if vols.is_empty() { String::from("none mounted") } else { vols.join(", ") }).as_bytes(),
                    );
                    #[cfg(target_arch = "x86_64")]
                    crate::drivers::screenshot::notice(format!("{app}: its game files are missing"), format!("copy its assets folder to {want}/assets"));
                },
                None => {},
            }
            seen.push((key, alt.clone(), now));
            alt
        },
    };
    alt.map(|a| format!("{a}/{rest}"))
}

fn find_app_data(base: &str, app: &str, first: &str) -> Option<String> {
    use crate::fs::media;
    // the places the normal lookup already tried
    let tried: Vec<String> = card_path(base).into_iter().chain(package_volume().map(|v| format!("media/{v}/{base}"))).collect();
    let holds = |c: &String| !tried.iter().any(|t| t.eq_ignore_ascii_case(c)) && media::is_dir(&format!("{c}/{first}"));
    for vol in media::names() {
        let v = format!("media/{vol}");
        let mut cands: Vec<String> = Vec::new();
        for (user, d, _) in media::list(&format!("{v}/home")) {
            if d {
                cands.push(format!("{v}/home/{user}/Programs/{app}"));
            }
        }
        cands.push(format!("{v}/Programs/{app}"));
        cands.push(format!("{v}/{app}"));
        cands.push(format!("{v}/{app}/client"));
        if let Some(c) = cands.into_iter().find(|c| holds(c)) {
            return Some(c);
        }
    }
    None
}

/// Write a file (creating its folders); false without storage, or when `chmod` made it read-only.
pub fn write(p: &str, data: &[u8]) -> bool {
    crate::hitch::timed(b"file write", || write_untimed(p, data))
}

fn write_untimed(p: &str, data: &[u8]) -> bool {
    note_change(p);
    if crate::fs::media::is_media_path(&norm(p)) {
        return crate::fs::media::write(&norm(p), data);
    }
    let r = resolve(p);
    if !crate::modes::writable(&r) {
        return false;
    }
    if let Some(c) = card_path(&r) {
        if let Some(i) = c.rfind('/') {
            crate::fs::media::mkdir_p(&c[..i]);
        }
        if crate::keyring::is_private_path(&r) {
            let Some(blob) = crate::keyring::seal("vfs-private", r.as_bytes(), data) else { return false };
            return crate::fs::media::write(&c, &blob);
        }
        let ok = crate::fs::media::write(&c, data);
        if ok {
            recent_auto(&r);
        }
        return ok;
    }
    if !ready() {
        return false;
    }
    if r.is_empty() {
        return false;
    }
    if let Some(i) = r.rfind('/') {
        let _ = crate::fs::lingfs::mkdir_p(&r[..i]);
    }
    if crate::keyring::is_private_path(&r) {
        // sealed under the signed-in user's keyring; refused while locked
        let Some(blob) = crate::keyring::seal("vfs-private", r.as_bytes(), data) else { return false };
        return crate::fs::lingfs::write_path_any(&r, &blob).is_ok();
    }
    let ok = crate::fs::lingfs::write_path_any(&r, data).is_ok();
    if ok {
        recent_auto(&r);
    }
    ok
}

// ── Recent files ────────────────────────────────────────────────────────────
// The Files window's "Recent" folder: paths the signed-in user opened, saved
// or changed, newest first, deduplicated, at most RECENT_MAX. Kept one per
// line in `home/<user>/.recent` (a dot-file, so it never lists itself). Every
// write into the home tree lands here (apps' saves included) except the apps'
// own settings and caches under `Programs/`; Files adds what it opens.

const RECENT_MAX: usize = 50;
/// >0 while a folder copy / move writes its many files (only the top is noted).
static mut RECENT_QUIET: u32 = 0;

fn recent_file() -> Option<String> {
    let u = crate::fs::lingfs::current_user();
    if u.is_empty() || !ready() {
        None
    } else {
        Some(format!("home/{u}/.recent"))
    }
}

/// The stored list, newest first (entries may no longer exist).
pub fn recent_list() -> Vec<String> {
    let Some(f) = recent_file() else { return Vec::new() };
    match read(&f) {
        Some(b) => String::from_utf8_lossy(&b).lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect(),
        None => Vec::new(),
    }
}

fn recent_store(l: &[String]) {
    if let Some(f) = recent_file() {
        let mut s = l.join("\n");
        s.push('\n');
        let _ = write(&f, s.as_bytes());
    }
}

/// `p` was opened or changed: to the top of the list.
pub fn recent_note(p: &str) {
    let n = norm(p);
    if n.is_empty() || n.split('/').any(|s| s.starts_with('.')) || unsafe { RECENT_QUIET } > 0 {
        return;
    }
    let mut l = recent_list();
    if l.first() == Some(&n) {
        return; // the common case (an app saving again): no write
    }
    l.retain(|x| *x != n);
    l.insert(0, n);
    l.truncate(RECENT_MAX);
    recent_store(&l);
}

/// A write landed at `r` (resolved): user files in the home tree count.
fn recent_auto(r: &str) {
    let u = crate::fs::lingfs::current_user();
    if u.is_empty() {
        return;
    }
    let Some(rest) = r.strip_prefix(&format!("home/{u}/")) else { return };
    if rest.starts_with("Programs/") {
        return;
    }
    recent_note(r);
}

/// Take `p` (and anything under it) off the list.
pub fn recent_forget(p: &str) {
    let n = norm(p);
    let mut l = recent_list();
    let before = l.len();
    l.retain(|x| *x != n && !x.starts_with(&format!("{n}/")));
    if l.len() != before {
        recent_store(&l);
    }
}

/// `a` moved to `b`: entries follow it.
fn recent_moved(a: &str, b: &str) {
    let mut l = recent_list();
    let mut changed = false;
    for x in l.iter_mut() {
        if *x == a {
            *x = String::from(b);
            changed = true;
        } else if let Some(rest) = x.strip_prefix(&format!("{a}/")) {
            *x = format!("{b}/{rest}");
            changed = true;
        }
    }
    if changed {
        recent_store(&l);
    }
}

/// A private file's bytes come back decrypted (None while the keyring is
/// locked or on a wrong key); a file written there before the keyring
/// existed is plain and passes through.
fn unseal_private(path: &str, bytes: Vec<u8>) -> Option<Vec<u8>> {
    if !crate::keyring::is_private_path(path) || !crate::keyring::is_sealed(&bytes) {
        return Some(bytes);
    }
    crate::keyring::open("vfs-private", path.as_bytes(), &bytes)
}

pub fn exists(p: &str) -> bool {
    let n = norm(p);
    if crate::fs::media::is_media_path(&n) {
        return crate::fs::media::exists(&n);
    }
    let r = resolve(&n);
    if card_path(&r).is_some_and(|c| crate::fs::media::exists(&c)) {
        return true;
    }
    if ready() && (crate::fs::lingfs::path_kind(&r).is_some() || crate::fs::lingfs::path_kind(&n).is_some()) {
        return true;
    }
    crate::appfs::exists(&n)
        || crate::appfs::is_dir(&n)
        || crate::appfs::exists(&r)
        || img(&n).is_some()
        || (is_model_path(&n) && locate_model(&n).is_some())
        || app_data_elsewhere(&r).is_some_and(|c| crate::fs::media::exists(&c))
}

pub fn is_dir(p: &str) -> bool {
    let n = norm(p);
    if n.is_empty() {
        return true;
    }
    if crate::fs::media::is_media_path(&n) {
        return crate::fs::media::is_dir(&n);
    }
    let r = resolve(&n);
    if card_path(&r).is_some_and(|c| crate::fs::media::is_dir(&c)) {
        return true;
    }
    if ready() && (crate::fs::lingfs::path_kind(&r) == Some(true) || crate::fs::lingfs::path_kind(&n) == Some(true)) {
        return true;
    }
    crate::appfs::is_dir(&n) || crate::appfs::is_dir(&r) || img(&n).is_some_and(|p| crate::appfs::is_dir(&p))
}

pub fn mkdir(p: &str) -> bool {
    note_change(p);
    if crate::fs::media::is_media_path(&norm(p)) {
        return crate::fs::media::mkdir(&norm(p));
    }
    if let Some(c) = card_path(&resolve(p)) {
        return crate::fs::media::mkdir_p(&c);
    }
    ready() && crate::fs::lingfs::mkdir_p(&resolve(p)).is_ok()
}

/// Delete a file or an empty folder, on removable media or the SD card's
/// lingfs (the image's built-in files stay).
pub fn delete(p: &str) -> bool {
    note_change(p);
    let n = norm(p);
    if crate::fs::media::is_media_path(&n) {
        return crate::fs::media::delete(&n);
    }
    if !crate::modes::writable(&resolve(&n)) {
        return false; // chmod made it read-only
    }
    if is_dir(&n) && !list(&n).is_empty() {
        return false;
    }
    let gone = delete_storage(&n);
    if gone {
        crate::modes::forget(&resolve(&n));
    }
    gone
}

fn delete_storage(n: &str) -> bool {
    if let Some(c) = card_path(&resolve(n)) {
        if crate::fs::media::exists(&c) {
            return crate::fs::media::delete(&c);
        }
    }
    if !ready() {
        return false;
    }
    let r = resolve(n);
    if crate::fs::lingfs::remove_path(&r).unwrap_or(false) {
        return true;
    }
    r != n && crate::fs::lingfs::remove_path(n).unwrap_or(false)
}

pub fn basename(p: &str) -> String {
    let n = norm(p);
    match n.rfind('/') {
        Some(i) => String::from(&n[i + 1..]),
        None => n,
    }
}

/// Where `cp`/`mv` put `src` for a `dst` argument: inside `dst` when it is an
/// existing folder, else at `dst` itself.
pub fn target_for(src: &str, dst: &str) -> String {
    if is_dir(dst) && !norm(dst).is_empty() || norm(dst).is_empty() {
        join(dst, &basename(src))
    } else {
        norm(dst)
    }
}

/// Copy a file, or a folder with everything in it.
pub fn copy(from: &str, to: &str) -> bool {
    let (a, b) = (norm(from), norm(to));
    if a == b {
        return true;
    }
    if is_dir(&a) {
        if !is_dir(&b) && !mkdir(&b) {
            return false;
        }
        let mut ok = true;
        // a folder's files don't each go to Recent
        unsafe { RECENT_QUIET += 1 };
        for (name, _, _) in list(&a) {
            ok &= copy(&join(&a, &name), &join(&b, &name));
        }
        unsafe { RECENT_QUIET -= 1 };
        return ok;
    }
    match read(&a) {
        Some(data) => write(&b, &data),
        None => false,
    }
}

/// Is `r` (resolved) sealed by the keyring, or a folder holding sealed files? Those are sealed
/// under their own path, so they never move by relinking (the bytes must be re-sealed).
fn holds_private(r: &str) -> bool {
    if crate::keyring::is_private_path(r) {
        return true;
    }
    let u = crate::fs::lingfs::current_user();
    if u.is_empty() {
        return false;
    }
    let pre = format!("{r}/");
    [format!("home/{u}/Private"), format!("home/{u}/Programs/Horizon"), format!("home/{u}/.messenger")]
        .iter()
        .any(|p| r.is_empty() || p.starts_with(&pre) || p == r)
}

/// A move that only relinks: inside one removable / data volume (home/ on the data partition
/// counts), or inside lingfs. False when it has to be copied (across volumes, sealed files,
/// something in the way) -- nothing is changed then. Never reads or writes file contents, so
/// it is instant whatever the size.
pub fn rename_fast(from: &str, to: &str) -> bool {
    let (a, b) = (norm(from), norm(to));
    if a.is_empty() || b.is_empty() || a == b || b.starts_with(&format!("{a}/")) {
        return false;
    }
    let media = crate::fs::media::is_media_path;
    if media(&a) && media(&b) {
        return crate::fs::media::rename(&a, &b);
    }
    let (ra, rb) = (resolve(&a), resolve(&b));
    if holds_private(&ra) || holds_private(&rb) {
        return false;
    }
    let sa = if media(&a) { Some(a.clone()) } else { card_path(&ra).filter(|c| crate::fs::media::exists(c)) };
    let sb = if media(&b) { Some(b.clone()) } else { card_path(&rb) };
    if let (Some(sa), Some(sb)) = (&sa, &sb) {
        if crate::fs::media::exists(sb) {
            return false;
        }
        let p = parent(sb);
        if !p.is_empty() && !crate::fs::media::is_dir(&p) {
            crate::fs::media::mkdir_p(&p);
        }
        return crate::fs::media::rename(sa, sb);
    }
    // both in lingfs (and nothing of `a` on a data volume or in the image)
    if sa.is_none() && sb.is_none() && !media(&a) && !media(&b) && ready() && crate::fs::lingfs::path_kind(&ra).is_some() {
        if crate::appfs::exists(&ra) || crate::appfs::is_dir(&ra) {
            return false;
        }
        return crate::fs::lingfs::move_path(&ra, &rb).is_ok();
    }
    false
}

/// [`rename`]'s instant half: a relinking move ([`rename_fast`]) with the mode and Recent
/// entries following it. False (nothing changed) when the move needs a copy.
pub fn rename_in_place(from: &str, to: &str) -> bool {
    note_change(from);
    note_change(to);
    let (a, b) = (norm(from), norm(to));
    let mode_from = resolve(&a);
    if !crate::modes::writable(&mode_from) || !rename_fast(&a, &b) {
        return false;
    }
    crate::modes::moved(&mode_from, &resolve(&b));
    recent_moved(&a, &b);
    true
}

/// Move / rename: in place inside one removable volume, else copy + delete. A file or folder keeps
/// its `chmod` mode; a read-only one is not moved (the delete half would be refused).
pub fn rename(from: &str, to: &str) -> bool {
    let (a, b) = (norm(from), norm(to));
    if !crate::modes::writable(&resolve(&a)) {
        return false;
    }
    if rename_in_place(&a, &b) {
        return true;
    }
    let mode_from = resolve(&a);
    // a move is not an edit: the copy half's writes don't count for Recent
    unsafe { RECENT_QUIET += 1 };
    let copied = copy(&a, &b);
    unsafe { RECENT_QUIET -= 1 };
    if !copied {
        return false;
    }
    // the modes follow the files before the delete half forgets the old path
    crate::modes::moved(&mode_from, &resolve(&b));
    recent_moved(&a, &b);
    delete_tree(&a)
}

/// Delete a file or a folder with everything in it (each part still honours read-only modes).
pub fn delete_tree(p: &str) -> bool {
    if is_dir(p) {
        for (name, _, _) in list(p) {
            delete_tree(&join(p, &name));
        }
    }
    delete(p)
}

fn push_entry(items: &mut Vec<(String, bool, usize)>, name: &str, is_dir: bool, size: usize) {
    if name.is_empty() || name.starts_with('.') || items.iter().any(|(n, _, _)| n == name) {
        return;
    }
    items.push((String::from(name), is_dir, size));
}

fn lingfs_list_into(dir: &str, items: &mut Vec<(String, bool, usize)>) {
    use crate::fs::lingfs;
    let mut i = 0usize;
    let mut nb = [0u8; 96];
    while let Some((len, is_dir)) = lingfs::list_entry(dir, i, &mut nb) {
        i += 1;
        if len == 0 {
            continue;
        }
        let Ok(name) = core::str::from_utf8(&nb[..len.min(nb.len())]) else { continue };
        let name = String::from(name);
        let size = if is_dir { 0 } else { lingfs::file_size(&join(dir, &name)).unwrap_or(0) };
        push_entry(items, &name, is_dir, size);
        if i > 4096 {
            break;
        }
    }
}

/// A folder's entries `(name, is_dir, size)`: storage and image merged, folders
/// first, names case-insensitive; dotfiles hidden.
/// Recent folder listings: (path, entries, when). A listing walks lingfs (several disk reads), the
/// data volume and the image; a game that lists a folder every few frames (FLY for FUN: 81 ms a call
/// on the Z590, every 0.7 s) held its frames on it. Any change through this module clears it; changes
/// made around it show after [`LIST_CACHE_MS`].
static mut LIST_CACHE: Vec<(String, Vec<(String, bool, usize)>, u64)> = Vec::new();
const LIST_CACHE_MS: u64 = 2_000;

pub fn list(p: &str) -> Vec<(String, bool, usize)> {
    let now = crate::arch::timer::now_ms();
    let cache = unsafe { &mut *(&raw mut LIST_CACHE) };
    cache.retain(|(_, _, t)| now.saturating_sub(*t) < LIST_CACHE_MS);
    if let Some((_, v, _)) = cache.iter().find(|(k, _, _)| k == p) {
        return v.clone();
    }
    let v = crate::hitch::timed(b"folder listing", || list_uncached(p));
    let cache = unsafe { &mut *(&raw mut LIST_CACHE) };
    if cache.len() >= 32 {
        cache.remove(0);
    }
    cache.push((String::from(p), v.clone(), now));
    v
}

fn list_uncached(p: &str) -> Vec<(String, bool, usize)> {
    let n = norm(p);
    if crate::fs::media::is_media_path(&n) {
        return crate::fs::media::list(&n);
    }
    let r = resolve(&n);
    let mut items: Vec<(String, bool, usize)> = Vec::new();
    if n.is_empty() && !crate::fs::media::names().is_empty() {
        items.push((String::from("media"), true, 0));
    }
    if let Some(c) = card_path(&r) {
        for (name, d, size) in crate::fs::media::list(&c) {
            push_entry(&mut items, &name, d, size);
        }
    }
    // The root also shows the packages installed on the package volume.
    if n.is_empty() {
        if let Some(v) = package_volume() {
            for (name, d, size) in crate::fs::media::list(&format!("media/{v}")) {
                if is_package_path(&name) {
                    push_entry(&mut items, &name, d, size);
                }
            }
        }
    }
    if ready() {
        lingfs_list_into(&r, &mut items);
        if r != n {
            lingfs_list_into(&n, &mut items);
        }
    }
    let mut dirs: Vec<String> = alloc::vec![n.clone()];
    if r != n {
        dirs.push(r.clone());
    }
    if let Some(p) = img(&n) {
        if !dirs.contains(&p) {
            dirs.push(p);
        }
    }
    for dir in &dirs {
        let base = dir.clone();
        crate::appfs::list(dir, |name, is_dir| {
            let size = if is_dir { 0 } else { crate::appfs::get(&join(&base, name)).map(|d| d.len()).unwrap_or(0) };
            push_entry(&mut items, name, is_dir, size);
        });
    }
    items.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.to_lowercase().cmp(&y.0.to_lowercase())));
    items
}

// ── big files read in pieces (language models) ─────────────────────────────

/// Is `n` a model path the search below applies to: a relative path with a
/// `models` folder in it (`assets/models/Qwen3-0.6B-Q8_0.gguf`, `models/x`)?
fn is_model_path(n: &str) -> bool {
    !crate::fs::media::is_media_path(n) && !n.starts_with("home/") && n.split('/').any(|s| s.eq_ignore_ascii_case("models"))
}

/// Where a model file an app names by a relative path actually is, on a FAT
/// volume (`media/<vol>/...`). Models are hundreds of MB to GBs, so they live
/// on the SD card's data partition (Pi) or the second disk (x86), never in
/// lingfs or the image. The search, first hit wins:
///   1. the path as programs resolve it: the app's data folder
///      (`home/<user>/Programs/<App>/assets/models/...`, on the card on the Pi)
///   2. an installed package that carries it: `media/<vol>/pkg-*/<path>`
///   3. the volume's root: `media/<vol>/<path>` (`media/card/assets/models/...`)
///   4. a plain models folder: `media/<vol>/models/<file name>`
/// over every mounted volume (card, disk1, usb0, ...). An absolute
/// `media/...` or `home/...` path (Settings > AI > Browse) is taken as is.
pub fn locate_model(p: &str) -> Option<String> {
    // apps ask every frame (LingStationX's Settings > AI looks for its three models): remember answers for 3 s, so a search over
    // the volumes' folders is not a few hundred SD-card sector reads a frame
    static mut SEEN: Vec<(String, Option<String>, u64)> = Vec::new();
    let now = crate::arch::timer::now_ms();
    let seen = unsafe { &mut *(&raw mut SEEN) };
    seen.retain(|e| now.wrapping_sub(e.2) < 3000);
    if let Some(e) = seen.iter().find(|e| e.0 == p) {
        return e.1.clone();
    }
    let found = locate_model_now(p);
    if seen.len() >= 32 {
        seen.remove(0);
    }
    seen.push((String::from(p), found.clone(), now));
    found
}

fn locate_model_now(p: &str) -> Option<String> {
    let n = norm(p);
    if crate::fs::media::is_media_path(&n) {
        return crate::fs::media::exists(&n).then_some(n);
    }
    let r = resolve(&n);
    if let Some(c) = card_path(&r) {
        if crate::fs::media::exists(&c) && !crate::fs::media::is_dir(&c) {
            return Some(c);
        }
    }
    let rel = under_root(&n).unwrap_or(n);
    if !is_model_path(&rel) {
        return None;
    }
    let base = basename(&rel);
    for vol in crate::fs::media::names() {
        let v = format!("media/{vol}");
        for (name, d, _) in crate::fs::media::list(&v) {
            if d && name.starts_with("pkg-") {
                let c = format!("{v}/{name}/{rel}");
                if crate::fs::media::exists(&c) {
                    return Some(c);
                }
            }
        }
        for c in [format!("{v}/{rel}"), format!("{v}/models/{base}")] {
            if crate::fs::media::exists(&c) && !crate::fs::media::is_dir(&c) {
                return Some(c);
            }
        }
    }
    None
}

/// `fsbench <dir> [times]` (SSH): what one uncached folder listing of `dir` costs, by part --
/// the data volume, lingfs (merged into every listing), the image -- and `exists` / `is_dir`.
/// `fsbench exfat|lingfs on|off` switches the exFAT folder cache / lingfs object cache.
pub fn bench(arg: &str) -> String {
    use core::sync::atomic::Ordering::Relaxed;
    let w: Vec<&str> = arg.split_whitespace().collect();
    match (w.first().copied(), w.get(1).copied()) {
        (Some("exfat"), Some(v)) => crate::fs::exfat::DCACHE_ENABLED.store(v == "on", Relaxed),
        (Some("lingfs"), Some(v)) => crate::fs::lingfs::objects::OCACHE_ON.store(v == "on", Relaxed),
        _ => {},
    }
    let caches = format!(
        "exfat folder cache {}, lingfs object cache {}\n",
        if crate::fs::exfat::DCACHE_ENABLED.load(Relaxed) { "on" } else { "off" },
        if crate::fs::lingfs::objects::OCACHE_ON.load(Relaxed) { "on" } else { "off" }
    );
    let Some(p) = w.first().copied().filter(|f| *f != "exfat" && *f != "lingfs") else { return caches };
    let times: u64 = w.get(1).and_then(|t| t.parse().ok()).unwrap_or(20).clamp(1, 1000);
    let now = crate::arch::timer::now_us;
    let n = norm(p);
    let r = resolve(&n);
    let (mut t_media, mut t_lingfs, mut t_img, mut t_all, mut t_exists, mut t_isdir) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut count = 0;
    for _ in 0..times {
        let t = now();
        if let Some(c) = card_path(&r) {
            count = crate::fs::media::list(&c).len();
        }
        let t1 = now();
        let mut items = Vec::new();
        if ready() {
            lingfs_list_into(&r, &mut items);
            if r != n {
                lingfs_list_into(&n, &mut items);
            }
        }
        let t2 = now();
        let _ = img(&n);
        crate::appfs::list(&n, |_, _| {});
        let t3 = now();
        let _ = list_uncached(&n);
        let t4 = now();
        let _ = exists(&n);
        let t5 = now();
        let _ = is_dir(&n);
        let t6 = now();
        t_media += t1 - t;
        t_lingfs += t2 - t1;
        t_img += t3 - t2;
        t_all += t4 - t3;
        t_exists += t5 - t4;
        t_isdir += t6 - t5;
    }
    let us = |x: u64| format!("{}.{:02} ms", x / times / 1000, x / times % 1000 / 10);
    format!(
        "{caches}{p} -> {} ({} entries on the volume), per call over {times}:\n  list total {}\n    data volume {}\n    lingfs {}\n    image {}\n  exists {}\n  is_dir {}\n",
        card_path(&r).unwrap_or_else(|| r.clone()),
        count,
        us(t_all),
        us(t_media),
        us(t_lingfs),
        us(t_img),
        us(t_exists),
        us(t_isdir)
    )
}
