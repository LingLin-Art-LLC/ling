//! LingDiffusion as a SERVER: this machine's Stable Diffusion engine (the in-kernel `ling-sd` CPU engine, see diffusion.rs)
//! offered to other machines over the same HTTP API the LingDiffusion clients already speak, so another LingOS (its
//! LingDiffusion app's "GPU server" backend and LAN search), the PC's LingDiffusion tools, LingAnime's SD bridge or
//! anything written for the AUTOMATIC1111 WebUI can use it unchanged:
//!
//! * the WebUI API: `GET /sdapi/v1/options | sd-models | samplers | schedulers | loras | sd-vae | upscalers | progress |
//!   memory`, `POST /sdapi/v1/options` (switch checkpoint), `POST /sdapi/v1/txt2img` (answers when the picture is done,
//!   `{images: [base64 png], parameters, info}`), `POST /sdapi/v1/interrupt | skip`;
//! * LingDiffusion's queued API: `POST /api/generate?mode=txt2img` -> `{job, outputs_root}`, `GET /api/status?job=`,
//!   `GET /api/preview?job=`, `POST /api/interrupt?job= | skip?job=`, `GET /api/engine | models`, `POST /api/unload`, and
//!   the finished picture at `GET /outputs/<job>.png`;
//! * `GET /health`, and `GET /` (a small status page for a browser).
//!
//! txt2img only (img2img, upscaling and editing answer 422). One picture per request (batch_size / n_iter > 1 make one).
//!
//! **Discovery.** LingDiffusion has no discovery protocol of its own: clients probe `GET /sdapi/v1/options` + `/api/engine`
//! on ports 7860 / 7861 of the gateway and the /24 (diffusion.rs `search`). Answering those IS answering discovery, so a
//! host on 0.0.0.0 or the LAN address on port 7860/7861 is found by every LingOS on the network. `/api/engine` carries
//! `lingos_host` (a random id of this boot) so this machine's own client never picks itself.
//!
//! **Where it listens** (Settings > "Serve pictures to other machines", the `diffusion serve` command; kept in lingfs
//! `/diffhost`, started at boot when on). The netstack has one active NIC (wired or Wi-Fi) and a loopback ring, so this
//! machine's addresses are 127.0.0.1 and `self_ip()`; the listener is bound like bind(2) (`netstack::tcp_listen_bind`):
//! * `127.0.0.1` / `localhost` (the default) -- local only: programs on this machine, nothing from the network;
//! * `0.0.0.0` -- every address: the network (anyone on it) and 127.0.0.1;
//! * this machine's LAN address -- connections to that address only (not 127.0.0.1); an address the machine does not
//!   have waits until it does (DHCP / static), nothing connects meanwhile;
//! * a name -- this machine's own name (`hostname`, `hostname.local`) means its LAN address; otherwise it is looked up:
//!   127.x -> local only, this machine's address -> that, anything else (typically a domain pointing at the router's
//!   public address, with the port forwarded here) -> the LAN address, since that is where forwarded connections arrive.
//!   The settings say which it chose and why. Re-resolved every 10 minutes and whenever the LAN address changes.
//!
//! An optional access key: then every request needs HTTP Basic auth with the key as the password (any user name), as the
//! WebUI's `--api-auth`, or `Authorization: Bearer <key>`, or `?key=<key>`. A LingOS client sends it when its server
//! address is written `key@host:port`.
//!
//! Nothing here waits on the network: one "system" coroutine (`diffusion-host`) accepts, reads, answers a little per
//! turn (`tcp_writable_h` before every write, FIN then free later), and hands jobs to the engine's own task one at a time,
//! after the app's own pictures. A WebUI-style txt2img connection just stays open until its job is done.
use crate::arch::timer;
use crate::drivers::diffusion as dfx;
use crate::drivers::netstack as ns;
use crate::lingrt::{boolean, mk_str, num, to_f64, to_string};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde_json::Value as J;

const CONF_PATH: &str = "/diffhost";
pub const DEFAULT_PORT: u16 = 7860;
const MAX_REQ: usize = 256 * 1024;
/// a connection that sends or takes nothing for this long is dropped (not while it waits for its picture)
const IDLE_US: u64 = 30_000_000;
const MAX_CONNS: usize = 8;
const MAX_QUEUE: usize = 8;
const KEEP_JOBS: usize = 16;
/// finished pictures kept for /outputs and the status of jobs nobody fetched yet
const KEEP_PNGS: usize = 6;
const SAMPLERS: [&str; 4] = ["Euler a", "Euler", "DPM++ 2M Karras", "DPM++ 2M"];

#[derive(Clone)]
struct Conf {
    on: bool,
    addr: String,
    port: u16,
    key: String,
}

struct Job {
    n: u64,
    id: String,
    peer: [u8; 4],
    prompt: String,
    negative: String,
    sampler: String,
    steps: usize,
    cfg: f32,
    w: usize,
    h: usize,
    seed: i64,
    /// the request as sent (the WebUI's reply echoes it as `parameters`)
    params: J,
    state: &'static str,
    error: String,
    png: Vec<u8>,
    info: String,
    seed_used: u64,
    t_start: u64,
    t_end: u64,
    /// the client went away: nobody wants it any more
    orphan: bool,
}

struct Conn {
    h: usize,
    peer: [u8; 4],
    buf: Vec<u8>,
    last: u64,
    out: Option<Vec<u8>>,
    sent: usize,
    fin_at: Option<u64>,
    /// a WebUI txt2img waiting for its picture
    wait: Option<u64>,
}

struct Host {
    conf: Conf,
    conf_loaded: bool,
    /// the port listening now (0 = none)
    bound_port: u16,
    bind_ip: [u8; 4],
    state: &'static str,
    note: String,
    warn: bool,
    /// the bind follows this machine's LAN address (re-applied when it changes)
    last_self: [u8; 4],
    applied_at: u64,
    want_apply: bool,
    conns: Vec<Conn>,
    jobs: Vec<Job>,
    running: u64,
    next_n: u64,
    served: u32,
    requests: u32,
    host_id: String,
    /// the live preview last encoded: (engine picture id, base64 PNG)
    preview: (u64, String),
    /// this machine's name (lingfs /hostname, read when the settings are; never per frame)
    hostname: String,
}

static mut HOST: Option<Host> = None;
static mut TASK: u64 = 0;

fn h() -> &'static mut Host {
    unsafe {
        (*(&raw mut HOST)).get_or_insert_with(|| Host {
            conf: Conf { on: false, addr: "127.0.0.1".to_string(), port: DEFAULT_PORT, key: String::new() },
            conf_loaded: false,
            bound_port: 0,
            bind_ip: [127, 0, 0, 1],
            state: "off",
            note: String::new(),
            warn: false,
            last_self: [0; 4],
            applied_at: 0,
            want_apply: false,
            conns: Vec::new(),
            jobs: Vec::new(),
            running: 0,
            next_n: 1,
            served: 0,
            requests: 0,
            host_id: new_id(),
            preview: (0, String::new()),
            hostname: String::from("lingos"),
        })
    }
}

