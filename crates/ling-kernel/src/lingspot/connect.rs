//! Spotify Connect, the way Spotify's own players and librespot do it: once signed in, Horizon
//! opens Spotify's "dealer" WebSocket (push messages for this account), registers itself as a
//! device with connect-state (PUT spclient /connect-state/v1/devices/hobs_<id>, protobuf
//! PutStateRequest), and from then on:
//!  - it is in the device list of the phone / desktop apps, which can pick it ("transfer"),
//!    play, pause, skip, seek and set its volume -- commands arrive over the dealer and drive
//!    the engine's own player (player.rs); every change is PUT back so the apps show it;
//!  - it sees the whole account's "cluster" (what plays where, every device): Horizon's
//!    now-playing bar and device list come from here, not from polling the Web API;
//!  - Horizon steers the other devices with connect-state commands (POST .../player/command/
//!    from/<me>/to/<them>), the web player's way, also outside the Web API.
//! The Web API calls spotify.ling makes for the player (/v1/me/player, /devices, play, pause,
//! next, seek, volume, shuffle, transfer) are answered from here while Connect is up (`web`).
//!
//! Field numbers: librespot's protocol/proto (connect.proto, player.proto, transfer_state.proto).

use super::pb::{self, W};
use super::{eng, log, now, Engine};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use base64::Engine as _;
use serde_json::{json, Value};

#[derive(Default, Clone)]
pub struct Dev {
    pub id: String,
    pub name: String,
    pub kind: u64,
    pub volume: u64,
}

/// The account's cluster as last heard (what the active device plays).
#[derive(Default, Clone)]
pub struct View {
    pub active: String,
    pub devices: Vec<Dev>,
    pub uri: String,
    pub context: String,
    pub duration: u64,
    pub pos: u64,
    /// local ms when `pos` was right
    pub pos_at: u64,
    pub playing: bool,
    pub shuffle: bool,
    pub name: String,
    pub artists: String,
    pub album: String,
    pub cover: String,
    /// uri the names above are for
    pub meta_uri: String,
}

#[derive(Default)]
pub struct Conn {
    pub ws: i64,
    pub connecting: bool,
    pub conn_id: String,
    pub registered: bool,
    pub next_try: u64,
    pub fails: u32,
    pub last_ping: u64,
    pub msg_id: u32,
    pub put_busy: bool,
    /// a PUT is wanted (its reason), sent when the last one is done
    pub put_due: u64,
    pub last_put: u64,
    pub sig: String,
    pub active: bool,
    pub started_at: u64,
    pub last_cmd_from: String,
    pub last_cmd_id: u64,
    /// after a transfer: seek here (ms) once the song plays; start paused
    pub pending_seek: Option<u32>,
    pub pending_pause: bool,
    pub meta_busy: bool,
    /// the context (album / playlist uri) this device plays from, when a command said
    pub ctx: String,
    pub view: View,
    pub state: String,
}

// connect.proto enums
const MEMBER_CONNECT_STATE: u64 = 2;
const PUT_NEW_DEVICE: u64 = 3;
const PUT_PLAYER_STATE_CHANGED: u64 = 4;
const PUT_VOLUME_CHANGED: u64 = 5;
const PUT_BECAME_INACTIVE: u64 = 7;
const DEVICE_COMPUTER: u64 = 1;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn epoch_ms() -> u64 {
    crate::clock::unix_now() * 1000 + now() % 1000
}

/// gzip (RFC 1952) -> bytes; anything else is returned as it is.
fn gunzip(b: &[u8]) -> Vec<u8> {
    if b.len() < 18 || b[0] != 0x1f || b[1] != 0x8b || b[2] != 8 {
        return b.to_vec();
    }
    let flg = b[3];
    let mut at = 10usize;
    if flg & 4 != 0 {
        let x = u16::from_le_bytes([b[at], b[at + 1]]) as usize;
        at += 2 + x;
    }
    for bit in [8u8, 16] {
        if flg & bit != 0 {
            while at < b.len() && b[at] != 0 {
                at += 1;
            }
            at += 1;
        }
    }
    if flg & 2 != 0 {
        at += 2;
    }
    if at >= b.len().saturating_sub(8) {
        return Vec::new();
    }
    miniz_oxide::inflate::decompress_to_vec(&b[at..b.len() - 8]).unwrap_or_default()
}

