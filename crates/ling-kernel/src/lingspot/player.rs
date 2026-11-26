//! Horizon's own Spotify player (the `local` ops): a queue of track uris, and
//! for each track the path librespot takes -- the track's audio files from
//! spclient's metadata, the file's AES key over the access point, its CDN
//! address from storage-resolve, the file itself, AES-128-CTR decrypted,
//! Spotify's 0xa7-byte header skipped, Ogg Vorbis decoded as it goes into a
//! streamed sound on the mixer's music voice (drivers/sfx.rs).

use super::pb;
use super::{eng, log, Engine};
use crate::drivers::sfx;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

const AUDIO_AESIV: [u8; 16] = [0x72, 0xe0, 0x67, 0xfb, 0xdd, 0xcb, 0xcf, 0x77, 0xeb, 0xe8, 0xbc, 0x64, 0x3f, 0x63, 0x0d, 0x93];
const OGG_HEADER_END: usize = 0xa7;

#[derive(Default, Clone)]
pub struct Local {
    pub playing: bool,
    pub active: bool,
    pub uri: String,
    pub name: String,
    pub artists: String,
    pub album: String,
    pub cover: String,
    pub duration: u32,
    pub pos: u32,
    pub pos_at: u64,
    pub volume: u16,
}

impl Local {
    pub fn now_ms(&self) -> u32 {
        let extra = if self.playing && self.pos_at > 0 { crate::arch::timer::now_ms().saturating_sub(self.pos_at) as u32 } else { 0 };
        (self.pos + extra).min(self.duration.max(self.pos))
    }
    fn at(&mut self, pos: u32, playing: bool) {
        self.pos = pos;
        self.pos_at = crate::arch::timer::now_ms();
        self.playing = playing;
    }
}

#[derive(Default)]
pub struct Player {
    pub queue: Vec<String>,
    pub idx: usize,
    pub shuffle: bool,
    pub repeat: bool,
    /// the sfx stream of the track that sounds
    pub stream: Option<usize>,
    pub rate: u32,
    /// all of the track is decoded into the stream
    pub complete: bool,
    /// bumps on every load: a slower load that was overtaken stops
    pub gen: u64,
    pub loading: bool,
    /// playback should start once enough is decoded
    pub want_play: bool,
    pub started: bool,
    pub note: String,
}

/// base62 track id -> its 16-byte gid
pub fn gid_of(id: &str) -> Option<[u8; 16]> {
    const A: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut v: u128 = 0;
    for c in id.bytes() {
        let d = A.iter().position(|&x| x == c)? as u128;
        v = v.checked_mul(62)?.checked_add(d)?;
    }
    Some(v.to_be_bytes())
}

fn id_of(uri: &str) -> &str {
    uri.rsplit(':').next().unwrap_or("")
}

fn hex(b: &[u8]) -> String {
    super::hex(b)
}

