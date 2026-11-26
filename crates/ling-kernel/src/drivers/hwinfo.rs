//! hwinfo: the PC information report behind the Specs app (`os_hwinfo()`)
//! and the `hwinfo` SSH / Terminal command -- a CPU-Z / HWiNFO style view.
//!
//! The report is plain text, one fact per line:
//!   `#Category/Sub`   starts a tree node (the path's parents are implied);
//!   `key\tvalue`      is a property of the node above it.
//! `text()` turns it into a readable listing (the Export file, the command).
//!
//! Sources: CPUID (+ the cpudrv MSR readouts), the firmware's SMBIOS tables
//! (BIOS, board, DIMMs, slots, caches), ACPI's table directory, the PCI bus,
//! the E820 / UEFI memory map and the drivers' cached state (GPU, NVMe,
//! SATA, HD Audio, NICs, Wi-Fi, USB devices). Gathering never waits on a
//! device: only config-space reads, firmware tables in RAM and what the
//! drivers already hold. The SMBIOS / ACPI tables are found through the
//! `smbios=` / `acpi=` addresses lingboot puts on the command line (UEFI),
//! else by scanning the BIOS area 0xE0000-0xFFFFF for their anchors.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::pci;

static BUSY: AtomicBool = AtomicBool::new(false);
static mut LAST: Option<String> = None;
/// The disk lingboot started from: found once (it reads partition tables).
static mut BOOT_DISK: Option<Option<crate::fs::disks::DiskId>> = None;

fn boot_disk() -> Option<crate::fs::disks::DiskId> {
    let b = unsafe { &mut *(&raw mut BOOT_DISK) };
    if b.is_none() {
        *b = Some(crate::fs::disks::boot_disk());
    }
    b.unwrap()
}

struct R {
    s: String,
}

impl R {
    fn node(&mut self, path: &str) {
        self.s.push('#');
        self.s.push_str(&clean(path));
        self.s.push('\n');
    }
    fn kv(&mut self, k: &str, v: &str) {
        let v = clean(v);
        if v.is_empty() {
            return;
        }
        self.s.push_str(&clean(k));
        self.s.push('\t');
        self.s.push_str(&v);
        self.s.push('\n');
    }
}

/// No tabs / line breaks / control bytes inside a key or value.
fn clean(s: &str) -> String {
    s.chars().map(|c| if c == '\t' || c == '\n' || c == '\r' || (c as u32) < 0x20 { ' ' } else { c }).collect::<String>().trim().to_string()
}

/// A node name segment: no '/' (it separates levels).
fn seg(s: &str) -> String {
    clean(s).replace('/', "-")
}

fn mib(bytes: u64) -> String {
    if bytes >= 10 << 30 {
        format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
    } else if bytes >= 1 << 30 {
        format!("{:.2} GiB ({} MiB)", bytes as f64 / (1u64 << 30) as f64, bytes >> 20)
    } else if bytes >= 1 << 20 {
        format!("{} MiB", bytes >> 20)
    } else {
        format!("{} KiB", bytes >> 10)
    }
}

fn kib(bytes: u64) -> String {
    if bytes >= 1 << 20 && bytes % (1 << 20) == 0 {
        format!("{} MiB", bytes >> 20)
    } else {
        format!("{} KiB", bytes >> 10)
    }
}

fn disk_size(bytes: u64) -> String {
    // drives are sold in decimal units; show both
    let gb = bytes as f64 / 1e9;
    if gb >= 1000.0 {
        format!("{:.2} TB ({})", gb / 1000.0, mib(bytes))
    } else {
        format!("{:.1} GB ({})", gb, mib(bytes))
    }
}

fn ip_str(ip: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

fn mac_str(m: [u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

fn secs_words(mut t: u64) -> String {
    let d = t / 86400;
    t %= 86400;
    let (h, m, s) = (t / 3600, (t % 3600) / 60, t % 60);
    if d > 0 { format!("{d} d {h} h {m} min") } else if h > 0 { format!("{h} h {m} min {s} s") } else { format!("{m} min {s} s") }
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

// ── physical memory (firmware tables) ───────────────────────────────────────

/// Copy `len` bytes of physical memory (identity-mapped below 4 GiB; mapped
/// uncached on demand above). None for 0 / absurd ranges.
fn phys(addr: u64, len: usize) -> Option<Vec<u8>> {
    if addr == 0 || len == 0 || len > (1 << 20) {
        return None;
    }
    let a = crate::arch::paging::map_mmio(addr, len as u64)?;
    let mut v = alloc::vec![0u8; len];
    for (i, b) in v.iter_mut().enumerate() {
        *b = unsafe { core::ptr::read_volatile((a as usize + i) as *const u8) };
    }
    Some(v)
}

fn u16le(b: &[u8], o: usize) -> u16 {
    if o + 2 <= b.len() { u16::from_le_bytes([b[o], b[o + 1]]) } else { 0 }
}
fn u32le(b: &[u8], o: usize) -> u32 {
    if o + 4 <= b.len() { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) } else { 0 }
}
fn u64le(b: &[u8], o: usize) -> u64 {
    if o + 8 <= b.len() { u64::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7]]) } else { 0 }
}
fn byte(b: &[u8], o: usize) -> u8 {
    b.get(o).copied().unwrap_or(0)
}
fn ascii(b: &[u8]) -> String {
    b.iter().map(|&c| if (0x20..0x7F).contains(&c) { c as char } else { ' ' }).collect::<String>().trim().to_string()
}

/// `key=<number>` from the boot command line (lingboot passes smbios= / acpi=).
fn cmdline_num(key: &str) -> Option<u64> {
    let cl = crate::arch::bootmodule::cmdline()?;
    for t in cl.split_whitespace() {
        if let Some(v) = t.strip_prefix(key).and_then(|r| r.strip_prefix('=')) {
            let n = if let Some(h) = v.strip_prefix("0x") { u64::from_str_radix(h, 16).ok() } else { v.parse::<u64>().ok() };
            if let Some(n) = n.filter(|&n| n != 0) {
                return Some(n);
            }
        }
    }
    None
}

/// Scan [lo, hi) on 16-byte boundaries for `anchor` where `ok` accepts the
/// bytes from there on (a checksummed entry point, not the BIOS code's own
/// copy of the anchor string -- SeaBIOS has "_SM3_" in its code).
fn scan(lo: u64, hi: u64, anchor: &[u8], ok: fn(&[u8]) -> bool) -> Option<u64> {
    let area = phys(lo, (hi - lo) as usize)?;
    let mut o = 0usize;
    while o + anchor.len() <= area.len() {
        if &area[o..o + anchor.len()] == anchor && ok(&area[o..]) {
            return Some(lo + o as u64);
        }
        o += 16;
    }
    None
}

fn sum_ok(b: &[u8], len: usize) -> bool {
    len >= 16 && len <= b.len() && b[..len].iter().fold(0u8, |a, &x| a.wrapping_add(x)) == 0
}

fn sm3_ok(b: &[u8]) -> bool {
    b.len() >= 0x18 && sum_ok(b, b[6] as usize) && b[7] >= 3
}

fn sm2_ok(b: &[u8]) -> bool {
    b.len() >= 0x1F && sum_ok(b, b[5] as usize) && &b[0x10..0x15] == b"_DMI_"
}

fn rsdp_ok(b: &[u8]) -> bool {
    sum_ok(b, 20)
}

// ── SMBIOS ──────────────────────────────────────────────────────────────────

struct Smbios {
    ver: (u8, u8, u8),
    entry: u64,
    table_addr: u64,
    found_by: &'static str,
    table: Vec<u8>,
}

/// One structure: its formatted area and its strings.
struct St<'a> {
    t: u8,
    handle: u16,
    f: &'a [u8],
    strs: Vec<&'a [u8]>,
}

impl<'a> St<'a> {
    fn b(&self, o: usize) -> u8 {
        byte(self.f, o)
    }
    fn w(&self, o: usize) -> u16 {
        u16le(self.f, o)
    }
    fn d(&self, o: usize) -> u32 {
        u32le(self.f, o)
    }
    fn q(&self, o: usize) -> u64 {
        u64le(self.f, o)
    }
    fn has(&self, o: usize) -> bool {
        o < self.f.len()
    }
    /// The string a formatted-area byte at `o` points at ("" for none).
    fn s(&self, o: usize) -> String {
        if !self.has(o) {
            return String::new();
        }
        let i = self.f[o] as usize;
        if i == 0 || i > self.strs.len() {
            return String::new();
        }
        ascii(self.strs[i - 1])
    }
}

static mut SMBIOS: Option<Option<Smbios>> = None;

fn smbios() -> Option<&'static Smbios> {
    let slot = unsafe { &mut *(&raw mut SMBIOS) };
    if slot.is_none() {
        *slot = Some(load_smbios());
    }
    slot.as_ref().unwrap().as_ref()
}

fn load_smbios() -> Option<Smbios> {
    let (entry, by) = match cmdline_num("smbios") {
        Some(a) => (a, "UEFI configuration table (lingboot)"),
        None => {
            let a = scan(0xF0000, 0x100000, b"_SM3_", sm3_ok).or_else(|| scan(0xF0000, 0x100000, b"_SM_", sm2_ok))?;
            (a, "BIOS area scan (0xF0000)")
        },
    };
    let ep = phys(entry, 32)?;
    if &ep[0..5] == b"_SM3_" {
        let max = u32le(&ep, 0x0C) as usize;
        let addr = u64le(&ep, 0x10);
        let table = phys(addr, max.clamp(4, 1 << 20))?;
        return Some(Smbios { ver: (ep[7], ep[8], ep[9]), entry, table_addr: addr, found_by: by, table });
    }
    if &ep[0..4] == b"_SM_" && &ep[0x10..0x15] == b"_DMI_" {
        let len = u16le(&ep, 0x16) as usize;
        let addr = u32le(&ep, 0x18) as u64;
        let table = phys(addr, len.clamp(4, 1 << 20))?;
        return Some(Smbios { ver: (ep[6], ep[7], 0), entry, table_addr: addr, found_by: by, table });
    }
    None
}

fn structures(t: &[u8]) -> Vec<St<'_>> {
    let mut out = Vec::new();
    let mut o = 0usize;
    while o + 4 <= t.len() && out.len() < 1024 {
        let ty = t[o];
        let len = t[o + 1] as usize;
        if len < 4 || o + len > t.len() {
            break;
        }
        let f = &t[o..o + len];
        // strings: NUL-terminated, the set ends with an extra NUL
        let mut p = o + len;
        let mut strs = Vec::new();
        if p + 1 < t.len() && t[p] == 0 && t[p + 1] == 0 {
            p += 2;
        } else {
            while p < t.len() {
                let start = p;
                while p < t.len() && t[p] != 0 {
                    p += 1;
                }
                strs.push(&t[start..p]);
                p += 1;
                if p >= t.len() || t[p] == 0 {
                    p += 1;
                    break;
                }
            }
        }
        out.push(St { t: ty, handle: u16le(f, 2), f, strs });
        if ty == 127 {
            break;
        }
        o = p;
    }
    out
}

fn chassis_name(t: u8) -> &'static str {
    match t & 0x7F {
        3 => "Desktop",
        4 => "Low-profile desktop",
        5 => "Pizza box",
        6 => "Mini tower",
        7 => "Tower",
        8 => "Portable",
        9 => "Laptop",
        10 => "Notebook",
        11 => "Hand-held",
        12 => "Docking station",
        13 => "All-in-one",
        14 => "Sub-notebook",
        15 => "Space-saving",
        16 => "Lunch box",
        17 => "Main server chassis",
        23 => "Rack mount",
        24 => "Sealed-case PC",
        30 => "Tablet",
        31 => "Convertible",
        32 => "Detachable",
        35 => "Mini PC",
        36 => "Stick PC",
        1 => "Other",
        _ => "Unknown",
    }
}

fn mem_type_name(t: u8) -> &'static str {
    match t {
        0x03 => "DRAM",
        0x07 => "RAM",
        0x0F => "SDRAM",
        0x12 => "DDR",
        0x13 => "DDR2",
        0x14 => "DDR2 FB-DIMM",
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        0x01 => "Other",
        _ => "Unknown",
    }
}

fn form_factor_name(t: u8) -> &'static str {
    match t {
        0x09 => "DIMM",
        0x0D => "SO-DIMM",
        0x0F => "FB-DIMM",
        0x0B => "Row of chips",
        0x0C => "RIMM",
        0x10 => "Die",
        0x0E => "SRIMM",
        0x0A => "TSOP",
        0x08 => "SIMM",
        _ => "Other",
    }
}

