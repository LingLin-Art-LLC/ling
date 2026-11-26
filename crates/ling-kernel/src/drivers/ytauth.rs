//! YouTube sign-in for each LingOS user: the cookies of the user's own signed-in browser
//! (`cookies.txt`, see `ytcookies.rs` for why), kept per account and sealed.
//!
//! * Stored at `home/<user>/Programs/Horizon/youtube-cookies`, a private path: `vfs::write` seals
//!   it with the account's keyring key (and refuses while the keyring is locked), so no readable
//!   copy is ever on the disk. Only youtube.com / google.com cookies are kept.
//! * Sent with YouTube's InnerTube requests (`videoplay::post_player`: playback and downloads, and
//!   the account check here) as `Cookie:` + `Authorization: SAPISIDHASH ...` + `X-Origin` +
//!   `X-Goog-AuthUser: 0`, and with Horizon's www.youtube.com page loads (watch pages, search) as
//!   `Cookie:` (netapi's `fetch_h`).
//! * Cookie values are never written to the console or a log.
//!
//! Builtins (Horizon's YouTube page): `os_yt_auth_login(path)` -> message (starts with "ok:" or
//! "err:"), `os_yt_auth_logout()`, `os_yt_auth_status()` -> "" when signed out, else the account's
//! name (or "signed in"). Shell: `youtube login <cookies.txt> | logout | status`.

use super::youtube as yt;
use super::ytcookies as yc;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub const ORIGIN: &str = "https://www.youtube.com";

struct State {
    user: String,
    name: String,
    cookies: Vec<yc::Cookie>,
    /// Rotated values taken from answers, not yet written (written from the app's own thread).
    dirty: bool,
}

static mut ST: Option<State> = None;

fn log(m: &str) {
    crate::console_write(format!("[youtube] {m}\n").as_bytes());
}

fn path_for(u: &str) -> String {
    format!("home/{u}/Programs/Horizon/youtube-cookies")
}

fn now() -> u64 {
    crate::clock::unix_now()
}

/// The signed-in LingOS user's state (loaded from the card the first time); None while locked.
fn cur() -> Option<&'static mut State> {
    let u = crate::keyring::user()?;
    let st = unsafe { &mut *(&raw mut ST) };
    if st.as_ref().is_none_or(|s| s.user != u) {
        let mut s = State { user: u.clone(), name: String::new(), cookies: Vec::new(), dirty: false };
        if let Some(bytes) = crate::vfs::read(&path_for(&u)) {
            let p = yc::parse_netscape(&String::from_utf8_lossy(&bytes), now());
            s.name = p.name;
            s.cookies = p.cookies;
        }
        *st = Some(s);
    }
    st.as_mut()
}

fn save(s: &State) -> bool {
    let _ = crate::vfs::mkdir(&format!("home/{}/Programs", s.user));
    let _ = crate::vfs::mkdir(&format!("home/{}/Programs/Horizon", s.user));
    crate::vfs::write(&path_for(&s.user), yc::to_text(&s.name, &s.cookies).as_bytes())
}

/// Write rotated cookie values (only from the app / shell thread, never a green thread mid-fetch).
pub fn flush() {
    if crate::coro::in_coro() {
        return;
    }
    if let Some(s) = cur() {
        if s.dirty {
            s.dirty = false;
            let _ = save(s);
        }
    }
}

pub fn signed_in() -> bool {
    cur().is_some_and(|s| !s.cookies.is_empty())
}

fn is_youtube_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h == "youtube.com" || h.ends_with(".youtube.com")
}

/// The `Cookie:` value for a request to a youtube.com host (None when signed out / another host).
pub fn cookie_header(host: &str, path: &str, https: bool) -> Option<String> {
    if !is_youtube_host(host) {
        return None;
    }
    let s = cur()?;
    yc::cookie_header(&s.cookies, host, path, https, now())
}

/// Horizon's own first-party jar header for a youtube.com page merged with the sign-in cookies
/// (the sign-in's win on a name clash).
pub fn merge_cookie_header(jar: Option<String>, host: &str, path: &str, https: bool) -> Option<String> {
    let Some(mine) = cookie_header(host, path, https) else { return jar };
    let Some(jar) = jar else { return Some(mine) };
    let names: Vec<&str> = mine.split("; ").filter_map(|p| p.split_once('=').map(|(n, _)| n)).collect();
    let mut out = mine.clone();
    for p in jar.split("; ") {
        let n = p.split_once('=').map(|(n, _)| n).unwrap_or(p);
        if !names.contains(&n) {
            out.push_str("; ");
            out.push_str(p);
        }
    }
    Some(out)
}

