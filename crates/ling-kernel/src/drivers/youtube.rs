//! YouTube stream addresses, found by LingOS itself: no helper program, no
//! server on the home network. The kernel asks YouTube's own InnerTube API
//! (`POST https://www.youtube.com/youtubei/v1/player`) the way YouTube's apps
//! do, as a client whose answers carry plain stream addresses (no signature
//! cipher, no JavaScript "n" challenge, no PO token): the visionOS app, else
//! the Quest (ANDROID_VR) app. Which clients still answer that way changes
//! now and then; the set here follows yt-dlp's `_base.py` INNERTUBE_CLIENTS
//! (checked 2026-11: `visionos` is yt-dlp's default JS-less client).
//!
//! YouTube asks unknown callers to "sign in to confirm you're not a bot"
//! (LOGIN_REQUIRED) until they come with a visitor id; that answer itself
//! carries one (`responseContext.visitorData`), so the request is made again
//! with it, and the id is kept for the next videos.
//!
//! Pure logic only (request bodies, reading the answer, picking formats), so
//! a host test can run it against the real YouTube; the network calls are in
//! `videoplay.rs`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde_json::Value;

/// An InnerTube client: what YouTube's apps send about themselves.
pub struct Client {
    /// Short name for messages ("visionos").
    pub key: &'static str,
    pub name: &'static str,
    pub version: &'static str,
    /// `X-YouTube-Client-Name`
    pub num: u32,
    pub user_agent: &'static str,
    /// More `context.client` fields, as JSON members (`"deviceMake":"Apple",...`).
    pub extra: &'static str,
}

/// Tried in this order (each one once more with a visitor id when YouTube asks).
pub const CLIENTS: &[Client] = &[
    Client {
        key: "visionos",
        name: "VISIONOS",
        version: "1.02",
        num: 101,
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
        extra: r#""deviceMake":"Apple","deviceModel":"RealityDevice17,1","osName":"visionOS","osVersion":"26.5.23O471""#,
    },
    Client {
        key: "android_vr",
        name: "ANDROID_VR",
        version: "1.65.10",
        num: 28,
        user_agent: "com.google.android.apps.youtube.vr.oculus/1.65.10 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip",
        extra: r#""deviceMake":"Oculus","deviceModel":"Quest 3","androidSdkVersion":32,"osName":"Android","osVersion":"12L""#,
    },
    Client {
        key: "android_vr_1.62",
        name: "ANDROID_VR",
        version: "1.62.27",
        num: 28,
        user_agent: "com.google.android.apps.youtube.vr.oculus/1.62.27 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip",
        extra: r#""deviceMake":"Oculus","deviceModel":"Quest 3","androidSdkVersion":32,"osName":"Android","osVersion":"12L""#,
    },
];

/// Clients for a signed-in viewer (`ytauth.rs`: the user's own cookies), tried only when the plain
/// clients above met YouTube's "sign in to confirm you're not a bot". The plain ones do not take
/// cookies (yt-dlp: SUPPORTS_COOKIES false); yt-dlp's signed-in default starts with `tv_samsung`
/// (checked 2026-11, `_DEFAULT_AUTHED_CLIENTS`). Its answers often carry only
/// `signatureCipher`-ed streams, which need YouTube's player JavaScript to unlock -- LingOS has no
/// solver for that yet, so then [`parse_player`] says so (status "CIPHERED").
pub const AUTHED_CLIENTS: &[Client] = &[Client {
    key: "tv_samsung",
    name: "TVHTML5",
    version: "5.20260707",
    num: 7,
    user_agent: "Mozilla/5.0 (SMART-TV; Linux; Tizen 2.4.0) AppleWebKit/538.1 (KHTML, like Gecko) Version/2.4.0 TV Safari/538.1",
    extra: r#""deviceMake":"Samsung","deviceModel":"UKS9800","osName":"Tizen","osVersion":"2.4.0""#,
}];

/// The WEB client (only for the signed-in account check, `/account/account_menu`).
pub const WEB_VERSION: &str = "2.20260708.00.00";
pub const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";

