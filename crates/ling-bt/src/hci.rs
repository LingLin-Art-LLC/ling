//! HCI (Bluetooth Core v5.3, Vol 4 Part E): command packets, events, ACL data.
//!
//! On USB (Vol 4 Part B) commands go out on the control endpoint, events come
//! in on the interrupt endpoint and ACL data on the bulk pair, each without
//! the UART's packet-type byte. USB packets are at most one max-packet each,
//! so [`Stream`] rebuilds whole HCI packets from the byte stream using the
//! packet headers' own lengths.

use crate::{addr_at, le16, le32, BdAddr};
use alloc::string::String;
use alloc::vec::Vec;

/// Opcodes (OGF << 10 | OCF).
pub mod op {
    pub const INQUIRY: u16 = 0x0401;
    pub const INQUIRY_CANCEL: u16 = 0x0402;
    pub const CREATE_CONNECTION: u16 = 0x0405;
    pub const DISCONNECT: u16 = 0x0406;
    pub const ACCEPT_CONNECTION: u16 = 0x0409;
    pub const REJECT_CONNECTION: u16 = 0x040A;
    pub const LINK_KEY_REPLY: u16 = 0x040B;
    pub const LINK_KEY_NEG_REPLY: u16 = 0x040C;
    pub const PIN_CODE_NEG_REPLY: u16 = 0x040E;
    pub const AUTH_REQUESTED: u16 = 0x0411;
    pub const SET_CONN_ENCRYPTION: u16 = 0x0413;
    pub const REMOTE_NAME_REQUEST: u16 = 0x0419;
    pub const IO_CAP_REPLY: u16 = 0x042B;
    pub const USER_CONFIRM_REPLY: u16 = 0x042C;
    pub const USER_CONFIRM_NEG_REPLY: u16 = 0x042D;
    pub const IO_CAP_NEG_REPLY: u16 = 0x0434;
    pub const WRITE_DEFAULT_LINK_POLICY: u16 = 0x080F;
    pub const SET_EVENT_MASK: u16 = 0x0C01;
    pub const RESET: u16 = 0x0C03;
    pub const WRITE_LOCAL_NAME: u16 = 0x0C13;
    pub const WRITE_PAGE_TIMEOUT: u16 = 0x0C18;
    pub const WRITE_SCAN_ENABLE: u16 = 0x0C1A;
    pub const WRITE_CLASS_OF_DEVICE: u16 = 0x0C24;
    pub const WRITE_INQUIRY_MODE: u16 = 0x0C45;
    pub const WRITE_SSP_MODE: u16 = 0x0C56;
    pub const WRITE_LE_HOST_SUPPORTED: u16 = 0x0C6D;
    pub const WRITE_SC_HOST_SUPPORT: u16 = 0x0C7A;
    pub const READ_LOCAL_VERSION: u16 = 0x1001;
    pub const READ_BUFFER_SIZE: u16 = 0x1005;
    pub const READ_BD_ADDR: u16 = 0x1009;
    pub const READ_ENC_KEY_SIZE: u16 = 0x1408;
    pub const LE_SET_EVENT_MASK: u16 = 0x2001;
    pub const LE_SET_RANDOM_ADDRESS: u16 = 0x2005;
    pub const LE_SET_SCAN_PARAMS: u16 = 0x200B;
    pub const LE_SET_SCAN_ENABLE: u16 = 0x200C;
    pub const LE_READ_BUFFER_SIZE: u16 = 0x2002;
    pub const LE_CREATE_CONNECTION: u16 = 0x200D;
    pub const LE_CREATE_CONNECTION_CANCEL: u16 = 0x200E;
    pub const LE_CONNECTION_UPDATE: u16 = 0x2013;
    pub const LE_START_ENCRYPTION: u16 = 0x2019;
    pub const LE_REMOTE_PARAM_REPLY: u16 = 0x2020;
    pub const LE_REMOTE_PARAM_NEG_REPLY: u16 = 0x2021;
    pub const USER_PASSKEY_NEG_REPLY: u16 = 0x042F;
}

