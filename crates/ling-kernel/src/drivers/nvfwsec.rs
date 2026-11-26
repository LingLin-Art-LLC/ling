//! NVIDIA GSP compute driver, stage 2 (x86-64, GA10x): run the card's own FWSEC firmware (from
//! its VBIOS) with the FRTS command. FWSEC is a signed "heavy-secure" falcon program; FRTS
//! ("firmware runtime security") makes it carve out a 1 MiB region near the top of VRAM and
//! protect it as WPR2 (write-protected region 2). GSP-RM's booter (stage 3) grows WPR2 from there,
//! so this is the step that has to work first.
//!
//! Everything here is ported from nouveau (Linux v6.8, MIT licence, drivers/gpu/drm/nouveau/nvkm):
//!  - subdev/gsp/fwsec.c: nvkm_gsp_fwsec / _v3 / nvkm_gsp_fwsec_patch / nvkm_gsp_fwsec_frts;
//!  - subdev/gsp/r535.c r535_gsp_oneinit ("Calculate FB layout") and tu102.c tu102_gsp_oneinit /
//!    tu102_gsp_vga_workspace_addr: where FRTS goes;
//!  - subdev/gsp/ga102.c ga102_gsp_fwsec_signature: which of the ROM's signatures to use;
//!  - falcon/fw.c nvkm_falcon_fw_boot, falcon/base.c nvkm_falcon_reset / nvkm_falcon_dma_wr,
//!    falcon/gm200.c gm200_flcn_disable / _enable / _fw_boot, falcon/gp102.c gp102_flcn_reset_eng,
//!    falcon/ga102.c ga102_flcn_* (select, reset_prep, scrubbing, DMA, fw_load, fw_boot).
//!
//! The file has two halves. The top half is pure (a ROM copy plus a few register values in, a
//! plan with the exact patched image out) and is unit-tested on the host against the user's real
//! ROM. The bottom half (`hw`, kernel only) reads those registers and runs the falcon sequence.
//!
//! Commands: `gpu fwsec plan` (reads only, never writes a GPU register) and `gpu fwsec run yes`.

use super::nvvbios::{Fwsec, Vbios, CMD_FRTS, DMEMMAPPER_INIT_CMD};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

// ---- registers (BAR0 offsets) ---------------------------------------------------------------

/// GA102+: usable framebuffer size in MiB (nouveau fb/ga102.c ga102_fb_vidmem_size).
pub const REG_USABLE_FB_MB: u32 = 0x0011_83A4;
/// Display: VGA workspace base (bit 3 = valid, bits 31:8 = address >> 16). Read by
/// tu102_gsp_vga_workspace_addr (and bios/shadowramin.c). Read-only use here.
pub const REG_VGA_WORKSPACE: u32 = 0x0062_5F04;
/// GSP ucode fuse versions, one register per ucode id (ga102_gsp_fwsec_signature):
/// 0x8241c0 + (ucode_id - 1) * 4, valid when the descriptor's engine mask has bit 10 (GSP).
pub const REG_FUSE_GSP_UCODE1: u32 = 0x0082_41C0;
/// NV_PFB_PRI_MMU_WPR2_ADDR_LO / _HI (nvkm_gsp_fwsec_frts, r535_gsp_booter_unload): HI != 0
/// means WPR2 is up. Field 31:4 holds address >> 12.
pub const REG_WPR2_LO: u32 = 0x001F_A824;
pub const REG_WPR2_HI: u32 = 0x001F_A828;
/// NV_PBUS_SW_SCRATCH(0xe): FWSEC-FRTS reports its error code in bits 31:16
/// (nvkm_gsp_fwsec_frts: `nvkm_rd32(device, 0x001400 + (0xe * 4)) >> 16`).
pub const REG_SCRATCH_E: u32 = 0x0000_1400 + 0xE * 4;
/// The GSP falcon on GA10x (nvkm_top gives this base) and its RISC-V / "addr2" block
/// (ga102_gsp_flcn.addr2 = 0x1000).
pub const GSP: u32 = 0x0011_0000;
pub const GSP2: u32 = GSP + 0x1000;

// falcon registers, relative to GSP (names from the falcon programming model; offsets as nouveau)
pub const F_IRQMCLR: u32 = 0x014; // write 1s: mask interrupts off (gm200_flcn_disable)
pub const F_MAILBOX0: u32 = 0x040;
pub const F_MAILBOX1: u32 = 0x044;
pub const F_ITFEN: u32 = 0x048; // interface enables, bits 1:0 cleared on disable
pub const F_RM: u32 = 0x084; // gets a copy of PMC_BOOT_0 on enable (gm200_flcn_enable)
pub const F_CPUCTL: u32 = 0x100; // bit 1 STARTCPU, bit 4 HALTED
pub const F_BOOTVEC: u32 = 0x104;
pub const F_HWCFG: u32 = 0x108; // IMEM size (8:0, *256), DMEM size (17:9, *256)
pub const F_DMACTL: u32 = 0x10C; // 0 = DMA without a context (ga102_flcn_fw_load)
pub const F_DMATRFBASE: u32 = 0x110; // DMA source base >> 8
pub const F_DMATRFMOFFS: u32 = 0x114; // destination offset in IMEM / DMEM
pub const F_DMATRFCMD: u32 = 0x118; // the transfer command; bit 1 = idle
pub const F_DMATRFFBOFFS: u32 = 0x11C; // source offset from the base
pub const F_DMATRFBASE1: u32 = 0x128; // DMA source base, high bits
pub const F_HWCFG1: u32 = 0x12C; // version 3:0, security 5:4, ports
pub const F_RESET_STATUS: u32 = 0x0F4; // bit 12 = memory scrubbing busy, bit 31 = ready for reset
pub const F_ENGINE: u32 = 0x3C0; // bit 0 = engine reset (gp102_flcn_reset_eng)
pub const F_FBIF_TRANSCFG0: u32 = 0x600; // DMA context 0: target 1:0, physical 2
pub const F_FBIF_CTL: u32 = 0x624; // bit 7 = allow physical addresses without a context
// "addr2" (RISC-V) block registers, relative to GSP2
pub const R_BROM_MOD_SEL: u32 = 0x180; // 1 = boot ROM checks the RSA3K PKC signature
pub const R_BROM_UCODE_ID: u32 = 0x198;
pub const R_BROM_ENGIDMASK: u32 = 0x19C;
pub const R_BROM_PARAADDR0: u32 = 0x210; // DMEM offset of the patched signature
pub const R_CPUCTL: u32 = 0x388; // bit 7 = RISC-V core active (ga102_flcn_riscv_active)
pub const R_BCR_CTRL: u32 = 0x668; // core select: bit 4 = RISC-V, bit 0 = valid (ga102_flcn_select)