/// "Sign in to confirm you're not a bot" (or another sign-in demand: LOGIN_REQUIRED).
pub fn is_bot_check(status: &str, reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    status == "LOGIN_REQUIRED" || r.contains("not a bot") || r.contains("sign in to confirm")
}

/// The account's name in an `/account/account_menu` answer (None: not signed in).
pub fn account_name(body: &[u8]) -> Option<String> {
    let j: Value = serde_json::from_slice(body).ok()?;
    let h = j.pointer("/actions/0/openPopupAction/popup/multiPageMenuRenderer/header/activeAccountHeaderRenderer")?;
    let text = |v: Option<&Value>| -> Option<String> {
        let v = v?;
        if let Some(t) = v.get("simpleText").and_then(|t| t.as_str()) {
            return Some(t.to_string());
        }
        let runs = v.get("runs")?.as_array()?;
        Some(runs.iter().filter_map(|r| r.get("text").and_then(|t| t.as_str())).collect())
    };
    let name = text(h.get("accountName")).filter(|n| !n.is_empty());
    let handle = text(h.get("channelHandle")).filter(|n| !n.is_empty());
    match (name, handle) {
        (Some(n), Some(hd)) => Some(format!("{n} ({hd})")),
        (Some(n), None) => Some(n),
        (None, Some(hd)) => Some(hd),
        (None, None) => Some("signed in".to_string()),
    }
}

pub const API_HOST: &str = "www.youtube.com";
pub const API_PATH: &str = "/youtubei/v1/player?prettyPrint=false";

