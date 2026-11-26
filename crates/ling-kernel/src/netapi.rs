//! Networking for programs: the host's `net_*` API (a 2-peer / small-lobby TCP
//! hub with newline-delimited messages, plus UDP-broadcast lobby discovery)
//! and `http_get` / `http_post_async` / `http_job_poll`, over the kernel's
//! netstack (TCP sockets, UDP, DNS, TLS). The semantics follow
//! `ling/src/runtime/net.rs`: `net_status` is 0 off, 2 up (hosting or joined),
//! 1 hosting with no client left; `net_recv_from` gives "id|msg";
//! `net_discover` gives "ip|info" lines seen in the last 5 s.
//!
//! `http_post_async` / `http_get_async` run the request on a green thread
//! (coro.rs): the id returns at once, the app polls `http_job_poll`, and the
//! coroutine-aware network waits (`timer::poll_until` + the net lock) hand the
//! CPU back to the UI between segments. If no stack can be spawned they fall
//! back to the old synchronous call. (The Pi 5's wired port is
//! `arch::rp1_eth`; with no driver everything reports "not connected".)

use crate::lingrt::{mk_str, num, to_f64, to_string, UNIT};
use alloc::string::String;

mod imp {
    use crate::drivers::netstack as ns;
    use alloc::collections::VecDeque;
    use alloc::format;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    const MAX_PEERS: usize = 8;
    /// TLS records are built in a 4 KiB buffer: request plaintext per record.
    #[allow(dead_code)]
    const TLS_CHUNK: usize = 3800;

    struct Peer {
        handle: usize,
        id: u64,
        inbuf: Vec<u8>,
    }

    enum Role {
        Idle,
        Host { port: u16, next_id: u64 },
        Client,
    }

    struct Net {
        role: Role,
        peers: Vec<Peer>,
        inbound: VecDeque<(u64, String)>,
        events: VecDeque<String>,
        status: u8,
        /// (port, info, last broadcast ms) while announcing.
        announce: Option<(u16, String, u64)>,
        discover_port: Option<u16>,
        /// (ip, info, last seen ms)
        found: Vec<([u8; 4], String, u64)>,
        /// (id, text for job_poll, status, header block): each job keeps its own
        /// answer, so several can run at once and `http_header` still describes
        /// the one the app just took.
        jobs: Vec<(String, String, u16, String)>,
        next_job: u64,
        last_status: u16,
        /// Header block of the last response (`http_header`).
        last_headers: String,
    }

    static mut NET: Net = Net {
        role: Role::Idle,
        peers: Vec::new(),
        inbound: VecDeque::new(),
        events: VecDeque::new(),
        status: 0,
        announce: None,
        discover_port: None,
        found: Vec::new(),
        jobs: Vec::new(),
        next_job: 1,
        last_status: 0,
        last_headers: String::new(),
    };

