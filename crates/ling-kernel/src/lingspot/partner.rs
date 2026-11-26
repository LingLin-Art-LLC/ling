//! Browsing without the public Web API. Horizon's pages ask for Web API paths (/v1/search,
//! /v1/albums/{id}, ...); api.spotify.com rate-limits the client id this engine's token is
//! minted for (Spotify's own desktop id, shared by every librespot-style client), so on a busy
//! day *every* request is answered 429 "wait N s" -- however few Horizon sends. Spotify's own
//! apps browse through "pathfinder" (api-partner.spotify.com, persisted GraphQL queries) with
//! the same kind of token, outside that quota. This turns the Web API GETs Horizon makes into
//! those queries and their answers back into the Web API's JSON shapes, so spotify.ling's pages
//! stay as they are. Anything not translated here, or a query Spotify does not answer (the
//! persisted-query hashes below change when Spotify ships a new web player), falls back to the
//! Web API in mod.rs.
//!
//! Hashes: from the public web player bundle (open.spotify.com, 2026-11-26).

use super::eng;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde_json::{json, Value};

const PF_URL: &str = "https://api-partner.spotify.com/pathfinder/v2/query";
const H_SEARCH: &str = "eef7cc54888d91bdd6802623477873caa3948ae173a0c34fd86827b267e94c03";
const H_ALBUM: &str = "0b6ce3511e1b02e4a40fa95440f992acbe85080eb308af9194322d708fc1914f";
const H_PLAYLIST: &str = "21ea11a6ac75a5f3e01fd365d29c18e0a4a8b7dc09f89bbb5d73910190c6acaf";
const H_ARTIST: &str = "eafb1620d783ece118e72fbfede2b0064483e029d3c1aafafa1c2de0e25a89e7";
const H_TRACK: &str = "e7b1bf4e95cb3d733d48b442f93006e9712fd504e8b88d7a82e6e3bac9306a6b";
const H_LIKED: &str = "087278b20b743578a6262c2b0b4bcd20d879c503cc359a2285baf083ef944240";
const H_LIBRARY: &str = "390c78e5b951029bad359785e69b07b536a509c581cbcd0aded5e5067f187455";
const H_PROFILE: &str = "08ffb4730af3746e04a8301396f20875dbbce10c75243803091a9274eacc8ac0";

/// Answers kept for a while: (engine, operation + variables, until (ms), data). One artist
/// overview serves three Web API paths, the library three more; and the same page twice is
/// one request.
static mut CACHE: Vec<(i64, String, u64, Value)> = Vec::new();
const TTL_MS: u64 = 180_000;
/// Pathfinder requests sent / failed this boot (for `spotify` and the log).
pub static mut SENT: u64 = 0;
pub static mut FAILED: u64 = 0;

fn cache() -> &'static mut Vec<(i64, String, u64, Value)> {
    unsafe { &mut *(&raw mut CACHE) }
}

pub fn forget(id: i64) {
    cache().retain(|c| c.0 != id);
}

/// One persisted query -> its `data` (Err: not answered; the caller falls back).
fn pf(id: i64, op: &str, hash: &str, vars: Value) -> Result<Value, String> {
    let key = format!("{op}{vars}");
    let now = crate::arch::timer::now_ms();
    cache().retain(|c| c.2 > now);
    if let Some(c) = cache().iter().find(|c| c.0 == id && c.1 == key) {
        return Ok(c.3.clone());
    }
    let tok = super::token(id)?;
    let ct = eng(id).map(|e| e.client_token.clone()).unwrap_or_default();
    let body = json!({"variables": vars, "operationName": op, "extensions": {"persistedQuery": {"version": 1, "sha256Hash": hash}}}).to_string();
    let headers = format!(
        "Authorization: Bearer {tok}\nclient-token: {ct}\nContent-Type: application/json;charset=UTF-8\nAccept: application/json\nApp-Platform: WebPlayer\nSpotify-App-Version: {}\nUser-Agent: {}",
        super::ap::SPOTIFY_SEMVER,
        super::web::user_agent()
    );
    unsafe { SENT += 1 };
    let r = crate::lingsock::request_bytes("POST", PF_URL, &headers, body.as_bytes());
    let (code, b) = match r {
        Ok((c, _, b)) => (c, b),
        Err(e) => {
            unsafe { FAILED += 1 };
            return Err(e);
        },
    };
    let v: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
    let data = &v["data"];
    if code != 200 || !data.is_object() {
        unsafe { FAILED += 1 };
        let why = v["errors"][0]["message"].as_str().map(String::from).unwrap_or_else(|| String::from_utf8_lossy(&b[..b.len().min(120)]).into_owned());
        super::log(&format!("pathfinder {op}: HTTP {code} {why} -- the Web API instead"));
        return Err(format!("pathfinder {op}: HTTP {code}"));
    }
    cache().push((id, key, now + TTL_MS, data.clone()));
    if cache().len() > 64 {
        cache().remove(0);
    }
    Ok(data.clone())
}

