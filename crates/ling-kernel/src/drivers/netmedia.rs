//! Web pictures and network video for the in-kernel Horizon browser (and any
//! Ling app that uses the host's picture-handle API).
//!
//! **Pictures** -- `image_fetch(url)` -> job, `image_job(job)` -> -2 while it
//! runs, -1 when it failed, else a picture id for `image_draw` /
//! `image_width` (compat.rs keeps the pixels). One worker green thread fetches
//! the queue in order (HTTPS through `tls::https_get_stream`, plain HTTP over
//! a socket handle, `data:` URLs inline), follows redirects, and decodes PNG,
//! JPEG (baseline + progressive, zune-jpeg), BMP and SVG. Also `image_resized`,
//! `image_tint`, `image_svg` and `image_fetch_cancel`, the rest of what
//! Horizon's page drawing asks for. Before this the kernel answered every
//! `image_fetch` with -1, so no web picture (YouTube thumbnails included)
//! ever showed in Horizon on LingOS.
//!
//! **Video** -- `os_video_*` is played by `videoplay.rs`, entirely on this
//! machine: YouTube's stream addresses from its InnerTube API (`youtube.rs`),
//! fetched over the kernel's HTTPS, demuxed by ling-mp4 and decoded by
//! ling-h264 / ling-aac. This file only routes those builtins there.

use crate::compat;
use crate::drivers::{image, netstack as ns};
use crate::lingrt::{num, to_f64, to_string, UNIT};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

fn now_ms() -> u64 {
    crate::arch::timer::now_ms()
}

/// Waiting on the network inside a green thread: let the app and the desktop run.
fn idle() {
    if crate::coro::in_coro() {
        crate::coro::yield_now();
    } else {
        core::hint::spin_loop();
    }
}

fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut ip = [0u8; 4];
    let mut n = 0;
    for part in s.trim().split('.') {
        if n >= 4 {
            return None;
        }
        ip[n] = part.parse::<u8>().ok()?;
        n += 1;
    }
    (n == 4).then_some(ip)
}

pub(crate) fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// A header's value from a raw header block (case-insensitive name).
pub(crate) fn header(head: &[u8], name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(head);
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// An HTTP/1.1 chunked body put back together.
pub(crate) fn dechunk(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let Some(e) = find(&b[i..], b"\r\n") else { break };
        let line = core::str::from_utf8(&b[i..i + e]).unwrap_or("");
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        i += e + 2;
        if size == 0 || i + size > b.len() {
            if size > 0 {
                out.extend_from_slice(&b[i..]);
            }
            break;
        }
        out.extend_from_slice(&b[i..i + size]);
        i += size + 2;
    }
    out
}