pub fn kind_name(k: u64) -> &'static str {
    match k {
        1 => "Computer",
        2 => "Tablet",
        3 => "Smartphone",
        4 => "Speaker",
        5 => "TV",
        6 => "AVR",
        7 => "STB",
        8 => "AudioDongle",
        9 => "GameConsole",
        10 => "CastVideo",
        11 => "CastAudio",
        12 => "Automobile",
        13 => "Smartwatch",
        14 => "Chromebook",
        _ => "Unknown",
    }
}

// ── the engine loop's turn ──

/// Connect up / down with the session; the dealer's messages; state PUTs when the player changed.
pub fn tick(e: &mut Engine) {
    if e.ap.is_none() {
        if e.conn.ws != 0 {
            stop(e);
        }
        return;
    }
    if e.conn.ws == 0 {
        if !e.conn.connecting && now() >= e.conn.next_try {
            start_dealer(e);
        }
        return;
    }
    let st = crate::lingsock::kws_state(e.conn.ws);
    if st != "open" {
        log(&format!("connect: the dealer socket {st}"));
        crate::lingsock::kws_close(e.conn.ws);
        e.conn.ws = 0;
        e.conn.registered = false;
        e.conn.conn_id.clear();
        e.conn.fails += 1;
        e.conn.next_try = now() + (5_000u64 << e.conn.fails.min(5)).min(120_000);
        e.conn.state = String::from("reconnecting");
        return;
    }
    let mut guard = 0;
    while guard < 32 {
        guard += 1;
        let Some(m) = crate::lingsock::kws_recv(e.conn.ws) else { break };
        on_message(e, &m);
    }
    if now().saturating_sub(e.conn.last_ping) > 30_000 {
        e.conn.last_ping = now();
        crate::lingsock::kws_send(e.conn.ws, "{\"type\":\"ping\"}");
    }
    // a song a transfer started: to where the other device was, paused if it was
    if let Some(ms) = e.conn.pending_seek {
        if e.player.started {
            e.conn.pending_seek = None;
            if ms > 1500 {
                super::player::command(e, &json!({"cmd": "seek", "ms": ms}));
            }
            if e.conn.pending_pause {
                super::player::command(e, &json!({"cmd": "pause"}));
            }
            e.conn.pending_pause = false;
        }
    }
    // our player changed: tell connect-state (at most every second)
    if e.conn.registered && e.conn.active {
        let l = &e.local;
        let sig = format!("{}|{}|{}|{}|{}", l.uri, l.playing, l.volume, l.duration, l.now_ms() / 5000);
        if sig != e.conn.sig {
            e.conn.sig = sig;
            if e.conn.put_due == 0 {
                e.conn.put_due = PUT_PLAYER_STATE_CHANGED;
            }
        }
    }
    if e.conn.put_due != 0 && !e.conn.put_busy && e.conn.registered && now().saturating_sub(e.conn.last_put) >= 1000 {
        let why = e.conn.put_due;
        e.conn.put_due = 0;
        put_state(e, why);
    }
    // names for what another device plays (spclient metadata, once per song)
    if !e.conn.view.uri.is_empty() && e.conn.view.uri != e.conn.view.meta_uri && !e.conn.meta_busy && e.conn.view.uri.starts_with("spotify:track:") {
        e.conn.meta_busy = true;
        let id = e.id;
        let uri = e.conn.view.uri.clone();
        let _ = crate::coro::spawn(
            "lingspot_meta",
            false,
            Box::new(move || {
                let m = super::player::track_meta(id, &uri);
                if let Some(e) = eng(id) {
                    e.conn.meta_busy = false;
                    e.conn.view.meta_uri = uri.clone();
                    if let Some(m) = m {
                        if e.conn.view.uri == uri {
                            e.conn.view.name = m.name;
                            e.conn.view.artists = m.artists;
                            e.conn.view.album = m.album;
                            e.conn.view.cover = m.cover;
                            if e.conn.view.duration == 0 {
                                e.conn.view.duration = m.duration as u64;
                            }
                        }
                    }
                }
            }),
        );
    }
}

pub fn stop(e: &mut Engine) {
    if e.conn.ws != 0 {
        crate::lingsock::kws_close(e.conn.ws);
    }
    e.conn.ws = 0;
    e.conn.registered = false;
    e.conn.conn_id.clear();
    e.conn.state = String::from("off");
}