/// Bytes of one FWSEC signature (RSA-3K, nvkm_gsp_fwsec_v3: `96 * 4`).
pub const SIG_SIZE: usize = 96 * 4;
/// FRTS region size (r535_gsp_oneinit: `gsp->fb.wpr2.frts.size = 0x100000`).
pub const FRTS_SIZE: u64 = 0x10_0000;
/// The FWSEC_FRTS command structure is 24 + 20 bytes (struct nvfw_fwsec_frts_cmd).
pub const FRTS_CMD_LEN: usize = 44;
/// NVFW_FRTS_CMD_REGION_TYPE_FB.
pub const FRTS_REGION_FB: u32 = 2;

// ---- pure planning ---------------------------------------------------------------------------

/// The register values the plan depends on (all read-only reads).
#[derive(Clone, Copy, Default)]
pub struct Inputs {
    pub vram_mib: u32,
    pub vga_workspace: u32,
    pub fuse: u32,
    pub wpr2_lo: u32,
    pub wpr2_hi: u32,
    pub scratch_e: u32,
}

/// Where things go in VRAM (byte addresses).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub fb_size: u64,
    pub vga_addr: u64,
    pub vga_size: u64,
    /// How the VGA workspace address was found (tu102_gsp_vga_workspace_addr's three cases).
    pub vga_how: &'static str,
    pub frts_addr: u64,
    pub frts_size: u64,
}

/// tu102_gsp_oneinit + the FRTS part of r535_gsp_oneinit.
pub fn layout(fb_size: u64, vga_reg: u32) -> Result<Layout, String> {
    if fb_size < 64 << 20 || fb_size & 0xFFFFF != 0 {
        return Err(format!("VRAM size {fb_size:#x} is not plausible"));
    }
    let base = fb_size - 0x10_0000;
    let (vga_addr, vga_how) = if vga_reg & 0x8 == 0 {
        (base, "0x625f04 bit 3 clear: top 1 MiB")
    } else {
        let a = ((vga_reg & 0xFFFF_FF00) as u64) << 8;
        if a < base { (fb_size - 0x2_0000, "0x625f04 points below the top 1 MiB: top 128 KiB") } else { (a, "0x625f04 address") }
    };
    if vga_addr >= fb_size {
        return Err(format!("VGA workspace {vga_addr:#x} is past the end of VRAM {fb_size:#x}"));
    }
    // gsp->fb.bios.addr = vga_workspace.addr; frts.addr = ALIGN_DOWN(bios.addr, 0x20000) - frts.size
    let frts_addr = (vga_addr & !0x1_FFFF) - FRTS_SIZE;
    Ok(Layout { fb_size, vga_addr, vga_size: fb_size - vga_addr, vga_how, frts_addr, frts_size: FRTS_SIZE })
}

/// The fuse register to read for this FWSEC (ga102_gsp_fwsec_signature).
pub fn fuse_register(f: &Fwsec) -> Result<u32, String> {
    if f.engine_id_mask & 0x400 == 0 {
        return Err(format!("FWSEC engine mask {:#x} has no GSP bit (0x400): not the GA10x layout", f.engine_id_mask));
    }
    if f.ucode_id == 0 {
        return Err(String::from("FWSEC ucode id 0"));
    }
    Ok(REG_FUSE_GSP_UCODE1 + (f.ucode_id as u32 - 1) * 4)
}

/// Linux fls(): 1-based index of the highest set bit, 0 for 0.
fn fls(x: u32) -> u32 {
    32 - x.leading_zeros()
}

/// ga102_gsp_fwsec_signature: the fuse register counts burned fuses (thermometer code); the
/// version it asks for is bit fls(reg). The descriptor's SignatureVersions mask says which
/// versions the ROM carries, in order; the index is how many carried versions come below it.
/// Returns (index, the version bit).
pub fn pick_signature(fuse: u32, versions: u16, count: u8) -> Result<(usize, u32), String> {
    if fuse & 0xFFF0_0000 == 0xBAD0_0000 {
        return Err(format!("fuse register reads {fuse:#010x}: a PRI error value, not a fuse count"));
    }
    let f = fls(fuse);
    if f >= 32 {
        return Err(format!("fuse register {fuse:#x} asks for version bit 32"));
    }
    let want = 1u32 << f;
    let mut sig = versions as u32;
    if want & sig == 0 {
        return Err(format!("the fuses ask for signature version bit {want:#x} (fuse register {fuse:#x}), the ROM only has {versions:#x}"));
    }
    let (mut reg, mut idx) = (want, 0usize);
    while reg & sig & 1 == 0 {
        idx += (sig & 1) as usize;
        reg >>= 1;
        sig >>= 1;
    }
    if idx >= count as usize {
        return Err(format!("signature index {idx} but the ROM has only {count}"));
    }
    Ok((idx, want))
}

