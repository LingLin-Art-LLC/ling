//! Spotify's access point ("AP"): apresolve, the Diffie-Hellman handshake
//! (ClientHello / APResponseMessage / ClientResponsePlaintext, the server's
//! signature checked against Spotify's RSA key), the Shannon-framed session,
//! and the login (ClientResponseEncrypted -> APWelcome | APLoginFailed).
//! Follows librespot 0.8's core/src/connection.

use super::pb::{self, W};
use super::shannon::Shannon;
use crate::drivers::netstack as ns;
use crate::lingsock::Stream;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use rsa::BigUint;

const DH_PRIME: [u8; 96] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xc9, 0x0f, 0xda, 0xa2, 0x21, 0x68, 0xc2, 0x34, 0xc4, 0xc6, 0x62, 0x8b, 0x80, 0xdc, 0x1c, 0xd1, 0x29, 0x02, 0x4e, 0x08,
    0x8a, 0x67, 0xcc, 0x74, 0x02, 0x0b, 0xbe, 0xa6, 0x3b, 0x13, 0x9b, 0x22, 0x51, 0x4a, 0x08, 0x79, 0x8e, 0x34, 0x04, 0xdd, 0xef, 0x95, 0x19, 0xb3, 0xcd, 0x3a, 0x43, 0x1b,
    0x30, 0x2b, 0x0a, 0x6d, 0xf2, 0x5f, 0x14, 0x37, 0x4f, 0xe1, 0x35, 0x6d, 0x6d, 0x51, 0xc2, 0x45, 0xe4, 0x85, 0xb5, 0x76, 0x62, 0x5e, 0x7e, 0xc6, 0xf4, 0x4c, 0x42, 0xe9,
    0xa6, 0x3a, 0x36, 0x20, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
];

const SERVER_KEY: [u8; 256] = [
    0xac, 0xe0, 0x46, 0x0b, 0xff, 0xc2, 0x30, 0xaf, 0xf4, 0x6b, 0xfe, 0xc3, 0xbf, 0xbf, 0x86, 0x3d, 0xa1, 0x91, 0xc6, 0xcc, 0x33, 0x6c, 0x93, 0xa1, 0x4f, 0xb3, 0xb0, 0x16,
    0x12, 0xac, 0xac, 0x6a, 0xf1, 0x80, 0xe7, 0xf6, 0x14, 0xd9, 0x42, 0x9d, 0xbe, 0x2e, 0x34, 0x66, 0x43, 0xe3, 0x62, 0xd2, 0x32, 0x7a, 0x1a, 0x0d, 0x92, 0x3b, 0xae, 0xdd,
    0x14, 0x02, 0xb1, 0x81, 0x55, 0x05, 0x61, 0x04, 0xd5, 0x2c, 0x96, 0xa4, 0x4c, 0x1e, 0xcc, 0x02, 0x4a, 0xd4, 0xb2, 0x0c, 0x00, 0x1f, 0x17, 0xed, 0xc2, 0x2f, 0xc4, 0x35,
    0x21, 0xc8, 0xf0, 0xcb, 0xae, 0xd2, 0xad, 0xd7, 0x2b, 0x0f, 0x9d, 0xb3, 0xc5, 0x32, 0x1a, 0x2a, 0xfe, 0x59, 0xf3, 0x5a, 0x0d, 0xac, 0x68, 0xf1, 0xfa, 0x62, 0x1e, 0xfb,
    0x2c, 0x8d, 0x0c, 0xb7, 0x39, 0x2d, 0x92, 0x47, 0xe3, 0xd7, 0x35, 0x1a, 0x6d, 0xbd, 0x24, 0xc2, 0xae, 0x25, 0x5b, 0x88, 0xff, 0xab, 0x73, 0x29, 0x8a, 0x0b, 0xcc, 0xcd,
    0x0c, 0x58, 0x67, 0x31, 0x89, 0xe8, 0xbd, 0x34, 0x80, 0x78, 0x4a, 0x5f, 0xc9, 0x6b, 0x89, 0x9d, 0x95, 0x6b, 0xfc, 0x86, 0xd7, 0x4f, 0x33, 0xa6, 0x78, 0x17, 0x96, 0xc9,
    0xc3, 0x2d, 0x0d, 0x32, 0xa5, 0xab, 0xcd, 0x05, 0x27, 0xe2, 0xf7, 0x10, 0xa3, 0x96, 0x13, 0xc4, 0x2f, 0x99, 0xc0, 0x27, 0xbf, 0xed, 0x04, 0x9c, 0x3c, 0x27, 0x58, 0x04,
    0xb6, 0xb2, 0x19, 0xf9, 0xc1, 0x2f, 0x02, 0xe9, 0x48, 0x63, 0xec, 0xa1, 0xb6, 0x42, 0xa0, 0x9d, 0x48, 0x25, 0xf8, 0xb3, 0x9d, 0xd0, 0xe8, 0x6a, 0xf9, 0x48, 0x4d, 0xa1,
    0xc2, 0xba, 0x86, 0x30, 0x42, 0xea, 0x9d, 0xb3, 0x08, 0x6c, 0x19, 0x0e, 0x48, 0xb3, 0x9d, 0x66, 0xeb, 0x00, 0x06, 0xa2, 0x5a, 0xee, 0xa1, 0x1b, 0x13, 0x87, 0x3c, 0xd7,
    0x19, 0xe6, 0x55, 0xbd,
];

