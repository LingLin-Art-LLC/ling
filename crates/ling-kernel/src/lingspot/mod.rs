//! lingspot: Spotify for Horizon on LingOS, in the kernel -- what the PC's
//! `ling-spotify` helper (librespot 0.8) does, so Horizon's spotify.ling runs
//! unchanged. Horizon "starts the helper" (plat_os.ling's p_proc_start ->
//! `spotify_engine_start(port, key, name)`) and opens
//! `ws://127.0.0.1:<port>/<key>`; lingsock.rs hands that WebSocket to the
//! engine's in-kernel queues instead of TCP. The messages both ways are the
//! helper's JSON:
//!
//!   in:  {"op":"discover"} | {"op":"login","creds":"..."} | {"op":"api",id,method,path,body}
//!        | {"op":"local","cmd":...} | {"op":"logout"}
//!   out: {"op":"status",state,why,user,device_id,name,local:{...}} twice a second,
//!        {"op":"creds","creds":"..."} once after signing in, {"op":"api",id,code,body}
//!
//! Signing in: the kept credentials, or zeroconf (zeroconf.rs: mDNS + getInfo /
//! addUser), then the access point (ap.rs: DH, Shannon, APWelcome). The Web
//! API token comes from login5 with the session's stored credential (web.rs);
//! the token never leaves the kernel. The engine runs on green threads under
//! the app that started it (Horizon), and ends with it, or when Horizon's
//! socket closes, or 30 s after starting with no one connected -- as the helper.

pub mod ap;
pub mod connect;
pub mod partner;
pub mod pb;
pub mod player;
pub mod shannon;
pub mod web;
pub mod zeroconf;

use crate::lingrt::{mk_str, num, to_f64, to_string};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use base64::Engine as _;
use serde_json::json;

pub fn log(m: &str) {
    crate::console_write(b"lingspot: ");
    crate::console_write(m.as_bytes());
    crate::console_write(b"\n");
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

pub(crate) fn now() -> u64 {
    crate::arch::timer::now_ms()
}

pub struct Engine {
    pub id: i64,
    port: u16,
    key: String,
    name: String,
    pub device_id: String,
    coro: u64,
    born: u64,
    /// Horizon's socket: attached, and whether it closed again
    attached: bool,
    detached: bool,
    killed: bool,
    pub exit: Option<i32>,
    inq: VecDeque<String>,
    outq: VecDeque<String>,
    last_status: u64,
    state: String,
    why: String,
    user: String,
    busy: bool,
    zc: Option<zeroconf::Server>,
    pub ap: Option<ap::Ap>,
    /// the reusable credentials: username, auth type, auth data
    creds: Option<(String, u32, Vec<u8>)>,
    pub client_token: String,
    token: String,
    token_until: u64,
    token_busy: bool,
    spclient: String,
    pub key_seq: u32,
    pub keys: Vec<(u32, Option<[u8; 16]>)>,
    pong_at: u64,
    pub local: player::Local,
    pub player: player::Player,
    /// Spotify Connect (connect.rs): the dealer socket, this device in connect-state, the cluster
    pub conn: connect::Conn,
}

static mut ENGINES: Vec<Box<Engine>> = Vec::new();
static mut NEXT: i64 = 1;

fn engines() -> &'static mut Vec<Box<Engine>> {
    unsafe { &mut *(&raw mut ENGINES) }
}

pub fn eng(id: i64) -> Option<&'static mut Engine> {
    engines().iter_mut().find(|e| e.id == id && e.exit.is_none()).map(|b| &mut **b)
}

fn eng_any(id: i64) -> Option<&'static mut Engine> {
    engines().iter_mut().find(|e| e.id == id).map(|b| &mut **b)
}

fn random_device_id() -> String {
    let mut b = [0u8; 20];
    crate::crypto::random_bytes(&mut b);
    hex(&b)
}

// ── starting and stopping (plat_os.ling's p_proc_*) ──

