//! Device drivers: console (VGA text mode + PL011 UART), input (PS/2
//! keyboard/mouse), storage (ATA PIO, AHCI), and the Multiboot2 linear
//! framebuffer. Each module is gated to the one architecture that actually
//! has it today — see `arch/mod.rs`'s doc for why a missing one is a
//! compile error at the call site rather than a stub.

#[cfg(target_arch = "x86_64")]
pub mod ac97;
#[cfg(target_arch = "x86_64")]
pub mod ahci;
/// Intel High Definition Audio (VirtualBox's default audio controller).
#[cfg(target_arch = "x86_64")]
pub mod hda;
#[cfg(target_arch = "x86_64")]
pub mod ata;
// NVMe SSDs: polled admin + one I/O queue; lingfs uses only a LingOS GPT partition.
#[cfg(target_arch = "x86_64")]
pub mod nvme;
// `install-disk`: install LingOS from the USB stick onto a SATA/NVMe disk (GPT + ESP).
#[cfg(target_arch = "x86_64")]
pub mod installer;
#[cfg(target_arch = "x86_64")]
pub mod browser;
#[cfg(target_arch = "x86_64")]
pub mod daw;
pub mod clipboard;
#[cfg(target_arch = "x86_64")]
pub mod display;
#[cfg(target_arch = "x86_64")]
pub mod editor;
#[cfg(target_arch = "x86_64")]
pub mod horizon;
// the Qwen3 engine and the llm_* app builtins (both architectures)
pub mod llm;
/// LingDiffusion: Stable Diffusion through a GPU server on the LAN, or on this PC's CPU (ling-sd).
#[cfg(target_arch = "x86_64")]
pub mod diffusion;
/// LingDiffusion as a server: this machine's engine for other machines (WebUI + LingDiffusion API, bound address).
#[cfg(target_arch = "x86_64")]
pub mod diffusion_host;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod messenger;
/// USB gamepad report layouts and decoding (used by the Pi's xHCI driver).
pub mod hidpad;
/// Every gamepad (USB today, Bluetooth HID later): player slots, decoded state, packets to send.
pub mod gamepad;
/// USB pens / pen displays (Wacom Cintiq...) and absolute pointers (QEMU usb-tablet): HID digitizer reports.
pub mod hiddigi;
/// The USB pen as the system pointer (clicks, tap freeze, double-tap snap) + `os_pen_*` and the `pen` command.
pub mod pen;
/// Sound effects and music for programs (WAV samples, synthesised blips).
pub mod sfx;
#[cfg(target_arch = "x86_64")]
pub mod mixer;
/// USB Audio Class playback over the xHCI isochronous endpoints.
#[cfg(target_arch = "x86_64")]
pub mod usbaudio;
/// UAC1/UAC2 descriptor parsing + stream arithmetic (pure, host-tested).
#[cfg(target_arch = "x86_64")]
pub mod usbaudio_desc;
#[cfg(target_arch = "x86_64")]
pub mod wallpaper;
// font8x8 + framebuffer are shared: x86_64 (Multiboot2/DISPI) and aarch64
// (Raspberry Pi VideoCore-mailbox framebuffer, incl. the Pi boot banner).
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod font8x8;
#[cfg(target_arch = "x86_64")]
pub mod flags;
#[cfg(target_arch = "x86_64")]
pub mod font_unicode;
pub mod ime;
#[cfg(target_arch = "x86_64")]
pub mod gallery;