/// One plain HTTP/1.0 GET over a socket handle (the net lock is taken only per
/// segment, so other fetches keep flowing). (status, headers, body).
fn http_plain(host: &str, port: u16, path: &str, connect_ms: u64, wait_ms: u64, max: usize) -> Result<(u16, Vec<u8>, Vec<u8>), String> {
    let ip = parse_ip(host).or_else(|| ns::dns_resolve(host)).ok_or_else(|| format!("{host}: unknown host"))?;
    let h = ns::tcp_connect_begin_h(ip, port).ok_or_else(|| "no free network socket".to_string())?;
    let t0 = now_ms();
    loop {
        ns::pump_once();
        if ns::tcp_established_h(h) {
            break;
        }
        if ns::tcp_closed_h(h) || now_ms() - t0 > connect_ms {
            ns::tcp_free_h(h);
            return Err(format!("no answer from {host}:{port}"));
        }
        idle();
    }
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}:{port}\r\nUser-Agent: LingOS-Horizon\r\nAccept: */*\r\nConnection: close\r\n\r\n");
    for part in req.as_bytes().chunks(1200) {
        if !ns::tcp_write_h(h, part) {
            ns::tcp_close_h(h);
            return Err(format!("{host}:{port} stopped taking the request"));
        }
    }
    let mut resp: Vec<u8> = Vec::new();
    let mut buf = alloc::vec![0u8; 16384];
    let start = now_ms();
    let mut last = start;
    let mut want: Option<usize> = None;
    loop {
        ns::pump_once();
        let n = ns::tcp_read_h(h, &mut buf, 0);
        if n > 0 {
            resp.extend_from_slice(&buf[..n]);
            last = now_ms();
            if want.is_none() {
                if let Some(he) = find(&resp, b"\r\n\r\n") {
                    if let Some(cl) = header(&resp[..he], "content-length").and_then(|v| v.parse::<usize>().ok()) {
                        want = Some(he + 4 + cl);
                    }
                }
            }
            if want.is_some_and(|w| resp.len() >= w) {
                break;
            }
            if resp.len() > max {
                ns::tcp_close_h(h);
                return Err("too big".to_string());
            }
            continue;
        }
        if ns::tcp_closed_h(h) {
            break;
        }
        let now = now_ms();
        if (resp.is_empty() && now - start > wait_ms) || (!resp.is_empty() && now - last > 15_000) {
            ns::tcp_close_h(h);
            return Err(format!("{host}:{port} took too long to answer"));
        }
        idle();
    }
    if ns::tcp_closed_h(h) {
        ns::tcp_free_h(h);
    } else {
        ns::tcp_close_h(h);
    }
    let he = find(&resp, b"\r\n\r\n").ok_or_else(|| format!("{host}:{port} sent no HTTP answer"))?;
    let status = core::str::from_utf8(&resp[..he]).ok().and_then(|s| s.split(' ').nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    let head = resp[..he].to_vec();
    let mut body = resp.split_off(he + 4);
    if let Some(w) = want {
        body.truncate(w - he - 4);
    }
    if header(&head, "transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        body = dechunk(&body);
    }
    Ok((status, head, body))
}

// ── pictures ─────────────────────────────────────────────────────────────────

/// Largest picture body taken (bytes).
const IMG_MAX_BYTES: usize = 12 << 20;

#[derive(Clone, Copy, PartialEq)]
enum JobState {
    Queued,
    Running,
    Done(f64),
}

struct ImgJob {
    id: u32,
    url: String,
    state: JobState,
    /// the worker running it (a stopped worker's job is queued again)
    worker: u64,
}

static mut JOBS: Vec<ImgJob> = Vec::new();
static mut NEXT_JOB: u32 = 1;
/// Pictures fetched side by side: each worker is a green thread with its own
/// connection (or a kept-alive one) under a shared net claim.
const WORKERS: usize = 4;
static mut WORKER_IDS: [u64; WORKERS] = [0; WORKERS];

// ── the picture cache ──
// Every fetched picture's bytes stay in memory (least recently used out past
// MEM_CAP), so a page shown again -- Discord redrawing a channel, going back --
// gets its pictures without the network. Discord's avatars, server icons and
// emoji are named by their content hash, never change, and come back on every
// channel: those are also kept on disk (the home folder on a data volume, not
// the small lingfs), so they show at once after a reboot.

const MEM_CAP: usize = 48 << 20;
const MEM_ONE_MAX: usize = 4 << 20;
struct Cached {
    key: String,
    bytes: Vec<u8>,
    used: u64,
}
static mut MEM: Vec<Cached> = Vec::new();
static mut MEM_BYTES: usize = 0;
static mut MEM_TICK: u64 = 0;
static mut CACHE_HITS: u64 = 0;
static mut DISK_HITS: u64 = 0;
static mut NET_FETCHES: u64 = 0;
static mut DISK_FILES: i64 = -1;
const DISK_MAX_FILES: i64 = 6000;
const DISK_ONE_MAX: usize = 512 * 1024;

fn mem() -> &'static mut Vec<Cached> {
    unsafe { &mut *(&raw mut MEM) }
}

/// The cache key: the address, but for Discord's CDN without the signed,
/// expiring query (`ex=`, `is=`, `hm=`), which changes while the file does not.
fn cache_key(url: &str) -> String {
    let Some((base, q)) = url.split_once('?') else { return url.to_string() };
    if !is_discord_cdn(url) {
        return url.to_string();
    }
    let keep: Vec<&str> = q.split('&').filter(|p| p.starts_with("size=") || p.starts_with("width=") || p.starts_with("height=") || p.starts_with("format=")).collect();
    if keep.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", keep.join("&"))
    }
}

fn is_discord_cdn(url: &str) -> bool {
    let rest = url.strip_prefix("https://").unwrap_or("");
    rest.starts_with("cdn.discordapp.com/") || rest.starts_with("media.discordapp.net/")
}

/// Discord pictures that never change at their address (kept on disk).
fn disk_cacheable(url: &str) -> bool {
    is_discord_cdn(url) && ["/avatars/", "/icons/", "/emojis/", "/embed/avatars/", "/banners/", "/role-icons/", "/guild-events/", "/app-icons/", "/stickers/", "/avatar-decoration-presets/"].iter().any(|p| url.contains(p))
}

fn mem_get(key: &str) -> Option<Vec<u8>> {
    let c = mem().iter_mut().find(|c| c.key == key)?;
    unsafe {
        MEM_TICK += 1;
        c.used = MEM_TICK;
    }
    Some(c.bytes.clone())
}

fn mem_put(key: &str, bytes: &[u8]) {
    if bytes.len() > MEM_ONE_MAX || mem().iter().any(|c| c.key == key) {
        return;
    }
    unsafe {
        while MEM_BYTES + bytes.len() > MEM_CAP && !mem().is_empty() {
            let (i, _) = mem().iter().enumerate().min_by_key(|(_, c)| c.used).unwrap();
            MEM_BYTES -= mem()[i].bytes.len();
            mem().swap_remove(i);
        }
        MEM_TICK += 1;
        MEM_BYTES += bytes.len();
        mem().push(Cached { key: key.to_string(), bytes: bytes.to_vec(), used: MEM_TICK });
    }
}

fn disk_dir() -> Option<String> {
    let d = format!("{}/.cache/horizon-pictures", crate::vfs::home());
    // only on a data volume (the Pi's card, the installed PC's data partition, a second disk):
    // lingfs is a few MiB and holds the accounts
    crate::vfs::storage_path(&d)?;
    Some(d)
}

fn disk_name(key: &str) -> String {
    let h = blake3::hash(key.as_bytes());
    let mut out = String::new();
    for b in &h.as_bytes()[..12] {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn disk_get(key: &str) -> Option<Vec<u8>> {
    let d = disk_dir()?;
    crate::vfs::read(&format!("{d}/{}", disk_name(key)))
}

fn disk_put(key: &str, bytes: &[u8]) {
    if bytes.len() > DISK_ONE_MAX {
        return;
    }
    let Some(d) = disk_dir() else { return };
    unsafe {
        if DISK_FILES < 0 {
            crate::vfs::mkdir(&d);
            DISK_FILES = crate::vfs::list(&d).len() as i64;
        }
        if DISK_FILES >= DISK_MAX_FILES {
            return;
        }
    }
    if crate::vfs::write(&format!("{d}/{}", disk_name(key)), bytes) {
        unsafe { DISK_FILES += 1 };
    }
}

/// The `horizon cache` console line.
pub fn cache_report() -> String {
    unsafe {
        format!(
            "pictures in memory: {} ({} KB of {} MB); on disk: {} ({}); this boot: {} from memory, {} from disk, {} over the network; idle connections kept: {}; network transactions now: {}\n",
            mem().len(),
            MEM_BYTES / 1024,
            MEM_CAP >> 20,
            if DISK_FILES < 0 { String::from("not looked at yet") } else { DISK_FILES.to_string() },
            disk_dir().unwrap_or_else(|| String::from("no data volume: memory only")),
            CACHE_HITS,
            DISK_HITS,
            NET_FETCHES,
            crate::lingsock::pool_len(),
            ns::net_sharers(),
        )
    }
}

/// Forget the cached pictures (memory and disk).
pub fn cache_clear() -> String {
    unsafe {
        mem().clear();
        MEM_BYTES = 0;
    }
    let mut n = 0;
    if let Some(d) = disk_dir() {
        for (name, dir, _) in crate::vfs::list(&d) {
            if !dir && crate::vfs::delete(&format!("{d}/{name}")) {
                n += 1;
            }
        }
        unsafe { DISK_FILES = 0 };
    }
    format!("picture cache cleared ({n} files on disk)\n")
}

/// A picture's bytes: the memory cache, the disk cache, then the network.
fn picture_bytes(url: &str) -> Result<Vec<u8>, String> {
    if url.starts_with("data:") {
        return fetch_bytes(url);
    }
    // a picture on this machine (vfs path), for the browser's own pages -- Discord's preview of a
    // pasted screenshot. Horizon lets only its about: pages ask for these (layout.ling
    // ly_img_request): a web page never gets a local file shown. Not cached: it may change.
    if let Some(p) = url.strip_prefix("vfs:") {
        let b = crate::vfs::read(p.trim_start_matches('/')).ok_or_else(|| format!("no such file: {p}"))?;
        if b.len() > IMG_MAX_BYTES {
            return Err(String::from("the picture is too big"));
        }
        return Ok(b);
    }
    let key = cache_key(url);
    if let Some(b) = mem_get(&key) {
        unsafe { CACHE_HITS += 1 };
        return Ok(b);
    }
    let disk = disk_cacheable(url);
    if disk {
        if let Some(b) = disk_get(&key) {
            if !b.is_empty() {
                unsafe { DISK_HITS += 1 };
                mem_put(&key, &b);
                return Ok(b);
            }
        }
    }
    unsafe { NET_FETCHES += 1 };
    let b = fetch_bytes(url)?;
    mem_put(&key, &b);
    if disk {
        disk_put(&key, &b);
    }
    Ok(b)
}

fn jobs() -> &'static mut Vec<ImgJob> {
    unsafe { &mut *(&raw mut JOBS) }
}

/// The bytes at a web address: (bytes) or why not.
fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',').ok_or("bad data: URL")?;
        if meta.ends_with(";base64") {
            use base64::Engine;
            let clean: String = data.chars().filter(|c| !c.is_whitespace()).collect();
            let clean = clean.replace("%2B", "+").replace("%2F", "/").replace("%3D", "=");
            return base64::engine::general_purpose::STANDARD
                .decode(clean.trim_end_matches('='))
                .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(clean.trim_end_matches('=')))
                .map_err(|_| "bad base64".to_string());
        }
        // an SVG quoted inside a stylesheet keeps its backslash escapes (url("data:...<svg fill=\"#000\">"))
        let mut d = pct_decode(data);
        if d.windows(2).any(|w| w == b"\\\"" || w == b"\\'") {
            let mut o = Vec::with_capacity(d.len());
            let mut i = 0;
            while i < d.len() {
                if d[i] == b'\\' && i + 1 < d.len() && (d[i + 1] == b'"' || d[i + 1] == b'\'' || d[i + 1] == b'\\') {
                    i += 1;
                }
                o.push(d[i]);
                i += 1;
            }
            d = o;
        }
        return Ok(d);
    }
    let mut cur = url.to_string();
    for _ in 0..5 {
        let (host, port, path, tls) = {
            let (h, p, pa, t) = ns::parse_url(&cur).ok_or("bad URL")?;
            (h.to_string(), p, pa.to_string(), t)
        };
        let _ = (&host, port, &path, tls);
        let (status, head, body) = crate::lingsock::request_full(
            "GET",
            &cur,
            "Accept: image/png,image/jpeg,image/svg+xml,image/bmp,image/gif;q=0.9,*/*;q=0.1\nUser-Agent: LingOS Horizon",
            &[],
            IMG_MAX_BYTES,
        )?;
        let location = header(head.as_bytes(), "location");
        if (300..400).contains(&status) {
            let loc = location.ok_or("redirect without a Location")?;
            cur = resolve(&cur, &loc);
            continue;
        }
        if status != 200 && status != 203 && status != 206 {
            return Err(format!("HTTP {status}"));
        }
        return Ok(body);
    }
    Err("too many redirects".to_string())
}

