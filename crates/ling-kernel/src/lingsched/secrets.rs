//! Named secrets for automations, sealed by the user's keyring, plus the
//! pinned SSH host keys.
//!
//! * `secrets/<user>` (lingfs, top level, an account store `vfs` refuses to
//!   apps, SFTP and SSH commands) holds one blob sealed with the keyring's
//!   `lingcal-secrets` key (`keyring::seal`, ChaCha20-Poly1305, bound to the
//!   path). Inside: `name|kind|hex(value)|created` lines. Kinds: `password`,
//!   `token`, `ssh-key` (a 32-byte Ed25519 seed, generated here and never
//!   shown -- only its public key is). Nothing opens while the keyring is
//!   locked, so a job that names a secret waits for its owner to sign in.
//! * `secrets/<user>-hosts` holds the pinned host keys (`host:port blob-b64 when`):
//!   public data, but kept out of reach of apps so a program cannot swap a pin.
//!   An unknown host is NOT trusted silently: the run fails and the key
//!   waits in `pending` until the user confirms its fingerprint in LingCal.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const PURPOSE: &str = "lingcal-secrets";

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Password,
    Token,
    SshKey,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Password => "password",
            Kind::Token => "token",
            Kind::SshKey => "ssh-key",
        }
    }
    pub fn from_word(s: &str) -> Option<Kind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "password" | "pass" => Some(Kind::Password),
            "token" | "api" => Some(Kind::Token),
            "ssh-key" | "ssh" | "key" | "ed25519" => Some(Kind::SshKey),
            _ => None,
        }
    }
}

pub struct Secret {
    pub name: String,
    pub kind: Kind,
    pub value: Vec<u8>,
    pub created: u64,
}

impl Drop for Secret {
    fn drop(&mut self) {
        wipe(&mut self.value);
    }
}

pub fn wipe(v: &mut [u8]) {
    for b in v.iter_mut() {
        unsafe { core::ptr::write_volatile(b, 0) };
    }
}

fn path(user: &str) -> String {
    format!("/secrets/{user}")
}

fn hosts_path(user: &str) -> String {
    format!("/secrets/{user}-hosts")
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 48 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == ' ') && !n.starts_with(' ')
}

fn valid_user(u: &str) -> bool {
    !u.is_empty() && u.len() <= 32 && !u.contains('/') && !u.contains('.') && !u.contains(' ')
}

fn read_lingfs(p: &str) -> Option<Vec<u8>> {
    if !crate::fs::lingfs::is_mounted() || crate::fs::lingfs::path_kind(p) != Some(false) {
        return None;
    }
    let n = crate::fs::lingfs::file_size(p)?;
    let mut buf = alloc::vec![0u8; n.max(1)];
    match crate::fs::lingfs::read_file_all(p, &mut buf) {
        Ok(Some(k)) => {
            buf.truncate(k);
            Some(buf)
        },
        _ => None,
    }
}

/// The keyring is open for exactly this user?
pub fn available(user: &str) -> bool {
    crate::keyring::user().as_deref() == Some(user)
}

fn load(user: &str) -> Result<Vec<Secret>, String> {
    if !valid_user(user) {
        return Err(String::from("no such user"));
    }
    if !available(user) {
        return Err(format!("the keyring of {user} is locked (sign in as {user})"));
    }
    let p = path(user);
    let Some(blob) = read_lingfs(&p) else { return Ok(Vec::new()) };
    let Some(mut plain) = crate::keyring::open(PURPOSE, p.as_bytes(), &blob) else {
        return Err(String::from("the secrets file does not open with this keyring"));
    };
    let mut out = Vec::new();
    if let Ok(text) = core::str::from_utf8(&plain) {
        for l in text.lines() {
            let f: Vec<&str> = l.split('|').collect();
            if f.len() < 3 {
                continue;
            }
            let (Some(kind), Some(value)) = (Kind::from_word(f[1]), unhex(f[2])) else { continue };
            out.push(Secret { name: String::from(f[0]), kind, value, created: f.get(3).and_then(|v| v.parse().ok()).unwrap_or(0) });
        }
    }
    wipe(&mut plain);
    Ok(out)
}

fn store(user: &str, list: &[Secret]) -> Result<(), String> {
    let mut text = String::new();
    for s in list {
        text.push_str(&format!("{}|{}|{}|{}\n", s.name, s.kind.word(), hex(&s.value), s.created));
    }
    let p = path(user);
    let sealed = crate::keyring::seal(PURPOSE, p.as_bytes(), text.as_bytes());
    // the plaintext copy goes before anything can fail
    let mut t = text.into_bytes();
    wipe(&mut t);
    let sealed = sealed.ok_or_else(|| String::from("the keyring is locked"))?;
    crate::fs::lingfs::write_path_any(&p, &sealed).map_err(|_| String::from("could not write the secrets file"))
}