/// Event codes.
pub mod ev {
    pub const INQUIRY_COMPLETE: u8 = 0x01;
    pub const INQUIRY_RESULT: u8 = 0x02;
    pub const CONN_COMPLETE: u8 = 0x03;
    pub const CONN_REQUEST: u8 = 0x04;
    pub const DISCONN_COMPLETE: u8 = 0x05;
    pub const AUTH_COMPLETE: u8 = 0x06;
    pub const REMOTE_NAME: u8 = 0x07;
    pub const ENCRYPTION_CHANGE: u8 = 0x08;
    pub const CMD_COMPLETE: u8 = 0x0E;
    pub const CMD_STATUS: u8 = 0x0F;
    pub const HARDWARE_ERROR: u8 = 0x10;
    pub const NUM_COMPLETED: u8 = 0x13;
    pub const PIN_CODE_REQUEST: u8 = 0x16;
    pub const LINK_KEY_REQUEST: u8 = 0x17;
    pub const LINK_KEY_NOTIFY: u8 = 0x18;
    pub const INQUIRY_RESULT_RSSI: u8 = 0x22;
    pub const EXT_INQUIRY_RESULT: u8 = 0x2F;
    pub const ENC_KEY_REFRESH: u8 = 0x30;
    pub const IO_CAP_REQUEST: u8 = 0x31;
    pub const IO_CAP_RESPONSE: u8 = 0x32;
    pub const USER_CONFIRM_REQUEST: u8 = 0x33;
    pub const USER_PASSKEY_REQUEST: u8 = 0x34;
    pub const SSP_COMPLETE: u8 = 0x36;
    pub const USER_PASSKEY_NOTIFY: u8 = 0x3B;
    pub const LE_META: u8 = 0x3E;
    pub const VENDOR: u8 = 0xFF;
}

/// The events [`Host`](crate::host::Host) handles, as a Set Event Mask value.
pub fn event_mask() -> u64 {
    let mut m = 0xFFu64; // inquiry complete/result, conn complete/request, disconn, auth, remote name, encryption change
    // hardware error, PIN / link key request, link key notification, inquiry result with RSSI,
    // extended inquiry result, encryption key refresh, IO capability request / response, user
    // confirmation / passkey request, simple pairing complete, passkey notification, LE meta
    for b in [15u32, 21, 22, 23, 33, 46, 47, 48, 49, 50, 51, 53, 58, 61] {
        m |= 1 << b;
    }
    m
}

/// A command packet: opcode, parameter length, parameters (at most 255 bytes).
pub fn command(opcode: u16, params: &[u8]) -> Vec<u8> {
    let n = params.len().min(255);
    let mut v = Vec::with_capacity(3 + n);
    v.extend_from_slice(&opcode.to_le_bytes());
    v.push(n as u8);
    v.extend_from_slice(&params[..n]);
    v
}

/// The opcode of a command packet.
pub fn command_opcode(pkt: &[u8]) -> Option<u16> {
    le16(pkt, 0)
}

pub fn reset() -> Vec<u8> {
    command(op::RESET, &[])
}

/// General inquiry (GIAC 0x9E8B33) for `len` x 1.28 s, unlimited responses.
pub fn inquiry(len: u8) -> Vec<u8> {
    command(op::INQUIRY, &[0x33, 0x8B, 0x9E, len.clamp(1, 0x30), 0])
}

/// Create Connection: DM1..DH5 packet types, no role switch (the host stays
/// central: a peripheral that asks to switch roles cannot use it to skip our
/// authentication).
pub fn create_connection(a: &BdAddr, pscan_rep_mode: u8, clock_offset: Option<u16>) -> Vec<u8> {
    let mut p = Vec::with_capacity(13);
    p.extend_from_slice(a);
    p.extend_from_slice(&0xCC18u16.to_le_bytes());
    p.push(pscan_rep_mode);
    p.push(0);
    p.extend_from_slice(&clock_offset.map(|c| c | 0x8000).unwrap_or(0).to_le_bytes());
    p.push(0); // allow role switch: no
    command(op::CREATE_CONNECTION, &p)
}