    fn net() -> &'static mut Net {
        unsafe { &mut *(&raw mut NET) }
    }

    fn now_ms() -> u64 {
        crate::arch::timer::now_ms()
    }

    fn parse_ip(s: &str) -> Option<[u8; 4]> {
        let mut ip = [0u8; 4];
        let mut n = 0;
        for part in s.trim().split('.') {
            if n >= 4 {
                return None;
            }
            ip[n] = part.parse::<u8>().ok()?;
            n += 1;
        }
        if n == 4 {
            Some(ip)
        } else {
            None
        }
    }

    fn ip_str(ip: [u8; 4]) -> String {
        format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
    }

    /// The app whose net_* lobby this is (appres.rs; 0 = none / the desktop).
    static mut LOBBY_UID: u32 = 0;
    /// (job id, owner) of every HTTP job not taken yet.
    static mut JOB_OWNERS: Vec<(String, u32)> = Vec::new();

    fn job_owned(id: &str) {
        let v = unsafe { &mut *(&raw mut JOB_OWNERS) };
        v.push((String::from(id), crate::appres::owner()));
    }

    /// App `uid` ended: its lobby closes (peers, announce, discovery) and the
    /// answers of its HTTP jobs nobody will poll now go.
    pub fn app_ended(uid: u32, f: &mut crate::appres::Freed) {
        let n = net();
        if unsafe { LOBBY_UID } == uid {
            f.sockets += n.peers.len() as u32;
            close();
            n.events.clear();
            n.announce = None;
            n.discover_port = None;
            n.found.clear();
            unsafe { LOBBY_UID = 0 };
        }
        let owners = unsafe { &mut *(&raw mut JOB_OWNERS) };
        let mine: Vec<String> = owners.iter().filter(|o| o.1 == uid).map(|o| o.0.clone()).collect();
        owners.retain(|o| o.1 != uid);
        let before = n.jobs.len();
        n.jobs.retain(|j| !mine.contains(&j.0));
        f.jobs += (mine.len().max(before - n.jobs.len())) as u32;
    }

    fn lobby_owned() {
        unsafe { LOBBY_UID = crate::appres::owner() };
    }

    pub fn host(port: u16) {
        close();
        lobby_owned();
        let n = net();
        n.role = Role::Host { port, next_id: 1 };
        n.status = 2;
        pump();
    }

    pub fn join(ip: &str, port: u16) {
        close();
        lobby_owned();
        let Some(addr) = parse_ip(ip).or_else(|| ns::dns_resolve(ip)) else {
            crate::console_write(format!("net_join {ip}:{port}: no such host\n").as_bytes());
            return;
        };
        match ns::tcp_connect_h(addr, port, 3_000_000) {
            Some(h) => {
                let n = net();
                n.role = Role::Client;
                n.peers.push(Peer { handle: h, id: 0, inbuf: Vec::new() });
                n.status = 2;
                crate::console_write(format!("net_join {ip}:{port} ({}): connected\n", ip_str(addr)).as_bytes());
            },
            None => {
                net().status = 0;
                crate::console_write(format!("net_join {ip}:{port} ({}): no answer\n", ip_str(addr)).as_bytes());
            },
        }
    }

    /// Accept new clients (host), read lines from every peer, drop closed
    /// peers, broadcast the lobby beacon, collect discovered hosts.
    pub fn pump() {
        let n = net();
        if let Role::Host { port, next_id } = &mut n.role {
            if n.peers.len() < MAX_PEERS {
                if let Some(h) = ns::tcp_listen_accept(*port, 1_000) {
                    let id = *next_id;
                    *next_id += 1;
                    n.peers.push(Peer { handle: h, id, inbuf: Vec::new() });
                    n.events.push_back(format!("join {id}"));
                    n.status = 2;
                }
            }
        } else {
            ns::pump_once();
        }
        let mut buf = [0u8; 2048];
        let mut i = 0;
        while i < n.peers.len() {
            let got = ns::tcp_read_h(n.peers[i].handle, &mut buf, 500);
            if got > 0 {
                n.peers[i].inbuf.extend_from_slice(&buf[..got]);
                while let Some(pos) = n.peers[i].inbuf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = n.peers[i].inbuf.drain(..=pos).collect();
                    let text = String::from_utf8_lossy(&line[..line.len() - 1]).trim_end_matches('\r').to_string();
                    let id = n.peers[i].id;
                    netlog(&format!("net_recv line ({} chars)", text.len()));
                    n.inbound.push_back((id, text));
                }
            } else if !ns::tcp_established_h(n.peers[i].handle) {
                let p = n.peers.remove(i);
                ns::tcp_close_h(p.handle);
                n.events.push_back(format!("leave {}", p.id));
                match n.role {
                    Role::Host { .. } => {
                        if n.peers.is_empty() {
                            n.status = 1;
                        }
                    },
                    Role::Client => {
                        n.status = 0;
                        n.role = Role::Idle;
                    },
                    Role::Idle => {},
                }
                continue;
            }
            i += 1;
        }
        if let Some((port, info, last)) = &mut n.announce {
            if now_ms().saturating_sub(*last) >= 1000 {
                ns::udp_broadcast(*port, *port, info.as_bytes());
                *last = now_ms();
            }
        }
        if n.discover_port.is_some() {
            let mut b = [0u8; 512];
            if let Some((ip, len)) = ns::udp_recv_beacon(&mut b) {
                let info = String::from_utf8_lossy(&b[..len]).replace(['\n', '\r'], " ");
                let now = now_ms();
                match n.found.iter_mut().find(|e| e.0 == ip) {
                    Some(e) => {
                        e.1 = info;
                        e.2 = now;
                    },
                    None => n.found.push((ip, info, now)),
                }
            }
        }
    }

    pub fn send(s: &str) {
        let mut line = s.as_bytes().to_vec();
        line.push(b'\n');
        for p in &net().peers {
            let ok = ns::tcp_write_h(p.handle, &line);
            netlog(&format!("net_send {} bytes -> {}", line.len(), if ok { "sent" } else { "FAILED" }));
        }
    }

    /// The first few net_* events on the serial log (diagnosing a game's login).
    fn netlog(msg: &str) {
        static mut N: u32 = 0;
        unsafe {
            if N < 24 {
                N += 1;
                crate::console_write(msg.as_bytes());
                crate::console_write(b"\n");
            }
        }
    }

    pub fn send_to(id: u64, s: &str) {
        let mut line = s.as_bytes().to_vec();
        line.push(b'\n');
        if let Some(p) = net().peers.iter().find(|p| p.id == id) {
            ns::tcp_write_h(p.handle, &line);
        }
    }

    pub fn recv() -> String {
        pump();
        net().inbound.pop_front().map(|(_, m)| m).unwrap_or_default()
    }

    pub fn recv_from() -> String {
        pump();
        net().inbound.pop_front().map(|(id, m)| format!("{id}|{m}")).unwrap_or_default()
    }

    pub fn status() -> u8 {
        pump();
        net().status
    }

    pub fn close() {
        let n = net();
        for p in n.peers.drain(..) {
            ns::tcp_close_h(p.handle);
        }
        n.role = Role::Idle;
        n.status = 0;
        n.inbound.clear();
    }

    pub fn clients() -> String {
        let ids: Vec<String> = net().peers.iter().map(|p| p.id.to_string()).collect();
        ids.join(",")
    }

    pub fn client_count() -> usize {
        net().peers.len()
    }

    pub fn events() -> String {
        pump();
        let ev: Vec<String> = net().events.drain(..).collect();
        ev.join("\n")
    }

    pub fn announce(port: u16, info: &str) {
        lobby_owned();
        net().announce = Some((port, String::from(info), 0));
        pump();
    }

    pub fn announce_stop() {
        net().announce = None;
    }

    pub fn discover(port: u16) -> String {
        let n = net();
        if n.discover_port != Some(port) {
            lobby_owned();
            ns::udp_listen(port);
            n.discover_port = Some(port);
            n.found.clear();
        }
        pump();
        let now = now_ms();
        let mut out = String::new();
        for (ip, info, t) in &net().found {
            if now.saturating_sub(*t) < 5000 {
                out.push_str(&ip_str(*ip));
                out.push('|');
                out.push_str(info);
                out.push('\n');
            }
        }
        out
    }

    pub fn test_bind(port: u16) -> String {
        format!(
            "TCP bind 0.0.0.0:{port}: OK (host can listen)\nLAN IP (give to same-network joiners): {}\n",
            ip_str(ns::self_ip())
        )
    }

    // ── HTTP ─────────────────────────────────────────────────────────────────

    // 4 MiB: whole modern pages (a Wikipedia article is 0.3-1 MB); 256 KiB cut them off


    #[allow(dead_code)]
    fn parse_status(resp: &[u8]) -> u16 {
        // "HTTP/1.x NNN"
        if resp.len() < 12 || !resp.starts_with(b"HTTP/") {
            return 0;
        }
        let s = &resp[9..12];
        if s.iter().all(|b| b.is_ascii_digit()) {
            (s[0] - b'0') as u16 * 100 + (s[1] - b'0') as u16 * 10 + (s[2] - b'0') as u16
        } else {
            0
        }
    }

    /// Plain HTTP/1.0 request over a socket handle; the whole response lands
    /// in `out`.
    #[allow(dead_code)]
    fn plain_request(host: &str, port: u16, method: &str, path: &str, content_type: &str, extra: &str, body: &[u8], out: &mut [u8]) -> usize {
        let Some(ip) = parse_ip(host).or_else(|| ns::dns_resolve(host)) else { return 0 };
        let Some(h) = ns::tcp_connect_h(ip, port, 5_000_000) else { return 0 };
        let mut req = format!("{method} {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: LingOS\r\nConnection: close\r\n{extra}").into_bytes();
        if !body.is_empty() {
            req.extend_from_slice(format!("Content-Type: {content_type}\r\nContent-Length: {}\r\n", body.len()).as_bytes());
        }
        req.extend_from_slice(b"\r\n");
        req.extend_from_slice(body);
        let mut total = 0usize;
        if ns::tcp_write_h(h, &req) {
            while total < out.len() {
                let got = ns::tcp_read_h(h, &mut out[total..], 3_000_000);
                if got == 0 {
                    break;
                }
                total += got;
            }
        }
        ns::tcp_close_h(h);
        total
    }

    /// The biggest page body handed to an app (a Wikipedia article is 0.3-1 MB).
    const PAGE_MAX: usize = 8 * 1024 * 1024;

    /// `(status, header block, body)` of an HTTP(S) request; status 0 = no
    /// response at all. Runs on LingSockets (`lingsock::request_full`): a
    /// connection of its own (or a kept-alive one to the same host), a SHARED
    /// net claim, gzip asked for -- so a page's stylesheets, scripts and
    /// pictures load side by side instead of one after another.
    fn fetch(method: &str, url: &str, content_type: &str, body: &str) -> (u16, String, String) {
        fetch_h(method, url, content_type, body, "")
    }

    /// What every browser sends with a top-level page navigation. Cloudflare's
    /// managed challenge (claude.ai and many other sites) answers a page GET
    /// without them with 403 + `cf-mitigated: challenge` ("Just a moment...");
    /// with them the real page comes back -- verified against claude.ai/login
    /// with this kernel's exact TLS ClientHello (Upgrade-Insecure-Requests is
    /// the one that decides it).
    pub const NAV_HEADERS: &str = "Accept: text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8\nAccept-Language: en-US,en;q=0.9\nUpgrade-Insecure-Requests: 1\n";

    /// `fetch` with `extra` request headers ("Name: value" lines).
    fn fetch_h(method: &str, url: &str, content_type: &str, body: &str, extra: &str) -> (u16, String, String) {
        let Some((host, _port, path, https)) = ns::parse_url(url) else { return (0, String::new(), String::new()) };
        let mut headers = String::new();
        // the first-party cookies for this request (cookies.rs); nothing for another site
        // (www.youtube.com pages also get the user's YouTube sign-in cookies, ytauth.rs)
        if let Some(c) = crate::drivers::ytauth::merge_cookie_header(crate::cookies::header_for(host, path, https), host, path, https) {
            headers.push_str(&format!("Cookie: {c}\n"));
        }
        if !body.is_empty() || method == "POST" {
            headers.push_str(&format!("Content-Type: {}\n", if content_type.is_empty() { "application/x-www-form-urlencoded" } else { content_type }));
        }
        headers.push_str("User-Agent: LingOS\n");
        // (after the User-Agent: the order the challenge-free probe used)
        headers.push_str(extra);
        if !extra.is_empty() && !extra.ends_with('\n') {
            headers.push('\n');
        }
        // The first connect after boot sometimes fails (ARP/DNS warm-up): one retry.
        let mut last_err = String::new();
        for attempt in 0..2 {
            match crate::lingsock::request_full(method, url, &headers, body.as_bytes(), PAGE_MAX) {
                Ok((status, head, b)) => {
                    crate::cookies::store_from_response(host, path, https, &head);
                    crate::drivers::ytauth::absorb(host, &head);
                    return (status, head, String::from_utf8_lossy(&b).into_owned());
                },
                Err(e) => {
                    crate::console_write(format!("[http] {e}{}\n", if attempt == 0 { " (retrying)" } else { "" }).as_bytes());
                    last_err = e;
                },
            }
        }
        let _ = last_err;
        (0, String::new(), String::new())
    }

    /// `(status, body)` of an HTTP(S) request (synchronous callers); sets what
    /// `http_get_status` / `http_header` report.
    pub fn request(method: &str, url: &str, content_type: &str, body: &str) -> (u16, String) {
        let (status, head, b) = fetch(method, url, content_type, body);
        net().last_status = status;
        net().last_headers = head;
        (status, b)
    }

    pub fn last_status() -> u16 {
        net().last_status
    }

    /// A header of the last response (case-insensitive name), "" if absent.
    pub fn last_header(name: &str) -> String {
        let want = name.trim().to_ascii_lowercase();
        for line in net().last_headers.lines() {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().to_ascii_lowercase() == want {
                    return String::from(v.trim());
                }
            }
        }
        String::new()
    }

    /// A finished job's text for `job_poll`. An empty body (a bare redirect, a
    /// 204) must not come back as "" -- that is what `job_poll` returns while
    /// the job is still running, so the app would wait for it forever.
    fn job_result(status: u16, out: String, url: &str) -> String {
        if status == 0 {
            format!("ERROR: no response from {url}")
        } else if out.is_empty() {
            String::from(" ")
        } else {
            out
        }
    }

    /// Start a POST on a green thread and return a job id at once; the app
    /// polls `job_poll` (each `present()` gives the thread a turn, and the
    /// coroutine-aware network waits hand the CPU back between segments).
    /// Falls back to the old synchronous call if no stack can be spawned.
    pub fn post_async(url: &str, body: &str, content_type: &str) -> String {
        let n = net();
        let id = format!("job{}", n.next_job);
        n.next_job += 1;
        job_owned(&id);
        let (u, b, ct, jid) = (String::from(url), String::from(body), String::from(content_type), id.clone());
        let spawned = crate::coro::spawn(
            "http_post",
            false,
            alloc::boxed::Box::new(move || {
                let (status, head, out) = fetch("POST", &u, &ct, &b);
                let result = job_result(status, out, &u);
                net().jobs.push((jid, result, status, head));
            }),
        );
        if spawned == 0 {
            let (status, head, out) = fetch("POST", url, content_type, body);
            let result = job_result(status, out, url);
            net().jobs.push((id.clone(), result, status, head));
        }
        id
    }

    /// `post_async` for GET: a page fetch that doesn't freeze the UI. Horizon
    /// polls `job_poll` and keeps drawing its spinner while the TLS handshake
    /// and body stream run on the green thread.
    pub fn get_async(url: &str) -> String {
        get_async_h(url, "")
    }

    /// `get_async` for a top-level page navigation (`http_nav_async`): the GET
    /// carries a browser's navigation headers (`NAV_HEADERS`).
    pub fn nav_async(url: &str) -> String {
        get_async_h(url, NAV_HEADERS)
    }

    fn get_async_h(url: &str, extra: &'static str) -> String {
        let n = net();
        let id = format!("job{}", n.next_job);
        n.next_job += 1;
        job_owned(&id);
        let (u, jid) = (String::from(url), id.clone());
        let spawned = crate::coro::spawn(
            "http_get",
            false,
            alloc::boxed::Box::new(move || {
                let (status, head, out) = fetch_h("GET", &u, "", "", extra);
                let result = job_result(status, out, &u);
                net().jobs.push((jid, result, status, head));
            }),
        );
        if spawned == 0 {
            let (status, head, out) = fetch_h("GET", url, "", "", extra);
            let result = job_result(status, out, url);
            net().jobs.push((id.clone(), result, status, head));
        }
        id
    }

    pub fn job_poll(id: &str) -> String {
        crate::cookies::flush(false);
        let n = net();
        match n.jobs.iter().position(|(j, ..)| j == id) {
            Some(i) => {
                let (_, text, status, head) = n.jobs.remove(i);
                unsafe { (*(&raw mut JOB_OWNERS)).retain(|o| o.0 != id) };
                // the app reads http_get_status / http_header right after taking it
                n.last_status = status;
                n.last_headers = head;
                text
            },
            None => String::new(),
        }
    }
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 2;
                    },
                    _ => out.push(b'%'),
                }
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// App `uid` ended (appres.rs): its net_* lobby and unpolled HTTP answers go.
pub fn app_ended(uid: u32, f: &mut crate::appres::Freed) {
    imp::app_ended(uid, f);
}

