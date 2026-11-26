//! One user's Bluetooth settings: the on/off switch, the bonds (link keys)
//! and the block list. Plain text here; the kernel seals it with the user's
//! keyring (ChaCha20-Poly1305 under a key derived from the login password)
//! before it touches the disk, in `home/<user>/.bluetooth`.
//!
//! ```text
//! power on
//! bond AA:BB:CC:DD:EE:FF <key hex> <key type> <cod hex> <flags> <name...>
//! block AA:BB:CC:DD:EE:FF <name...>
//! blockname <pattern>
//! showblocked on
//! ```
//! flags: `k` may type (keyboard), `p` may point (mouse), `a` audio, `g`
//! gamepad, `L` Bluetooth LE (the key is the LTK), `r` the LE identity address
//! is random static, `-` none.
//! ```text
//! irk AA:BB:CC:DD:EE:FF <irk hex>        (an LE bond's identity resolving key)
//! hidmap AA:BB:CC:DD:EE:FF <hash hex>    (its Report Map as seen at pairing)
//! block AA:BB:CC:DD:EE:FF irk=<hex> <name...>   (irk= optional: an LE identity)
//! ```

use crate::{addr_str, parse_addr, BdAddr};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub const MAX_BONDS: usize = 32;
pub const MAX_BLOCKED: usize = 256;

/// Link key types (HCI Link Key Notification).
pub const KEY_UNAUTH_P256: u8 = 0x07;
pub const KEY_AUTH_P256: u8 = 0x08;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Bond {
    pub addr: BdAddr,
    pub key: [u8; 16],
    pub key_type: u8,
    pub cod: u32,
    /// Approved at pairing (or later by the user): keyboard input, pointer input, audio.
    pub allow_kbd: bool,
    pub allow_ptr: bool,
    pub audio: bool,
    /// A gamepad / joystick (Bluetooth LE HID): its reports go to drivers::gamepad, never to the keyboard.
    pub gamepad: bool,
    /// Bluetooth LE: `key` is the LTK; `addr` the identity address (`addr_type` 0 public, 1 random static).
    pub le: bool,
    pub addr_type: u8,
    pub irk: Option<[u8; 16]>,
    /// Hash of the HID Report Map seen at pairing (0 = none yet).
    pub desc_hash: u32,
    /// Classic HID: the report descriptor and Device ID read over SDP at pairing.
    pub hid_desc: Vec<u8>,
    pub vid: u16,
    pub pid: u16,
    pub name: String,
}

impl Bond {
    /// MITM-protected (numeric comparison / passkey) Secure Connections key.
    pub fn authenticated(&self) -> bool {
        self.key_type == KEY_AUTH_P256
    }

    pub fn level(&self) -> &'static str {
        level_name(self.key_type)
    }

    fn flags(&self) -> String {
        let mut s = String::new();
        if self.allow_kbd {
            s.push('k');
        }
        if self.allow_ptr {
            s.push('p');
        }
        if self.audio {
            s.push('a');
        }
        if self.gamepad {
            s.push('g');
        }
        if self.le {
            s.push('L');
        }
        if self.addr_type == 1 {
            s.push('r');
        }
        if s.is_empty() {
            s.push('-');
        }
        s
    }
}

pub fn level_name(key_type: u8) -> &'static str {
    match key_type {
        KEY_AUTH_P256 => "Secure Connections, authenticated",
        KEY_UNAUTH_P256 => "Secure Connections, just works",
        _ => "insecure (legacy)",
    }
}

/// Secure Connections keys are the only ones we keep.
pub fn key_type_ok(t: u8) -> bool {
    t == KEY_AUTH_P256 || t == KEY_UNAUTH_P256
}

