//! LingSockets: the network and data builtins Horizon's apps (Discord, Spotify,
//! YouTube search) use, with the same names and answers as the PC runtime's
//! `runtime/netx.rs`, so an app written against one runs on the other:
//!
//! - `http_request_async(method, url, headers, body)` -> job id; `http_req_poll(id)`
//!   -> "" while it runs, then "<status>\n<body>" (status 0 and "ERROR: ..." when
//!   the request could not be made). `headers` is "Name: value" lines. No cookie
//!   jar, no cache: nothing is kept between requests.
//! - `ws_open(url, headers)` -> id; `ws_state(id)` "connecting" | "open" |
//!   "closed: <code> <why>" | "error: <why>"; `ws_send(id, text)`,
//!   `ws_send_b64(id, b64)` (a binary frame); `ws_recv(id)` the next message (""
//!   none; a binary frame comes as "\u{1}" + its base64); `ws_close(id)`.
//! - `json_open(text)` -> handle (-1 when not JSON); `json_get(h, path)`,
//!   `json_num`, `json_len`, `json_type`, `json_keys`, `json_close` -- a path is
//!   dot separated keys and indexes ("d.guilds.0.name", "" the root).
//! - `rsa_oaep_new()` (RSA-2048), `rsa_public_b64`, `rsa_oaep_decrypt`,
//!   `rsa_free`; `base64_encode(text)`, `base64_decode_text(b64)`,
//!   `sha256_b64url(b64)`.
//!
//! Every socket is its own TLS 1.3 stream (`tls::handshake_io` over a netstack
//! socket handle), so a WebSocket can stay open while pages load. Connecting
//! and requests run on green threads (coro.rs) under the net lock, like
//! `netapi`'s jobs; afterwards a WebSocket is pumped without blocking whenever
//! the app polls it, and only while no other task holds the net lock (a page
//! fetch in flight), so the window never waits on it. Server pings are
//! answered here. (The kernel's TLS checks no certificates -- see tls.rs.)

use crate::drivers::netstack as ns;
use crate::lingrt::{boolean, list_from, mk_str, num, to_f64, to_string, UNIT};
use crate::tls::{self, Io, Keys};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use base64::Engine as _;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn log_line(m: &str) {
    crate::console_write(b"lingsock: ");
    crate::console_write(m.as_bytes());
    crate::console_write(b"\n");
}

// ── one TLS (or plain) stream on a socket handle of its own ──

pub(crate) struct Stream {
    h: usize,
    /// None: plain TCP (http:// and ws://)
    keys: Option<(Keys, Keys)>,
    raw: Vec<u8>,
    pub(crate) plain: Vec<u8>,
    pub(crate) closed: bool,
}

/// "host:port" pieces of a URL (http, https, ws, wss); (host, port, path, tls).
pub(crate) fn split_url(url: &str) -> Option<(String, u16, String, bool)> {
    let (rest, tls, port) = if let Some(r) = url.strip_prefix("https://") {
        (r, true, 443)
    } else if let Some(r) = url.strip_prefix("wss://") {
        (r, true, 443)
    } else if let Some(r) = url.strip_prefix("http://") {
        (r, false, 80)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (r, false, 80)
    } else {
        return None;
    };
    let (hostport, path) = match rest.find(|c| c == '/' || c == '?') {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], String::from(&rest[i..])),
        Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
        None => (rest, String::from("/")),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) && !p.is_empty() => (h, p.parse().ok()?),
        _ => (hostport, port),
    };
    if host.is_empty() {
        return None;
    }
    Some((String::from(host), port, path, tls))
}

fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut ip = [0u8; 4];
    let mut it = s.split('.');
    for b in ip.iter_mut() {
        *b = it.next()?.parse().ok()?;
    }
    if it.next().is_some() {
        return None;
    }
    Some(ip)
}

impl Stream {
    /// TLS with the server's certificate checked (tls::handshake_io_opts verify): chain to a
    /// bundled root, host name, dates, CertificateVerify, Finished. The Claude client sends its
    /// API key only over one of these; a check that fails is an error, never a fallback.
    pub(crate) fn connect_verified(host: &str, port: u16) -> Result<Stream, String> {
        Self::connect_verified_at(host, port, host)
    }

    /// `connect_verified` to `addr` while checking the certificate for `name` (SNI + host
    /// name): for tests that put a server with the wrong certificate in front (`claude tls-check`).
    pub(crate) fn connect_verified_at(addr: &str, port: u16, name: &str) -> Result<Stream, String> {
        let host = name;
        let _ = ns::arp_gateway();
        let ip = parse_ip(addr).or_else(|| ns::dns_resolve(addr)).ok_or_else(|| format!("could not resolve {addr}"))?;
        let h = ns::tcp_connect_h(ip, port, 6_000_000).ok_or_else(|| format!("{addr}:{port} did not answer"))?;
        let mut tx = vec![0u8; 4096];
        let mut rx = vec![0u8; 64 * 1024];
        let mut quiet = |_m: &[u8]| {};
        match tls::handshake_io_opts(Io::H(h), host, &mut tx, &mut rx, &mut quiet, true) {
            Ok((c, s, pos, rxlen)) => {
                let mut st = Stream { h, keys: Some((c, s)), raw: rx[pos..rxlen].to_vec(), plain: Vec::new(), closed: false };
                st.decode();
                Ok(st)
            },
            Err(e) => {
                ns::tcp_close_h(h);
                ns::tcp_free_h(h);
                let why = tls::last_verify_error();
                Err(if why.is_empty() { format!("TLS with {host} failed: {e}") } else { format!("refusing {host}: {why}") })
            },
        }
    }

    /// Connect (and handshake). Call with the net lock held (a green thread).
    pub(crate) fn connect(host: &str, port: u16, tls_on: bool) -> Result<Stream, String> {
        let _ = ns::arp_gateway();
        let ip = parse_ip(host).or_else(|| ns::dns_resolve(host)).ok_or_else(|| format!("could not resolve {host}"))?;
        let h = ns::tcp_connect_h(ip, port, 6_000_000).ok_or_else(|| format!("{host}:{port} did not answer"))?;
        if !tls_on {
            return Ok(Stream { h, keys: None, raw: Vec::new(), plain: Vec::new(), closed: false });
        }
        let mut tx = vec![0u8; 4096];
        let mut rx = vec![0u8; 64 * 1024];
        let mut quiet = |_m: &[u8]| {};
        match tls::handshake_io(Io::H(h), host, &mut tx, &mut rx, &mut quiet) {
            Ok((c, s, pos, rxlen)) => {
                let mut st = Stream { h, keys: Some((c, s)), raw: rx[pos..rxlen].to_vec(), plain: Vec::new(), closed: false };
                st.decode();
                Ok(st)
            },
            Err(e) => {
                ns::tcp_close_h(h);
                ns::tcp_free_h(h);
                Err(format!("TLS with {host} failed: {e}"))
            },
        }
    }