/// librespot 0.8's desktop client version (version.rs)
pub const SPOTIFY_VERSION: u64 = 124200290;
pub const SPOTIFY_SEMVER: &str = "1.2.52.442";
/// the keymaster client id librespot uses on Linux (config.rs)
pub const CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";

pub const CMD_PING: u8 = 0x04;
pub const CMD_REQUEST_KEY: u8 = 0x0c;
pub const CMD_AES_KEY: u8 = 0x0d;
pub const CMD_AES_KEY_ERROR: u8 = 0x0e;
pub const CMD_COUNTRY: u8 = 0x1b;
pub const CMD_PONG: u8 = 0x49;
pub const CMD_PONG_ACK: u8 = 0x4a;
pub const CMD_PRODUCT_INFO: u8 = 0x50;
pub const CMD_LOGIN: u8 = 0xab;
pub const CMD_WELCOME: u8 = 0xac;
pub const CMD_AUTH_FAILURE: u8 = 0xad;

// ── Diffie-Hellman over Spotify's 768-bit group (generator 2) ──

pub struct Dh {
    private: BigUint,
    pub public: Vec<u8>,
}

impl Dh {
    pub fn random() -> Dh {
        let mut b = [0u8; 95];
        crate::crypto::random_bytes(&mut b);
        let private = BigUint::from_bytes_le(&b);
        let p = BigUint::from_bytes_be(&DH_PRIME);
        let public = BigUint::from(2u32).modpow(&private, &p).to_bytes_be();
        Dh { private, public }
    }
    pub fn shared(&self, remote: &[u8]) -> Vec<u8> {
        let p = BigUint::from_bytes_be(&DH_PRIME);
        BigUint::from_bytes_be(remote).modpow(&self.private, &p).to_bytes_be()
    }
}