/// `name|kind|info`: info is the public key line for an SSH key, else "".
pub fn list(user: &str) -> Result<Vec<(String, Kind, String)>, String> {
    Ok(load(user)?
        .iter()
        .map(|s| {
            let info = if s.kind == Kind::SshKey { pub_line_of(s, user) } else { String::new() };
            (s.name.clone(), s.kind, info)
        })
        .collect())
}

pub fn add(user: &str, name: &str, kind: Kind, value: &[u8]) -> Result<(), String> {
    if !valid_name(name) {
        return Err(String::from("a name is 1-48 letters, digits, space, - _ ."));
    }
    if value.is_empty() || value.len() > 4096 {
        return Err(String::from("the value is empty or too long"));
    }
    let mut l = load(user)?;
    if l.iter().any(|s| s.name == name) {
        return Err(format!("there is already a secret called {name}"));
    }
    let v = if kind == Kind::SshKey {
        // an imported key is a 32-byte seed as 64 hex digits
        match core::str::from_utf8(value).ok().and_then(|t| unhex(t.trim())) {
            Some(s) if s.len() == 32 => s,
            _ => return Err(String::from("an SSH key is made here (Generate), not typed in")),
        }
    } else {
        value.to_vec()
    };
    l.push(Secret { name: String::from(name), kind, value: v, created: crate::clock::unix_now() });
    store(user, &l)
}

pub fn rename(user: &str, old: &str, new: &str) -> Result<(), String> {
    if !valid_name(new) {
        return Err(String::from("a name is 1-48 letters, digits, space, - _ ."));
    }
    let mut l = load(user)?;
    if l.iter().any(|s| s.name == new) {
        return Err(format!("there is already a secret called {new}"));
    }
    let s = l.iter_mut().find(|s| s.name == old).ok_or_else(|| format!("no secret called {old}"))?;
    s.name = String::from(new);
    store(user, &l)
}

pub fn delete(user: &str, name: &str) -> Result<(), String> {
    let mut l = load(user)?;
    let n = l.len();
    l.retain(|s| s.name != name);
    if l.len() == n {
        return Err(format!("no secret called {name}"));
    }
    store(user, &l)
}

/// Make a new Ed25519 key pair; returns its public key line (the private
/// half never leaves the sealed file).
pub fn generate_ssh_key(user: &str, name: &str) -> Result<String, String> {
    if !valid_name(name) {
        return Err(String::from("a name is 1-48 letters, digits, space, - _ ."));
    }
    let mut l = load(user)?;
    if l.iter().any(|s| s.name == name) {
        return Err(format!("there is already a secret called {name}"));
    }
    let mut seed = [0u8; 32];
    if !crate::crypto::random_bytes(&mut seed) {
        return Err(String::from("no hardware random source: cannot make a key safely"));
    }
    let s = Secret { name: String::from(name), kind: Kind::SshKey, value: seed.to_vec(), created: crate::clock::unix_now() };
    wipe(&mut seed);
    let line = pub_line_of(&s, user);
    l.push(s);
    store(user, &l)?;
    Ok(line)
}

pub fn ssh_pub_blob(pk: &[u8; 32]) -> Vec<u8> {
    let mut v = Vec::new();
    put_string(&mut v, b"ssh-ed25519");
    put_string(&mut v, pk);
    v
}

fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

fn pub_line_of(s: &Secret, user: &str) -> String {
    let Ok(seed) = <[u8; 32]>::try_from(&s.value[..]) else { return String::new() };
    let pk = crate::ed25519::public_from_seed(&seed);
    format!("ssh-ed25519 {} {}-{}@lingos", b64(&ssh_pub_blob(&pk)), user, s.name.replace(' ', "_"))
}

/// One secret's value for a run (the caller wipes it when done).
pub fn get(user: &str, name: &str) -> Result<(Kind, Vec<u8>), String> {
    let l = load(user)?;
    let s = l.iter().find(|s| s.name == name).ok_or_else(|| format!("no secret called \"{name}\" in the keyring of {user} (add it in LingCal > Keyring)"))?;
    Ok((s.kind, s.value.clone()))
}

// ── known hosts ──────────────────────────────────────────────────────────────

/// SHA256:<base64> as OpenSSH prints it.
pub fn fingerprint(blob: &[u8]) -> String {
    let h = crate::crypto::sha256(blob);
    let mut s = b64(&h);
    while s.ends_with('=') {
        s.pop();
    }
    format!("SHA256:{s}")
}

fn host_key(host: &str, port: u16) -> String {
    format!("{}:{port}", host.to_ascii_lowercase())
}

