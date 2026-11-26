//! A small SSH **client** for LingScheduler: run a command (`ssh.run`) and
//! upload a file over SFTP (`sftp.put`) to a real OpenSSH server.
//!
//! Same suite as the server in `drivers/sshd.rs` (and the same primitives):
//! `curve25519-sha256` key exchange, `ssh-ed25519` host keys,
//! `chacha20-poly1305@openssh.com`. Authentication: an Ed25519 key from the
//! user's keyring (`publickey`), else a password from the keyring.
//!
//! The server's host key is checked against the user's pins
//! (`secrets::pinned`): a mismatch stops the connection before any secret
//! is sent and says so loudly; a host seen for the first time is NOT
//! trusted -- its key waits in `secrets::pending` for the user to confirm
//! the fingerprint in LingCal, and the run fails until then.
//!
//! Runs on a background scheduler task (its socket reads yield to the
//! desktop). Error strings never contain a secret.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use chacha20::cipher::{KeyIvInit, StreamCipher};
use chacha20::ChaCha20Legacy;
use poly1305::universal_hash::KeyInit;
use poly1305::Poly1305;

use crate::drivers::netstack;

const IDENT: &[u8] = b"SSH-2.0-LingOS_LingCal_1.0";

const MSG_DISCONNECT: u8 = 1;
const MSG_IGNORE: u8 = 2;
const MSG_UNIMPLEMENTED: u8 = 3;
const MSG_DEBUG: u8 = 4;
const MSG_SERVICE_REQUEST: u8 = 5;
const MSG_SERVICE_ACCEPT: u8 = 6;
const MSG_EXT_INFO: u8 = 7;
const MSG_KEXINIT: u8 = 20;
const MSG_NEWKEYS: u8 = 21;
const MSG_KEX_ECDH_INIT: u8 = 30;
const MSG_KEX_ECDH_REPLY: u8 = 31;
const MSG_USERAUTH_REQUEST: u8 = 50;
const MSG_USERAUTH_FAILURE: u8 = 51;
const MSG_USERAUTH_SUCCESS: u8 = 52;
const MSG_USERAUTH_BANNER: u8 = 53;
const MSG_GLOBAL_REQUEST: u8 = 80;
const MSG_REQUEST_FAILURE: u8 = 82;
const MSG_CHANNEL_OPEN: u8 = 90;
const MSG_CHANNEL_OPEN_CONFIRMATION: u8 = 91;
const MSG_CHANNEL_OPEN_FAILURE: u8 = 92;
const MSG_CHANNEL_WINDOW_ADJUST: u8 = 93;
const MSG_CHANNEL_DATA: u8 = 94;
const MSG_CHANNEL_EXTENDED_DATA: u8 = 95;
const MSG_CHANNEL_EOF: u8 = 96;
const MSG_CHANNEL_CLOSE: u8 = 97;
const MSG_CHANNEL_REQUEST: u8 = 98;
const MSG_CHANNEL_SUCCESS: u8 = 99;
const MSG_CHANNEL_FAILURE: u8 = 100;

const OUR_WINDOW: u32 = 1 << 20;
const OUR_MAXPKT: u32 = 32768;
const READ_US: u64 = 15_000_000;

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s);
}
fn put_mpint(out: &mut Vec<u8>, val: &[u8]) {
    let mut i = 0;
    while i < val.len() && val[i] == 0 {
        i += 1;
    }
    let sig = &val[i..];
    if sig.is_empty() {
        put_u32(out, 0);
    } else if sig[0] & 0x80 != 0 {
        put_u32(out, (sig.len() + 1) as u32);
        out.push(0);
        out.extend_from_slice(sig);
    } else {
        put_u32(out, sig.len() as u32);
        out.extend_from_slice(sig);
    }
}
fn get_u32(buf: &[u8], pos: &mut usize) -> Option<u32> {
    if *pos + 4 > buf.len() {
        return None;
    }
    let v = u32::from_be_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]]);
    *pos += 4;
    Some(v)
}
fn get_string<'a>(buf: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let len = get_u32(buf, pos)? as usize;
    if *pos + len > buf.len() {
        return None;
    }
    let s = &buf[*pos..*pos + len];
    *pos += len;
    Some(s)
}

fn chacha_apply(key: &[u8], seq: u32, skip_block0: bool, data: &mut [u8]) {
    let nonce = (seq as u64).to_be_bytes();
    let mut c = ChaCha20Legacy::new_from_slices(key, &nonce).expect("32-byte key, 8-byte nonce");
    if skip_block0 {
        let mut skip = [0u8; 64];
        c.apply_keystream(&mut skip);
    }
    c.apply_keystream(data);
}