/// The FWSEC_FRTS command block (nvkm_gsp_fwsec_patch): read_vbios {ver 1, hdr 24, addr 0,
/// size 0, flags 2} then frts_region {ver 1, hdr 20, addr >> 12, size >> 12, type FB}.
pub fn frts_cmd(frts_addr: u64, frts_size: u64) -> [u8; FRTS_CMD_LEN] {
    let mut b = [0u8; FRTS_CMD_LEN];
    let w = |b: &mut [u8; FRTS_CMD_LEN], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    w(&mut b, 0, 1); // read_vbios.ver
    w(&mut b, 4, 24); // read_vbios.hdr = sizeof(read_vbios)
    // 8..16: read_vbios.addr (u64) = 0, 16..20: size = 0
    w(&mut b, 20, 2); // read_vbios.flags
    w(&mut b, 24, 1); // frts_region.ver
    w(&mut b, 28, 20); // frts_region.hdr = sizeof(frts_region)
    w(&mut b, 32, (frts_addr >> 12) as u32);
    w(&mut b, 36, (frts_size >> 12) as u32);
    w(&mut b, 40, FRTS_REGION_FB);
    b
}

pub struct Patch {
    pub what: &'static str,
    /// Offset inside DMEM.
    pub dmem_off: u32,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
}

pub struct Plan {
    pub inputs: Inputs,
    pub fw: Fwsec,
    pub layout: Layout,
    pub fuse_reg: u32,
    pub sig_index: usize,
    pub sig_version: u32,
    /// The exact bytes the falcon gets: IMEM image, then DMEM (padded to 256 bytes).
    pub image: Vec<u8>,
    pub imem_size: u32,
    /// Offset of DMEM inside `image` (nouveau fw->dmem_base_img) and its loaded size.
    pub dmem_img: u32,
    pub dmem_size: u32,
    pub patches: Vec<Patch>,
    pub cmd: [u8; FRTS_CMD_LEN],
    pub rom_len: usize,
    pub rom_crc: u32,
    pub image_crc: u32,
}

/// CRC-32 (IEEE), to tie a plan to a run.
pub fn crc32(d: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in d {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    !c
}

pub fn wpr2_addr(v: u32) -> u64 {
    ((v & 0xFFFF_FFF0) as u64) << 8
}

impl Plan {
    pub fn wpr2_up(&self) -> bool {
        self.inputs.wpr2_hi != 0
    }
}

/// Parse the ROM, pick the signature, compute the layout and build the patched image
/// (nvkm_gsp_fwsec_v3 + nvkm_falcon_fw_patch + nvkm_gsp_fwsec_patch).
pub fn plan(rom: &[u8], inputs: Inputs) -> Result<Plan, String> {
    let v = Vbios::parse(rom)?;
    let fw = v.fwsec()?;
    if fw.version != 3 {
        return Err(format!("FWSEC descriptor v{}: only v3 (Ampere, signed with PKC) is handled", fw.version));
    }
    let fuse_reg = fuse_register(&fw)?;
    let (sig_index, sig_version) = pick_signature(inputs.fuse, fw.signature_versions, fw.signature_count)?;
    let layout = layout((inputs.vram_mib as u64) << 20, inputs.vga_workspace)?;

    // nvkm_gsp_fwsec_v3: imem 0..IMEMLoadSize, dmem at img+IMEMLoadSize, size ALIGN(DMEMLoadSize, 256)
    let imem_size = fw.imem_load_size;
    let dmem_img = fw.imem_load_size;
    let dmem_size = (fw.dmem_load_size + 255) & !255;
    if imem_size == 0 || imem_size % 256 != 0 {
        return Err(format!("IMEM size {imem_size:#x} is not a multiple of 256 (the DMA moves 256-byte blocks)"));
    }
    if fw.imem_phys_base % 256 != 0 || fw.dmem_phys_base % 256 != 0 {
        return Err(format!("IMEM/DMEM bases {:#x}/{:#x} are not 256-aligned", fw.imem_phys_base, fw.dmem_phys_base));
    }
    let src_len = (fw.imem_load_size + fw.dmem_load_size) as usize;
    let src = rom.get(fw.image..fw.image + src_len).ok_or("FWSEC image is past the end of the ROM")?;
    let mut image = Vec::with_capacity((imem_size + dmem_size) as usize);
    image.extend_from_slice(src);
    image.resize((imem_size + dmem_size) as usize, 0);

    let mut patches = Vec::new();
    let mut put = |image: &mut Vec<u8>, what: &'static str, dmem_off: u32, bytes: &[u8]| -> Result<(), String> {
        let end = dmem_off as usize + bytes.len();
        if end > fw.dmem_load_size as usize {
            return Err(format!("{what} at DMEM+{dmem_off:#x} ({} bytes) runs past DMEM ({:#x})", bytes.len(), fw.dmem_load_size));
        }
        let at = (dmem_img + dmem_off) as usize;
        let before = image[at..at + bytes.len()].to_vec();
        image[at..at + bytes.len()].copy_from_slice(bytes);
        patches.push(Patch { what, dmem_off, before, after: bytes.to_vec() });
        Ok(())
    };

    // 1. the signature (nvkm_falcon_fw_sign + _patch): the ROM keeps SignatureCount signatures
    //    right after the 0x2c-byte descriptor; the chosen one goes to DMEM + PKCDataOffset
    if 0x2C + fw.signature_count as usize * SIG_SIZE > fw.desc_size {
        return Err(format!("{} signatures do not fit the {}-byte descriptor", fw.signature_count, fw.desc_size));
    }
    let s = fw.signatures + sig_index * SIG_SIZE;
    let sig = rom.get(s..s + SIG_SIZE).ok_or("signature past the end of the ROM")?.to_vec();
    put(&mut image, "signature", fw.pkc_data_offset, &sig)?;

    // 2. DMEMMAPPER init_cmd = FRTS (nvkm_gsp_fwsec_patch)
    let mapper = fw.dmemmapper.ok_or("no DMEMMAPPER entry in FWSEC's application interface table")?;
    let (cmd_off, cmd_size) = fw.cmd_in.ok_or("DMEMMAPPER has no command buffer")?;
    if (cmd_size as usize) < FRTS_CMD_LEN {
        return Err(format!("command buffer is {cmd_size} bytes, FRTS needs {FRTS_CMD_LEN}"));
    }
    put(&mut image, "DMEMMAPPER init_cmd", mapper + DMEMMAPPER_INIT_CMD, &CMD_FRTS.to_le_bytes())?;

    // 3. the FRTS command in the input buffer
    let cmd = frts_cmd(layout.frts_addr, layout.frts_size);
    put(&mut image, "FRTS command", cmd_off, &cmd)?;

    // the three patches must not overlap each other
    for (i, a) in patches.iter().enumerate() {
        for b in &patches[i + 1..] {
            let (a0, a1) = (a.dmem_off, a.dmem_off + a.after.len() as u32);
            let (b0, b1) = (b.dmem_off, b.dmem_off + b.after.len() as u32);
            if a0 < b1 && b0 < a1 {
                return Err(format!("patches overlap: {} and {}", a.what, b.what));
            }
        }
    }
    let image_crc = crc32(&image);
    Ok(Plan { inputs, fw, layout, fuse_reg, sig_index, sig_version, image, imem_size, dmem_img, dmem_size, patches, cmd, rom_len: rom.len(), rom_crc: crc32(rom), image_crc })
}