fn slot_type_name(t: u8) -> &'static str {
    match t {
        0x06 => "PCI",
        0x0E | 0x0F | 0x10 | 0x11 | 0x13 => "AGP",
        0x17..=0x1E => "M.2",
        0x1F => "PCIe Gen2 SFF-8639 (U.2)",
        0x20 => "PCIe Gen3 SFF-8639 (U.2)",
        0x21 => "PCIe Mini 52-pin",
        0x22 => "PCIe Mini 52-pin",
        0x23 => "PCIe Mini 76-pin",
        0x24 => "PCIe Gen4 SFF-8639 (U.2)",
        0x25 => "PCIe Gen5 SFF-8639 (U.2)",
        0x26 => "OCP NIC 3.0 SFF",
        0x27 => "OCP NIC 3.0 LFF",
        0xA5 => "PCI Express",
        0xA6 => "PCI Express x1",
        0xA7 => "PCI Express x2",
        0xA8 => "PCI Express x4",
        0xA9 => "PCI Express x8",
        0xAA => "PCI Express x16",
        0xAB => "PCI Express Gen2",
        0xAC => "PCI Express Gen2 x1",
        0xAD => "PCI Express Gen2 x2",
        0xAE => "PCI Express Gen2 x4",
        0xAF => "PCI Express Gen2 x8",
        0xB0 => "PCI Express Gen2 x16",
        0xB1 => "PCI Express Gen3",
        0xB2 => "PCI Express Gen3 x1",
        0xB3 => "PCI Express Gen3 x2",
        0xB4 => "PCI Express Gen3 x4",
        0xB5 => "PCI Express Gen3 x8",
        0xB6 => "PCI Express Gen3 x16",
        0xB8 => "PCI Express Gen4",
        0xB9 => "PCI Express Gen4 x1",
        0xBA => "PCI Express Gen4 x2",
        0xBB => "PCI Express Gen4 x4",
        0xBC => "PCI Express Gen4 x8",
        0xBD => "PCI Express Gen4 x16",
        0xBE => "PCI Express Gen5",
        0xBF => "PCI Express Gen5 x1",
        0xC0 => "PCI Express Gen5 x2",
        0xC1 => "PCI Express Gen5 x4",
        0xC2 => "PCI Express Gen5 x8",
        0xC3 => "PCI Express Gen5 x16",
        0xC4 => "PCI Express Gen6",
        _ => "Other",
    }
}

fn slot_width_name(w: u8) -> &'static str {
    match w {
        0x08 => "x1",
        0x09 => "x2",
        0x0A => "x4",
        0x0B => "x8",
        0x0C => "x12",
        0x0D => "x16",
        0x0E => "x32",
        0x03 => "8-bit",
        0x04 => "16-bit",
        0x05 => "32-bit",
        0x06 => "64-bit",
        _ => "",
    }
}

fn cache_size_smbios(v: u16) -> u64 {
    let n = (v & 0x7FFF) as u64;
    if v & 0x8000 != 0 { n * 64 * 1024 } else { n * 1024 }
}

fn assoc_name(a: u8) -> String {
    String::from(match a {
        0x03 => "direct mapped",
        0x04 => "2-way",
        0x05 => "4-way",
        0x06 => "fully associative",
        0x07 => "8-way",
        0x08 => "16-way",
        0x09 => "12-way",
        0x0A => "24-way",
        0x0B => "32-way",
        0x0C => "48-way",
        0x0D => "64-way",
        0x0E => "20-way",
        _ => "",
    })
}