/// The extra request header lines ("\r\n"-terminated) for an InnerTube call to www.youtube.com
/// when signed in: Cookie, Authorization (SAPISIDHASH), X-Origin, X-Goog-AuthUser,
/// X-Youtube-Bootstrap-Logged-In. "" when signed out.
pub fn api_headers() -> String {
    let Some(s) = cur() else { return String::new() };
    let t = now();
    let mut h = String::new();
    if let Some(c) = yc::cookie_header(&s.cookies, yt::API_HOST, "/youtubei/v1/player", true, t) {
        h.push_str(&format!("Cookie: {c}\r\n"));
    } else {
        return h;
    }
    if let Some(a) = yc::auth_header(&s.cookies, t, ORIGIN) {
        h.push_str(&format!("Authorization: {a}\r\nX-Origin: {ORIGIN}\r\n"));
    }
    h.push_str("X-Goog-AuthUser: 0\r\nX-Youtube-Bootstrap-Logged-In: true\r\n");
    h
}

/// The answer's `Set-Cookie` lines refresh the values of the cookies already kept (written later by `flush`).
pub fn absorb(host: &str, head: &str) {
    if !is_youtube_host(host) || !head.to_ascii_lowercase().contains("set-cookie") {
        return;
    }
    if let Some(s) = cur() {
        if yc::absorb(&mut s.cookies, host, head, now()) {
            s.dirty = true;
        }
    }
}