pub fn accept_connection(a: &BdAddr) -> Vec<u8> {
    let mut p = a.to_vec();
    p.push(0x00); // become central
    command(op::ACCEPT_CONNECTION, &p)
}

/// Reject Connection Request; reason 0x0F = unacceptable BD_ADDR.
pub fn reject_connection(a: &BdAddr, reason: u8) -> Vec<u8> {
    let mut p = a.to_vec();
    p.push(reason);
    command(op::REJECT_CONNECTION, &p)
}

/// Disconnect; reason 0x13 = remote user terminated, 0x05 = authentication failure.
pub fn disconnect(handle: u16, reason: u8) -> Vec<u8> {
    let mut p = handle.to_le_bytes().to_vec();
    p.push(reason);
    command(op::DISCONNECT, &p)
}

pub fn link_key_reply(a: &BdAddr, key: &[u8; 16]) -> Vec<u8> {
    let mut p = a.to_vec();
    p.extend_from_slice(key);
    command(op::LINK_KEY_REPLY, &p)
}

pub fn link_key_neg_reply(a: &BdAddr) -> Vec<u8> {
    command(op::LINK_KEY_NEG_REPLY, a)
}

pub fn pin_code_neg_reply(a: &BdAddr) -> Vec<u8> {
    command(op::PIN_CODE_NEG_REPLY, a)
}

/// IO Capability Request Reply: our IO capability, OOB data absent, auth requirements.
pub fn io_cap_reply(a: &BdAddr, io_cap: u8, auth_req: u8) -> Vec<u8> {
    let mut p = a.to_vec();
    p.extend_from_slice(&[io_cap, 0, auth_req]);
    command(op::IO_CAP_REPLY, &p)
}

/// IO Capability Request Negative Reply; reason 0x18 = pairing not allowed.
pub fn io_cap_neg_reply(a: &BdAddr, reason: u8) -> Vec<u8> {
    let mut p = a.to_vec();
    p.push(reason);
    command(op::IO_CAP_NEG_REPLY, &p)
}

pub fn user_confirm_reply(a: &BdAddr, yes: bool) -> Vec<u8> {
    command(if yes { op::USER_CONFIRM_REPLY } else { op::USER_CONFIRM_NEG_REPLY }, a)
}

pub fn auth_requested(handle: u16) -> Vec<u8> {
    command(op::AUTH_REQUESTED, &handle.to_le_bytes())
}

pub fn set_conn_encryption(handle: u16, on: bool) -> Vec<u8> {
    let mut p = handle.to_le_bytes().to_vec();
    p.push(on as u8);
    command(op::SET_CONN_ENCRYPTION, &p)
}

pub fn read_enc_key_size(handle: u16) -> Vec<u8> {
    command(op::READ_ENC_KEY_SIZE, &handle.to_le_bytes())
}

pub fn remote_name_request(a: &BdAddr, pscan_rep_mode: u8, clock_offset: Option<u16>) -> Vec<u8> {
    let mut p = a.to_vec();
    p.push(pscan_rep_mode);
    p.push(0);
    p.extend_from_slice(&clock_offset.map(|c| c | 0x8000).unwrap_or(0).to_le_bytes());
    command(op::REMOTE_NAME_REQUEST, &p)
}

/// Write Scan Enable: bit 0 inquiry scan (discoverable), bit 1 page scan (connectable).
pub fn write_scan_enable(inquiry_scan: bool, page_scan: bool) -> Vec<u8> {
    command(op::WRITE_SCAN_ENABLE, &[(inquiry_scan as u8) | ((page_scan as u8) << 1)])
}

pub fn write_local_name(name: &str) -> Vec<u8> {
    let mut p = [0u8; 248];
    let b = name.as_bytes();
    let n = b.len().min(247);
    p[..n].copy_from_slice(&b[..n]);
    command(op::WRITE_LOCAL_NAME, &p)
}

pub fn write_class_of_device(cod: u32) -> Vec<u8> {
    command(op::WRITE_CLASS_OF_DEVICE, &cod.to_le_bytes()[..3])
}

pub fn set_event_mask(m: u64) -> Vec<u8> {
    command(op::SET_EVENT_MASK, &m.to_le_bytes())
}