    pub(crate) fn send(&mut self, data: &[u8]) -> bool {
        let Some((c, _)) = self.keys.as_mut() else {
            return Io::H(self.h).write(data);
        };
        let mut out = vec![0u8; 4096 + 32];
        for piece in data.chunks(4096) {
            let m = tls::encrypt_record(c, 0x17, piece, &mut out);
            if !Io::H(self.h).write(&out[..m]) {
                return false;
            }
        }
        true
    }

    /// Whole TLS records in `raw` -> application bytes in `plain`.
    fn decode(&mut self) {
        let Some((_, s)) = self.keys.as_mut() else {
            self.plain.extend_from_slice(&self.raw);
            self.raw.clear();
            return;
        };
        let mut at = 0usize;
        while self.raw.len() >= at + 5 {
            let rtype = self.raw[at];
            let rlen = ((self.raw[at + 3] as usize) << 8) | self.raw[at + 4] as usize;
            if self.raw.len() < at + 5 + rlen {
                break;
            }
            let mut hdr = [0u8; 5];
            hdr.copy_from_slice(&self.raw[at..at + 5]);
            let mut body = self.raw[at + 5..at + 5 + rlen].to_vec();
            at += 5 + rlen;
            match rtype {
                0x17 => match tls::decrypt_record(s, &hdr, &mut body) {
                    Some((0x17, n)) => self.plain.extend_from_slice(&body[..n]),
                    Some((0x15, _)) => self.closed = true, // close_notify / alert
                    Some(_) => {},                         // session tickets, key updates
                    None => self.closed = true,
                },
                0x15 => self.closed = true,
                _ => {},
            }
        }
        self.raw.drain(..at);
    }

    /// Take what has arrived (waiting up to `budget_us` for something).
    pub(crate) fn pump(&mut self, budget_us: u64) -> usize {
        if self.closed {
            return 0;
        }
        let mut buf = vec![0u8; 16 * 1024];
        let n = ns::tcp_read_h(self.h, &mut buf, budget_us);
        if n > 0 {
            self.raw.extend_from_slice(&buf[..n]);
            self.decode();
        } else if ns::tcp_closed_h(self.h) {
            self.closed = true;
        }
        n
    }

    pub(crate) fn close(&mut self) {
        ns::tcp_close_h(self.h);
        ns::tcp_free_h(self.h);
        self.closed = true;
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// "Name: value" lines -> header lines for the wire, leaving out the ones the
/// client writes itself.
fn header_block(headers: &str, skip: &[&str]) -> String {
    let mut out = String::new();
    for l in headers.lines() {
        let Some((k, v)) = l.split_once(':') else { continue };
        let k = k.trim();
        if k.is_empty() || k.eq_ignore_ascii_case("x-horizon-jar") || skip.iter().any(|s| k.eq_ignore_ascii_case(s)) {
            continue;
        }
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v.trim());
        out.push_str("\r\n");
    }
    out
}

fn has_header(headers: &str, name: &str) -> bool {
    headers.lines().any(|l| l.split_once(':').map(|(k, _)| k.trim().eq_ignore_ascii_case(name)).unwrap_or(false))
}

fn dechunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0usize;
    while i < body.len() {
        let Some(e) = find(&body[i..], b"\r\n") else { break };
        let line = core::str::from_utf8(&body[i..i + e]).unwrap_or("0");
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("0").trim(), 16).unwrap_or(0);
        i += e + 2;
        if size == 0 {
            break;
        }
        let end = (i + size).min(body.len());
        out.extend_from_slice(&body[i..end]);
        i = end + 2;
    }
    out
}

// ── HTTP with a method and headers ──

const MAX_BODY: usize = 32 << 20;

/// "<status>\n<body>" of one request (a green thread; holds the net lock).
fn request(method: &str, url: &str, headers: &str, body: &str) -> String {
    match request_bytes(method, url, headers, body.as_bytes()) {
        Ok((status, _head, b)) => format!("{status}\n{}", String::from_utf8_lossy(&b)),
        Err(e) => format!("0\nERROR: {e}"),
    }
}

// ── keep-alive: finished connections wait here for the next request to the same
// host (one TCP + TLS handshake per host instead of per request -- a Discord
// channel's avatars, a page's stylesheets and pictures all come from a few hosts).

struct Idle {
    key: String,
    st: Stream,
    at_ms: u64,
}
static mut POOL: Vec<Idle> = Vec::new();
const POOL_MAX: usize = 8;
const POOL_PER_HOST: usize = 3;
/// Servers drop idle keep-alive connections after 5-60 s; ours go sooner.
const POOL_IDLE_MS: u64 = 15_000;

fn pool() -> &'static mut Vec<Idle> {
    unsafe { &mut *(&raw mut POOL) }
}

fn pool_take(key: &str) -> Option<Stream> {
    let now = crate::arch::timer::now_ms();
    let p = pool();
    let mut i = 0;
    while i < p.len() {
        let stale = now.saturating_sub(p[i].at_ms) > POOL_IDLE_MS || ns::tcp_closed_h(p[i].st.h) || ns::tcp_fin_h(p[i].st.h);
        if stale {
            let mut d = p.remove(i);
            d.st.close();
            continue;
        }
        i += 1;
    }
    let i = p.iter().rposition(|x| x.key == key)?;
    let st = p.remove(i).st;
    ns::adopt_socket(st.h, Some(crate::coro::task_key()));
    Some(st)
}

fn pool_put(key: String, st: Stream) {
    let p = pool();
    if p.iter().filter(|x| x.key == key).count() >= POOL_PER_HOST || p.len() >= POOL_MAX {
        let mut old = p.remove(p.iter().position(|x| x.key == key).unwrap_or(0));
        old.st.close();
    }
    ns::adopt_socket(st.h, None);
    p.push(Idle { key, st, at_ms: crate::arch::timer::now_ms() });
}

/// Connections waiting in the pool (the `lingsock` console line).
pub(crate) fn pool_len() -> usize {
    pool().len()
}

/// gzip / deflate bodies (we ask for them: pages and scripts arrive 4-8x smaller).
fn decode_body(enc: &str, b: &[u8], max: usize) -> Option<Vec<u8>> {
    use miniz_oxide::inflate::{decompress_to_vec_with_limit, decompress_to_vec_zlib_with_limit};
    match enc {
        "gzip" | "x-gzip" => {
            if b.len() < 18 || b[0] != 0x1f || b[1] != 0x8b || b[2] != 8 {
                return None;
            }
            let flg = b[3];
            let mut p = 10usize;
            if flg & 4 != 0 {
                let xlen = *b.get(p)? as usize | (*b.get(p + 1)? as usize) << 8;
                p += 2 + xlen;
            }
            for bit in [8u8, 16] {
                if flg & bit != 0 {
                    while *b.get(p)? != 0 {
                        p += 1;
                    }
                    p += 1;
                }
            }
            if flg & 2 != 0 {
                p += 2;
            }
            decompress_to_vec_with_limit(b.get(p..)?, max).ok()
        },
        "deflate" => decompress_to_vec_zlib_with_limit(b, max).or_else(|_| decompress_to_vec_with_limit(b, max)).ok(),
        _ => None,
    }
}