fn hmac_sha1(key: &[u8], parts: &[&[u8]]) -> [u8; 20] {
    use hmac::{Hmac, Mac};
    let mut m = <Hmac<sha1::Sha1> as Mac>::new_from_slice(key).expect("hmac key");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// The server's DH public key must be signed by Spotify (PKCS#1 v1.5, SHA-1).
fn verify_server(gs: &[u8], sig: &[u8]) -> bool {
    use sha1::Digest;
    let Ok(key) = rsa::RsaPublicKey::new(BigUint::from_bytes_be(&SERVER_KEY), BigUint::from(65537u32)) else { return false };
    // DigestInfo for SHA-1 + the hash (the unprefixed scheme compares the padded block's payload)
    let mut di = vec![0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14];
    di.extend_from_slice(&sha1::Sha1::digest(gs));
    key.verify(rsa::Pkcs1v15Sign::new_unprefixed(), &di, sig).is_ok()
}

// ── apresolve ──

/// "host:port" access points, best first (apresolve.spotify.com; ap.spotify.com:443 when it fails).
pub fn resolve(kind: &str) -> Vec<(String, u16)> {
    let mut out = Vec::new();
    if let Ok((200, _, body)) = crate::lingsock::request_bytes("GET", &format!("https://apresolve.spotify.com/?type={kind}"), "", b"") {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
            if let Some(a) = v.get(kind).and_then(|a| a.as_array()) {
                for e in a {
                    if let Some((h, p)) = e.as_str().and_then(|s| s.rsplit_once(':')) {
                        if let Ok(p) = p.parse::<u16>() {
                            out.push((String::from(h), p));
                        }
                    }
                }
            }
        }
    }
    if out.is_empty() {
        let fb = match kind {
            "dealer" => "dealer.spotify.com",
            "spclient" => "spclient.wg.spotify.com",
            _ => "ap.spotify.com",
        };
        out.push((String::from(fb), 443));
    }
    out
}

// ── the session transport ──

pub struct Ap {
    pub(crate) st: Stream,
    enc: Shannon,
    enc_n: u32,
    dec: Shannon,
    dec_n: u32,
    /// a decrypted header waiting for its payload: (cmd, size)
    pending: Option<(u8, usize)>,
    pub host: String,
}

/// What the login answered.
pub enum Login {
    /// canonical username, reusable auth type, reusable auth data
    Welcome(String, u32, Vec<u8>),
    /// the APLoginFailed error code + description
    Failed(u32, String),
}

pub fn error_name(code: u32) -> &'static str {
    match code {
        0 => "ProtocolError",
        2 => "TryAnotherAP",
        5 => "BadConnectionId",
        9 => "TravelRestriction",
        11 => "PremiumAccountRequired",
        12 => "BadCredentials",
        13 => "CouldNotValidateCredentials",
        14 => "AccountExists",
        15 => "ExtraVerificationRequired",
        16 => "InvalidAppKey",
        17 => "ApplicationBanned",
        _ => "unknown",
    }
}

fn read_exact(st: &mut Stream, n: usize, budget_ms: u64) -> Result<Vec<u8>, String> {
    let t0 = crate::arch::timer::now_ms();
    while st.plain.len() < n {
        if st.closed {
            return Err(String::from("the access point closed the connection"));
        }
        if crate::arch::timer::now_ms().saturating_sub(t0) > budget_ms {
            return Err(String::from("the access point did not answer"));
        }
        st.pump(500_000);
    }
    Ok(st.plain.drain(..n).collect())
}

#[cfg(target_arch = "aarch64")]
const PLATFORM: u64 = 0x11; // PLATFORM_LINUX_ARM
#[cfg(not(target_arch = "aarch64"))]
const PLATFORM: u64 = 0x8; // PLATFORM_LINUX_X86_64
#[cfg(target_arch = "aarch64")]
const CPU: u64 = 5; // CPU_ARM
#[cfg(not(target_arch = "aarch64"))]
const CPU: u64 = 2; // CPU_X86_64