/// HTTPS GET: (status, body, Location of a redirect).
fn https_get(host: &str, port: u16, path: &str) -> Result<(u16, Vec<u8>, Option<String>), String> {
    let mut body: Vec<u8> = Vec::new();
    let mut status = 0u16;
    let mut too_big = false;
    let mut log = |_m: &[u8]| {};
    let info = crate::tls::https_get_stream(
        host,
        port,
        path,
        "Accept: image/png,image/jpeg,image/svg+xml,image/bmp;q=0.9,*/*;q=0.1\r\n",
        &mut |i| {
            status = i.status;
            // a redirect or an error: no body wanted
            i.status == 200 || i.status == 203 || i.status == 206
        },
        &mut |piece| {
            if body.len() + piece.len() > IMG_MAX_BYTES {
                too_big = true;
                return false;
            }
            body.extend_from_slice(piece);
            true
        },
        &mut log,
    )
    .map_err(|e| e.to_string())?;
    if too_big {
        return Err("picture too big".to_string());
    }
    let status = if status == 0 { info.status } else { status };
    if (300..400).contains(&status) {
        // the streaming reader keeps no headers: ask again for the whole (short) answer
        let mut out = alloc::vec![0u8; 64 * 1024];
        let n = crate::tls::https_request(host, port, "GET", path, "", "", &[], &mut out, 3800, &mut log).map_err(|e| e.to_string())?;
        let he = find(&out[..n], b"\r\n\r\n").unwrap_or(n);
        return Ok((status, Vec::new(), header(&out[..he], "location")));
    }
    Ok((status, body, None))
}