fn start(port: u16, key: String, name: String) -> i64 {
    if key.len() < 16 {
        log("the key must be at least 16 characters");
        return -1;
    }
    // one per port (the PC helper's port-taken exit is code 3)
    engines().retain(|e| e.exit.is_none() || now().saturating_sub(e.born) < 600_000);
    if engines().iter().any(|e| e.exit.is_none() && e.port == port) {
        return -1;
    }
    let id = unsafe {
        NEXT += 1;
        NEXT
    };
    let e = Box::new(Engine {
        id,
        port,
        key,
        name: if name.is_empty() { String::from("Horizon") } else { name },
        device_id: random_device_id(),
        coro: 0,
        born: now(),
        attached: false,
        detached: false,
        killed: false,
        exit: None,
        inq: VecDeque::new(),
        outq: VecDeque::new(),
        last_status: 0,
        state: String::from("idle"),
        why: String::new(),
        user: String::new(),
        busy: false,
        zc: None,
        ap: None,
        creds: None,
        client_token: String::new(),
        token: String::new(),
        token_until: 0,
        token_busy: false,
        spclient: String::new(),
        key_seq: 0,
        keys: Vec::new(),
        pong_at: 0,
        local: player::Local { volume: 32768, ..Default::default() },
        player: player::Player::default(),
        conn: connect::Conn::default(),
    });
    engines().push(e);
    let c = crate::coro::spawn("lingspot", false, Box::new(move || run(id)));
    match eng(id) {
        Some(e) if c != 0 => e.coro = c,
        _ => {
            engines().retain(|e| e.id != id);
            return -1;
        },
    }
    log(&format!("engine {id} on port {port}"));
    id
}

/// -2 while it runs, else its exit code (0)
fn poll(id: i64) -> i64 {
    match eng_any(id) {
        None => 1,
        Some(e) => match e.exit {
            Some(c) => c as i64,
            // its green thread was dropped (the app that started it quit)
            None if e.coro != 0 && !crate::coro::alive(e.coro) => {
                finish(e);
                0
            },
            None => -2,
        },
    }
}

fn kill(id: i64) {
    if let Some(e) = eng(id) {
        e.killed = true;
        if e.coro == 0 || !crate::coro::alive(e.coro) {
            finish(e);
        }
    }
}

/// Playback stops, the session closes, the announcement ends.
fn finish(e: &mut Engine) {
    if e.exit.is_some() {
        return;
    }
    player::stop(e);
    connect::stop(e);
    partner::forget(e.id);
    if let Some(mut z) = e.zc.take() {
        z.stop();
    }
    if let Some(mut a) = e.ap.take() {
        a.close();
    }
    e.token.clear();
    e.creds = None;
    e.exit = Some(0);
    log(&format!("engine {} ended", e.id));
}

// ── the WebSocket bridge (lingsock.rs) ──

/// `ws://127.0.0.1:<port>/<key>` of a running engine -> its id (once: a second
/// connection is refused, as the helper does).
pub fn ws_attach(url: &str) -> Option<i64> {
    let rest = url.strip_prefix("ws://127.0.0.1:").or_else(|| url.strip_prefix("ws://localhost:"))?;
    let (port, path) = rest.split_once('/')?;
    let port: u16 = port.parse().ok()?;
    let path = path.split('?').next().unwrap_or("");
    let e = engines().iter_mut().find(|e| e.exit.is_none() && e.port == port)?;
    if path != e.key || e.attached {
        return Some(-1);
    }
    e.attached = true;
    Some(e.id)
}

pub fn ws_to(id: i64, text: &str) -> bool {
    match eng(id) {
        Some(e) if !e.detached => {
            if e.inq.len() < 256 {
                e.inq.push_back(String::from(text));
            }
            true
        },
        _ => false,
    }
}