fn new_id() -> String {
    let mut r = [0u8; 8];
    if !crate::crypto::random_bytes(&mut r) {
        r = (timer::now_us() ^ 0x4C49_4E47_4449_4646).to_le_bytes();
    }
    r.iter().map(|b| format!("{b:02x}")).collect()
}

fn log(s: &str) {
    crate::drivers::serial::write(b"diffusion-host: ");
    crate::drivers::serial::write(s.as_bytes());
    crate::drivers::serial::write(b"\n");
}

fn ip_text(ip: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut it = s.split('.');
    for o in out.iter_mut() {
        *o = it.next()?.trim().parse().ok()?;
    }
    it.next().is_none().then_some(out)
}

fn private_ip(ip: [u8; 4]) -> bool {
    ip[0] == 10 || (ip[0] == 172 && (16..32).contains(&ip[1])) || (ip[0] == 192 && ip[1] == 168) || (ip[0] == 169 && ip[1] == 254) || (ip[0] == 100 && (64..128).contains(&ip[1]))
}

fn jstr(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// This boot's server id (`/api/engine`'s `lingos_host`): lets diffusion.rs's client skip its own machine.
pub fn host_id() -> String {
    h().host_id.clone()
}

// ── settings ───────────────────────────────────────────────────────────────────

fn load_conf() {
    let s = h();
    if s.conf_loaded || !crate::fs::lingfs::is_mounted() {
        return;
    }
    s.conf_loaded = true;
    s.hostname = crate::lingapp::host_name();
    let mut buf = [0u8; 1024];
    if let Ok(Some(n)) = crate::fs::lingfs::read_file_all(CONF_PATH, &mut buf) {
        let text = core::str::from_utf8(&buf[..n]).unwrap_or("");
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "on" => s.conf.on = v == "1",
                "addr" => s.conf.addr = v.to_string(),
                "port" => s.conf.port = v.parse::<u16>().ok().filter(|p| *p > 0).unwrap_or(DEFAULT_PORT),
                "key" => s.conf.key = v.to_string(),
                _ => {},
            }
        }
    }
}

fn save_conf() -> bool {
    let c = &h().conf;
    let text = format!("on={}\naddr={}\nport={}\nkey={}\n", if c.on { 1 } else { 0 }, c.addr, c.port, c.key);
    crate::fs::lingfs::is_mounted() && crate::fs::lingfs::write_file_any(CONF_PATH, text.as_bytes()).is_ok()
}

/// "addr:port", "addr", ":port" or "port" -> (addr, port); the port must be 1-65535.
pub fn parse_listen(spec: &str, default_port: u16) -> Result<(String, u16), String> {
    let t = spec.trim().trim_start_matches("http://").trim_end_matches('/');
    if t.is_empty() {
        return Ok(("127.0.0.1".to_string(), default_port));
    }
    if t.chars().all(|c| c.is_ascii_digit()) {
        return valid_port(t).map(|p| ("127.0.0.1".to_string(), p));
    }
    match t.rsplit_once(':') {
        Some((a, p)) => valid_port(p).map(|p| (if a.is_empty() { "127.0.0.1".to_string() } else { a.to_string() }, p)),
        None => Ok((t.to_string(), default_port)),
    }
}

fn valid_port(p: &str) -> Result<u16, String> {
    match p.trim().parse::<u32>() {
        Ok(v) if (1..=65535).contains(&v) => Ok(v as u16),
        _ => Err(format!("\"{}\" is not a port: use a number from 1 to 65535", p.trim())),
    }
}

/// Change the settings (saved, applied by the host task). Err(why) for a bad port / address.
pub fn configure(on: bool, addr: &str, port: u16, key: &str) -> Result<(), String> {
    load_conf();
    if port == 0 {
        return Err("the port must be a number from 1 to 65535".to_string());
    }
    let addr = addr.trim();
    if addr.contains(char::is_whitespace) || addr.contains('/') || addr.contains('@') {
        return Err(format!("\"{addr}\" is not an address: an IP (127.0.0.1, 0.0.0.0, 192.0.2.10) or a name"));
    }
    if key.contains(char::is_whitespace) || key.contains(':') {
        return Err("the access key cannot contain spaces or \":\"".to_string());
    }
    let s = h();
    s.conf = Conf { on, addr: if addr.is_empty() { "127.0.0.1".to_string() } else { addr.to_string() }, port, key: key.to_string() };
    s.want_apply = true;
    if !on {
        s.state = "off";
    } else if s.state == "off" {
        s.state = "starting";
    }
    let saved = save_conf();
    if !saved {
        log("could not save /diffhost (no LingOS storage?)");
    }
    ensure_task();
    Ok(())
}

/// At boot (startup::boot_once): serve when it was left on.
pub fn boot() {
    load_conf();
    if h().conf.on {
        h().want_apply = true;
        h().state = "starting";
        ensure_task();
    }
}

fn ensure_task() {
    unsafe {
        if TASK == 0 || !crate::coro::alive(TASK) {
            TASK = crate::coro::spawn_owned("diffusion-host", false, "system", Box::new(task));
        }
    }
}

// ── where to listen ────────────────────────────────────────────────────────────

/// What the address means here: (address for the netstack's bind, state, explanation, reachable from the network).
fn resolve_bind(addr: &str, port: u16) -> ([u8; 4], &'static str, String, bool) {
    let a = addr.trim().trim_end_matches('.').to_ascii_lowercase();
    let me = ns::self_ip();
    let have = me != [0; 4];
    let lan = if have { format!("{}:{port}", ip_text(me)) } else { "(no network address yet)".to_string() };
    let local_only = |why: String| ([127, 0, 0, 1], "listening", format!("{why}Local only: programs on this machine reach it at 127.0.0.1:{port}; nothing on the network can."), false);
    if a.is_empty() || a == "localhost" || a == "loopback" || a.starts_with("127.") && parse_ip(&a).is_some() {
        return local_only(String::new());
    }
    if matches!(a.as_str(), "0.0.0.0" | "*" | "any" | "all" | "::" | "[::]") {
        return ([0; 4], "listening", format!("All addresses: anyone on this network can use it at {lan}, and this machine at 127.0.0.1:{port}."), true);
    }
    if let Some(ip) = parse_ip(&a) {
        if have && ip == me {
            return (ip, "listening", format!("This machine's network address: reachable from the network at {lan} (not at 127.0.0.1)."), true);
        }
        if !have {
            return (ip, "waiting", format!("This machine has no network address yet: it serves on {}:{port} once it gets that address.", ip_text(ip)), true);
        }
        return (ip, "waiting", format!("{} is not an address of this machine (it has 127.0.0.1 and {}): nothing can connect until it gets {}. Use {} or 0.0.0.0.", ip_text(ip), ip_text(me), ip_text(ip), ip_text(me)), false);
    }
    // a name
    let own = h().hostname.to_ascii_lowercase();
    if a == own || a == format!("{own}.local") || a == format!("{own}.lan") || a == format!("{own}.home") {
        if !have {
            return (me, "waiting", format!("\"{a}\" is this machine's own name; it has no network address yet, so it serves once it gets one."), true);
        }
        return (me, "listening", format!("\"{a}\" is this machine's own name, so it listens on its network address: {lan}."), true);
    }
    let Some(r) = ns::dns_resolve(&a) else {
        return ([0; 4], "error", format!("\"{a}\" could not be looked up (no DNS answer): not serving. It is tried again every minute; or use an IP address."), false);
    };
    if r[0] == 127 {
        return local_only(format!("\"{a}\" resolves to {} (loopback). ", ip_text(r)));
    }
    if !have {
        return (me, "waiting", format!("\"{a}\" resolves to {}; this machine has no network address yet.", ip_text(r)), true);
    }
    if r == me {
        return (me, "listening", format!("\"{a}\" resolves to {}, this machine's network address: reachable at {a}:{port}.", ip_text(r)), true);
    }
    if private_ip(r) {
        return (me, "listening", format!("\"{a}\" resolves to {}, another machine on this network (this one is {}): listening on {lan} instead. Check the name.", ip_text(r), ip_text(me)), true);
    }
    (
        me,
        "listening",
        format!(
            "\"{a}\" resolves to {}, which is not this machine -- usually your router's public address. With port {port} forwarded to {}, connections to {a}:{port} arrive here, so it listens on {lan}.",
            ip_text(r),
            ip_text(me)
        ),
        true,
    )
}

