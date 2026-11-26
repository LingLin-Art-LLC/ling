//! Spotify Connect zeroconf, as librespot-discovery does it: an mDNS
//! responder for `_spotify-connect._tcp.local` (PTR / SRV / TXT / A) and the
//! small HTTP endpoint the Spotify app talks to -- `getInfo` (our DH public
//! key) and `addUser` (the sign-in blob, encrypted to the DH shared secret,
//! then the inner blob encrypted to this device id + the username).

use super::ap::{Dh, CLIENT_ID};
use crate::drivers::netstack as ns;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use base64::Engine as _;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    use hmac::{Hmac, Mac};
    let mut m = <Hmac<sha1::Sha1> as Mac>::new_from_slice(key).expect("hmac key");
    m.update(data);
    m.finalize().into_bytes().into()
}

fn pbkdf2_sha1_20(pass: &[u8], salt: &[u8], rounds: u32) -> [u8; 20] {
    let mut s = Vec::from(salt);
    s.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha1(pass, &s);
    let mut t = u;
    for _ in 1..rounds {
        u = hmac_sha1(pass, &u);
        for i in 0..20 {
            t[i] ^= u[i];
        }
    }
    t
}

/// AES-128-CTR (big-endian 128-bit counter), in place.
pub fn aes128_ctr(key: &[u8], iv: &[u8; 16], data: &mut [u8]) {
    aes128_ctr_at(key, iv, 0, data)
}

/// AES-128-CTR starting `block` blocks into the stream.
pub fn aes128_ctr_at(key: &[u8], iv: &[u8; 16], block: u64, data: &mut [u8]) {
    use aes::cipher::{BlockEncrypt, KeyInit};
    let c = aes::Aes128::new(key.into());
    let mut ctr = u128::from_be_bytes(*iv).wrapping_add(block as u128);
    for chunk in data.chunks_mut(16) {
        let mut b = aes::Block::from(ctr.to_be_bytes());
        c.encrypt_block(&mut b);
        for (x, k) in chunk.iter_mut().zip(b.iter()) {
            *x ^= k;
        }
        ctr = ctr.wrapping_add(1);
    }
}

fn blob_key(username: &str, device_id: &str) -> [u8; 24] {
    use sha1::Digest;
    let secret = sha1::Sha1::digest(device_id.as_bytes());
    let k20 = pbkdf2_sha1_20(&secret, username.as_bytes(), 0x100);
    let h = sha1::Sha1::digest(k20);
    let mut key = [0u8; 24];
    key[..20].copy_from_slice(&h);
    key[20..].copy_from_slice(&20u32.to_be_bytes());
    key
}

/// librespot's Credentials::with_blob: (auth type, auth data) out of the inner blob (base64).
pub fn decode_inner_blob(username: &str, blob_b64: &[u8], device_id: &str) -> Result<(u32, Vec<u8>), String> {
    use aes::cipher::{BlockDecrypt, KeyInit};
    let key = blob_key(username, device_id);
    let mut data = b64().decode(blob_b64.iter().copied().filter(|c| !c.is_ascii_whitespace()).collect::<Vec<u8>>()).map_err(|_| String::from("the blob is not base64"))?;
    if data.len() < 16 {
        return Err(String::from("the blob is too short"));
    }
    let c = aes::Aes192::new((&key[..]).into());
    for ch in data.chunks_exact_mut(16) {
        c.decrypt_block(aes::Block::from_mut_slice(ch));
    }
    let l = data.len();
    for i in 0..l - 0x10 {
        data[l - i - 1] ^= data[l - i - 0x11];
    }
    let mut at = 0usize;
    let u8_ = |at: &mut usize| -> Result<u8, String> {
        let v = *data.get(*at).ok_or("the blob ended early")?;
        *at += 1;
        Ok(v)
    };
    let int = |at: &mut usize| -> Result<u32, String> {
        let lo = u8_(at)? as u32;
        if lo & 0x80 == 0 {
            return Ok(lo);
        }
        let hi = u8_(at)? as u32;
        Ok((lo & 0x7f) | (hi << 7))
    };
    u8_(&mut at)?;
    let n = int(&mut at)? as usize;
    at += n;
    u8_(&mut at)?;
    let typ = int(&mut at)?;
    u8_(&mut at)?;
    let n = int(&mut at)? as usize;
    let auth = data.get(at..at + n).ok_or("the blob's auth data is cut short")?.to_vec();
    Ok((typ, auth))
}