fn start_dealer(e: &mut Engine) {
    e.conn.connecting = true;
    e.conn.state = String::from("connecting");
    let id = e.id;
    let _ = crate::coro::spawn(
        "lingspot_dealer",
        false,
        Box::new(move || {
            let r = (|| -> Result<i64, String> {
                let tok = super::token(id)?;
                let host = super::ap::resolve("dealer").first().map(|(h, p)| if *p == 443 { h.clone() } else { format!("{h}:{p}") }).unwrap_or_else(|| String::from("dealer.spotify.com"));
                crate::lingsock::kws_open(&format!("wss://{host}/?access_token={tok}"), &format!("User-Agent: {}", super::web::user_agent()))
            })();
            let Some(e) = eng(id) else {
                if let Ok(ws) = r {
                    crate::lingsock::kws_close(ws);
                }
                return;
            };
            e.conn.connecting = false;
            match r {
                Ok(ws) => {
                    e.conn.ws = ws;
                    e.conn.last_ping = now();
                    e.conn.state = String::from("waiting for the dealer");
                    log("connect: dealer open");
                },
                Err(why) => {
                    e.conn.fails += 1;
                    e.conn.next_try = now() + (5_000u64 << e.conn.fails.min(5)).min(120_000);
                    e.conn.state = format!("could not reach Spotify Connect: {why}");
                    log(&format!("connect: {why}"));
                },
            }
        }),
    );
}

// ── the dealer's messages ──

fn on_message(e: &mut Engine, m: &str) {
    let Ok(v) = serde_json::from_str::<Value>(m) else { return };
    match v["type"].as_str().unwrap_or("") {
        "message" => {
            let uri = v["uri"].as_str().unwrap_or("");
            if uri.starts_with("hm://pusher/v1/connections/") {
                let cid = v["headers"]["Spotify-Connection-Id"].as_str().map(String::from).unwrap_or_else(|| url_dec(uri.trim_start_matches("hm://pusher/v1/connections/")));
                e.conn.conn_id = cid;
                e.conn.fails = 0;
                put_state(e, PUT_NEW_DEVICE);
                return;
            }
            let gz = v["headers"]["Transfer-Encoding"].as_str() == Some("gzip");
            let payload: Vec<u8> = v["payloads"].as_array().and_then(|a| a.first()).and_then(|p| p.as_str()).and_then(|s| b64().decode(s).ok()).unwrap_or_default();
            let data = if gz { gunzip(&payload) } else { payload };
            if uri.starts_with("hm://connect-state/v1/cluster") {
                if let Some(c) = pb::get_b(&data, 1) {
                    let c = c.to_vec();
                    on_cluster(e, &c);
                }
            } else if uri.starts_with("hm://connect-state/v1/connect/volume") {
                // SetVolumeCommand { volume = 1 }
                let vol = pb::get_n(&data, 1).unwrap_or(0).min(65535);
                super::player::command(e, &json!({"cmd": "volume", "pct": vol * 100 / 65535}));
                e.local.volume = vol as u16;
                e.conn.put_due = PUT_VOLUME_CHANGED;
            }
        },
        "request" => {
            let key = v["key"].as_str().unwrap_or("").to_string();
            let p = &v["payload"];
            let body: Value = match p["compressed"].as_str() {
                Some(c) => serde_json::from_slice(&gunzip(&b64().decode(c).unwrap_or_default())).unwrap_or(Value::Null),
                None => p.clone(),
            };
            let ok = on_command(e, &body);
            let reply = json!({"type": "reply", "key": key, "payload": {"success": ok}}).to_string();
            crate::lingsock::kws_send(e.conn.ws, &reply);
        },
        _ => {},
    }
}