pub fn le_set_random_address(a: &BdAddr) -> Vec<u8> {
    command(op::LE_SET_RANDOM_ADDRESS, a)
}

/// Passive LE scan (we never transmit scan requests) from our random address.
pub fn le_set_scan_params() -> Vec<u8> {
    command(op::LE_SET_SCAN_PARAMS, &[0x00, 0x60, 0x00, 0x30, 0x00, 0x01, 0x00])
}

pub fn le_set_scan_enable(on: bool) -> Vec<u8> {
    command(op::LE_SET_SCAN_ENABLE, &[on as u8, 1])
}

/// LE events we use: connection complete, advertising report, connection
/// update complete, remote features, LTK request, remote connection parameter request.
pub fn le_set_event_mask() -> Vec<u8> {
    command(op::LE_SET_EVENT_MASK, &0x3Fu64.to_le_bytes())
}

/// Connection parameters we ask for: 7.5..15 ms interval (gamepads), no latency, 2 s supervision.
pub const LE_INTERVAL_MIN: u16 = 6;
pub const LE_INTERVAL_MAX: u16 = 12;
pub const LE_TIMEOUT: u16 = 200;

/// LE Create Connection from our public address to `peer` (type 0 public / 1 random).
pub fn le_create_connection(peer_type: u8, peer: &BdAddr) -> Vec<u8> {
    let mut p = Vec::with_capacity(25);
    p.extend_from_slice(&0x0060u16.to_le_bytes()); // scan interval
    p.extend_from_slice(&0x0030u16.to_le_bytes()); // scan window
    p.push(0); // no filter accept list: this peer only
    p.push(peer_type & 1);
    p.extend_from_slice(peer);
    p.push(0); // own address: public
    p.extend_from_slice(&LE_INTERVAL_MIN.to_le_bytes());
    p.extend_from_slice(&LE_INTERVAL_MAX.to_le_bytes());
    p.extend_from_slice(&0u16.to_le_bytes());
    p.extend_from_slice(&LE_TIMEOUT.to_le_bytes());
    p.extend_from_slice(&0u16.to_le_bytes());
    p.extend_from_slice(&0u16.to_le_bytes());
    command(op::LE_CREATE_CONNECTION, &p)
}

pub fn le_create_connection_cancel() -> Vec<u8> {
    command(op::LE_CREATE_CONNECTION_CANCEL, &[])
}

/// LE Start Encryption with a Secure Connections LTK (EDIV = 0, Rand = 0).
pub fn le_start_encryption(handle: u16, ltk: &[u8; 16]) -> Vec<u8> {
    let mut p = handle.to_le_bytes().to_vec();
    p.extend_from_slice(&[0; 10]);
    p.extend_from_slice(ltk);
    command(op::LE_START_ENCRYPTION, &p)
}

/// Connection Update / Remote Connection Parameter Request Reply with these parameters.
pub fn le_conn_params(opcode: u16, handle: u16, min: u16, max: u16, latency: u16, timeout: u16) -> Vec<u8> {
    let mut p = handle.to_le_bytes().to_vec();
    for v in [min, max, latency, timeout, 0, 0] {
        p.extend_from_slice(&v.to_le_bytes());
    }
    command(opcode, &p)
}

pub fn le_remote_param_neg(handle: u16) -> Vec<u8> {
    let mut p = handle.to_le_bytes().to_vec();
    p.push(0x3B); // unacceptable connection parameters
    command(op::LE_REMOTE_PARAM_NEG_REPLY, &p)
}

/// Connection parameters within the spec's ranges and sensible for an input device.
pub fn le_params_ok(min: u16, max: u16, latency: u16, timeout: u16) -> bool {
    (6..=3200).contains(&min) && (min..=3200).contains(&max) && latency <= 499 && (10..=3200).contains(&timeout) && (timeout as u32) * 4 > (1 + latency as u32) * max as u32
}

