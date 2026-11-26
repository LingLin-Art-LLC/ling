//! YouTube sign-in with the user's own browser cookies: the pure part (no kernel calls), so a host
//! test builds it unchanged.
//!
//! Google's sign-in pages refuse browsers they do not know ("This browser or app may not be
//! secure") and YouTube's TV device-code sign-in no longer works for InnerTube, so LingOS does what
//! yt-dlp recommends: the user exports `cookies.txt` (Netscape format) from a browser that is
//! signed in to youtube.com, and LingOS sends those cookies with its YouTube requests, plus the
//! `Authorization: SAPISIDHASH ...` header YouTube's own web player computes from the `SAPISID`
//! cookie.
//!
//! * [`parse_netscape`] reads a cookies.txt and keeps only youtube.com / google.com cookies.
//! * [`cookie_header`] is the `Cookie:` value for one request (domain / path / secure / expiry).
//! * [`auth_header`] is the `Authorization:` value: `SAPISIDHASH <ts>_<sha1("<ts> <SAPISID> <origin>")>`,
//!   plus `SAPISID1PHASH` / `SAPISID3PHASH` from `__Secure-1PAPISID` / `__Secure-3PAPISID`
//!   (yt-dlp's `_get_sid_authorization_header`).
//!
//! Cookie values are secrets: nothing here logs them, and [`Cookie`]'s `Debug` hides the value.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use sha1::{Digest, Sha1};

#[derive(Clone, PartialEq)]
pub struct Cookie {
    /// Lowercase, without a leading dot.
    pub domain: String,
    /// Sent to subdomains too (cookies.txt's second field, or a leading dot).
    pub sub: bool,
    pub path: String,
    pub secure: bool,
    /// Unix seconds; 0 = a session cookie.
    pub expires: u64,
    pub name: String,
    pub value: String,
}

impl core::fmt::Debug for Cookie {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Cookie({} on {}{} path {} secure {} expires {}, {} value bytes)", self.name, if self.sub { "." } else { "" }, self.domain, self.path, self.secure, self.expires, self.value.len())
    }
}

/// What a cookies.txt held.
#[derive(Default, Debug)]
pub struct Parsed {
    /// The youtube.com / google.com cookies kept.
    pub cookies: Vec<Cookie>,
    /// Cookie lines of other sites, left out.
    pub other: usize,
    /// Kept-site cookies already expired, left out.
    pub expired: usize,
    /// Lines that are not cookies.txt cookie lines.
    pub bad: usize,
    /// The `# LingOS-YouTube-Name:` line of LingOS's own stored copy.
    pub name: String,
}

const NAME_TAG: &str = "# LingOS-YouTube-Name:";

/// youtube.com, google.com and their subdomains.
pub fn wanted_domain(d: &str) -> bool {
    let d = d.trim_start_matches('.').to_ascii_lowercase();
    ["youtube.com", "google.com"].iter().any(|s| d == *s || d.ends_with(&format!(".{s}")))
}

fn clean(s: &str) -> bool {
    !s.bytes().any(|b| b < 0x20 || b == 0x7f || b == b';')
}

fn flag(s: &str) -> bool {
    s.eq_ignore_ascii_case("true")
}