/// The 11-character video id in a watch / share / shorts / embed address (or a bare id).
pub fn video_id(url: &str) -> Option<String> {
    let ok = |s: &str| s.len() >= 11 && s.as_bytes()[..11].iter().all(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    let u = url.trim();
    if u.len() == 11 && ok(u) {
        return Some(u.to_string());
    }
    for key in ["?v=", "&v=", "youtu.be/", "/shorts/", "/embed/", "/live/", "/v/"] {
        if let Some(i) = u.find(key) {
            let rest = &u[i + key.len()..];
            if ok(rest) {
                return Some(rest[..11].to_string());
            }
        }
    }
    None
}

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// The `/player` request: (extra header lines, JSON body).
pub fn player_request(c: &Client, id: &str, visitor: &str) -> (String, String) {
    let mut client = format!(r#""clientName":"{}","clientVersion":"{}","userAgent":{},"hl":"en","gl":"US",{}"#, c.name, c.version, json_str(c.user_agent), c.extra);
    if !visitor.is_empty() {
        client.push_str(&format!(r#","visitorData":{}"#, json_str(visitor)));
    }
    let body = format!(
        r#"{{"context":{{"client":{{{client}}}}},"videoId":{},"playbackContext":{{"contentPlaybackContext":{{"html5Preference":"HTML5_PREF_WANTS"}}}},"contentCheckOk":true,"racyCheckOk":true}}"#,
        json_str(id)
    );
    let mut headers = format!("X-YouTube-Client-Name: {}\r\nX-YouTube-Client-Version: {}\r\nOrigin: https://www.youtube.com\r\n", c.num, c.version);
    if !visitor.is_empty() {
        headers.push_str(&format!("X-Goog-Visitor-Id: {visitor}\r\n"));
    }
    (headers, body)
}

/// One stream YouTube offers.
#[derive(Clone, Default, Debug)]
pub struct Format {
    pub itag: u32,
    pub url: String,
    pub mime: String,
    pub codecs: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub bitrate: u64,
    /// Bytes (0 = not said).
    pub len: u64,
    /// Inclusive byte ranges of the init segment (moov) and the index (sidx); (0, 0) = none.
    pub init: (u64, u64),
    pub index: (u64, u64),
}

impl Format {
    pub fn has_ranges(&self) -> bool {
        self.index.1 > self.index.0 && self.init.1 > 0
    }
}

/// What a `/player` answer gave.
#[derive(Default, Debug)]
pub struct Streams {
    pub client: &'static str,
    pub title: String,
    pub duration: f64,
    pub live: bool,
    /// H.264 picture (adaptive, fragmented MP4 with a sidx).
    pub video: Option<Format>,
    /// AAC-LC sound (adaptive, itag 140 when there).
    pub audio: Option<Format>,
    /// itag 18: 360p H.264 + AAC in one (not fragmented) file.
    pub progressive: Option<Format>,
    /// Every H.264 height up to `video`'s (one stream each, picked like `video`), lowest first:
    /// the player starts low and steps up as the measured throughput allows.
    pub ladder: Vec<Format>,
}

/// Why a `/player` answer was not playable, and the visitor id it carried.
pub struct Refusal {
    pub status: String,
    pub reason: String,
    pub visitor: String,
}

fn range(v: &Value) -> (u64, u64) {
    let n = |k: &str| v.get(k).and_then(|x| x.as_str()).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    (n("start"), n("end"))
}

fn format_of(v: &Value) -> Option<Format> {
    let url = v.get("url")?.as_str()?.to_string();
    let mime_full = v.get("mimeType").and_then(|m| m.as_str()).unwrap_or("");
    let (mime, codecs) = match mime_full.split_once(';') {
        Some((m, rest)) => (m.trim().to_string(), rest.split_once('=').map(|(_, c)| c.trim().trim_matches('"').to_string()).unwrap_or_default()),
        None => (mime_full.trim().to_string(), String::new()),
    };
    let num = |k: &str| v.get(k).and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0);
    Some(Format {
        itag: num("itag") as u32,
        url,
        mime,
        codecs,
        width: num("width") as u32,
        height: num("height") as u32,
        fps: v.get("fps").and_then(|x| x.as_f64()).unwrap_or(0.0),
        bitrate: num("bitrate"),
        len: num("contentLength"),
        init: v.get("initRange").map(range).unwrap_or((0, 0)),
        index: v.get("indexRange").map(range).unwrap_or((0, 0)),
    })
}

/// A sound track that is the video's own (not a dubbed language, not the
/// "stable volume" DRC copy).
fn plain_audio(v: &Value) -> bool {
    if v.get("isDrc").and_then(|x| x.as_bool()).unwrap_or(false) {
        return false;
    }
    match v.get("audioTrack") {
        Some(t) => t.get("audioIsDefault").and_then(|x| x.as_bool()).unwrap_or(true),
        None => true,
    }
}

/// Read a `/player` answer and pick the streams: the tallest H.264 picture at
/// most `max_h` high (30 fps or less preferred: half the decoding), AAC-LC
/// sound (itag 140 first), and the itag 18 file when offered.
pub fn parse_player(body: &[u8], client: &'static str, max_h: u32) -> Result<Streams, Refusal> {
    let j: Value = serde_json::from_slice(body).map_err(|_| Refusal { status: "BAD_JSON".to_string(), reason: "YouTube's answer was not JSON".to_string(), visitor: String::new() })?;
    let visitor = j.pointer("/responseContext/visitorData").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let ps = j.get("playabilityStatus");
    let status = ps.and_then(|p| p.get("status")).and_then(|s| s.as_str()).unwrap_or("").to_string();
    if status != "OK" {
        let reason = ps.and_then(|p| p.get("reason")).and_then(|s| s.as_str()).unwrap_or("YouTube did not give this video").to_string();
        return Err(Refusal { status, reason, visitor });
    }
    let mut out = Streams { client, ..Default::default() };
    if let Some(d) = j.get("videoDetails") {
        out.title = d.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string();
        out.duration = d.get("lengthSeconds").and_then(|t| t.as_str()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        out.live = d.get("isLive").and_then(|t| t.as_bool()).unwrap_or(false);
    }
    let Some(sd) = j.get("streamingData") else {
        return Err(Refusal { status: "NO_STREAMS".to_string(), reason: "YouTube gave no streams for this video".to_string(), visitor });
    };
    let empty = Vec::new();
    let adaptive = sd.get("adaptiveFormats").and_then(|a| a.as_array()).unwrap_or(&empty);
    let mut best_v: Option<(bool, u32, u64, Format)> = None;
    let mut best_a: Option<(bool, u64, Format)> = None;
    // the best stream of each height (same preference as `video`): the adaptive ladder
    let mut per_h: Vec<((bool, u64), Format)> = Vec::new();
    for f in adaptive {
        let Some(fm) = format_of(f) else { continue };
        if !fm.has_ranges() {
            continue; // OTF / live: no sidx to walk
        }
        if fm.mime == "video/mp4" && fm.codecs.starts_with("avc1") && fm.height > 0 && fm.height <= max_h {
            let key = (fm.fps <= 31.0, fm.height, u64::MAX - fm.bitrate);
            let hk = (key.0, key.2);
            match per_h.iter_mut().find(|(_, g)| g.height == fm.height) {
                Some(e) if e.0 < hk => *e = (hk, fm.clone()),
                Some(_) => {},
                None => per_h.push((hk, fm.clone())),
            }
            if best_v.as_ref().is_none_or(|b| (b.0, b.1, b.2) < key) {
                best_v = Some((key.0, key.1, key.2, fm));
            }
        } else if fm.mime == "audio/mp4" && fm.codecs == "mp4a.40.2" && plain_audio(f) {
            let key = (fm.itag == 140, fm.bitrate);
            if best_a.as_ref().is_none_or(|b| (b.0, b.1) < key) {
                best_a = Some((key.0, key.1, fm));
            }
        }
    }
    out.video = best_v.map(|b| b.3);
    out.audio = best_a.map(|b| b.2);
    // up to the chosen stream's height (a 60 fps rung above a 30 fps choice is left out too)
    let top = out.video.as_ref().map(|v| (v.height, v.fps)).unwrap_or((0, 0.0));
    let mut ladder: Vec<Format> = per_h.into_iter().map(|(_, f)| f).filter(|f| f.height <= top.0 && (f.fps <= 31.0 || top.1 > 31.0)).collect();
    ladder.sort_by_key(|f| f.height);
    out.ladder = ladder;
    if let Some(progs) = sd.get("formats").and_then(|a| a.as_array()) {
        out.progressive = progs.iter().filter_map(format_of).find(|f| f.itag == 18);
    }
    if out.duration <= 0.0 {
        let ms = adaptive.iter().find_map(|f| f.get("approxDurationMs").and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok()));
        out.duration = ms.unwrap_or(0.0) / 1000.0;
    }
    if out.video.is_none() && out.progressive.is_none() {
        let ciphered = |v: &Value| v.get("url").is_none() && (v.get("signatureCipher").is_some() || v.get("cipher").is_some());
        if adaptive.iter().any(ciphered) || sd.get("formats").and_then(|a| a.as_array()).is_some_and(|a| a.iter().any(ciphered)) {
            return Err(Refusal {
                status: "CIPHERED".to_string(),
                reason: "YouTube gave this signed-in client only locked (signature-ciphered) streams, which need YouTube's player JavaScript -- LingOS cannot unlock those yet".to_string(),
                visitor,
            });
        }
        return Err(Refusal { status: "NO_H264".to_string(), reason: "YouTube offered no H.264 stream LingOS can play for this video".to_string(), visitor });
    }
    Ok(out)
}

/// `url` with `&range=a-b` (YouTube's own way: no throttling, unlike a Range header on adaptive streams).
pub fn with_range(url: &str, a: u64, b: u64) -> String {
    format!("{url}{}range={a}-{b}", if url.contains('?') { '&' } else { '?' })
}

/// A short tag for logs: never the whole address (it carries an IP-bound token).
pub fn short(f: &Format) -> String {
    format!("itag {} {}x{}{}", f.itag, f.width, f.height, if f.fps > 0.0 { format!("@{}", f.fps as u32) } else { String::new() })
}