pub fn ws_from(id: i64) -> Option<String> {
    let e = eng(id)?;
    if let Some(m) = e.outq.pop_front() {
        return Some(m);
    }
    if now().saturating_sub(e.last_status) >= 500 {
        e.last_status = now();
        return Some(status_json(e));
    }
    None
}

pub fn ws_alive(id: i64) -> bool {
    poll(id) == -2 && eng(id).is_some_and(|e| !e.detached)
}

pub fn ws_detach(id: i64) {
    if let Some(e) = eng(id) {
        e.detached = true;
    }
}

fn status_json(e: &Engine) -> String {
    let l = &e.local;
    json!({
        "op": "status", "state": e.state, "why": e.why, "user": e.user,
        "device_id": e.device_id, "name": e.name,
        "local": {
            "active": l.active, "playing": l.playing, "uri": l.uri, "name": l.name, "artists": l.artists,
            "album": l.album, "cover": l.cover, "duration": l.duration, "pos": l.now_ms(),
            "volume": (l.volume as u32 * 100 + 32767) / 65535,
        },
        "connect": connect::status(e),
    })
    .to_string()
}

fn set(e: &mut Engine, state: &str, why: &str) {
    e.state = String::from(state);
    e.why = String::from(why);
    // a state change goes out at once
    e.last_status = 0;
}

// ── the engine's green thread ──

fn run(id: i64) {
    loop {
        let Some(e) = eng(id) else { return };
        if e.killed || e.detached || (!e.attached && now().saturating_sub(e.born) > 30_000) {
            finish(e);
            return;
        }
        while let Some(m) = e.inq.pop_front() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&m) else { continue };
            match v["op"].as_str().unwrap_or("") {
                "logout" => {
                    finish(e);
                    return;
                },
                "discover" | "login" => {
                    if e.ap.is_some() || e.busy {
                        continue;
                    }
                    let creds = v["creds"].as_str().unwrap_or("").to_string();
                    if creds.is_empty() {
                        set(e, "discovering", "");
                        if e.zc.is_none() {
                            e.zc = Some(zeroconf::Server::new(&e.device_id, &e.name));
                            log(&format!("zeroconf: \"{}\" on port {} (mDNS _spotify-connect._tcp)", e.name, zeroconf::HTTP_PORT));
                        }
                    } else {
                        match parse_creds(&creds) {
                            Some((u, t, a)) => sign_in(id, u, t, a),
                            None => set(e, "error", "the kept sign-in could not be read; sign in again from your phone"),
                        }
                    }
                },
                "api" => {
                    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
                    let (rid, method, path, body) = (s("id"), s("method"), s("path"), s("body"));
                    let _ = crate::coro::spawn(
                        "lingspot_api",
                        false,
                        Box::new(move || {
                            let (code, out) = api(id, &method, &path, &body);
                            if let Some(e) = eng(id) {
                                e.outq.push_back(json!({"op": "api", "id": rid, "code": code, "body": out}).to_string());
                            }
                        }),
                    );
                },
                "local" => {
                    player::command(e, &v);
                    // Horizon played something itself: it is the account's active device now
                    if matches!(v["cmd"].as_str(), Some("load") | Some("play")) {
                        connect::local_play_started(e);
                    }
                },
                _ => {},
            }
            // sign_in ran the green thread through network waits: look again
            if eng(id).is_none() {
                return;
            }
        }
        let Some(e) = eng(id) else { return };
        // a phone handed over the sign-in
        let got = e.zc.as_mut().and_then(|z| z.tick());
        if let Some(c) = got {
            log(&format!("zeroconf: credentials for {} from a phone", c.username));
            if let Some(mut z) = e.zc.take() {
                z.stop();
            }
            sign_in(id, c.username, c.auth_type, c.auth_data);
        }
        let Some(e) = eng(id) else { return };
        session_tick(e);
        player::tick(e);
        connect::tick(e);
        crate::coro::sleep_ms(40);
    }
}

