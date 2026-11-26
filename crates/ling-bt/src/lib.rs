//! Bluetooth host stack for LingOS, freestanding (no_std + alloc).
//!
//! Everything here is pure protocol logic with no I/O: the kernel driver
//! (`ling-kernel/src/drivers/bluetooth.rs`) moves bytes between the USB
//! transport and [`host::Host`], which answers with commands / ACL packets to
//! send and with input reports, notices and store changes. That keeps every
//! parser and every security decision testable on the host (`cargo test -p
//! ling-bt`), with recorded or synthetic packets.
//!
//! - [`hci`]: HCI command builders, event parser, ACL framing, the USB
//!   byte-stream reassembler.
//! - [`btintel`]: Intel controllers (AX201 = USB 8087:0026): Read Version /
//!   Boot Params, firmware file naming, SHA-256 pinning, the `.sfi` secure-send
//!   plan, the `.ddc` device configuration, the Intel Reset that boots it.
//! - [`l2cap`]: ACL reassembly (bounded), basic-mode channels, signalling.
//! - [`hidp`]: the HID host protocol (boot keyboard / mouse reports).
//! - [`store`]: one user's bonds, block list and Bluetooth switch (text, sealed
//!   at rest by the kernel keyring).
//! - [`host`]: the state machine: controller init, inquiry + passive LE scan,
//!   Secure Simple Pairing (Secure Connections only), the security gates, HID.
//! - [`sbc`]: the SBC audio encoder (A2DP's mandatory codec).
//!
//! Every parser takes untrusted bytes: lengths are checked, malformed input is
//! dropped, nothing panics (see the fuzz tests).

#![no_std]

extern crate alloc;

pub mod avdtp;
pub mod btintel;
pub mod hci;
pub mod hidp;
pub mod host;
pub mod l2cap;
pub mod gatt;
pub mod sbc;
pub mod sdp;
pub mod smp;
pub mod store;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_fuzz;
#[cfg(test)]
mod tests_le;
#[cfg(test)]
mod tests_pads;
/// tests_pads includes the kernel's gamepad.rs, which marks activity for the idle lock
/// (`crate::startup::touch`); outside the kernel that is a no-op.
#[cfg(test)]
mod startup {
    pub fn touch() {}
}

use alloc::string::String;

/// A Bluetooth device address as it goes over HCI (least significant byte first).
pub type BdAddr = [u8; 6];

/// "AA:BB:CC:DD:EE:FF" (most significant byte first, the way devices print it).
pub fn addr_str(a: &BdAddr) -> String {
    let mut s = String::with_capacity(17);
    for i in (0..6).rev() {
        let b = a[i];
        let hex = b"0123456789ABCDEF";
        s.push(hex[(b >> 4) as usize] as char);
        s.push(hex[(b & 15) as usize] as char);
        if i > 0 {
            s.push(':');
        }
    }
    s
}

/// Parse "AA:BB:CC:DD:EE:FF" (or with '-'); None when malformed.
pub fn parse_addr(s: &str) -> Option<BdAddr> {
    let s = s.trim();
    let parts: alloc::vec::Vec<&str> = s.split(|c| c == ':' || c == '-').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut a = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        if p.len() != 2 {
            return None;
        }
        a[5 - i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(a)
}

/// Little-endian u16 at `o`, or None past the end.
pub(crate) fn le16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(o)?, *b.get(o + 1)?]))
}

pub(crate) fn le32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes([*b.get(o)?, *b.get(o + 1)?, *b.get(o + 2)?, *b.get(o + 3)?]))
}

pub(crate) fn addr_at(b: &[u8], o: usize) -> Option<BdAddr> {
    b.get(o..o + 6)?.try_into().ok()
}

pub(crate) fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        let h = b"0123456789abcdef";
        s.push(h[(x >> 4) as usize] as char);
        s.push(h[(x & 15) as usize] as char);
    }
    s
}

pub(crate) fn unhex(s: &str) -> Option<alloc::vec::Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}