// ── pathfinder objects -> Web API objects ──

fn id_of(uri: &str) -> &str {
    uri.rsplit(':').next().unwrap_or("")
}

/// `data` inside the wrappers pathfinder puts around entities ({item:{data}}, {data}, {itemV2:{data}}).
fn unwrap(v: &Value) -> &Value {
    for k in ["itemV2", "item"] {
        if v[k].is_object() {
            return unwrap(&v[k]);
        }
    }
    if v["data"].is_object() {
        return &v["data"];
    }
    v
}

/// sources [{url,width,height}] -> Web API images (largest first).
fn images(sources: &Value) -> Value {
    let mut s: Vec<(u64, String)> =
        sources.as_array().map(|a| a.iter().filter_map(|x| Some((x["width"].as_u64().unwrap_or(0), x["url"].as_str()?.to_string()))).collect()).unwrap_or_default();
    s.sort_by(|a, b| b.0.cmp(&a.0));
    Value::Array(s.into_iter().map(|(w, u)| json!({"url": u, "width": w, "height": w})).collect())
}

fn cover(v: &Value) -> Value {
    if v["coverArt"]["sources"].is_array() {
        return images(&v["coverArt"]["sources"]);
    }
    if v["visuals"]["avatarImage"]["sources"].is_array() {
        return images(&v["visuals"]["avatarImage"]["sources"]);
    }
    if v["images"]["items"][0]["sources"].is_array() {
        return images(&v["images"]["items"][0]["sources"]);
    }
    json!([])
}

fn artist_of(a: &Value) -> Value {
    let a = unwrap(a);
    let uri = a["uri"].as_str().or_else(|| a["_uri"].as_str()).unwrap_or("");
    json!({"id": id_of(uri), "uri": uri, "name": a["profile"]["name"].as_str().or_else(|| a["name"].as_str()).unwrap_or(""), "images": cover(a)})
}

fn artists(v: &Value) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for k in ["artists", "firstArtist", "otherArtists"] {
        if let Some(a) = v[k]["items"].as_array() {
            out.extend(a.iter().map(artist_of));
        }
    }
    Value::Array(out)
}

fn date(v: &Value) -> String {
    if let Some(s) = v["date"]["isoString"].as_str() {
        return s.chars().take(10).collect();
    }
    v["date"]["year"].as_u64().map(|y| y.to_string()).unwrap_or_default()
}

fn album_of(v: &Value) -> Value {
    let v = unwrap(v);
    let uri = v["uri"].as_str().or_else(|| v["_uri"].as_str()).unwrap_or("");
    json!({"id": id_of(uri), "uri": uri, "name": v["name"].as_str().unwrap_or(""), "images": cover(v), "artists": artists(v), "release_date": date(v)})
}

fn track_of(t: &Value) -> Value {
    // {_uri, data:{...}} (library), {track:{...}} (album / artist), wrappers (search, playlist)
    let outer_uri = t["_uri"].as_str().or_else(|| t["track"]["_uri"].as_str());
    let mut v = if t["track"].is_object() { &t["track"] } else { t };
    v = unwrap(v);
    let uri = v["uri"].as_str().or(outer_uri).unwrap_or("");
    let dur = v["duration"]["totalMilliseconds"].as_u64().or_else(|| v["trackDuration"]["totalMilliseconds"].as_u64()).unwrap_or(0);
    let album = if v["albumOfTrack"].is_object() { album_of(&v["albumOfTrack"]) } else { json!({"id": "", "name": "", "images": []}) };
    json!({
        "id": id_of(uri), "uri": uri, "name": v["name"].as_str().unwrap_or(""), "duration_ms": dur,
        "track_number": v["trackNumber"].as_u64().unwrap_or(0), "artists": artists(v), "album": album,
    })
}

fn playlist_of(v: &Value) -> Value {
    let v = unwrap(v);
    let uri = v["uri"].as_str().or_else(|| v["_uri"].as_str()).unwrap_or("");
    let owner = v["ownerV2"]["data"]["name"].as_str().or_else(|| v["owner"]["name"].as_str()).unwrap_or("");
    json!({"id": id_of(uri), "uri": uri, "name": v["name"].as_str().unwrap_or(""), "description": v["description"].as_str().unwrap_or(""), "images": cover(v), "owner": {"display_name": owner}})
}