fn uuid_str(b: &[u8], ver: (u8, u8, u8)) -> String {
    if b.len() < 16 {
        return String::new();
    }
    if b.iter().all(|&x| x == 0) {
        return String::from("(not set)");
    }
    if b.iter().all(|&x| x == 0xFF) {
        return String::from("(not present)");
    }
    // SMBIOS 2.6+: the first three fields are little-endian
    let le = ver.0 > 2 || (ver.0 == 2 && ver.1 >= 6);
    let (a, bb, c) = if le {
        (u32le(b, 0), u16le(b, 4), u16le(b, 6))
    } else {
        (u32::from_be_bytes([b[0], b[1], b[2], b[3]]), u16::from_be_bytes([b[4], b[5]]), u16::from_be_bytes([b[6], b[7]]))
    };
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        a, bb, c, b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

fn smbios_sections(r: &mut R) -> Vec<(String, Vec<(String, String)>)> {
    // returns the DIMM nodes for the Memory section (path suffix, rows)
    let mut dimms: Vec<(String, Vec<(String, String)>)> = Vec::new();
    let Some(sm) = smbios() else {
        r.node("Motherboard");
        r.kv("SMBIOS", "not found (no smbios= from the boot loader, no _SM_ / _SM3_ entry point in 0xF0000-0xFFFFF)");
        r.node("BIOS");
        r.kv("Firmware tables", "no SMBIOS: vendor, version and date unknown");
        return dimms;
    };
    let all = structures(&sm.table);
    let ver = sm.ver;
    let of = |t: u8| all.iter().filter(move |s| s.t == t);

    // Motherboard: system (1), board (2), chassis (3)
    r.node("Motherboard");
    for s in of(2).take(1) {
        r.kv("Manufacturer", &s.s(4));
        r.kv("Model", &s.s(5));
        r.kv("Version", &s.s(6));
        r.kv("Serial number", &s.s(7));
        r.kv("Asset tag", &s.s(8));
        r.kv("Location in chassis", &s.s(0x0A));
    }
    r.node("Motherboard/System");
    for s in of(1).take(1) {
        r.kv("Manufacturer", &s.s(4));
        r.kv("Product", &s.s(5));
        r.kv("Version", &s.s(6));
        r.kv("Serial number", &s.s(7));
        if s.has(0x17) {
            r.kv("UUID", &uuid_str(&s.f[8..24.min(s.f.len())], ver));
        }
        if s.has(0x18) {
            r.kv(
                "Wake-up type",
                match s.b(0x18) {
                    6 => "Power switch",
                    5 => "PCI PME#",
                    4 => "Modem ring",
                    3 => "APM timer",
                    7 => "AC power restored",
                    8 => "LAN remote",
                    _ => "Other / unknown",
                },
            );
        }
        r.kv("SKU", &s.s(0x19));
        r.kv("Family", &s.s(0x1A));
    }
    r.node("Motherboard/Chassis");
    for s in of(3).take(1) {
        r.kv("Manufacturer", &s.s(4));
        r.kv("Type", chassis_name(s.b(5)));
        r.kv("Version", &s.s(6));
        r.kv("Serial number", &s.s(7));
        r.kv("Asset tag", &s.s(8));
    }
    // Slots (9)
    let slots: Vec<&St> = of(9).collect();
    if !slots.is_empty() {
        r.node("Motherboard/Slots");
        for s in &slots {
            let use_ = match s.b(7) {
                3 => "available",
                4 => "in use",
                5 => "unavailable",
                _ => "usage unknown",
            };
            let w = slot_width_name(s.b(6));
            let mut v = String::from(slot_type_name(s.b(5)));
            if !w.is_empty() && !v.contains(w) {
                v.push_str(&format!(", {w} wide"));
            }
            v.push_str(&format!(", {use_}"));
            if s.has(0x0F) {
                let (seg_, bus, df) = (s.w(0x0D), s.b(0x0F), s.b(0x10));
                if s.b(7) == 4 && bus != 0xFF && seg_ != 0xFFFF {
                    v.push_str(&format!(" ({:02x}:{:02x}.{})", bus, df >> 3, df & 7));
                }
            }
            let name = s.s(4);
            r.kv(if name.is_empty() { "Slot" } else { &name }, &v);
        }
    }
    // On-board devices (41)
    let onb: Vec<&St> = of(41).collect();
    if !onb.is_empty() {
        r.node("Motherboard/On-board devices");
        for s in &onb {
            let kind = match s.b(5) & 0x7F {
                3 => "Video",
                4 => "SCSI controller",
                5 => "Ethernet",
                6 => "Token ring",
                7 => "Sound",
                8 => "PATA controller",
                9 => "SATA controller",
                10 => "SAS controller",
                11 => "Wireless LAN",
                12 => "Bluetooth",
                13 => "WWAN",
                14 => "eMMC",
                15 => "NVMe controller",
                16 => "UFS controller",
                _ => "Other",
            };
            r.kv(&s.s(4), &format!("{kind}{}", if s.b(5) & 0x80 != 0 { ", enabled" } else { ", disabled" }));
        }
    }
    // Port connectors (8): just the names
    let ports: Vec<&St> = of(8).collect();
    if !ports.is_empty() {
        r.node("Motherboard/Connectors");
        for s in &ports {
            let int = s.s(4);
            let ext = s.s(6);
            let kind = match s.b(8) {
                0x10 => "USB",
                0x0B => "RJ-45",
                0x1F => "Mini jack (headphones)",
                0x0F => "PS/2",
                0x07 => "DB-9 male (serial)",
                0x08 => "DB-9 female",
                0x09 => "DB-25 male",
                0x0A => "DB-25 female",
                0x0D => "Mini-DIN",
                0x1C => "Mini-Centronics",
                0x21 => "IEEE 1394",
                0x22 => "SAS/SATA plug receptacle",
                0x23 => "USB Type-C",
                0xFF => "Other",
                _ => "",
            };
            let name = if !ext.is_empty() { ext } else { int.clone() };
            if name.is_empty() {
                continue;
            }
            let v = if !int.is_empty() && int != name { format!("{kind} (internal {int})") } else { String::from(if kind.is_empty() { "connector" } else { kind }) };
            r.kv(&name, &v);
        }
    }

    // BIOS (0)
    r.node("BIOS");
    for s in of(0).take(1) {
        r.kv("Vendor", &s.s(4));
        r.kv("Version", &s.s(5));
        r.kv("Release date", &s.s(8));
        let rom = if s.b(9) == 0xFF && s.has(0x19) {
            let e = s.w(0x18);
            let n = (e & 0x3FFF) as u64;
            if e >> 14 == 1 { n << 30 } else { n << 20 }
        } else {
            (s.b(9) as u64 + 1) * 64 * 1024
        };
        r.kv("ROM size", &kib(rom));
        if s.has(0x15) && s.b(0x14) != 0xFF {
            r.kv("System BIOS release", &format!("{}.{}", s.b(0x14), s.b(0x15)));
        }
        if s.has(0x17) && s.b(0x16) != 0xFF {
            r.kv("Embedded controller firmware", &format!("{}.{}", s.b(0x16), s.b(0x17)));
        }
        let ch = s.q(0x0A);
        let mut feats: Vec<&str> = Vec::new();
        for (bit, n) in [(7, "PCI"), (9, "Plug and Play"), (11, "upgradeable (flash)"), (12, "shadowing"), (15, "boot from CD"), (16, "selectable boot")] {
            if ch & (1 << bit) != 0 {
                feats.push(n);
            }
        }
        if s.has(0x13) {
            let x2 = s.b(0x13);
            if x2 & 0x08 != 0 {
                feats.push("UEFI");
            }
            if x2 & 0x10 != 0 {
                feats.push("virtual machine");
            }
            if x2 & 0x01 != 0 {
                feats.push("network boot");
            }
        }
        if s.has(0x12) && s.b(0x12) & 0x01 != 0 {
            feats.push("ACPI");
        }
        if s.has(0x12) && s.b(0x12) & 0x08 != 0 {
            feats.push("USB legacy");
        }
        r.kv("Characteristics", &feats.join(", "));
    }
    r.node("BIOS/SMBIOS");
    r.kv("Version", &if sm.ver.2 > 0 { format!("{}.{}.{}", sm.ver.0, sm.ver.1, sm.ver.2) } else { format!("{}.{}", sm.ver.0, sm.ver.1) });
    r.kv("Entry point", &format!("0x{:x} ({})", sm.entry, sm.found_by));
    r.kv("Table", &format!("0x{:x}, {} bytes, {} structures", sm.table_addr, sm.table.len(), all.len()));
    let mut counts: Vec<(u8, usize)> = Vec::new();
    for s in &all {
        match counts.iter_mut().find(|c| c.0 == s.t) {
            Some(c) => c.1 += 1,
            None => counts.push((s.t, 1)),
        }
    }
    counts.sort_by_key(|c| c.0);
    r.kv("Structure types", &counts.iter().map(|(t, n)| if *n > 1 { format!("{t} (x{n})") } else { format!("{t}") }).collect::<Vec<_>>().join(", "));
    // OEM strings (11)
    for s in of(11).take(1) {
        let list: Vec<String> = s.strs.iter().map(|b| ascii(b)).filter(|x| !x.is_empty()).take(8).collect();
        r.kv("OEM strings", &list.join(" | "));
    }

    // CPU socket (4) + caches (7): the CPU section's "Socket" node
    r.node("CPU/Socket (SMBIOS)");
    let caches: Vec<&St> = of(7).collect();
    for s in of(4) {
        r.kv("Socket", &s.s(4));
        r.kv("Manufacturer", &s.s(7));
        r.kv("Version", &s.s(0x10));
        let ext = s.w(0x12);
        if ext != 0 {
            r.kv("External clock", &format!("{ext} MHz"));
        }
        r.kv("Max speed", &format!("{} MHz", s.w(0x14)));
        r.kv("Current speed (at boot)", &format!("{} MHz", s.w(0x16)));
        let v = s.b(0x11);
        if v & 0x80 != 0 {
            r.kv("Voltage", &format!("{:.1} V", (v & 0x7F) as f64 / 10.0));
        }
        let st = s.b(0x18);
        r.kv(
            "Status",
            &format!(
                "{}, {}",
                if st & 0x40 != 0 { "populated" } else { "empty" },
                match st & 7 {
                    1 => "enabled",
                    2 => "disabled by user",
                    3 => "disabled by BIOS (POST error)",
                    4 => "idle",
                    _ => "unknown",
                }
            ),
        );
        if s.has(0x25) {
            let (mut cores, mut en, mut th) = (s.b(0x23) as u16, s.b(0x24) as u16, s.b(0x25) as u16);
            if s.has(0x2F) {
                if cores == 0xFF {
                    cores = s.w(0x2A);
                }
                if en == 0xFF {
                    en = s.w(0x2C);
                }
                if th == 0xFF {
                    th = s.w(0x2E);
                }
            }
            r.kv("Cores / enabled / threads", &format!("{cores} / {en} / {th}"));
        }
        r.kv("Serial number", &s.s(0x20));
        r.kv("Part number", &s.s(0x22));
        let id = s.q(8);
        if id != 0 {
            r.kv("Processor ID", &format!("{:016X}", id));
        }
        for (k, off) in [("L1 cache", 0x1A), ("L2 cache", 0x1C), ("L3 cache", 0x1E)] {
            let h = s.w(off);
            if let Some(c) = caches.iter().find(|c| c.handle == h) {
                let mut size = cache_size_smbios(c.w(9)) as u64;
                if c.has(0x1A) && c.d(0x17) != 0 {
                    let e = c.d(0x17);
                    let n = (e & 0x7FFF_FFFF) as u64;
                    size = if e & 0x8000_0000 != 0 { n * 64 * 1024 } else { n * 1024 };
                }
                r.kv(k, &format!("{} {}", kib(size), c.s(4)));
            }
        }
    }
    if !caches.is_empty() {
        r.node("CPU/Caches (SMBIOS)");
        for c in &caches {
            let cfg = c.w(5);
            let level = (cfg & 7) + 1;
            let kind = match c.b(0x11) {
                3 => "instruction",
                4 => "data",
                5 => "unified",
                _ => "",
            };
            let mut size = cache_size_smbios(c.w(9));
            if c.has(0x1A) && c.d(0x17) != 0 {
                let e = c.d(0x17);
                let n = (e & 0x7FFF_FFFF) as u64;
                size = if e & 0x8000_0000 != 0 { n * 64 * 1024 } else { n * 1024 };
            }
            let name = c.s(4);
            r.kv(
                &if name.is_empty() { format!("L{level}") } else { name },
                &format!("L{level} {kind} {}, {}{}", kib(size), assoc_name(c.b(0x12)), if cfg & 0x80 != 0 { "" } else { ", disabled" }),
            );
        }
    }

    // Memory arrays (16) and devices (17)
    for (i, s) in of(16).enumerate() {
        let mut cap = s.d(7) as u64 * 1024;
        if s.d(7) == 0x8000_0000 && s.has(0x16) {
            cap = s.q(0x0F);
        }
        let ecc = match s.b(6) {
            3 => "none",
            4 => "parity",
            5 => "single-bit ECC",
            6 => "multi-bit ECC",
            7 => "CRC",
            _ => "unknown",
        };
        dimms.push((
            format!("Array {}", i + 1),
            alloc::vec![
                (String::from("Slots"), format!("{}", s.w(0x0D))),
                (String::from("Max capacity"), mib(cap)),
                (String::from("Error correction"), String::from(ecc)),
                (
                    String::from("Use"),
                    String::from(match s.b(5) {
                        3 => "system memory",
                        4 => "video memory",
                        5 => "flash",
                        6 => "NVRAM",
                        7 => "cache",
                        _ => "other",
                    })
                ),
            ],
        ));
    }
    for s in of(17) {
        let loc = s.s(0x10);
        let bank = s.s(0x11);
        let raw = s.w(0x0C);
        let name = if loc.is_empty() { format!("DIMM 0x{:04x}", s.handle) } else { loc.clone() };
        let mut rows: Vec<(String, String)> = Vec::new();
        let size: u64 = if raw == 0 {
            0
        } else if raw == 0x7FFF && s.has(0x1F) {
            (s.d(0x1C) & 0x7FFF_FFFF) as u64 * (1 << 20)
        } else if raw == 0xFFFF {
            u64::MAX
        } else if raw & 0x8000 != 0 {
            (raw & 0x7FFF) as u64 * 1024
        } else {
            raw as u64 * (1 << 20)
        };
        if size == 0 {
            rows.push((String::from("Size"), String::from("empty slot")));
            rows.push((String::from("Bank"), bank));
            dimms.push((format!("Slot {}", seg(&name)), rows));
            continue;
        }
        rows.push((String::from("Size"), if size == u64::MAX { String::from("unknown") } else { mib(size) }));
        rows.push((String::from("Type"), String::from(mem_type_name(s.b(0x12)))));
        rows.push((String::from("Form factor"), String::from(form_factor_name(s.b(0x0E)))));
        let mut speed = s.w(0x15) as u32;
        if speed == 0xFFFF && s.has(0x57) {
            speed = s.d(0x54);
        }
        if speed != 0 {
            rows.push((String::from("Speed (max)"), format!("{speed} MT/s")));
        }
        if s.has(0x21) {
            let mut cs = s.w(0x20) as u32;
            if cs == 0xFFFF && s.has(0x5B) {
                cs = s.d(0x58);
            }
            if cs != 0 {
                rows.push((String::from("Configured speed"), format!("{cs} MT/s")));
            }
        }
        rows.push((String::from("Manufacturer"), s.s(0x17)));
        rows.push((String::from("Part number"), s.s(0x1A)));
        rows.push((String::from("Serial number"), s.s(0x18)));
        rows.push((String::from("Bank"), bank));
        if s.has(0x1B) && s.b(0x1B) & 0xF != 0 {
            rows.push((String::from("Rank"), format!("{}", s.b(0x1B) & 0xF)));
        }
        let (tw, dw) = (s.w(8), s.w(0x0A));
        if dw != 0 && dw != 0xFFFF {
            rows.push((String::from("Data width"), format!("{dw} bits{}", if tw > dw && tw != 0xFFFF { format!(" ({tw} total: ECC)") } else { String::new() })));
        }
        if s.has(0x27) {
            let mv = s.w(0x26);
            if mv != 0 {
                rows.push((String::from("Configured voltage"), format!("{:.2} V", mv as f64 / 1000.0)));
            }
        }
        dimms.push((seg(&name), rows));
    }
    dimms
}

// ── ACPI ────────────────────────────────────────────────────────────────────

fn acpi_section(r: &mut R) {
    r.node("ACPI");
    let (rsdp, by) = match cmdline_num("acpi") {
        Some(a) => (Some(a), "UEFI configuration table (lingboot)"),
        None => {
            let ebda = phys(0x40E, 2).map(|b| (u16le(&b, 0) as u64) << 4).unwrap_or(0);
            let a = if (0x80000..0xA0000).contains(&ebda) { scan(ebda, ebda + 1024, b"RSD PTR ", rsdp_ok) } else { None };
            (a.or_else(|| scan(0xE0000, 0x100000, b"RSD PTR ", rsdp_ok)), "BIOS area scan")
        },
    };
    let Some(rsdp) = rsdp else {
        r.kv("RSDP", "not found");
        return;
    };
    let Some(p) = phys(rsdp, 36) else { return };
    if &p[0..8] != b"RSD PTR " {
        r.kv("RSDP", &format!("0x{rsdp:x}: no RSD PTR signature"));
        return;
    }
    let rev = p[15];
    r.kv("RSDP", &format!("0x{rsdp:x} ({by})"));
    r.kv("OEM", &ascii(&p[9..15]));
    r.kv("Revision", if rev >= 2 { "ACPI 2.0+" } else { "ACPI 1.0" });
    let (root, wide) = if rev >= 2 && u64le(&p, 24) != 0 { (u64le(&p, 24), true) } else { (u32le(&p, 16) as u64, false) };
    let Some(h) = phys(root, 36) else { return };
    let len = (u32le(&h, 4) as usize).clamp(36, 4096);
    let Some(t) = phys(root, len) else { return };
    r.kv(if wide { "XSDT" } else { "RSDT" }, &format!("0x{root:x}, OEM {} {}", ascii(&t[10..16]), ascii(&t[16..24])));
    let step = if wide { 8 } else { 4 };
    let mut tables: Vec<(String, String)> = Vec::new();
    let mut o = 36;
    while o + step <= t.len() && tables.len() < 64 {
        let a = if wide { u64le(&t, o) } else { u32le(&t, o) as u64 };
        o += step;
        let Some(th) = phys(a, 36) else { continue };
        let sig = ascii(&th[0..4]);
        let tl = u32le(&th, 4);
        tables.push((sig.clone(), format!("rev {}, {} bytes, OEM {} {} at 0x{a:x}", th[8], tl, ascii(&th[10..16]), ascii(&th[16..24]))));
        let full = || phys(a, (tl as usize).clamp(36, 1 << 16));
        if sig == "APIC" {
            if let Some(m) = full() {
                let (mut cpus, mut en, mut io) = (0, 0, 0);
                let mut q = 44;
                while q + 2 <= m.len() {
                    let (ty, l) = (m[q], m[q + 1] as usize);
                    if l < 2 {
                        break;
                    }
                    match ty {
                        0 => {
                            cpus += 1;
                            if u32le(&m, q + 4) & 1 != 0 {
                                en += 1;
                            }
                        },
                        9 => {
                            cpus += 1;
                            if u32le(&m, q + 8) & 1 != 0 {
                                en += 1;
                            }
                        },
                        1 => io += 1,
                        _ => {},
                    }
                    q += l;
                }
                r.kv("Processors (MADT)", &format!("{en} enabled of {cpus} listed"));
                r.kv("I/O APICs", &format!("{io}"));
            }
        } else if sig == "FACP" {
            if let Some(f) = full() {
                r.kv(
                    "PM profile",
                    match byte(&f, 45) {
                        1 => "Desktop",
                        2 => "Mobile",
                        3 => "Workstation",
                        4 => "Enterprise server",
                        5 => "SOHO server",
                        6 => "Appliance PC",
                        7 => "Performance server",
                        8 => "Tablet",
                        _ => "Unspecified",
                    },
                );
                r.kv("FADT revision", &format!("{}.{}", byte(&f, 8), if f.len() > 131 { byte(&f, 131) & 0xF } else { 0 }));
            }
        } else if sig == "MCFG" {
            if let Some(m) = full() {
                if m.len() >= 60 {
                    r.kv("PCIe config space (MCFG)", &format!("0x{:x}, buses {:02x}-{:02x}", u64le(&m, 44), m[54], m[55]));
                }
            }
        }
    }
    r.node("ACPI/Tables");
    for (k, v) in tables {
        r.kv(&k, &v);
    }
}

// ── PCI ─────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct PDev {
    bus: u8,
    dev: u8,
    func: u8,
    vendor: u16,
    device: u16,
    class: u8,
    sub: u8,
    prog: u8,
    rev: u8,
    ssv: u16,
    ssid: u16,
    htype: u8,
    irq: u8,
}

impl PDev {
    fn bdf(&self) -> String {
        format!("{:02x}:{:02x}.{}", self.bus, self.dev, self.func)
    }
    fn r(&self, off: u8) -> u32 {
        pci::read32(self.bus, self.dev, self.func, off)
    }
}

fn pci_scan() -> Vec<PDev> {
    let mut out = Vec::new();
    for bus in 0..=255u8 {
        for dev in 0..32u8 {
            if pci::read32(bus, dev, 0, 0) & 0xFFFF == 0xFFFF {
                continue;
            }
            let multi = (pci::read32(bus, dev, 0, 0x0C) >> 16) & 0x80 != 0;
            for func in 0..if multi { 8u8 } else { 1 } {
                let id = pci::read32(bus, dev, func, 0);
                if id & 0xFFFF == 0xFFFF || id == 0 {
                    continue;
                }
                let cr = pci::read32(bus, dev, func, 8);
                let ht = ((pci::read32(bus, dev, func, 0x0C) >> 16) & 0x7F) as u8;
                let ss = if ht == 0 { pci::read32(bus, dev, func, 0x2C) } else { 0 };
                out.push(PDev {
                    bus,
                    dev,
                    func,
                    vendor: id as u16,
                    device: (id >> 16) as u16,
                    class: (cr >> 24) as u8,
                    sub: (cr >> 16) as u8,
                    prog: (cr >> 8) as u8,
                    rev: cr as u8,
                    ssv: ss as u16,
                    ssid: (ss >> 16) as u16,
                    htype: ht,
                    irq: pci::read32(bus, dev, func, 0x3C) as u8,
                });
            }
        }
    }
    out
}

/// (current gen, lanes, max gen, max lanes) from the PCI Express capability.
fn pcie_link(d: &PDev) -> Option<(u8, u8, u8, u8)> {
    if d.r(0x04) >> 16 & 0x10 == 0 {
        return None;
    }
    let mut p = (d.r(0x34) & 0xFC) as u8;
    let mut n = 0;
    while p >= 0x40 && n < 48 {
        let h = d.r(p);
        if h & 0xFF == 0x10 {
            let cap = d.r(p + 0x0C);
            let st = d.r(p + 0x10) >> 16;
            return Some(((st & 0xF) as u8, ((st >> 4) & 0x3F) as u8, (cap & 0xF) as u8, ((cap >> 4) & 0x3F) as u8));
        }
        p = ((h >> 8) & 0xFC) as u8;
        n += 1;
    }
    None
}