/// The explanation without looking anything up (for the settings screen while typing).
pub fn describe(addr: &str, port: u16) -> String {
    let a = addr.trim().to_ascii_lowercase();
    if a.is_empty() || a == "localhost" || a == "loopback" || a.starts_with("127.") || parse_ip(&a).is_some() || matches!(a.as_str(), "0.0.0.0" | "*" | "any" | "all") {
        return resolve_bind(&a, port).2;
    }
    let own = h().hostname.to_ascii_lowercase();
    if a == own || a == format!("{own}.local") {
        return resolve_bind(&a, port).2;
    }
    format!("\"{a}\" is a name: it is looked up when the server starts. If it points at this machine (or at your router, with port {port} forwarded here) it listens on this machine's network address.")
}

fn stop_listening() {
    let s = h();
    for c in s.conns.drain(..) {
        ns::tcp_free_h(c.h);
    }
    if s.bound_port != 0 {
        ns::tcp_unlisten(s.bound_port);
        ns::tcp_unbind(s.bound_port);
        log(&format!("stopped listening on port {}", s.bound_port));
        s.bound_port = 0;
    }
}

fn apply() {
    let s = h();
    s.want_apply = false;
    s.applied_at = timer::now_ms();
    s.last_self = ns::self_ip();
    s.hostname = crate::lingapp::host_name();
    let c = s.conf.clone();
    if !c.on {
        stop_listening();
        // nobody will collect what is queued; the running one stops
        let s = h();
        for j in s.jobs.iter_mut() {
            if j.state == "queued" {
                j.state = "interrupted";
                j.error = "the server was switched off".to_string();
            }
        }
        if s.running != 0 {
            dfx::engine_cancel();
        }
        s.state = "off";
        s.note.clear();
        s.warn = false;
        return;
    }
    let (ip, state, note, warn) = resolve_bind(&c.addr, c.port);
    let s = h();
    // another service on this port (the SSH server, the phone page) -- never share its listener
    if s.bound_port != c.port && ns::tcp_listening(c.port) {
        stop_listening();
        let s = h();
        s.state = "error";
        s.note = format!("Port {} is already used by another service on this machine: pick another (7860 or 7861 are what LingDiffusion clients look for).", c.port);
        s.warn = false;
        return;
    }
    if state == "error" {
        stop_listening();
        let s = h();
        s.state = state;
        s.note = note;
        s.warn = false;
        return;
    }
    if s.bound_port != 0 && s.bound_port != c.port {
        stop_listening();
    }
    let s = h();
    ns::tcp_listen_bind(c.port, ip);
    if s.bound_port == 0 {
        // open the listener now (the first SYN connects)
        match ns::tcp_listen_accept(c.port, 1) {
            Some(hd) => {
                ns::adopt_socket(hd, None);
                let peer = ns::tcp_remote_h(hd).map(|r| r.0).unwrap_or([0; 4]);
                h().conns.push(Conn::new(hd, peer));
            },
            None => {
                if !ns::tcp_listening(c.port) {
                    let s = h();
                    s.state = "error";
                    s.note = "No free network socket to listen on.".to_string();
                    return;
                }
            },
        }
        log(&format!("listening on port {} bound to {} -- {note}", c.port, ip_text(ip)));
    }
    let s = h();
    s.bound_port = c.port;
    s.bind_ip = ip;
    s.state = state;
    s.note = note;
    s.warn = warn;
}

// ── the task ───────────────────────────────────────────────────────────────────

fn task() {
    loop {
        load_conf();
        let s = h();
        if s.want_apply {
            apply();
        }
        let s = h();
        if !s.conf.on && s.bound_port == 0 && s.running == 0 {
            // off and nothing left to finish: the task ends (configure starts it again)
            break;
        }
        // the machine's address changed (DHCP, Wi-Fi), or a name to look up again
        let now = timer::now_ms();
        if s.conf.on && (ns::self_ip() != s.last_self || (s.state == "error" && now.saturating_sub(s.applied_at) > 60_000) || now.saturating_sub(s.applied_at) > 600_000) {
            s.want_apply = true;
            continue;
        }
        if s.bound_port != 0 {
            serve_once();
        }
        schedule();
        let busy = h().conns.iter().any(|c| c.out.is_some() || !c.buf.is_empty());
        crate::coro::sleep_ms(if busy { 1 } else { 10 });
    }
    unsafe { TASK = 0 };
}

/// The next queued job onto the engine, when it is free.
fn schedule() {
    let s = h();
    if s.running != 0 || dfx::engine_busy() {
        return;
    }
    // jobs whose WebUI client went away before they started are dropped
    for j in s.jobs.iter_mut() {
        if j.state == "queued" && j.orphan {
            j.state = "interrupted";
            j.error = "the client went away".to_string();
        }
    }
    let Some(j) = s.jobs.iter_mut().find(|j| j.state == "queued") else { return };
    if dfx::host_submit(j.n, &j.prompt, &j.negative, &j.sampler, j.steps, j.cfg, j.w, j.h, j.seed) {
        j.state = "loading";
        j.t_start = timer::now_ms();
        s.running = j.n;
        s.preview = (0, String::new());
        log(&format!("job {} for {} started: {}x{}, {} steps of {}", j.id, ip_text(j.peer), j.w, j.h, j.steps, j.sampler));
    }
}

/// diffusion.rs: the engine is done with hosted job `n`.
pub fn finish(n: u64, res: Result<(Vec<u8>, String, u64, usize, usize), String>) {
    let s = h();
    if s.running == n {
        s.running = 0;
    }
    let Some(j) = s.jobs.iter_mut().find(|j| j.n == n) else { return };
    j.t_end = timer::now_ms();
    match res {
        Ok((png, info, seed, w, h_)) => {
            j.state = "done";
            j.png = png;
            j.info = info;
            j.seed_used = seed;
            j.w = w;
            j.h = h_;
            s.served += 1;
        },
        Err(e) => {
            j.state = if e == "interrupted" { "interrupted" } else { "error" };
            j.error = e;
        },
    }
    // keep the newest pictures only
    let mut with_png = 0;
    for j in s.jobs.iter_mut().rev() {
        if !j.png.is_empty() {
            with_png += 1;
            if with_png > KEEP_PNGS {
                j.png = Vec::new();
            }
        }
    }
}

