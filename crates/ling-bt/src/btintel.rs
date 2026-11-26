//! Intel Bluetooth controllers ("btintel" in Linux): the AX201's BT half (USB
//! 8087:0026) and its relatives with the legacy (non-TLV) bootloader.
//!
//! The controller comes up in its bootloader with no firmware. The host
//!  1. sends Read Version (0xFC05): fw_variant 0x06 = bootloader, 0x23 = the
//!     operational firmware already runs;
//!  2. sends Read Boot Params (0xFC0D): device revision, lock state;
//!  3. names the firmware `ibt-<hw_variant>-<hw_revision>-<fw_revision>.sfi`
//!     (`ibt-<hw_variant>-<dev_revid>.sfi` for variants 0x0B/0x0C);
//!  4. checks the file against a pinned SHA-256 (this module) -- only files
//!     LingOS knows go to the radio;
//!  5. sends it with Secure Send (0xFC09) -- the 128-byte CSS header (type 0),
//!     the 256-byte public key (type 3), the 256-byte signature (type 2), then
//!     the payload's HCI commands in 4-byte-aligned data fragments (type 1),
//!     each Secure Send carrying at most 252 bytes. While in the bootloader
//!     0xFC09 goes over the bulk-OUT endpoint, and events may come back on the
//!     bulk-IN endpoint;
//!  6. waits for the vendor event "secure send result" (0xFF 0x06), sends
//!     Intel Reset (0xFC01) with the boot address the payload's Write Boot
//!     Params (0xFC0E) carried, and waits for "bootup" (0xFF 0x02);
//!  7. sends the `.ddc` device configuration (0xFC8B per entry).
//!
//! The controller checks the RSA signature itself; the hash pin is ours, so a
//! swapped file on the data disk never reaches it.

use crate::{le16, le32};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use sha2::{Digest, Sha256};

pub const OP_RESET: u16 = 0xFC01;
pub const OP_READ_VERSION: u16 = 0xFC05;
pub const OP_SECURE_SEND: u16 = 0xFC09;
pub const OP_READ_BOOT_PARAMS: u16 = 0xFC0D;
pub const OP_WRITE_BOOT_PARAMS: u16 = 0xFC0E;
pub const OP_WRITE_DDC: u16 = 0xFC8B;

pub const FW_VARIANT_BOOTLOADER: u8 = 0x06;
pub const FW_VARIANT_OPERATIONAL: u8 = 0x23;

/// Read Version's answer (legacy layout, 10 bytes with the status).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Version {
    pub status: u8,
    pub hw_platform: u8,
    pub hw_variant: u8,
    pub hw_revision: u8,
    pub fw_variant: u8,
    pub fw_revision: u8,
    pub fw_build_num: u8,
    pub fw_build_ww: u8,
    pub fw_build_yy: u8,
    pub fw_patch_num: u8,
}

/// Read Version (legacy form: no parameter).
pub fn read_version_cmd() -> Vec<u8> {
    crate::hci::command(OP_READ_VERSION, &[])
}

pub fn read_boot_params_cmd() -> Vec<u8> {
    crate::hci::command(OP_READ_BOOT_PARAMS, &[])
}

/// A Read Version answer in the TLV format (newer controllers: AX210 and on).
pub fn is_tlv(ret: &[u8]) -> bool {
    ret.len() != 10 || ret.get(1) != Some(&0x37)
}

pub fn parse_version(ret: &[u8]) -> Option<Version> {
    if ret.len() != 10 {
        return None;
    }
    Some(Version {
        status: ret[0],
        hw_platform: ret[1],
        hw_variant: ret[2],
        hw_revision: ret[3],
        fw_variant: ret[4],
        fw_revision: ret[5],
        fw_build_num: ret[6],
        fw_build_ww: ret[7],
        fw_build_yy: ret[8],
        fw_patch_num: ret[9],
    })
}

impl Version {
    pub fn describe(&self) -> String {
        format!(
            "Intel hw platform {:#04x} variant {:#04x} rev {}, firmware variant {:#04x} ({}) rev {} build {}-{}.{}",
            self.hw_platform,
            self.hw_variant,
            self.hw_revision,
            self.fw_variant,
            match self.fw_variant {
                FW_VARIANT_BOOTLOADER => "bootloader",
                FW_VARIANT_OPERATIONAL => "operational",
                _ => "unknown",
            },
            self.fw_revision,
            self.fw_build_num,
            self.fw_build_ww,
            2000 + self.fw_build_yy as u32
        )
    }