/// The phone's side of the inner blob (for the self-check only).
fn encode_inner_blob(username: &str, typ: u32, auth: &[u8], device_id: &str) -> Vec<u8> {
    use aes::cipher::{BlockEncrypt, KeyInit};
    let put_int = |v: &mut Vec<u8>, x: u32| {
        if x < 0x80 {
            v.push(x as u8);
        } else {
            v.push((x & 0x7f) as u8 | 0x80);
            v.push((x >> 7) as u8);
        }
    };
    let mut d = Vec::new();
    d.push(0x49);
    put_int(&mut d, username.len() as u32);
    d.extend_from_slice(username.as_bytes());
    d.push(0x50);
    put_int(&mut d, typ);
    d.push(0x51);
    put_int(&mut d, auth.len() as u32);
    d.extend_from_slice(auth);
    while d.len() % 16 != 0 {
        d.push(0);
    }
    let l = d.len();
    // inverse of the decoder's chaining: forward from the front
    for i in (0..l - 0x10).rev() {
        d[l - i - 1] ^= d[l - i - 0x11];
    }
    let key = blob_key(username, device_id);
    let c = aes::Aes192::new((&key[..]).into());
    for ch in d.chunks_exact_mut(16) {
        c.encrypt_block(aes::Block::from_mut_slice(ch));
    }
    b64().encode(d).into_bytes()
}

/// addUser's outer layer: (username, auth type, auth data).
pub fn decrypt_add_user(dh: &Dh, device_id: &str, username: &str, blob_b64: &str, client_key_b64: &str) -> Result<(u32, Vec<u8>), String> {
    use sha1::Digest;
    let blob = b64().decode(blob_b64.trim()).map_err(|_| String::from("blob is not base64"))?;
    let ck = b64().decode(client_key_b64.trim()).map_err(|_| String::from("clientKey is not base64"))?;
    if blob.len() < 36 {
        return Err(String::from("the blob is too short"));
    }
    let shared = dh.shared(&ck);
    let base = sha1::Sha1::digest(&shared);
    let base = &base[..16];
    let ck_key = hmac_sha1(base, b"checksum");
    let enc_key = hmac_sha1(base, b"encryption");
    let n = blob.len();
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&blob[..16]);
    let enc = &blob[16..n - 20];
    if hmac_sha1(&ck_key, enc)[..] != blob[n - 20..] {
        return Err(String::from("MAC"));
    }
    let mut inner = enc.to_vec();
    aes128_ctr(&enc_key[..16], &iv, &mut inner);
    decode_inner_blob(username, &inner, device_id)
}

/// The phone's side of addUser (self-check): (blob b64, clientKey b64).
fn encrypt_add_user(device_pub: &[u8], device_id: &str, username: &str, typ: u32, auth: &[u8]) -> (String, String) {
    use sha1::Digest;
    let phone = Dh::random();
    let shared = phone.shared(device_pub);
    let base = sha1::Sha1::digest(&shared);
    let base = &base[..16];
    let ck_key = hmac_sha1(base, b"checksum");
    let enc_key = hmac_sha1(base, b"encryption");
    let mut iv = [0u8; 16];
    crate::crypto::random_bytes(&mut iv);
    let mut inner = encode_inner_blob(username, typ, auth, device_id);
    aes128_ctr(&enc_key[..16], &iv, &mut inner);
    let mut blob = iv.to_vec();
    blob.extend_from_slice(&inner);
    blob.extend_from_slice(&hmac_sha1(&ck_key, &inner));
    (b64().encode(blob), b64().encode(&phone.public))
}

/// Both layers of addUser, phone side then ours, plus AES-128-CTR / PBKDF2
/// known answers (RFC 6070 for PBKDF2-HMAC-SHA1, NIST SP800-38A F.5.1 for CTR).
pub fn selftest() -> Result<(), String> {
    let p = pbkdf2_sha1_20(b"password", b"salt", 2);
    if super::hex(&p) != "ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957" {
        return Err(format!("pbkdf2: {}", super::hex(&p)));
    }
    let key = super::shannon::hex_to("2b7e151628aed2a6abf7158809cf4f3c");
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&super::shannon::hex_to("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff"));
    let mut d = super::shannon::hex_to("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51");
    aes128_ctr(&key, &iv, &mut d);
    if super::hex(&d) != "874d6191b620e3261bef6864990db6ce9806f66b7970fdff8617187bb9fffdff" {
        return Err(format!("aes-ctr: {}", super::hex(&d)));
    }
    let dev = Dh::random();
    let did = "0123456789abcdef0123456789abcdef01234567";
    let auth: Vec<u8> = (0..200u32).map(|i| (i * 7) as u8).collect();
    let (blob, ck) = encrypt_add_user(&dev.public, did, "someone", 1, &auth);
    let (t, a) = decrypt_add_user(&dev, did, "someone", &blob, &ck)?;
    if t != 1 || a != auth {
        return Err(String::from("addUser round trip gave other credentials"));
    }
    Ok(())
}