fn hex_lines(s: &mut String, base: u32, b: &[u8]) {
    for (i, c) in b.chunks(16).enumerate() {
        s.push_str(&format!("      DMEM+{:#05x}:", base as usize + i * 16));
        for x in c {
            s.push_str(&format!(" {x:02x}"));
        }
        s.push('\n');
    }
}

fn gib(n: u64) -> String {
    format!("{:#x} ({} MiB)", n, n >> 20)
}

impl Plan {
    pub fn report(&self) -> String {
        let (f, l, i) = (&self.fw, &self.layout, &self.inputs);
        let mut s = String::new();
        s.push_str("FWSEC-FRTS plan\n");
        s.push_str(&format!("  ROM: {} bytes, crc32 {:08x}\n", self.rom_len, self.rom_crc));
        s.push_str(&format!(
            "  FWSEC v{} descriptor at ROM {:#x}; image at ROM {:#x}: IMEM {:#x} bytes -> IMEM {:#x}, DMEM {:#x} bytes (loaded {:#x}) -> DMEM {:#x}\n",
            f.version, f.desc, f.image, f.imem_load_size, f.imem_phys_base, f.dmem_load_size, self.dmem_size, f.dmem_phys_base
        ));
        s.push_str(&format!("  ucode id {}, engine mask {:#x}, boot vector 0, signature at DMEM+{:#x}\n", f.ucode_id, f.engine_id_mask, f.pkc_data_offset));
        s.push_str("registers read (read-only):\n");
        s.push_str(&format!("  {REG_USABLE_FB_MB:#08x} usable FB size      = {:#010x} ({} MiB)\n", i.vram_mib, i.vram_mib));
        s.push_str(&format!("  {REG_VGA_WORKSPACE:#08x} VGA workspace       = {:#010x}\n", i.vga_workspace));
        s.push_str(&format!("  {:#08x} GSP ucode {} fuse   = {:#010x}\n", self.fuse_reg, f.ucode_id, i.fuse));
        s.push_str(&format!("  {REG_WPR2_LO:#08x} WPR2 lo             = {:#010x}\n", i.wpr2_lo));
        s.push_str(&format!("  {REG_WPR2_HI:#08x} WPR2 hi             = {:#010x}\n", i.wpr2_hi));
        s.push_str(&format!("  {REG_SCRATCH_E:#08x} SW scratch 0xe      = {:#010x}\n", i.scratch_e));
        s.push_str("signature:\n");
        s.push_str(&format!(
            "  fuse {:#x} -> fls {} -> wants version bit {:#x}; ROM carries mask {:#x} ({} signatures) -> index {} (ROM {:#x})\n",
            i.fuse,
            fls(i.fuse),
            self.sig_version,
            f.signature_versions,
            f.signature_count,
            self.sig_index,
            f.signatures + self.sig_index * SIG_SIZE
        ));
        s.push_str("VRAM layout (tu102_gsp_oneinit / r535_gsp_oneinit):\n");
        s.push_str(&format!("  VRAM size        {}\n", gib(l.fb_size)));
        s.push_str(&format!("  VGA workspace    {:#x} .. {:#x} ({} KiB; {})\n", l.vga_addr, l.fb_size, l.vga_size >> 10, l.vga_how));
        s.push_str(&format!("  FRTS             {:#x} .. {:#x} ({} KiB) = ALIGN_DOWN(VGA, 128 KiB) - 1 MiB\n", l.frts_addr, l.frts_addr + l.frts_size, l.frts_size >> 10));
        s.push_str("  below FRTS       stage 3 (booter) extends WPR2 downward: GSP boot loader, GSP-RM ELF, heap\n");
        s.push_str(&format!("  low VRAM         the firmware framebuffer (BAR1+0) -- untouched; {} MiB below FRTS stay free\n", l.frts_addr >> 20));
        s.push_str("DMEM patches:\n");
        for p in &self.patches {
            if p.what == "signature" {
                s.push_str(&format!(
                    "  {:<20} DMEM+{:#05x} {} bytes, crc32 {:08x}; first 16: {:02x?}\n",
                    p.what,
                    p.dmem_off,
                    p.after.len(),
                    crc32(&p.after),
                    &p.after[..16]
                ));
            } else if p.after.len() == 4 {
                let b = u32::from_le_bytes([p.before[0], p.before[1], p.before[2], p.before[3]]);
                let a = u32::from_le_bytes([p.after[0], p.after[1], p.after[2], p.after[3]]);
                s.push_str(&format!("  {:<20} DMEM+{:#05x} {b:#010x} -> {a:#010x}\n", p.what, p.dmem_off));
            } else {
                s.push_str(&format!("  {:<20} DMEM+{:#05x} {} bytes; was:\n", p.what, p.dmem_off, p.after.len()));
                hex_lines(&mut s, p.dmem_off, &p.before);
                s.push_str("    now:\n");
                hex_lines(&mut s, p.dmem_off, &p.after);
            }
        }
        let w = |o: usize| u32::from_le_bytes([self.cmd[o], self.cmd[o + 1], self.cmd[o + 2], self.cmd[o + 3]]);
        s.push_str(&format!(
            "  FRTS command decoded: read_vbios {{ver {}, hdr {}, addr 0, size 0, flags {}}}, frts_region {{ver {}, hdr {}, addr {:#x} (<<12 = {:#x}), size {:#x} (<<12 = {:#x}), type {} (FB)}}\n",
            w(0),
            w(4),
            w(20),
            w(24),
            w(28),
            w(32),
            (w(32) as u64) << 12,
            w(36),
            (w(36) as u64) << 12,
            w(40)
        ));
        s.push_str(&format!("image: {} bytes (IMEM {:#x} + DMEM {:#x}), crc32 {:08x}\n", self.image.len(), self.imem_size, self.dmem_size, self.image_crc));
        if self.wpr2_up() {
            s.push_str(&format!(
                "WPR2 IS ALREADY SET ({:#x} .. {:#x}): FRTS already ran this power cycle (or GSP-RM is up). `gpu fwsec run` will refuse; power the PC off fully to clear it.\n",
                wpr2_addr(i.wpr2_lo),
                wpr2_addr(i.wpr2_hi)
            ));
        } else {
            s.push_str("WPR2 is not set: FRTS has not run this power cycle.\n");
        }
        s
    }
}