fn poly_tag(k_main: &[u8], seq: u32, data: &[u8]) -> [u8; 16] {
    let nonce = (seq as u64).to_be_bytes();
    let mut c = ChaCha20Legacy::new_from_slices(k_main, &nonce).expect("key/nonce");
    let mut pk = [0u8; 32];
    c.apply_keystream(&mut pk);
    let mac = Poly1305::new_from_slice(&pk).expect("32-byte poly key");
    let tag = mac.compute_unpadded(data);
    let mut out = [0u8; 16];
    out.copy_from_slice(&tag);
    out
}

fn kdf(k_mpint: &[u8], h: &[u8; 32], letter: u8, sid: &[u8; 32]) -> [u8; 64] {
    let mut input = Vec::new();
    input.extend_from_slice(k_mpint);
    input.extend_from_slice(h);
    input.push(letter);
    input.extend_from_slice(sid);
    let mut key: Vec<u8> = crate::crypto::sha256(&input).to_vec();
    while key.len() < 64 {
        let mut i2 = Vec::new();
        i2.extend_from_slice(k_mpint);
        i2.extend_from_slice(h);
        i2.extend_from_slice(&key);
        key.extend_from_slice(&crate::crypto::sha256(&i2));
    }
    let mut out = [0u8; 64];
    out.copy_from_slice(&key[..64]);
    out
}

fn name_list_has(list: &[u8], want: &str) -> bool {
    core::str::from_utf8(list).map(|s| s.split(',').any(|n| n == want)).unwrap_or(false)
}

/// The netstack takes at most one segment (1400 bytes) per write: send a packet in pieces.
fn write_all(handle: usize, data: &[u8]) -> bool {
    for chunk in data.chunks(1400) {
        if !netstack::tcp_write_h(handle, chunk) {
            return false;
        }
    }
    true
}

fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut o = [0u8; 4];
    let mut it = s.split('.');
    for b in o.iter_mut() {
        *b = it.next()?.parse().ok()?;
    }
    it.next().is_none().then_some(o)
}

pub struct Conn {
    handle: usize,
    rx: Vec<u8>,
    send_seq: u32,
    recv_seq: u32,
    enc: bool,
    k_c2s: [u8; 64],
    k_s2c: [u8; 64],
    sid: [u8; 32],
    // the session channel
    chan: u32,
    peer_window: u64,
    peer_max: u32,
    our_left: u64,
    pub exit_status: Option<i32>,
    eof: bool,
    closed: bool,
    chan_rx: Vec<u8>,
    pub host: String,
    sftp_id: u32,
    made_dirs: Vec<String>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        crate::lingsched::secrets::wipe(&mut self.k_c2s);
        crate::lingsched::secrets::wipe(&mut self.k_s2c);
        if !self.closed {
            netstack::tcp_close_h(self.handle);
            self.closed = true;
        }
    }
}

impl Conn {
    fn ensure(&mut self, n: usize) -> Result<(), String> {
        let mut idle = 0;
        while self.rx.len() < n {
            let mut tmp = [0u8; 4096];
            let got = netstack::tcp_read_h(self.handle, &mut tmp, 1_000_000);
            if got == 0 {
                if !netstack::tcp_established_h(self.handle) && netstack::tcp_closed_h(self.handle) {
                    return Err(format!("{} closed the connection", self.host));
                }
                idle += 1;
                if idle as u64 * 1_000_000 >= READ_US {
                    return Err(format!("{} stopped answering (timeout)", self.host));
                }
                continue;
            }
            idle = 0;
            self.rx.extend_from_slice(&tmp[..got]);
        }
        Ok(())
    }

    fn recv_raw(&mut self) -> Result<Vec<u8>, String> {
        if self.enc {
            self.ensure(4)?;
            let mut len4 = [self.rx[0], self.rx[1], self.rx[2], self.rx[3]];
            chacha_apply(&self.k_s2c[32..64], self.recv_seq, false, &mut len4);
            let pkt_len = u32::from_be_bytes(len4) as usize;
            if !(8..=262144).contains(&pkt_len) {
                return Err(String::from("corrupt packet length from the server"));
            }
            self.ensure(4 + pkt_len + 16)?;
            let tag = poly_tag(&self.k_s2c[0..32], self.recv_seq, &self.rx[0..4 + pkt_len]);
            if tag[..] != self.rx[4 + pkt_len..4 + pkt_len + 16] {
                return Err(String::from("message authentication failed (tampered or corrupt stream)"));
            }
            let mut body = self.rx[4..4 + pkt_len].to_vec();
            chacha_apply(&self.k_s2c[0..32], self.recv_seq, true, &mut body);
            let pad = body[0] as usize;
            if 1 + pad > body.len() {
                return Err(String::from("bad padding from the server"));
            }
            let payload = body[1..body.len() - pad].to_vec();
            self.rx.drain(0..4 + pkt_len + 16);
            self.recv_seq = self.recv_seq.wrapping_add(1);
            Ok(payload)
        } else {
            self.ensure(4)?;
            let pkt_len = u32::from_be_bytes([self.rx[0], self.rx[1], self.rx[2], self.rx[3]]) as usize;
            if !(5..=262144).contains(&pkt_len) {
                return Err(String::from("corrupt packet length from the server"));
            }
            self.ensure(4 + pkt_len)?;
            let pad = self.rx[4] as usize;
            if 1 + pad > pkt_len {
                return Err(String::from("bad padding from the server"));
            }
            let payload = self.rx[5..4 + pkt_len - pad].to_vec();
            self.rx.drain(0..4 + pkt_len);
            self.recv_seq = self.recv_seq.wrapping_add(1);
            Ok(payload)
        }
    }