/// A non-resolvable private address (top two bits 00, not all-zero / all-one)
/// from 6 random bytes.
pub fn nrpa(mut r: [u8; 6]) -> BdAddr {
    r[5] &= 0x3F;
    if r[..5].iter().all(|&b| b == 0) {
        r[0] = 1;
    }
    if r[..5].iter().all(|&b| b == 0xFF) && r[5] == 0x3F {
        r[0] = 0xFE;
    }
    r
}

// ── events ──────────────────────────────────────────────────────────────────

/// A device heard in an inquiry.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub addr: BdAddr,
    pub pscan_rep_mode: u8,
    pub cod: u32,
    pub clock_offset: u16,
    pub rssi: Option<i8>,
    pub name: Option<String>,
}

/// One LE advertising report.
#[derive(Clone, Debug, PartialEq)]
pub struct LeAdv {
    pub evt_type: u8,
    pub addr_type: u8,
    pub addr: BdAddr,
    pub rssi: i8,
    pub name: Option<String>,
    /// GAP Appearance (0x03C1 keyboard, 0x03C2 mouse, 0x03C4 gamepad...), 0 when absent.
    pub appearance: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    CommandComplete { ncmd: u8, opcode: u16, ret: Vec<u8> },
    CommandStatus { status: u8, ncmd: u8, opcode: u16 },
    InquiryComplete { status: u8 },
    InquiryResult(Vec<Found>),
    ConnectionComplete { status: u8, handle: u16, addr: BdAddr, link_type: u8, encrypted: bool },
    ConnectionRequest { addr: BdAddr, cod: u32, link_type: u8 },
    DisconnectionComplete { status: u8, handle: u16, reason: u8 },
    AuthenticationComplete { status: u8, handle: u16 },
    RemoteNameComplete { status: u8, addr: BdAddr, name: String },
    /// `mode`: 0 off, 1 E0 (legacy) / AES-CCM on LE, 2 AES-CCM (Secure Connections).
    EncryptionChange { status: u8, handle: u16, mode: u8 },
    EncryptionKeyRefresh { status: u8, handle: u16 },
    NumCompletedPackets(Vec<(u16, u16)>),
    PinCodeRequest(BdAddr),
    LinkKeyRequest(BdAddr),
    LinkKeyNotification { addr: BdAddr, key: [u8; 16], key_type: u8 },
    IoCapabilityRequest(BdAddr),
    IoCapabilityResponse { addr: BdAddr, io_cap: u8, oob: u8, auth_req: u8 },
    UserConfirmationRequest { addr: BdAddr, value: u32 },
    UserPasskeyRequest(BdAddr),
    UserPasskeyNotification { addr: BdAddr, passkey: u32 },
    SimplePairingComplete { status: u8, addr: BdAddr },
    HardwareError(u8),
    LeAdvertisingReport(Vec<LeAdv>),
    LeConnectionComplete { status: u8, handle: u16, role: u8, peer_type: u8, peer: BdAddr, interval: u16, latency: u16, timeout: u16 },
    LeRemoteParamRequest { handle: u16, min: u16, max: u16, latency: u16, timeout: u16 },
    LeMeta { sub: u8 },
    Vendor(Vec<u8>),
    Other { code: u8 },
}

/// Most responses one event may carry (bounds a hostile `num` field).
const MAX_ITEMS: usize = 32;