impl Ap {
    /// TCP to the access point + the DH handshake (holds the net lock).
    pub fn connect(host: &str, port: u16) -> Result<Ap, String> {
        let _g = ns::NetShare::new();
        let mut st = Stream::connect(host, port, false)?;
        let dh = Dh::random();
        // ClientHello
        let mut nonce = [0u8; 16];
        crate::crypto::random_bytes(&mut nonce);
        let mut bi = W::new();
        bi.uint(10, 0).uint(20, 0).uint(30, PLATFORM).uint(40, SPOTIFY_VERSION);
        let mut dhh = W::new();
        dhh.bytes(10, &dh.public).uint(20, 1);
        let mut lch = W::new();
        lch.msg(10, &dhh);
        let mut hello = W::new();
        hello.msg(10, &bi).uint(30, 0).msg(50, &lch).bytes(60, &nonce).bytes(70, &[0x1e]);
        let mut acc = vec![0u8, 4];
        acc.extend_from_slice(&((2 + 4 + hello.0.len()) as u32).to_be_bytes());
        acc.extend_from_slice(&hello.0);
        if !st.send(&acc) {
            st.close();
            return Err(format!("could not send the ClientHello to {host}"));
        }
        // APResponseMessage
        let r = (|| {
            let head = read_exact(&mut st, 4, 8000)?;
            let size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
            if !(4..=65536).contains(&size) {
                return Err(format!("odd APResponse size {size}"));
            }
            let body = read_exact(&mut st, size - 4, 8000)?;
            Ok((head, body))
        })();
        let (head, body) = match r {
            Ok(x) => x,
            Err(e) => {
                st.close();
                return Err(e);
            },
        };
        acc.extend_from_slice(&head);
        acc.extend_from_slice(&body);
        if let Some(f) = pb::get_b(&body, 30) {
            st.close();
            let code = pb::get_n(f, 10).unwrap_or(0) as u32;
            return Err(format!("the access point refused the hello: {}", error_name(code)));
        }
        let Some(ch) = pb::path(&body, &[10, 10, 10]) else {
            st.close();
            return Err(String::from("no DH challenge in the APResponse"));
        };
        let gs = pb::get_b(ch, 10).unwrap_or(&[]).to_vec();
        let sig = pb::get_b(ch, 30).unwrap_or(&[]).to_vec();
        if !verify_server(&gs, &sig) {
            st.close();
            return Err(String::from("the access point's key is not signed by Spotify"));
        }
        let shared = dh.shared(&gs);
        let mut data = Vec::with_capacity(100);
        for i in 1u8..6 {
            data.extend_from_slice(&hmac_sha1(&shared, &[&acc, &[i]]));
        }
        let challenge = hmac_sha1(&data[..20], &[&acc]);
        let send_key = data[20..52].to_vec();
        let recv_key = data[52..84].to_vec();
        // ClientResponsePlaintext
        let mut dhr = W::new();
        dhr.bytes(10, &challenge);
        let mut lcr = W::new();
        lcr.msg(10, &dhr);
        let mut resp = W::new();
        resp.msg(10, &lcr).msg(20, &W::new()).msg(30, &W::new());
        let mut out = ((4 + resp.0.len()) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&resp.0);
        if !st.send(&out) {
            st.close();
            return Err(String::from("could not send the client response"));
        }
        Ok(Ap { st, enc: Shannon::new(&send_key), enc_n: 0, dec: Shannon::new(&recv_key), dec_n: 0, pending: None, host: format!("{host}:{port}") })
    }

    pub fn send(&mut self, cmd: u8, payload: &[u8]) -> bool {
        let _g = ns::NetShare::new();
        let mut buf = Vec::with_capacity(payload.len() + 7);
        buf.push(cmd);
        buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        buf.extend_from_slice(payload);
        self.enc.nonce_u32(self.enc_n);
        self.enc_n = self.enc_n.wrapping_add(1);
        self.enc.encrypt(&mut buf);
        let mut mac = [0u8; 4];
        self.enc.finish(&mut mac);
        buf.extend_from_slice(&mac);
        self.st.send(&buf)
    }

    /// The next whole packet already received, if any (Err on a bad MAC).
    pub fn take(&mut self) -> Result<Option<(u8, Vec<u8>)>, String> {
        if self.pending.is_none() && self.st.plain.len() >= 3 {
            let mut h: Vec<u8> = self.st.plain.drain(..3).collect();
            self.dec.nonce_u32(self.dec_n);
            self.dec_n = self.dec_n.wrapping_add(1);
            self.dec.decrypt(&mut h);
            self.pending = Some((h[0], u16::from_be_bytes([h[1], h[2]]) as usize));
        }
        if let Some((cmd, size)) = self.pending {
            if self.st.plain.len() >= size + 4 {
                let mut p: Vec<u8> = self.st.plain.drain(..size).collect();
                let mac: Vec<u8> = self.st.plain.drain(..4).collect();
                self.dec.decrypt(&mut p);
                let mut want = [0u8; 4];
                self.dec.finish(&mut want);
                self.pending = None;
                if want[..] != mac[..] {
                    return Err(String::from("bad MAC on a packet from the access point"));
                }
                return Ok(Some((cmd, p)));
            }
        }
        Ok(None)
    }