/// diffusion.rs: who asked for hosted job `n` (for its messages).
pub fn job_peer(n: u64) -> String {
    h().jobs.iter().find(|j| j.n == n).map(|j| ip_text(j.peer)).unwrap_or_else(|| "another machine".to_string())
}

impl Conn {
    fn new(h: usize, peer: [u8; 4]) -> Conn {
        Conn { h, peer, buf: Vec::new(), last: timer::now_us(), out: None, sent: 0, fin_at: None, wait: None }
    }
}

fn serve_once() {
    let port = h().bound_port;
    for _ in 0..4 {
        if h().conns.len() >= MAX_CONNS {
            break;
        }
        match ns::tcp_listen_accept(port, 1) {
            Some(hd) => {
                ns::adopt_socket(hd, None);
                let peer = ns::tcp_remote_h(hd).map(|r| r.0).unwrap_or([0; 4]);
                h().conns.push(Conn::new(hd, peer));
            },
            None => break,
        }
    }
    let now = timer::now_us();
    let mut done: Vec<usize> = Vec::new();
    let n_conns = h().conns.len();
    for i in 0..n_conns {
        let c = &mut h().conns[i];
        if let Some(t) = c.fin_at {
            if ns::tcp_unacked_h(c.h) == 0 || ns::tcp_closed_h(c.h) || now.saturating_sub(t) > 3_000_000 {
                ns::tcp_free_h(c.h);
                done.push(i);
            }
            continue;
        }
        if c.out.is_none() && c.wait.is_none() {
            let mut tmp = [0u8; 4096];
            loop {
                let n = ns::tcp_read_h(c.h, &mut tmp, 1);
                if n == 0 {
                    break;
                }
                c.buf.extend_from_slice(&tmp[..n]);
                c.last = now;
                if c.buf.len() >= MAX_REQ {
                    break;
                }
            }
            if !c.buf.is_empty() {
                let peer = c.peer;
                let buf = core::mem::take(&mut c.buf);
                match handle(&buf, peer) {
                    Handled::More => h().conns[i].buf = buf,
                    Handled::Reply(r) => h().conns[i].out = Some(r),
                    Handled::Wait(n) => h().conns[i].wait = Some(n),
                }
            }
        }
        let c = &mut h().conns[i];
        if let Some(n) = c.wait {
            let st = h().jobs.iter().find(|j| j.n == n).map(|j| j.state).unwrap_or("unknown");
            let c = &mut h().conns[i];
            if matches!(st, "done" | "error" | "interrupted" | "unknown") {
                c.wait = None;
                c.out = Some(webui_result(n));
                c.last = now;
            } else if ns::tcp_closed_h(c.h) {
                // the client gave up on its picture
                abandon(n);
                ns::tcp_free_h(c.h);
                done.push(i);
                continue;
            } else {
                continue;
            }
        }
        let c = &mut h().conns[i];
        if let Some(out) = c.out.as_ref() {
            // as much as the connection takes now -- never a wait; pumping between rounds lets the ACKs of a LAN or
            // loopback peer in so a big picture leaves in one turn
            for round in 0..8 {
                while c.sent < out.len() {
                    let n = (out.len() - c.sent).min(1400);
                    if !ns::tcp_writable_h(c.h, n) || !ns::tcp_write_h(c.h, &out[c.sent..c.sent + n]) {
                        break;
                    }
                    c.sent += n;
                    c.last = now;
                }
                if c.sent >= out.len() || round == 7 {
                    break;
                }
                ns::pump_once();
            }
            if c.sent >= out.len() {
                ns::tcp_fin_h(c.h);
                c.fin_at = Some(now);
                continue;
            }
        }
        if ns::tcp_closed_h(c.h) || now.saturating_sub(c.last) > IDLE_US {
            ns::tcp_free_h(c.h);
            done.push(i);
        }
    }
    let conns = &mut h().conns;
    for i in done.into_iter().rev() {
        conns.remove(i);
    }
}

/// Nobody waits for job `n` any more: a queued one is dropped, a running one stopped.
fn abandon(n: u64) {
    let s = h();
    if let Some(j) = s.jobs.iter_mut().find(|j| j.n == n) {
        j.orphan = true;
        if j.state == "queued" {
            j.state = "interrupted";
            j.error = "the client went away".to_string();
        }
    }
    if s.running == n {
        dfx::engine_cancel();
    }
}

// ── HTTP ───────────────────────────────────────────────────────────────────────

enum Handled {
    More,
    Reply(Vec<u8>),
    Wait(u64),
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn reply(status: u16, ctype: &str, body: &[u8], extra: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let head = format!("HTTP/1.1 {status} {reason}\r\nServer: LingOS-LingDiffusion\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n{extra}\r\n", body.len());
    let mut all = Vec::with_capacity(head.len() + body.len());
    all.extend_from_slice(head.as_bytes());
    all.extend_from_slice(body);
    all
}

fn rjson(body: &str) -> Vec<u8> {
    reply(200, "application/json; charset=utf-8", body.as_bytes(), "")
}

fn rerr(status: u16, msg: &str) -> Vec<u8> {
    let b = format!("{{\"error\":{},\"detail\":{}}}", jstr(msg), jstr(msg));
    reply(status, "application/json; charset=utf-8", b.as_bytes(), "")
}

fn query_param(query: &str, key: &str) -> String {
    for kv in query.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            if k == key {
                return url_decode(v);
            }
        }
    }
    String::new()
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hx = core::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|x| u8::from_str_radix(x, 16).ok());
                match hx {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    },
                    None => out.push(b'%'),
                }
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Constant-time-ish comparison of the access key.
fn key_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn authorized(head: &str, query: &str) -> bool {
    let key = &h().conf.key;
    if key.is_empty() {
        return true;
    }
    if key_eq(&query_param(query, "key"), key) {
        return true;
    }
    for line in head.split("\r\n").skip(1) {
        let Some((k, v)) = line.split_once(':') else { continue };
        if !k.trim().eq_ignore_ascii_case("authorization") {
            continue;
        }
        let v = v.trim();
        if let Some(t) = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")) {
            return key_eq(t.trim(), key);
        }
        if let Some(b) = v.strip_prefix("Basic ").or_else(|| v.strip_prefix("basic ")) {
            use base64::Engine;
            let dec = base64::engine::general_purpose::STANDARD.decode(b.trim().as_bytes()).unwrap_or_default();
            let cred = String::from_utf8_lossy(&dec).into_owned();
            let pass = cred.split_once(':').map(|(_, p)| p.to_string()).unwrap_or(cred);
            return key_eq(&pass, key);
        }
    }
    false
}