/// Parse an event packet `[code, plen, params...]`. None when it is truncated
/// or its length byte disagrees with the packet.
pub fn parse_event(pkt: &[u8]) -> Option<Event> {
    let code = *pkt.first()?;
    let plen = *pkt.get(1)? as usize;
    let p = pkt.get(2..2 + plen)?;
    let a = |o: usize| addr_at(p, o);
    let u8_ = |o: usize| p.get(o).copied();
    Some(match code {
        ev::CMD_COMPLETE => Event::CommandComplete { ncmd: u8_(0)?, opcode: le16(p, 1)?, ret: p.get(3..)?.to_vec() },
        ev::CMD_STATUS => Event::CommandStatus { status: u8_(0)?, ncmd: u8_(1)?, opcode: le16(p, 2)? },
        ev::INQUIRY_COMPLETE => Event::InquiryComplete { status: u8_(0)? },
        ev::INQUIRY_RESULT | ev::INQUIRY_RESULT_RSSI => {
            let n = (u8_(0)? as usize).min(MAX_ITEMS);
            let mut v = Vec::new();
            for i in 0..n {
                let o = 1 + i * 14;
                let e = p.get(o..o + 14)?;
                let rssi = code == ev::INQUIRY_RESULT_RSSI;
                // without RSSI: addr, rep mode, 2 reserved, cod, clock offset;
                // with RSSI: addr, rep mode, 1 reserved, cod, clock offset, rssi
                let cod_at = if rssi { 8 } else { 9 };
                v.push(Found {
                    addr: addr_at(e, 0)?,
                    pscan_rep_mode: e[6],
                    cod: cod24(e, cod_at)?,
                    clock_offset: le16(e, cod_at + 3)?,
                    rssi: rssi.then(|| e[13] as i8),
                    name: None,
                });
            }
            Event::InquiryResult(v)
        },
        ev::EXT_INQUIRY_RESULT => {
            let e = p.get(1..)?;
            Event::InquiryResult(alloc::vec![Found {
                addr: addr_at(e, 0)?,
                pscan_rep_mode: *e.get(6)?,
                cod: cod24(e, 8)?,
                clock_offset: le16(e, 11)?,
                rssi: Some(*e.get(13)? as i8),
                name: e.get(14..).and_then(eir_name),
            }])
        },
        ev::CONN_COMPLETE => Event::ConnectionComplete { status: u8_(0)?, handle: le16(p, 1)? & 0x0FFF, addr: a(3)?, link_type: u8_(9)?, encrypted: u8_(10)? != 0 },
        ev::CONN_REQUEST => Event::ConnectionRequest { addr: a(0)?, cod: cod24(p, 6)?, link_type: u8_(9)? },
        ev::DISCONN_COMPLETE => Event::DisconnectionComplete { status: u8_(0)?, handle: le16(p, 1)? & 0x0FFF, reason: u8_(3)? },
        ev::AUTH_COMPLETE => Event::AuthenticationComplete { status: u8_(0)?, handle: le16(p, 1)? & 0x0FFF },
        ev::REMOTE_NAME => Event::RemoteNameComplete { status: u8_(0)?, addr: a(1)?, name: cstr(p.get(7..)?) },
        ev::ENCRYPTION_CHANGE => Event::EncryptionChange { status: u8_(0)?, handle: le16(p, 1)? & 0x0FFF, mode: u8_(3)? },
        ev::ENC_KEY_REFRESH => Event::EncryptionKeyRefresh { status: u8_(0)?, handle: le16(p, 1)? & 0x0FFF },
        ev::NUM_COMPLETED => {
            let n = (u8_(0)? as usize).min(MAX_ITEMS);
            let mut v = Vec::new();
            for i in 0..n {
                v.push((le16(p, 1 + i * 4)? & 0x0FFF, le16(p, 3 + i * 4)?));
            }
            Event::NumCompletedPackets(v)
        },
        ev::PIN_CODE_REQUEST => Event::PinCodeRequest(a(0)?),
        ev::LINK_KEY_REQUEST => Event::LinkKeyRequest(a(0)?),
        ev::LINK_KEY_NOTIFY => Event::LinkKeyNotification { addr: a(0)?, key: p.get(6..22)?.try_into().ok()?, key_type: u8_(22)? },
        ev::IO_CAP_REQUEST => Event::IoCapabilityRequest(a(0)?),
        ev::IO_CAP_RESPONSE => Event::IoCapabilityResponse { addr: a(0)?, io_cap: u8_(6)?, oob: u8_(7)?, auth_req: u8_(8)? },
        ev::USER_CONFIRM_REQUEST => Event::UserConfirmationRequest { addr: a(0)?, value: le32(p, 6)? },
        ev::USER_PASSKEY_REQUEST => Event::UserPasskeyRequest(a(0)?),
        ev::USER_PASSKEY_NOTIFY => Event::UserPasskeyNotification { addr: a(0)?, passkey: le32(p, 6)? },
        ev::SSP_COMPLETE => Event::SimplePairingComplete { status: u8_(0)?, addr: a(1)? },
        ev::HARDWARE_ERROR => Event::HardwareError(u8_(0)?),
        ev::LE_META => {
            let sub = u8_(0)?;
            if sub == 0x02 {
                let n = (u8_(1)? as usize).min(MAX_ITEMS);
                let mut o = 2;
                let mut v = Vec::new();
                for _ in 0..n {
                    let evt_type = u8_(o)?;
                    let addr_type = u8_(o + 1)?;
                    let addr = a(o + 2)?;
                    let dl = u8_(o + 8)? as usize;
                    let data = p.get(o + 9..o + 9 + dl)?;
                    let rssi = u8_(o + 9 + dl)? as i8;
                    let appearance = eir_iter(data).find(|(t, d)| *t == 0x19 && d.len() >= 2).map(|(_, d)| u16::from_le_bytes([d[0], d[1]])).unwrap_or(0);
                    v.push(LeAdv { evt_type, addr_type, addr, rssi, name: eir_name(data), appearance });
                    o += 10 + dl;
                }
                Event::LeAdvertisingReport(v)
            } else if sub == 0x01 {
                Event::LeConnectionComplete {
                    status: u8_(1)?,
                    handle: le16(p, 2)? & 0x0FFF,
                    role: u8_(4)?,
                    peer_type: u8_(5)?,
                    peer: a(6)?,
                    interval: le16(p, 12)?,
                    latency: le16(p, 14)?,
                    timeout: le16(p, 16)?,
                }
            } else if sub == 0x06 {
                Event::LeRemoteParamRequest { handle: le16(p, 1)? & 0x0FFF, min: le16(p, 3)?, max: le16(p, 5)?, latency: le16(p, 7)?, timeout: le16(p, 9)? }
            } else {
                Event::LeMeta { sub }
            }
        },
        ev::VENDOR => Event::Vendor(p.to_vec()),
        c => Event::Other { code: c },
    })
}