// ── the HTTP endpoint ──

pub const HTTP_PORT: u16 = 45454;

pub struct Server {
    pub dh: Dh,
    pub device_id: String,
    pub name: String,
    pub active_user: String,
    conns: Vec<(usize, Vec<u8>, u64)>,
    announced_at: u64,
    announce_left: u32,
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(core::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    out.push(v);
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

fn params(q: &str, into: &mut Vec<(String, String)>) {
    for kv in q.split('&') {
        if kv.is_empty() {
            continue;
        }
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        into.push((url_decode(k), url_decode(v)));
    }
}

/// What addUser handed over.
pub struct Creds {
    pub username: String,
    pub auth_type: u32,
    pub auth_data: Vec<u8>,
}

impl Server {
    pub fn new(device_id: &str, name: &str) -> Server {
        mdns_start();
        Server { dh: Dh::random(), device_id: String::from(device_id), name: String::from(name), active_user: String::new(), conns: Vec::new(), announced_at: 0, announce_left: 3 }
    }

    pub fn stop(&mut self) {
        ns::tcp_unlisten(HTTP_PORT);
        for (h, _, _) in self.conns.drain(..) {
            ns::tcp_close_h(h);
            ns::tcp_free_h(h);
        }
        // say goodbye: TTL 0
        mdns_send(&answer(&self.name, 0));
        mdns_stop();
    }

    fn get_info(&self) -> String {
        serde_json::json!({
            "status": 101, "statusString": "OK", "spotifyError": 0, "version": "2.9.0",
            "deviceID": self.device_id, "deviceType": "computer", "remoteName": self.name,
            "publicKey": b64().encode(&self.dh.public), "brandDisplayName": "LingOS", "modelDisplayName": "Horizon",
            "libraryVersion": "0.8.0", "resolverVersion": "1", "groupStatus": "NONE", "tokenType": "default",
            "clientID": CLIENT_ID, "productID": 0, "scope": "streaming", "availability": "",
            "supported_drm_media_formats": [], "supported_capabilities": 1, "accountReq": "PREMIUM",
            "activeUser": self.active_user, "aliases": [],
        })
        .to_string()
    }

    /// (status line, body, credentials when addUser succeeded)
    fn handle(&mut self, req: &[u8]) -> (&'static str, String, Option<Creds>) {
        let he = req.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(req.len());
        let head = String::from_utf8_lossy(&req[..he]).into_owned();
        let body = if he + 4 <= req.len() { String::from_utf8_lossy(&req[he + 4..]).into_owned() } else { String::new() };
        let first = head.lines().next().unwrap_or("");
        let mut it = first.split(' ');
        let method = it.next().unwrap_or("");
        let target = it.next().unwrap_or("/");
        let mut ps = Vec::new();
        if let Some((_, q)) = target.split_once('?') {
            params(q, &mut ps);
        }
        params(&body, &mut ps);
        let get = |k: &str| ps.iter().find(|p| p.0 == k).map(|p| p.1.clone());
        match (method, get("action").as_deref()) {
            ("GET", Some("getInfo")) => ("200 OK", self.get_info(), None),
            ("POST", Some("addUser")) => {
                let (Some(user), Some(blob), Some(ck)) = (get("userName"), get("blob"), get("clientKey")) else {
                    return ("400 Bad Request", String::from("{\"status\":102,\"spotifyError\":1,\"statusString\":\"ERROR-MISSING\"}"), None);
                };
                match decrypt_add_user(&self.dh, &self.device_id, &user, &blob, &ck) {
                    Ok((t, a)) => {
                        self.active_user = user.clone();
                        (
                            "200 OK",
                            String::from("{\"status\":101,\"spotifyError\":0,\"statusString\":\"OK\"}"),
                            Some(Creds { username: user, auth_type: t, auth_data: a }),
                        )
                    },
                    Err(e) => {
                        super::log(&format!("zeroconf addUser from {user}: {e}"));
                        ("200 OK", String::from("{\"status\":102,\"spotifyError\":1,\"statusString\":\"ERROR-MAC\"}"), None)
                    },
                }
            },
            _ => ("404 Not Found", String::new(), None),
        }
    }

    /// One pass: mDNS queries answered, new HTTP connections taken, requests
    /// answered. Returns credentials once a phone has handed them over.
    pub fn tick(&mut self) -> Option<Creds> {
        let now = crate::arch::timer::now_ms();
        // announcements: three at the start, a second apart, then one a minute
        if self.announce_left > 0 && now.saturating_sub(self.announced_at) > 1000 || now.saturating_sub(self.announced_at) > 60_000 {
            self.announced_at = now;
            self.announce_left = self.announce_left.saturating_sub(1);
            if !ns::net_busy_for_me() {
                let _g = ns::NetShare::new();
                mdns_send(&answer(&self.name, 120));
            }
        }
        if ns::net_busy_for_me() {
            return None;
        }
        if let Some(h) = ns::tcp_listen_accept(HTTP_PORT, 0) {
            self.conns.push((h, Vec::new(), now));
        }
        while let Some((src, sport, q)) = mdns_take() {
            if let Some(name) = query_wants(&q, &self.name) {
                let _ = name;
                let a = answer(&self.name, 120);
                let _g = ns::NetShare::new();
                if sport != 5353 {
                    // a one-shot (legacy) query: answer it directly
                    let _ = ns::udp_send_to(src, sport, 5353, &with_id(&a, &q));
                } else {
                    mdns_send(&a);
                }
            }
        }
        let mut out = None;
        let mut k = 0;
        while k < self.conns.len() {
            let (h, ref mut buf, born) = self.conns[k];
            let mut tmp = [0u8; 4096];
            let n = ns::tcp_read_h(h, &mut tmp, 0);
            buf.extend_from_slice(&tmp[..n]);
            let complete = match buf.windows(4).position(|w| w == b"\r\n\r\n") {
                Some(he) => {
                    let head = String::from_utf8_lossy(&buf[..he]).to_ascii_lowercase();
                    let cl = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                    buf.len() >= he + 4 + cl
                },
                None => false,
            };
            if complete {
                let req = core::mem::take(buf);
                let (status, body, creds) = self.handle(&req);
                let resp = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = crate::tls::Io::H(h).write(resp.as_bytes());
                ns::tcp_close_h(h);
                ns::tcp_free_h(h);
                self.conns.remove(k);
                if creds.is_some() {
                    out = creds;
                }
                continue;
            }
            if ns::tcp_closed_h(h) || now.saturating_sub(born) > 10_000 {
                ns::tcp_close_h(h);
                ns::tcp_free_h(h);
                self.conns.remove(k);
                continue;
            }
            k += 1;
        }
        out
    }

    /// Answer one request as if it came over the network (the `spotify zc` check).
    pub fn handle_local(&mut self, req: &str) -> (String, Option<Creds>) {
        let (s, b, c) = self.handle(req.as_bytes());
        (format!("{s} {b}"), c)
    }
}

/// The self-check's phone: addUser's fields for this server.
pub fn phone_add_user(srv: &Server, username: &str, typ: u32, auth: &[u8]) -> String {
    let (blob, ck) = encrypt_add_user(&srv.dh.public, &srv.device_id, username, typ, auth);
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|c| if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'.' { String::from(c as char) } else { format!("%{c:02X}") })
            .collect()
    };
    format!("POST /?action=addUser HTTP/1.1\r\nContent-Type: application/x-www-form-urlencoded\r\n\r\naction=addUser&userName={}&blob={}&clientKey={}", enc(username), enc(&blob), enc(&ck))
}