/// A whole request in `buf` -> what to do; More while bytes are due.
fn handle(buf: &[u8], peer: [u8; 4]) -> Handled {
    let Some(end) = find(buf, b"\r\n\r\n") else {
        return if buf.len() >= MAX_REQ { Handled::Reply(rerr(413, "request too big")) } else { Handled::More };
    };
    let head = core::str::from_utf8(&buf[..end]).unwrap_or("");
    let mut clen = 0usize;
    for line in head.split("\r\n").skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                clen = v.trim().parse().unwrap_or(0);
            }
        }
    }
    if clen > MAX_REQ {
        return Handled::Reply(rerr(413, "request too big (img2img and uploads are not served here)"));
    }
    let body_at = end + 4;
    if buf.len() < body_at + clen {
        return Handled::More;
    }
    let body = &buf[body_at..body_at + clen];
    let first = head.split("\r\n").next().unwrap_or("");
    let mut parts = first.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = path.trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };
    h().requests += 1;
    if !authorized(head, query) {
        log(&format!("{} {method} {path} 401", ip_text(peer)));
        return Handled::Reply(reply(401, "application/json; charset=utf-8", b"{\"error\":\"Unauthorized\",\"detail\":\"this LingDiffusion server needs its access key\"}", "WWW-Authenticate: Basic realm=\"LingDiffusion\"\r\n"));
    }
    let r = route(&method, path, query, body, peer);
    if !matches!(path, "/sdapi/v1/progress" | "/api/status" | "/api/preview") {
        let code = match &r {
            Handled::Reply(b) => core::str::from_utf8(&b[9..12.min(b.len())]).unwrap_or("?").to_string(),
            Handled::Wait(_) => "(waits for its picture)".to_string(),
            Handled::More => String::new(),
        };
        log(&format!("{} {method} {path} {code}", ip_text(peer)));
    }
    r
}

fn model_title(path: &str) -> String {
    crate::vfs::basename(path)
}

/// The checkpoint the next picture uses: the one chosen, else the first found.
fn current_model() -> String {
    let (chosen, loaded) = dfx::engine_models();
    if !chosen.is_empty() {
        return chosen;
    }
    if !loaded.is_empty() {
        return loaded;
    }
    dfx::find_models().into_iter().next().unwrap_or_default()
}

fn device_text() -> String {
    format!("cpu (LingOS: {})", dfx::cpu_line())
}

fn models_json() -> String {
    let list: Vec<String> = dfx::find_models()
        .iter()
        .map(|p| {
            let t = model_title(p);
            let stem = t.rsplit_once('.').map(|(a, _)| a.to_string()).unwrap_or(t.clone());
            format!("{{\"title\":{},\"model_name\":{},\"hash\":null,\"sha256\":null,\"filename\":{},\"config\":null}}", jstr(&t), jstr(&stem), jstr(&format!("/{p}")))
        })
        .collect();
    format!("[{}]", list.join(","))
}

fn samplers_json() -> String {
    let v: Vec<String> = SAMPLERS.iter().map(|s| format!("{{\"name\":{},\"aliases\":[],\"options\":{{}}}}", jstr(s))).collect();
    format!("[{}]", v.join(","))
}

fn schedulers_json() -> String {
    "[{\"name\":\"automatic\",\"label\":\"Automatic\",\"aliases\":null,\"default_rho\":-1,\"need_inner_model\":false},{\"name\":\"uniform\",\"label\":\"Uniform\",\"aliases\":null,\"default_rho\":-1,\"need_inner_model\":false},{\"name\":\"karras\",\"label\":\"Karras\",\"aliases\":null,\"default_rho\":7,\"need_inner_model\":false}]".to_string()
}

fn queue_len() -> usize {
    h().jobs.iter().filter(|j| j.state == "queued").count()
}

fn engine_json() -> String {
    let (_, loaded) = dfx::engine_models();
    let q = queue_len() + (h().running != 0) as usize;
    format!(
        "{{\"checkpoint\":{},\"arch\":\"sd15\",\"device\":{},\"vae\":\"\",\"loras\":[],\"queue\":{q},\"cuda\":false,\"edit\":null,\"server\":\"LingOS\",\"lingos_host\":{},\"txt2img_only\":true}}",
        jstr(&if loaded.is_empty() { String::new() } else { format!("/{loaded}") }),
        jstr(&device_text()),
        jstr(&h().host_id)
    )
}

fn options_json() -> String {
    format!(
        "{{\"sd_model_checkpoint\":{},\"sd_vae\":\"Automatic\",\"CLIP_stop_at_last_layers\":1,\"samples_format\":\"png\",\"outdir_txt2img_samples\":\"outputs\",\"device\":{},\"precision\":\"fp16\"}}",
        jstr(&model_title(&current_model())),
        jstr(&device_text())
    )
}

/// The checkpoint a client named (title, file name, stem or path).
fn find_checkpoint(want: &str) -> Option<String> {
    let w = want.trim().trim_start_matches('/').to_ascii_lowercase();
    // the WebUI's titles look like "name.safetensors [hash]"
    let w = w.split(" [").next().unwrap_or(&w).to_string();
    dfx::find_models().into_iter().find(|p| {
        let t = model_title(p).to_ascii_lowercase();
        let stem = t.rsplit_once('.').map(|(a, _)| a.to_string()).unwrap_or(t.clone());
        p.to_ascii_lowercase() == w || t == w || stem == w
    })
}

