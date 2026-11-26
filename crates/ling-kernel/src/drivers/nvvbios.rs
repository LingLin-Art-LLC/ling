//! NVIDIA VBIOS parser (pure: works on a byte copy of the ROM, no registers), the first piece of
//! the GSP compute driver. What it finds is FWSEC: a signed falcon program stored in the card's
//! own VBIOS that the driver runs (on the GSP falcon) to carve out the WPR2 region before NVIDIA's
//! GSP-RM firmware can boot -- step one of bringing up compute on Turing / Ampere / Ada.
//!
//! The layout follows nouveau (MIT licence, drivers/gpu/drm/nouveau/nvkm/subdev/bios: image.c,
//! pcir.c, npde.c, bit.c, pmu.c, and nvkm/subdev/gsp/fwsec.c):
//!  - the ROM is a chain of PCI expansion-ROM images (0x55AA, PCIR, NVIDIA's NPDE extension);
//!    image type 0xE0 holds the extra tables, and table pointers past the first image's size are
//!    relative to that image (`addr`);
//!  - the "BIT" table (signature FF B8 'B' 'I' 'T') lists tables by letter: 'i' = version,
//!    'p' = the falcon ucode table, whose entry of type 0x85 is FWSEC;
//!  - FWSEC starts with a falcon ucode descriptor (v2, or v3 with signatures on Ampere), followed
//!    by the IMEM + DMEM images; DMEM carries an "application interface" table whose DMEMMAPPER
//!    entry the driver patches with the command to run (FRTS: set up the firmware runtime region).
//!
//! No `unsafe`, no allocation beyond the report string: a malformed ROM yields Err, never a panic.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// FWSEC's entry type in the 'p' (falcon ucode) table.
pub const UCODE_FWSEC: u8 = 0x85;
/// DMEMMAPPER application id (nouveau NVFW_FALCON_APPIF_ID_DMEMMAPPER).
pub const APPIF_DMEMMAPPER: u32 = 4;
/// DMEMMAPPER init commands.
pub const CMD_FRTS: u32 = 0x15;
pub const CMD_SB: u32 = 0x19;
/// Offset of `init_cmd` inside the DMEMMAPPER block (the command FWSEC runs).
pub const DMEMMAPPER_INIT_CMD: u32 = 0x2C;

pub struct Vbios<'a> {
    pub rom: &'a [u8],
    /// Size of the first image (pointers at or past it are relative to `imaged`).
    pub image0_size: u32,
    /// Start of the type-0xE0 image (0 = none).
    pub imaged: u32,
    pub bit: u32,
    pub images: Vec<Image>,
}

#[derive(Clone, Copy)]
pub struct Image {
    pub base: u32,
    pub size: u32,
    pub kind: u8,
    pub last: bool,
}

/// FWSEC as found in the ROM: its descriptor and where its pieces are (offsets into the ROM).
pub struct Fwsec {
    /// Descriptor version (2 or 3).
    pub version: u8,
    /// ROM offset of the descriptor; the ucode image follows at `desc + desc_size`.
    pub desc: usize,
    pub desc_size: usize,
    /// ROM offset of the ucode image (IMEM then DMEM).
    pub image: usize,
    pub imem_phys_base: u32,
    pub imem_load_size: u32,
    pub imem_virt_base: u32,
    pub dmem_phys_base: u32,
    pub dmem_load_size: u32,
    /// Where DMEM starts inside the image (v3: right after IMEM).
    pub dmem_offset: u32,
    pub interface_offset: u32,
    /// v3: signatures (each 96 * 4 bytes) and where the chosen one is patched into DMEM.
    pub pkc_data_offset: u32,
    pub engine_id_mask: u16,
    pub ucode_id: u8,
    pub signature_count: u8,
    pub signature_versions: u16,
    /// v3: ROM offset of the first signature (right after the 0x2c-byte descriptor fields).
    pub signatures: usize,
    /// DMEM offset of the DMEMMAPPER block (found through the application interface table).
    pub dmemmapper: Option<u32>,
    /// From the DMEMMAPPER: where the command's input buffer is in DMEM, and its size.
    pub cmd_in: Option<(u32, u32)>,
}

fn rd8(r: &[u8], at: usize) -> Option<u8> {
    r.get(at).copied()
}
fn rd16(r: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*r.get(at)?, *r.get(at + 1)?]))
}
fn rd32(r: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes([*r.get(at)?, *r.get(at + 1)?, *r.get(at + 2)?, *r.get(at + 3)?]))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// One image's header at `base`: (size, type, last).
fn image_at(rom: &[u8], base: usize) -> Option<Image> {
    let sig = rd16(rom, base)?;
    if !matches!(sig, 0xAA55 | 0xBB77 | 0x4E56) {
        return None;
    }
    let pcir = base + rd16(rom, base + 0x18)? as usize;
    let psig = rd32(rom, pcir)?;
    if !matches!(psig, 0x5249_4350 | 0x5349_4752 | 0x5344_504E) {
        return None; // "PCIR" / "RGIS" / "NPDS"
    }
    let hdr = rd16(rom, pcir + 0x0A)? as usize;
    let mut size = rd16(rom, pcir + 0x10)? as u32 * 512;
    let kind = rd8(rom, pcir + 0x14)?;
    let mut last = rd8(rom, pcir + 0x15)? & 0x80 != 0;
    if kind == 0x70 {
        last = true;
    } else {
        // NVIDIA's extension right after the PCIR (16-byte aligned) overrides size / last
        let npde = (pcir + hdr + 0x0F) & !0x0F;
        if rd32(rom, npde) == Some(0x4544_504E) {
            size = rd16(rom, npde + 0x08)? as u32 * 512;
            last = rd8(rom, npde + 0x0A)? & 0x80 != 0;
        }
    }
    Some(Image { base: base as u32, size, kind, last })
}