// ── mDNS ──

static mut MDNS_Q: VecDeque<([u8; 4], u16, Vec<u8>)> = VecDeque::new();

fn mdns_q() -> &'static mut VecDeque<([u8; 4], u16, Vec<u8>)> {
    unsafe { &mut *(&raw mut MDNS_Q) }
}

fn on_mdns(src: [u8; 4], sport: u16, payload: &[u8]) {
    let q = mdns_q();
    if q.len() < 32 && payload.len() >= 12 && payload[2] & 0x80 == 0 {
        // queries only (QR bit clear)
        q.push_back((src, sport, payload.to_vec()));
    }
}

fn mdns_start() {
    mdns_q().clear();
    ns::udp_tap(5353, Some(on_mdns));
}

fn mdns_stop() {
    ns::udp_tap(5353, None);
    mdns_q().clear();
}

fn mdns_take() -> Option<([u8; 4], u16, Vec<u8>)> {
    mdns_q().pop_front()
}

fn mdns_send(p: &[u8]) {
    let _ = ns::udp_multicast([224, 0, 0, 251], 5353, 5353, p);
}

const SERVICE: &str = "_spotify-connect._tcp.local";

fn host_name() -> String {
    let ip = ns::self_ip();
    format!("lingos-{}-{}-{}-{}.local", ip[0], ip[1], ip[2], ip[3])
}