/// Where the response that starts at `plain[0]` ends, once it is all here:
/// Some(end) (bytes of head + body), None while more is to come. `close_delim`
/// = neither a length nor chunks: it ends when the server closes.
fn response_end(plain: &[u8], method: &str) -> (Option<usize>, bool) {
    let Some(he) = find(plain, b"\r\n\r\n") else { return (None, false) };
    let head = String::from_utf8_lossy(&plain[..he]).to_ascii_lowercase();
    let status: u16 = head.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body_at = he + 4;
    if method == "HEAD" || status == 204 || status == 304 || (100..200).contains(&status) {
        return (Some(body_at), false);
    }
    if head.contains("transfer-encoding: chunked") {
        // walk the chunks
        let mut i = body_at;
        loop {
            let Some(e) = find(&plain[i..], b"\r\n") else { return (None, false) };
            let line = core::str::from_utf8(&plain[i..i + e]).unwrap_or("0");
            let size = usize::from_str_radix(line.split(';').next().unwrap_or("0").trim(), 16).unwrap_or(0);
            i += e + 2;
            if size == 0 {
                // trailers end with an empty line
                return match find(&plain[i..], b"\r\n") {
                    Some(0) => (Some(i + 2), false),
                    Some(_) => match find(&plain[i..], b"\r\n\r\n") {
                        Some(t) => (Some(i + t + 4), false),
                        None => (None, false),
                    },
                    None => (None, false),
                };
            }
            if plain.len() < i + size + 2 {
                return (None, false);
            }
            i += size + 2;
        }
    }
    if let Some(cl) = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()) {
        return if plain.len() >= body_at + cl { (Some(body_at + cl), false) } else { (None, false) };
    }
    (None, true)
}

/// One request with a binary body (Spotify's protobuf APIs): (status, response
/// head, body). A green thread or a task of its own; takes a SHARED net claim,
/// so any number of these run side by side, each on its own socket.
pub(crate) fn request_bytes(method: &str, url: &str, headers: &str, body: &[u8]) -> Result<(u16, String, Vec<u8>), String> {
    request_full(method, url, headers, body, MAX_BODY)
}