    fn send(&mut self, payload: &[u8]) -> Result<(), String> {
        let wire = if self.enc {
            let mut pad = 8 - ((1 + payload.len()) % 8);
            if pad < 4 {
                pad += 8;
            }
            let pkt_len = 1 + payload.len() + pad;
            let mut body = Vec::with_capacity(pkt_len);
            body.push(pad as u8);
            body.extend_from_slice(payload);
            let mut p = alloc::vec![0u8; pad];
            let _ = crate::crypto::random_bytes(&mut p);
            body.extend_from_slice(&p);
            let mut len4 = (pkt_len as u32).to_be_bytes();
            chacha_apply(&self.k_c2s[32..64], self.send_seq, false, &mut len4);
            chacha_apply(&self.k_c2s[0..32], self.send_seq, true, &mut body);
            let mut w = Vec::with_capacity(4 + pkt_len + 16);
            w.extend_from_slice(&len4);
            w.extend_from_slice(&body);
            let tag = poly_tag(&self.k_c2s[0..32], self.send_seq, &w);
            w.extend_from_slice(&tag);
            w
        } else {
            let mut pad = 8 - ((4 + 1 + payload.len()) % 8);
            if pad < 4 {
                pad += 8;
            }
            let pkt_len = 1 + payload.len() + pad;
            let mut w = Vec::with_capacity(4 + pkt_len);
            put_u32(&mut w, pkt_len as u32);
            w.push(pad as u8);
            w.extend_from_slice(payload);
            let mut p = alloc::vec![0u8; pad];
            let _ = crate::crypto::random_bytes(&mut p);
            w.extend_from_slice(&p);
            w
        };
        if !write_all(self.handle, &wire) {
            return Err(format!("could not send to {} (the connection stalled or closed)", self.host));
        }
        self.send_seq = self.send_seq.wrapping_add(1);
        Ok(())
    }

    /// The next message that is not transport noise.
    fn next(&mut self) -> Result<Vec<u8>, String> {
        loop {
            let m = self.recv_raw()?;
            match m.first().copied() {
                Some(MSG_IGNORE) | Some(MSG_DEBUG) | Some(MSG_EXT_INFO) | Some(MSG_UNIMPLEMENTED) => continue,
                Some(MSG_DISCONNECT) => {
                    let mut p = 5;
                    let why = get_string(&m, &mut p).map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
                    return Err(format!("{} disconnected: {why}", self.host));
                },
                Some(MSG_GLOBAL_REQUEST) => {
                    let mut p = 1;
                    let _ = get_string(&m, &mut p);
                    if m.get(p).copied().unwrap_or(0) != 0 {
                        self.send(&[MSG_REQUEST_FAILURE])?;
                    }
                    continue;
                },
                Some(_) => return Ok(m),
                None => continue,
            }
        }
    }

