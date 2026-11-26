//! Spotify's HTTPS side: a client token (clienttoken.spotify.com, with its
//! hash-cash challenge), a Web API access token from login5 (the session's
//! stored credential), and the Web API calls Horizon makes through the engine.
//! All protobufs are written by hand (pb.rs); requests go over lingsock's
//! TLS streams.

use super::ap::{CLIENT_ID, SPOTIFY_SEMVER, SPOTIFY_VERSION};
use super::pb::{self, W};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub fn user_agent() -> String {
    format!("Spotify/{SPOTIFY_VERSION} Linux/0 (librespot-lingos)")
}

fn post_pb(url: &str, extra: &str, body: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let headers = format!("User-Agent: {}\nAccept: application/x-protobuf\nContent-Type: application/x-protobuf\n{extra}", user_agent());
    let (code, _head, b) = crate::lingsock::request_bytes("POST", url, &headers, body)?;
    Ok((code, b))
}

/// SHA-1 hash cash, as librespot's util::solve_hash_cash: a 16-byte suffix
/// (target + counter, counter, both big endian) whose SHA-1 with the prefix has
/// `length` trailing zero bits in bytes 12..20.
pub fn hash_cash(ctx: &[u8], prefix: &[u8], length: u32) -> Option<[u8; 16]> {
    use sha1::Digest;
    let md = sha1::Sha1::digest(ctx);
    let target = i64::from_be_bytes(md[12..20].try_into().ok()?);
    let mut counter: i64 = 0;
    while counter < 50_000_000 {
        let mut suffix = [0u8; 16];
        suffix[..8].copy_from_slice(&target.wrapping_add(counter).to_be_bytes());
        suffix[8..].copy_from_slice(&counter.to_be_bytes());
        let mut h = sha1::Sha1::new();
        h.update(prefix);
        h.update(suffix);
        let d = h.finalize();
        let v = i64::from_be_bytes(d[12..20].try_into().ok()?);
        if v.trailing_zeros() >= length {
            return Some(suffix);
        }
        counter += 1;
        if counter % 20_000 == 0 {
            crate::coro::maybe_preempt();
        }
    }
    None
}

fn hex_upper(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()).collect()
}

/// A client token (what login5 and spclient want in `client-token`).
pub fn client_token(device_id: &str) -> Result<String, String> {
    // ClientTokenRequest { request_type: CLIENT_DATA, client_data { client_version, client_id,
    //   connectivity_sdk_data { platform_specific_data { desktop_linux {...} }, device_id } } }
    let mut linux = W::new();
    linux.str(1, "Linux").str(2, "0").str(3, "0").str(4, if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" });
    let mut psd = W::new();
    psd.msg(5, &linux);
    let mut sdk = W::new();
    sdk.msg(1, &psd).str(2, device_id);
    let mut cd = W::new();
    cd.str(1, SPOTIFY_SEMVER).str(2, CLIENT_ID).msg(3, &sdk);
    let mut req = W::new();
    req.uint(1, 1).msg(2, &cd);
    let mut body = req.done();
    for _ in 0..3 {
        let (code, resp) = post_pb("https://clienttoken.spotify.com/v1/clienttoken", "", &body)?;
        if code != 200 {
            return Err(format!("clienttoken answered HTTP {code}: {}", String::from_utf8_lossy(&resp).chars().take(120).collect::<String>()));
        }
        match pb::get_n(&resp, 1).unwrap_or(0) {
            1 => {
                let g = pb::get_b(&resp, 2).ok_or("no granted token")?;
                let t = pb::get_str(g, 1);
                if t.is_empty() {
                    return Err(String::from("an empty client token"));
                }
                return Ok(t);
            },
            2 => {
                // ChallengesResponse { state, challenges[{ type, evaluate_hashcash_parameters { length, prefix } }] }
                let ch = pb::get_b(&resp, 3).ok_or("no challenges")?;
                let state = pb::get_str(ch, 1);
                let c0 = pb::get_b(ch, 2).ok_or("an empty challenge list")?;
                let hp = pb::get_b(c0, 4).ok_or("not a hash-cash challenge")?;
                let len = pb::get_n(hp, 1).unwrap_or(0) as u32;
                let prefix = unhex(&pb::get_str(hp, 2));
                let suffix = hash_cash(&[], &prefix, len).ok_or("hash cash not solved")?;
                let mut hc = W::new();
                hc.str(1, &hex_upper(&suffix));
                let mut ans = W::new();
                ans.uint(1, 3).msg(4, &hc);
                let mut car = W::new();
                car.str(1, &state).msg(2, &ans);
                let mut r = W::new();
                r.uint(1, 2).msg(3, &car);
                body = r.done();
            },
            t => return Err(format!("clienttoken: response type {t}")),
        }
    }
    Err(String::from("clienttoken: too many challenges"))
}

/// What login5 answered.
pub enum Login5 {
    /// access token, expires in (s), username
    Ok(String, u64, String),
    Error(u64),
}

pub fn login5_error(code: u64) -> &'static str {
    match code {
        1 => "INVALID_CREDENTIALS",
        2 => "BAD_REQUEST",
        3 => "UNSUPPORTED_LOGIN_PROTOCOL",
        4 => "TIMEOUT",
        5 => "UNKNOWN_IDENTIFIER",
        6 => "TOO_MANY_ATTEMPTS",
        7 => "INVALID_PHONENUMBER",
        8 => "TRY_AGAIN_LATER",
        _ => "UNKNOWN_ERROR",
    }
}