/// Read a Netscape `cookies.txt` (what browser extensions and `yt-dlp --cookies` use): one cookie
/// per line, `domain  include-subdomains  path  secure  expires  name  value`, tab-separated;
/// `#HttpOnly_` before the domain marks an HttpOnly cookie, other `#` lines are comments.
/// `now` (unix seconds, 0 = unknown) drops expired cookies.
pub fn parse_netscape(text: &str, now: u64) -> Parsed {
    let mut out = Parsed::default();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for raw in text.lines() {
        let line = raw.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            continue;
        }
        if let Some(n) = line.strip_prefix(NAME_TAG) {
            out.name = n.trim().to_string();
            continue;
        }
        let line = match line.strip_prefix("#HttpOnly_") {
            Some(rest) => rest,
            None if line.starts_with('#') => continue,
            None => line,
        };
        let mut f: Vec<&str> = line.split('\t').collect();
        if f.len() < 6 {
            // some tools write spaces instead of tabs (a value never holds a space)
            f = line.split_whitespace().collect();
        }
        // a cookie with an empty value can lose its last (empty) field
        if f.len() == 6 {
            f.push("");
        }
        if f.len() != 7 {
            out.bad += 1;
            continue;
        }
        let domain = f[0].trim();
        if !wanted_domain(domain) {
            out.other += 1;
            continue;
        }
        let Ok(expires) = f[4].trim().parse::<f64>().map(|e| if e <= 0.0 { 0 } else { e as u64 }) else {
            out.bad += 1;
            continue;
        };
        let (name, value) = (f[5].trim(), f[6].trim());
        if name.is_empty() || !clean(name) || !clean(value) || name.contains('=') {
            out.bad += 1;
            continue;
        }
        if expires != 0 && now != 0 && expires <= now {
            out.expired += 1;
            continue;
        }
        let path = if f[2].starts_with('/') { f[2].trim() } else { "/" };
        let c = Cookie {
            domain: domain.trim_start_matches('.').to_ascii_lowercase(),
            sub: flag(f[1].trim()) || domain.starts_with('.'),
            path: path.to_string(),
            secure: flag(f[3].trim()),
            expires,
            name: name.to_string(),
            value: value.to_string(),
        };
        // a later line of the same cookie replaces an earlier one
        out.cookies.retain(|o| !(o.name == c.name && o.domain == c.domain && o.path == c.path));
        out.cookies.push(c);
    }
    out
}

/// The stored copy: a cookies.txt again (so [`parse_netscape`] reads it back), with the account's
/// name on a comment line.
pub fn to_text(name: &str, cookies: &[Cookie]) -> String {
    let mut s = String::from("# Netscape HTTP Cookie File\n# LingOS YouTube sign-in (youtube.com / google.com only)\n");
    let name: String = name.chars().filter(|c| !c.is_control()).collect();
    if !name.is_empty() {
        s.push_str(&format!("{NAME_TAG} {name}\n"));
    }
    for c in cookies {
        let dom = if c.sub { format!(".{}", c.domain) } else { c.domain.clone() };
        let tf = |b: bool| if b { "TRUE" } else { "FALSE" };
        s.push_str(&format!("{dom}\t{}\t{}\t{}\t{}\t{}\t{}\n", tf(c.sub), c.path, tf(c.secure), c.expires, c.name, c.value));
    }
    s
}

/// Does cookie `c` go to `host`?
pub fn host_matches(c: &Cookie, host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h == c.domain {
        return true;
    }
    c.sub && h.len() > c.domain.len() && h.ends_with(&c.domain) && h.as_bytes()[h.len() - c.domain.len() - 1] == b'.'
}

fn path_matches(cookie_path: &str, req_path: &str) -> bool {
    let req = req_path.split(['?', '#']).next().unwrap_or("/");
    let req = if req.is_empty() { "/" } else { req };
    req == cookie_path || (req.starts_with(cookie_path) && (cookie_path.ends_with('/') || req.as_bytes().get(cookie_path.len()) == Some(&b'/')))
}

fn live(c: &Cookie, now: u64) -> bool {
    c.expires == 0 || now == 0 || c.expires > now
}

/// The `Cookie:` header value for a request to `host` / `path`, if any cookie applies.
pub fn cookie_header(cookies: &[Cookie], host: &str, path: &str, https: bool, now: u64) -> Option<String> {
    let mut parts: Vec<(usize, String)> = Vec::new();
    for c in cookies {
        if live(c, now) && (https || !c.secure) && host_matches(c, host) && path_matches(&c.path, path) {
            parts.push((c.path.len(), format!("{}={}", c.name, c.value)));
        }
    }
    if parts.is_empty() {
        return None;
    }
    parts.sort_by(|a, b| b.0.cmp(&a.0));
    Some(parts.into_iter().map(|p| p.1).collect::<Vec<_>>().join("; "))
}