fn url_dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(x) = u8::from_str_radix(core::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                out.push(x);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Cluster { active_device_id 2, player_state 3, device 4 (map), server_timestamp_ms 9 }.
fn on_cluster(e: &mut Engine, c: &[u8]) {
    let me = e.device_id.clone();
    parse_cluster(&mut e.conn.view, c);
    let v = &e.conn.view;
    // another device took over: ours stops (as the apps do)
    if e.conn.active && !v.active.is_empty() && v.active != me {
        e.conn.active = false;
        if e.local.playing {
            super::player::command(e, &json!({"cmd": "pause"}));
        }
        e.conn.put_due = PUT_BECAME_INACTIVE;
    }
}

/// The cluster's fields into `v` (pure: the selftest feeds it a made-up cluster).
pub fn parse_cluster(v: &mut View, c: &[u8]) {
    v.active = pb::get_str(c, 2);
    v.devices.clear();
    for entry in pb::get_all_b(c, 4) {
        let key = pb::get_str(entry, 1);
        let Some(d) = pb::get_b(entry, 2) else { continue };
        let id = { let i = pb::get_str(d, 10); if i.is_empty() { key } else { i } };
        v.devices.push(Dev { id, name: pb::get_str(d, 3), kind: pb::get_n(d, 7).unwrap_or(0), volume: pb::get_n(d, 2).unwrap_or(0) });
    }
    if let Some(ps) = pb::get_b(c, 3) {
        let ts = pb::get_n(ps, 1).unwrap_or(0);
        let server = pb::get_n(c, 9).unwrap_or(ts);
        let pos0 = pb::get_n(ps, 10).unwrap_or(0);
        let playing = pb::get_n(ps, 12).unwrap_or(0) != 0 && pb::get_n(ps, 13).unwrap_or(0) == 0;
        let uri = pb::get_b(ps, 7).map(|t| pb::get_str(t, 1)).unwrap_or_default();
        if uri != v.uri {
            v.name.clear();
            v.artists.clear();
            v.album.clear();
            v.cover.clear();
            v.meta_uri.clear();
        }
        v.uri = uri;
        v.context = pb::get_str(ps, 2);
        v.duration = pb::get_n(ps, 11).unwrap_or(0);
        v.playing = playing;
        v.pos = if playing { pos0 + server.saturating_sub(ts) } else { pos0 };
        v.pos_at = now();
        v.shuffle = pb::get_b(ps, 16).and_then(|o| pb::get_n(o, 1)).unwrap_or(0) != 0;
        // what the track metadata already says (title / album_title / image_url)
        if let Some(t) = pb::get_b(ps, 7) {
            for kv in pb::get_all_b(t, 3) {
                let (k, val) = (pb::get_str(kv, 1), pb::get_str(kv, 2));
                match k.as_str() {
                    "title" => v.name = val,
                    "album_title" => v.album = val,
                    "artist_name" => v.artists = val,
                    "image_url" if val.starts_with("spotify:image:") => v.cover = format!("https://i.scdn.co/image/{}", &val[14..]),
                    _ => {},
                }
            }
        }
    }
}

/// A player command for this device (from the phone): true when it was carried out.
fn on_command(e: &mut Engine, body: &Value) -> bool {
    let c = &body["command"];
    let ep = c["endpoint"].as_str().unwrap_or("");
    e.conn.last_cmd_from = body["sent_by_device_id"].as_str().unwrap_or("").to_string();
    e.conn.last_cmd_id = body["message_id"].as_u64().unwrap_or(0);
    log(&format!("connect: {ep} from {}", if e.conn.last_cmd_from.is_empty() { "?" } else { &e.conn.last_cmd_from }));
    let local = |e: &mut Engine, j: Value| super::player::command(e, &j);
    match ep {
        "transfer" => {
            let ts = b64().decode(c["data"].as_str().unwrap_or("")).unwrap_or_default();
            // TransferState { options 1, playback 2 { timestamp 1, position_as_of_timestamp 2,
            //   is_paused 4, current_track 5 { uri 1 } }, current_session 3 { context 2 { uri 1 } } }
            let pbk = pb::get_b(&ts, 2).unwrap_or(&[]);
            let track = pb::get_b(pbk, 5).map(|t| pb::get_str(t, 1)).unwrap_or_default();
            let at = pb::get_n(pbk, 2).unwrap_or(0) as i32;
            let stamp = pb::get_n(pbk, 1).unwrap_or(0);
            let paused = pb::get_n(pbk, 4).unwrap_or(0) != 0;
            let ctx = pb::path(&ts, &[3, 2]).map(|x| pb::get_str(x, 1)).unwrap_or_default();
            let shuffle = pb::get_b(&ts, 1).and_then(|o| pb::get_n(o, 1)).unwrap_or(0) != 0;
            let elapsed = if paused || stamp == 0 { 0 } else { epoch_ms().saturating_sub(stamp) };
            if track.is_empty() && ctx.is_empty() {
                return false;
            }
            become_active(e);
            e.player.shuffle = shuffle;
            e.conn.ctx = if ctx.starts_with("spotify:track:") { String::new() } else { ctx.clone() };
            if ctx.is_empty() || ctx.starts_with("spotify:track:") {
                local(e, json!({"cmd": "load", "tracks": [track.clone()], "track": track}));
            } else {
                local(e, json!({"cmd": "load", "context": ctx, "track": track}));
            }
            e.conn.pending_seek = Some((at.max(0) as u64 + elapsed) as u32);
            e.conn.pending_pause = paused;
            true
        },
        "play" => {
            let ctx = c["context"]["uri"].as_str().unwrap_or("").to_string();
            let tracks: Vec<String> = c["context"]["pages"]
                .as_array()
                .map(|pages| pages.iter().flat_map(|p| p["tracks"].as_array().cloned().unwrap_or_default()).filter_map(|t| t["uri"].as_str().map(String::from)).collect())
                .unwrap_or_default();
            let skip = &c["options"]["skip_to"];
            let mut track = skip["track_uri"].as_str().unwrap_or("").to_string();
            if track.is_empty() {
                if let Some(i) = skip["track_index"].as_u64() {
                    track = tracks.get(i as usize).cloned().unwrap_or_default();
                }
            }
            become_active(e);
            e.conn.ctx = if ctx.starts_with("spotify:list:") || ctx.starts_with("spotify:track:") { String::new() } else { ctx.clone() };
            if !tracks.is_empty() || ctx.starts_with("spotify:track:") || ctx.is_empty() {
                let mut q = tracks;
                if q.is_empty() {
                    q.push(if track.is_empty() { ctx.clone() } else { track.clone() });
                }
                local(e, json!({"cmd": "load", "tracks": q, "track": track}));
            } else {
                local(e, json!({"cmd": "load", "context": ctx, "track": track}));
            }
            let seek = c["options"]["seek_to"].as_u64().unwrap_or(0) as u32;
            e.conn.pending_seek = Some(seek);
            e.conn.pending_pause = c["options"]["initially_paused"].as_bool().unwrap_or(false);
            true
        },
        "pause" => {
            local(e, json!({"cmd": "pause"}));
            true
        },
        "resume" => {
            local(e, json!({"cmd": "play"}));
            true
        },
        "seek_to" => {
            let ms = c["value"].as_u64().or_else(|| c["position"].as_u64()).unwrap_or(0);
            local(e, json!({"cmd": "seek", "ms": ms}));
            true
        },
        "skip_next" => {
            local(e, json!({"cmd": "next"}));
            true
        },
        "skip_prev" => {
            local(e, json!({"cmd": "prev"}));
            true
        },
        "set_shuffling_context" => {
            local(e, json!({"cmd": "shuffle", "on": c["value"].as_bool().unwrap_or(false)}));
            true
        },
        "set_repeating_context" => {
            local(e, json!({"cmd": "repeat", "on": c["value"].as_bool().unwrap_or(false)}));
            true
        },
        "add_to_queue" => {
            if let Some(u) = c["track"]["uri"].as_str() {
                let at = (e.player.idx + 1).min(e.player.queue.len());
                e.player.queue.insert(at, String::from(u));
            }
            true
        },
        // queue edits, options, context updates: taken (nothing here keeps them yet)
        "set_queue" | "set_options" | "update_context" | "set_repeating_track" => true,
        _ => false,
    }
}

fn become_active(e: &mut Engine) {
    if !e.conn.active {
        e.conn.active = true;
        e.conn.started_at = epoch_ms();
    }
    e.local.active = true;
    e.conn.put_due = PUT_PLAYER_STATE_CHANGED;
}

/// Horizon itself started playing (its own Play button): it is the active device from now on.
pub fn local_play_started(e: &mut Engine) {
    if e.conn.registered {
        become_active(e);
    }
}

// ── PUT state ──

fn player_state(e: &Engine) -> W {
    let l = &e.local;
    let mut ps = W::new();
    let ctx = e.conn.ctx.clone();
    ps.uint(1, epoch_ms());
    if !ctx.is_empty() {
        ps.str(2, &ctx).str(3, &format!("context://{ctx}"));
    }
    if !l.uri.is_empty() {
        let mut t = W::new();
        t.str(1, &l.uri).str(6, "context");
        ps.msg(7, &t);
    }
    ps.fixed64(9, 1.0f64.to_bits());
    ps.uint(10, l.now_ms() as u64).uint(11, l.duration as u64);
    let has = !l.uri.is_empty();
    ps.bool(12, has).bool(13, has && !l.playing).bool(14, e.player.loading);
    let mut o = W::new();
    o.bool(1, e.player.shuffle).bool(2, e.player.repeat);
    ps.msg(16, &o);
    ps.uint(25, l.now_ms() as u64);
    ps
}

fn put_body(e: &mut Engine, reason: u64) -> Vec<u8> {
    let mut caps = W::new();
    caps.bool(2, true) // can_be_player
        .bool(5, true) // gaia_eq_connect_id
        .bool(6, true) // supports_logout
        .bool(7, true) // is_observable
        .int(8, 64) // volume_steps
        .str(9, "audio/track")
        .bool(10, true) // command_acks
        .bool(16, true) // is_controllable
        .bool(19, true) // supports_transfer_command
        .bool(20, true) // supports_command_request
        .bool(23, true); // supports_gzip_pushes
    let mut info = W::new();
    info.bool(1, true)
        .uint(2, e.local.volume as u64)
        .str(3, &e.name)
        .msg(4, &caps)
        .str(6, "lingspot-lingos")
        .uint(7, DEVICE_COMPUTER)
        .str(9, "3.2.6")
        .str(10, &e.device_id)
        .str(13, super::ap::CLIENT_ID)
        .str(14, "LingOS")
        .str(15, "Horizon");
    let mut dev = W::new();
    dev.msg(1, &info).msg(2, &player_state(e));
    e.conn.msg_id += 1;
    let mut req = W::new();
    req.msg(2, &dev).uint(3, MEMBER_CONNECT_STATE).bool(4, e.conn.active).uint(5, reason).uint(6, e.conn.msg_id as u64);
    if !e.conn.last_cmd_from.is_empty() {
        req.str(7, &e.conn.last_cmd_from).uint(8, e.conn.last_cmd_id);
    }
    if e.conn.active {
        req.uint(9, e.conn.started_at).uint(11, epoch_ms().saturating_sub(e.conn.started_at));
    }
    req.uint(12, epoch_ms());
    req.done()
}

fn put_state(e: &mut Engine, reason: u64) {
    let id = e.id;
    if e.conn.conn_id.is_empty() || e.conn.put_busy {
        e.conn.put_due = reason;
        return;
    }
    e.conn.put_busy = true;
    e.conn.last_put = now();
    let body = put_body(e, reason);
    let cid = e.conn.conn_id.clone();
    let did = e.device_id.clone();
    let _ = crate::coro::spawn(
        "lingspot_put",
        false,
        Box::new(move || {
            let r = (|| -> Result<(u16, Vec<u8>), String> {
                let tok = super::token(id)?;
                let host = super::spclient(id);
                let ct = eng(id).map(|e| e.client_token.clone()).unwrap_or_default();
                let h = format!(
                    "Authorization: Bearer {tok}\nclient-token: {ct}\nX-Spotify-Connection-Id: {cid}\nContent-Type: application/x-protobuf\nAccept: application/x-protobuf\nUser-Agent: {}",
                    super::web::user_agent()
                );
                let (code, head, b) = crate::lingsock::request_bytes("PUT", &format!("https://{host}/connect-state/v1/devices/hobs_{did}"), &h, &body)?;
                let gz = head.lines().any(|l| l.to_ascii_lowercase().starts_with("content-encoding:") && l.contains("gzip"));
                Ok((code, if gz { gunzip(&b) } else { b }))
            })();
            let Some(e) = eng(id) else { return };
            e.conn.put_busy = false;
            match r {
                Ok((200, cluster)) => {
                    if !e.conn.registered {
                        log(&format!("connect: \"{}\" is a Spotify Connect device now", e.name));
                    }
                    e.conn.registered = true;
                    e.conn.state = String::from("connected");
                    if !cluster.is_empty() {
                        on_cluster(e, &cluster);
                    }
                },
                Ok((code, b)) => {
                    e.conn.state = format!("connect-state answered HTTP {code}");
                    log(&format!("connect: PUT state HTTP {code} {}", String::from_utf8_lossy(&b[..b.len().min(120)])));
                },
                Err(why) => {
                    e.conn.state = format!("connect-state: {why}");
                    log(&format!("connect: PUT state: {why}"));
                },
            }
        }),
    );
}

// ── steering the other devices ──

fn spclient_json(id: i64, method: &str, path: &str, body: &Value) -> (u16, String) {
    let r = (|| -> Result<(u16, String), String> {
        let tok = super::token(id)?;
        let host = super::spclient(id);
        let ct = eng(id).map(|e| e.client_token.clone()).unwrap_or_default();
        let h = format!("Authorization: Bearer {tok}\nclient-token: {ct}\nContent-Type: application/json\nAccept: application/json\nUser-Agent: {}", super::web::user_agent());
        let (code, _, b) = crate::lingsock::request_bytes(method, &format!("https://{host}{path}"), &h, body.to_string().as_bytes())?;
        Ok((code, String::from_utf8_lossy(&b).into_owned()))
    })();
    match r {
        Ok((c, b)) if (200..300).contains(&c) => (204, b),
        Ok((c, b)) => (c, super::web::err_body(&format!("Spotify Connect answered HTTP {c} {}", b.chars().take(80).collect::<String>()))),
        Err(e) => (0, super::web::err_body(&e)),
    }
}

fn command_to(id: i64, to: &str, cmd: Value) -> (u16, String) {
    let me = eng(id).map(|e| e.device_id.clone()).unwrap_or_default();
    spclient_json(id, "POST", &format!("/connect-state/v1/player/command/from/{me}/to/{to}"), &json!({"command": cmd}))
}

fn qarg<'a>(q: &'a str, name: &str) -> &'a str {
    q.split('&').find_map(|p| p.strip_prefix(name).and_then(|r| r.strip_prefix('='))).unwrap_or("")
}