/// The signed-in account's name from `/account/account_menu` (WEB client, signed request).
fn account_name(s: &mut State) -> Result<String, String> {
    let t = now();
    let Some(cookie) = yc::cookie_header(&s.cookies, yt::API_HOST, "/youtubei/v1/account/account_menu", true, t) else {
        return Err("no youtube.com cookies to send".to_string());
    };
    let mut hdrs = format!(
        "Content-Type: application/json\r\nUser-Agent: {}\r\nX-YouTube-Client-Name: 1\r\nX-YouTube-Client-Version: {}\r\nOrigin: {ORIGIN}\r\nCookie: {cookie}\r\nX-Goog-AuthUser: 0\r\n",
        yt::WEB_UA,
        yt::WEB_VERSION
    );
    if let Some(a) = yc::auth_header(&s.cookies, t, ORIGIN) {
        hdrs.push_str(&format!("Authorization: {a}\r\nX-Origin: {ORIGIN}\r\n"));
    }
    let body = format!(r#"{{"context":{{"client":{{"clientName":"WEB","clientVersion":"{}","hl":"en","gl":"US"}}}}}}"#, yt::WEB_VERSION);
    let url = format!("https://{}/youtubei/v1/account/account_menu?prettyPrint=false", yt::API_HOST);
    let (status, head, b) = crate::lingsock::request_full("POST", &url, &hdrs, body.as_bytes(), 2 << 20)?;
    let _ = yc::absorb(&mut s.cookies, yt::API_HOST, &head, now());
    if status != 200 {
        return Err(format!("YouTube answered HTTP {status}"));
    }
    yt::account_name(&b).ok_or_else(|| "YouTube did not accept these cookies as a signed-in account (signed out in that browser, or expired: export them again)".to_string())
}

/// Import a cookies.txt: keep the youtube.com / google.com cookies, check them with YouTube, store
/// them sealed for this LingOS user.
pub fn login(path: &str) -> Result<String, String> {
    let Some(s) = cur() else {
        return Err("sign in to LingOS first: the cookies are sealed with your account's key".to_string());
    };
    let p = crate::vfs::norm(path.trim().trim_start_matches('/'));
    let Some(bytes) = crate::vfs::read(&p) else {
        return Err(format!("could not read {p}"));
    };
    if bytes.len() > 4 << 20 {
        return Err(format!("{p} is too big for a cookies.txt"));
    }
    let parsed = yc::parse_netscape(&String::from_utf8_lossy(&bytes), now());
    if parsed.cookies.is_empty() {
        return Err(format!(
            "no youtube.com / google.com cookies in {p} ({} other sites' cookies, {} expired, {} unreadable lines) -- export cookies.txt while signed in to youtube.com",
            parsed.other, parsed.expired, parsed.bad
        ));
    }
    if !yc::looks_signed_in(&parsed.cookies, now()) {
        return Err(format!(
            "{p} has {} youtube.com / google.com cookies but no signed-in session (SAPISID + SID) -- sign in to youtube.com in that browser, then export again",
            parsed.cookies.len()
        ));
    }
    let n = parsed.cookies.len();
    let old = core::mem::replace(&mut s.cookies, parsed.cookies);
    let old_name = core::mem::take(&mut s.name);
    let checked = account_name(s);
    match &checked {
        Ok(name) => s.name = name.clone(),
        Err(e) if e.starts_with("YouTube did not accept") => {
            s.cookies = old;
            s.name = old_name;
            return Err(e.clone());
        },
        Err(_) => {},
    }
    s.dirty = false;
    if !save(s) {
        s.cookies.clear();
        s.name.clear();
        return Err("could not store the cookies (is your keyring unlocked?)".to_string());
    }
    log(&format!("signed in for {} ({n} cookies kept, {} other sites' left out)", s.user, parsed.other));
    Ok(match checked {
        Ok(name) => format!("signed in to YouTube as {name} ({n} cookies kept, sealed for {})", s.user),
        Err(e) => format!("signed in to YouTube ({n} cookies kept, sealed for {}); the account could not be checked now: {e}", s.user),
    })
}

/// Forget the sign-in (the stored file too).
pub fn logout() -> bool {
    let Some(s) = cur() else { return false };
    s.cookies.clear();
    s.name.clear();
    s.dirty = false;
    let p = path_for(&s.user);
    if crate::vfs::exists(&p) {
        crate::vfs::delete(&p)
    } else {
        true
    }
}

/// "" when signed out, else the account's name or "signed in".
pub fn status_name() -> String {
    flush();
    match cur() {
        Some(s) if !s.cookies.is_empty() => {
            if s.name.is_empty() {
                "signed in".to_string()
            } else {
                s.name.clone()
            }
        },
        _ => String::new(),
    }
}

/// The `os_yt_auth_*` builtins.
pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    use crate::lingrt::{boolean, mk_str, to_string};
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    Some(match name {
        b"os_yt_auth_login" => mk_str(&match login(&s(0)) {
            Ok(m) => format!("ok:{m}"),
            Err(e) => format!("err:{e}"),
        }),
        b"os_yt_auth_logout" => boolean(logout()),
        b"os_yt_auth_status" => mk_str(&status_name()),
        _ => return None,
    })
}

/// `youtube login <cookies.txt> | logout | status` (SSH / Terminal); `path` already resolved.
pub fn command(sub: &str, path: &str) -> String {
    match sub {
        "login" if !path.is_empty() => match login(path) {
            Ok(m) => format!("youtube: {m}\n"),
            Err(e) => format!("youtube: {e}\n"),
        },
        "logout" => {
            if logout() {
                "youtube: signed out (the stored cookies are deleted)\n".to_string()
            } else {
                "youtube: nothing to sign out of (no LingOS user signed in)\n".to_string()
            }
        },
        "status" | "" => match cur() {
            None => "youtube: no LingOS user signed in (the keyring is locked)\n".to_string(),
            Some(s) if s.cookies.is_empty() => format!("youtube: signed out for {} (youtube login <cookies.txt>)\n", s.user),
            Some(_) => {
                let n = status_name();
                let s = cur().unwrap();
                let soonest = s.cookies.iter().filter(|c| c.expires != 0 && ["SID", "SAPISID", "__Secure-3PSID"].contains(&c.name.as_str())).map(|c| c.expires).min().unwrap_or(0);
                let left = if soonest > now() && crate::clock::is_set() { format!(", session cookies good for {} more days", (soonest - now()) / 86_400) } else { String::new() };
                format!("youtube: {n} (LingOS user {}, {} cookies{left})\n", s.user, s.cookies.len())
            },
        },
        _ => "usage: youtube login <cookies.txt> | youtube logout | youtube status\n".to_string(),
    }
}