/// Native 3-D (camera, z-buffer, textured meshes, shapes, particles) for compiled games.
pub mod g3d;
/// LingAnime's `art_*` builtins: the ling-paint engine (no_std) bound to compiled Ling.
pub mod artpaint;
/// glTF (.glb) models: skins + skeletal animation for g3d's gltf_* builtins.
pub mod gltf;
/// Menu, app and tray icons drawn from shapes (supersampled).
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod icons;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod framebuffer;
/// The animated boot screen (ROYGBIV ring, stage text, crypto corners).
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod bootscreen;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod cursor;
#[cfg(target_arch = "x86_64")]
pub mod cursor_overlay;
pub mod kbdlayout;
#[cfg(target_arch = "x86_64")]
pub mod keyboard;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod lingfu;
#[cfg(target_arch = "x86_64")]
pub mod locale;
// PNG/SVG decoding: the x86 desktop's Gallery/browsers and the Pi desktop's Files.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod image;
// audio file tags (ID3, FLAC / Vorbis comments, WAV INFO) + cover thumbnails for the music library
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod tags;
// web pictures (image_fetch); os_video_* routes to videoplay
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod netmedia;
// Horizon's video player: YouTube streams found, fetched, demuxed and decoded on LingOS itself
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod videoplay;
// the desktop's player for local files (os_media_*): MKV / MP4 / AVI / GIF, subtitles
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod mediafile;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod mediaplay;
// video frame timing (present-slot targeting, smoothed audio clock, cadence stats) for both players
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod avpace;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod subs;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod youtube;
// YouTube downloads (MP4 / FLAC / WAV) for Horizon's Download buttons
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod ytsave;
// YouTube sign-in: the user's own browser cookies (cookies.txt), sealed per LingOS user
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod ytauth;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod ytcookies;
#[cfg(target_arch = "x86_64")]
pub mod media;
#[cfg(target_arch = "x86_64")]
pub mod pkgman;
#[cfg(target_arch = "x86_64")]
pub mod mouse;
#[cfg(target_arch = "x86_64")]
pub mod net_e1000;
// Intel Wi-Fi 6 AX201 (Tiger Lake CNVi, iwlwifi gen2), staged bring-up.
#[cfg(target_arch = "x86_64")]
pub mod iwlwifi;
// Bluetooth: the AX201's BT half over USB (HCI, Intel firmware, ling-bt host stack), x86 desktop.
#[cfg(target_arch = "x86_64")]
pub mod bluetooth;
// Intel I225/I226 2.5GbE ("igc"; also QEMU's igb/82576 for testing).
#[cfg(target_arch = "x86_64")]
pub mod net_igc;
// x86's NIC as netstack sees it: e1000 if present, else igc.
#[cfg(target_arch = "x86_64")]
pub mod netdev;
// NVIDIA GPUs, stage 1: identify the card, VRAM, PCIe link (read-only).
#[cfg(target_arch = "x86_64")]
pub mod nvgpu;
// NVIDIA VBIOS parser (pure; FWSEC for the GSP compute driver).
#[cfg(target_arch = "x86_64")]
pub mod nvvbios;
// NVIDIA GSP driver, stage 2: run the VBIOS's FWSEC with the FRTS command (WPR2 carve-out).
#[cfg(target_arch = "x86_64")]
pub mod nvfwsec;
// The Specs app / `hwinfo`: CPU, SMBIOS board + DIMMs, PCI, GPU, storage, audio, network, USB, ACPI.
#[cfg(target_arch = "x86_64")]
pub mod hwinfo;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod netstack;
/// PrtSc: the region / full-screen screenshot overlay.
#[cfg(target_arch = "x86_64")]
pub mod screenshot;
/// Screen recording to Videos/screenrec (H.264 .mp4, or MJPEG .avi).
#[cfg(target_arch = "x86_64")]
pub mod screenrec;
#[cfg(target_arch = "x86_64")]
pub mod serial;
pub mod sshd;
pub mod sshcmds;
// the SSH shell's zsh-style line editor + prompt, and ROYGBIV terminal colour
pub mod sshline;
pub mod ansi;
pub mod sftp;
#[cfg(target_arch = "x86_64")]
pub mod term;
#[cfg(target_arch = "x86_64")]
pub mod terminal;
#[cfg(target_arch = "x86_64")]
pub mod theme;
#[cfg(target_arch = "x86_64")]
pub mod ui_scale;
#[cfg(target_arch = "x86_64")]
pub mod vga;
#[cfg(target_arch = "x86_64")]
pub mod wm;
#[cfg(target_arch = "x86_64")]
pub mod wm_liquid;

#[cfg(target_arch = "aarch64")]
pub mod fbcon;
#[cfg(target_arch = "aarch64")]
pub mod uart;
// USB flash drives (bulk-only mass storage over xHCI).
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod usbmsd;