// ---- hardware (kernel only) ------------------------------------------------------------------

#[cfg(target_os = "none")]
pub use hw::command;

#[cfg(target_os = "none")]
mod hw {
    use super::*;
    use crate::arch::pci;
    use crate::arch::timer;
    use crate::drivers::nvgpu::{self, Gpu};

    /// Where the saved ROM (`gpu vbios`) and the report live.
    const DATA_DIRS: [&str; 2] = ["media/data", "media/lingdata"];

    unsafe fn rd(bar0: u64, reg: u32) -> u32 {
        core::ptr::read_volatile((bar0 + reg as u64) as *const u32)
    }

    /// The saved VBIOS from `gpu vbios` (no GPU access at all).
    fn saved_rom(g: &Gpu) -> Option<(String, Vec<u8>)> {
        for d in ["media/lingdata", "media/data"] {
            let p = format!("{d}/nvidia/vbios-10de-{:04x}.rom", g.device_id);
            if let Some(r) = crate::vfs::read(&p) {
                if r.len() > 0x1000 && r[0] == 0x55 && r[1] == 0xAA {
                    return Some((p, r));
                }
            }
        }
        None
    }

    /// Read-only falcon state, for the report.
    fn falcon_state(b: u64) -> String {
        unsafe {
            let hw = rd(b, GSP + F_HWCFG);
            format!(
                "GSP falcon (read-only): CPUCTL {:#010x}, MAILBOX0 {:#010x}, MAILBOX1 {:#010x}, HWCFG {:#010x} (IMEM {} KiB, DMEM {} KiB), HWCFG1 {:#010x}, RESET_STATUS {:#010x}, BCR_CTRL {:#010x}, RISCV_CPUCTL {:#010x}\n",
                rd(b, GSP + F_CPUCTL),
                rd(b, GSP + F_MAILBOX0),
                rd(b, GSP + F_MAILBOX1),
                hw,
                ((hw & 0x1FF) << 8) >> 10,
                ((hw & 0x3FE00) >> 1) >> 10,
                rd(b, GSP + F_HWCFG1),
                rd(b, GSP + F_RESET_STATUS),
                rd(b, GSP2 + R_BCR_CTRL),
                rd(b, GSP2 + R_CPUCTL)
            )
        }
    }

    /// Everything `plan` needs from the card, by reads only.
    fn read_plan(g: &Gpu) -> Result<(Plan, String), String> {
        if g.boot0 == 0 || g.boot0 == 0xFFFF_FFFF {
            return Err(String::from("the card's registers are not readable (see `gpu`)"));
        }
        let (path, rom) = saved_rom(g).ok_or("no saved VBIOS on the data drive: run `gpu vbios` first (plan reads the saved copy so it never writes the ROM-shadow bit)")?;
        let v = Vbios::parse(&rom)?;
        let fw = v.fwsec()?;
        let fuse_reg = fuse_register(&fw)?;
        let b = g.bar0;
        let inputs = unsafe {
            Inputs {
                vram_mib: rd(b, REG_USABLE_FB_MB),
                vga_workspace: rd(b, REG_VGA_WORKSPACE),
                fuse: rd(b, fuse_reg),
                wpr2_lo: rd(b, REG_WPR2_LO),
                wpr2_hi: rd(b, REG_WPR2_HI),
                scratch_e: rd(b, REG_SCRATCH_E),
            }
        };
        let p = plan(&rom, inputs)?;
        let cmd = g.loc.read32(0x04);
        let mut s = format!(
            "gpu {:02x}:{:02x}.{} 10de:{:04x} {} PMC_BOOT_0 {:#010x}, BAR0 {:#x}; VBIOS from {path}\n",
            g.loc.bus,
            g.loc.device,
            g.loc.function,
            g.device_id,
            nvgpu::chip_name(g.chipset()),
            g.boot0,
            g.bar0
        );
        s.push_str(&format!("PCI command {:#06x}: memory decode {}, bus master {}\n", cmd & 0xFFFF, if cmd & 2 != 0 { "on" } else { "OFF" }, if cmd & 4 != 0 { "on" } else { "off (run turns it on: the falcon's DMA reads the image from RAM)" }));
        s.push_str(&falcon_state(b));
        s.push_str(&p.report());
        Ok((p, s))
    }

