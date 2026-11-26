//! A browser's cookie jar, for Horizon (and any `.ling` program that asks for one).
//!
//! Off until `cookie_jar_on(true)`: with it off every HTTP builtin behaves as before (no cookies
//! kept, none sent). With it on, `http_get`, `http_get_all`, `http_get_async`, `http_post_async`,
//! `http_fetch`, image fetches, and `http_request_async` calls that carry the header line
//! `X-Horizon-Jar: 1` (stripped before sending) all share this one jar.
//!
//! Privacy rules, always on:
//! - **first party only**: `cookie_jar_site(url)` names the page in the address bar; a response
//!   from any other site can neither set a cookie nor be sent one (no third-party cookies).
//! - **no known trackers**: analytics / ad cookies by name (`_ga`, `_fbp`, `__utm*`, `IDE` ...)
//!   are never stored.
//! - values over 4 KiB and more than 3000 cookies are refused.
//!
//! The jar lives in memory. `cookie_jar_export()` gives it as text (one cookie a line, tab
//! separated) for the program to keep where it wants (Horizon seals it in its keychain);
//! `cookie_jar_import(text)` puts it back.
//!
//! Builtins: `cookie_jar_on(b)`, `cookie_jar_site(url)`, `cookie_jar_export()`,
//! `cookie_jar_import(text)` -> count, `cookie_jar_clear()`, `cookie_jar_clear_site(host)`,
//! `cookie_jar_count()`, `cookie_jar_changed()` (a counter that moves whenever a cookie does),
//! `cookie_jar_get(url)` (the "a=b; c=d" a script may read: no HttpOnly ones),
//! `cookie_jar_set(url, line)` (a script's `document.cookie = line`; HttpOnly refused),
//! `cookie_jar_list()` ("site\tname\texpires" lines, for a settings page);
//! `http_fetch(method, url, headers, body, follow)` -> body (blocking; `follow` false leaves
//! redirects to the caller), `http_last_url()`, `http_last_header(name)`.

use super::Value;
use reqwest::header::HeaderValue;
use std::sync::{Arc, Mutex, OnceLock};

pub const NAMES: &[&str] = &[
    "cookie_jar_on",
    "cookie_jar_site",
    "cookie_jar_export",
    "cookie_jar_import",
    "cookie_jar_clear",
    "cookie_jar_clear_site",
    "cookie_jar_count",
    "cookie_jar_changed",
    "cookie_jar_get",
    "cookie_jar_set",
    "cookie_jar_list",
    "http_fetch",
    "http_last_url",
    "http_last_header",
];

pub fn handles(name: &str) -> bool {
    NAMES.contains(&name)
}

const MAX_COOKIES: usize = 3000;
const MAX_VALUE: usize = 4096;
pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Horizon/1.0";

#[derive(Clone, Debug, PartialEq)]
struct Cookie {
    name: String,
    value: String,
    domain: String, // lower case, no leading dot
    host_only: bool,
    path: String,
    expires: i64, // unix seconds; 0 = a session cookie
    secure: bool,
    http_only: bool,
    same_site: String,
}

#[derive(Default)]
struct State {
    on: bool,
    site: String, // registrable domain of the page in the address bar ("" = no rule)
    cookies: Vec<Cookie>,
    changed: u64,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Default::default)
}

pub fn is_on() -> bool {
    state().lock().map(|s| s.on).unwrap_or(false)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── sites ──

/// Suffixes under which anyone may register a name (so `x.co.uk`, not `co.uk`, is a site).
const MULTI_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "ac.uk", "gov.uk", "me.uk", "ltd.uk", "plc.uk", "net.uk", "co.jp", "ne.jp", "or.jp",
    "ac.jp", "go.jp", "com.au", "net.au", "org.au", "edu.au", "gov.au", "co.nz", "org.nz", "com.br",
    "com.cn", "net.cn", "org.cn", "com.hk", "com.tw", "co.kr", "or.kr", "co.in", "co.za", "com.mx",
    "com.ar", "com.tr", "com.sg", "com.my", "co.id", "co.th", "github.io", "gitlab.io", "pages.dev",
    "vercel.app", "netlify.app", "herokuapp.com", "appspot.com", "blogspot.com", "workers.dev",
];