/// A `local` op from Horizon.
pub fn command(e: &mut Engine, v: &serde_json::Value) {
    let cmd = v["cmd"].as_str().unwrap_or("");
    let p = &mut e.player;
    match cmd {
        "play" => {
            if p.stream.is_some() && sfx::music_sample() == p.stream && !sfx::music_playing() {
                sfx::pause_music();
            } else if p.stream.is_none() && !p.queue.is_empty() && !p.loading {
                let i = p.idx;
                start_load(e, i);
                return;
            }
            p.want_play = true;
            let pos = e.local.now_ms();
            e.local.at(pos, true);
        },
        "pause" => {
            if sfx::music_playing() && sfx::music_sample() == p.stream {
                sfx::pause_music();
            }
            p.want_play = false;
            let pos = e.local.now_ms();
            e.local.at(pos, false);
        },
        "next" => skip(e, 1),
        "prev" => {
            // like the apps: back to the start unless it has only just begun
            if e.local.now_ms() > 3000 {
                seek(e, 0);
            } else {
                skip(e, -1);
            }
        },
        "seek" => seek(e, v["ms"].as_u64().unwrap_or(0) as u32),
        "volume" => {
            let pct = v["pct"].as_u64().unwrap_or(50).min(100) as u32;
            e.local.volume = (pct * 65535 / 100) as u16;
            sfx::set_music_volume(pct as f32 / 100.0);
        },
        "shuffle" => {
            p.shuffle = v["on"].as_bool().unwrap_or(false);
            if p.shuffle && p.queue.len() > p.idx + 2 {
                // the songs after this one, in another order
                let mut rest: Vec<String> = p.queue.drain(p.idx + 1..).collect();
                let mut out = Vec::with_capacity(rest.len());
                while !rest.is_empty() {
                    let mut r = [0u8; 4];
                    crate::crypto::random_bytes(&mut r);
                    out.push(rest.swap_remove(u32::from_le_bytes(r) as usize % rest.len()));
                }
                p.queue.extend(out);
            }
        },
        "repeat" => p.repeat = v["on"].as_bool().unwrap_or(false),
        "activate" => e.local.active = true,
        // Horizon's Refresh: browse answers are fetched again (partner.rs keeps them a few minutes)
        "refresh" => super::partner::forget(e.id),
        "load" => {
            let ctx = v["context"].as_str().unwrap_or("").to_string();
            let track = v["track"].as_str().unwrap_or("").to_string();
            let tracks: Vec<String> = v["tracks"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect()).unwrap_or_default();
            e.local.active = true;
            if ctx.is_empty() || !tracks.is_empty() {
                let mut q = tracks;
                if q.is_empty() && !track.is_empty() {
                    q.push(track.clone());
                }
                let i = q.iter().position(|t| *t == track).unwrap_or(0);
                e.player.queue = q;
                start_load(e, i);
            } else {
                // an album / playlist: its songs from the Web API first
                let id = e.id;
                e.player.gen += 1;
                let g = e.player.gen;
                e.player.loading = true;
                e.local.uri = track.clone();
                let _ = crate::coro::spawn(
                    "lingspot_ctx",
                    false,
                    Box::new(move || {
                        let q = context_tracks(id, &ctx);
                        let Some(e) = eng(id) else { return };
                        if e.player.gen != g {
                            return;
                        }
                        e.player.loading = false;
                        let mut q = q;
                        if q.is_empty() && !track.is_empty() {
                            q.push(track.clone());
                        }
                        let i = q.iter().position(|t| *t == track).unwrap_or(0);
                        e.player.queue = q;
                        start_load(e, i);
                    }),
                );
            }
        },
        _ => {},
    }
}

fn seek(e: &mut Engine, ms: u32) {
    let p = &e.player;
    if p.stream.is_some() && sfx::music_sample() == p.stream {
        sfx::seek_music(ms as f64 / 1000.0);
    }
    let playing = e.local.playing;
    e.local.at(ms.min(e.local.duration.max(ms)), playing);
}

fn skip(e: &mut Engine, d: i64) {
    let n = e.player.queue.len() as i64;
    if n == 0 {
        return;
    }
    let mut i = e.player.idx as i64 + d;
    if i >= n {
        if !e.player.repeat {
            stop(e);
            return;
        }
        i = 0;
    }
    if i < 0 {
        i = 0;
    }
    start_load(e, i as usize);
}

pub fn stop(e: &mut Engine) {
    if e.player.stream.is_some() && sfx::music_sample() == e.player.stream {
        sfx::stop_music();
    }
    if let Some(s) = e.player.stream.take() {
        sfx::release(s);
    }
    e.player.gen += 1;
    e.player.loading = false;
    let pos = e.local.now_ms();
    e.local.at(pos, false);
}

/// The engine loop's turn: start playing once enough is decoded, next song at the end.
pub fn tick(e: &mut Engine) {
    let Some(s) = e.player.stream else { return };
    let rate = e.player.rate.max(1) as usize;
    if !e.player.started && e.player.want_play && (sfx::stream_end(s) >= rate * 2 || e.player.complete) {
        let vol = e.local.volume as f32 / 65535.0;
        sfx::set_music_volume(vol);
        if sfx::play(s, 1.0, 0.0, false, 1.0, true).is_some() {
            e.player.started = true;
            e.local.at(0, true);
        }
    }
    if e.player.started {
        // the clock follows the mixer
        if sfx::music_sample() == Some(s) {
            let pos = (sfx::music_pos() * 1000.0) as u32;
            let playing = sfx::music_playing();
            e.local.at(pos, playing);
        } else if e.player.complete && e.local.playing {
            // the voice ran to the end of the song
            skip(e, 1);
        } else if !e.player.complete && e.local.playing && sfx::music_sample().is_none() {
            // decoding fell behind the voice: carry on from where it stopped
            let at = e.local.pos as f64 / 1000.0;
            if sfx::stream_end(s) as f64 / rate as f64 > at + 2.0 && sfx::play(s, 1.0, 0.0, false, 1.0, true).is_some() {
                sfx::seek_music(at);
            }
        }
    }
}