fn list<'a>(v: &'a Value, path: &[&str]) -> Vec<&'a Value> {
    let mut x = v;
    for p in path {
        x = &x[*p];
    }
    x.as_array().map(|a| a.iter().collect()).unwrap_or_default()
}

fn qarg(q: &str, name: &str) -> String {
    for p in q.split('&') {
        if let Some(v) = p.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
            return url_decode(v);
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
                if let Ok(x) = u8::from_str_radix(core::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    out.push(x);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ── the pages ──

fn search(id: i64, q: &str, limit: u64) -> Result<Value, String> {
    let d = pf(
        id,
        "searchDesktop",
        H_SEARCH,
        json!({"searchTerm": q, "offset": 0, "limit": limit, "numberOfTopResults": 5, "includeAudiobooks": false, "includeArtistHasConcertsField": false,
               "includePreReleases": false, "includeLocalConcertsField": false, "includeAuthors": false, "includeAlbumPreReleases": false, "includeEpisodeContentRatingsV2": false}),
    )?;
    let s = &d["searchV2"];
    let tracks: Vec<Value> = list(s, &["tracksV2", "items"]).into_iter().map(track_of).filter(|t| t["uri"] != "").collect();
    let ar: Vec<Value> = list(s, &["artists", "items"]).into_iter().map(artist_of).filter(|a| a["id"] != "").collect();
    let mut al: Vec<Value> = list(s, &["albumsV2", "items"]).into_iter().map(album_of).filter(|a| a["id"] != "").collect();
    if al.is_empty() {
        al = list(s, &["albums", "items"]).into_iter().map(album_of).filter(|a| a["id"] != "").collect();
    }
    let pl: Vec<Value> = list(s, &["playlists", "items"]).into_iter().map(playlist_of).filter(|p| p["id"] != "").collect();
    if tracks.is_empty() && ar.is_empty() && al.is_empty() && pl.is_empty() && !s.is_object() {
        return Err(String::from("pathfinder search: no searchV2"));
    }
    Ok(json!({"tracks": {"items": tracks}, "artists": {"items": ar}, "albums": {"items": al}, "playlists": {"items": pl}}))
}

fn album(id: i64, aid: &str) -> Result<Value, String> {
    let d = pf(id, "getAlbum", H_ALBUM, json!({"uri": format!("spotify:album:{aid}"), "locale": "", "offset": 0, "limit": 50}))?;
    let a = &d["albumUnion"];
    if !a.is_object() {
        return Err(String::from("no albumUnion"));
    }
    let mut v = album_of(a);
    let mut items: Vec<Value> = list(a, &["tracksV2", "items"]).into_iter().map(track_of).collect();
    if items.is_empty() {
        items = list(a, &["tracks", "items"]).into_iter().map(track_of).collect();
    }
    let total = items.len();
    v["tracks"] = json!({"items": items, "total": total});
    Ok(v)
}

fn playlist(id: i64, pid: &str) -> Result<Value, String> {
    let d = pf(
        id,
        "fetchPlaylist",
        H_PLAYLIST,
        json!({"uri": format!("spotify:playlist:{pid}"), "offset": 0, "limit": 100, "enableWatchFeedEntrypoint": false, "includeEpisodeContentRatingsV2": false}),
    )?;
    let p = &d["playlistV2"];
    if !p.is_object() {
        return Err(String::from("no playlistV2"));
    }
    let mut v = playlist_of(p);
    let items: Vec<Value> = list(p, &["content", "items"]).into_iter().map(|it| json!({"track": track_of(it)})).filter(|t| t["track"]["uri"] != "").collect();
    let total = p["content"]["totalCount"].as_u64().unwrap_or(items.len() as u64);
    v["tracks"] = json!({"items": items, "total": total});
    Ok(v)
}

fn artist_union(id: i64, aid: &str) -> Result<Value, String> {
    let d = pf(id, "queryArtistOverview", H_ARTIST, json!({"uri": format!("spotify:artist:{aid}"), "locale": "", "includePrerelease": false}))?;
    if !d["artistUnion"].is_object() {
        return Err(String::from("no artistUnion"));
    }
    Ok(d["artistUnion"].clone())
}

fn artist(id: i64, aid: &str, sub: &str) -> Result<Value, String> {
    let a = artist_union(id, aid)?;
    match sub {
        "" => {
            let mut v = artist_of(&a);
            v["followers"] = json!({"total": a["stats"]["followers"].as_u64().unwrap_or(0)});
            Ok(v)
        },
        "top-tracks" => Ok(json!({"tracks": list(&a, &["discography", "topTracks", "items"]).into_iter().map(track_of).collect::<Vec<_>>()})),
        "albums" => {
            let mut out = Vec::new();
            for group in ["albums", "singles"] {
                for it in list(&a, &["discography", group, "items"]) {
                    if let Some(r) = it["releases"]["items"].get(0) {
                        out.push(album_of(r));
                    }
                }
            }
            Ok(json!({"items": out}))
        },
        "related-artists" => Ok(json!({"artists": list(&a, &["relatedContent", "relatedArtists", "items"]).into_iter().map(artist_of).collect::<Vec<_>>()})),
        _ => Err(String::from("not translated")),
    }
}

fn track(id: i64, tid: &str) -> Result<Value, String> {
    let d = pf(id, "getTrack", H_TRACK, json!({"uri": format!("spotify:track:{tid}")}))?;
    let t = &d["trackUnion"];
    if !t.is_object() {
        return Err(String::from("no trackUnion"));
    }
    let mut v = track_of(t);
    if v["uri"] == "" {
        v["uri"] = json!(format!("spotify:track:{tid}"));
        v["id"] = json!(tid);
    }
    Ok(v)
}

fn liked(id: i64) -> Result<Value, String> {
    let d = pf(id, "fetchLibraryTracks", H_LIKED, json!({"offset": 0, "limit": 50}))?;
    let t = &d["me"]["library"]["tracks"];
    if !t.is_object() {
        return Err(String::from("no library tracks"));
    }
    Ok(json!({"items": list(t, &["items"]).into_iter().map(|it| json!({"track": track_of(it)})).filter(|t| t["track"]["uri"] != "").collect::<Vec<_>>()}))
}

/// The library (playlists, albums, artists in one list): what=playlists|albums|artists.
fn library(id: i64, what: &str) -> Result<Value, String> {
    let d = pf(
        id,
        "libraryV3",
        H_LIBRARY,
        json!({"filters": [], "order": null, "textFilter": "", "features": ["LIKED_SONGS", "YOUR_EPISODES"], "limit": 50, "offset": 0, "flatten": false, "expandedFolders": [], "folderUri": null, "includeFoldersWhenFlattening": true}),
    )?;
    let items = list(&d["me"]["libraryV3"], &["items"]);
    if !d["me"]["libraryV3"].is_object() {
        return Err(String::from("no libraryV3"));
    }
    let mut out = Vec::new();
    for it in items {
        let data = &it["item"]["data"];
        let uri = it["item"]["_uri"].as_str().unwrap_or("");
        let kind = data["__typename"].as_str().unwrap_or("");
        let mut x = data.clone();
        if x["uri"].is_null() {
            x["uri"] = json!(uri);
        }
        match (what, kind) {
            ("playlists", "Playlist") => out.push(playlist_of(&x)),
            ("albums", "Album") => out.push(json!({"album": album_of(&x)})),
            ("artists", "Artist") => out.push(artist_of(&x)),
            _ => {},
        }
    }
    Ok(if what == "artists" { json!({"artists": {"items": out}}) } else { json!({"items": out}) })
}

fn me(id: i64) -> Result<Value, String> {
    let d = pf(id, "profileAttributes", H_PROFILE, json!({}))?;
    let name = d["me"]["profile"]["name"].as_str().unwrap_or("");
    if name.is_empty() {
        return Err(String::from("no profile name"));
    }
    Ok(json!({"display_name": name}))
}

/// A Web API GET Horizon asked for, answered through pathfinder: Some((200, Web API JSON)),
/// or None (not one of these, or pathfinder did not answer: the Web API then).
pub fn route(id: i64, method: &str, path: &str) -> Option<(u16, String)> {
    if method != "GET" {
        return None;
    }
    let (p, q) = path.split_once('?').unwrap_or((path, ""));
    let seg: Vec<&str> = p.trim_start_matches("/v1/").split('/').collect();
    let r = match seg.as_slice() {
        ["search"] => search(id, &qarg(q, "q"), qarg(q, "limit").parse().unwrap_or(10).clamp(1, 50)),
        ["albums", a] => album(id, a),
        ["albums", a, "tracks"] => album(id, a).map(|v| v["tracks"].clone()),
        ["playlists", pl] => playlist(id, pl),
        ["playlists", pl, "tracks"] => playlist(id, pl).map(|v| v["tracks"].clone()),
        ["artists", a] => artist(id, a, ""),
        ["artists", a, sub] => artist(id, a, sub),
        ["tracks", t] => track(id, t),
        ["me"] => me(id),
        ["me", "tracks"] => liked(id),
        ["me", "playlists"] => library(id, "playlists"),
        ["me", "albums"] => library(id, "albums"),
        ["me", "following"] if qarg(q, "type") == "artist" => library(id, "artists"),
        _ => return None,
    };
    r.ok().map(|v| (200, v.to_string()))
}