fn route(method: &str, path: &str, query: &str, body: &[u8], peer: [u8; 4]) -> Handled {
    let get = method == "GET" || method == "HEAD";
    let post = method == "POST";
    Handled::Reply(match path {
        "/" if get => reply(200, "text/html; charset=utf-8", status_page().as_bytes(), ""),
        "/health" | "/internal/ping" if get => reply(200, "text/plain", b"ok", ""),
        "/sdapi/v1/options" if get => rjson(&options_json()),
        "/sdapi/v1/options" if post => {
            let j: J = serde_json::from_slice(body).unwrap_or(J::Null);
            if let Some(want) = j.get("sd_model_checkpoint").and_then(|v| v.as_str()) {
                match find_checkpoint(want) {
                    Some(p) => dfx::engine_set_model(&p),
                    None => return Handled::Reply(rerr(404, &format!("checkpoint not found: {want}"))),
                }
            }
            rjson("null")
        },
        "/sdapi/v1/sd-models" if get => rjson(&models_json()),
        "/sdapi/v1/refresh-checkpoints" if post => rjson("null"),
        "/sdapi/v1/samplers" if get => rjson(&samplers_json()),
        "/sdapi/v1/schedulers" if get => rjson(&schedulers_json()),
        "/sdapi/v1/loras" | "/sdapi/v1/sd-vae" | "/sdapi/v1/upscalers" | "/sdapi/v1/hypernetworks" | "/sdapi/v1/embeddings" if get => {
            rjson(if path.ends_with("embeddings") { "{\"loaded\":{},\"skipped\":{}}" } else { "[]" })
        },
        "/sdapi/v1/memory" if get => {
            let free = dfx::free_ram_bytes();
            rjson(&format!("{{\"ram\":{{\"free\":{free}}},\"cuda\":{{\"error\":\"no CUDA: this server renders on the CPU (LingOS)\"}}}}"))
        },
        "/sdapi/v1/progress" if get => rjson(&webui_progress(query_param(query, "skip_current_image") == "true")),
        "/sdapi/v1/interrupt" | "/sdapi/v1/skip" if post => {
            let r = h().running;
            if r != 0 {
                abandon_running(r);
            }
            rjson("{}")
        },
        "/sdapi/v1/txt2img" if post => match new_job(body, peer) {
            Ok(n) => return Handled::Wait(n),
            Err((code, e)) => rerr(code, &e),
        },
        "/sdapi/v1/img2img" | "/sdapi/v1/extra-single-image" | "/sdapi/v1/extra-batch-images" if post => {
            rerr(422, "this LingOS server makes pictures from text only (txt2img)")
        },
        "/sdapi/v1/unload-checkpoint" | "/api/unload" if post => {
            if h().running == 0 {
                dfx::engine_unload();
            }
            rjson(if path.starts_with("/api") { "{\"ok\":true}" } else { "{}" })
        },
        "/api/engine" if get => rjson(&engine_json()),
        "/api/models" if get => {
            rjson(&format!(
                "{{\"models\":{},\"loras\":[],\"vaes\":[],\"upscalers\":[],\"current\":{},\"vae\":\"\",\"samplers\":{},\"schedulers\":{},\"engine\":{}}}",
                models_json(),
                jstr(&current_model()),
                samplers_json(),
                schedulers_json(),
                engine_json()
            ))
        },
        "/api/models/select" if post => {
            let j: J = serde_json::from_slice(body).unwrap_or(J::Null);
            let want = j.get("checkpoint").and_then(|v| v.as_str()).unwrap_or("");
            match find_checkpoint(want) {
                Some(p) => {
                    dfx::engine_set_model(&p);
                    rjson(&format!("{{\"job\":\"\",\"checkpoint\":{}}}", jstr(&format!("/{p}"))))
                },
                None => rerr(404, &format!("checkpoint not found: {want}")),
            }
        },
        "/api/generate" if post => {
            let mode = query_param(query, "mode");
            if !mode.is_empty() && mode != "txt2img" {
                rerr(400, &format!("this LingOS server makes pictures from text only (mode {mode} is not served)"))
            } else {
                match new_job(body, peer) {
                    Ok(n) => {
                        let id = h().jobs.iter().find(|j| j.n == n).map(|j| j.id.clone()).unwrap_or_default();
                        rjson(&format!("{{\"job\":{},\"outputs_root\":\"outputs\"}}", jstr(&id)))
                    },
                    Err((code, e)) => rerr(code, &e),
                }
            }
        },
        "/api/status" if get => rjson(&status_json(&query_param(query, "job"))),
        "/api/preview" if get => {
            let id = query_param(query, "job");
            let img = match h().jobs.iter().find(|j| j.id == id) {
                Some(j) if j.n == h().running => preview_b64(),
                _ => String::new(),
            };
            rjson(&format!("{{\"image\":{}}}", jstr(&img)))
        },
        "/api/interrupt" | "/api/skip" if post => {
            let id = query_param(query, "job");
            let n = if id.is_empty() { h().running } else { h().jobs.iter().find(|j| j.id == id).map(|j| j.n).unwrap_or(0) };
            if n != 0 {
                abandon_running(n);
            }
            rjson("{\"ok\":true}")
        },
        "/api/gallery" if get => rjson("{\"images\":[],\"total\":0,\"page\":0,\"per\":60}"),
        p if get && p.starts_with("/outputs/") => {
            let name = url_decode(&p["/outputs/".len()..]);
            let id = name.trim_end_matches(".png");
            match h().jobs.iter().find(|j| j.id == id && !j.png.is_empty()) {
                Some(j) => reply(200, "image/png", &j.png, ""),
                None => rerr(404, "no such picture (only the newest few are kept)"),
            }
        },
        _ if get || post => rerr(404, "Not Found"),
        _ => rerr(405, "Method Not Allowed"),
    })
}

/// Interrupt hosted job `n`: queued -> dropped, running -> the engine stops after its current round.
fn abandon_running(n: u64) {
    let s = h();
    if let Some(j) = s.jobs.iter_mut().find(|j| j.n == n) {
        if j.state == "queued" {
            j.state = "interrupted";
            j.error = "interrupted".to_string();
        }
    }
    if s.running == n {
        dfx::engine_cancel();
    }
}

/// A txt2img request (the WebUI's body) -> a queued job.
fn new_job(body: &[u8], peer: [u8; 4]) -> Result<u64, (u16, String)> {
    if !h().conf.on {
        return Err((503, "the server is switched off".to_string()));
    }
    let j: J = serde_json::from_slice(body).map_err(|_| (422, "the request body must be a JSON object".to_string()))?;
    if !j.is_object() {
        return Err((422, "the request body must be a JSON object".to_string()));
    }
    if j.get("init_images").and_then(|v| v.as_array()).is_some_and(|a| !a.is_empty()) {
        return Err((422, "this LingOS server makes pictures from text only (txt2img)".to_string()));
    }
    if queue_len() >= MAX_QUEUE {
        return Err((503, format!("the queue is full ({MAX_QUEUE} pictures waiting): try again later")));
    }
    if dfx::find_models().is_empty() && dfx::engine_models().1.is_empty() {
        return Err((503, "this LingOS machine has no Stable Diffusion 1.5 checkpoint (models/ on a USB stick or the data disk)".to_string()));
    }
    let gs = |k: &str| j.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let gn = |k: &str, d: f64| j.get(k).and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).filter(|v| v.is_finite()).unwrap_or(d);
    let mut sampler = gs("sampler_name");
    if sampler.is_empty() {
        sampler = gs("sampler_index");
    }
    let sched = gs("scheduler").to_ascii_lowercase();
    let mut sampler = SAMPLERS.iter().find(|s| s.eq_ignore_ascii_case(sampler.trim())).map(|s| s.to_string()).unwrap_or_else(|| {
        // close enough: the WebUI's other DPM++ / Euler names
        let l = sampler.to_ascii_lowercase();
        if l.contains("dpm") { "DPM++ 2M Karras".to_string() } else if l.starts_with("euler") && !l.contains(" a") { "Euler".to_string() } else { "Euler a".to_string() }
    });
    if sched == "karras" && !sampler.ends_with("Karras") {
        sampler.push_str(" Karras");
    } else if sched == "uniform" && sampler.ends_with(" Karras") {
        sampler.truncate(sampler.len() - " Karras".len());
    }
    if let Some(m) = j.pointer("/override_settings/sd_model_checkpoint").and_then(|v| v.as_str()) {
        if let Some(p) = find_checkpoint(m) {
            dfx::engine_set_model(&p);
        }
    }
    let snap = |v: f64| ((libm::round(v.clamp(64.0, 1024.0) / 64.0) * 64.0) as usize).clamp(64, 1024);
    let s = h();
    let n = s.next_n;
    s.next_n += 1;
    let id = format!("lingos-{}-{n}", &s.host_id[..6]);
    s.jobs.push(Job {
        n,
        id,
        peer,
        prompt: gs("prompt"),
        negative: gs("negative_prompt"),
        sampler,
        steps: gn("steps", 20.0).clamp(1.0, 150.0) as usize,
        cfg: gn("cfg_scale", 7.0).clamp(1.0, 30.0) as f32,
        w: snap(gn("width", 512.0)),
        h: snap(gn("height", 512.0)),
        seed: { let v = gn("seed", -1.0); if v < 0.0 { -1 } else { v as i64 } },
        params: j,
        state: "queued",
        error: String::new(),
        png: Vec::new(),
        info: String::new(),
        seed_used: 0,
        t_start: 0,
        t_end: 0,
        orphan: false,
    });
    // old jobs go
    while s.jobs.len() > KEEP_JOBS {
        let Some(k) = s.jobs.iter().position(|j| matches!(j.state, "done" | "error" | "interrupted")) else { break };
        s.jobs.remove(k);
    }
    Ok(n)
}