    /// Hardware this code knows how to boot (Linux btintel's legacy bootloader list).
    pub fn supported(&self) -> Result<(), String> {
        if self.status != 0 {
            return Err(format!("Read Version failed (status {:#04x})", self.status));
        }
        if self.hw_platform != 0x37 {
            return Err(format!("unknown Intel hardware platform {:#04x}", self.hw_platform));
        }
        match self.hw_variant {
            0x0B | 0x0C | 0x11 | 0x12 | 0x13 | 0x14 => Ok(()),
            v => Err(format!("Intel hw variant {v:#04x} has no bootloader support here")),
        }
    }
}

/// Read Boot Params' answer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BootParams {
    pub status: u8,
    pub dev_revid: u16,
    pub secure_boot: u8,
    pub otp_bdaddr: [u8; 6],
    pub min_fw_build_nn: u8,
    pub min_fw_build_cw: u8,
    pub min_fw_build_yy: u8,
    pub limited_cce: u8,
}

pub fn parse_boot_params(ret: &[u8]) -> Option<BootParams> {
    if ret.len() < 22 {
        return None;
    }
    Some(BootParams {
        status: ret[0],
        dev_revid: le16(ret, 4)?,
        secure_boot: ret[6],
        otp_bdaddr: ret[12..18].try_into().ok()?,
        min_fw_build_nn: ret[18],
        min_fw_build_cw: ret[19],
        min_fw_build_yy: ret[20],
        limited_cce: ret[21],
    })
}

/// The firmware file name (without "intel/"), `ext` "sfi" or "ddc".
pub fn fw_name(v: &Version, p: &BootParams, ext: &str) -> Option<String> {
    match v.hw_variant {
        0x0B | 0x0C => Some(format!("ibt-{}-{}.{ext}", v.hw_variant, p.dev_revid)),
        0x11..=0x14 => Some(format!("ibt-{}-{}-{}.{ext}", v.hw_variant, v.hw_revision, v.fw_revision)),
        _ => None,
    }
}

/// For a TLV-format controller: the name Linux would load (for the report
/// only -- the TLV/ECDSA download is not implemented).
pub fn tlv_fw_name(ret: &[u8]) -> Option<String> {
    let (mut cnvi, mut cnvr) = (None, None);
    let mut o = 1usize;
    while o + 2 <= ret.len() {
        let ty = ret[o];
        let len = ret[o + 1] as usize;
        let val = ret.get(o + 2..o + 2 + len)?;
        match ty {
            0x10 => cnvi = le32(val, 0),
            0x11 => cnvr = le32(val, 0),
            _ => {},
        }
        o += 2 + len;
    }
    let pack = |x: u32| -> u16 {
        let t = (x & 0xFFF) as u16;
        let s = ((x & 0x0F00_0000) >> 24) as u16;
        ((t << 4) | s).swap_bytes()
    };
    Some(format!("ibt-{:04x}-{:04x}.sfi", pack(cnvi?), pack(cnvr?)))
}

// ── firmware pinning ────────────────────────────────────────────────────────