/// `request_bytes` with a size limit: a body over `max` bytes is an error.
/// The connection comes from (and goes back to) the keep-alive pool; the body
/// is un-chunked and un-gzipped; the returned head keeps the status line and
/// headers minus transfer-encoding / content-encoding / content-length (they
/// describe the wire, not the body handed back).
pub(crate) fn request_full(method: &str, url: &str, headers: &str, body: &[u8], max: usize) -> Result<(u16, String, Vec<u8>), String> {
    let _g = ns::NetShare::new();
    let Some((host, port, path, tls_on)) = split_url(url) else {
        return Err(format!("not an http(s) URL: {url}"));
    };
    let m = method.trim().to_ascii_uppercase();
    let m = if m.is_empty() { String::from("GET") } else { m };
    let mut req = format!("{m} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: keep-alive\r\nAccept-Encoding: gzip, deflate\r\n");
    if !has_header(headers, "user-agent") {
        req.push_str("User-Agent: LingOS Horizon\r\n");
    }
    if !has_header(headers, "accept") {
        req.push_str("Accept: */*\r\n");
    }
    req.push_str(&header_block(headers, &["host", "connection", "content-length", "accept-encoding", "transfer-encoding"]));
    if !body.is_empty() || m == "POST" || m == "PUT" || m == "PATCH" {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    let mut wire = req.into_bytes();
    wire.extend_from_slice(body);
    let key = format!("{}://{host}:{port}", if tls_on { "s" } else { "p" });
    // a pooled connection the server already dropped answers nothing: then once more on a new one
    for attempt in 0..2 {
        let reused = attempt == 0;
        let mut st = match if reused { pool_take(&key) } else { None } {
            Some(st) => st,
            None if reused => continue,
            None => Stream::connect(&host, port, tls_on)?,
        };
        st.plain.clear();
        if !st.send(&wire) {
            st.close();
            if reused {
                continue;
            }
            return Err(format!("could not send the request to {host}"));
        }
        let mut idle = 0;
        let (end, close_delim) = loop {
            let (end, cd) = response_end(&st.plain, &m);
            if end.is_some() {
                break (end, false);
            }
            if st.plain.len() > max + 64 * 1024 {
                st.close();
                return Err(format!("{url}: the answer is over {} KB", max / 1024));
            }
            if st.closed {
                break (None, cd);
            }
            // nothing at all from a reused connection within a short wait: it was dead
            let wait = if reused && st.plain.is_empty() { 1_500_000 } else { 4_000_000 };
            if st.pump(wait) == 0 {
                idle += 1;
                if st.closed || idle >= 3 || (reused && st.plain.is_empty()) {
                    break (None, cd);
                }
            } else {
                idle = 0;
            }
        };
        if reused && st.plain.is_empty() {
            st.close();
            continue;
        }
        let Some(he) = find(&st.plain, b"\r\n\r\n") else {
            st.close();
            return Err(format!("no answer from {host}"));
        };
        let total = match end {
            Some(e) => e,
            None if close_delim => st.plain.len(),
            None => {
                st.close();
                return Err(format!("{host} stopped sending mid-answer"));
            },
        };
        let head = String::from_utf8_lossy(&st.plain[..he]).into_owned();
        let lower = head.to_ascii_lowercase();
        let status: u16 = head.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let mut b = st.plain[he + 4..total].to_vec();
        if lower.contains("transfer-encoding: chunked") {
            b = dechunk(&b);
        }
        let enc = lower.lines().find_map(|l| l.strip_prefix("content-encoding:")).map(|v| v.trim().to_string()).unwrap_or_default();
        let mut decoded_ok = true;
        if !enc.is_empty() && enc != "identity" {
            match decode_body(&enc, &b, max) {
                Some(d) => b = d,
                None => decoded_ok = false,
            }
        }
        let reusable = end.is_some() && st.plain.len() == total && !lower.contains("connection: close") && !st.closed;
        if reusable {
            st.plain.clear();
            pool_put(key.clone(), st);
        } else {
            st.close();
        }
        if b.len() > max {
            return Err(format!("{url}: the answer is over {} KB", max / 1024));
        }
        // the head as the body is handed back
        let mut out_head = String::new();
        for (i, l) in head.split("\r\n").enumerate() {
            let k = l.split(':').next().unwrap_or("").trim().to_ascii_lowercase();
            if i > 0 && (k == "transfer-encoding" || k == "content-length" || (decoded_ok && k == "content-encoding")) {
                continue;
            }
            if i > 0 {
                out_head.push_str("\r\n");
            }
            out_head.push_str(l);
        }
        return Ok((status, out_head, b));
    }
    Err(format!("no answer from {host}"))
}

/// An upload whose body streams from `read` (a file read piece by piece) instead of one buffer:
/// Discord's attachments go as a PUT of the raw file to a signed Google Cloud Storage URL
/// (storage.googleapis.com), up to 500 MB. The URL is sent exactly as given (its query is the
/// signature). HTTPS here always checks the server's certificate (`connect_verified`): the bytes
/// are someone's file. `read(offset, buf)` fills `buf` (cleared) with the next piece, false on a
/// read error; `sent(bytes)` follows the body as it goes. A fresh connection (not the pool), and
/// (status, response head, body) back like `request_full`; the answer body is kept to `max`.
pub(crate) fn request_upload(
    method: &str,
    url: &str,
    headers: &str,
    len: u64,
    read: &mut dyn FnMut(u64, &mut Vec<u8>) -> bool,
    sent: &mut dyn FnMut(u64),
    max: usize,
) -> Result<(u16, String, Vec<u8>), String> {
    let _g = ns::NetShare::new();
    let Some((host, port, path, tls_on)) = split_url(url) else {
        return Err(format!("not an http(s) URL: {url}"));
    };
    let m = method.trim().to_ascii_uppercase();
    let mut req = format!("{m} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if !has_header(headers, "user-agent") {
        req.push_str("User-Agent: LingOS Horizon\r\n");
    }
    req.push_str(&header_block(headers, &["host", "connection", "content-length", "accept-encoding", "transfer-encoding"]));
    req.push_str(&format!("Content-Length: {len}\r\n\r\n"));
    let mut st = if tls_on { Stream::connect_verified(&host, port)? } else { Stream::connect(&host, port, false)? };
    st.plain.clear();
    if !st.send(req.as_bytes()) {
        st.close();
        return Err(format!("could not send the request to {host}"));
    }
    let mut off = 0u64;
    let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    while off < len {
        buf.clear();
        if !read(off, &mut buf) || buf.is_empty() {
            st.close();
            return Err(format!("the file could not be read at {off} of {len} bytes"));
        }
        let take = (buf.len() as u64).min(len - off) as usize;
        if !st.send(&buf[..take]) {
            // a server that refuses the request (bad signature, no such bucket) answers right
            // after the headers and closes: our next write fails, but its answer is here -- read
            // it and report that, not just "stopped taking"
            let mut tries = 0;
            while find(&st.plain, b"\r\n\r\n").is_none() && !st.closed && tries < 4 {
                st.pump(1_000_000);
                tries += 1;
            }
            st.pump(200_000);
            if find(&st.plain, b"\r\n\r\n").is_some() {
                break;
            }
            let why = format!("{}, unacked segments {}, room for a segment: {}", ns::tcp_debug_h(st.h), ns::tcp_unacked_h(st.h), ns::tcp_writable_h(st.h, 1400));
            log_line(&format!("upload to {host} stopped at {off} of {len} bytes: {why}"));
            st.close();
            return Err(format!("{host} stopped taking the upload at {off} of {len} bytes ({why})"));
        }
        off += take as u64;
        sent(off);
        // a server that refused early (403) may have answered already: stop sending
        if find(&st.plain, b"\r\n\r\n").is_some() {
            break;
        }
    }
    // the answer: until its end, or the server closing (Connection: close)
    let mut idle = 0;
    loop {
        let (end, _) = response_end(&st.plain, &m);
        if end.is_some() || st.closed || st.plain.len() > max + 64 * 1024 {
            break;
        }
        if st.pump(4_000_000) == 0 {
            idle += 1;
            if idle >= 8 {
                break;
            }
        } else {
            idle = 0;
        }
    }
    let Some(he) = find(&st.plain, b"\r\n\r\n") else {
        st.close();
        return Err(format!("no answer from {host} after the upload"));
    };
    let head = String::from_utf8_lossy(&st.plain[..he]).into_owned();
    let lower = head.to_ascii_lowercase();
    let status: u16 = head.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let total = response_end(&st.plain, &m).0.unwrap_or(st.plain.len()).min(st.plain.len());
    let mut b = st.plain[he + 4..total].to_vec();
    st.close();
    if lower.contains("transfer-encoding: chunked") {
        b = dechunk(&b);
    }
    let enc = lower.lines().find_map(|l| l.strip_prefix("content-encoding:")).map(|v| v.trim().to_string()).unwrap_or_default();
    if !enc.is_empty() && enc != "identity" {
        if let Some(d) = decode_body(&enc, &b, max) {
            b = d;
        }
    }
    b.truncate(max);
    Ok((status, head, b))
}

static mut JOBS: Vec<(String, Option<String>)> = Vec::new();
static mut NEXT_ID: u64 = 1;

fn jobs() -> &'static mut Vec<(String, Option<String>)> {
    unsafe { &mut *(&raw mut JOBS) }
}

fn next_id() -> u64 {
    unsafe {
        NEXT_ID += 1;
        NEXT_ID
    }
}

fn http_start(method: String, url: String, headers: String, body: String) -> String {
    let n = next_id();
    owned(OWN_JOB, n as i64);
    let id = format!("lsq{n}");
    jobs().push((id.clone(), None));
    let jid = id.clone();
    let spawned = crate::coro::spawn(
        "lingsock_http",
        false,
        Box::new(move || {
            let out = request(&method, &url, &headers, &body);
            if let Some(j) = jobs().iter_mut().find(|j| j.0 == jid) {
                j.1 = Some(out);
            }
        }),
    );
    if spawned == 0 {
        jobs().retain(|j| j.0 != id);
        return String::new();
    }
    id
}

fn http_poll(id: &str) -> String {
    match jobs().iter().position(|j| j.0 == id && j.1.is_some()) {
        Some(i) => jobs().remove(i).1.unwrap_or_default(),
        None if !jobs().iter().any(|j| j.0 == id) => String::from("0\nERROR: no such request"),
        None => String::new(),
    }
}

// ── WebSocket client ──

struct Ws {
    id: i64,
    state: String,
    stream: Option<Stream>,
    inbox: VecDeque<String>,
    outq: VecDeque<Vec<u8>>,
    frag: Vec<u8>,
    frag_op: u8,
    cancel: bool,
    /// an in-kernel engine instead of TCP (lingspot: Spotify's "helper"), its id; 0 = none
    local: i64,
}

static mut WS: Vec<Box<Ws>> = Vec::new();
const MAX_WS: usize = 16;
const MAX_MSG: usize = 64 << 20;

fn sockets() -> &'static mut Vec<Box<Ws>> {
    unsafe { &mut *(&raw mut WS) }
}

fn ws_get(id: i64) -> Option<&'static mut Ws> {
    sockets().iter_mut().find(|w| w.id == id).map(|b| &mut **b)
}