#[derive(Clone, Debug, PartialEq)]
pub struct Blocked {
    pub addr: BdAddr,
    pub name: String,
    /// An LE device's IRK (kept from its bond): its rotating private addresses still resolve to it.
    pub irk: Option<[u8; 16]>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Store {
    pub power: bool,
    pub show_blocked: bool,
    pub bonds: Vec<Bond>,
    pub blocked: Vec<Blocked>,
    /// Name patterns (case-insensitive substrings). Weak: names are not authenticated.
    pub blocked_names: Vec<String>,
}

/// One line's name field: printable, no newlines, at most 64 chars.
fn tidy(s: &str) -> String {
    crate::hci::clean_name(s.as_bytes()).trim().to_string()
}

impl Store {
    pub fn parse(text: &str) -> Store {
        let mut s = Store::default();
        for line in text.lines() {
            let line = line.trim();
            let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
            match word {
                "power" => s.power = rest.trim() == "on",
                "showblocked" => s.show_blocked = rest.trim() == "on",
                "bond" => {
                    let f: Vec<&str> = rest.splitn(6, ' ').collect();
                    if f.len() < 5 || s.bonds.len() >= MAX_BONDS {
                        continue;
                    }
                    let (Some(addr), Some(key), Ok(kt), Ok(cod)) = (parse_addr(f[0]), crate::unhex(f[1]), u8::from_str_radix(f[2], 16), u32::from_str_radix(f[3], 16)) else {
                        continue;
                    };
                    let Ok(key) = <[u8; 16]>::try_from(key.as_slice()) else { continue };
                    if !key_type_ok(kt) || s.bonds.iter().any(|b| b.addr == addr) {
                        continue;
                    }
                    s.bonds.push(Bond {
                        addr,
                        key,
                        key_type: kt,
                        cod: cod & 0xFF_FFFF,
                        allow_kbd: f[4].contains('k'),
                        allow_ptr: f[4].contains('p'),
                        audio: f[4].contains('a'),
                        gamepad: f[4].contains('g'),
                        le: f[4].contains('L'),
                        addr_type: f[4].contains('r') as u8,
                        irk: None,
                        desc_hash: 0,
                        name: tidy(f.get(5).copied().unwrap_or("")),
                        ..Default::default()
                    });
                },
                "hiddesc" | "hidid" => {
                    let (a, v) = rest.split_once(' ').unwrap_or((rest, ""));
                    let Some(addr) = parse_addr(a) else { continue };
                    let Some(b) = s.bonds.iter_mut().find(|b| b.addr == addr) else { continue };
                    if word == "hiddesc" {
                        b.hid_desc = crate::unhex(v.trim()).filter(|d| d.len() <= 4096).unwrap_or_default();
                    } else {
                        let mut it = v.split_whitespace().map(|x| u16::from_str_radix(x, 16).unwrap_or(0));
                        b.vid = it.next().unwrap_or(0);
                        b.pid = it.next().unwrap_or(0);
                    }
                },
                "irk" | "hidmap" => {
                    let (a, v) = rest.split_once(' ').unwrap_or((rest, ""));
                    let Some(addr) = parse_addr(a) else { continue };
                    let Some(b) = s.bonds.iter_mut().find(|b| b.addr == addr) else { continue };
                    if word == "irk" {
                        b.irk = crate::unhex(v.trim()).and_then(|k| <[u8; 16]>::try_from(k.as_slice()).ok());
                    } else {
                        b.desc_hash = u32::from_str_radix(v.trim(), 16).unwrap_or(0);
                    }
                },
                "block" => {
                    let (a, n) = rest.split_once(' ').unwrap_or((rest, ""));
                    let (irk, n) = match n.strip_prefix("irk=") {
                        Some(r) => {
                            let (k, n2) = r.split_once(' ').unwrap_or((r, ""));
                            (crate::unhex(k).and_then(|k| <[u8; 16]>::try_from(k.as_slice()).ok()), n2)
                        },
                        None => (None, n),
                    };
                    if let Some(addr) = parse_addr(a) {
                        if s.blocked.len() < MAX_BLOCKED && !s.blocked.iter().any(|b| b.addr == addr) {
                            s.blocked.push(Blocked { addr, name: tidy(n), irk });
                        }
                    }
                },
                "blockname" => {
                    let p = tidy(rest);
                    if !p.is_empty() && s.blocked_names.len() < 64 && !s.blocked_names.contains(&p) {
                        s.blocked_names.push(p);
                    }
                },
                _ => {},
            }
        }
        s
    }