impl<'a> Vbios<'a> {
    pub fn parse(rom: &'a [u8]) -> Result<Vbios<'a>, String> {
        let mut images = Vec::new();
        let mut base = 0usize;
        loop {
            let Some(img) = image_at(rom, base) else { break };
            if img.size == 0 {
                break;
            }
            images.push(img);
            if img.last || images.len() > 16 {
                break;
            }
            base += img.size as usize;
        }
        if images.is_empty() {
            return Err(format!("no ROM image at 0 (signature {:04x?}): not a VBIOS", rd16(rom, 0)));
        }
        let image0_size = images[0].size;
        let imaged = images.iter().skip(1).find(|i| i.kind == 0xE0).map(|i| i.base).unwrap_or(0);
        let bit = find(rom, b"\xff\xb8BIT").ok_or("no BIT table in the VBIOS")? as u32;
        Ok(Vbios { rom, image0_size, imaged, bit, images })
    }

    /// nouveau's nvbios_addr: a table pointer past the first image is relative to the 0xE0 image.
    pub fn addr(&self, a: u32) -> usize {
        if a >= self.image0_size && self.imaged != 0 { (a - self.image0_size + self.imaged) as usize } else { a as usize }
    }

    fn r8(&self, a: u32) -> Option<u8> {
        rd8(self.rom, self.addr(a))
    }
    fn r16(&self, a: u32) -> Option<u16> {
        rd16(self.rom, self.addr(a))
    }
    fn r32(&self, a: u32) -> Option<u32> {
        rd32(self.rom, self.addr(a))
    }

    /// A BIT entry by id: (version, length, offset).
    pub fn bit_entry(&self, id: u8) -> Option<(u8, u16, u16)> {
        let b = self.bit;
        let hdr_len = self.r8(b + 9)? as u32;
        let count = self.r8(b + 10)? as u32;
        let mut e = b + 12;
        for _ in 0..count {
            if self.r8(e)? == id {
                return Some((self.r8(e + 1)?, self.r16(e + 2)?, self.r16(e + 4)?));
            }
            e += hdr_len;
        }
        None
    }

    /// "major.chip.minor.micro.patch" from BIT 'i'.
    pub fn version(&self) -> Option<String> {
        let (_, len, off) = self.bit_entry(b'i')?;
        if len < 5 {
            return None;
        }
        let o = off as u32;
        Some(format!("{:02x}.{:02x}.{:02x}.{:02x}.{:02x}", self.r8(o + 3)?, self.r8(o + 2)?, self.r8(o + 1)?, self.r8(o)?, self.r8(o + 4)?))
    }

    /// The falcon ucode table ('p' v2): every (type, data pointer).
    pub fn ucodes(&self) -> Vec<(u8, u32)> {
        let mut out = Vec::new();
        let Some((ver, len, off)) = self.bit_entry(b'p') else { return out };
        if ver != 2 || len < 4 {
            return out;
        }
        let Some(t) = self.r32(off as u32) else { return out };
        if t == 0 {
            return out;
        }
        let (Some(hdr), Some(elen), Some(cnt)) = (self.r8(t + 1), self.r8(t + 2), self.r8(t + 3)) else { return out };
        for i in 0..cnt as u32 {
            let e = t + hdr as u32 + i * elen as u32;
            if let (Some(ty), Some(data)) = (self.r8(e), self.r32(e + 2)) {
                out.push((ty, data));
            }
        }
        out
    }

    pub fn fwsec(&self) -> Result<Fwsec, String> {
        let (_, data) = self.ucodes().into_iter().find(|(t, _)| *t == UCODE_FWSEC).ok_or("no FWSEC (type 0x85) in the falcon ucode table")?;
        let d = self.addr(data);
        let r = self.rom;
        let hdr = rd32(r, d).ok_or("FWSEC descriptor is past the end of the ROM")?;
        if hdr & 1 == 0 {
            return Err(format!("FWSEC descriptor header {hdr:#x} has no valid bit"));
        }
        let desc_size = (hdr >> 16) as usize;
        let version = ((hdr >> 8) & 0xFF) as u8;
        let g = |o: usize| rd32(r, d + o).ok_or_else(|| String::from("FWSEC descriptor cut short"));
        let mut f = match version {
            2 => Fwsec {
                version,
                desc: d,
                desc_size,
                image: d + desc_size,
                interface_offset: g(0x10)?,
                imem_phys_base: g(0x14)?,
                imem_load_size: g(0x18)?,
                imem_virt_base: g(0x1C)?,
                dmem_offset: g(0x28)?,
                dmem_phys_base: g(0x2C)?,
                dmem_load_size: g(0x30)?,
                pkc_data_offset: 0,
                engine_id_mask: 0,
                ucode_id: 0,
                signature_count: 0,
                signature_versions: 0,
                signatures: 0,
                dmemmapper: None,
                cmd_in: None,
            },
            3 => {
                let imem_load_size = g(0x14)?;
                Fwsec {
                    version,
                    desc: d,
                    desc_size,
                    image: d + desc_size,
                    pkc_data_offset: g(0x08)?,
                    interface_offset: g(0x0C)?,
                    imem_phys_base: g(0x10)?,
                    imem_load_size,
                    imem_virt_base: g(0x18)?,
                    dmem_phys_base: g(0x1C)?,
                    dmem_load_size: g(0x20)?,
                    dmem_offset: imem_load_size,
                    engine_id_mask: rd16(r, d + 0x24).ok_or("cut short")?,
                    ucode_id: rd8(r, d + 0x26).ok_or("cut short")?,
                    signature_count: rd8(r, d + 0x27).ok_or("cut short")?,
                    signature_versions: rd16(r, d + 0x28).ok_or("cut short")?,
                    signatures: d + 0x2C,
                    dmemmapper: None,
                    cmd_in: None,
                }
            },
            v => return Err(format!("FWSEC descriptor version {v} is not known (2 and 3 are)")),
        };
        let end = f.image + (f.imem_load_size + f.dmem_load_size) as usize;
        if end > r.len() {
            return Err(format!("FWSEC image runs past the ROM ({end:#x} > {:#x})", r.len()));
        }
        // the application interface table in DMEM -> the DMEMMAPPER block -> its command buffer
        let dmem = f.image + f.dmem_offset as usize;
        let ai = dmem + f.interface_offset as usize;
        if rd8(r, ai) == Some(1) {
            let (h, l, n) = (rd8(r, ai + 1).unwrap_or(0) as usize, rd8(r, ai + 2).unwrap_or(0) as usize, rd8(r, ai + 3).unwrap_or(0) as usize);
            for i in 0..n {
                let e = ai + h + i * l;
                if rd32(r, e) == Some(APPIF_DMEMMAPPER) {
                    if let Some(base) = rd32(r, e + 4) {
                        f.dmemmapper = Some(base);
                        let m = dmem + base as usize;
                        // v3 block: signature, version u16, size u16, cmd_in offset, cmd_in size, ..., init_cmd at +0x2C
                        if let (Some(off), Some(sz)) = (rd32(r, m + 0x08), rd32(r, m + 0x0C)) {
                            f.cmd_in = Some((off, sz));
                        }
                    }
                    break;
                }
            }
        }
        Ok(f)
    }

    /// Everything above, for the `gpu vbios` report.
    pub fn report(&self) -> String {
        let mut s = format!("VBIOS: {} bytes, version {}\n", self.rom.len(), self.version().unwrap_or_else(|| String::from("?")));
        for (i, im) in self.images.iter().enumerate() {
            s.push_str(&format!("  image {i}: at {:#07x}, {:#x} bytes, type {:#04x}{}\n", im.base, im.size, im.kind, if im.last { ", last" } else { "" }));
        }
        s.push_str(&format!("  BIT at {:#x}; pointers past {:#x} map into the 0xE0 image at {:#x}\n", self.bit, self.image0_size, self.imaged));
        let uc = self.ucodes();
        s.push_str(&format!("  falcon ucodes: {}\n", uc.iter().map(|(t, a)| format!("{t:#04x}@{a:#x}")).collect::<Vec<_>>().join(" ")));
        match self.fwsec() {
            Ok(f) => {
                s.push_str(&format!(
                    "  FWSEC: descriptor v{} at {:#x} ({} bytes); IMEM {:#x} bytes @ {:#x}, DMEM {:#x} bytes @ {:#x}; interface at DMEM+{:#x}\n",
                    f.version, f.desc, f.desc_size, f.imem_load_size, f.imem_phys_base, f.dmem_load_size, f.dmem_phys_base, f.interface_offset
                ));
                if f.version == 3 {
                    s.push_str(&format!(
                        "    signatures: {} (versions mask {:#x}), patched at DMEM+{:#x}; ucode id {}, engine mask {:#x}\n",
                        f.signature_count, f.signature_versions, f.pkc_data_offset, f.ucode_id, f.engine_id_mask
                    ));
                }
                match (f.dmemmapper, f.cmd_in) {
                    (Some(m), Some((o, z))) => s.push_str(&format!("    DMEMMAPPER at DMEM+{m:#x}; command buffer DMEM+{o:#x} ({z} bytes)\n")),
                    _ => s.push_str("    DMEMMAPPER: not found (the interface table looks different)\n"),
                }
            },
            Err(e) => s.push_str(&format!("  FWSEC: {e}\n")),
        }
        s
    }
}