fn bars(d: &PDev) -> Vec<String> {
    let mut out = Vec::new();
    let n = match d.htype {
        0 => 6,
        1 => 2,
        _ => 0,
    };
    let mut i = 0u8;
    while i < n {
        let v = d.r(0x10 + i * 4);
        if v == 0 {
            i += 1;
            continue;
        }
        if v & 1 != 0 {
            out.push(format!("BAR{i} I/O 0x{:x}", v & 0xFFFC));
            i += 1;
        } else if (v >> 1) & 3 == 2 && i + 1 < n {
            let hi = d.r(0x10 + (i + 1) * 4) as u64;
            out.push(format!("BAR{i} mem64 0x{:x}{}", (hi << 32) | (v & 0xFFFF_FFF0) as u64, if v & 8 != 0 { " prefetchable" } else { "" }));
            i += 2;
        } else {
            out.push(format!("BAR{i} mem 0x{:x}{}", v & 0xFFFF_FFF0, if v & 8 != 0 { " prefetchable" } else { "" }));
            i += 1;
        }
    }
    out
}

pub fn vendor_name(v: u16) -> &'static str {
    match v {
        0x8086 | 0x8087 => "Intel",
        0x1022 => "AMD",
        0x1002 => "AMD / ATI",
        0x10DE => "NVIDIA",
        0x10EC => "Realtek",
        0x144D => "Samsung",
        0x15B7 => "Sandisk / Western Digital",
        0x1179 => "Toshiba",
        0x1E0F => "Kioxia",
        0xC0A9 => "Micron / Crucial",
        0x1344 => "Micron",
        0x1987 => "Phison",
        0x1CC1 => "ADATA",
        0x126F => "Silicon Motion",
        0x2646 => "Kingston",
        0x1C5C => "SK hynix",
        0x1E4B => "MAXIO",
        0x1E49 => "YMTC",
        0x1B4B | 0x11AB => "Marvell",
        0x1B21 => "ASMedia",
        0x14E4 => "Broadcom",
        0x1000 => "Broadcom / LSI",
        0x168C | 0x1969 => "Qualcomm Atheros",
        0x17CB => "Qualcomm",
        0x14C3 => "MediaTek",
        0x1D6A => "Aquantia / Marvell",
        0x1912 => "Renesas",
        0x104C => "Texas Instruments",
        0x1106 => "VIA",
        0x1283 => "ITE",
        0x1095 => "Silicon Image",
        0x15B3 => "Mellanox / NVIDIA",
        0x1D17 => "Zhaoxin",
        0x1234 => "QEMU / Bochs",
        0x1B36 => "Red Hat (QEMU)",
        0x1AF4 => "Red Hat / virtio",
        0x80EE => "VirtualBox",
        0x15AD => "VMware",
        0x1414 => "Microsoft",
        0x1D0F => "Amazon",
        0x1043 => "ASUSTeK",
        0x1458 => "Gigabyte",
        0x1462 => "MSI",
        0x1849 => "ASRock",
        0x1028 => "Dell",
        0x103C => "HP",
        0x17AA => "Lenovo",
        0x1025 => "Acer",
        0x106B => "Apple",
        0x1569 => "Palit",
        0x19DA => "Zotac",
        0x3842 => "EVGA",
        0x1682 => "XFX",
        0x174B => "Sapphire",
        0x1DA2 => "Sapphire",
        0x148C => "PowerColor",
        0x7377 => "Colorful",
        0x1B4C => "Galax / KFA2",
        0x10B5 => "PLX / Broadcom",
        0x1217 => "O2 Micro",
        0x9005 => "Adaptec",
        _ => "",
    }
}

fn device_name(v: u16, d: u16) -> &'static str {
    match (v, d) {
        // QEMU / virtual machines
        (0x8086, 0x1237) => "440FX host bridge",
        (0x8086, 0x7000) => "PIIX3 ISA bridge",
        (0x8086, 0x7010) => "PIIX3 IDE controller",
        (0x8086, 0x7020) => "PIIX3 USB UHCI",
        (0x8086, 0x7113) => "PIIX4 ACPI / SMBus",
        (0x8086, 0x100E) => "82540EM Gigabit Ethernet (e1000)",
        (0x8086, 0x100F) => "82545EM Gigabit Ethernet",
        (0x8086, 0x10D3) => "82574L Gigabit Ethernet (e1000e)",
        (0x8086, 0x10C9) => "82576 Gigabit Ethernet",
        (0x8086, 0x29C0) => "82G33 / Q35 host bridge",
        (0x8086, 0x2918) => "ICH9 LPC bridge",
        (0x8086, 0x2922) => "ICH9 AHCI SATA controller",
        (0x8086, 0x2930) => "ICH9 SMBus",
        (0x8086, 0x293E) => "ICH9 HD Audio",
        (0x8086, 0x2668) => "ICH6 HD Audio",
        (0x8086, 0x2415) => "82801AA AC'97 audio",
        (0x8086, 0x24CD) => "ICH4 USB2 EHCI",
        (0x8086, 0x2934) | (0x8086, 0x2935) | (0x8086, 0x2936) => "ICH9 USB UHCI",
        (0x8086, 0x293A) => "ICH9 USB2 EHCI",
        (0x1234, 0x1111) => "Standard VGA (Bochs DISPI)",
        (0x1B36, 0x000D) => "QEMU xHCI USB 3 controller",
        (0x1B36, 0x0010) => "QEMU NVMe controller",
        (0x1B36, 0x0100) => "QXL display",
        (0x1B36, 0x0001) => "QEMU PCI-PCI bridge",
        (0x1B36, 0x000C) => "QEMU PCIe root port",
        (0x1AF4, 0x1000) | (0x1AF4, 0x1041) => "virtio network",
        (0x1AF4, 0x1001) | (0x1AF4, 0x1042) => "virtio block",
        (0x1AF4, 0x1050) => "virtio GPU",
        (0x80EE, 0xBEEF) => "VirtualBox graphics adapter",
        (0x80EE, 0xCAFE) => "VirtualBox guest service",
        (0x15AD, 0x0405) => "SVGA II adapter",
        // Intel desktop (the 500 series / Rocket Lake platform and friends)
        (0x8086, 0x15F3) => "Ethernet Controller I225-V (2.5 GbE)",
        (0x8086, 0x15F2) => "Ethernet Controller I225-LM (2.5 GbE)",
        (0x8086, 0x125C) => "Ethernet Controller I226-V (2.5 GbE)",
        (0x8086, 0x125B) => "Ethernet Controller I226-LM (2.5 GbE)",
        (0x8086, 0x15B8) => "Ethernet Connection I219-V",
        (0x8086, 0x0D4F) => "Ethernet Connection I219-V",
        (0x8086, 0x43F0) => "Wi-Fi 6 AX201 160MHz (CNVi)",
        (0x8086, 0xA0F0) => "Wi-Fi 6 AX201 160MHz (CNVi)",
        (0x8086, 0x06F0) => "Wi-Fi 6 AX201 (CNVi)",
        (0x8086, 0x2723) => "Wi-Fi 6 AX200",
        (0x8086, 0x2725) => "Wi-Fi 6E AX210 / AX211",
        (0x8086, 0x51F0) | (0x8086, 0x7AF0) => "Wi-Fi 6E AX211 (CNVi)",
        (0x8086, 0x43ED) => "500 Series USB 3.2 xHCI controller",
        (0x8086, 0x43EF) => "500 Series shared SRAM",
        (0x8086, 0x43E0) => "500 Series Management Engine interface",
        (0x8086, 0x43C8) => "500 Series HD Audio",
        (0x8086, 0x43A3) => "500 Series SMBus",
        (0x8086, 0x43A4) => "500 Series SPI controller",
        (0x8086, 0x4C43) => "Rocket Lake host bridge / DRAM controller",
        (0x8086, 0x4C8A) => "UHD Graphics 750 (Rocket Lake)",
        (0x8086, 0x4C8B) => "UHD Graphics 730 (Rocket Lake)",
        (0x8086, 0x4680) | (0x8086, 0x4690) => "UHD Graphics 770 (Alder Lake)",
        (0x8086, 0xA780) => "UHD Graphics 770 (Raptor Lake)",
        (0x8086, 0x3E92) | (0x8086, 0x3E98) => "UHD Graphics 630",
        (0x8086, 0x9A49) => "Iris Xe Graphics (Tiger Lake)",
        (0x8086, 0x7AE0) => "700 Series USB 3.2 xHCI controller",
        (0x8086, 0x7A60) => "600 Series USB 3.2 xHCI controller",
        (0x8086, 0xA36D) => "Cannon Lake USB 3.1 xHCI",
        // NVIDIA
        (0x10DE, 0x2489) => "GeForce RTX 3060 Ti LHR (GA104)",
        (0x10DE, 0x2486) => "GeForce RTX 3060 Ti (GA104)",
        (0x10DE, 0x2484) => "GeForce RTX 3070 (GA104)",
        (0x10DE, 0x2488) => "GeForce RTX 3070 LHR (GA104)",
        (0x10DE, 0x2482) => "GeForce RTX 3070 Ti (GA104)",
        (0x10DE, 0x2206) => "GeForce RTX 3080 (GA102)",
        (0x10DE, 0x2216) => "GeForce RTX 3080 LHR (GA102)",
        (0x10DE, 0x2208) => "GeForce RTX 3080 Ti (GA102)",
        (0x10DE, 0x2204) => "GeForce RTX 3090 (GA102)",
        (0x10DE, 0x2503) => "GeForce RTX 3060 (GA106)",
        (0x10DE, 0x2504) => "GeForce RTX 3060 LHR (GA106)",
        (0x10DE, 0x2507) => "GeForce RTX 3050 (GA106)",
        (0x10DE, 0x2684) => "GeForce RTX 4090 (AD102)",
        (0x10DE, 0x2704) => "GeForce RTX 4080 (AD103)",
        (0x10DE, 0x2782) => "GeForce RTX 4070 Ti (AD104)",
        (0x10DE, 0x2786) => "GeForce RTX 4070 (AD104)",
        (0x10DE, 0x2803) => "GeForce RTX 4060 Ti (AD106)",
        (0x10DE, 0x2882) => "GeForce RTX 4060 (AD107)",
        (0x10DE, 0x1E84) => "GeForce RTX 2070 SUPER (TU104)",
        (0x10DE, 0x1F08) => "GeForce RTX 2060 (TU106)",
        (0x10DE, 0x2182) => "GeForce GTX 1660 Ti (TU116)",
        (0x10DE, 0x1B80) => "GeForce GTX 1080 (GP104)",
        (0x10DE, 0x1B81) => "GeForce GTX 1070 (GP104)",
        (0x10DE, 0x228B) => "GA104 High Definition Audio",
        (0x10DE, 0x228E) => "GA106 High Definition Audio",
        (0x10DE, 0x1AEF) => "GA102 High Definition Audio",
        (0x10DE, 0x22BA) => "AD102 High Definition Audio",
        // Realtek
        (0x10EC, 0x8168) => "RTL8111/8168 Gigabit Ethernet",
        (0x10EC, 0x8125) => "RTL8125 2.5 GbE",
        (0x10EC, 0x8139) => "RTL8139 Fast Ethernet",
        (0x10EC, 0xC821) => "RTL8821CE Wi-Fi",
        (0x10EC, 0xB852) => "RTL8852BE Wi-Fi 6",
        // storage
        (0x144D, 0xA808) => "NVMe SSD 970 EVO / PM981",
        (0x144D, 0xA809) => "NVMe SSD 980",
        (0x144D, 0xA80A) => "NVMe SSD 980 PRO / 990 PRO",
        (0x144D, 0xA80C) => "NVMe SSD 990 EVO",
        (0x15B7, 0x5006) => "WD Black SN750",
        (0x15B7, 0x5011) => "WD Black SN850",
        (0xC0A9, 0x540A) => "Crucial P2 NVMe",
        (0xC0A9, 0x5407) => "Crucial P5 NVMe",
        (0xC0A9, 0x5426) | (0xC0A9, 0x5427) => "Crucial / Micron NVMe SSD",
        (0x1B21, 0x1242) => "ASM1142 USB 3.1 xHCI",
        (0x1B21, 0x2142) => "ASM2142 USB 3.1 xHCI",
        (0x1B21, 0x0612) => "ASM1062 SATA controller",
        (0x1022, 0x43EE) => "500 Series USB 3.1 xHCI (AMD)",
        _ => "",
    }
}