fn preview_b64() -> String {
    use base64::Engine;
    let known = h().preview.0;
    match dfx::engine_preview_png(known) {
        Some((id, png)) if id != known => {
            let b = base64::engine::general_purpose::STANDARD.encode(&png);
            h().preview = (id, b.clone());
            b
        },
        Some(_) => h().preview.1.clone(),
        None => String::new(),
    }
}

/// (state, step, steps, progress, eta s, elapsed s, message) of job `j`.
fn job_progress(j: &Job) -> (&'static str, usize, usize, f64, f64, f64, String) {
    let now = timer::now_ms();
    if j.n == h().running {
        let (ph, step, steps, frac, msg) = dfx::engine_progress();
        let state = if ph == "loading" {
            "loading"
        } else if frac >= 0.955 {
            "decoding"
        } else {
            "running"
        };
        let el = now.saturating_sub(j.t_start) as f64 / 1000.0;
        let p = (frac as f64).clamp(0.0, 0.999);
        let eta = if p > 0.01 && state != "loading" { el / p - el } else { 0.0 };
        return (state, step, steps.max(1), p, eta, el, msg);
    }
    let el = if j.t_start > 0 { j.t_end.max(j.t_start).saturating_sub(j.t_start) as f64 / 1000.0 } else { 0.0 };
    match j.state {
        "done" => ("done", j.steps, j.steps, 1.0, 0.0, el, "done".to_string()),
        "queued" => ("queued", 0, j.steps, 0.0, 0.0, 0.0, "waiting in the queue".to_string()),
        s => (if s == "interrupted" { "interrupted" } else { "error" }, 0, j.steps, if s == "interrupted" { 1.0 } else { 0.0 }, 0.0, el, j.error.clone()),
    }
}

fn status_json(id: &str) -> String {
    let s = h();
    let Some(j) = s.jobs.iter().find(|j| j.id == id) else { return format!("{{\"id\":{},\"state\":\"unknown\"}}", jstr(id)) };
    let (state, step, steps, prog, eta, el, msg) = job_progress(j);
    let pos = if j.state == "queued" { s.jobs.iter().filter(|x| x.state == "queued" && x.n <= j.n).count() } else { 0 };
    let (images, seeds, infos) = if state == "done" {
        (format!("[{}]", jstr(&format!("outputs/{}.png", j.id))), format!("[{}]", j.seed_used), format!("[{}]", jstr(&j.info)))
    } else {
        ("[]".to_string(), "[]".to_string(), "[]".to_string())
    };
    format!(
        "{{\"id\":{},\"state\":\"{state}\",\"message\":{},\"queue\":{pos},\"step\":{step},\"steps\":{steps},\"batch\":0,\"batches\":1,\"progress\":{prog},\"elapsed\":{el},\"eta\":{eta},\"images\":{images},\"seeds\":{seeds},\"infotexts\":{infos},\"notes\":[],\"error\":{},\"preview_n\":0,\"model\":{}}}",
        jstr(&j.id),
        jstr(&msg),
        jstr(if state == "error" || state == "interrupted" { &j.error } else { "" }),
        jstr(&model_title(&current_model()))
    )
}

fn webui_progress(skip_image: bool) -> String {
    let s = h();
    let running = s.running;
    let Some(j) = s.jobs.iter().find(|j| j.n == running && running != 0) else {
        // nothing of ours runs (the engine may be busy with this machine's own picture: that stays private)
        return format!("{{\"progress\":0.0,\"eta_relative\":0.0,\"state\":{{\"job\":\"\",\"job_count\":{},\"sampling_step\":0,\"sampling_steps\":0,\"interrupted\":false,\"skipped\":false}},\"current_image\":null,\"textinfo\":null}}", queue_len());
    };
    let (_, step, steps, prog, eta, _, msg) = job_progress(j);
    let id = j.id.clone();
    let img = if skip_image { String::new() } else { preview_b64() };
    format!(
        "{{\"progress\":{prog},\"eta_relative\":{eta},\"state\":{{\"job\":{},\"job_count\":1,\"job_no\":0,\"sampling_step\":{step},\"sampling_steps\":{steps},\"interrupted\":false,\"skipped\":false}},\"current_image\":{},\"textinfo\":{}}}",
        jstr(&id),
        if img.is_empty() { "null".to_string() } else { jstr(&img) },
        jstr(&msg)
    )
}

/// The WebUI's txt2img answer for finished job `n`.
fn webui_result(n: u64) -> Vec<u8> {
    use base64::Engine;
    let s = h();
    let Some(j) = s.jobs.iter().find(|j| j.n == n) else { return rerr(500, "the job went away") };
    match j.state {
        "done" => {},
        "interrupted" => return rerr(500, "interrupted"),
        _ => return rerr(500, &if j.error.is_empty() { "the picture could not be made".to_string() } else { j.error.clone() }),
    }
    let info = format!(
        "{{\"prompt\":{p},\"all_prompts\":[{p}],\"negative_prompt\":{np},\"all_negative_prompts\":[{np}],\"seed\":{seed},\"all_seeds\":[{seed}],\"subseed\":-1,\"width\":{w},\"height\":{h},\"sampler_name\":{sm},\"cfg_scale\":{cfg},\"steps\":{st},\"batch_size\":1,\"sd_model_name\":{m},\"infotexts\":[{it}],\"job_timestamp\":\"\",\"clip_skip\":1}}",
        p = jstr(&j.prompt),
        np = jstr(&j.negative),
        seed = j.seed_used,
        w = j.w,
        h = j.h,
        sm = jstr(&j.sampler),
        cfg = j.cfg,
        st = j.steps,
        m = jstr(&model_title(&current_model())),
        it = jstr(&j.info)
    );
    let body = format!("{{\"images\":[\"{}\"],\"parameters\":{},\"info\":{}}}", base64::engine::general_purpose::STANDARD.encode(&j.png), j.params, jstr(&info));
    rjson(&body)
}