fn cod24(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes([*b.get(o)?, *b.get(o + 1)?, *b.get(o + 2)?, 0]))
}

/// A NUL-terminated UTF-8 name, lossily decoded, control characters dropped.
fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    clean_name(&b[..end])
}

/// Names come from untrusted radios: keep printable text only, at most 64 chars.
pub fn clean_name(b: &[u8]) -> String {
    String::from_utf8_lossy(b).chars().filter(|c| !c.is_control()).take(64).collect()
}

/// Walk EIR / advertising data: `(type, data)` of each structure.
pub fn eir_iter(b: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut o = 0usize;
    core::iter::from_fn(move || {
        let len = *b.get(o)? as usize;
        if len == 0 {
            return None;
        }
        let ty = *b.get(o + 1)?;
        let data = b.get(o + 2..o + 1 + len)?;
        o += 1 + len;
        Some((ty, data))
    })
}

/// The complete (0x09) or shortened (0x08) local name in EIR / advertising data.
pub fn eir_name(b: &[u8]) -> Option<String> {
    let mut short = None;
    for (ty, d) in eir_iter(b) {
        if ty == 0x09 {
            return Some(clean_name(d));
        }
        if ty == 0x08 {
            short = Some(clean_name(d));
        }
    }
    short
}

/// What a Class of Device says the device is.
pub fn cod_kind(cod: u32) -> &'static str {
    let major = (cod >> 8) & 0x1F;
    let minor = (cod >> 2) & 0x3F;
    match major {
        1 => "computer",
        2 => "phone",
        3 => "network",
        4 => match minor {
            1 => "headset",
            2 => "hands-free",
            4 => "microphone",
            5 => "speaker",
            6 => "headphones",
            7 => "portable audio",
            8 => "car audio",
            10 => "hi-fi audio",
            _ => "audio",
        },
        5 => match (minor >> 4) & 3 {
            1 => "keyboard",
            2 => "mouse",
            3 => "keyboard + mouse",
            _ => match minor & 0xF {
                1 => "joystick",
                2 => "gamepad",
                5 => "tablet",
                _ => "input device",
            },
        },
        6 => "imaging",
        7 => "wearable",
        8 => "toy",
        9 => "health",
        _ => "device",
    }
}