    /// Wait (up to `budget_ms`) for the next packet.
    pub fn recv(&mut self, budget_ms: u64) -> Result<(u8, Vec<u8>), String> {
        let t0 = crate::arch::timer::now_ms();
        loop {
            if let Some(p) = self.take()? {
                return Ok(p);
            }
            if self.st.closed {
                return Err(String::from("the access point closed the connection"));
            }
            if crate::arch::timer::now_ms().saturating_sub(t0) > budget_ms {
                return Err(String::from("the access point did not answer"));
            }
            let _g = ns::NetShare::new();
            self.st.pump(300_000);
        }
    }

    /// Take what arrived without waiting (only when no one else is mid-transaction).
    pub fn poll(&mut self) {
        if ns::net_busy_for_me() {
            return;
        }
        let _g = ns::NetShare::new();
        let mut n = 0;
        while n < 32 && self.st.pump(0) > 0 {
            n += 1;
        }
    }

    /// ClientResponseEncrypted -> APWelcome | APLoginFailed.
    pub fn login(&mut self, username: &str, auth_type: u32, auth_data: &[u8], device_id: &str) -> Result<Login, String> {
        let mut lc = W::new();
        if !username.is_empty() {
            lc.str(10, username);
        }
        lc.uint(20, auth_type as u64).bytes(30, auth_data);
        let mut si = W::new();
        si.uint(10, CPU).uint(60, 5).str(90, "librespot-lingos-0").str(100, device_id);
        let mut m = W::new();
        m.msg(10, &lc).msg(50, &si).str(70, "librespot 0.8.0");
        if !self.send(CMD_LOGIN, &m.0) {
            return Err(String::from("could not send the login"));
        }
        loop {
            let (cmd, data) = self.recv(10_000)?;
            match cmd {
                CMD_WELCOME => {
                    let user = pb::get_str(&data, 10);
                    let typ = pb::get_n(&data, 30).unwrap_or(1) as u32;
                    let auth = pb::get_b(&data, 40).unwrap_or(&[]).to_vec();
                    return Ok(Login::Welcome(user, typ, auth));
                },
                CMD_AUTH_FAILURE => {
                    return Ok(Login::Failed(pb::get_n(&data, 10).unwrap_or(0) as u32, pb::get_str(&data, 40)));
                },
                // anything else before the answer (none is expected) is skipped
                _ => {},
            }
        }
    }

    pub fn close(&mut self) {
        let _g = ns::NetShare::new();
        self.st.close();
    }
}

/// The whole sign-in: apresolve, then access points in turn until one answers
/// the login (TryAnotherAP moves on, as librespot does).
pub fn sign_in(username: &str, auth_type: u32, auth_data: &[u8], device_id: &str, log: &mut dyn FnMut(&str)) -> Result<(Ap, Login), String> {
    let aps = resolve("accesspoint");
    let mut last = String::from("no access point");
    for (h, p) in aps.iter().take(6) {
        log(&format!("access point {h}:{p}"));
        let mut ap = match Ap::connect(h, *p) {
            Ok(a) => a,
            Err(e) => {
                log(&format!("  {e}"));
                last = e;
                continue;
            },
        };
        log("  handshake ok (DH, server signature checked)");
        match ap.login(username, auth_type, auth_data, device_id) {
            Ok(Login::Failed(2, _)) => {
                log("  TryAnotherAP");
                ap.close();
                last = String::from("every access point said TryAnotherAP");
            },
            Ok(l) => return Ok((ap, l)),
            Err(e) => {
                log(&format!("  {e}"));
                ap.close();
                last = e;
            },
        }
    }
    Err(last)
}