/// SHA-256 of every Intel firmware file LingOS will send to a controller
/// (unmodified linux-firmware files, see ling-kernel/firmware/README.md).
/// A file not listed here, or with another hash, is refused.
/// Hashes taken 2026-11-24 from gitlab.com/kernel-firmware/linux-firmware (main).
pub const PINS: &[(&str, &str)] = &[
    // AX201 (variant 0x13 = 19): the file Linux loads, embedded in the kernel
    ("ibt-19-0-4.sfi", "2e6a2027b9eeb3aa94255f10752bb828077739104b8be6769c7500322edddfd6"),
    ("ibt-19-0-4.ddc", "fe272982577efdc289cfe3e8bedafca607c0b7d6c9df1aab1880e22ef930d077"),
    // the relatives, read from firmware/intel/ on storage when present
    ("ibt-20-0-3.sfi", "fd51628c307a1b03fbd7156f0d2a7f5af67b7202d4f358f5d4d807911982c3cc"),
    ("ibt-20-0-3.ddc", "fe272982577efdc289cfe3e8bedafca607c0b7d6c9df1aab1880e22ef930d077"),
    ("ibt-20-1-3.sfi", "fd51628c307a1b03fbd7156f0d2a7f5af67b7202d4f358f5d4d807911982c3cc"),
    ("ibt-20-1-3.ddc", "fe272982577efdc289cfe3e8bedafca607c0b7d6c9df1aab1880e22ef930d077"),
    ("ibt-20-1-4.sfi", "00e29ce24fc8f312309f9df0282345c8c1e6d0745245ed3a8dfe1de0a4b20289"),
    ("ibt-20-1-4.ddc", "fe272982577efdc289cfe3e8bedafca607c0b7d6c9df1aab1880e22ef930d077"),
    ("ibt-17-0-1.sfi", "43f033a45d3e594ca135eba95774d7e15ab7f5f868eecc20e14724cb9200a4cd"),
    ("ibt-17-0-1.ddc", "a251dac7382530032a8e74460edc5e22212437f70c5f57be101fb19e637aa5a8"),
    ("ibt-17-16-1.sfi", "a0604a0685a322e0ab2c3bd019bf862498c7b2b440ca6f515e0c8d7f849ac517"),
    ("ibt-17-16-1.ddc", "fe272982577efdc289cfe3e8bedafca607c0b7d6c9df1aab1880e22ef930d077"),
    ("ibt-18-0-1.sfi", "43f033a45d3e594ca135eba95774d7e15ab7f5f868eecc20e14724cb9200a4cd"),
    ("ibt-18-0-1.ddc", "a251dac7382530032a8e74460edc5e22212437f70c5f57be101fb19e637aa5a8"),
    ("ibt-18-16-1.sfi", "a0604a0685a322e0ab2c3bd019bf862498c7b2b440ca6f515e0c8d7f849ac517"),
    ("ibt-18-16-1.ddc", "fe272982577efdc289cfe3e8bedafca607c0b7d6c9df1aab1880e22ef930d077"),
    ("ibt-11-5.sfi", "62e0ffb26d8f04adb5b9a2838ec7c60840bdabb38c024a98b905b991ed08e000"),
    ("ibt-11-5.ddc", "9e57302abc15ac991b71350705c4b5060f69c7111813bc5034704f6edea30872"),
    ("ibt-12-16.sfi", "75b8b8cc10abd616a96066bce9ce62be80ebc293d6a4e9a0a11c3ecc5d3ca373"),
    ("ibt-12-16.ddc", "3ca252ad20e8f76d6d906536bf9e5abe6654c657183584a48e90a0dcf40ac4d5"),
];

/// linux-firmware's WHENCE links: names that are the same file as another.
pub fn canonical(name: &str) -> &str {
    let (stem, ext) = match name.rsplit_once('.') {
        Some(x) => x,
        None => return name,
    };
    let to = match stem {
        "ibt-19-0-0" | "ibt-19-0-1" | "ibt-19-0-3" | "ibt-19-16-4" | "ibt-19-32-0" | "ibt-19-32-1" | "ibt-19-32-4" | "ibt-19-240-1" | "ibt-19-240-4" => "ibt-19-0-4",
        "ibt-17-2" => "ibt-17-16-1",
        "ibt-18-2" => "ibt-18-16-1",
        "ibt-17-1" => "ibt-17-0-1",
        "ibt-18-1" => "ibt-18-0-1",
        _ => return name,
    };
    match ext {
        "sfi" => match to {
            "ibt-19-0-4" => "ibt-19-0-4.sfi",
            "ibt-17-16-1" => "ibt-17-16-1.sfi",
            "ibt-18-16-1" => "ibt-18-16-1.sfi",
            "ibt-17-0-1" => "ibt-17-0-1.sfi",
            _ => "ibt-18-0-1.sfi",
        },
        "ddc" => match to {
            "ibt-19-0-4" => "ibt-19-0-4.ddc",
            "ibt-17-16-1" => "ibt-17-16-1.ddc",
            "ibt-18-16-1" => "ibt-18-16-1.ddc",
            "ibt-17-0-1" => "ibt-17-0-1.ddc",
            _ => "ibt-18-0-1.ddc",
        },
        _ => name,
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    crate::hex(&Sha256::digest(data))
}

/// Ok when `data` is the pinned file `name`.
pub fn verify(name: &str, data: &[u8]) -> Result<(), String> {
    verify_with(PINS, name, data)
}

pub fn verify_with(pins: &[(&str, &str)], name: &str, data: &[u8]) -> Result<(), String> {
    let c = canonical(name);
    let Some((_, want)) = pins.iter().find(|(n, _)| *n == c) else {
        return Err(format!("{name} is not a firmware file LingOS knows (no pinned SHA-256): refused"));
    };
    let got = sha256_hex(data);
    if got != *want {
        return Err(format!("{name}: SHA-256 {got} does not match the pinned {want}: refused"));
    }
    Ok(())
}

// ── the .sfi download ───────────────────────────────────────────────────────

/// One Secure Send: fragment type and the byte range of the file it carries (<= 252 bytes).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chunk {
    pub frag: u8,
    pub off: usize,
    pub len: usize,
}