/// librespot's serde form: {"username":..,"auth_type":1,"auth_data":"<base64>"}
fn parse_creds(t: &str) -> Option<(String, u32, Vec<u8>)> {
    let v: serde_json::Value = serde_json::from_str(t).ok()?;
    let u = v["username"].as_str().unwrap_or("").to_string();
    let typ = v["auth_type"].as_u64()? as u32;
    let data = v["auth_data"].as_str().or_else(|| v["encoded_auth_blob"].as_str())?;
    Some((u, typ, b64().decode(data).ok()?))
}

fn creds_json(u: &str, t: u32, a: &[u8]) -> String {
    json!({"username": u, "auth_type": t, "auth_data": b64().encode(a)}).to_string()
}

fn sign_in(id: i64, user: String, typ: u32, auth: Vec<u8>) {
    let Some(e) = eng(id) else { return };
    e.busy = true;
    set(e, "connecting", "");
    let did = e.device_id.clone();
    let r = ap::sign_in(&user, typ, &auth, &did, &mut |m| log(m));
    let Some(e) = eng(id) else { return };
    e.busy = false;
    match r {
        Ok((a, ap::Login::Welcome(name, rt, ra))) => {
            log(&format!("signed in as {name} at {}", a.host));
            e.ap = Some(a);
            e.user = name.clone();
            e.creds = Some((name.clone(), rt, ra.clone()));
            e.pong_at = 0;
            e.outq.push_back(json!({"op": "creds", "creds": creds_json(&name, rt, &ra)}).to_string());
            set(e, "ready", "");
        },
        Ok((mut a, ap::Login::Failed(code, desc))) => {
            a.close();
            log(&format!("login failed: {} {desc}", ap::error_name(code)));
            let why = match code {
                11 => String::from("Playing in Horizon needs Spotify Premium."),
                12 | 13 => String::from("Spotify did not accept the sign-in. Pick Horizon from the Spotify app again."),
                _ => format!("could not sign in: {}", ap::error_name(code)),
            };
            set(e, "error", &why);
        },
        Err(why) => set(e, "error", &format!("could not sign in: {why}")),
    }
}

/// The access point's packets: pings answered, audio keys handed to the player.
fn session_tick(e: &mut Engine) {
    let Some(a) = e.ap.as_mut() else { return };
    a.poll();
    let mut lost = a.st.closed;
    loop {
        match a.take() {
            Ok(Some((cmd, data))) => match cmd {
                ap::CMD_PING => e.pong_at = now() + 1000,
                ap::CMD_AES_KEY | ap::CMD_AES_KEY_ERROR if data.len() >= 4 => {
                    let seq = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                    let k = if cmd == ap::CMD_AES_KEY && data.len() >= 20 {
                        let mut k = [0u8; 16];
                        k.copy_from_slice(&data[4..20]);
                        Some(k)
                    } else {
                        None
                    };
                    e.keys.push((seq, k));
                },
                ap::CMD_COUNTRY => log(&format!("country {}", String::from_utf8_lossy(&data))),
                _ => {},
            },
            Ok(None) => break,
            Err(why) => {
                log(&why);
                lost = true;
                break;
            },
        }
    }
    if e.pong_at != 0 && now() >= e.pong_at {
        e.pong_at = 0;
        if let Some(a) = e.ap.as_mut() {
            if !a.send(ap::CMD_PONG, &[0, 0, 0, 0]) {
                lost = true;
            }
        }
    }
    if lost {
        if let Some(mut a) = e.ap.take() {
            a.close();
        }
        set(e, "lost", "the connection to Spotify ended");
    }
}

// ── the Web API on the session's token ──