/// The Web API's player calls, answered from the cluster / with connect-state commands while
/// Connect is up (None: not one of them, or Connect is not up -- the Web API then).
pub fn web(id: i64, method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    let e = eng(id)?;
    if !e.conn.registered || !path.starts_with("/v1/me/player") {
        return None;
    }
    let (p, q) = path.split_once('?').unwrap_or((path, ""));
    let me = e.device_id.clone();
    let v = e.conn.view.clone();
    let target = { let d = qarg(q, "device_id"); if d.is_empty() { v.active.clone() } else { String::from(d) } };
    match (method, p) {
        ("GET", "/v1/me/player") | ("GET", "/v1/me/player/currently-playing") => {
            if v.active.is_empty() || v.uri.is_empty() {
                return Some((204, String::new()));
            }
            let dev = v.devices.iter().find(|d| d.id == v.active).cloned().unwrap_or_default();
            let pos = if v.playing { v.pos + now().saturating_sub(v.pos_at) } else { v.pos }.min(v.duration.max(v.pos));
            let tid = v.uri.rsplit(':').next().unwrap_or("");
            Some((
                200,
                json!({
                    "device": {"id": v.active, "name": (if v.active == me { "Horizon".to_string() } else { dev.name }), "type": kind_name(dev.kind), "volume_percent": dev.volume * 100 / 65535, "is_active": true},
                    "is_playing": v.playing, "progress_ms": pos, "shuffle_state": v.shuffle, "context": {"uri": v.context},
                    "item": {"uri": v.uri, "id": tid, "name": v.name, "duration_ms": v.duration,
                             "artists": v.artists.split(", ").filter(|s| !s.is_empty()).map(|a| json!({"name": a, "id": ""})).collect::<Vec<_>>(),
                             "album": {"name": v.album, "id": "", "images": (if v.cover.is_empty() { json!([]) } else { json!([{"url": v.cover}]) })}},
                })
                .to_string(),
            ))
        },
        ("GET", "/v1/me/player/devices") => {
            let mut list: Vec<Value> = v
                .devices
                .iter()
                .map(|d| json!({"id": d.id, "name": d.name, "type": kind_name(d.kind), "volume_percent": d.volume * 100 / 65535, "is_active": d.id == v.active}))
                .collect();
            if !v.devices.iter().any(|d| d.id == me) {
                list.insert(0, json!({"id": me, "name": "Horizon", "type": "Computer", "volume_percent": 50, "is_active": v.active == me}));
            }
            Some((200, json!({"devices": list}).to_string()))
        },
        ("PUT", "/v1/me/player") => {
            // transfer: {"device_ids":[to],"play":bool}
            let b: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let to = b["device_ids"][0].as_str().unwrap_or("").to_string();
            if to.is_empty() {
                return Some((400, super::web::err_body("no device")));
            }
            let from = if v.active.is_empty() { me.clone() } else { v.active.clone() };
            Some(spclient_json(id, "POST", &format!("/connect-state/v1/connect/transfer/from/{from}/to/{to}"), &json!({"transfer_options": {"restore_paused": (if b["play"].as_bool().unwrap_or(true) { "resume" } else { "restore" })}})))
        },
        ("PUT", "/v1/me/player/pause") => Some(command_to(id, &target, json!({"endpoint": "pause"}))),
        ("PUT", "/v1/me/player/play") if body.trim().is_empty() => Some(command_to(id, &target, json!({"endpoint": "resume"}))),
        ("PUT", "/v1/me/player/play") => {
            let b: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let off = b["offset"]["uri"].as_str().unwrap_or("").to_string();
            let cmd = if let Some(uris) = b["uris"].as_array() {
                let tracks: Vec<Value> = uris.iter().filter_map(|u| u.as_str()).map(|u| json!({"uri": u})).collect();
                let first = uris.first().and_then(|u| u.as_str()).unwrap_or("").to_string();
                let ctx = format!("spotify:list:{}", first.rsplit(':').next().unwrap_or(""));
                json!({"endpoint": "play", "context": {"uri": ctx, "url": format!("context://{ctx}"), "pages": [{"tracks": tracks}]},
                       "options": {"skip_to": {"track_uri": (if off.is_empty() { first } else { off })}, "license": "on-demand"},
                       "play_origin": {"feature_identifier": "harmony", "feature_version": "4.13.0"}})
            } else {
                let ctx = b["context_uri"].as_str().unwrap_or("").to_string();
                json!({"endpoint": "play", "context": {"uri": ctx, "url": format!("context://{ctx}")},
                       "options": {"skip_to": (if off.is_empty() { json!({}) } else { json!({"track_uri": off}) }), "license": "on-demand"},
                       "play_origin": {"feature_identifier": "harmony", "feature_version": "4.13.0"}})
            };
            Some(command_to(id, &target, cmd))
        },
        ("POST", "/v1/me/player/next") => Some(command_to(id, &target, json!({"endpoint": "skip_next"}))),
        ("POST", "/v1/me/player/previous") => Some(command_to(id, &target, json!({"endpoint": "skip_prev"}))),
        ("PUT", "/v1/me/player/seek") => {
            let ms: u64 = qarg(q, "position_ms").parse().unwrap_or(0);
            Some(command_to(id, &target, json!({"endpoint": "seek_to", "value": ms})))
        },
        ("PUT", "/v1/me/player/shuffle") => Some(command_to(id, &target, json!({"endpoint": "set_shuffling_context", "value": qarg(q, "state") == "true"}))),
        ("PUT", "/v1/me/player/volume") => {
            let pct: u64 = qarg(q, "volume_percent").parse().unwrap_or(50);
            Some(spclient_json(id, "PUT", &format!("/connect-state/v1/connect/volume/from/{me}/to/{target}"), &json!({"volume": pct.min(100) * 65535 / 100})))
        },
        _ => None,
    }
}