pub struct Plan {
    pub chunks: Vec<Chunk>,
    /// From the payload's Write Boot Params command; the Intel Reset boots it.
    pub boot_addr: u32,
}

const RSA_HEADER_LEN: usize = 644;
const MAX_FRAG: usize = 252;

fn split(out: &mut Vec<Chunk>, frag: u8, off: usize, len: usize) {
    let mut o = 0;
    while o < len {
        let n = (len - o).min(MAX_FRAG);
        out.push(Chunk { frag, off: off + o, len: n });
        o += n;
    }
}

/// Plan the download of an `.sfi` file (RSA-signed, legacy bootloader).
pub fn plan_sfi(fw: &[u8]) -> Result<Plan, String> {
    if fw.len() < RSA_HEADER_LEN + 3 {
        return Err(String::from("firmware file too short"));
    }
    let mut chunks = Vec::new();
    split(&mut chunks, 0x00, 0, 128); // CSS header
    split(&mut chunks, 0x03, 128, 256); // public key
    split(&mut chunks, 0x02, 388, 256); // signature
    let mut boot_addr = 0u32;
    let mut ptr = RSA_HEADER_LEN;
    let mut frag_len = 0usize;
    while ptr + frag_len < fw.len() {
        let c = ptr + frag_len;
        let (Some(opcode), Some(&plen)) = (le16(fw, c), fw.get(c + 2)) else {
            return Err(format!("firmware truncated in a command header at {c}"));
        };
        if c + 3 + plen as usize > fw.len() {
            return Err(format!("firmware truncated in a command at {c}"));
        }
        if opcode == OP_WRITE_BOOT_PARAMS {
            boot_addr = le32(fw, c + 3).ok_or("bad boot params")?;
        }
        frag_len += 3 + plen as usize;
        // the Secure Send parameter length must be a multiple of 4; the file
        // carries Intel NOPs so that the command groups line up
        if frag_len % 4 == 0 {
            split(&mut chunks, 0x01, ptr, frag_len);
            ptr += frag_len;
            frag_len = 0;
        }
    }
    if frag_len != 0 {
        return Err(String::from("firmware payload does not end on a 4-byte boundary"));
    }
    if boot_addr == 0 {
        return Err(String::from("firmware has no Write Boot Params command"));
    }
    Ok(Plan { chunks, boot_addr })
}

/// The Secure Send command for one chunk.
pub fn secure_send_cmd(fw: &[u8], c: &Chunk) -> Vec<u8> {
    let mut p = Vec::with_capacity(1 + c.len);
    p.push(c.frag);
    p.extend_from_slice(&fw[c.off..c.off + c.len]);
    crate::hci::command(OP_SECURE_SEND, &p)
}

/// Intel Reset into the downloaded firmware.
pub fn reset_cmd(boot_addr: u32) -> Vec<u8> {
    let mut p = alloc::vec![0x00, 0x01, 0x00, 0x01];
    p.extend_from_slice(&boot_addr.to_le_bytes());
    crate::hci::command(OP_RESET, &p)
}

/// The `.ddc` file as Write DDC commands (one per entry: length byte, id, data).
pub fn ddc_cmds(ddc: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut out = Vec::new();
    let mut o = 0usize;
    while o < ddc.len() {
        let n = ddc[o] as usize + 1;
        let e = ddc.get(o..o + n).ok_or("ddc entry runs past the end of the file")?;
        out.push(crate::hci::command(OP_WRITE_DDC, e));
        o += n;
    }
    Ok(out)
}

/// Intel vendor events (0xFF) the download waits for.
#[derive(Debug, PartialEq)]
pub enum VendorEvt {
    /// The operational firmware has started.
    Bootup,
    /// The download finished: result 0 = accepted.
    SecureSendResult { result: u8, opcode: u16, status: u8 },
    Other(u8),
}

pub fn parse_vendor(params: &[u8]) -> Option<VendorEvt> {
    Some(match *params.first()? {
        0x02 => VendorEvt::Bootup,
        0x06 => VendorEvt::SecureSendResult { result: *params.get(1)?, opcode: le16(params, 2)?, status: *params.get(4)? },
        x => VendorEvt::Other(x),
    })
}