/// (host:port, fingerprint, when)
pub fn hosts(user: &str) -> Vec<(String, String, u64)> {
    let Some(b) = read_lingfs(&hosts_path(user)) else { return Vec::new() };
    let t = String::from_utf8_lossy(&b).into_owned();
    t.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let blob = unb64(f.get(1)?)?;
            Some((String::from(f[0]), fingerprint(&blob), f.get(2).and_then(|v| v.parse().ok()).unwrap_or(0)))
        })
        .collect()
}

/// The pinned key blob for a host, if any.
pub fn pinned(user: &str, host: &str, port: u16) -> Option<Vec<u8>> {
    let b = read_lingfs(&hosts_path(user))?;
    let t = String::from_utf8_lossy(&b).into_owned();
    let k = host_key(host, port);
    t.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.first() == Some(&k.as_str()) {
            unb64(f.get(1)?)
        } else {
            None
        }
    })
}

fn write_hosts(user: &str, lines: &[String]) -> bool {
    let mut t = String::new();
    for l in lines {
        t.push_str(l);
        t.push('\n');
    }
    crate::fs::lingfs::write_path_any(&hosts_path(user), t.as_bytes()).is_ok()
}

pub fn pin(user: &str, host: &str, port: u16, blob: &[u8]) -> bool {
    if !valid_user(user) {
        return false;
    }
    let k = host_key(host, port);
    let mut lines: Vec<String> = read_lingfs(&hosts_path(user)).map(|b| String::from_utf8_lossy(&b).lines().map(String::from).collect()).unwrap_or_default();
    lines.retain(|l| l.split_whitespace().next() != Some(k.as_str()));
    lines.push(format!("{k} {} {}", b64(blob), crate::clock::unix_now()));
    write_hosts(user, &lines)
}

pub fn forget(user: &str, hostport: &str) -> bool {
    let mut lines: Vec<String> = read_lingfs(&hosts_path(user)).map(|b| String::from_utf8_lossy(&b).lines().map(String::from).collect()).unwrap_or_default();
    let n = lines.len();
    lines.retain(|l| l.split_whitespace().next() != Some(hostport));
    lines.len() != n && write_hosts(user, &lines)
}

/// Host keys seen for the first time, waiting for the user's yes:
/// (user, host, port, blob).
static mut PENDING: Vec<(String, String, u16, Vec<u8>)> = Vec::new();

pub fn add_pending(user: &str, host: &str, port: u16, blob: &[u8]) {
    let p = unsafe { &mut *(&raw mut PENDING) };
    p.retain(|(u, h, pt, _)| !(u == user && h.eq_ignore_ascii_case(host) && *pt == port));
    if p.len() >= 16 {
        p.remove(0);
    }
    p.push((String::from(user), String::from(host), port, blob.to_vec()));
}

/// (host:port, fingerprint) waiting for `user`.
pub fn pending(user: &str) -> Vec<(String, String)> {
    let p = unsafe { &*(&raw const PENDING) };
    p.iter().filter(|(u, ..)| u == user).map(|(_, h, pt, b)| (host_key(h, *pt), fingerprint(b))).collect()
}

/// The user said yes to this fingerprint: pin it (the fingerprint must match
/// what was shown, so a key swapped in meanwhile is not trusted by accident).
pub fn confirm(user: &str, hostport: &str, fp: &str) -> Result<(), String> {
    let p = unsafe { &mut *(&raw mut PENDING) };
    let i = p.iter().position(|(u, h, pt, b)| u == user && host_key(h, *pt) == hostport && fingerprint(b) == fp).ok_or_else(|| String::from("that host key is no longer waiting (run the job again)"))?;
    let (_, h, pt, b) = p.remove(i);
    if pin(user, &h, pt, &b) {
        Ok(())
    } else {
        Err(String::from("could not save the host key"))
    }
}

pub fn reject(user: &str, hostport: &str) {
    let p = unsafe { &mut *(&raw mut PENDING) };
    p.retain(|(u, h, pt, _)| !(u == user && host_key(h, *pt) == hostport));
}

// ── encoding ─────────────────────────────────────────────────────────────────

pub fn hex(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(D[(x >> 4) as usize] as char);
        s.push(D[(x & 15) as usize] as char);
    }
    s
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let v = |c: u8| -> Option<u8> {
        Some(match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        })
    };
    let mut out = Vec::with_capacity(b.len() / 2);
    for i in (0..b.len()).step_by(2) {
        out.push((v(b[i])? << 4) | v(b[i + 1])?);
    }
    Some(out)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64(data: &[u8]) -> String {
    let mut s = String::with_capacity((data.len() + 2) / 3 * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(B64[(n >> 18) as usize & 63] as char);
        s.push(B64[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    s
}

pub fn unb64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = B64.iter().position(|&x| x == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}