/// The registrable domain of a host ("mail.google.com" -> "google.com").
pub fn site_of_host(host: &str) -> String {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h.parse::<std::net::IpAddr>().is_ok() || !h.contains('.') {
        return h;
    }
    let labels: Vec<&str> = h.split('.').collect();
    let n = labels.len();
    let last2 = labels[n - 2..].join(".");
    if n >= 3 && MULTI_SUFFIXES.contains(&last2.as_str()) {
        return labels[n - 3..].join(".");
    }
    last2
}

fn is_public_suffix(d: &str) -> bool {
    !d.contains('.') || MULTI_SUFFIXES.contains(&d)
}

fn domain_match(host: &str, domain: &str) -> bool {
    host == domain || (host.ends_with(domain) && host.as_bytes()[host.len() - domain.len() - 1] == b'.')
}

fn path_match(req: &str, cpath: &str) -> bool {
    if req == cpath {
        return true;
    }
    req.starts_with(cpath) && (cpath.ends_with('/') || req.as_bytes().get(cpath.len()) == Some(&b'/'))
}

fn default_path(url: &reqwest::Url) -> String {
    let p = url.path();
    match p.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => p[..i].to_string(),
    }
}

/// Analytics and advertising cookies, by name: never kept.
fn is_tracker(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "_gid", "_fbp", "_fbc", "IDE", "DSID", "test_cookie", "__qca", "_uetsid", "_uetvid", "_clck", "_clsk",
        "MUID", "_scid", "_tt_enable_cookie", "_ttp", "__gads", "__gpi", "_pin_unauth", "_rdt_uuid", "ajs_anonymous_id",
        "mp_mixpanel", "_mkto_trk", "hubspotutk", "__hstc", "__hssc", "__hssrc", "_hjid", "_lfa", "_cioid",
    ];
    const PREFIX: &[&str] = &["_ga", "__utm", "_gcl_", "_gat", "_hj", "amplitude_id", "mp_", "_dc_gtm", "__adroll", "_parsely"];
    EXACT.contains(&name) || PREFIX.iter().any(|p| name.starts_with(p))
}

// ── Set-Cookie ──

fn month(m: &str) -> Option<u32> {
    let m = m.to_ascii_lowercase();
    ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"]
        .iter()
        .position(|x| m.starts_with(x))
        .map(|i| i as u32 + 1)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// An HTTP / cookie date ("Wed, 12-Nov-2027 10:00:00 GMT" and its relatives) -> unix seconds.
fn parse_date(s: &str) -> Option<i64> {
    let (mut day, mut mon, mut year, mut hms) = (None, None, None, None);
    for tok in s.split(|c: char| c == ' ' || c == ',' || c == '-').filter(|t| !t.is_empty()) {
        if hms.is_none() && tok.contains(':') {
            let p: Vec<i64> = tok.split(':').filter_map(|x| x.parse().ok()).collect();
            if p.len() == 3 {
                hms = Some(p[0] * 3600 + p[1] * 60 + p[2]);
            }
        } else if let Ok(n) = tok.parse::<i64>() {
            if day.is_none() && tok.len() <= 2 {
                day = Some(n as u32);
            } else if year.is_none() {
                year = Some(if n < 70 { n + 2000 } else if n < 100 { n + 1900 } else { n });
            }
        } else if mon.is_none() {
            mon = month(tok);
        }
    }
    Some(days_from_civil(year?, mon?, day?) * 86400 + hms.unwrap_or(0))
}

fn parse_set_cookie(line: &str, url: &reqwest::Url, from_script: bool) -> Option<Cookie> {
    let host = url.host_str()?.to_ascii_lowercase();
    let mut parts = line.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let (name, value) = (name.trim().to_string(), value.trim().trim_matches('"').to_string());
    if name.is_empty() || value.len() > MAX_VALUE || is_tracker(&name) {
        return None;
    }
    let mut c = Cookie {
        name,
        value,
        domain: host.clone(),
        host_only: true,
        path: default_path(url),
        expires: 0,
        secure: false,
        http_only: false,
        same_site: String::new(),
    };
    let mut max_age: Option<i64> = None;
    for attr in parts {
        let (k, v) = match attr.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
            None => (attr.trim().to_ascii_lowercase(), ""),
        };
        match k.as_str() {
            "domain" => {
                let d = v.trim_start_matches('.').to_ascii_lowercase();
                if d.is_empty() {
                    continue;
                }
                if !domain_match(&host, &d) || is_public_suffix(&d) {
                    return None;
                }
                c.domain = d;
                c.host_only = false;
            },
            "path" if v.starts_with('/') => c.path = v.to_string(),
            "expires" => {
                if let Some(t) = parse_date(v) {
                    c.expires = t.max(1);
                }
            },
            "max-age" => max_age = v.parse().ok(),
            "secure" => c.secure = true,
            "httponly" => c.http_only = true,
            "samesite" => c.same_site = v.to_ascii_lowercase(),
            _ => {},
        }
    }
    if let Some(a) = max_age {
        c.expires = if a <= 0 { 1 } else { now() + a };
    }
    if from_script && c.http_only {
        return None;
    }
    if c.name.starts_with("__Secure-") && !c.secure {
        return None;
    }
    if c.name.starts_with("__Host-") && (!c.secure || !c.host_only || c.path != "/") {
        return None;
    }
    if c.secure && url.scheme() != "https" && host != "localhost" && host != "127.0.0.1" {
        return None;
    }
    Some(c)
}