    /// Open a connection, run the key exchange and check the host key.
    pub fn connect(host: &str, port: u16, owner: &str) -> Result<Conn, String> {
        let ip = match parse_ip(host) {
            Some(ip) => ip,
            None => netstack::dns_resolve(host).ok_or_else(|| format!("cannot find {host} (DNS)"))?,
        };
        let handle = netstack::tcp_connect_h(ip, port, 8_000_000).ok_or_else(|| format!("{host}:{port} is unreachable (no answer on the SSH port)"))?;
        let mut c = Conn {
            handle,
            rx: Vec::new(),
            send_seq: 0,
            recv_seq: 0,
            enc: false,
            k_c2s: [0; 64],
            k_s2c: [0; 64],
            sid: [0; 32],
            chan: 0,
            peer_window: 0,
            peer_max: 0,
            our_left: OUR_WINDOW as u64,
            exit_status: None,
            eof: false,
            closed: false,
            chan_rx: Vec::new(),
            host: String::from(host),
            sftp_id: 1,
            made_dirs: Vec::new(),
        };
        // 1. versions
        let mut line = IDENT.to_vec();
        line.extend_from_slice(b"\r\n");
        if !write_all(handle, &line) {
            return Err(format!("could not talk to {host}"));
        }
        let v_s = loop {
            c.ensure(1)?;
            match c.rx.iter().position(|&b| b == b'\n') {
                Some(nl) => {
                    let mut l = c.rx[..nl].to_vec();
                    c.rx.drain(0..nl + 1);
                    if l.last() == Some(&b'\r') {
                        l.pop();
                    }
                    if l.starts_with(b"SSH-") {
                        if !l.starts_with(b"SSH-2.0-") && !l.starts_with(b"SSH-1.99-") {
                            return Err(format!("{host} does not speak SSH 2"));
                        }
                        break l;
                    }
                    // a banner line before the version: skip it
                },
                None => {
                    if c.rx.len() > 8192 {
                        return Err(format!("{host} is not an SSH server"));
                    }
                    let n = c.rx.len();
                    c.ensure(n + 1)?;
                },
            }
        };
        // 2. KEXINIT both ways
        let mut i_c = alloc::vec![MSG_KEXINIT];
        let mut cookie = [0u8; 16];
        if !crate::crypto::random_bytes(&mut cookie) {
            return Err(String::from("no hardware random source: SSH cannot run safely"));
        }
        i_c.extend_from_slice(&cookie);
        put_string(&mut i_c, b"curve25519-sha256,curve25519-sha256@libssh.org");
        put_string(&mut i_c, b"ssh-ed25519");
        put_string(&mut i_c, b"chacha20-poly1305@openssh.com");
        put_string(&mut i_c, b"chacha20-poly1305@openssh.com");
        put_string(&mut i_c, b"hmac-sha2-256,hmac-sha2-512");
        put_string(&mut i_c, b"hmac-sha2-256,hmac-sha2-512");
        put_string(&mut i_c, b"none");
        put_string(&mut i_c, b"none");
        put_string(&mut i_c, b"");
        put_string(&mut i_c, b"");
        i_c.push(0);
        put_u32(&mut i_c, 0);
        c.send(&i_c)?;
        let i_s = c.next()?;
        if i_s.first() != Some(&MSG_KEXINIT) {
            return Err(format!("{host}: unexpected message before key exchange"));
        }
        {
            let mut p = 17;
            let kex = get_string(&i_s, &mut p).unwrap_or(b"");
            let hk = get_string(&i_s, &mut p).unwrap_or(b"");
            let ec = get_string(&i_s, &mut p).unwrap_or(b"");
            let es = get_string(&i_s, &mut p).unwrap_or(b"");
            if !name_list_has(kex, "curve25519-sha256") && !name_list_has(kex, "curve25519-sha256@libssh.org") {
                return Err(format!("{host} offers no curve25519 key exchange"));
            }
            if !name_list_has(hk, "ssh-ed25519") {
                return Err(format!("{host} has no Ed25519 host key (LingOS checks Ed25519 host keys only)"));
            }
            if !name_list_has(ec, "chacha20-poly1305@openssh.com") || !name_list_has(es, "chacha20-poly1305@openssh.com") {
                return Err(format!("{host} does not offer chacha20-poly1305@openssh.com"));
            }
        }
        // 3. curve25519-sha256
        let mut eph = [0u8; 32];
        if !crate::crypto::random_bytes(&mut eph) {
            return Err(String::from("no hardware random source: SSH cannot run safely"));
        }
        let q_c = crate::crypto::x25519_public(eph);
        let mut init = alloc::vec![MSG_KEX_ECDH_INIT];
        put_string(&mut init, &q_c);
        c.send(&init)?;
        let reply = c.next()?;
        if reply.first() != Some(&MSG_KEX_ECDH_REPLY) {
            return Err(format!("{host}: no key-exchange reply"));
        }
        let mut p = 1;
        let k_s = get_string(&reply, &mut p).ok_or("bad key-exchange reply")?.to_vec();
        let q_s = get_string(&reply, &mut p).ok_or("bad key-exchange reply")?.to_vec();
        let sig_blob = get_string(&reply, &mut p).ok_or("bad key-exchange reply")?.to_vec();
        let q_s: [u8; 32] = q_s.try_into().map_err(|_| String::from("bad server key share"))?;
        let k = crate::crypto::x25519(eph, q_s);
        crate::lingsched::secrets::wipe(&mut eph);
        if k.iter().all(|&b| b == 0) {
            return Err(String::from("bad server key share"));
        }
        let mut hb = Vec::new();
        put_string(&mut hb, IDENT);
        put_string(&mut hb, &v_s);
        put_string(&mut hb, &i_c);
        put_string(&mut hb, &i_s);
        put_string(&mut hb, &k_s);
        put_string(&mut hb, &q_c);
        put_string(&mut hb, &q_s);
        put_mpint(&mut hb, &k);
        let h = crate::crypto::sha256(&hb);
        // the host key: ssh-ed25519 blob, signature over H
        let (hk_pub, sig) = {
            let mut a = 0;
            let alg = get_string(&k_s, &mut a).unwrap_or(b"");
            let pk = get_string(&k_s, &mut a).unwrap_or(b"");
            let mut b = 0;
            let salg = get_string(&sig_blob, &mut b).unwrap_or(b"");
            let sg = get_string(&sig_blob, &mut b).unwrap_or(b"");
            if alg != b"ssh-ed25519" || salg != b"ssh-ed25519" || pk.len() != 32 || sg.len() != 64 {
                return Err(format!("{host} sent a host key that is not Ed25519"));
            }
            (pk.to_vec(), sg.to_vec())
        };
        if !crate::ed25519::verify(&hk_pub, &h, &sig) {
            return Err(format!("{host}: the host key's signature does not verify -- someone may be in the middle; refusing"));
        }
        let fp = crate::lingsched::secrets::fingerprint(&k_s);
        match crate::lingsched::secrets::pinned(owner, host, port) {
            Some(pin) if pin == k_s => {},
            Some(pin) => {
                return Err(format!(
                    "HOST KEY MISMATCH for {host}:{port}: pinned {}, the server now shows {fp}. This can be an attack (or a reinstalled server). Nothing was sent. If you know the server changed, remove the old pin in LingCal > Keyring > Hosts.",
                    crate::lingsched::secrets::fingerprint(&pin)
                ));
            },
            None => {
                crate::lingsched::secrets::add_pending(owner, host, port, &k_s);
                return Err(format!("unknown host {host}:{port} ({fp}) -- confirm its fingerprint in LingCal > Keyring > Hosts, then run again"));
            },
        }
        // 4. NEWKEYS
        let mut k_mp = Vec::new();
        put_mpint(&mut k_mp, &k);
        c.sid = h;
        c.k_c2s = kdf(&k_mp, &h, b'C', &h);
        c.k_s2c = kdf(&k_mp, &h, b'D', &h);
        crate::lingsched::secrets::wipe(&mut k_mp);
        c.send(&[MSG_NEWKEYS])?;
        let nk = c.next()?;
        if nk.first() != Some(&MSG_NEWKEYS) {
            return Err(format!("{host}: key exchange did not finish"));
        }
        c.enc = true;
        Ok(c)
    }