/// login5 with the session's stored credential -> a Web API access token.
pub fn login5(client_token: &str, device_id: &str, username: &str, stored: &[u8]) -> Result<Login5, String> {
    let mut ci = W::new();
    ci.str(1, CLIENT_ID).str(2, device_id);
    let mut sc = W::new();
    sc.str(1, username).bytes(2, stored);
    let mut ctx: Vec<u8> = Vec::new();
    let mut solutions: Option<W> = None;
    for _ in 0..3 {
        let mut req = W::new();
        req.msg(1, &ci);
        if !ctx.is_empty() {
            req.bytes(2, &ctx);
        }
        if let Some(s) = &solutions {
            req.msg(3, s);
        }
        req.msg(100, &sc);
        let (code, resp) = post_pb("https://login5.spotify.com/v3/login", &format!("client-token: {client_token}"), &req.0)?;
        if code != 200 {
            return Err(format!("login5 answered HTTP {code}"));
        }
        if let Some(ok) = pb::get_b(&resp, 1) {
            return Ok(Login5::Ok(pb::get_str(ok, 2), pb::get_n(ok, 4).unwrap_or(3600), pb::get_str(ok, 1)));
        }
        if let Some(e) = pb::get_n(&resp, 2) {
            if e != 4 && e != 6 {
                return Ok(Login5::Error(e));
            }
            crate::coro::sleep_ms(3000);
        }
        if let Some(chs) = pb::get_b(&resp, 3) {
            let lctx = pb::get_b(&resp, 5).unwrap_or(&[]).to_vec();
            let mut sols = W::new();
            for c in pb::get_all_b(chs, 1) {
                if pb::get_b(c, 2).is_some() {
                    return Err(String::from("login5 wants a code challenge"));
                }
                let Some(hc) = pb::get_b(c, 1) else { continue };
                let prefix = pb::get_b(hc, 1).unwrap_or(&[]).to_vec();
                let len = pb::get_n(hc, 2).unwrap_or(0) as u32;
                let t0 = crate::arch::timer::now_ms();
                let suffix = hash_cash(&lctx, &prefix, len).ok_or("login5 hash cash not solved")?;
                let ms = crate::arch::timer::now_ms().saturating_sub(t0);
                let mut dur = W::new();
                dur.int(1, (ms / 1000) as i64).int(2, ((ms % 1000) * 1_000_000) as i64);
                let mut hs = W::new();
                hs.bytes(1, &suffix).msg(2, &dur);
                let mut sol = W::new();
                sol.msg(1, &hs);
                sols.msg(1, &sol);
            }
            solutions = Some(sols);
            ctx = lctx;
        }
    }
    Err(String::from("login5: no answer after its challenges"))
}

/// Until when (timer ms) Spotify told us to stay away from the Web API (its 429 Retry-After).
/// Every call in that time is answered here, without a request: a request sent during the
/// cooldown (the 3 s player poll, a page's next GET) counts against the same rolling window
/// and earned a fresh "wait 8 s" for the click that came after the wait.
static mut COOL_UNTIL_MS: u64 = 0;
/// Web API requests actually sent (for the 429 log line).
static mut SENT: u64 = 0;

/// Web API requests sent this boot.
pub fn sent() -> u64 {
    unsafe { SENT }
}

/// Seconds left of a Spotify Web API cooldown (0 = none).
pub fn cooldown_secs() -> u64 {
    let now = crate::arch::timer::now_ms();
    let until = unsafe { COOL_UNTIL_MS };
    if until > now { (until - now).div_ceil(1000) } else { 0 }
}

/// One Web API call with the session's token: (status, body).
pub fn api(token: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    let m = match method {
        "PUT" | "POST" | "DELETE" => method,
        _ => "GET",
    };
    let left = cooldown_secs();
    if left > 0 {
        return (429, err_body(&format!("Spotify asks Horizon to wait {left} s")));
    }
    unsafe { SENT += 1 };
    let headers = format!("Authorization: Bearer {token}\nContent-Type: application/json\nAccept: application/json\nUser-Agent: {}", user_agent());
    match crate::lingsock::request_bytes(m, &format!("https://api.spotify.com{path}"), &headers, body.as_bytes()) {
        Ok((code, head, b)) => {
            if code == 429 {
                let secs = head
                    .lines()
                    .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("retry-after")).and_then(|(_, v)| v.trim().parse::<u64>().ok()))
                    .unwrap_or(5)
                    .clamp(1, 3600);
                unsafe { COOL_UNTIL_MS = crate::arch::timer::now_ms() + secs * 1000 + 250 };
                super::log(&format!("web api: 429 on {m} {} -- Retry-After {secs} s ({} requests sent this boot)", path.split('?').next().unwrap_or(path), unsafe { SENT }));
                // a handful of requests and already limited: the quota is the shared client id's
                // (everyone's), not Horizon's -- say so instead of blaming the clicks
                let why = if unsafe { SENT } <= 20 { " (Spotify limits the app key Horizon shares with other players, not your clicks)" } else { "" };
                return (429, err_body(&format!("Spotify asks Horizon to wait {secs} s{why}")));
            }
            (code, String::from_utf8_lossy(&b).into_owned())
        },
        Err(e) => (0, err_body(&e)),
    }
}

pub fn err_body(why: &str) -> String {
    serde_json::json!({"error": {"message": why}}).to_string()
}