/// The access token (login5 with the stored credential), fetched once and kept until it expires.
pub fn token(id: i64) -> Result<String, String> {
    loop {
        let e = eng(id).ok_or("the engine stopped")?;
        if !e.token.is_empty() && now() < e.token_until {
            return Ok(e.token.clone());
        }
        if !e.token_busy {
            break;
        }
        crate::coro::sleep_ms(50);
    }
    let e = eng(id).ok_or("the engine stopped")?;
    let Some((user, _, data)) = e.creds.clone() else { return Err(String::from("not signed in")) };
    e.token_busy = true;
    let did = e.device_id.clone();
    let r = (|| {
        let ct = web::client_token(&did)?;
        if let Some(e) = eng(id) {
            e.client_token = ct.clone();
        }
        match web::login5(&ct, &did, &user, &data)? {
            web::Login5::Ok(tok, secs, _) => Ok((tok, secs)),
            web::Login5::Error(c) => Err(format!("login5: {}", web::login5_error(c))),
        }
    })();
    let e = eng(id).ok_or("the engine stopped")?;
    e.token_busy = false;
    let (tok, secs) = r?;
    e.token = tok.clone();
    e.token_until = now() + secs.saturating_sub(60).max(60) * 1000;
    Ok(tok)
}

pub fn spclient(id: i64) -> String {
    if let Some(e) = eng(id) {
        if !e.spclient.is_empty() {
            return e.spclient.clone();
        }
    }
    let host = ap::resolve("spclient").first().map(|(h, p)| if *p == 443 { h.clone() } else { format!("{h}:{p}") }).unwrap_or_else(|| String::from("spclient.wg.spotify.com"));
    if let Some(e) = eng(id) {
        e.spclient = host.clone();
    }
    host
}

fn api(id: i64, method: &str, path: &str, body: &str) -> (u16, String) {
    if !path.starts_with("/v1/") || path.contains("..") || path.contains('@') || path.contains("://") {
        return (400, web::err_body("not a Web API path"));
    }
    // The public Web API rate-limits the client id this token is for across everyone who uses
    // it ("wait N s" on every click, however little Horizon asks). So: the player from Spotify
    // Connect (connect.rs), browsing through pathfinder (partner.rs), the Web API only for what
    // neither covers or when they do not answer.
    if let Some(r) = connect::web(id, method, path, body) {
        return r;
    }
    if let Some(r) = partner::route(id, method, path) {
        return r;
    }
    let tok = match token(id) {
        Ok(t) => t,
        Err(why) => return (0, web::err_body(&format!("no Web API token: {why}"))),
    };
    let r = web::api(&tok, method, path, body);
    if r.0 == 401 {
        // an expired token: once more with a new one
        if let Some(e) = eng(id) {
            e.token.clear();
        }
        if let Ok(t) = token(id) {
            return web::api(&t, method, path, body);
        }
    }
    r
}

// ── the builtins ──

pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let n = |i: usize| args.get(i).map(|&v| to_f64(v)).unwrap_or(-1.0);
    Some(match name {
        b"spotify_engine_start" => {
            // the port as Horizon has it: a number, or the text of one (argv)
            let port = match s(0).trim().parse::<f64>() {
                Ok(p) => p,
                Err(_) => n(0),
            };
            num(start(port as u16, s(1), s(2)) as f64)
        },
        b"spotify_engine_poll" => num(poll(n(0) as i64) as f64),
        b"spotify_engine_kill" => {
            kill(n(0) as i64);
            num(0.0)
        },
        b"spotify_engine_status" => mk_str(&eng(n(0) as i64).map(|e| status_json(e)).unwrap_or_default()),
        _ => return None,
    })
}

// ── `spotify` on the console / SSH: checks against the real service ──