    /// Sign in: the Ed25519 key when given, else the password.
    pub fn auth(&mut self, user: &str, key_seed: Option<&[u8; 32]>, password: Option<&[u8]>) -> Result<(), String> {
        let mut req = alloc::vec![MSG_SERVICE_REQUEST];
        put_string(&mut req, b"ssh-userauth");
        self.send(&req)?;
        let acc = self.next()?;
        if acc.first() != Some(&MSG_SERVICE_ACCEPT) {
            return Err(String::from("the server refused the sign-in service"));
        }
        let mut tried = Vec::new();
        if let Some(seed) = key_seed {
            tried.push("key");
            let pk = crate::ed25519::public_from_seed(seed);
            let blob = crate::lingsched::secrets::ssh_pub_blob(&pk);
            let mut data = Vec::new();
            put_string(&mut data, &self.sid);
            data.push(MSG_USERAUTH_REQUEST);
            put_string(&mut data, user.as_bytes());
            put_string(&mut data, b"ssh-connection");
            put_string(&mut data, b"publickey");
            data.push(1);
            put_string(&mut data, b"ssh-ed25519");
            put_string(&mut data, &blob);
            let sig = crate::ed25519::sign(seed, &data);
            let mut sb = Vec::new();
            put_string(&mut sb, b"ssh-ed25519");
            put_string(&mut sb, &sig);
            let mut m = alloc::vec![MSG_USERAUTH_REQUEST];
            put_string(&mut m, user.as_bytes());
            put_string(&mut m, b"ssh-connection");
            put_string(&mut m, b"publickey");
            m.push(1);
            put_string(&mut m, b"ssh-ed25519");
            put_string(&mut m, &blob);
            put_string(&mut m, &sb);
            self.send(&m)?;
            if self.auth_result()? {
                return Ok(());
            }
        }
        if let Some(pw) = password {
            tried.push("password");
            let mut m = alloc::vec![MSG_USERAUTH_REQUEST];
            put_string(&mut m, user.as_bytes());
            put_string(&mut m, b"ssh-connection");
            put_string(&mut m, b"password");
            m.push(0);
            put_string(&mut m, pw);
            self.send(&m)?;
            crate::lingsched::secrets::wipe(&mut m);
            if self.auth_result()? {
                return Ok(());
            }
        }
        Err(format!(
            "{} refused the sign-in as {user} ({}) -- is the public key in ~/.ssh/authorized_keys there?",
            self.host,
            if tried.is_empty() { String::from("no key or password given") } else { tried.join(", then ") }
        ))
    }