fn pct_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// `loc` (a Location header) against the address it came from.
pub(crate) fn resolve(base: &str, loc: &str) -> String {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    if let Some(rest) = loc.strip_prefix("//") {
        return format!("{}{rest}", &base[..scheme_end]);
    }
    let host_end = base[scheme_end..].find('/').map(|i| i + scheme_end).unwrap_or(base.len());
    if loc.starts_with('/') {
        return format!("{}{loc}", &base[..host_end]);
    }
    let dir_end = base.rfind('/').filter(|&i| i >= host_end).map(|i| i + 1).unwrap_or(base.len());
    let mut out = base[..dir_end].to_string();
    if !out.ends_with('/') {
        out.push('/');
    }
    out.push_str(loc);
    out
}

/// Big pictures are kept at most this wide / tall (the page shows them smaller anyway).
const IMG_KEEP_SIDE: u32 = 2048;

fn load_picture(url: &str) -> f64 {
    let bytes = match picture_bytes(url) {
        Ok(b) => b,
        Err(e) => {
            crate::console_write(format!("[image] {url}: {e}\n").as_bytes());
            return -1.0;
        },
    };
    let Some(mut img) = image::decode_web(&bytes) else {
        crate::console_write(format!("[image] {url}: not a picture the kernel reads ({} bytes)\n", bytes.len()).as_bytes());
        return -1.0;
    };
    drop(bytes);
    if img.w == 0 || img.h == 0 {
        return -1.0;
    }
    if img.w > IMG_KEEP_SIDE || img.h > IMG_KEEP_SIDE {
        let s = IMG_KEEP_SIDE as f64 / img.w.max(img.h) as f64;
        img = image::resize_rgba(&img, ((img.w as f64 * s) as u32).max(1), ((img.h as f64 * s) as u32).max(1));
    }
    compat::image_push(img)
}