    fn save_report(text: &str) -> Option<String> {
        for d in DATA_DIRS {
            if !crate::vfs::exists(d) {
                continue;
            }
            let _ = crate::vfs::mkdir(&format!("{d}/nvidia"));
            let p = format!("{d}/nvidia/fwsec-report.txt");
            if crate::vfs::write(&p, text.as_bytes()) {
                let _ = crate::fs::media::flush(&p);
                // read it back: the report must really be on the drive before the GPU is touched
                if crate::vfs::read(&p).map(|r| r.len()) == Some(text.len()) {
                    return Some(p);
                }
            }
        }
        None
    }

    /// The GSP falcon, with every write logged (old value -> new value).
    struct Falcon {
        b: u64,
        log: String,
        quiet: bool,
    }

    impl Falcon {
        fn rd(&self, reg: u32) -> u32 {
            unsafe { rd(self.b, reg) }
        }
        fn wr(&mut self, reg: u32, v: u32, why: &str) {
            if !self.quiet {
                let old = self.rd(reg);
                self.log.push_str(&format!("    wr {reg:#08x} = {v:#010x} (was {old:#010x})  {why}\n"));
            }
            unsafe { core::ptr::write_volatile((self.b + reg as u64) as *mut u32, v) };
        }
        /// nvkm_mask: read, clear `mask`, or in `val`, write.
        fn mask(&mut self, reg: u32, mask: u32, val: u32, why: &str) {
            let old = self.rd(reg);
            let v = (old & !mask) | val;
            if !self.quiet {
                self.log.push_str(&format!("    mask {reg:#08x}: {old:#010x} -> {v:#010x}  {why}\n"));
            }
            unsafe { core::ptr::write_volatile((self.b + reg as u64) as *mut u32, v) };
        }
        /// Poll until `done(value)` or `us` microseconds pass; logs the outcome.
        fn wait(&mut self, reg: u32, us: u64, what: &str, done: impl Fn(u32) -> bool) -> bool {
            let t0 = timer::now_us();
            loop {
                let v = self.rd(reg);
                if done(v) {
                    if !self.quiet {
                        self.log.push_str(&format!("    wait {what}: ok after {} us ({reg:#08x} = {v:#010x})\n", timer::now_us() - t0));
                    }
                    return true;
                }
                if timer::now_us() - t0 > us {
                    self.log.push_str(&format!("    wait {what}: TIMEOUT after {us} us ({reg:#08x} = {v:#010x})\n"));
                    return false;
                }
                unsafe { crate::arch::cpu::pause() };
            }
        }

        // ga102_flcn_select: switch the core from RISC-V to falcon if needed (BCR_CTRL).
        fn select(&mut self) -> bool {
            if self.rd(GSP2 + R_BCR_CTRL) & 0x10 != 0 {
                self.wr(GSP2 + R_BCR_CTRL, 0, "BCR_CTRL: select the falcon core (ga102_flcn_select)");
                return self.wait(GSP2 + R_BCR_CTRL, 10_000, "core select valid", |v| v & 1 != 0);
            }
            self.log.push_str("    select: falcon core already selected\n");
            true
        }
        // ga102_flcn_reset_prep: wait (up to 150 us, no error) for RESET_STATUS bit 31.
        fn reset_prep(&mut self) {
            let _ = self.rd(GSP + F_RESET_STATUS);
            let t0 = timer::now_us();
            while self.rd(GSP + F_RESET_STATUS) & 0x8000_0000 == 0 && timer::now_us() - t0 < 150 {
                unsafe { crate::arch::cpu::pause() };
            }
            self.log.push_str(&format!("    reset_prep: RESET_STATUS {:#010x}\n", self.rd(GSP + F_RESET_STATUS)));
        }
        // ga102_flcn_reset_wait_mem_scrubbing: touch MAILBOX0 (nvkm_mask with 0/0), wait for
        // RESET_STATUS bit 12 (IMEM/DMEM scrubbing) to clear, 20 ms.
        fn wait_scrub(&mut self) -> bool {
            self.mask(GSP + F_MAILBOX0, 0, 0, "touch MAILBOX0 (as nouveau)");
            self.wait(GSP + F_RESET_STATUS, 20_000, "memory scrubbing done", |v| v & 0x1000 == 0)
        }
        // gp102_flcn_reset_eng: pulse the engine reset bit, then wait for scrubbing.
        fn reset_eng(&mut self) -> bool {
            self.reset_prep();
            self.mask(GSP + F_ENGINE, 1, 1, "ENGINE reset on (gp102_flcn_reset_eng)");
            timer::delay_us(10);
            self.mask(GSP + F_ENGINE, 1, 0, "ENGINE reset off");
            self.wait_scrub()
        }
        // nvkm_falcon_reset = gm200_flcn_disable + gm200_flcn_enable (ga102_gsp_flcn has no
        // reset_pmc, so no PMC_ENABLE writes).
        fn reset(&mut self) -> bool {
            self.log.push_str("  reset: disable (gm200_flcn_disable)\n");
            if !self.select() {
                return false;
            }
            self.mask(GSP + F_ITFEN, 3, 0, "ITFEN off");
            self.wr(GSP + F_IRQMCLR, 0xFFFF_FFFF, "IRQMCLR: mask all falcon interrupts");
            if !self.reset_eng() {
                return false;
            }
            self.log.push_str("  reset: enable (gm200_flcn_enable)\n");
            if !self.reset_eng() || !self.select() || !self.wait_scrub() {
                return false;
            }
            let boot0 = self.rd(0);
            self.wr(GSP + F_RM, boot0, "RM = PMC_BOOT_0");
            true
        }
        // nvkm_falcon_dma_wr with ga102_flcn_dma: 256-byte blocks from `phys`(+`src`) into IMEM
        // or DMEM at `dst`. For DMEM nouveau moves the base to phys + src and counts offsets from
        // there.
        fn dma(&mut self, phys: u64, src: u32, dst: u32, len: u32, imem: bool) -> bool {
            let (base, off0) = if imem { (phys, src) } else { (phys + src as u64, 0) };
            // ga102_flcn_dma_init: size code log2(256)-2 = 6 in bits 10:8, bit 4 IMEM, bit 2 secure
            let cmd = (6 << 8) | if imem { 0x10 | 0x4 } else { 0 };
            self.wr(GSP + F_DMATRFBASE, (base >> 8) as u32, "DMATRFBASE = source >> 8");
            self.wr(GSP + F_DMATRFBASE1, 0, "DMATRFBASE1 = 0");
            self.log.push_str(&format!("    dma {} blocks of 256 bytes, cmd {cmd:#x}, {} {dst:#x}..{:#x}\n", len / 256, if imem { "IMEM" } else { "DMEM" }, dst + len));
            let mut done = 0;
            while done < len {
                unsafe {
                    core::ptr::write_volatile((self.b + (GSP + F_DMATRFMOFFS) as u64) as *mut u32, dst + done);
                    core::ptr::write_volatile((self.b + (GSP + F_DMATRFFBOFFS) as u64) as *mut u32, off0 + done);
                    core::ptr::write_volatile((self.b + (GSP + F_DMATRFCMD) as u64) as *mut u32, cmd);
                }
                self.quiet = true;
                let ok = self.wait(GSP + F_DMATRFCMD, 100_000, "DMA block idle", |v| v & 2 != 0);
                self.quiet = false;
                if !ok {
                    self.log.push_str(&format!("    dma stopped at block {} ({:#x})\n", done / 256, dst + done));
                    return false;
                }
                done += 256;
            }
            self.log.push_str(&format!("    dma done; last DMATRFCMD {:#010x}\n", self.rd(GSP + F_DMATRFCMD)));
            true
        }
    }