/// `(status, body)` of a GET -- the console's `curl`.
pub fn http_get(url: &str) -> (u16, String) {
    imp::request("GET", url, "", "")
}

/// A header of the last response (case-insensitive), "" if absent.
pub fn last_header(name: &str) -> String {
    imp::last_header(name)
}

/// The networking builtins; `None` for any other name.
pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    let s = |i: usize| args.get(i).map(|&v| to_string(v)).unwrap_or_default();
    let n = |i: usize, d: f64| args.get(i).map(|&v| to_f64(v)).unwrap_or(d);
    let name: &[u8] = match core::str::from_utf8(name).unwrap_or("") {
        "เน็ตโฮสต์" => b"net_host",
        "เน็ตจอย" => b"net_join",
        "เน็ตส่ง" => b"net_send",
        "เน็ตรับ" => b"net_recv",
        "เน็ตสถานะ" => b"net_status",
        "연결종료" => b"net_close",
        "เน็ตประกาศ" => b"net_announce",
        "เน็ตหยุดประกาศ" => b"net_announce_stop",
        "เน็ตค้นหา" => b"net_discover",
        "เน็ตทดสอบ" => b"net_test",
        "เว็บรับ" => b"http_get",
        _ => name,
    };
    Some(match name {
        b"net_host" => {
            imp::host(n(0, 7777.0) as u16);
            UNIT
        },
        b"net_join" => {
            let ip = if args.is_empty() { String::from("127.0.0.1") } else { s(0) };
            imp::join(&ip, n(1, 7777.0) as u16);
            UNIT
        },
        b"net_send" => {
            imp::send(&s(0));
            UNIT
        },
        b"net_send_to" => {
            imp::send_to(n(0, 0.0) as u64, &s(1));
            UNIT
        },
        b"net_recv" => mk_str(&imp::recv()),
        b"net_recv_from" => mk_str(&imp::recv_from()),
        b"net_status" => num(imp::status() as f64),
        b"net_close" => {
            imp::close();
            UNIT
        },
        b"net_clients" => mk_str(&imp::clients()),
        b"net_client_count" => num(imp::client_count() as f64),
        b"net_events" => mk_str(&imp::events()),
        b"net_announce" => {
            imp::announce(n(0, 7778.0) as u16, &s(1));
            UNIT
        },
        b"net_announce_stop" => {
            imp::announce_stop();
            UNIT
        },
        b"net_discover" => mk_str(&imp::discover(n(0, 7778.0) as u16)),
        b"net_test" => mk_str(&imp::test_bind(n(0, 7777.0) as u16)),
        b"http_get" => mk_str(&imp::request("GET", &s(0), "", "").1),
        b"http_get_status" => num(imp::last_status() as f64),
        b"http_header" => mk_str(&imp::last_header(&s(0))),
        b"http_post_async" => {
            let ct = if args.len() > 2 { s(2) } else { String::from("application/json") };
            mk_str(&imp::post_async(&s(0), &s(1), &ct))
        },
        b"http_get_async" => mk_str(&imp::get_async(&s(0))),
        b"http_nav_async" => mk_str(&imp::nav_async(&s(0))),
        b"http_job_poll" => mk_str(&imp::job_poll(&s(0))),
        b"url_decode" => mk_str(&url_decode(&s(0))),
        b"capsrv_start" => mk_str(&capsrv::start(&s(0))),
        b"capsrv_poll" => mk_str(&capsrv::poll()),
        b"capsrv_hits" => num(capsrv::hits() as f64),
        b"capsrv_stop" => {
            capsrv::stop();
            UNIT
        },
        _ => return None,
    })
}