fn image_worker() {
    let me = crate::coro::current();
    loop {
        let next = jobs().iter_mut().find(|j| j.state == JobState::Queued).map(|j| {
            j.state = JobState::Running;
            j.worker = me;
            (j.id, j.url.clone())
        });
        let Some((id, url)) = next else { break };
        let r = load_picture(&url);
        match jobs().iter_mut().find(|j| j.id == id) {
            Some(j) => j.state = JobState::Done(r),
            // nobody waits for it any more
            None if r >= 0.0 => compat::image_release(r),
            None => {},
        }
        crate::coro::yield_now();
    }
}

fn ensure_worker() {
    unsafe {
        // a worker that was stopped mid-fetch (its app closed) leaves its job to the others
        for j in jobs().iter_mut() {
            if j.state == JobState::Running && !crate::coro::alive(j.worker) {
                j.state = JobState::Queued;
            }
        }
        let queued = jobs().iter().filter(|j| j.state == JobState::Queued).count();
        let mut running = (0..WORKERS).filter(|&i| WORKER_IDS[i] != 0 && crate::coro::alive(WORKER_IDS[i])).count();
        for i in 0..WORKERS {
            if running >= queued.min(WORKERS) {
                break;
            }
            if WORKER_IDS[i] != 0 && crate::coro::alive(WORKER_IDS[i]) {
                continue;
            }
            WORKER_IDS[i] = crate::coro::spawn("horizon-images", false, Box::new(image_worker));
            if WORKER_IDS[i] == 0 {
                if running == 0 {
                    // no stack for a green thread: fetch right here
                    image_worker();
                }
                break;
            }
            running += 1;
        }
    }
}