fn class_name(c: u8, s: u8, p: u8) -> &'static str {
    match (c, s) {
        (0x00, 0x01) => "VGA-compatible (legacy)",
        (0x01, 0x00) => "SCSI controller",
        (0x01, 0x01) => "IDE controller",
        (0x01, 0x02) => "Floppy controller",
        (0x01, 0x04) => "RAID controller",
        (0x01, 0x05) => "ATA controller",
        (0x01, 0x06) if p == 1 => "SATA controller (AHCI)",
        (0x01, 0x06) => "SATA controller",
        (0x01, 0x07) => "SAS controller",
        (0x01, 0x08) if p == 2 => "NVMe controller",
        (0x01, 0x08) => "Non-volatile memory controller",
        (0x01, _) => "Storage controller",
        (0x02, 0x00) => "Ethernet controller",
        (0x02, 0x80) => "Network controller (Wi-Fi / other)",
        (0x02, _) => "Network controller",
        (0x03, 0x00) => "VGA display controller",
        (0x03, 0x01) => "XGA display controller",
        (0x03, 0x02) => "3D controller",
        (0x03, _) => "Display controller",
        (0x04, 0x00) => "Video device",
        (0x04, 0x01) => "Audio device",
        (0x04, 0x03) => "HD Audio controller",
        (0x04, _) => "Multimedia controller",
        (0x05, 0x00) => "RAM controller",
        (0x05, _) => "Memory controller",
        (0x06, 0x00) => "Host bridge",
        (0x06, 0x01) => "ISA bridge",
        (0x06, 0x04) => "PCI-to-PCI bridge",
        (0x06, 0x07) => "CardBus bridge",
        (0x06, _) => "Bridge",
        (0x07, 0x00) => "Serial controller",
        (0x07, 0x80) => "Communication controller",
        (0x07, _) => "Communication controller",
        (0x08, 0x00) => "Interrupt controller",
        (0x08, 0x01) => "DMA controller",
        (0x08, 0x02) => "Timer",
        (0x08, 0x03) => "RTC",
        (0x08, 0x05) => "SD host controller",
        (0x08, 0x06) => "IOMMU",
        (0x08, _) => "System peripheral",
        (0x09, _) => "Input controller",
        (0x0B, _) => "Processor",
        (0x0C, 0x00) => "FireWire controller",
        (0x0C, 0x03) => match p {
            0x00 => "USB UHCI controller",
            0x10 => "USB OHCI controller",
            0x20 => "USB 2 EHCI controller",
            0x30 => "USB 3 xHCI controller",
            0x40 => "USB4 controller",
            _ => "USB controller",
        },
        (0x0C, 0x05) => "SMBus controller",
        (0x0C, 0x07) => "IPMI interface",
        (0x0C, 0x80) => "Serial bus controller",
        (0x0C, _) => "Serial bus controller",
        (0x0D, 0x11) => "Bluetooth controller",
        (0x0D, _) => "Wireless controller",
        (0x10, _) => "Encryption controller",
        (0x11, _) => "Signal processing controller",
        (0x12, _) => "Processing accelerator",
        (0x13, _) => "Instrumentation",
        _ => "Device",
    }
}

fn pci_label(d: &PDev) -> String {
    let vn = vendor_name(d.vendor);
    let dn = device_name(d.vendor, d.device);
    match (vn.is_empty(), dn.is_empty()) {
        (false, false) => format!("{vn} {dn}"),
        (false, true) => format!("{vn} {}", class_name(d.class, d.sub, d.prog)),
        (true, false) => String::from(dn),
        (true, true) => String::from(class_name(d.class, d.sub, d.prog)),
    }
}

fn pci_rows(r: &mut R, d: &PDev) {
    r.kv("Location", &d.bdf());
    r.kv("Device", &pci_label(d));
    r.kv("Vendor : device", &format!("{:04x}:{:04x} ({})", d.vendor, d.device, if vendor_name(d.vendor).is_empty() { "unknown vendor" } else { vendor_name(d.vendor) }));
    r.kv("Class", &format!("{} ({:02x}.{:02x}.{:02x})", class_name(d.class, d.sub, d.prog), d.class, d.sub, d.prog));
    r.kv("Revision", &format!("{:02x}", d.rev));
    if d.ssv != 0 && d.ssv != 0xFFFF {
        let sv = vendor_name(d.ssv);
        r.kv("Subsystem", &format!("{:04x}:{:04x}{}", d.ssv, d.ssid, if sv.is_empty() { String::new() } else { format!(" ({sv})") }));
    }
    if let Some((g, w, mg, mw)) = pcie_link(d) {
        if w > 0 {
            r.kv("PCIe link", &format!("Gen{g} x{w} (max Gen{mg} x{mw})"));
        }
    }
    let b = bars(d);
    if !b.is_empty() {
        r.kv("Resources", &b.join(", "));
    }
    if d.irq != 0 && d.irq != 0xFF {
        r.kv("IRQ line", &format!("{}", d.irq));
    }
}

// ── CPU ─────────────────────────────────────────────────────────────────────

fn cpuid(leaf: u32, sub: u32) -> (u32, u32, u32, u32) {
    unsafe { crate::arch::cpu::cpuid(leaf, sub) }
}