fn html_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn status_page() -> String {
    let s = h();
    let (_, loaded) = dfx::engine_models();
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>LingDiffusion on LingOS</title>\
<style>body{{font:15px system-ui,sans-serif;background:#1b1b1b;color:#ddd;max-width:760px;margin:24px auto;padding:0 16px}}h1{{color:#f7941e;font-weight:600}}code{{background:#2c2c2c;padding:1px 5px;border-radius:4px}}td{{padding:3px 10px 3px 0;vertical-align:top}}</style></head><body>\
<h1>LingDiffusion on LingOS</h1><p>Stable Diffusion 1.5 on this machine's CPU ({}), served over the AUTOMATIC1111 WebUI API and LingDiffusion's queued API. Text to image only.</p>\
<table><tr><td>checkpoint</td><td>{}</td></tr><tr><td>loaded</td><td>{}</td></tr><tr><td>queue</td><td>{} waiting, {} running</td></tr><tr><td>served</td><td>{} pictures</td></tr></table>\
<h3>API</h3><p><code>POST /sdapi/v1/txt2img</code> <code>GET /sdapi/v1/progress</code> <code>POST /sdapi/v1/interrupt</code> <code>GET /sdapi/v1/sd-models</code> <code>GET /sdapi/v1/samplers</code> <code>GET|POST /sdapi/v1/options</code><br>\
<code>POST /api/generate?mode=txt2img</code> <code>GET /api/status?job=ID</code> <code>GET /api/preview?job=ID</code> <code>POST /api/interrupt?job=ID</code> <code>GET /api/engine</code> <code>GET /outputs/ID.png</code></p>\
<p>From another LingOS: LingDiffusion &gt; Settings &gt; GPU server address <code>this-machine:{}</code> (or Search network).</p></body></html>",
        html_esc(&dfx::cpu_line()),
        html_esc(&model_title(&current_model())),
        html_esc(if loaded.is_empty() { "nothing yet (the first picture loads it)" } else { &loaded }),
        queue_len(),
        (s.running != 0) as u8,
        s.served,
        s.conf.port
    )
}

// ── what the app and the console see ───────────────────────────────────────────

/// Where to reach it: "http://192.0.2.10:7860, http://127.0.0.1:7860".
fn urls() -> String {
    let s = h();
    if s.bound_port == 0 {
        return String::new();
    }
    let p = s.bound_port;
    let me = ns::self_ip();
    let mut v: Vec<String> = Vec::new();
    let ip = s.bind_ip;
    if ip == [0; 4] {
        if me != [0; 4] {
            v.push(format!("http://{}:{p}", ip_text(me)));
        }
        v.push(format!("http://127.0.0.1:{p}"));
    } else if ip[0] == 127 {
        v.push(format!("http://127.0.0.1:{p}"));
    } else if ip == me {
        v.push(format!("http://{}:{p}", ip_text(me)));
        let a = s.conf.addr.to_ascii_lowercase();
        if parse_ip(&a).is_none() {
            v.push(format!("http://{a}:{p}"));
        }
    }
    v.join(", ")
}

fn stats() -> String {
    let s = h();
    let open = s.conns.iter().filter(|c| c.out.is_some() || c.wait.is_some() || !c.buf.is_empty()).count();
    let running = s.jobs.iter().find(|j| j.n == s.running && s.running != 0).map(|j| format!(", making one for {}", ip_text(j.peer))).unwrap_or_default();
    format!("{} pictures served, {} waiting{running}; {} requests, {} connected", s.served, queue_len(), s.requests, open)
}

/// `diffusion serve [on [addr[:port]] [key=K] | off | status]` (sshcmds).
pub fn command(arg: &str) -> String {
    load_conf();
    let mut w = arg.split_whitespace();
    let sub = w.next().unwrap_or("status");
    let status = || {
        let s = h();
        let c = &s.conf;
        let mut t = format!(
            "diffusion serve: {} -- configured {}:{} ({}){}\n",
            s.state,
            c.addr,
            c.port,
            if c.on { "on" } else { "off" },
            if c.key.is_empty() { ", no access key" } else { ", access key set" }
        );
        if !s.note.is_empty() {
            t.push_str(&format!("  {}\n", s.note));
        }
        let u = urls();
        if !u.is_empty() {
            t.push_str(&format!("  at {u}\n"));
        }
        t.push_str(&format!("  {}\n", stats()));
        t
    };
    match sub {
        "status" | "" => status(),
        "off" => {
            let c = h().conf.clone();
            match configure(false, &c.addr, c.port, &c.key) {
                Ok(()) => String::from("diffusion serve: switched off\n"),
                Err(e) => format!("diffusion serve: {e}\n"),
            }
        },
        "on" => {
            let c = h().conf.clone();
            let (mut addr, mut port, mut key) = (c.addr.clone(), c.port, c.key.clone());
            for a in w {
                if let Some(k) = a.strip_prefix("key=") {
                    key = k.to_string();
                } else {
                    match parse_listen(a, port) {
                        Ok((ad, p)) => {
                            // "on 7861" keeps the address
                            if !a.chars().all(|ch| ch.is_ascii_digit()) {
                                addr = ad;
                            }
                            port = p;
                        },
                        Err(e) => return format!("diffusion serve: {e}\n"),
                    }
                }
            }
            match configure(true, &addr, port, &key) {
                Ok(()) => format!("diffusion serve: on, {addr}:{port}\n  {}\n  (`diffusion serve` shows how it went)\n", describe(&addr, port)),
                Err(e) => format!("diffusion serve: {e}\n"),
            }
        },
        _ => String::from("usage: diffusion serve [on [addr[:port]] [key=K] | off | status]\n  addr: 127.0.0.1 (this machine only, the default), 0.0.0.0 (the whole network), this machine's IP, or a name; port 1-65535 (7860)\n"),
    }
}

/// The `diffusion_host_*` builtins (the app's Settings).
pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    load_conf();
    let sarg = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let narg = |i: usize, d: f64| args.get(i).map(|&v| to_f64(v)).filter(|v| v.is_finite()).unwrap_or(d);
    Some(match name {
        // diffusion_host_set(on, addr, port, key) -> "" when taken, else why not
        b"diffusion_host_set" => {
            let on = args.first().map(|&v| crate::lingrt::truthy(v)).unwrap_or(false);
            let p = narg(2, DEFAULT_PORT as f64);
            if !(1.0..=65535.0).contains(&p) || libm::floor(p) != p {
                return Some(mk_str("the port must be a whole number from 1 to 65535"));
            }
            match configure(on, &sarg(1), p as u16, &sarg(3)) {
                Ok(()) => mk_str(""),
                Err(e) => mk_str(&e),
            }
        },
        b"diffusion_host_on" => boolean(h().conf.on),
        b"diffusion_host_addr" => mk_str(&h().conf.addr),
        b"diffusion_host_port" => num(h().conf.port as f64),
        b"diffusion_host_key" => mk_str(&h().conf.key),
        // off | starting | listening | waiting | error
        b"diffusion_host_state" => mk_str(h().state),
        b"diffusion_host_info" => mk_str(&h().note),
        b"diffusion_host_warn" => boolean(h().warn && h().conf.on),
        b"diffusion_host_urls" => mk_str(&urls()),
        b"diffusion_host_stats" => mk_str(&stats()),
        // what an address would mean (no lookup), for the field while it is typed
        b"diffusion_host_describe" => mk_str(&describe(&sarg(0), narg(1, DEFAULT_PORT as f64).clamp(1.0, 65535.0) as u16)),
        b"diffusion_host_new_key" => mk_str(&new_id()),
        _ => return None,
    })
}