pub(crate) fn image_fetch(url: &str) -> f64 {
    let id = unsafe {
        let id = NEXT_JOB;
        NEXT_JOB = NEXT_JOB.wrapping_add(1).max(1);
        id
    };
    if jobs().len() > 2000 {
        return -1.0;
    }
    jobs().push(ImgJob { id, url: url.to_string(), state: JobState::Queued, worker: 0 });
    ensure_worker();
    id as f64
}

pub(crate) fn image_job(id: f64) -> f64 {
    let id = id as u32;
    match jobs().iter().position(|j| j.id == id) {
        Some(i) => match jobs()[i].state {
            JobState::Done(r) => {
                jobs().remove(i);
                r
            },
            _ => {
                // the worker may have been stopped with its app: carry on
                ensure_worker();
                -2.0
            },
        },
        None => -1.0,
    }
}

/// The page changed: what is still queued is not wanted (those jobs answer -1).
fn image_fetch_cancel() {
    for j in jobs().iter_mut() {
        if j.state == JobState::Queued {
            j.state = JobState::Done(-1.0);
        }
    }
}

fn image_resized(id: f64, w: f64, h: f64) -> f64 {
    let Some(src) = compat::image_get(id) else { return -1.0 };
    if !(w >= 1.0 && h >= 1.0) || w > 4096.0 || h > 4096.0 {
        return -1.0;
    }
    let img = image::resize_rgba(src, w as u32, h as u32);
    compat::image_push(img)
}

/// The picture's shape (its alpha) in one colour (`0xRRGGBB`).
fn image_tint(id: f64, col: f64) -> f64 {
    let Some(src) = compat::image_get(id) else { return -1.0 };
    let c = (col.max(0.0) as u32) & 0xFF_FFFF;
    let px = src.px.iter().map(|&p| (p & 0xFF00_0000) | c).collect();
    compat::image_push(image::Image { w: src.w, h: src.h, px })
}

/// Inline `<svg>` markup drawn at (w, h).
fn image_svg(markup: &str, w: f64, h: f64) -> f64 {
    let cap = w.max(h).clamp(8.0, 1024.0) as u32;
    let Some(img) = image::decode_svg_alpha(markup.as_bytes(), cap) else { return -1.0 };
    let (tw, th) = (w.clamp(1.0, 2048.0) as u32, h.clamp(1.0, 2048.0) as u32);
    let img = if img.w != tw || img.h != th { image::resize_rgba(&img, tw, th) } else { img };
    compat::image_push(img)
}

/// The builtins: `image_*` (web pictures) and `os_video_*` (the player, `videoplay.rs`).
pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let n = |i: usize, d: f64| args.get(i).map(|&v| to_f64(v)).unwrap_or(d);
    Some(match name {
        b"image_fetch" => num(image_fetch(&s(0))),
        b"image_job" => num(image_job(n(0, -1.0))),
        b"image_fetch_cancel" => {
            image_fetch_cancel();
            UNIT
        },
        b"image_resized" => num(image_resized(n(0, -1.0), n(1, 0.0), n(2, 0.0))),
        b"image_tint" => num(image_tint(n(0, -1.0), n(1, 0.0))),
        b"image_svg" => num(image_svg(&s(0), n(1, 24.0), n(2, 24.0))),
        _ if name.starts_with(b"os_video_") => return super::videoplay::dispatch(name, args),
        _ => return None,
    })
}