fn first_party(st: &State, host: &str) -> bool {
    st.site.is_empty() || site_of_host(host) == st.site
}

fn store(st: &mut State, c: Cookie) {
    let t = now();
    st.cookies.retain(|o| o.expires == 0 || o.expires > t);
    let pos = st.cookies.iter().position(|o| o.name == c.name && o.domain == c.domain && o.path == c.path);
    let dead = c.expires != 0 && c.expires <= t;
    match (pos, dead) {
        (Some(i), true) => {
            st.cookies.remove(i);
        },
        (Some(i), false) => {
            if st.cookies[i] == c {
                return;
            }
            st.cookies[i] = c;
        },
        (None, true) => return,
        (None, false) => {
            if st.cookies.len() >= MAX_COOKIES {
                return;
            }
            st.cookies.push(c);
        },
    }
    st.changed += 1;
}

/// The cookies for `url` as "a=b; c=d"; `script` leaves the HttpOnly ones out.
fn header_for(st: &State, url: &reqwest::Url, script: bool) -> String {
    let host = match url.host_str() {
        Some(h) => h.to_ascii_lowercase(),
        None => return String::new(),
    };
    if !first_party(st, &host) {
        return String::new();
    }
    let secure = url.scheme() == "https" || host == "localhost" || host == "127.0.0.1";
    let t = now();
    let mut v: Vec<&Cookie> = st
        .cookies
        .iter()
        .filter(|c| {
            (c.expires == 0 || c.expires > t)
                && (if c.host_only { host == c.domain } else { domain_match(&host, &c.domain) })
                && path_match(url.path(), &c.path)
                && (secure || !c.secure)
                && !(script && c.http_only)
        })
        .collect();
    v.sort_by(|a, b| b.path.len().cmp(&a.path.len()));
    v.iter().map(|c| format!("{}={}", c.name, c.value)).collect::<Vec<_>>().join("; ")
}

/// reqwest's hook: every request made through a jar client asks here.
pub struct Jar;

impl reqwest::cookie::CookieStore for Jar {
    fn set_cookies(&self, headers: &mut dyn Iterator<Item = &HeaderValue>, url: &reqwest::Url) {
        let mut st = state().lock().unwrap();
        if !st.on {
            return;
        }
        let host = url.host_str().unwrap_or("").to_ascii_lowercase();
        if !first_party(&st, &host) {
            return;
        }
        for h in headers {
            if let Some(c) = h.to_str().ok().and_then(|l| parse_set_cookie(l, url, false)) {
                store(&mut st, c);
            }
        }
    }

    fn cookies(&self, url: &reqwest::Url) -> Option<HeaderValue> {
        let st = state().lock().unwrap();
        if !st.on {
            return None;
        }
        let h = header_for(&st, url, false);
        if h.is_empty() { None } else { HeaderValue::from_str(&h).ok() }
    }
}

/// A client that keeps cookies in the jar (and follows redirects itself, up to 10).
pub fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| build(true))
}

/// The same, but leaving redirects to the caller (Horizon walks them to track the site per hop).
fn client_manual() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| build(false))
}

fn build(follow: bool) -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(UA)
        .cookie_provider(Arc::new(Jar))
        .redirect(if follow { reqwest::redirect::Policy::limited(10) } else { reqwest::redirect::Policy::none() })
        .timeout(std::time::Duration::from_secs(40))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

// ── export / import ──