pub fn command(args: &str) -> String {
    let a = args.trim();
    let mut out = String::new();
    let (cmd, rest) = a.split_once(' ').unwrap_or((a, ""));
    match cmd {
        "selftest" | "self" => {
            out.push_str(&format!("shannon (vs the shannon crate): {}\n", ok_or(shannon::selftest())));
            let d1 = ap::Dh::random();
            let d2 = ap::Dh::random();
            out.push_str(&format!("dh (768-bit group, both sides agree): {}\n", if d1.shared(&d2.public) == d2.shared(&d1.public) { "ok" } else { "FAILED" }));
            out.push_str(&format!("zeroconf blob (pbkdf2 RFC 6070, AES-CTR SP800-38A, addUser round trip): {}\n", ok_or(zeroconf::selftest())));
            out.push_str(&format!("mdns answer: {}\n", match zeroconf::mdns_selftest("Horizon") {
                Ok(n) => format!("ok ({n} bytes)"),
                Err(e) => format!("FAILED: {e}"),
            }));
            let g = player::gid_of("6rqhFgbbKwnb9MLmUQDhG6").map(|g| hex(&g)).unwrap_or_default();
            out.push_str(&format!("track id 6rqhFgbbKwnb9MLmUQDhG6 -> gid {g} ({})\n", if g == "d3aca7e43e3b452cbfa9ddd2eab9497e" { "ok" } else { "check" }));
            out.push_str(&format!("connect (gzip pushes, cluster parse): {}\n", ok_or(connect::selftest())));
        },
        "ap" => {
            // a login no account has: reaching APLoginFailed proves the handshake, Shannon and the protobufs
            let user = if rest.trim().is_empty() { "lingos-selftest-nobody" } else { rest.trim() };
            let t0 = now();
            match ap::sign_in(user, 0, b"not-a-password", &random_device_id(), &mut |m| {
                out.push_str(m);
                out.push('\n');
            }) {
                Ok((mut a, ap::Login::Failed(code, desc))) => {
                    a.close();
                    out.push_str(&format!("APLoginFailed: {} ({code}) {desc} -- in {} ms\n", ap::error_name(code), now() - t0));
                },
                Ok((mut a, ap::Login::Welcome(u, _, _))) => {
                    a.close();
                    out.push_str(&format!("APWelcome for {u}?!\n"));
                },
                Err(e) => out.push_str(&format!("error: {e}\n")),
            }
        },
        "resolve" => {
            for k in ["accesspoint", "dealer", "spclient"] {
                let v = ap::resolve(k);
                out.push_str(&format!("{k}: {}\n", v.iter().take(4).map(|(h, p)| format!("{h}:{p}")).collect::<Vec<_>>().join(" ")));
            }
        },
        "token" => {
            let did = random_device_id();
            let t0 = now();
            match web::client_token(&did) {
                Ok(t) => {
                    out.push_str(&format!("client token: {}... ({} chars) in {} ms\n", t.chars().take(16).collect::<String>(), t.len(), now() - t0));
                    match web::login5(&t, &did, "lingos-selftest-nobody", b"not-a-stored-credential") {
                        Ok(web::Login5::Ok(_, _, u)) => out.push_str(&format!("login5 gave a token for {u}?!\n")),
                        Ok(web::Login5::Error(c)) => out.push_str(&format!("login5 answered LoginError {} ({c}) -- the request was understood\n", web::login5_error(c))),
                        Err(e) => out.push_str(&format!("login5: {e}\n")),
                    }
                },
                Err(e) => out.push_str(&format!("client token: {e}\n")),
            }
        },
        "dealer" => {
            // the Connect push channel with a made-up token: a refused upgrade (401) proves the socket path reaches Spotify
            let host = ap::resolve("dealer").first().map(|(h, p)| if *p == 443 { h.clone() } else { format!("{h}:{p}") }).unwrap_or_else(|| String::from("dealer.spotify.com"));
            match crate::lingsock::kws_open(&format!("wss://{host}/?access_token=not-a-token"), "") {
                Ok(ws) => {
                    out.push_str(&format!("dealer {host}: opened?! state {}
", crate::lingsock::kws_state(ws)));
                    crate::lingsock::kws_close(ws);
                },
                Err(e) => out.push_str(&format!("dealer {host}: {e}
")),
            }
        },
        "pf" => {
            // pathfinder with a made-up token: 401 = the endpoint is there and wants a real token
            let body = r#"{"variables":{"uri":"spotify:track:6rqhFgbbKwnb9MLmUQDhG6"},"operationName":"getTrack","extensions":{"persistedQuery":{"version":1,"sha256Hash":"e7b1bf4e95cb3d733d48b442f93006e9712fd504e8b88d7a82e6e3bac9306a6b"}}}"#;
            match crate::lingsock::request_bytes("POST", "https://api-partner.spotify.com/pathfinder/v2/query", "Authorization: Bearer not-a-token
Content-Type: application/json", body.as_bytes()) {
                Ok((c, _, b)) => out.push_str(&format!("pathfinder with a made-up token: HTTP {c} {}
", String::from_utf8_lossy(&b).chars().take(160).collect::<String>())),
                Err(e) => out.push_str(&format!("pathfinder: {e}
")),
            }
        },
        "api" => {
            let (c, b) = web::api("not-a-token", "GET", "/v1/me", "");
            out.push_str(&format!("api.spotify.com /v1/me with a made-up token: HTTP {c} {}\n", b.chars().take(160).collect::<String>()));
        },
        "zc" => {
            // the zeroconf endpoint: getInfo, a phone's addUser (made here), then listen for real ones
            let mut z = zeroconf::Server::new(&random_device_id(), "Horizon");
            let (r, _) = z.handle_local("GET /?action=getInfo HTTP/1.1\r\n\r\n");
            out.push_str(&format!("getInfo: {}\n", r.chars().take(200).collect::<String>()));
            let req = zeroconf::phone_add_user(&z, "someone", 1, b"stored-credential-bytes");
            let (r, c) = z.handle_local(&req);
            out.push_str(&format!("addUser: {r} -> {}\n", c.map(|c| format!("{} type {} {} bytes", c.username, c.auth_type, c.auth_data.len())).unwrap_or_else(|| String::from("no credentials"))));
            let secs: u64 = rest.trim().parse().unwrap_or(0);
            if secs > 0 {
                out.push_str(&format!("listening on tcp {} + mdns for {secs} s\n", zeroconf::HTTP_PORT));
                let t0 = now();
                while now().saturating_sub(t0) < secs * 1000 {
                    if let Some(c) = z.tick() {
                        out.push_str(&format!("addUser over the network: {} type {} ({} bytes)\n", c.username, c.auth_type, c.auth_data.len()));
                    }
                    crate::arch::timer::poll_until(50_000, || false);
                }
            }
            z.stop();
        },
        "" | "status" => {
            for e in engines().iter() {
                out.push_str(&format!(
                    "engine {} port {}: {} {} user={} ap={} ws={} exit={:?} queue={} note={}\n",
                    e.id,
                    e.port,
                    e.state,
                    e.why,
                    e.user,
                    e.ap.as_ref().map(|a| a.host.as_str()).unwrap_or("-"),
                    if e.detached { "closed" } else if e.attached { "open" } else { "-" },
                    e.exit,
                    e.player.queue.len(),
                    e.player.note
                ));
                out.push_str(&format!("  connect: {}\n", connect::status(e)));
            }
            out.push_str(&format!(
                "pathfinder requests this boot: {} ({} failed -> Web API); Web API: {} sent, cooldown {} s\n",
                unsafe { partner::SENT },
                unsafe { partner::FAILED },
                web::sent(),
                web::cooldown_secs()
            ));
            out.push_str("usage: spotify [status | selftest | resolve | ap [user] | token | api | dealer | pf | zc [listen-secs]]\n");
        },
        _ => out.push_str("usage: spotify [status | selftest | resolve | ap [user] | token | api | dealer | pf | zc [listen-secs]]\n"),
    }
    out
}

fn ok_or(r: Result<(), String>) -> String {
    match r {
        Ok(()) => String::from("ok"),
        Err(e) => format!("FAILED: {e}"),
    }
}