fn frame(op: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(payload.len() + 14);
    f.push(0x80 | op);
    let n = payload.len();
    if n < 126 {
        f.push(0x80 | n as u8);
    } else if n < 65536 {
        f.push(0x80 | 126);
        f.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        f.push(0x80 | 127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    let mut mask = [0u8; 4];
    crate::crypto::random_bytes(&mut mask);
    f.extend_from_slice(&mask);
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
    f
}

fn ws_connect(url: &str, headers: &str) -> Result<Stream, String> {
    let _g = ns::NetShare::new();
    let (host, port, path, tls_on) = split_url(url).ok_or_else(|| format!("not a ws(s) URL: {url}"))?;
    let mut st = Stream::connect(&host, port, tls_on)?;
    let mut key = [0u8; 16];
    crate::crypto::random_bytes(&mut key);
    let mut req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n",
        b64().encode(key)
    );
    if !has_header(headers, "user-agent") {
        req.push_str("User-Agent: LingOS Horizon\r\n");
    }
    req.push_str(&header_block(headers, &["host", "upgrade", "connection", "sec-websocket-key", "sec-websocket-version", "sec-websocket-extensions"]));
    req.push_str("\r\n");
    if !st.send(req.as_bytes()) {
        st.close();
        return Err(format!("could not send the upgrade to {host}"));
    }
    let mut waited = 0;
    let he = loop {
        if let Some(he) = find(&st.plain, b"\r\n\r\n") {
            break he;
        }
        if st.closed || waited >= 4 {
            st.close();
            return Err(format!("{host} did not answer the WebSocket upgrade"));
        }
        if st.pump(3_000_000) == 0 {
            waited += 1;
        }
    };
    let head = String::from_utf8_lossy(&st.plain[..he]).into_owned();
    if head.split(' ').nth(1) != Some("101") {
        st.close();
        return Err(format!("upgrade refused: {}", head.lines().next().unwrap_or("")));
    }
    st.plain.drain(..he + 4);
    Ok(st)
}

// ── Per-app ownership (appres.rs): WebSockets, requests and JSON documents
// an app opened go when it ends ──

/// (id, owner) of each WebSocket / JSON document / request an app made.
static mut OWNED: Vec<(u8, i64, u32)> = Vec::new();
const OWN_WS: u8 = 0;
const OWN_DOC: u8 = 1;
const OWN_JOB: u8 = 2;

fn owned(kind: u8, id: i64) {
    let who = crate::appres::owner();
    if who != 0 {
        unsafe { (*(&raw mut OWNED)).push((kind, id, who)) };
    }
}

/// App `uid` ended: its WebSockets close (a connect still running is
/// cancelled), its JSON documents and request answers go.
pub fn app_ended(uid: u32, f: &mut crate::appres::Freed) {
    let all = unsafe { &mut *(&raw mut OWNED) };
    let mine: Vec<(u8, i64)> = all.iter().filter(|o| o.2 == uid).map(|o| (o.0, o.1)).collect();
    all.retain(|o| o.2 != uid);
    for (kind, id) in mine {
        match kind {
            OWN_WS => {
                if let Some(w) = ws_get(id) {
                    if w.local != 0 {
                        crate::lingspot::ws_detach(w.local);
                    } else if w.state == "connecting" {
                        // the connecting green thread was killed with the app; its socket with it
                        w.cancel = true;
                    } else if let Some(st) = w.stream.as_mut() {
                        if w.state == "open" {
                            let _ = st.send(&frame(8, &1001u16.to_be_bytes()));
                        }
                        // abrupt: no FIN handshake wait on the desktop's time
                        ns::tcp_free_h(st.h);
                        st.closed = true;
                    }
                    f.sockets += 1;
                }
                sockets().retain(|w| w.id != id);
            },
            OWN_DOC => {
                jdocs().retain(|d| d.0 != id);
            },
            _ => {
                let key = format!("lsq{id}");
                let before = jobs().len();
                jobs().retain(|j| j.0 != key);
                f.jobs += (before - jobs().len()) as u32;
            },
        }
    }
}

fn ws_open(url: String, headers: String) -> i64 {
    sockets().retain(|w| !(w.state.starts_with("closed") || w.state.starts_with("error")) || !w.inbox.is_empty());
    if sockets().len() >= MAX_WS {
        return -1;
    }
    let id = next_id() as i64;
    owned(OWN_WS, id);
    // ws://127.0.0.1:<port>/<key> of the in-kernel Spotify engine: its queues, no TCP
    if let Some(eid) = crate::lingspot::ws_attach(&url) {
        sockets().push(Box::new(Ws {
            id,
            state: if eid > 0 { String::from("open") } else { String::from("error: refused") },
            stream: None,
            inbox: VecDeque::new(),
            outq: VecDeque::new(),
            frag: Vec::new(),
            frag_op: 1,
            cancel: false,
            local: eid.max(0),
        }));
        return id;
    }
    sockets().push(Box::new(Ws {
        id,
        state: String::from("connecting"),
        stream: None,
        inbox: VecDeque::new(),
        outq: VecDeque::new(),
        frag: Vec::new(),
        frag_op: 1,
        cancel: false,
        local: 0,
    }));
    let spawned = crate::coro::spawn(
        "lingsock_ws",
        false,
        Box::new(move || {
            let r = ws_connect(&url, &headers);
            let Some(w) = ws_get(id) else {
                if let Ok(mut s) = r {
                    s.close();
                }
                return;
            };
            match r {
                Ok(mut s) if w.cancel => {
                    s.close();
                    w.state = String::from("closed: 1000 closed by us");
                },
                Ok(s) => {
                    w.stream = Some(s);
                    w.state = String::from("open");
                    parse_frames(w);
                },
                Err(e) => {
                    log_line(&e);
                    w.state = format!("error: {e}");
                },
            }
        }),
    );
    if spawned == 0 {
        if let Some(w) = ws_get(id) {
            w.state = String::from("error: no green thread for the connection");
        }
    }
    id
}

/// Complete frames in the stream's plain bytes -> messages (control frames are
/// handled here: ping -> pong, close -> state).
fn parse_frames(w: &mut Ws) {
    let Some(st) = w.stream.as_mut() else { return };
    loop {
        let p = &st.plain;
        if p.len() < 2 {
            break;
        }
        let (b0, b1) = (p[0], p[1]);
        let mut hl = 2usize;
        let mut len = (b1 & 0x7f) as usize;
        if len == 126 {
            if p.len() < 4 {
                break;
            }
            len = u16::from_be_bytes([p[2], p[3]]) as usize;
            hl = 4;
        } else if len == 127 {
            if p.len() < 10 {
                break;
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&p[2..10]);
            len = u64::from_be_bytes(b).min(MAX_MSG as u64 + 1) as usize;
            hl = 10;
        }
        let masked = b1 & 0x80 != 0;
        let mut mask = [0u8; 4];
        if masked {
            if p.len() < hl + 4 {
                break;
            }
            mask.copy_from_slice(&p[hl..hl + 4]);
            hl += 4;
        }
        if len > MAX_MSG {
            w.state = String::from("error: message too big");
            st.close();
            return;
        }
        if p.len() < hl + len {
            break;
        }
        let mut payload = p[hl..hl + len].to_vec();
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i & 3];
            }
        }
        st.plain.drain(..hl + len);
        let fin = b0 & 0x80 != 0;
        let op = b0 & 0x0f;
        match op {
            0 | 1 | 2 => {
                if op != 0 {
                    w.frag_op = op;
                    w.frag.clear();
                }
                w.frag.extend_from_slice(&payload);
                if fin {
                    let data = core::mem::take(&mut w.frag);
                    let msg = if w.frag_op == 2 { format!("\u{1}{}", b64().encode(&data)) } else { String::from_utf8_lossy(&data).into_owned() };
                    w.inbox.push_back(msg);
                }
            },
            8 => {
                let code = if payload.len() >= 2 { u16::from_be_bytes([payload[0], payload[1]]) } else { 1005 };
                let why = if payload.len() > 2 { String::from_utf8_lossy(&payload[2..]).into_owned() } else { String::new() };
                let _ = st.send(&frame(8, &payload[..payload.len().min(2)]));
                st.close();
                w.state = format!("closed: {code} {why}").trim_end().to_string();
                log_line(&format!("ws {} {}", w.id, w.state));
                return;
            },
            9 => w.outq.push_back(frame(10, &payload)),
            _ => {},
        }
    }
    if st.closed && w.state == "open" {
        w.state = String::from("closed: 1006 connection lost");
        log_line(&format!("ws {} {}", w.id, w.state));
    }
}