fn flags(bits: u32, names: &[(u32, &'static str)]) -> Vec<&'static str> {
    names.iter().filter(|(b, _)| bits & (1 << b) != 0).map(|(_, n)| *n).collect()
}

fn cpu_section(r: &mut R) {
    let f = crate::arch::cpudrv::features();
    let (max_leaf, vb, vc, vd) = cpuid(0, 0);
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&vb.to_le_bytes());
    vendor[4..8].copy_from_slice(&vd.to_le_bytes());
    vendor[8..12].copy_from_slice(&vc.to_le_bytes());
    let vendor = ascii(&vendor);
    let amd = vendor == "AuthenticAMD";
    let (a1, b1, c1, d1) = cpuid(1, 0);
    let max_ext = cpuid(0x8000_0000, 0).0;
    let (smt, pkg) = crate::arch::cpudrv::topology();
    r.node("CPU");
    r.kv("Name", &crate::arch::cpudrv::brand());
    r.kv(
        "Vendor",
        &format!(
            "{vendor} ({})",
            match vendor.as_str() {
                "GenuineIntel" => "Intel",
                "AuthenticAMD" => "AMD",
                "TCGTCGTCGTCG" => "QEMU TCG",
                "HygonGenuine" => "Hygon",
                "CentaurHauls" | "  Shanghai  " => "Zhaoxin / VIA",
                _ => "other",
            }
        ),
    );
    r.kv("Family / model / stepping", &format!("{} (0x{:X}) / {} (0x{:02X}) / {}", f.family, f.family, f.model, f.model, f.stepping));
    r.kv("Signature (CPUID 1)", &format!("0x{a1:08X}"));
    if let Some(m) = crate::arch::cpudrv::microcode() {
        r.kv("Microcode", &format!("0x{m:X}"));
    }
    let cores = (pkg / smt.max(1)).max(1);
    r.kv("Cores / threads (package)", &format!("{cores} / {pkg}"));
    r.kv("Threads per core", &format!("{smt}"));
    r.kv("Logical processors (CPUID 1)", &format!("{}", (b1 >> 16) & 0xFF));
    r.kv("Cores running LingOS", &format!("{} online, {} doing parallel work", crate::arch::smp::cores_online(), crate::arch::smp::cores()));
    r.kv("Local APIC", if crate::arch::smp::x2apic() { "x2APIC" } else { "xAPIC" });
    let base = crate::arch::cpudrv::base_mhz();
    if base > 0 {
        r.kv("Base clock", &format!("{base} MHz"));
    }
    if f.max_mhz > 0 {
        r.kv("Max turbo", &format!("{} MHz", f.max_mhz));
    }
    r.kv("TSC rate", &format!("{} MHz", crate::arch::timer::tsc_mhz()));
    if let Some(m) = crate::arch::cpudrv::measure_mhz(10_000) {
        r.kv("Current clock (APERF/MPERF)", &format!("{m} MHz"));
    } else {
        r.kv("Current clock (APERF/MPERF)", if f.hypervisor { "not readable in a virtual machine" } else { "not available" });
    }
    if let Some(t) = crate::arch::cpudrv::package_temp() {
        r.kv("Package temperature", &format!("{t} C"));
    }
    if let Some(t) = crate::arch::cpudrv::core_temp() {
        r.kv("Boot core temperature", &format!("{t} C"));
    }
    let tj = crate::arch::cpudrv::tjmax();
    if tj > 0 {
        r.kv("TjMax", &format!("{tj} C"));
    }
    let hwp = crate::arch::cpudrv::hwp_request_now();
    r.kv(
        "Power management",
        &if hwp != 0 {
            format!("HWP on (energy preference {})", (hwp >> 24) & 0xFF)
        } else if f.hwp {
            String::from("HWP present, not enabled")
        } else {
            String::from("firmware P-states")
        },
    );
    r.kv("Turbo", yes(f.turbo));
    r.kv("Running in a virtual machine", yes(f.hypervisor));
    if f.hypervisor {
        let (_, hb, hc, hd) = cpuid(0x4000_0000, 0);
        let mut hv = [0u8; 12];
        hv[0..4].copy_from_slice(&hb.to_le_bytes());
        hv[4..8].copy_from_slice(&hc.to_le_bytes());
        hv[8..12].copy_from_slice(&hd.to_le_bytes());
        let hv = ascii(&hv);
        if !hv.is_empty() {
            r.kv(
                "Hypervisor",
                &format!(
                    "{hv}{}",
                    match hv.as_str() {
                        "KVMKVMKVM" => " (KVM)",
                        "TCGTCGTCGTCG" => " (QEMU TCG)",
                        "VBoxVBoxVBox" => " (VirtualBox)",
                        "Microsoft Hv" => " (Hyper-V)",
                        "VMwareVMware" => " (VMware)",
                        _ => "",
                    }
                ),
            );
        }
    }
    if max_ext >= 0x8000_0008 {
        let a = cpuid(0x8000_0008, 0).0;
        r.kv("Address sizes", &format!("{} bits physical, {} bits virtual", a & 0xFF, (a >> 8) & 0xFF));
    }
    r.kv("Max CPUID leaf", &format!("0x{max_leaf:X} / 0x{max_ext:X}"));

    // feature flags
    let (_, b7, c7, d7) = if max_leaf >= 7 { cpuid(7, 0) } else { (0, 0, 0, 0) };
    let a71 = if max_leaf >= 7 && cpuid(7, 0).0 >= 1 { cpuid(7, 1).0 } else { 0 };
    let (_, _, ce, de) = if max_ext >= 0x8000_0001 { cpuid(0x8000_0001, 0) } else { (0, 0, 0, 0) };
    let inv_tsc = max_ext >= 0x8000_0007 && cpuid(0x8000_0007, 0).3 & (1 << 8) != 0;
    r.node("CPU/Features");
    let mut simd: Vec<&str> = Vec::new();
    simd.extend(flags(d1, &[(23, "MMX"), (25, "SSE"), (26, "SSE2")]));
    simd.extend(flags(c1, &[(0, "SSE3"), (9, "SSSE3"), (19, "SSE4.1"), (20, "SSE4.2")]));
    simd.extend(flags(ce, &[(6, "SSE4a"), (11, "XOP"), (16, "FMA4")]));
    simd.extend(flags(c1, &[(28, "AVX"), (12, "FMA3"), (29, "F16C")]));
    simd.extend(flags(b7, &[(5, "AVX2")]));
    simd.extend(flags(a71, &[(4, "AVX-VNNI"), (5, "AVX512-BF16")]));
    r.kv("SIMD", &simd.join(" "));
    let avx512 = {
        let mut v = flags(b7, &[(16, "F"), (17, "DQ"), (21, "IFMA"), (26, "PF"), (27, "ER"), (28, "CD"), (30, "BW"), (31, "VL")]);
        v.extend(flags(c7, &[(1, "VBMI"), (6, "VBMI2"), (11, "VNNI"), (12, "BITALG"), (14, "VPOPCNTDQ")]));
        v.extend(flags(d7, &[(2, "4VNNIW"), (3, "4FMAPS"), (8, "VP2INTERSECT"), (23, "FP16")]));
        v
    };
    r.kv("AVX-512", &if avx512.is_empty() { String::from("none") } else { avx512.join(" ") });
    let mut amx = flags(d7, &[(22, "AMX-BF16"), (24, "AMX-TILE"), (25, "AMX-INT8")]);
    if !amx.is_empty() {
        amx.sort();
        r.kv("AMX", &amx.join(" "));
    }
    let mut crypto = flags(c1, &[(25, "AES-NI"), (1, "PCLMULQDQ"), (30, "RDRAND")]);
    crypto.extend(flags(b7, &[(29, "SHA"), (18, "RDSEED")]));
    crypto.extend(flags(c7, &[(9, "VAES"), (10, "VPCLMULQDQ"), (8, "GFNI"), (23, "Key Locker")]));
    r.kv("Crypto", &crypto.join(" "));
    let mut virt = flags(c1, &[(5, "VT-x (VMX)"), (6, "SMX (TXT)")]);
    virt.extend(flags(ce, &[(2, "AMD-V (SVM)")]));
    r.kv("Virtualization", &if virt.is_empty() { String::from("none reported") } else { virt.join(" ") });
    let mut sec = flags(b7, &[(7, "SMEP"), (20, "SMAP"), (2, "SGX"), (14, "MPX")]);
    sec.extend(flags(c7, &[(2, "UMIP"), (3, "PKU"), (7, "CET-SS"), (13, "TME"), (31, "PKS")]));
    sec.extend(flags(d7, &[(20, "CET-IBT"), (10, "MD_CLEAR"), (26, "IBRS/IBPB"), (27, "STIBP"), (28, "L1D_FLUSH"), (29, "ARCH_CAPABILITIES"), (31, "SSBD")]));
    sec.extend(flags(de, &[(20, "NX")]));
    r.kv("Security", &sec.join(" "));
    let mut bits = flags(b7, &[(3, "BMI1"), (8, "BMI2"), (19, "ADX"), (9, "ERMS"), (4, "HLE"), (11, "RTM"), (23, "CLFLUSHOPT"), (24, "CLWB"), (0, "FSGSBASE"), (10, "INVPCID")]);
    bits.extend(flags(c1, &[(23, "POPCNT"), (22, "MOVBE"), (13, "CX16"), (26, "XSAVE"), (17, "PCID"), (21, "x2APIC"), (24, "TSC-deadline"), (3, "MONITOR/MWAIT"), (7, "EIST"), (8, "TM2")]));
    bits.extend(flags(ce, &[(5, "LZCNT/ABM"), (0, "LAHF"), (8, "PREFETCHW"), (21, "TBM"), (22, "TOPOEXT")]));
    bits.extend(flags(de, &[(11, "SYSCALL"), (26, "1 GiB pages"), (27, "RDTSCP"), (29, "x86-64")]));
    bits.extend(flags(c7, &[(5, "WAITPKG"), (16, "LA57"), (22, "RDPID"), (25, "CLDEMOTE"), (27, "MOVDIRI"), (28, "MOVDIR64B")]));
    bits.extend(flags(d7, &[(4, "FSRM"), (14, "SERIALIZE"), (15, "Hybrid"), (18, "PCONFIG")]));
    if inv_tsc {
        bits.push("Invariant TSC");
    }
    bits.extend(flags(d1, &[(28, "HTT"), (29, "TM"), (22, "ACPI thermal"), (9, "APIC"), (16, "PAT"), (12, "MTRR")]));
    r.kv("Other", &bits.join(" "));
    let isa = if b7 & (1 << 16) != 0 && b7 & (1 << 17) != 0 && b7 & (1 << 30) != 0 && b7 & (1 << 31) != 0 {
        "x86-64-v4"
    } else if b7 & (1 << 5) != 0 && b7 & (1 << 3) != 0 && b7 & (1 << 8) != 0 && c1 & (1 << 12) != 0 && c1 & (1 << 22) != 0 {
        "x86-64-v3"
    } else if c1 & (1 << 20) != 0 && c1 & (1 << 23) != 0 && c1 & (1 << 9) != 0 {
        "x86-64-v2"
    } else {
        "x86-64"
    };
    r.kv("Microarchitecture level", isa);
    r.kv(
        "Enabled by LingOS",
        &format!(
            "SSE (kernel){}{}{}",
            if crate::arch::cpudrv::avx2() { ", AVX2 + FMA" } else { "" },
            if crate::arch::cpudrv::avx512() { ", AVX-512" } else { "" },
            if f.smep { ", SMEP" } else { "" }
        ),
    );

    // caches: CPUID 4 (Intel) / 0x8000001D (AMD)
    r.node("CPU/Caches");
    let leaf = if amd && max_ext >= 0x8000_001D && ce & (1 << 22) != 0 {
        Some(0x8000_001D)
    } else if !amd && max_leaf >= 4 {
        Some(4)
    } else {
        None
    };
    let mut any = false;
    if let Some(leaf) = leaf {
        for i in 0..16 {
            let (a, b, c, d) = cpuid(leaf, i);
            let ty = a & 0x1F;
            if ty == 0 {
                break;
            }
            any = true;
            let level = (a >> 5) & 7;
            let shared = ((a >> 14) & 0xFFF) + 1;
            let ways = ((b >> 22) & 0x3FF) + 1;
            let parts = ((b >> 12) & 0x3FF) + 1;
            let line = (b & 0xFFF) + 1;
            let sets = c + 1;
            let size = ways as u64 * parts as u64 * line as u64 * sets as u64;
            let (name, kind) = match (level, ty) {
                (1, 1) => ("L1 data", "data"),
                (1, 2) => ("L1 instruction", "instruction"),
                (2, _) => ("L2", "unified"),
                (3, _) => ("L3", "unified"),
                (4, _) => ("L4", "unified"),
                _ => ("Cache", "unified"),
            };
            let per = if shared <= smt.max(1) { String::from("per core") } else { format!("shared by {shared} threads") };
            let instances = if shared > 0 { (pkg + shared - 1) / shared } else { 1 };
            let total = if instances > 1 { format!(" ({} x {} = {})", instances, kib(size), kib(size * instances as u64)) } else { String::new() };
            r.kv(
                name,
                &format!(
                    "{}{}, {}-way, {}-byte lines, {} sets, {}{}{}",
                    kib(size),
                    total,
                    ways,
                    line,
                    sets,
                    per,
                    if d & 2 != 0 { ", inclusive" } else { "" },
                    if kind == "data" || kind == "instruction" { "" } else { "" }
                ),
            );
        }
    }
    if !any && max_ext >= 0x8000_0006 {
        let (_, _, c5, d5) = cpuid(0x8000_0005, 0);
        let (_, _, c6, d6) = cpuid(0x8000_0006, 0);
        if c5 != 0 {
            r.kv("L1 data", &format!("{} KiB, {}-byte lines", c5 >> 24, c5 & 0xFF));
            r.kv("L1 instruction", &format!("{} KiB, {}-byte lines", d5 >> 24, d5 & 0xFF));
        }
        if c6 != 0 {
            r.kv("L2", &format!("{} KiB, {}-byte lines", c6 >> 16, c6 & 0xFF));
        }
        if d6 >> 18 != 0 {
            r.kv("L3", &format!("{} KiB", (d6 >> 18) * 512));
        }
    }

    // per core
    let temps = crate::arch::cpudrv::core_temps();
    let (_, _, by) = crate::arch::smp::stats();
    for i in 0..crate::arch::smp::cores_online().max(1) {
        r.node(&format!("CPU/Core {i}"));
        r.kv("State", if i == 0 { "online (boot core: desktop, apps, drivers)" } else { "online (worker: parallel jobs)" });
        if let Some(&t) = temps.get(i) {
            if t > 0 {
                r.kv("Temperature", &format!("{t} C (last reading)"));
            }
        }
        if let Some(&n) = by.get(i) {
            r.kv("Parallel jobs run", &format!("{n}"));
        }
    }
}

// ── the report ──────────────────────────────────────────────────────────────

fn system_section(r: &mut R) {
    r.node("System");
    r.kv("Operating system", "LingOS x86_64");
    r.kv("Kernel", &format!("ling-kernel {} (Ling, no_std Rust)", env!("CARGO_PKG_VERSION")));
    let host = crate::vfs::read("/hostname").map(|b| String::from_utf8_lossy(&b).trim().to_string()).filter(|h| !h.is_empty()).unwrap_or_else(|| String::from("lingos"));
    r.kv("Computer name", &host);
    let user = crate::fs::lingfs::current_user();
    r.kv("User", if user.is_empty() { "(nobody signed in)" } else { &user });
    r.kv("Date / time (UTC)", &crate::lingapp::format_utc(crate::clock::unix_now()));
    r.kv("Uptime", &secs_words(crate::arch::timer::now_ms() / 1000));
    let cl = crate::arch::bootmodule::cmdline();
    let mode = match &cl {
        Some(c) if c.starts_with("lingboot") => "UEFI (lingboot)",
        Some(_) => "BIOS (GRUB, Multiboot2)",
        None if crate::drivers::framebuffer::booted_from_disk() => "BIOS (LingOS disk boot)",
        None => "BIOS",
    };
    r.kv("Boot mode", mode);
    if let Some(c) = cl {
        r.kv("Boot command line", &c);
    }
    if let Some(d) = boot_disk() {
        r.kv("Booted from", d.kind());
    }
    r.kv(
        "System storage",
        &if crate::fs::blockdev::is_ram() {
            format!("RAM disk (nothing is kept: {})", crate::fs::blockdev::ram_reason())
        } else {
            match crate::fs::blockdev::system_disk() {
                Some(d) => format!("lingfs on the {} disk", d.kind()),
                None => String::from("lingfs"),
            }
        },
    );
    let ip = crate::drivers::netstack::self_ip();
    r.kv("IP address", &if ip == [0; 4] { String::from("none") } else { ip_str(ip) });
    r.kv("Network", &crate::drivers::netdev::status_line());
    if crate::drivers::framebuffer::available() {
        r.kv("Display", &format!("{} x {} at {} Hz frame cap", crate::drivers::framebuffer::width(), crate::drivers::framebuffer::height(), crate::drivers::framebuffer::present_hz()));
    }
    r.kv("Mouse", if crate::arch::xhci::mouse_ready() { "USB" } else { "PS/2" });
    r.kv("Keyboard", if crate::arch::xhci::keyboard_ready() { "USB" } else { "PS/2" });
}

fn memory_section(r: &mut R, dimms: &[(String, Vec<(String, String)>)]) {
    r.node("Memory");
    let total = crate::kstats::total_frames() * 4096;
    let free = crate::mm::frame::free_frame_count() as u64 * 4096;
    let map = crate::arch::meminfo::map();
    let usable: u64 = map.iter().filter(|e| e.2 == 1).map(|e| e.1).sum();
    r.kv("Usable RAM (firmware map)", &mib(usable));
    r.kv("Managed by LingOS", &mib(total));
    r.kv("Free", &mib(free));
    r.kv("Used", &mib(total.saturating_sub(free)));
    if total > 0 {
        r.kv("Load", &format!("{} %", (total.saturating_sub(free)) * 100 / total));
    }
    r.kv("Highest RAM address", &format!("0x{:x}", crate::mm::ram_end()));
    let mut slots = 0;
    let mut filled = 0;
    let mut sum = 0u64;
    let mut kinds: Vec<String> = Vec::new();
    for (name, rows) in dimms {
        if name.starts_with("Array") {
            continue;
        }
        slots += 1;
        let size = rows.iter().find(|(k, _)| k == "Size").map(|(_, v)| v.clone()).unwrap_or_default();
        if size != "empty slot" {
            filled += 1;
            if let Some(n) = size.split(' ').next().and_then(|n| n.parse::<f64>().ok()) {
                sum += if size.contains("GiB") { (n * 1024.0) as u64 } else { n as u64 };
            }
            let ty = rows.iter().find(|(k, _)| k == "Type").map(|(_, v)| v.clone()).unwrap_or_default();
            let sp = rows.iter().find(|(k, _)| k == "Configured speed").or_else(|| rows.iter().find(|(k, _)| k == "Speed (max)")).map(|(_, v)| v.clone()).unwrap_or_default();
            let k = format!("{ty} {sp}");
            if !kinds.contains(&k) {
                kinds.push(k);
            }
        }
    }
    if slots > 0 {
        r.kv("Memory slots", &format!("{filled} of {slots} in use"));
        if sum > 0 {
            r.kv("Installed (SMBIOS)", &format!("{} MiB ({:.1} GiB)", sum, sum as f64 / 1024.0));
        }
        r.kv("Memory type", &kinds.join(", "));
    }
    if !map.is_empty() {
        let tot = |k: u32| map.iter().filter(|e| e.2 == k).map(|e| e.1).sum::<u64>();
        r.kv("Reserved / ACPI / NVS", &format!("{} / {} / {}", mib(tot(2)), kib(tot(3)), kib(tot(4))));
        r.node("Memory/Firmware memory map");
        for &(b, l, t) in &map {
            let kind = match t {
                1 => "usable",
                2 => "reserved",
                3 => "ACPI reclaimable",
                4 => "ACPI NVS",
                5 => "bad memory",
                _ => "other",
            };
            r.kv(&format!("0x{:010x}-0x{:010x}", b, b.saturating_add(l).saturating_sub(1)), &format!("{kind}, {}", if l >= 1 << 20 { mib(l) } else { kib(l) }));
        }
    }
    for (name, rows) in dimms {
        r.node(&format!("Memory/{}", seg(name)));
        for (k, v) in rows {
            r.kv(k, v);
        }
    }
}

fn graphics_section(r: &mut R, devs: &[PDev]) {
    r.node("Graphics");
    let gpus: Vec<&PDev> = devs.iter().filter(|d| d.class == 0x03).collect();
    r.kv("Display controllers", &format!("{}", gpus.len()));
    if let Some(fb) = crate::drivers::framebuffer::get() {
        r.kv("Framebuffer", &format!("{} x {}, {} bpp, pitch {} bytes", fb.width, fb.height, fb.bpp, fb.pitch));
        r.kv("Framebuffer address", &format!("0x{:x}", fb.addr));
    }
    r.kv("Current mode", crate::drivers::display::current_str());
    r.kv("Frame cap", &format!("{} Hz", crate::drivers::framebuffer::present_hz()));
    r.kv("Live mode switching", if crate::drivers::framebuffer::dispi_capable() { "yes (Bochs DISPI)" } else { "no (the firmware's mode)" });
    if let Some((w, h)) = crate::drivers::framebuffer::dispi_max() {
        r.kv("Adapter maximum", &format!("{w} x {h}"));
    }
    let n = crate::drivers::display::mode_count();
    let modes: Vec<&str> = (0..n).map(crate::drivers::display::mode_label).collect();
    r.kv("Available modes", &modes.join(", "));
    r.kv("Monitor (EDID)", &crate::drivers::framebuffer::edid_summary().unwrap_or_else(|| String::from("not reported by the firmware")));
    {
        use crate::drivers::framebuffer as fb;
        let name = fb::monitor_name();
        if !name.is_empty() {
            r.kv("Monitor name", &name);
        }
        if let Some((w, h)) = fb::monitor_mm() {
            r.kv("Monitor size", &format!("{w} x {h} mm ({:.1}\")", libm::sqrt((w * w + h * h) as f64) / 25.4));
        }
        if fb::monitor_max_hz() > 0 {
            r.kv("Monitor refresh range", &format!("up to {} Hz", fb::monitor_max_hz()));
        }
        r.kv("Colour depth / DPI", &format!("{}-bit, {} dpi ({})", fb::depth(), fb::dpi(), fb::info_source()));
        r.kv("Frame cap mode", if fb::present_auto() { "Auto (follows the monitor)" } else { "chosen in Settings" });
    }
    let nv = crate::drivers::nvgpu::probe();
    for (i, d) in gpus.iter().enumerate() {
        let name = pci_label(d);
        r.node(&format!("Graphics/GPU {} {}", i, seg(&name)));
        pci_rows(r, d);
        if let Some(g) = nv.iter().find(|g| g.loc.bus == d.bus && g.loc.device == d.dev && g.loc.function == d.func) {
            let cs = g.chipset();
            if g.boot0 != 0 {
                r.kv("Chip", &format!("{} ({}), chipset 0x{:03x}", crate::drivers::nvgpu::chip_name(cs), crate::drivers::nvgpu::arch_name(cs), cs));
                r.kv("PMC_BOOT_0 / BOOT_42", &format!("0x{:08x} / 0x{:08x}", g.boot0, g.boot42));
            }
            if let Some(v) = g.vram_mib {
                r.kv("Video memory", &format!("{} MiB ({:.1} GiB)", v, v as f64 / 1024.0));
            }
            r.kv("BAR0 registers", &format!("0x{:x} ({})", g.bar0, mib(g.bar0_len)));
            r.kv("BAR1 VRAM aperture", &format!("0x{:x} ({})", g.bar1, mib(g.bar1_len)));
            if let Some(((g0, w0), (g1, w1))) = g.link {
                r.kv("PCIe link (GPU)", &format!("Gen{g0} x{w0} (card + slot can do Gen{g1} x{w1})"));
            }
            r.kv("Driver", "LingOS nvgpu (identify only; the screen is the firmware framebuffer)");
            if !g.note.is_empty() {
                r.kv("Note", &g.note);
            }
        } else if d.vendor == 0x1234 || d.vendor == 0x80EE {
            r.kv("Driver", "LingOS Bochs DISPI (resolution switching)");
        } else {
            r.kv("Driver", "firmware framebuffer");
        }
        if let Some(fb) = crate::drivers::framebuffer::get() {
            let b = bars(d);
            if b.iter().any(|x| x.contains(&format!("0x{:x}", fb.addr & !0xFFFFF)) || x.contains(&format!("0x{:x}", fb.addr))) {
                r.kv("Drives the screen", "yes (the framebuffer is in its memory)");
            }
        }
    }
}

fn storage_section(r: &mut R, devs: &[PDev]) {
    r.node("Storage");
    let disks = crate::fs::disks::list();
    r.kv("Disks", &format!("{}", disks.len()));
    let ctrls: Vec<&PDev> = devs.iter().filter(|d| d.class == 0x01).collect();
    r.kv("Controllers", &ctrls.iter().map(|d| format!("{} {}", d.bdf(), class_name(d.class, d.sub, d.prog))).collect::<Vec<_>>().join(", "));
    let vols = crate::fs::media::names();
    r.kv("Mounted volumes", &if vols.is_empty() { String::from("none") } else { vols.iter().map(|v| format!("media/{v}")).collect::<Vec<_>>().join(", ") });
    r.kv("System storage", &if crate::fs::blockdev::is_ram() { String::from("RAM disk") } else { String::from("lingfs") });
    if !crate::drivers::nvme::present() {
        r.kv("NVMe", &crate::drivers::nvme::status());
    }
    if !crate::fs::blockdev::is_ram() && crate::fs::blockdev::system_disk().is_none() {
        r.kv("IDE disk (ata0)", "primary master, holds LingOS system storage (lingfs)");
    }
    for (i, d) in disks.iter().enumerate() {
        let label = match d.id {
            crate::fs::disks::DiskId::Sata(p) => format!("SATA port {p}"),
            crate::fs::disks::DiskId::Nvme => String::from("NVMe"),
            crate::fs::disks::DiskId::Usb => String::from("USB"),
        };
        r.node(&format!("Storage/Disk {i} {}", seg(&d.model)));
        r.kv("Model", &d.model);
        r.kv("Interface", &label);
        r.kv("Capacity", &disk_size(d.sectors * 512));
        r.kv("Sectors", &format!("{} x 512 bytes (logical block {} bytes)", d.sectors, d.block));
        if let crate::fs::disks::DiskId::Nvme = d.id {
            if let Some((serial, fw, loc, bs, n)) = crate::drivers::nvme::details() {
                r.kv("Serial number", &serial);
                r.kv("Firmware", &fw);
                r.kv("Namespace 1", &format!("{n} blocks of {bs} bytes"));
                if let Some(p) = devs.iter().find(|p| p.bus == loc.bus && p.dev == loc.device && p.func == loc.function) {
                    r.kv("Controller", &format!("{} {}", p.bdf(), pci_label(p)));
                    if let Some((g, w, mg, mw)) = pcie_link(p) {
                        r.kv("PCIe link", &format!("Gen{g} x{w} (max Gen{mg} x{mw})"));
                    }
                }
            }
        }
        if Some(d.id) == boot_disk() {
            r.kv("Boot disk", "yes (LingOS started from it)");
        }
        if Some(d.id) == crate::fs::blockdev::system_disk() {
            r.kv("Holds", "LingOS system storage (lingfs)");
        }
    }
}

fn audio_section(r: &mut R, devs: &[PDev]) {
    r.node("Audio");
    let hda = crate::drivers::hda::present();
    let ac97 = crate::drivers::ac97::present();
    let usb = crate::drivers::usbaudio::present();
    r.kv(
        "Playing through",
        &if usb {
            format!("USB audio: {}", crate::drivers::usbaudio::name())
        } else if hda {
            String::from(crate::drivers::hda::label())
        } else if ac97 {
            String::from("AC'97")
        } else {
            String::from("no sound device")
        },
    );
    for d in devs.iter().filter(|d| d.class == 0x04) {
        r.node(&format!("Audio/{} {}", d.bdf(), seg(&pci_label(d))));
        pci_rows(r, d);
    }
    if hda {
        r.node("Audio/HD Audio");
        for (b, v, dv) in crate::drivers::hda::hw_controllers() {
            r.kv(&format!("Controller {b}"), &format!("{:04x}:{:04x} {} {}", v, dv, vendor_name(v), device_name(v, dv)));
        }
        for (b, c, id, vn) in crate::drivers::hda::hw_codecs() {
            r.kv(&format!("Codec {b} #{c}"), &format!("{} {:04x}:{:04x}{}", vn, id >> 16, id & 0xFFFF, codec_name(id).map(|n| format!(" ({n})")).unwrap_or_default()));
        }
        r.kv("Routing", if crate::drivers::hda::manual() { "chosen by hand" } else { "automatic" });
        r.node("Audio/HD Audio/Outputs");
        let act = crate::drivers::hda::active_output();
        for i in 0..crate::drivers::hda::output_count() {
            let st = crate::drivers::hda::output_state(i);
            r.kv(
                &format!("{i:2} {}", crate::drivers::hda::output_name(i)),
                &format!("{}{}{}", if act == Some(i) { "PLAYING, " } else { "" }, if st.is_empty() { "jack cannot tell" } else { st }, format!(" -- {}", crate::drivers::hda::output_describe(i))),
            );
        }
        if crate::drivers::hda::input_count() > 0 {
            r.node("Audio/HD Audio/Inputs");
            for i in 0..crate::drivers::hda::input_count() {
                let st = crate::drivers::hda::input_state(i);
                r.kv(&format!("{i:2} {}", crate::drivers::hda::input_name(i)), if st.is_empty() { "jack cannot tell" } else { st });
            }
        }
    }
    if usb {
        r.node("Audio/USB audio");
        r.kv("Device", crate::drivers::usbaudio::name());
        r.kv("Sample depth", &format!("{} bits", crate::drivers::usbaudio::bits()));
        r.kv("Capture", yes(crate::drivers::usbaudio::has_capture()));
        if let Some(a) = crate::arch::xhci::audio_info() {
            r.kv("USB id", &format!("{:04x}:{:04x} on port {}", a.vid, a.pid, a.port));
        }
    }
}

fn codec_name(id: u32) -> Option<&'static str> {
    Some(match id {
        0x10EC0887 => "ALC887",
        0x10EC0892 => "ALC892",
        0x10EC0897 => "ALC897",
        0x10EC0888 => "ALC888",
        0x10EC0662 => "ALC662",
        0x10EC1220 => "ALC1220",
        0x10EC1168 => "ALC1220-VB",
        0x10EC0285 => "ALC285",
        0x10EC0295 => "ALC295",
        0x10EC0256 => "ALC256",
        0x10EC0269 => "ALC269",
        0x10EC4080 => "ALC4080 (USB)",
        0x80862882 => "Intel HDMI (Tiger Lake)",
        0x80862812 => "Intel HDMI (Tiger Lake-H)",
        0x80862818 => "Intel HDMI (Raptor Lake)",
        0x8086280B => "Intel HDMI (Kaby Lake)",
        0x80862807 => "Intel HDMI (Haswell)",
        0x1AF40022 => "QEMU HDA duplex",
        0x1AF40021 => "QEMU HDA output",
        0x1AF40020 => "QEMU HDA micro",
        0x10DE009A | 0x10DE009B | 0x10DE009C | 0x10DE009D | 0x10DE009E => "NVIDIA HDMI / DP",
        0x11060397 => "VIA VT1708S",
        0x111D76B2 => "IDT 92HD71B",
        0x14F15051 => "Conexant CX20561",
        _ => return None,
    })
}

fn network_section(r: &mut R, devs: &[PDev]) {
    r.node("Network");
    r.kv("Active connection", &crate::drivers::netdev::status_line());
    r.kv("IPv4 address", &ip_str(crate::drivers::netstack::self_ip()));
    r.kv("Subnet mask", &ip_str(crate::drivers::netdev::subnet_mask()));
    r.kv("Gateway", &ip_str(crate::drivers::netstack::gateway_ip()));
    let (d1, d2) = crate::drivers::netstack::dns_servers();
    r.kv("DNS", &format!("{}{}", ip_str(d1), if d2 != [0; 4] { format!(", {}", ip_str(d2)) } else { String::new() }));
    r.kv("Hardware address", &mac_str(crate::drivers::netdev::mac()));
    r.kv("Traffic", &format!("{} received, {} sent", mib(crate::kstats::get_net_rx()), mib(crate::kstats::get_net_tx())));
    for d in devs.iter().filter(|d| d.class == 0x02 || (d.class == 0x0D)) {
        r.node(&format!("Network/{} {}", d.bdf(), seg(&pci_label(d))));
        pci_rows(r, d);
        let pid = (d.device as u32) << 16 | d.vendor as u32;
        let (il, _, _) = (crate::drivers::net_igc::pci_location(), 0, 0);
        if crate::drivers::net_igc::is_up() && il == (d.bus, d.dev, d.func) {
            r.kv("Driver", &format!("LingOS igc ({})", crate::drivers::net_igc::name()));
            r.kv("MAC", &mac_str(crate::drivers::net_igc::mac()));
            r.kv(
                "Link",
                &if crate::drivers::net_igc::link_up() {
                    format!("up, {} Mb/s{}", crate::drivers::net_igc::speed_mbps(), if crate::drivers::net_igc::full_duplex() { " full duplex" } else { "" })
                } else {
                    String::from("down (no cable?)")
                },
            );
        } else if crate::drivers::net_e1000::pci_id() == pid && pid != 0 {
            r.kv("Driver", "LingOS e1000");
            r.kv("MAC", &mac_str(crate::drivers::net_e1000::mac()));
            r.kv("Link", &if crate::drivers::net_e1000::link_up() { format!("up, {} Mb/s", crate::drivers::net_e1000::speed_mbps()) } else { String::from("down") });
        } else if d.vendor == 0x8086 && d.class == 0x02 && d.sub == 0x80 {
            r.kv("Driver", "LingOS iwlwifi (Intel Wi-Fi)");
            r.kv("Status", &crate::drivers::iwlwifi::short_status());
            let m = crate::drivers::iwlwifi::mac();
            if m != [0; 6] {
                r.kv("MAC", &mac_str(m));
            }
        } else {
            let why = crate::drivers::net_igc::last_error();
            r.kv("Driver", if crate::drivers::net_igc::model_for(d.vendor, d.device).is_some() && !why.is_empty() { why } else { "none" });
        }
    }
    r.node("Network/Wi-Fi");
    r.kv("Radio", &crate::drivers::iwlwifi::short_status());
    r.kv("Joined", yes(crate::drivers::iwlwifi::is_joined()));
    if crate::drivers::iwlwifi::is_joined() {
        r.kv("Signal", &format!("{} dBm", crate::drivers::iwlwifi::rssi()));
    }
}

fn usb_class(c: u8) -> &'static str {
    match c {
        0x00 => "composite",
        0x01 => "audio",
        0x02 => "communications",
        0x03 => "HID",
        0x05 => "physical",
        0x06 => "imaging",
        0x07 => "printer",
        0x08 => "mass storage",
        0x09 => "hub",
        0x0A => "CDC data",
        0x0B => "smart card",
        0x0D => "content security",
        0x0E => "video",
        0x0F => "healthcare",
        0x10 => "audio/video",
        0x11 => "billboard",
        0x12 => "USB-C bridge",
        0xDC => "diagnostic",
        0xE0 => "wireless (Bluetooth)",
        0xEF => "miscellaneous",
        0xFE => "application specific",
        0xFF => "vendor specific",
        _ => "other",
    }
}