fn put_name(out: &mut Vec<u8>, name: &str) {
    for label in name.split('.') {
        if label.is_empty() {
            continue;
        }
        let l = label.as_bytes();
        out.push(l.len().min(63) as u8);
        out.extend_from_slice(&l[..l.len().min(63)]);
    }
    out.push(0);
}

/// A name at `at` (with compression pointers) as dotted text, and the offset after it.
fn read_name(p: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut end = None;
    let mut hops = 0;
    loop {
        let l = *p.get(at)? as usize;
        if l == 0 {
            at += 1;
            break;
        }
        if l & 0xc0 == 0xc0 {
            let ptr = ((l & 0x3f) << 8) | *p.get(at + 1)? as usize;
            if end.is_none() {
                end = Some(at + 2);
            }
            at = ptr;
            hops += 1;
            if hops > 16 {
                return None;
            }
            continue;
        }
        let s = p.get(at + 1..at + 1 + l)?;
        if !out.is_empty() {
            out.push('.');
        }
        out.push_str(&String::from_utf8_lossy(s));
        at += 1 + l;
    }
    Some((out, end.unwrap_or(at)))
}

/// Does this query ask about us? (the service, our instance or our host name)
fn query_wants(p: &[u8], name: &str) -> Option<String> {
    let qd = u16::from_be_bytes([p[4], p[5]]) as usize;
    let inst = format!("{name}.{SERVICE}");
    let host = host_name();
    let mut at = 12;
    for _ in 0..qd.min(16) {
        let (n, next) = read_name(p, at)?;
        at = next + 4;
        if n.eq_ignore_ascii_case(SERVICE) || n.eq_ignore_ascii_case(&inst) || n.eq_ignore_ascii_case(&host) || n.eq_ignore_ascii_case("_services._dns-sd._udp.local") {
            return Some(n);
        }
    }
    None
}

fn rr(out: &mut Vec<u8>, name: &str, typ: u16, flush: bool, ttl: u32, data: &[u8]) {
    put_name(out, name);
    out.extend_from_slice(&typ.to_be_bytes());
    out.extend_from_slice(&(if flush { 0x8001u16 } else { 1u16 }).to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
}

/// An mDNS response: PTR (service -> instance), SRV, TXT, A.
pub fn answer(name: &str, ttl: u32) -> Vec<u8> {
    let inst = format!("{name}.{SERVICE}");
    let host = host_name();
    let mut out = Vec::with_capacity(400);
    out.extend_from_slice(&[0, 0, 0x84, 0, 0, 0, 0, 4, 0, 0, 0, 0]);
    let mut d = Vec::new();
    put_name(&mut d, &inst);
    rr(&mut out, SERVICE, 12, false, ttl, &d);
    let mut srv = Vec::new();
    srv.extend_from_slice(&[0, 0, 0, 0]);
    srv.extend_from_slice(&HTTP_PORT.to_be_bytes());
    put_name(&mut srv, &host);
    rr(&mut out, &inst, 33, true, ttl, &srv);
    let mut txt = Vec::new();
    for t in ["VERSION=1.0", "CPath=/", "Stack=SP"] {
        txt.push(t.len() as u8);
        txt.extend_from_slice(t.as_bytes());
    }
    rr(&mut out, &inst, 16, true, ttl, &txt);
    rr(&mut out, &host, 1, true, ttl, &ns::self_ip());
    out
}

/// A legacy unicast answer echoes the query's id and question.
fn with_id(a: &[u8], q: &[u8]) -> Vec<u8> {
    let mut v = a.to_vec();
    v[0] = q[0];
    v[1] = q[1];
    v
}

/// The `spotify mdns` check: an answer parsed back by the query reader.
pub fn mdns_selftest(name: &str) -> Result<usize, String> {
    let a = answer(name, 120);
    if a.len() > 512 {
        return Err(format!("the answer is {} bytes (the netstack sends up to 512)", a.len()));
    }
    let (n, _) = read_name(&a, 12).ok_or("unreadable answer")?;
    if n != SERVICE {
        return Err(format!("first name {n}"));
    }
    let mut q = alloc::vec![0u8, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    put_name(&mut q, SERVICE);
    q.extend_from_slice(&[0, 12, 0, 1]);
    query_wants(&q, name).ok_or("our own query was not recognised")?;
    Ok(a.len())
}