/// Send what is queued and take what arrived, without waiting -- and only when
/// no other task is in the middle of a network transaction.
fn ws_service(w: &mut Ws) {
    if w.local != 0 {
        if w.state == "open" {
            while w.inbox.len() < 256 {
                match crate::lingspot::ws_from(w.local) {
                    Some(m) => w.inbox.push_back(m),
                    None => break,
                }
            }
            if !crate::lingspot::ws_alive(w.local) {
                w.state = String::from("closed: 1000 the engine stopped");
            }
        }
        return;
    }
    if w.state != "open" || w.stream.is_none() {
        return;
    }
    if ns::net_busy_for_me() {
        return;
    }
    let st = w.stream.as_mut().unwrap();
    while let Some(f) = w.outq.pop_front() {
        if !st.send(&f) {
            w.state = String::from("error: send failed");
            st.close();
            return;
        }
    }
    let mut rounds = 0;
    while rounds < 64 && st.pump(0) > 0 {
        rounds += 1;
    }
    parse_frames(w);
}

fn ws_send(id: i64, data: &[u8], op: u8) -> bool {
    let Some(w) = ws_get(id) else { return false };
    if w.state != "open" {
        return false;
    }
    if w.local != 0 {
        return op == 1 && crate::lingspot::ws_to(w.local, &String::from_utf8_lossy(data));
    }
    w.outq.push_back(frame(op, data));
    ws_service(w);
    true
}

fn ws_close(id: i64) -> bool {
    let Some(w) = ws_get(id) else { return true };
    if w.local != 0 {
        crate::lingspot::ws_detach(w.local);
        w.state = String::from("closed: 1000 closed by us");
        return true;
    }
    if w.state == "connecting" {
        w.cancel = true;
        return true;
    }
    if let Some(st) = w.stream.as_mut() {
        if w.state == "open" {
            let _ = st.send(&frame(8, &1000u16.to_be_bytes()));
        }
        st.close();
    }
    w.state = String::from("closed: 1000 closed by us");
    true
}

// ── WebSockets for in-kernel clients (lingspot's Spotify "dealer" push channel) ──
// The same sockets as an app's, without an owning app: the engine that opened one closes it.

/// Connect (blocking: call from a green thread) -> the socket's id.
pub(crate) fn kws_open(url: &str, headers: &str) -> Result<i64, String> {
    let st = ws_connect(url, headers)?;
    let id = next_id() as i64;
    sockets().push(Box::new(Ws {
        id,
        state: String::from("open"),
        stream: Some(st),
        inbox: VecDeque::new(),
        outq: VecDeque::new(),
        frag: Vec::new(),
        frag_op: 1,
        cancel: false,
        local: 0,
    }));
    Ok(id)
}

/// A text message out.
pub(crate) fn kws_send(id: i64, text: &str) -> bool {
    ws_send(id, text.as_bytes(), 1)
}

/// The next message in (binary ones as "\u{1}" + base64), without waiting.
pub(crate) fn kws_recv(id: i64) -> Option<String> {
    let w = ws_get(id)?;
    if w.inbox.is_empty() {
        ws_service(w);
    }
    w.inbox.pop_front()
}

/// "open", "connecting", "closed: ..." or "error: ..." ("" = no such socket).
pub(crate) fn kws_state(id: i64) -> String {
    ws_get(id).map(|w| w.state.clone()).unwrap_or_default()
}

pub(crate) fn kws_close(id: i64) {
    ws_close(id);
    sockets().retain(|w| w.id != id);
}

// ── JSON documents ──

static mut JDOCS: Vec<(i64, serde_json::Value)> = Vec::new();

fn jdocs() -> &'static mut Vec<(i64, serde_json::Value)> {
    unsafe { &mut *(&raw mut JDOCS) }
}

fn jpath<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    use serde_json::Value as J;
    let mut cur = v;
    if path.is_empty() {
        return Some(cur);
    }
    for seg in path.split('.') {
        cur = match cur {
            J::Object(m) => m.get(seg)?,
            J::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn jdoc<R>(h: i64, path: &str, f: impl FnOnce(Option<&serde_json::Value>) -> R) -> R {
    f(jdocs().iter().find(|d| d.0 == h).and_then(|d| jpath(&d.1, path)))
}

fn jtext(v: Option<&serde_json::Value>) -> String {
    use serde_json::Value as J;
    match v {
        None | Some(J::Null) => String::new(),
        Some(J::String(s)) => s.clone(),
        Some(J::Bool(b)) => b.to_string(),
        Some(J::Number(n)) => n.to_string(),
        Some(o) => o.to_string(),
    }
}

// ── RSA-OAEP (Discord's QR sign-in) ──

struct KRng;
impl rand_core::RngCore for KRng {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.fill_bytes(&mut b);
        u32::from_le_bytes(b)
    }
    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill_bytes(&mut b);
        u64::from_le_bytes(b)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        crate::crypto::random_bytes(dest);
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}
impl rand_core::CryptoRng for KRng {}

static mut RSA_KEYS: Vec<(i64, rsa::RsaPrivateKey)> = Vec::new();

fn rsa_keys() -> &'static mut Vec<(i64, rsa::RsaPrivateKey)> {
    unsafe { &mut *(&raw mut RSA_KEYS) }
}