/// For the status line Horizon gets twice a second.
pub fn status(e: &Engine) -> Value {
    let v = &e.conn.view;
    let active_name = if v.active == e.device_id { String::from("Horizon") } else { v.devices.iter().find(|d| d.id == v.active).map(|d| d.name.clone()).unwrap_or_default() };
    json!({"state": e.conn.state, "registered": e.conn.registered, "active": v.active, "active_name": active_name, "me_active": e.conn.active, "devices": v.devices.len()})
}

/// `spotify selftest`: gzip pushes and a cluster parse, no network.
pub fn selftest() -> Result<(), String> {
    // a gzip member around a raw deflate stream, as the dealer sends
    let text = b"{\"command\":{\"endpoint\":\"pause\"}} -- and some more so it compresses";
    let mut gz = alloc::vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    gz.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(text, 6));
    gz.extend_from_slice(&[0u8; 8]);
    if gunzip(&gz) != text {
        return Err(String::from("gunzip did not give the text back"));
    }
    // Cluster { active 2, player_state 3 { timestamp 1, context 2, track 7 { uri 1, metadata 3 },
    //   position 10, duration 11, is_playing 12 }, device 4 { key 1, DeviceInfo 2 { volume 2, name 3, type 7, id 10 } } }
    let mut md = W::new();
    md.str(1, "title").str(2, "A Song");
    let mut t = W::new();
    t.str(1, "spotify:track:6rqhFgbbKwnb9MLmUQDhG6").msg(3, &md);
    let mut ps = W::new();
    ps.uint(1, 1_000_000).str(2, "spotify:album:x").msg(7, &t).uint(10, 42_000).uint(11, 200_000).bool(12, true);
    let mut di = W::new();
    di.uint(2, 32768).str(3, "Phone").uint(7, 3).str(10, "phone1");
    let mut ent = W::new();
    ent.str(1, "phone1").msg(2, &di);
    let mut c = W::new();
    c.str(2, "phone1").msg(3, &ps).msg(4, &ent).uint(9, 1_001_000);
    let mut v = View::default();
    parse_cluster(&mut v, &c.done());
    let ok = v.active == "phone1"
        && v.uri == "spotify:track:6rqhFgbbKwnb9MLmUQDhG6"
        && v.name == "A Song"
        && v.playing
        && v.pos == 43_000
        && v.duration == 200_000
        && v.devices.len() == 1
        && v.devices[0].name == "Phone"
        && v.devices[0].kind == 3;
    if !ok {
        return Err(format!("cluster parse: active {} uri {} name {} pos {} devices {}", v.active, v.uri, v.name, v.pos, v.devices.len()));
    }
    Ok(())
}