    fn auth_result(&mut self) -> Result<bool, String> {
        loop {
            let m = self.next()?;
            match m.first().copied() {
                Some(MSG_USERAUTH_SUCCESS) => return Ok(true),
                Some(MSG_USERAUTH_FAILURE) => return Ok(false),
                Some(MSG_USERAUTH_BANNER) => continue,
                Some(60) => return Ok(false), // a password-change request: not supported
                Some(t) => return Err(format!("unexpected message {t} during sign-in")),
                None => continue,
            }
        }
    }

    fn open_session(&mut self) -> Result<(), String> {
        let mut m = alloc::vec![MSG_CHANNEL_OPEN];
        put_string(&mut m, b"session");
        put_u32(&mut m, 0);
        put_u32(&mut m, OUR_WINDOW);
        put_u32(&mut m, OUR_MAXPKT);
        self.send(&m)?;
        loop {
            let r = self.next()?;
            match r.first().copied() {
                Some(MSG_CHANNEL_OPEN_CONFIRMATION) => {
                    let mut p = 5;
                    self.chan = get_u32(&r, &mut p).ok_or("bad channel confirmation")?;
                    self.peer_window = get_u32(&r, &mut p).ok_or("bad channel confirmation")? as u64;
                    self.peer_max = get_u32(&r, &mut p).ok_or("bad channel confirmation")?.clamp(1024, 1 << 20);
                    return Ok(());
                },
                Some(MSG_CHANNEL_OPEN_FAILURE) => return Err(format!("{} refused a session channel", self.host)),
                _ => continue,
            }
        }
    }

    /// Ask for exec / subsystem and wait for the answer.
    fn chan_request(&mut self, kind: &[u8], arg: &[u8]) -> Result<(), String> {
        let mut m = alloc::vec![MSG_CHANNEL_REQUEST];
        put_u32(&mut m, self.chan);
        put_string(&mut m, kind);
        m.push(1);
        put_string(&mut m, arg);
        self.send(&m)?;
        loop {
            let r = self.next()?;
            match r.first().copied() {
                Some(MSG_CHANNEL_SUCCESS) => return Ok(()),
                Some(MSG_CHANNEL_FAILURE) => return Err(format!("{} refused `{}`", self.host, String::from_utf8_lossy(kind))),
                _ => self.channel_msg(&r)?,
            }
        }
    }

    /// Handle a channel message that is not the answer being waited for.
    fn channel_msg(&mut self, r: &[u8]) -> Result<(), String> {
        let mut p = 1;
        match r.first().copied() {
            Some(MSG_CHANNEL_WINDOW_ADJUST) => {
                let _ = get_u32(r, &mut p);
                self.peer_window += get_u32(r, &mut p).unwrap_or(0) as u64;
            },
            Some(MSG_CHANNEL_DATA) => {
                let _ = get_u32(r, &mut p);
                let d = get_string(r, &mut p).unwrap_or(b"");
                self.our_left = self.our_left.saturating_sub(d.len() as u64);
                if self.chan_rx.len() < (4 << 20) {
                    self.chan_rx.extend_from_slice(d);
                }
                self.refill_window()?;
            },
            Some(MSG_CHANNEL_EXTENDED_DATA) => {
                let _ = get_u32(r, &mut p);
                let _ = get_u32(r, &mut p);
                let d = get_string(r, &mut p).unwrap_or(b"");
                self.our_left = self.our_left.saturating_sub(d.len() as u64);
                if self.chan_rx.len() < (4 << 20) {
                    self.chan_rx.extend_from_slice(d); // stderr goes with stdout
                }
                self.refill_window()?;
            },
            Some(MSG_CHANNEL_EOF) => self.eof = true,
            Some(MSG_CHANNEL_CLOSE) => {
                self.eof = true;
                if !self.closed_chan_sent() {
                    let mut m = alloc::vec![MSG_CHANNEL_CLOSE];
                    put_u32(&mut m, self.chan);
                    let _ = self.send(&m);
                }
                self.peer_window = 0;
                self.chan = u32::MAX;
            },
            Some(MSG_CHANNEL_REQUEST) => {
                let _ = get_u32(r, &mut p);
                let kind = get_string(r, &mut p).unwrap_or(b"").to_vec();
                let want = r.get(p).copied().unwrap_or(0) != 0;
                p += 1;
                if kind == b"exit-status" {
                    self.exit_status = get_u32(r, &mut p).map(|v| v as i32);
                } else if kind == b"exit-signal" {
                    self.exit_status = Some(-1);
                }
                if want && self.chan != u32::MAX {
                    let mut m = alloc::vec![MSG_CHANNEL_FAILURE];
                    put_u32(&mut m, self.chan);
                    self.send(&m)?;
                }
            },
            _ => {},
        }
        Ok(())
    }