    fn run(g: &Gpu) -> String {
        let (p, mut rep) = match read_plan(g) {
            Ok(x) => x,
            Err(e) => return format!("gpu fwsec run: {e}\n"),
        };
        let cs = g.chipset();
        if !(0x172..=0x177).contains(&cs) {
            return format!("gpu fwsec run: chipset {cs:#x} ({}) is not GA102..GA107; this sequence is the GA10x one\n", nvgpu::chip_name(cs));
        }
        if p.wpr2_up() {
            return format!("{rep}gpu fwsec run: REFUSED. WPR2 is already up, so FWSEC-FRTS has already run this power cycle; running it again fails at best. Shut the PC down completely (not a reboot) to start over.\n");
        }
        let b = g.bar0;
        let hw = unsafe { rd(b, GSP + F_HWCFG) };
        let (imem_cap, dmem_cap) = ((hw & 0x1FF) << 8, (hw & 0x3FE00) >> 1);
        if p.imem_size > imem_cap || p.dmem_size > dmem_cap {
            return format!("{rep}gpu fwsec run: REFUSED. The image (IMEM {:#x}, DMEM {:#x}) does not fit the falcon (HWCFG {hw:#x}: IMEM {imem_cap:#x}, DMEM {dmem_cap:#x}).\n", p.imem_size, p.dmem_size);
        }
        if unsafe { rd(b, GSP + F_CPUCTL) } & 0x10 == 0 && unsafe { rd(b, GSP2 + R_CPUCTL) } & 0x80 != 0 {
            return format!("{rep}gpu fwsec run: REFUSED. The GSP's RISC-V core is active (something is already running on it).\n");
        }
        rep.push_str("\nRUN (gpu fwsec run yes)\n");
        let Some(path) = save_report(&rep) else {
            return format!("{rep}gpu fwsec run: REFUSED. Could not write the report to media/data or media/lingdata; it must be on the drive before the GPU is touched.\n");
        };
        rep.push_str(&format!("  plan saved to {path} before any GPU write\n"));

        // the image in RAM, for the falcon's DMA: 2^order zeroed frames, identity-mapped below 4 GiB
        let len = p.image.len();
        let mut order = 0;
        while (4096usize << order) < len {
            order += 1;
        }
        let Some(phys) = crate::mm::frame::alloc_frames(order) else {
            rep.push_str("  could not allocate the DMA buffer; nothing was written to the GPU\n");
            let _ = save_report(&rep);
            return rep;
        };
        if phys + (4096u64 << order) > 1 << 32 {
            crate::mm::frame::free_frames(phys, order);
            rep.push_str("  DMA buffer is above 4 GiB; nothing was written to the GPU\n");
            let _ = save_report(&rep);
            return rep;
        }
        unsafe {
            core::ptr::write_bytes(phys as *mut u8, 0, 4096 << order);
            core::ptr::copy_nonoverlapping(p.image.as_ptr(), phys as *mut u8, len);
        }
        let check = crc32(unsafe { core::slice::from_raw_parts(phys as *const u8, len) });
        rep.push_str(&format!("  image copied to RAM at {phys:#x} ({len} bytes, order {order}), crc32 {check:08x} (plan {:08x})\n", p.image_crc));
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        // the falcon reads the image itself: PCI bus mastering has to be on
        let cmd = g.loc.read32(0x04);
        if cmd & 4 == 0 {
            pci::write32(g.loc.bus, g.loc.device, g.loc.function, 0x04, (cmd & 0xFFFF) | 4);
            rep.push_str(&format!("  PCI command {:#06x} -> {:#06x} (bus master on)\n", cmd & 0xFFFF, g.loc.read32(0x04) & 0xFFFF));
        }

        let mut f = Falcon { b, log: String::new(), quiet: false };
        let t0 = timer::now_us();
        // nvkm_falcon_fw_boot: reset, load, boot
        let mut ok = f.reset();
        if ok {
            f.log.push_str("  load (ga102_flcn_fw_load)\n");
            f.mask(GSP + F_FBIF_CTL, 0x80, 0x80, "FBIF_CTL: allow physical DMA without a context");
            f.wr(GSP + F_DMACTL, 0, "DMACTL = 0");
            f.mask(GSP + F_FBIF_TRANSCFG0, 0x0001_0007, (1 << 2) | 1, "FBIF_TRANSCFG(0): physical, coherent system memory");
            ok = f.dma(phys, 0, p.fw.imem_phys_base, p.imem_size, true) && f.dma(phys, p.dmem_img, p.fw.dmem_phys_base, p.dmem_size, false);
        }
        if ok {
            f.log.push_str("  boot (ga102_flcn_fw_boot + gm200_flcn_fw_boot)\n");
            f.wr(GSP2 + R_BROM_PARAADDR0, p.fw.pkc_data_offset, "BROM PARAADDR(0) = signature offset in DMEM");
            f.wr(GSP2 + R_BROM_ENGIDMASK, p.fw.engine_id_mask as u32, "BROM ENGIDMASK");
            f.wr(GSP2 + R_BROM_UCODE_ID, p.fw.ucode_id as u32, "BROM CURR_UCODE_ID");
            f.wr(GSP2 + R_BROM_MOD_SEL, 1, "BROM MOD_SEL = RSA3K");
            f.wr(GSP + F_MAILBOX0, 0, "MAILBOX0 = 0 (nvkm_gsp_fwsec passes mbox0 = 0)");
            f.wr(GSP + F_BOOTVEC, 0, "BOOTVEC = 0");
            f.wr(GSP + F_CPUCTL, 2, "CPUCTL = STARTCPU");
            ok = f.wait(GSP + F_CPUCTL, 3_000_000, "falcon halted", |v| v & 0x10 != 0);
        }
        rep.push_str(&f.log);
        rep.push_str(&format!("  sequence took {} us\n", timer::now_us() - t0));

        // outcome: reads only from here on
        let (mb0, mb1) = unsafe { (rd(b, GSP + F_MAILBOX0), rd(b, GSP + F_MAILBOX1)) };
        let (lo, hi, sc) = unsafe { (rd(b, REG_WPR2_LO), rd(b, REG_WPR2_HI), rd(b, REG_SCRATCH_E)) };
        rep.push_str("after:\n  ");
        rep.push_str(&falcon_state(b));
        rep.push_str(&format!("  MAILBOX0 {mb0:#010x} (0 = success), MAILBOX1 {mb1:#010x}\n"));
        rep.push_str(&format!("  SW scratch 0xe {sc:#010x}: FRTS error code {:#06x} (0 = none)\n", sc >> 16));
        rep.push_str(&format!("  WPR2 lo {lo:#010x} hi {hi:#010x} -> {:#x} .. {:#x}\n", wpr2_addr(lo), wpr2_addr(hi)));
        let verdict = if !ok {
            "FAILED: a step timed out (see above); no further writes were made"
        } else if mb0 != 0 {
            "FAILED: FWSEC halted with a non-zero MAILBOX0"
        } else if sc >> 16 != 0 {
            "FAILED: FWSEC reported an FRTS error"
        } else if hi == 0 {
            "FAILED: FWSEC halted cleanly but WPR2 is not set"
        } else if wpr2_addr(lo) != p.layout.frts_addr {
            "DONE, but WPR2 does not start at the planned FRTS address (check the decode)"
        } else {
            "SUCCESS: WPR2/FRTS is up at the planned address"
        };
        rep.push_str(&format!("outcome: {verdict}\n"));
        // a timed-out DMA might still read the buffer: only give it back after a clean halt
        if ok {
            crate::mm::frame::free_frames(phys, order);
        } else {
            rep.push_str(&format!("  (the DMA buffer at {phys:#x} is left allocated on purpose)\n"));
        }
        match save_report(&rep) {
            Some(p) => rep.push_str(&format!("report saved to {p}\n")),
            None => rep.push_str("could not save the final report\n"),
        }
        rep
    }