    pub fn to_text(&self) -> String {
        let mut t = String::new();
        t.push_str(if self.power { "power on\n" } else { "power off\n" });
        if self.show_blocked {
            t.push_str("showblocked on\n");
        }
        for b in &self.bonds {
            t.push_str(&format!("bond {} {} {:02x} {:06x} {} {}\n", addr_str(&b.addr), crate::hex(&b.key), b.key_type, b.cod, b.flags(), b.name));
        }
        for b in &self.bonds {
            if let Some(k) = b.irk {
                t.push_str(&format!("irk {} {}\n", addr_str(&b.addr), crate::hex(&k)));
            }
            if !b.hid_desc.is_empty() {
                t.push_str(&format!("hiddesc {} {}\n", addr_str(&b.addr), crate::hex(&b.hid_desc)));
            }
            if b.vid != 0 || b.pid != 0 {
                t.push_str(&format!("hidid {} {:04x} {:04x}\n", addr_str(&b.addr), b.vid, b.pid));
            }
            if b.desc_hash != 0 {
                t.push_str(&format!("hidmap {} {:08x}\n", addr_str(&b.addr), b.desc_hash));
            }
        }
        for b in &self.blocked {
            match b.irk {
                Some(k) => t.push_str(&format!("block {} irk={} {}\n", addr_str(&b.addr), crate::hex(&k), b.name)),
                None => t.push_str(&format!("block {} {}\n", addr_str(&b.addr), b.name)),
            }
        }
        for p in &self.blocked_names {
            t.push_str(&format!("blockname {p}\n"));
        }
        t
    }

    pub fn bond(&self, a: &BdAddr) -> Option<&Bond> {
        self.bonds.iter().find(|b| &b.addr == a)
    }

    pub fn bond_mut(&mut self, a: &BdAddr) -> Option<&mut Bond> {
        self.bonds.iter_mut().find(|b| &b.addr == a)
    }

    pub fn remove_bond(&mut self, a: &BdAddr) -> bool {
        let n = self.bonds.len();
        self.bonds.retain(|b| &b.addr != a);
        n != self.bonds.len()
    }

    /// The identity behind an address: itself, or the bond / blocked entry whose
    /// IRK resolves this LE resolvable private address.
    pub fn resolve(&self, a: &BdAddr) -> BdAddr {
        for b in &self.bonds {
            if b.irk.is_some_and(|k| crate::smp::rpa_matches(&k, a)) {
                return b.addr;
            }
        }
        for b in &self.blocked {
            if b.irk.is_some_and(|k| crate::smp::rpa_matches(&k, a)) {
                return b.addr;
            }
        }
        *a
    }

    /// Blocked by address (an LE private address resolved through known IRKs),
    /// or by a name pattern when the name is known.
    pub fn is_blocked(&self, a: &BdAddr, name: Option<&str>) -> bool {
        let id = self.resolve(a);
        if self.blocked.iter().any(|b| &b.addr == a || b.addr == id) {
            return true;
        }
        match name {
            Some(n) if !n.is_empty() => {
                let n = n.to_lowercase();
                self.blocked_names.iter().any(|p| n.contains(&p.to_lowercase()))
            },
            _ => false,
        }
    }

    /// Block an address: its bond (keys) goes too. Err when the list is full.
    pub fn block(&mut self, a: &BdAddr, name: &str) -> Result<bool, String> {
        let a = &self.resolve(a);
        // an LE bond's IRK stays with the block entry (its keys go)
        let irk = self.bond(a).and_then(|b| b.irk);
        self.remove_bond(a);
        if self.blocked.iter().any(|b| &b.addr == a) {
            return Ok(false);
        }
        if self.blocked.len() >= MAX_BLOCKED {
            return Err(String::from("the block list is full"));
        }
        self.blocked.push(Blocked { addr: *a, name: tidy(name), irk });
        Ok(true)
    }

    pub fn unblock(&mut self, a: &BdAddr) -> bool {
        let n = self.blocked.len();
        self.blocked.retain(|b| &b.addr != a);
        n != self.blocked.len()
    }

    pub fn block_name(&mut self, pattern: &str) -> bool {
        let p = tidy(pattern);
        if p.is_empty() || self.blocked_names.contains(&p) || self.blocked_names.len() >= 64 {
            return false;
        }
        // bonds whose name matches go now
        let pl = p.to_lowercase();
        self.bonds.retain(|b| !b.name.to_lowercase().contains(&pl));
        self.blocked_names.push(p);
        true
    }

    pub fn unblock_name(&mut self, pattern: &str) -> bool {
        let n = self.blocked_names.len();
        let p = pattern.trim();
        self.blocked_names.retain(|x| x != p);
        n != self.blocked_names.len()
    }
}