/// Load the queue's song `i` (a green thread of its own).
pub fn start_load(e: &mut Engine, i: usize) {
    stop(e);
    let Some(uri) = e.player.queue.get(i).cloned() else { return };
    e.player.idx = i;
    e.player.gen += 1;
    e.player.loading = true;
    e.player.want_play = true;
    e.player.started = false;
    e.player.complete = false;
    e.player.note.clear();
    e.local.uri = uri.clone();
    e.local.active = true;
    e.local.at(0, false);
    let id = e.id;
    let g = e.player.gen;
    let spawned = crate::coro::spawn(
        "lingspot_load",
        false,
        Box::new(move || {
            if let Err(why) = load(id, g, &uri) {
                log(&format!("player: {uri}: {why}"));
                if let Some(e) = eng(id) {
                    if e.player.gen == g {
                        e.player.loading = false;
                        e.player.note = why;
                        // a song that cannot play: the next one
                        if e.player.idx + 1 < e.player.queue.len() {
                            let n = e.player.idx + 1;
                            start_load(e, n);
                        }
                    }
                }
            }
        }),
    );
    if spawned == 0 {
        e.player.loading = false;
    }
}

fn current(id: i64, g: u64) -> Result<&'static mut Engine, String> {
    match eng(id) {
        Some(e) if e.player.gen == g => Ok(e),
        _ => Err(String::from("overtaken")),
    }
}

/// What a song is, from its spclient metadata (protobuf Track: name 2, album 3 {name 2,
/// cover_group 17 {image 1 {file_id 1, size 2}}, cover 9}, artist 4 {name 2}, duration 7 sint32).
#[derive(Default, Clone)]
pub struct Meta {
    pub name: String,
    pub artists: String,
    pub album: String,
    pub cover: String,
    pub duration: u32,
}

pub fn meta_of(t: &[u8]) -> Meta {
    let mut m = Meta { name: pb::get_str(t, 2), ..Default::default() };
    m.artists = pb::get_all_b(t, 4).iter().map(|a| pb::get_str(a, 2)).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
    if let Some(al) = pb::get_b(t, 3) {
        m.album = pb::get_str(al, 2);
        // the default (300 px) picture first, else the first there is
        let mut imgs: Vec<&[u8]> = pb::get_b(al, 17).map(|cg| pb::get_all_b(cg, 1)).unwrap_or_default();
        if imgs.is_empty() {
            imgs = pb::get_all_b(al, 9);
        }
        let pick = imgs.iter().find(|i| pb::get_n(i, 2).unwrap_or(0) == 0).or(imgs.first());
        if let Some(fid) = pick.and_then(|i| pb::get_b(i, 1)) {
            m.cover = format!("https://i.scdn.co/image/{}", hex(fid));
        }
    }
    let z = pb::get_n(t, 7).unwrap_or(0);
    m.duration = ((z >> 1) as i64 ^ -((z & 1) as i64)).max(0) as u32;
    m
}

/// A song's metadata by uri (spclient), for the Connect view of what another device plays.
pub fn track_meta(id: i64, uri: &str) -> Option<Meta> {
    let gid = gid_of(id_of(uri))?;
    let (code, meta) = spclient_get(id, &format!("/metadata/4/track/{}?market=from_token", hex(&gid)), "application/x-protobuf").ok()?;
    (code == 200).then(|| meta_of(&meta))
}