// -- capsrv: a one-page LAN web server for a captcha handed to the phone --------
//
// Horizon cannot run hCaptcha (iframes, canvas, its proof-of-work scripts), so a
// sign-in that asks for one shows a QR code instead: the phone opens this page
// on the local network, a person solves the real hCaptcha there, and the page
// POSTs the answer back here. `capsrv_poll` hands that answer to the app once.
// Every request path must carry the random nonce, so nothing else on the
// network can read the page or plant an answer by guessing the port.
pub mod capsrv {
    use crate::arch::timer;
    use crate::drivers::netstack as ns;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;

    pub const PORT: u16 = 8642;
    const MAX_REQ: usize = 64 * 1024;
    const CONN_TTL_US: u64 = 15_000_000;

    /// A phone's connection. Nothing here ever waits on the network: capsrv_poll runs on the
    /// desktop's own core every frame, and a reply that waited for the peer (a phone that went
    /// to sleep, a LAN scanner that never ACKs) froze the whole screen for 7-8 s each time.
    /// The answer goes out as fast as the connection takes it, a little per frame, then FIN.
    struct Conn {
        h: usize,
        buf: Vec<u8>,
        since: u64,
        /// the reply, once the request is in, and how much of it has gone out
        out: Option<Vec<u8>>,
        sent: usize,
        /// when our FIN went (the socket is freed once it is acknowledged, or soon after)
        fin_at: Option<u64>,
    }