fn esc(s: &str) -> String {
    s.replace('%', "%25").replace('\t', "%09").replace('\n', "%0A").replace('\r', "%0D")
}
fn unesc(s: &str) -> String {
    s.replace("%0D", "\r").replace("%0A", "\n").replace("%09", "\t").replace("%25", "%")
}

fn export(st: &State) -> String {
    let t = now();
    let mut out = String::new();
    for c in st.cookies.iter().filter(|c| c.expires == 0 || c.expires > t) {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            esc(&c.domain),
            c.host_only as u8,
            esc(&c.path),
            c.secure as u8,
            c.http_only as u8,
            c.expires,
            esc(&c.same_site),
            esc(&c.name),
            esc(&c.value)
        ));
    }
    out
}

fn import(st: &mut State, text: &str) -> usize {
    let mut n = 0;
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 9 {
            continue;
        }
        let c = Cookie {
            domain: unesc(f[0]),
            host_only: f[1] == "1",
            path: unesc(f[2]),
            secure: f[3] == "1",
            http_only: f[4] == "1",
            expires: f[5].parse().unwrap_or(0),
            same_site: unesc(f[6]),
            name: unesc(f[7]),
            value: unesc(f[8]),
        };
        if c.name.is_empty() || c.domain.is_empty() || is_tracker(&c.name) {
            continue;
        }
        store(st, c);
        n += 1;
    }
    n
}

// ── the last http_fetch / http_get ──

#[derive(Default)]
struct Last {
    url: String,
    headers: Vec<(String, String)>,
}
fn last() -> &'static Mutex<Last> {
    static L: OnceLock<Mutex<Last>> = OnceLock::new();
    L.get_or_init(Default::default)
}

pub fn note_response(url: &str, headers: &reqwest::header::HeaderMap) {
    let mut l = last().lock().unwrap();
    l.url = url.to_string();
    l.headers = headers
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_ascii_lowercase(), v.to_str().ok()?.to_string())))
        .collect();
}

fn parse_headers(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            let k = k.trim();
            if k.is_empty() { None } else { Some((k.to_string(), v.trim().to_string())) }
        })
        .collect()
}

/// A blocking request through the jar: (status, body). Status 0 = no response at all.
fn fetch(method: &str, url: &str, headers: &str, body: String, follow: bool) -> (u16, String) {
    let m = reqwest::Method::from_bytes(method.to_uppercase().as_bytes()).unwrap_or(reqwest::Method::GET);
    let url = url.to_string();
    let hs = parse_headers(headers);
    super::web::async_runtime_handle().block_on(async move {
        let c = if follow { client() } else { client_manual() };
        let mut rq = c.request(m, &url);
        for (k, v) in hs {
            rq = rq.header(k, v);
        }
        if !body.is_empty() {
            rq = rq.body(body);
        }
        match rq.send().await {
            Ok(resp) => {
                let st = resp.status().as_u16();
                note_response(resp.url().as_str(), resp.headers());
                (st, resp.text().await.unwrap_or_default())
            },
            Err(_) => {
                note_response(&url, &Default::default());
                (0, String::new())
            },
        }
    })
}

fn s(args: &[Value], i: usize) -> String {
    args.get(i).map(|v| v.to_string()).unwrap_or_default()
}
fn truthy(args: &[Value], i: usize) -> bool {
    match args.get(i) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => *n != 0.0,
        Some(v) => {
            let t = v.to_string();
            !(t.is_empty() || t == "false" || t == "0")
        },
        None => false,
    }
}