/// The songs of an album, playlist or artist uri: spclient's context-resolve (what Spotify's
/// own players and librespot use), else the Web API.
pub fn context_tracks(id: i64, ctx: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok((200, b)) = spclient_get(id, &format!("/context-resolve/v1/{ctx}"), "application/json") {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
            for page in v["pages"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                for t in page["tracks"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                    if let Some(u) = t["uri"].as_str() {
                        if u.starts_with("spotify:track:") && out.len() < 500 {
                            out.push(String::from(u));
                        }
                    }
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    let Ok(tok) = super::token(id) else { return out };
    let cid = id_of(ctx);
    let (path, items, field) = if ctx.contains(":album:") {
        (format!("/v1/albums/{cid}/tracks?limit=50"), "items", None)
    } else if ctx.contains(":playlist:") {
        (format!("/v1/playlists/{cid}/tracks?limit=100&fields=items(track(uri))"), "items", Some("track"))
    } else if ctx.contains(":artist:") {
        (format!("/v1/artists/{cid}/top-tracks"), "tracks", None)
    } else {
        return out;
    };
    let (code, body) = super::web::api(&tok, "GET", &path, "");
    if code != 200 {
        return out;
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
        if let Some(a) = v[items].as_array() {
            for it in a {
                let t = match field {
                    Some(f) => &it[f],
                    None => it,
                };
                if let Some(u) = t["uri"].as_str() {
                    if u.starts_with("spotify:track:") {
                        out.push(String::from(u));
                    }
                }
            }
        }
    }
    out
}

fn spclient_get(id: i64, path: &str, accept: &str) -> Result<(u16, Vec<u8>), String> {
    let tok = super::token(id)?;
    let host = super::spclient(id);
    let ct = eng(id).map(|e| e.client_token.clone()).unwrap_or_default();
    let headers = format!("Authorization: Bearer {tok}\nclient-token: {ct}\nAccept: {accept}\nUser-Agent: {}", super::web::user_agent());
    let (code, _, b) = crate::lingsock::request_bytes("GET", &format!("https://{host}{path}"), &headers, b"")?;
    Ok((code, b))
}

fn load(id: i64, g: u64, uri: &str) -> Result<(), String> {
    let tid = id_of(uri);
    if !uri.starts_with("spotify:track:") {
        return Err(String::from("only songs play in Horizon (not episodes)"));
    }
    let gid = gid_of(tid).ok_or("not a track id")?;
    // what it is and its audio files: spclient metadata (protobuf Track) -- not the Web API,
    // whose shared-client quota answered 429 to every call (the names were one call per song)
    let (code, meta) = spclient_get(id, &format!("/metadata/4/track/{}?market=from_token", hex(&gid)), "application/x-protobuf")?;
    if code != 200 {
        return Err(format!("track metadata: HTTP {code}"));
    }
    {
        let m = meta_of(&meta);
        let e = current(id, g)?;
        e.local.name = m.name;
        e.local.artists = m.artists;
        e.local.album = m.album;
        e.local.cover = m.cover;
        e.local.duration = m.duration;
    }
    fn collect(t: &[u8], files: &mut Vec<(Vec<u8>, u64)>) {
        for f in pb::get_all_b(t, 12) {
            if let (Some(fid), Some(fmt)) = (pb::get_b(f, 1), pb::get_n(f, 2)) {
                files.push((fid.to_vec(), fmt));
            }
        }
    }
    let mut files: Vec<(Vec<u8>, u64)> = Vec::new();
    collect(&meta, &mut files);
    if files.is_empty() {
        for alt in pb::get_all_b(&meta, 13) {
            collect(alt, &mut files);
        }
    }
    // Ogg Vorbis, 160 kbit/s first (then 320, then 96)
    let pick = [1u64, 2, 0].iter().find_map(|want| files.iter().find(|f| f.1 == *want)).ok_or("no Ogg Vorbis file for this song")?.clone();
    let fid = pick.0;
    // its key, over the access point
    let key = audio_key(id, g, &fid, &gid)?;
    // where it is
    let (code, sr) = spclient_get(id, &format!("/storage-resolve/files/audio/interactive/{}", hex(&fid)), "application/x-protobuf")?;
    if code != 200 {
        return Err(format!("storage-resolve: HTTP {code}"));
    }
    let urls: Vec<String> = pb::get_all_b(&sr, 2).iter().map(|u| String::from_utf8_lossy(u).into_owned()).collect();
    let mut data = None;
    let mut why = String::from("no CDN address");
    for u in urls.iter().take(3) {
        current(id, g)?;
        match crate::lingsock::request_bytes("GET", u, &format!("User-Agent: {}", super::web::user_agent()), b"") {
            Ok((200, _, b)) | Ok((206, _, b)) if b.len() > OGG_HEADER_END => {
                data = Some(b);
                break;
            },
            Ok((c, _, _)) => why = format!("CDN: HTTP {c}"),
            Err(e) => why = e,
        }
    }
    let mut data = data.ok_or(why)?;
    super::zeroconf::aes128_ctr(&key, &AUDIO_AESIV, &mut data);
    let ogg = &data[OGG_HEADER_END..];
    if ogg.len() < 4 || &ogg[..4] != b"OggS" {
        return Err(String::from("the decrypted file is not Ogg (wrong key?)"));
    }
    decode_into(id, g, ogg)
}

/// The file's AES key: RequestKey (file id, track gid, sequence, 0) -> AesKey.
fn audio_key(id: i64, g: u64, fid: &[u8], gid: &[u8; 16]) -> Result<[u8; 16], String> {
    let e = current(id, g)?;
    let seq = e.key_seq;
    e.key_seq += 1;
    let mut req = Vec::with_capacity(42);
    req.extend_from_slice(fid);
    req.extend_from_slice(gid);
    req.extend_from_slice(&seq.to_be_bytes());
    req.extend_from_slice(&[0, 0]);
    let Some(ap) = e.ap.as_mut() else { return Err(String::from("no access point session")) };
    if !ap.send(super::ap::CMD_REQUEST_KEY, &req) {
        return Err(String::from("could not ask for the song's key"));
    }
    let t0 = crate::arch::timer::now_ms();
    loop {
        let e = current(id, g)?;
        if let Some(i) = e.keys.iter().position(|k| k.0 == seq) {
            let (_, r) = e.keys.remove(i);
            return r.ok_or_else(|| String::from("Spotify refused the song's key"));
        }
        if crate::arch::timer::now_ms().saturating_sub(t0) > 4000 {
            return Err(String::from("no answer for the song's key"));
        }
        crate::coro::sleep_ms(20);
    }
}

/// Ogg pages -> Vorbis packets -> PCM pushed into a streamed sound, yielding as it goes.
fn decode_into(id: i64, g: u64, data: &[u8]) -> Result<(), String> {
    use crate::codec::vorbis::{audio, header, samples};
    let packets = ogg_packets(data);
    if packets.len() < 3 {
        return Err(String::from("no Vorbis headers"));
    }
    let ident = header::read_header_ident(&packets[0]).map_err(|_| String::from("bad Vorbis ident header"))?;
    let setup = header::read_header_setup(&packets[2], ident.audio_channels, (ident.blocksize_0, ident.blocksize_1)).map_err(|_| String::from("bad Vorbis setup header"))?;
    let ch = ident.audio_channels.clamp(1, 2);
    let stereo = ch == 2;
    let s = sfx::stream_open(ident.audio_sample_rate, stereo);
    {
        let e = current(id, g).inspect_err(|_| sfx::release(s))?;
        e.player.stream = Some(s);
        e.player.rate = ident.audio_sample_rate;
        e.player.loading = false;
    }
    let mut pwr = audio::PreviousWindowRight::new();
    let mut buf: Vec<i16> = Vec::with_capacity(16384);
    for p in &packets[3..] {
        if let Ok(x) = audio::read_audio_packet_generic::<samples::InterleavedSamples<i16>>(&ident, &setup, p, &mut pwr) {
            if ident.audio_channels > 2 {
                let n = ident.audio_channels as usize;
                for f in x.samples.chunks_exact(n) {
                    buf.push(f[0]);
                    buf.push(f[1]);
                }
            } else {
                buf.extend_from_slice(&x.samples);
            }
        }
        if buf.len() >= 16384 {
            if current(id, g).is_err() {
                return Err(String::from("overtaken"));
            }
            sfx::stream_push(s, &buf);
            buf.clear();
            // the desktop and the other green threads get their turn
            crate::coro::sleep_ms(0);
        }
    }
    sfx::stream_push(s, &buf);
    sfx::stream_close(s);
    let e = current(id, g)?;
    e.player.complete = true;
    if e.local.duration == 0 {
        e.local.duration = (sfx::stream_end(s) as u64 * 1000 / ident.audio_sample_rate.max(1) as u64) as u32;
    }
    Ok(())
}

/// The packets of the first logical stream (as codec/vorbis does).
fn ogg_packets(data: &[u8]) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut serial: Option<u32> = None;
    let mut off = 0usize;
    while off + 27 <= data.len() {
        if &data[off..off + 4] != b"OggS" {
            match data[off + 1..].windows(4).position(|w| w == b"OggS") {
                Some(p) => {
                    off += 1 + p;
                    continue;
                },
                None => break,
            }
        }
        let sn = u32::from_le_bytes([data[off + 14], data[off + 15], data[off + 16], data[off + 17]]);
        let nseg = data[off + 26] as usize;
        let table = off + 27;
        if table + nseg > data.len() {
            break;
        }
        let mut body = table + nseg;
        let this = *serial.get_or_insert(sn) == sn;
        for k in 0..nseg {
            let l = data[table + k] as usize;
            if body + l > data.len() {
                return packets;
            }
            if this {
                cur.extend_from_slice(&data[body..body + l]);
                if l < 255 {
                    packets.push(core::mem::take(&mut cur));
                }
            }
            body += l;
        }
        off = body;
    }
    packets
}