// ── the builtins ──

pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    use serde_json::Value as J;
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let n = |i: usize| args.get(i).map(|&v| to_f64(v) as i64).unwrap_or(-1);
    Some(match name {
        b"http_request_async" => mk_str(&http_start(s(0), s(1), s(2), s(3))),
        b"http_req_poll" => mk_str(&http_poll(&s(0))),
        b"ws_open" => num(ws_open(s(0), s(1)) as f64),
        b"ws_state" => match ws_get(n(0)) {
            Some(w) => {
                ws_service(w);
                mk_str(&w.state.clone())
            },
            None => mk_str("closed: no such socket"),
        },
        b"ws_send" => boolean(ws_send(n(0), s(1).as_bytes(), 1)),
        b"ws_send_b64" => boolean(ws_send(n(0), &b64().decode(s(1).trim()).unwrap_or_default(), 2)),
        b"ws_recv" => match ws_get(n(0)) {
            Some(w) => {
                if w.inbox.is_empty() {
                    ws_service(w);
                }
                mk_str(&w.inbox.pop_front().unwrap_or_default())
            },
            None => mk_str(""),
        },
        b"ws_close" => boolean(ws_close(n(0))),
        b"json_open" => match serde_json::from_str::<J>(&s(0)) {
            Ok(v) => {
                let h = next_id() as i64;
                jdocs().push((h, v));
                owned(OWN_DOC, h);
                num(h as f64)
            },
            Err(_) => num(-1.0),
        },
        b"json_get" => mk_str(&jdoc(n(0), &s(1), jtext)),
        b"json_num" => num(jdoc(n(0), &s(1), |v| match v {
            Some(J::Number(x)) => x.as_f64().unwrap_or(0.0),
            Some(J::String(t)) => t.trim().parse::<f64>().unwrap_or(0.0),
            Some(J::Bool(b)) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            },
            _ => 0.0,
        })),
        b"json_len" => num(jdoc(n(0), &s(1), |v| match v {
            Some(J::Array(a)) => a.len() as f64,
            Some(J::Object(m)) => m.len() as f64,
            Some(J::String(t)) => t.chars().count() as f64,
            Some(_) => 0.0,
            None => -1.0,
        })),
        b"json_type" => mk_str(jdoc(n(0), &s(1), |v| match v {
            None => "",
            Some(J::Null) => "null",
            Some(J::Bool(_)) => "bool",
            Some(J::Number(_)) => "num",
            Some(J::String(_)) => "str",
            Some(J::Array(_)) => "arr",
            Some(J::Object(_)) => "obj",
        })),
        b"json_keys" => {
            let keys: Vec<String> = jdoc(n(0), &s(1), |v| match v {
                Some(J::Object(m)) => m.keys().cloned().collect(),
                Some(J::Array(a)) => (0..a.len()).map(|i| i.to_string()).collect(),
                _ => Vec::new(),
            });
            list_from(keys.iter().map(|k| mk_str(k)).collect())
        },
        b"json_close" => {
            let h = n(0);
            jdocs().retain(|d| d.0 != h);
            boolean(true)
        },
        b"rsa_oaep_new" => match rsa::RsaPrivateKey::new(&mut KRng, 2048) {
            Ok(k) => {
                let h = next_id() as i64;
                rsa_keys().push((h, k));
                num(h as f64)
            },
            Err(_) => num(-1.0),
        },
        b"rsa_public_b64" => {
            use rsa::pkcs8::EncodePublicKey;
            let h = n(0);
            let out = rsa_keys()
                .iter()
                .find(|k| k.0 == h)
                .and_then(|k| k.1.to_public_key().to_public_key_der().ok())
                .map(|d| b64().encode(d.as_bytes()))
                .unwrap_or_default();
            mk_str(&out)
        },
        b"rsa_oaep_decrypt" => {
            let h = n(0);
            let data = b64().decode(s(1).trim()).unwrap_or_default();
            let out = rsa_keys()
                .iter()
                .find(|k| k.0 == h)
                .and_then(|k| k.1.decrypt(rsa::Oaep::new::<sha2::Sha256>(), &data).ok())
                .map(|p| b64().encode(p))
                .unwrap_or_default();
            mk_str(&out)
        },
        b"rsa_free" => {
            let h = n(0);
            rsa_keys().retain(|k| k.0 != h);
            boolean(true)
        },
        // the PC runtime's json_escape (Horizon's apps build JSON by concatenation:
        // without it every quoted value read as 0 -- Discord's QR got "Handshake Error")
        b"json_escape" => {
            let t = s(0);
            let mut out = String::with_capacity(t.len() + 8);
            for c in t.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                    c => out.push(c),
                }
            }
            mk_str(&out)
        },
        b"base64_encode" => mk_str(&b64().encode(s(0).as_bytes())),
        b"base64_decode_text" => {
            let b = b64().decode(s(0).trim()).unwrap_or_default();
            mk_str(&String::from_utf8_lossy(&b))
        },
        b"sha256_b64url" => {
            use sha2::Digest;
            let b = b64().decode(s(0).trim()).unwrap_or_default();
            let d = sha2::Sha256::digest(&b);
            mk_str(&base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(d))
        },
        // `lingsock` console: what is open
        _ => return None,
    })
}