    static mut ON: bool = false;
    static mut PAGE: String = String::new();
    static mut NONCE: String = String::new();
    static mut ANSWER: Option<String> = None;
    static mut HITS: u32 = 0;
    static mut CONNS: Vec<Conn> = Vec::new();

    fn ip_text(ip: [u8; 4]) -> String {
        format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
    }

    /// Start (or restart) serving `page`; answers the URL the phone should open
    /// (with a fresh nonce), or "" when there is no network address yet.
    /// The page may contain `{ANSWER_PATH}`, replaced by where to POST the answer.
    pub fn start(page: &str) -> String {
        stop();
        let ip = ns::self_ip();
        if ip == [0, 0, 0, 0] {
            return String::new();
        }
        let mut r = [0u8; 12];
        if !crate::crypto::random_bytes(&mut r) {
            let t = timer::now_us().to_le_bytes();
            r[..8].copy_from_slice(&t);
        }
        let mut nonce = String::new();
        for b in r {
            nonce.push_str(&format!("{:02x}", b));
        }
        let page = page.replace("{ANSWER_PATH}", &format!("/{}/answer", nonce));
        unsafe {
            *(&raw mut PAGE) = page;
            *(&raw mut NONCE) = nonce.clone();
            *(&raw mut ANSWER) = None;
            HITS = 0;
            ON = true;
        }
        // open the listener now so the phone's first try connects
        if let Some(h) = ns::tcp_listen_accept(PORT, 1) {
            unsafe { (*(&raw mut CONNS)).push(Conn::new(h)) };
        }
        format!("http://{}:{}/{}", ip_text(ip), PORT, nonce)
    }

