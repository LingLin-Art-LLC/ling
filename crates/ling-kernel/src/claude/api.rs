//! One streamed Messages API request (POST /v1/messages, `"stream": true`) over the
//! kernel's TLS with the server's certificate checked (`lingsock::Stream::connect_verified`).
//! The HTTP body is un-chunked as it arrives and cut into Server-Sent Events; each
//! event updates the content blocks being assembled and, for text, is handed to
//! `on_text` at once so the reply shows while it is written.
//!
//! Content blocks are kept as the API's JSON objects (serde_json::Value), so every
//! block -- thinking (with its signature), tool_use, fallback markers, anything a
//! later API version adds -- goes back into the history byte-for-byte as it came.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde_json::{json, Value};

pub const HOST: &str = "api.anthropic.com";
pub const VERSION: &str = "2026-10-21";
pub const BETA: &str = "server-side-fallback-2026-07-01";

/// What a finished (or failed) request produced.
pub struct Reply {
    pub content: Vec<Value>,
    pub stop_reason: String,
    pub stop_details: Value,
    pub usage: Usage,
    pub model: String,
}

#[derive(Clone, Copy, Default)]
pub struct Usage {
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl Usage {
    fn take(&mut self, u: &Value) {
        let g = |k: &str| u.get(k).and_then(|v| v.as_u64());
        if let Some(v) = g("input_tokens") {
            self.input = v;
        }
        if let Some(v) = g("cache_creation_input_tokens") {
            self.cache_write = v;
        }
        if let Some(v) = g("cache_read_input_tokens") {
            self.cache_read = v;
        }
        if let Some(v) = g("output_tokens") {
            self.output = v;
        }
    }

    /// US dollars at Claude Opus 5.5's prices: $4 / MTok in, $20 out, $0.20 cache reads,
    /// $5 cache writes (5-minute TTL, 1.25x input).
    pub fn dollars(&self) -> f64 {
        (self.input as f64 * 4.0 + self.output as f64 * 20.0 + self.cache_read as f64 * 0.2 + self.cache_write as f64 * 5.0) / 1_000_000.0
    }
}

pub enum Event<'a> {
    /// streamed reply text
    Text(&'a str),
    /// a new block started (its type: "thinking", "tool_use", ...) and, for tool_use, its name
    BlockStart(&'a str, &'a str),
    /// what the request is doing now ("connecting to ...", "sending ...", "waiting for ...") --
    /// the caller tells the user when one of these takes long
    Stage(&'a str),
}

/// The whole request must be on its way within this (a stalled upload is an error, not a hang).
const SEND_LIMIT_MS: u64 = 60_000;
/// The response head must start within this after the request went out. The API answers
/// with its headers at once (the reply then streams), so a long silence here is a dead
/// connection -- seen on the AX201 Wi-Fi as a cursor that sat there for minutes.
const FIRST_BYTE_LIMIT_MS: u64 = 60_000;

/// One line on the console (the boot log) about a failed request, with its timings: on
/// real hardware it is the only trace of where a request died.
fn note_failure(host: &str, why: &str, t0: u64, t_conn: u64, t_sent: u64, bytes: usize) {
    let now = crate::arch::timer::now_ms();
    let conn = if t_conn > 0 { format!("{} ms", t_conn - t0) } else { String::from("-") };
    let sent = if t_sent > 0 { format!("{} ms", t_sent - t0) } else { String::from("-") };
    crate::console_write(format!("claude: request to {host} failed after {} ms (connected {conn}, sent {sent}, {bytes} B): {why}\n", now - t0).as_bytes());
}

/// Where the request goes: the real API (verified TLS, the user's key) or a test server
/// (`claude --base-url http://host:port`: plain HTTP, or `https://host:port`: TLS without a
/// certificate check -- either way a dummy key: the real key never leaves for anything but
/// a verified api.anthropic.com).
#[derive(Clone)]
pub enum Target {
    Anthropic,
    /// host, port, TLS
    Mock(String, u16, bool),
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// Body bytes of a chunked response decoded so far; `raw` keeps what is not a whole chunk yet.
struct Chunked {
    raw: Vec<u8>,
    done: bool,
}

impl Chunked {
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        self.raw.extend_from_slice(bytes);
        loop {
            let Some(e) = find(&self.raw, b"\r\n") else { return };
            let size = usize::from_str_radix(core::str::from_utf8(&self.raw[..e]).unwrap_or("0").split(';').next().unwrap_or("0").trim(), 16).unwrap_or(0);
            if size == 0 {
                self.done = true;
                return;
            }
            if self.raw.len() < e + 2 + size + 2 {
                return;
            }
            out.extend_from_slice(&self.raw[e + 2..e + 2 + size]);
            self.raw.drain(..e + 2 + size + 2);
        }
    }
}

/// Stream one request. `cancel()` is polled between reads (true = stop now: Err("cancelled")).
pub fn stream(target: &Target, key: &str, body: &Value, on: &mut dyn FnMut(Event), cancel: &mut dyn FnMut() -> bool) -> Result<Reply, String> {
    use crate::drivers::netstack as ns;
    let body = serde_json::to_vec(body).map_err(|e| format!("could not encode the request: {e}"))?;
    let (host, port, real, mock_tls) = match target {
        Target::Anthropic => (String::from(HOST), 443u16, true, false),
        Target::Mock(h, p, t) => (h.clone(), *p, false, *t),
    };
    let key_sent = if real { key } else { "sk-ant-test-not-a-real-key" };
    let head = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: {host}\r\nUser-Agent: LingOS-Claude/1.0\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nAccept-Encoding: identity\r\nx-api-key: {key_sent}\r\nanthropic-version: {VERSION}\r\nanthropic-beta: {BETA}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _g = ns::NetShare::new();
    let now = crate::arch::timer::now_ms;
    let t0 = now();
    let fail = |why: String, t_conn: u64, t_sent: u64, bytes: usize| -> String {
        if why != "cancelled" {
            note_failure(&host, &why, t0, t_conn, t_sent, bytes);
        }
        why
    };
    on(Event::Stage(&format!("connecting to {host}")));
    let mut st = match if real { crate::lingsock::Stream::connect_verified(&host, port) } else { crate::lingsock::Stream::connect(&host, port, mock_tls) } {
        Ok(st) => st,
        Err(e) => return Err(fail(e, 0, 0, 0)),
    };
    let t_conn = now();
    let mut wire = head.into_bytes();
    wire.extend_from_slice(&body);
    // in slices (one TLS record each): between them the session sees Ctrl+C, other green
    // threads and the desktop get the CPU, and a stalled upload ends at SEND_LIMIT_MS
    on(Event::Stage(&format!("sending the request ({} KB)", wire.len().div_ceil(1024))));
    let mut sent = 0usize;
    for piece in wire.chunks(4096) {
        if cancel() {
            st.close();
            return Err(String::from("cancelled"));
        }
        if now() - t_conn > SEND_LIMIT_MS {
            st.close();
            return Err(fail(format!("the request to {host} did not go out: {} of {} KB sent in {} s (the network is not getting data through)", sent / 1024, wire.len().div_ceil(1024), SEND_LIMIT_MS / 1000), t_conn, 0, sent));
        }
        if !st.send(piece) {
            st.close();
            return Err(fail(format!("could not send the request to {host} ({} of {} KB went out)", sent / 1024, wire.len().div_ceil(1024)), t_conn, 0, sent));
        }
        sent += piece.len();
    }
    let t_sent = now();
    // read the response head
    on(Event::Stage(&format!("waiting for {host} to answer")));
    let he = loop {
        if let Some(i) = find(&st.plain, b"\r\n\r\n") {
            break i;
        }
        if cancel() {
            st.close();
            return Err(String::from("cancelled"));
        }
        if st.closed {
            st.close();
            return Err(fail(format!("{host} closed the connection {} s after the request without answering", (now() - t_sent) / 1000), t_conn, t_sent, sent));
        }
        if now() - t_sent > FIRST_BYTE_LIMIT_MS {
            st.close();
            let got = if st.plain.is_empty() { String::from("nothing came back") } else { format!("only {} bytes of a reply came back", st.plain.len()) };
            return Err(fail(format!("{host} did not answer within {} s of the request ({got})", FIRST_BYTE_LIMIT_MS / 1000), t_conn, t_sent, sent));
        }
        st.pump(200_000);
    };
    on(Event::Stage("")); // answered: what follows is the reply
    let head = String::from_utf8_lossy(&st.plain[..he]).to_string();
    let status: u16 = head.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let lower = head.to_ascii_lowercase();
    let chunked = lower.contains("transfer-encoding: chunked");
    let mut rest: Vec<u8> = st.plain[he + 4..].to_vec();
    st.plain.clear();
    let mut dech = Chunked { raw: Vec::new(), done: false };
    let mut body_buf: Vec<u8> = Vec::new();
    let mut take = |bytes: &[u8], out: &mut Vec<u8>, dech: &mut Chunked| {
        if chunked {
            dech.feed(bytes, out);
        } else {
            out.extend_from_slice(bytes);
        }
    };
    take(&rest, &mut body_buf, &mut dech);
    rest.clear();

    if status != 200 {
        // an error answer: read it whole (small) and report the API's message
        let t0 = crate::arch::timer::now_ms();
        while !st.closed && !dech.done && crate::arch::timer::now_ms() - t0 < 10_000 {
            if st.pump(200_000) > 0 {
                let p = core::mem::take(&mut st.plain);
                take(&p, &mut body_buf, &mut dech);
            }
        }
        st.close();
        let msg = serde_json::from_slice::<Value>(&body_buf)
            .ok()
            .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(String::from))
            .unwrap_or_else(|| String::from_utf8_lossy(&body_buf).chars().take(300).collect());
        let hint = match status {
            401 => " -- the API key was refused (claude login sets a new one)",
            403 => " -- this key may not use that model or feature",
            429 => " -- rate limited; wait a moment and try again",
            529 | 503 => " -- the API is busy; try again shortly",
            _ => "",
        };
        return Err(format!("HTTP {status}: {msg}{hint}"));
    }

    let mut reply = Reply { content: Vec::new(), stop_reason: String::new(), stop_details: Value::Null, usage: Usage::default(), model: String::new() };
    // per block: the accumulated tool input JSON text
    let mut partial: Vec<String> = Vec::new();
    let mut sse: Vec<u8> = Vec::new();
    let mut finished = false;
    let mut idle_ms = 0u64;
    loop {
        // events in the decoded body
        sse.extend_from_slice(&body_buf);
        body_buf.clear();
        while let Some(end) = find(&sse, b"\n\n") {
            let ev: Vec<u8> = sse.drain(..end + 2).collect();
            let text = String::from_utf8_lossy(&ev).replace('\r', "");
            let data: String = text.lines().filter_map(|l| l.strip_prefix("data:")).map(|d| d.trim_start()).collect::<Vec<_>>().join("\n");
            if data.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&data) else { continue };
            match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "message_start" => {
                    if let Some(m) = v.get("message") {
                        reply.model = m.get("model").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        if let Some(u) = m.get("usage") {
                            reply.usage.take(u);
                        }
                    }
                },
                "content_block_start" => {
                    let i = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                    let mut b = v.get("content_block").cloned().unwrap_or(json!({}));
                    let ty = b.get("type").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    let name = b.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    if ty == "tool_use" {
                        // the input arrives as JSON fragments; it is set at the block's end
                        b["input"] = json!({});
                    }
                    while reply.content.len() <= i {
                        reply.content.push(json!({}));
                        partial.push(String::new());
                    }
                    reply.content[i] = b;
                    on(Event::BlockStart(&ty, &name));
                },
                "content_block_delta" => {
                    let i = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                    let Some(d) = v.get("delta") else { continue };
                    if i >= reply.content.len() {
                        continue;
                    }
                    match d.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                        "text_delta" => {
                            let t = d.get("text").and_then(|x| x.as_str()).unwrap_or("");
                            let cur = reply.content[i].get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
                            reply.content[i]["text"] = Value::String(cur + t);
                            on(Event::Text(t));
                        },
                        "thinking_delta" => {
                            let t = d.get("thinking").and_then(|x| x.as_str()).unwrap_or("");
                            let cur = reply.content[i].get("thinking").and_then(|x| x.as_str()).unwrap_or("").to_string();
                            reply.content[i]["thinking"] = Value::String(cur + t);
                        },
                        "signature_delta" => {
                            let t = d.get("signature").and_then(|x| x.as_str()).unwrap_or("");
                            let cur = reply.content[i].get("signature").and_then(|x| x.as_str()).unwrap_or("").to_string();
                            reply.content[i]["signature"] = Value::String(cur + t);
                        },
                        "input_json_delta" => {
                            partial[i].push_str(d.get("partial_json").and_then(|x| x.as_str()).unwrap_or(""));
                        },
                        "citations_delta" => {
                            if let Some(c) = d.get("citation") {
                                let mut arr = reply.content[i].get("citations").and_then(|x| x.as_array()).cloned().unwrap_or_default();
                                arr.push(c.clone());
                                reply.content[i]["citations"] = Value::Array(arr);
                            }
                        },
                        _ => {},
                    }
                },
                "content_block_stop" => {
                    let i = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                    if i < reply.content.len() && reply.content[i].get("type").and_then(|x| x.as_str()) == Some("tool_use") {
                        let raw = if partial[i].trim().is_empty() { "{}" } else { partial[i].as_str() };
                        // strict: the caller validates against the schema; unparsable input is marked
                        match serde_json::from_str::<Value>(raw) {
                            Ok(inp) => reply.content[i]["input"] = inp,
                            Err(_) => reply.content[i]["_invalid_json"] = Value::String(String::from(raw)),
                        }
                    }
                },
                "message_delta" => {
                    if let Some(d) = v.get("delta") {
                        if let Some(r) = d.get("stop_reason").and_then(|x| x.as_str()) {
                            reply.stop_reason = r.to_string();
                        }
                        if let Some(sd) = d.get("stop_details") {
                            reply.stop_details = sd.clone();
                        }
                    }
                    if let Some(u) = v.get("usage") {
                        reply.usage.take(u);
                    }
                },
                "message_stop" => finished = true,
                "error" => {
                    st.close();
                    let m = v.pointer("/error/message").and_then(|x| x.as_str()).unwrap_or("unknown error");
                    let t = v.pointer("/error/type").and_then(|x| x.as_str()).unwrap_or("error");
                    return Err(format!("the API stopped the reply ({t}): {m}"));
                },
                _ => {}, // ping and anything new
            }
        }
        if finished || (chunked && dech.done) {
            break;
        }
        if cancel() {
            st.close();
            return Err(String::from("cancelled"));
        }
        if st.closed {
            break;
        }
        if st.pump(150_000) == 0 {
            idle_ms += 150;
            if idle_ms > 180_000 {
                st.close();
                return Err(String::from("the reply stalled for 3 minutes"));
            }
        } else {
            idle_ms = 0;
            let p = core::mem::take(&mut st.plain);
            take(&p, &mut body_buf, &mut dech);
        }
    }
    st.close();
    if !finished {
        return Err(String::from("the connection ended before the reply was complete"));
    }
    // remove the client-side marker before the block goes into history (kept separately by the caller)
    Ok(reply)
}