    fn closed_chan_sent(&self) -> bool {
        self.chan == u32::MAX
    }

    fn refill_window(&mut self) -> Result<(), String> {
        if self.our_left < (OUR_WINDOW / 2) as u64 && self.chan != u32::MAX {
            let add = OUR_WINDOW as u64 - self.our_left;
            let mut m = alloc::vec![MSG_CHANNEL_WINDOW_ADJUST];
            put_u32(&mut m, self.chan);
            put_u32(&mut m, add as u32);
            self.send(&m)?;
            self.our_left += add;
        }
        Ok(())
    }

    fn chan_send(&mut self, mut data: &[u8]) -> Result<(), String> {
        while !data.is_empty() {
            while self.peer_window == 0 {
                if self.chan == u32::MAX {
                    return Err(format!("{} closed the channel", self.host));
                }
                let r = self.next()?;
                self.channel_msg(&r)?;
            }
            let n = data.len().min(self.peer_window as usize).min(self.peer_max as usize - 64);
            let mut m = alloc::vec![MSG_CHANNEL_DATA];
            put_u32(&mut m, self.chan);
            put_string(&mut m, &data[..n]);
            self.send(&m)?;
            self.peer_window -= n as u64;
            data = &data[n..];
        }
        Ok(())
    }

    /// Run a command; returns (exit status, output -- stdout and stderr, up to 64 KiB).
    pub fn exec(&mut self, cmd: &str) -> Result<(i32, String), String> {
        self.open_session()?;
        self.chan_request(b"exec", cmd.as_bytes())?;
        let t0 = crate::arch::timer::now_ms();
        while self.chan != u32::MAX {
            if crate::arch::timer::now_ms().saturating_sub(t0) > 10 * 60_000 {
                return Err(String::from("the command ran longer than 10 minutes"));
            }
            let r = self.next()?;
            self.channel_msg(&r)?;
        }
        let mut out = core::mem::take(&mut self.chan_rx);
        out.truncate(65536);
        Ok((self.exit_status.unwrap_or(-1), String::from_utf8_lossy(&out).into_owned()))
    }

    // ── SFTP (version 3) ──────────────────────────────────────────────────────

    fn sftp_send(&mut self, kind: u8, body: &[u8]) -> Result<(), String> {
        let mut m = Vec::with_capacity(5 + body.len());
        put_u32(&mut m, (1 + body.len()) as u32);
        m.push(kind);
        m.extend_from_slice(body);
        self.chan_send(&m)
    }

    fn sftp_recv(&mut self) -> Result<(u8, Vec<u8>), String> {
        loop {
            if self.chan_rx.len() >= 4 {
                let n = u32::from_be_bytes([self.chan_rx[0], self.chan_rx[1], self.chan_rx[2], self.chan_rx[3]]) as usize;
                if n == 0 || n > (1 << 20) {
                    return Err(String::from("corrupt SFTP reply"));
                }
                if self.chan_rx.len() >= 4 + n {
                    let kind = self.chan_rx[4];
                    let body = self.chan_rx[5..4 + n].to_vec();
                    self.chan_rx.drain(0..4 + n);
                    return Ok((kind, body));
                }
            }
            if self.chan == u32::MAX || self.eof {
                return Err(format!("{} ended the SFTP session", self.host));
            }
            let r = self.next()?;
            self.channel_msg(&r)?;
        }
    }

    /// The STATUS for request `id`: Ok, or the server's message.
    fn sftp_status(&mut self, want_id: u32) -> Result<(), String> {
        loop {
            let (kind, body) = self.sftp_recv()?;
            let mut p = 0;
            let id = get_u32(&body, &mut p).unwrap_or(u32::MAX);
            if id != want_id {
                continue;
            }
            if kind != 101 {
                return Err(String::from("unexpected SFTP reply"));
            }
            let code = get_u32(&body, &mut p).unwrap_or(4);
            if code == 0 {
                return Ok(());
            }
            let msg = get_string(&body, &mut p).map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
            return Err(format!("SFTP error {code}: {msg}"));
        }
    }

    /// Upload `data` to `remote` (a file path); missing folders are made.
    pub fn sftp_put(&mut self, data: &[u8], remote: &str) -> Result<(), String> {
        self.sftp_start()?;
        self.sftp_put_file(data, remote)
    }