/// The value of cookie `name` that a request to `host` would carry.
pub fn get<'a>(cookies: &'a [Cookie], name: &str, host: &str, now: u64) -> Option<&'a str> {
    cookies.iter().filter(|c| c.name == name && live(c, now) && host_matches(c, host)).map(|c| c.value.as_str()).find(|v| !v.is_empty())
}

fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

/// `<ts>_<sha1("<ts> <sid> <origin>") in hex>` (what follows the scheme name).
pub fn sapisid_hash(ts: u64, sid: &str, origin: &str) -> String {
    let d = Sha1::digest(format!("{ts} {sid} {origin}").as_bytes());
    format!("{ts}_{}", hex(&d))
}

/// The `Authorization:` header value for `origin` ("https://www.youtube.com") at unix time `ts`;
/// None without a SAPISID-type cookie for the origin's host.
pub fn auth_header(cookies: &[Cookie], ts: u64, origin: &str) -> Option<String> {
    let host = origin.split_once("://").map(|(_, h)| h).unwrap_or(origin).split(['/', ':']).next().unwrap_or("");
    // yt-dlp: SAPISID, else __Secure-3PAPISID stands in for it
    let sapisid = get(cookies, "SAPISID", host, ts).or_else(|| get(cookies, "__Secure-3PAPISID", host, ts));
    let mut out: Vec<String> = Vec::new();
    for (scheme, sid) in [("SAPISIDHASH", sapisid), ("SAPISID1PHASH", get(cookies, "__Secure-1PAPISID", host, ts)), ("SAPISID3PHASH", get(cookies, "__Secure-3PAPISID", host, ts))] {
        if let Some(sid) = sid {
            out.push(format!("{scheme} {}", sapisid_hash(ts, sid, origin)));
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join(" "))
    }
}

/// Does this set look like a signed-in YouTube session (a SAPISID-type cookie plus a session id)?
pub fn looks_signed_in(cookies: &[Cookie], now: u64) -> bool {
    let h = "www.youtube.com";
    let sap = get(cookies, "SAPISID", h, now).or_else(|| get(cookies, "__Secure-3PAPISID", h, now)).is_some();
    let sid = ["SID", "__Secure-1PSID", "__Secure-3PSID"].iter().any(|n| get(cookies, n, h, now).is_some());
    sap && sid
}

/// Take the `Set-Cookie` lines of a YouTube answer for cookies already in the set (Google rotates
/// `__Secure-1PSIDTS` / `__Secure-3PSIDTS` and the like now and then): their new values replace the
/// old. Only names already present, only on hosts they apply to. True when something changed.
pub fn absorb(cookies: &mut [Cookie], host: &str, headers: &str, now: u64) -> bool {
    let mut changed = false;
    for line in headers.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        if !k.trim().eq_ignore_ascii_case("set-cookie") {
            continue;
        }
        let mut segs = v.trim().split(';');
        let Some((name, value)) = segs.next().and_then(|nv| nv.split_once('=')) else { continue };
        let (name, value) = (name.trim(), value.trim());
        if !clean(value) || value.is_empty() {
            continue;
        }
        let mut expires: Option<u64> = None;
        let mut domain: Option<String> = None;
        for a in segs {
            let (an, av) = a.trim().split_once('=').map(|(x, y)| (x.trim(), y.trim())).unwrap_or((a.trim(), ""));
            if an.eq_ignore_ascii_case("max-age") {
                if let Ok(m) = av.parse::<i64>() {
                    expires = Some(if m <= 0 { 1 } else if now == 0 { 0 } else { now + m as u64 });
                }
            } else if an.eq_ignore_ascii_case("domain") {
                domain = Some(av.trim_start_matches('.').to_ascii_lowercase());
            }
        }
        for c in cookies.iter_mut() {
            if c.name == name && host_matches(c, host) && domain.as_deref().is_none_or(|d| d == c.domain) {
                if expires == Some(1) {
                    continue; // a deletion: keep ours (the user's browser decides when it ends)
                }
                if c.value != value {
                    c.value = value.to_string();
                    changed = true;
                }
                if let Some(e) = expires {
                    if e > c.expires && c.expires != 0 {
                        c.expires = e;
                        changed = true;
                    }
                }
            }
        }
    }
    changed
}