/// Console / SSH `lingsock [get <url> | ws <url> [text]]`: the sockets, or a quick check.
pub fn command(args: &str) -> String {
    let a = args.trim();
    if let Some(u) = a.strip_prefix("get ") {
        let r = request("GET", u.trim(), "", "");
        let (st, body) = r.split_once('\n').unwrap_or(("0", ""));
        return format!("status {st}, {} bytes: {}\n", body.len(), body.chars().take(300).collect::<String>());
    }
    if a == "cache" {
        return crate::drivers::netmedia::cache_report();
    }
    if a == "cache clear" {
        return crate::drivers::netmedia::cache_clear();
    }
    // `lingsock par <n> <url>...`: the same requests one after another, then all at once
    // (green threads, shared net claim) -- the multiplexing check
    if let Some(rest) = a.strip_prefix("par ") {
        let mut it = rest.split_whitespace();
        let n: usize = it.next().and_then(|x| x.parse().ok()).unwrap_or(4).clamp(1, 16);
        let urls: Vec<String> = it.map(String::from).collect();
        if urls.is_empty() {
            return String::from("lingsock par <n> <url> [url...]\n");
        }
        let list: Vec<String> = (0..n).map(|i| urls[i % urls.len()].clone()).collect();
        let now = || crate::arch::timer::now_ms();
        let mut out = String::new();
        let t0 = now();
        let mut ok = 0;
        for u in &list {
            if let Ok((200, _, _)) = request_bytes("GET", u, "", b"") {
                ok += 1;
            }
        }
        out.push_str(&format!("one after another: {} of {n} OK in {} ms\n", ok, now() - t0));
        let t1 = now();
        let ids: Vec<String> = list.iter().map(|u| http_start(String::from("GET"), u.clone(), String::new(), String::new())).collect();
        let mut done = alloc::vec![false; ids.len()];
        let mut ok = 0;
        while done.iter().any(|d| !d) && now() - t1 < 60_000 {
            for (i, id) in ids.iter().enumerate() {
                if !done[i] {
                    let r = http_poll(id);
                    if !r.is_empty() {
                        done[i] = true;
                        if r.starts_with("200\n") {
                            ok += 1;
                        }
                    }
                }
            }
            crate::proc::sched::yield_now();
        }
        out.push_str(&format!("all at once:       {} of {n} OK in {} ms\n", ok, now() - t1));
        out.push_str(&crate::drivers::netmedia::cache_report());
        return out;
    }
    // `lingsock img <url>`: a picture through Horizon's image_fetch (the cache says where it came from)
    if let Some(u) = a.strip_prefix("img ") {
        let t0 = crate::arch::timer::now_ms();
        let job = crate::drivers::netmedia::image_fetch(u.trim());
        let mut r = -2.0;
        while r == -2.0 && crate::arch::timer::now_ms() - t0 < 30_000 {
            crate::proc::sched::yield_now();
            r = crate::drivers::netmedia::image_job(job);
        }
        return format!("picture {r} in {} ms\n{}", crate::arch::timer::now_ms() - t0, crate::drivers::netmedia::cache_report());
    }
    if a == "ra" {
        return remote_auth_check();
    }
    if let Some(rest) = a.strip_prefix("ws ") {
        let mut it = rest.trim().splitn(2, ' ');
        let u = it.next().unwrap_or("");
        let text = it.next().unwrap_or("").trim();
        let mut out = String::new();
        match ws_connect(u, "") {
            Ok(st) => {
                let id = next_id() as i64;
                sockets().push(Box::new(Ws {
                    id,
                    state: String::from("open"),
                    stream: Some(st),
                    inbox: VecDeque::new(),
                    outq: VecDeque::new(),
                    frag: Vec::new(),
                    frag_op: 1,
                    cancel: false,
                    local: 0,
                }));
                out.push_str("open\n");
                if !text.is_empty() {
                    ws_send(id, text.as_bytes(), 1);
                    out.push_str(&format!("sent {text}\n"));
                }
                let t0 = crate::arch::timer::now_ms();
                while crate::arch::timer::now_ms().saturating_sub(t0) < 4000 {
                    let w = ws_get(id).unwrap();
                    if let Some(st) = w.stream.as_mut() {
                        st.pump(200_000);
                    }
                    parse_frames(w);
                    while let Some(m) = w.inbox.pop_front() {
                        out.push_str(&format!("recv {}\n", m.chars().take(300).collect::<String>()));
                    }
                    if w.state != "open" {
                        break;
                    }
                }
                ws_close(id);
                out.push_str(&format!("{}\n", ws_get(id).map(|w| w.state.clone()).unwrap_or_default()));
            },
            Err(e) => out.push_str(&format!("error: {e}\n")),
        }
        return out;
    }
    let mut out = String::new();
    for w in sockets().iter() {
        out.push_str(&format!("ws {}: {} ({} waiting)\n", w.id, w.state, w.inbox.len()));
    }
    out.push_str(&format!("{} request(s) running, {} json document(s), {} rsa key(s)\n", jobs().iter().filter(|j| j.1.is_none()).count(), jdocs().len(), rsa_keys().len()));
    out.push_str("usage: lingsock [get <url> | ws <url> [text to send]]\n");
    out
}

/// `lingsock ra`: Discord's QR sign-in handshake, as Horizon's discord.ling does it, up to the
/// QR code (hello -> init with an RSA-2048 public key -> nonce_proof -> pending_remote_init).
fn remote_auth_check() -> String {
    let mut out = String::new();
    let t = || crate::arch::timer::now_ms();
    let t0 = t();
    let key = match rsa::RsaPrivateKey::new(&mut KRng, 2048) {
        Ok(k) => k,
        Err(_) => return String::from("rsa keygen failed\n"),
    };
    out.push_str(&format!("rsa key in {} ms\n", t() - t0));
    let pubb64 = {
        use rsa::pkcs8::EncodePublicKey;
        key.to_public_key().to_public_key_der().map(|d| b64().encode(d.as_bytes())).unwrap_or_default()
    };
    let hdr = "Origin: https://discord.com\nUser-Agent: Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Horizon/0.1";
    let st = match ws_connect("wss://remote-auth-gateway.discord.gg/?v=2", hdr) {
        Ok(s) => s,
        Err(e) => return format!("{out}connect: {e}\n"),
    };
    let id = next_id() as i64;
    sockets().push(Box::new(Ws {
        id,
        state: String::from("open"),
        stream: Some(st),
        inbox: VecDeque::new(),
        outq: VecDeque::new(),
        frag: Vec::new(),
        frag_op: 1,
        cancel: false,
        local: 0,
    }));
    let t1 = t();
    while t().saturating_sub(t1) < 15_000 {
        let w = ws_get(id).unwrap();
        if let Some(st) = w.stream.as_mut() {
            st.pump(200_000);
        }
        parse_frames(w);
        let mut done = false;
        while let Some(m) = w.inbox.pop_front() {
            out.push_str(&format!("recv {}\n", m.chars().take(160).collect::<String>()));
            let v: serde_json::Value = serde_json::from_str(&m).unwrap_or(serde_json::Value::Null);
            match v.get("op").and_then(|o| o.as_str()).unwrap_or("") {
                "hello" => {
                    let msg = format!("{{\"op\":\"init\",\"encoded_public_key\":\"{pubb64}\"}}");
                    w.outq.push_back(frame(1, msg.as_bytes()));
                    out.push_str("sent init\n");
                },
                "nonce_proof" => {
                    let enc = b64().decode(v.get("encrypted_nonce").and_then(|x| x.as_str()).unwrap_or("")).unwrap_or_default();
                    match key.decrypt(rsa::Oaep::new::<sha2::Sha256>(), &enc) {
                        Ok(nonce) => {
                            use sha2::Digest;
                            let proof = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(&nonce));
                            let msg = format!("{{\"op\":\"nonce_proof\",\"proof\":\"{proof}\"}}");
                            w.outq.push_back(frame(1, msg.as_bytes()));
                            out.push_str("sent nonce_proof\n");
                        },
                        Err(_) => out.push_str("nonce decrypt FAILED\n"),
                    }
                },
                "pending_remote_init" => {
                    out.push_str("QR fingerprint received: OK\n");
                    done = true;
                },
                _ => {},
            }
        }
        ws_service(w);
        if done || w.state != "open" {
            break;
        }
    }
    let state = ws_get(id).map(|w| w.state.clone()).unwrap_or_default();
    ws_close(id);
    out.push_str(&format!("state at the end: {state}\n"));
    out
}