fn usb_vendor(v: u16) -> &'static str {
    match v {
        0x046D => "Logitech",
        0x045E => "Microsoft",
        0x056A => "Wacom",
        0x0781 => "SanDisk",
        0x0951 => "Kingston",
        0x8087 | 0x8086 => "Intel",
        0x0B05 => "ASUS",
        0x1532 => "Razer",
        0x1B1C => "Corsair",
        0x04D9 => "Holtek",
        0x0627 => "QEMU",
        0x46F4 => "QEMU",
        0x1050 => "Yubico",
        0x0BDA => "Realtek",
        0x05AC => "Apple",
        0x054C => "Sony",
        0x057E => "Nintendo",
        0x0D8C => "C-Media",
        0x1D6B => "Linux Foundation",
        0x0B95 => "ASIX",
        0x13FE => "Phison / Kingston",
        0x090C => "Silicon Motion",
        0x058F => "Alcor Micro",
        0x0930 => "Toshiba",
        0x0BC2 => "Seagate",
        0x1058 => "Western Digital",
        0x04E8 => "Samsung",
        0x2109 => "VIA Labs",
        0x05E3 => "Genesys Logic",
        0x0424 => "Microchip / SMSC",
        0x1A40 => "Terminus",
        0x0C45 => "Microdia",
        0x258A => "SINO WEALTH",
        0x3434 => "Keychron",
        0x1209 => "pid.codes",
        0x20A0 => "Nitrokey / Clay Logic",
        0x1E7D => "ROCCAT",
        0x28DE => "Valve",
        0x2DC8 => "8BitDo",
        0x0E8D => "MediaTek",
        0x0A12 => "Cambridge Silicon Radio",
        0x1915 => "Nordic Semiconductor",
        0x0483 => "STMicroelectronics",
        0x10C4 => "Silicon Labs",
        0x0403 => "FTDI",
        0x1A86 => "QinHeng (CH340)",
        0x067B => "Prolific",
        0x0B0E => "Jabra / GN",
        0x047F => "Plantronics",
        0x1395 => "Sennheiser / EPOS",
        0x0D8D => "Focusrite",
        0x1235 => "Focusrite / Novation",
        0x0499 => "Yamaha",
        0x0582 => "Roland",
        0x0944 => "Korg",
        0x1397 => "Behringer",
        0x2972 => "FiiO",
        0x262A => "Savitech",
        0x0CF3 => "Qualcomm Atheros",
        0x13D3 => "IMC Networks (AzureWave)",
        0x0489 => "Foxconn",
        _ => "",
    }
}