    /// Open the SFTP subsystem (once per connection; then `sftp_put_file` per file).
    pub fn sftp_start(&mut self) -> Result<(), String> {
        self.open_session()?;
        self.chan_request(b"subsystem", b"sftp")?;
        let mut init = Vec::new();
        put_u32(&mut init, 3);
        self.sftp_send(1, &init)?;
        let (k, _) = self.sftp_recv()?;
        if k != 2 {
            return Err(String::from("the server's SFTP did not start"));
        }
        self.sftp_id = 1;
        self.made_dirs.clear();
        Ok(())
    }

    /// Upload one file over the started SFTP session; missing folders are made.
    pub fn sftp_put_file(&mut self, data: &[u8], remote: &str) -> Result<(), String> {
        let mut id = self.sftp_id;
        // mkdir -p (failures are fine: the folder exists)
        let remote = remote.strip_prefix("~/").unwrap_or(remote);
        if let Some(slash) = remote.rfind('/') {
            let dir = &remote[..slash];
            let mut acc = String::new();
            for (i, part) in dir.split('/').enumerate() {
                if i == 0 && part.is_empty() {
                    acc.push('/');
                    continue;
                }
                if part.is_empty() {
                    continue;
                }
                if !acc.is_empty() && !acc.ends_with('/') {
                    acc.push('/');
                }
                acc.push_str(part);
                if self.made_dirs.contains(&acc) {
                    continue;
                }
                let mut b = Vec::new();
                put_u32(&mut b, id);
                put_string(&mut b, acc.as_bytes());
                put_u32(&mut b, 0);
                self.sftp_send(14, &b)?;
                let _ = self.sftp_status(id);
                self.made_dirs.push(acc.clone());
                id += 1;
            }
        }
        // open for writing (create, truncate), mode 0644
        let mut b = Vec::new();
        put_u32(&mut b, id);
        put_string(&mut b, remote.as_bytes());
        put_u32(&mut b, 0x02 | 0x08 | 0x10);
        put_u32(&mut b, 0x04);
        put_u32(&mut b, 0o644);
        self.sftp_send(3, &b)?;
        let handle = loop {
            let (kind, body) = self.sftp_recv()?;
            let mut p = 0;
            if get_u32(&body, &mut p) != Some(id) {
                continue;
            }
            match kind {
                102 => break get_string(&body, &mut p).ok_or("bad SFTP handle")?.to_vec(),
                101 => {
                    let _ = get_u32(&body, &mut p);
                    let msg = get_string(&body, &mut p).map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
                    return Err(format!("cannot create {remote} there: {msg}"));
                },
                _ => return Err(String::from("unexpected SFTP reply")),
            }
        };
        id += 1;
        // the writes, up to 16 in flight
        const CHUNK: usize = 16 * 1024;
        let mut off = 0usize;
        let mut inflight: Vec<u32> = Vec::new();
        while off < data.len() || !inflight.is_empty() {
            while off < data.len() && inflight.len() < 16 {
                let n = (data.len() - off).min(CHUNK);
                let mut w = Vec::with_capacity(n + 32 + handle.len());
                put_u32(&mut w, id);
                put_string(&mut w, &handle);
                w.extend_from_slice(&(off as u64).to_be_bytes());
                put_string(&mut w, &data[off..off + n]);
                self.sftp_send(6, &w)?;
                inflight.push(id);
                id += 1;
                off += n;
            }
            let first = inflight.remove(0);
            self.sftp_status(first)?;
        }
        let mut cl = Vec::new();
        put_u32(&mut cl, id);
        put_string(&mut cl, &handle);
        self.sftp_send(4, &cl)?;
        self.sftp_status(id)?;
        self.sftp_id = id + 1;
        Ok(())
    }

    pub fn close(mut self) {
        if self.chan != u32::MAX && self.peer_max != 0 {
            let mut m = alloc::vec![MSG_CHANNEL_CLOSE];
            put_u32(&mut m, self.chan);
            let _ = self.send(&m);
        }
        let mut d = alloc::vec![MSG_DISCONNECT];
        put_u32(&mut d, 11);
        put_string(&mut d, b"bye");
        put_string(&mut d, b"");
        let _ = self.send(&d);
        netstack::tcp_close_h(self.handle);
        self.closed = true;
    }
}

/// Connect + sign in for the owner of a job (`key`: an Ed25519 seed).
pub fn session(owner: &str, user: &str, host: &str, port: u16, key: Option<&[u8; 32]>, password: Option<&[u8]>) -> Result<Conn, String> {
    let mut c = Conn::connect(host, port, owner)?;
    c.auth(user, key, password)?;
    Ok(c)
}