/// Runs a cookie_jar_* / http_fetch builtin; `status` receives http_fetch's response code.
pub fn call(name: &str, args: &[Value], status: &mut u16) -> Value {
    match name {
        "http_fetch" => {
            let (st, body) = fetch(&s(args, 0), &s(args, 1), &s(args, 2), s(args, 3), truthy(args, 4));
            *status = st;
            return Value::Str(body);
        },
        "http_last_url" => return Value::Str(last().lock().unwrap().url.clone()),
        "http_last_header" => {
            let k = s(args, 0).to_ascii_lowercase();
            let l = last().lock().unwrap();
            let v: Vec<&str> = l.headers.iter().filter(|(n, _)| *n == k).map(|(_, v)| v.as_str()).collect();
            return Value::Str(v.join("\n"));
        },
        _ => {},
    }
    let mut st = state().lock().unwrap();
    match name {
        "cookie_jar_on" => {
            if !args.is_empty() {
                st.on = truthy(args, 0);
            }
            Value::Bool(st.on)
        },
        "cookie_jar_site" => {
            let u = s(args, 0);
            st.site = reqwest::Url::parse(&u)
                .ok()
                .and_then(|u| u.host_str().map(site_of_host))
                .unwrap_or_default();
            Value::Str(st.site.clone())
        },
        "cookie_jar_export" => Value::Str(export(&st)),
        "cookie_jar_import" => Value::Number(import(&mut st, &s(args, 0)) as f64),
        "cookie_jar_clear" => {
            st.cookies.clear();
            st.changed += 1;
            Value::Bool(true)
        },
        "cookie_jar_clear_site" => {
            let site = site_of_host(&s(args, 0));
            let before = st.cookies.len();
            st.cookies.retain(|c| site_of_host(&c.domain) != site);
            if st.cookies.len() != before {
                st.changed += 1;
            }
            Value::Number((before - st.cookies.len()) as f64)
        },
        "cookie_jar_count" => {
            let t = now();
            Value::Number(st.cookies.iter().filter(|c| c.expires == 0 || c.expires > t).count() as f64)
        },
        "cookie_jar_changed" => Value::Number(st.changed as f64),
        "cookie_jar_get" => match reqwest::Url::parse(&s(args, 0)) {
            Ok(u) if st.on => Value::Str(header_for(&st, &u, true)),
            _ => Value::Str(String::new()),
        },
        "cookie_jar_set" => {
            let line = s(args, 1);
            match reqwest::Url::parse(&s(args, 0)) {
                Ok(u) if st.on && first_party(&st, u.host_str().unwrap_or("")) => {
                    match parse_set_cookie(&line, &u, true) {
                        Some(c) => {
                            store(&mut st, c);
                            Value::Bool(true)
                        },
                        None => Value::Bool(false),
                    }
                },
                _ => Value::Bool(false),
            }
        },
        "cookie_jar_list" => {
            let t = now();
            let mut out = String::new();
            for c in st.cookies.iter().filter(|c| c.expires == 0 || c.expires > t) {
                out.push_str(&format!("{}\t{}\t{}\n", site_of_host(&c.domain), c.name, c.expires));
            }
            Value::Str(out)
        },
        _ => Value::Str(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites() {
        assert_eq!(site_of_host("mail.google.com"), "google.com");
        assert_eq!(site_of_host("www.bbc.co.uk"), "bbc.co.uk");
        assert_eq!(site_of_host("localhost"), "localhost");
    }

    #[test]
    fn dates() {
        assert_eq!(parse_date("Thu, 01 Jan 1970 00:00:10 GMT"), Some(10));
        assert_eq!(parse_date("Wed, 12-Nov-2025 10:00:00 GMT"), parse_date("Wed, 12 Nov 2025 10:00:00 GMT"));
    }

    #[test]
    fn set_and_send() {
        let u = reqwest::Url::parse("https://accounts.google.com/signin").unwrap();
        let mut st = State { on: true, site: "google.com".into(), ..Default::default() };
        let c = parse_set_cookie("SID=abc; Domain=.google.com; Path=/; Secure; HttpOnly", &u, false).unwrap();
        store(&mut st, c);
        store(&mut st, parse_set_cookie("_ga=x; Domain=.google.com", &u, false).unwrap_or_else(|| Cookie {
            name: "skip".into(),
            value: "".into(),
            domain: "nowhere".into(),
            host_only: true,
            path: "/".into(),
            expires: 1,
            secure: false,
            http_only: false,
            same_site: "".into(),
        }));
        let m = reqwest::Url::parse("https://mail.google.com/mail/u/0/").unwrap();
        assert_eq!(header_for(&st, &m, false), "SID=abc");
        assert_eq!(header_for(&st, &m, true), "");
        assert!(parse_set_cookie("a=b; Domain=com", &u, false).is_none());
        assert!(parse_set_cookie("a=b; Domain=example.com", &u, false).is_none());
        let text = export(&st);
        let mut st2 = State { on: true, ..Default::default() };
        assert_eq!(import(&mut st2, &text), 1);
        assert_eq!(header_for(&st2, &m, false), "SID=abc");
        st2.site = "example.com".into();
        assert_eq!(header_for(&st2, &m, false), "");
    }
}