fn usb_section(r: &mut R, devs: &[PDev]) {
    r.node("USB");
    let ctls = crate::arch::xhci::controllers();
    for d in devs.iter().filter(|d| d.class == 0x0C && d.sub == 0x03) {
        r.kv(&format!("Controller {}", d.bdf()), &format!("{} ({:04x}:{:04x})", pci_label(d), d.vendor, d.device));
    }
    for (name, n) in &ctls {
        r.kv(&format!("Driver {}", String::from_utf8_lossy(name)), &format!("running, {n} devices"));
    }
    let list = crate::arch::xhci::devices();
    r.kv("Devices attached", &format!("{}", list.len()));
    r.kv("Keyboard", yes(crate::arch::xhci::keyboard_ready()));
    r.kv("Mouse", yes(crate::arch::xhci::mouse_ready()));
    let pen = core::str::from_utf8(crate::drivers::pen::device_line()).unwrap_or("").trim().to_string();
    if !pen.is_empty() {
        r.kv("Pen", &pen);
    }
    r.kv("Security key (FIDO)", yes(crate::arch::xhci::fido_present()));
    for (i, u) in list.iter().enumerate() {
        let vn = usb_vendor(u.vid);
        let mut name = match (u.vid, u.pid) {
            (0x0627, 0x0001) => String::from("QEMU USB HID (tablet / mouse / keyboard)"),
            (0x46F4, 0x0001) => String::from("QEMU USB storage"),
            (0x056A, p) if u.class == 9 => format!("Wacom pen display USB hub{}", crate::drivers::hiddigi::hub_name(p).map(|n| format!(" ({n})")).unwrap_or_default()),
            (0x056A, p) => crate::drivers::hiddigi::hid_name(p).map(String::from).unwrap_or_else(|| String::from("Wacom tablet")),
            _ => String::new(),
        };
        let classes: Vec<String> = u
            .ifaces
            .iter()
            .map(|&(c, s, p)| match (c, p) {
                (3, 1) if s == 1 => String::from("keyboard"),
                (3, 2) if s == 1 => String::from("mouse"),
                (8, _) => String::from("mass storage"),
                (1, _) if s == 2 => String::from("audio streaming"),
                (1, _) => String::from("audio control"),
                _ => String::from(usb_class(c)),
            })
            .collect();
        let mut uniq: Vec<String> = Vec::new();
        for c in classes {
            if !uniq.contains(&c) {
                uniq.push(c);
            }
        }
        if name.is_empty() {
            name = format!("{} {}", if vn.is_empty() { "USB" } else { vn }, uniq.first().cloned().unwrap_or_else(|| String::from(usb_class(u.class))));
        }
        r.node(&format!("USB/Device {i} {}", seg(&name)));
        r.kv("Name", &name);
        r.kv("Vendor : product", &format!("{:04x}:{:04x}{}", u.vid, u.pid, if vn.is_empty() { String::new() } else { format!(" ({vn})") }));
        r.kv("Class", &format!("{} (0x{:02x})", usb_class(u.class), u.class));
        r.kv("Interfaces", &uniq.join(", "));
        r.kv("USB version", &format!("{}.{}", u.bcd_usb >> 8, (u.bcd_usb >> 4) & 0xF));
        r.kv(
            "Speed",
            match u.speed {
                1 => "full speed (12 Mb/s)",
                2 => "low speed (1.5 Mb/s)",
                3 => "high speed (480 Mb/s)",
                4 => "SuperSpeed (5 Gb/s)",
                5 => "SuperSpeed+ (10 Gb/s)",
                _ => "SuperSpeed+",
            },
        );
        r.kv("Port / slot", &format!("{} / {} on {}", u.path(), u.slot, String::from_utf8_lossy(u.controller)));
        if u.depth > 0 {
            r.kv("Behind hub", &format!("tier {} (root port {}, hub port path {})", u.depth + 1, u.port, u.path()));
        }
    }
}

fn pci_section(r: &mut R, devs: &[PDev]) {
    r.node("PCI");
    r.kv("Devices", &format!("{}", devs.len()));
    let buses: Vec<u8> = {
        let mut b: Vec<u8> = devs.iter().map(|d| d.bus).collect();
        b.dedup();
        b
    };
    r.kv("Buses in use", &buses.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "));
    for d in devs {
        r.kv(&d.bdf(), &format!("{:04x}:{:04x}  {}", d.vendor, d.device, pci_label(d)));
    }
    for d in devs {
        r.node(&format!("PCI/{} {}", d.bdf(), seg(&pci_label(d))));
        pci_rows(r, d);
    }
}

/// The whole report (see the module doc for the format).
pub fn report() -> String {
    if BUSY.swap(true, Ordering::Acquire) {
        // another task is gathering (the app and SSH at once): its last result
        return unsafe { (*(&raw const LAST)).clone() }.unwrap_or_else(|| String::from("#System\nStatus\tbusy, try again\n"));
    }
    let mut r = R { s: String::with_capacity(32 * 1024) };
    let devs = pci_scan();
    system_section(&mut r);
    cpu_section(&mut r);
    let dimms = smbios_sections(&mut r);
    memory_section(&mut r, &dimms);
    graphics_section(&mut r, &devs);
    storage_section(&mut r, &devs);
    audio_section(&mut r, &devs);
    network_section(&mut r, &devs);
    usb_section(&mut r, &devs);
    pci_section(&mut r, &devs);
    acpi_section(&mut r);
    // one node per path, each under its top-level category
    let mut s = String::with_capacity(r.s.len());
    for (p, rows) in nodes(&r.s) {
        s.push('#');
        s.push_str(&p);
        s.push('\n');
        for (k, v) in rows {
            s.push_str(&k);
            s.push('\t');
            s.push_str(&v);
            s.push('\n');
        }
    }
    unsafe { *(&raw mut LAST) = Some(s.clone()) };
    BUSY.store(false, Ordering::Release);
    s
}

/// The report as readable text: a header per node, aligned name / value
/// columns (the Specs app's Export file and the `hwinfo` command).
pub fn text() -> String {
    format_text(&report())
}

/// The report's nodes in display order: the same path given twice is one
/// node, and every node follows its top-level category.
pub fn nodes(rep: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut list: Vec<(String, Vec<(String, String)>)> = Vec::new();
    let mut cur: Option<usize> = None;
    for line in rep.lines() {
        if let Some(p) = line.strip_prefix('#') {
            cur = Some(match list.iter().position(|n| n.0 == p) {
                Some(i) => i,
                None => {
                    list.push((String::from(p), Vec::new()));
                    list.len() - 1
                },
            });
        } else if let (Some(i), Some((k, v))) = (cur, line.split_once('\t')) {
            list[i].1.push((String::from(k), String::from(v)));
        }
    }
    let top = |p: &str| String::from(p.split('/').next().unwrap_or(""));
    let mut order: Vec<String> = Vec::new();
    for n in &list {
        let t = top(&n.0);
        if !order.contains(&t) {
            order.push(t);
        }
    }
    let mut keyed: Vec<(usize, usize, (String, Vec<(String, String)>))> =
        list.into_iter().enumerate().map(|(i, n)| (order.iter().position(|t| *t == top(&n.0)).unwrap_or(0), i, n)).collect();
    keyed.sort_by_key(|k| (k.0, k.1));
    keyed.into_iter().map(|k| k.2).collect()
}

pub fn format_text(rep: &str) -> String {
    let mut out = String::new();
    out.push_str("LingOS hardware report\n");
    out.push_str(&format!("{} UTC\n", crate::lingapp::format_utc(crate::clock::unix_now())));
    for (p, rows) in nodes(rep) {
        out.push('\n');
        let depth = p.matches('/').count();
        out.push_str(&format!("{}== {} ==\n", "  ".repeat(depth.min(4)), p.replace('/', " > ")));
        for (k, v) in rows {
            let pad = 30usize.saturating_sub(k.chars().count()).max(1);
            out.push_str(&format!("{}  {}{}{}\n", "  ".repeat(depth.min(4)), k, " ".repeat(pad), v));
        }
    }
    out
}

/// `hwinfo [raw | <category>]`: the report, the raw tree format, or one
/// category (e.g. `hwinfo cpu`, `hwinfo memory`).
pub fn command(arg: &str) -> String {
    let a = arg.trim();
    if a == "raw" {
        return report();
    }
    if a.is_empty() {
        return text();
    }
    let rep = report();
    let want = a.to_ascii_lowercase();
    let mut keep = String::new();
    let mut on = false;
    for line in rep.lines() {
        if let Some(p) = line.strip_prefix('#') {
            on = p.to_ascii_lowercase().starts_with(&want);
        }
        if on {
            keep.push_str(line);
            keep.push('\n');
        }
    }
    if keep.is_empty() {
        return format!("hwinfo: no category \"{a}\" (System, CPU, Motherboard, BIOS, Memory, Graphics, Storage, Audio, Network, USB, PCI, ACPI)\n");
    }
    format_text(&keep)
}