    pub fn stop() {
        unsafe {
            if !ON {
                return;
            }
            ON = false;
            for c in (*(&raw mut CONNS)).drain(..) {
                ns::tcp_free_h(c.h);
            }
            *(&raw mut PAGE) = String::new();
            *(&raw mut NONCE) = String::new();
        }
        ns::tcp_unlisten(PORT);
    }

    /// How many times the page itself was fetched (the UI says "opened on the phone").
    pub fn hits() -> u32 {
        unsafe { HITS }
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    impl Conn {
        fn new(h: usize) -> Conn {
            Conn { h, buf: Vec::new(), since: timer::now_us(), out: None, sent: 0, fin_at: None }
        }
    }

    /// The HTTP answer as bytes (sent later, without waiting, by `poll`).
    fn reply(status: &str, ctype: &str, body: &[u8]) -> Vec<u8> {
        let head = format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
            status,
            ctype,
            body.len()
        );
        let mut all = Vec::with_capacity(head.len() + body.len());
        all.extend_from_slice(head.as_bytes());
        all.extend_from_slice(body);
        all
    }

    /// A whole request in `buf` -> the answer to send; None while more bytes are due.
    fn handle(buf: &[u8]) -> Option<Vec<u8>> {
        let Some(end) = find(buf, b"\r\n\r\n") else {
            return if buf.len() >= MAX_REQ { Some(reply("413 Payload Too Large", "text/plain", b"too big")) } else { None };
        };
        let head = core::str::from_utf8(&buf[..end]).unwrap_or("");
        let mut clen = 0usize;
        for line in head.split("\r\n").skip(1) {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case("content-length") {
                    clen = v.trim().parse().unwrap_or(0);
                }
            }
        }
        let body_at = end + 4;
        if buf.len() < body_at + clen && buf.len() < MAX_REQ {
            return None;
        }
        let body = &buf[body_at..(body_at + clen).min(buf.len())];
        let first = head.split("\r\n").next().unwrap_or("");
        let mut parts = first.split(' ');
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        let nonce = unsafe { (*(&raw const NONCE)).clone() };
        let page_path = format!("/{}", nonce);
        let answer_path = format!("/{}/answer", nonce);
        Some(if nonce.is_empty() {
            reply("404 Not Found", "text/plain", b"gone")
        } else if method == "GET" && path == page_path {
            unsafe { HITS += 1 };
            let page = unsafe { (*(&raw const PAGE)).clone() };
            reply("200 OK", "text/html; charset=utf-8", page.as_bytes())
        } else if method == "POST" && path == answer_path && !body.is_empty() {
            let tok = String::from(core::str::from_utf8(body).unwrap_or("").trim());
            if tok.is_empty() || tok.len() > 32 * 1024 {
                reply("400 Bad Request", "text/plain", b"empty answer")
            } else {
                unsafe { *(&raw mut ANSWER) = Some(tok) };
                reply("200 OK", "text/plain", b"ok")
            }
        } else {
            reply("404 Not Found", "text/plain", b"not here")
        })
    }

    /// Serve what has arrived (never waits long); answers the solved captcha once, else "".
    pub fn poll() -> String {
        if !unsafe { ON } {
            return String::new();
        }
        // new phones (at most a few per frame)
        for _ in 0..4 {
            match ns::tcp_listen_accept(PORT, 1) {
                Some(h) => unsafe { (*(&raw mut CONNS)).push(Conn::new(h)) },
                None => break,
            }
        }
        let now = timer::now_us();
        let mut done: Vec<usize> = Vec::new();
        let conns = unsafe { &mut *(&raw mut CONNS) };
        for (i, c) in conns.iter_mut().enumerate() {
            let too_old = now.saturating_sub(c.since) > CONN_TTL_US;
            if let Some(t) = c.fin_at {
                // our FIN is out: free once everything is acknowledged (or 3 s on)
                if ns::tcp_unacked_h(c.h) == 0 || ns::tcp_closed_h(c.h) || now.saturating_sub(t) > 3_000_000 || too_old {
                    ns::tcp_free_h(c.h);
                    done.push(i);
                }
                continue;
            }
            if c.out.is_none() {
                let mut tmp = [0u8; 2048];
                loop {
                    let n = ns::tcp_read_h(c.h, &mut tmp, 1);
                    if n == 0 {
                        break;
                    }
                    c.buf.extend_from_slice(&tmp[..n]);
                    if c.buf.len() >= MAX_REQ {
                        break;
                    }
                }
                c.out = handle(&c.buf);
            }
            if let Some(out) = c.out.as_ref() {
                // as much as the connection takes right now -- never a wait
                while c.sent < out.len() {
                    let n = (out.len() - c.sent).min(1400);
                    if !ns::tcp_writable_h(c.h, n) || !ns::tcp_write_h(c.h, &out[c.sent..c.sent + n]) {
                        break;
                    }
                    c.sent += n;
                }
                if c.sent >= out.len() {
                    ns::tcp_fin_h(c.h);
                    c.fin_at = Some(now);
                    continue;
                }
            }
            if ns::tcp_closed_h(c.h) || too_old {
                ns::tcp_free_h(c.h);
                done.push(i);
            }
        }
        for i in done.into_iter().rev() {
            conns.remove(i);
        }
        unsafe { (*(&raw mut ANSWER)).take() }.unwrap_or_default()
    }
}