/// HID capabilities the Class of Device announces: (keyboard, pointer).
pub fn cod_hid(cod: u32) -> (bool, bool) {
    if (cod >> 8) & 0x1F != 5 {
        return (false, false);
    }
    let m = (cod >> 6) & 3;
    (m & 1 != 0, m & 2 != 0)
}

/// What a GAP Appearance value says (the HID category 0x03Cx).
pub fn appearance_kind(a: u16) -> &'static str {
    match a {
        0x03C1 => "keyboard",
        0x03C2 => "mouse",
        0x03C3 => "joystick",
        0x03C4 => "gamepad",
        0x03C5 => "tablet",
        0x03C0..=0x03CF => "input device",
        0x0940..=0x097F => "audio",
        _ => "LE device",
    }
}

pub fn cod_is_audio(cod: u32) -> bool {
    (cod >> 8) & 0x1F == 4
}

// ── ACL data ────────────────────────────────────────────────────────────────

/// Packet boundary flags.
pub const PB_FIRST_NONFLUSH: u8 = 0b00;
pub const PB_CONT: u8 = 0b01;
pub const PB_FIRST: u8 = 0b10;

#[derive(Debug, PartialEq)]
pub struct Acl<'a> {
    pub handle: u16,
    pub pb: u8,
    pub data: &'a [u8],
}

pub fn parse_acl(pkt: &[u8]) -> Option<Acl<'_>> {
    let h = le16(pkt, 0)?;
    let len = le16(pkt, 2)? as usize;
    Some(Acl { handle: h & 0x0FFF, pb: ((h >> 12) & 3) as u8, data: pkt.get(4..4 + len)? })
}

pub fn acl_packet(handle: u16, pb: u8, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + data.len());
    v.extend_from_slice(&((handle & 0x0FFF) | ((pb as u16 & 3) << 12)).to_le_bytes());
    v.extend_from_slice(&(data.len() as u16).to_le_bytes());
    v.extend_from_slice(data);
    v
}

/// Split one L2CAP frame into ACL packets of at most `mtu` data bytes.
pub fn fragment(handle: u16, frame: &[u8], mtu: usize) -> Vec<Vec<u8>> {
    let mtu = mtu.max(1);
    let mut out = Vec::new();
    for (i, c) in frame.chunks(mtu).enumerate() {
        out.push(acl_packet(handle, if i == 0 { PB_FIRST } else { PB_CONT }, c));
    }
    if frame.is_empty() {
        out.push(acl_packet(handle, PB_FIRST, &[]));
    }
    out
}

// ── the USB byte stream ─────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    /// `[code, plen]` + plen
    Event,
    /// `[handle lo, handle hi, len lo, len hi]` + len
    Acl,
}

/// Rebuilds whole packets from USB transfers (each at most one max-packet).
/// Bounded: a header claiming more than an HCI packet can hold, or a stream
/// that grows past 64 KiB, resets it (the bytes are dropped).
pub struct Stream {
    kind: Kind,
    buf: Vec<u8>,
    pub dropped: u32,
}

impl Stream {
    pub fn new(kind: Kind) -> Self {
        Stream { kind, buf: Vec::new(), dropped: 0 }
    }

    pub fn push(&mut self, data: &[u8]) {
        if self.buf.len() + data.len() > 65536 {
            self.buf.clear();
            self.dropped += 1;
            return;
        }
        self.buf.extend_from_slice(data);
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// The next whole packet, if one is in.
    pub fn next(&mut self) -> Option<Vec<u8>> {
        let total = match self.kind {
            Kind::Event => 2 + *self.buf.get(1)? as usize,
            Kind::Acl => {
                let n = le16(&self.buf, 2)? as usize;
                if n > 1021 + 4096 {
                    // no controller sends ACL packets that large: out of sync
                    self.buf.clear();
                    self.dropped += 1;
                    return None;
                }
                4 + n
            },
        };
        if self.buf.len() < total {
            return None;
        }
        let rest = self.buf.split_off(total);
        Some(core::mem::replace(&mut self.buf, rest))
    }
}