    /// `gpu fwsec ...`
    pub fn command(arg: &str) -> String {
        let Some(g) = nvgpu::probe().first() else { return String::from("gpu fwsec: no NVIDIA GPU\n") };
        let mut it = arg.split_whitespace();
        match (it.next(), it.next()) {
            (Some("plan") | None, _) => match read_plan(g) {
                Ok((_, s)) => s + "(plan only: nothing was written to the GPU)\n",
                Err(e) => format!("gpu fwsec plan: {e}\n"),
            },
            (Some("run"), Some("yes")) => run(g),
            (Some("run"), _) => {
                let mut s = match read_plan(g) {
                    Ok((_, s)) => s,
                    Err(e) => return format!("gpu fwsec run: {e}\n"),
                };
                s.push_str(
                    "\n`gpu fwsec run` would now: save this plan to media/data/nvidia/fwsec-report.txt, copy the image to RAM, \
reset the GSP falcon (0x110000), DMA-load IMEM + DMEM, set the boot-ROM signature parameters, start it, and wait up to 3 s \
for it to halt, then read MAILBOX0/1, the FRTS error and WPR2.\nTo do it: gpu fwsec run yes\n",
                );
                s
            },
            _ => String::from("usage: gpu fwsec plan | gpu fwsec run [yes]\n"),
        }
    }
}
