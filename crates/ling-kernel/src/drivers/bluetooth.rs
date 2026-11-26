//! Bluetooth on the x86 desktop: the Intel AX201's Bluetooth half (USB
//! 8087:0026, behind the xHCI controller) with the ling-bt host stack.
//!
//! ## When the radio runs
//! Never at boot, at the sign-in screen or on the lock screen. The radio is
//! touched only while ALL of these hold ([`session`]):
//! - a user is signed in with an open keyring (`keyring::user()`; logging out
//!   closes it),
//! - the desktop reports the screen unlocked (`os_bt_session`, every frame;
//!   the default is "locked", so a desktop that never reports keeps it off),
//! - that user's Bluetooth switch is on (stored per user, default off).
//!
//! Until then the controller is only recorded at USB enumeration (not even
//! SET_CONFIGURATION). When the session ends (lock, idle lock, sign-out, the
//! switch) [`shutdown`] drops every link, turns page / inquiry / LE scanning
//! off and resets the controller, and forgets the user's keys in memory.
//!
//! ## Start-up
//! 1. `xhci::bt_open` (SET_CONFIGURATION, endpoints).
//! 2. Intel Read Version: the bootloader (0x06) gets its firmware: the file
//!    is named from Read Version / Boot Params, must match a pinned SHA-256
//!    (ling-bt btintel::PINS), and goes over Secure Send a few dozen
//!    fragments per poll (so the desktop keeps drawing); then Intel Reset, the
//!    bootup event and the .ddc configuration. `ibt-19-0-4` (the AX201's) is
//!    embedded; the others are read from `firmware/intel/` on storage.
//! 3. The host stack ([`ling_bt::host::Host`]) initialises the controller
//!    (Secure Connections only) and takes over.
//!
//! ## Per-user data
//! `home/<user>/.bluetooth`: the switch, bonds (link keys + security level +
//! approved input kinds), block list -- sealed with the user's keyring
//! (ChaCha20-Poly1305), so another account or a disk thief cannot read the
//! link keys.

use alloc::borrow::Cow;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::arch::timer::now_ms;
use crate::arch::xhci;
use ling_bt::host::{Host, Output, Prompt, State};
use ling_bt::store::Store;
use ling_bt::{addr_str, btintel, hci, parse_addr, BdAddr};

static FW_SFI: &[u8] = include_bytes!("../../firmware/ibt-19-0-4.sfi");
static FW_DDC: &[u8] = include_bytes!("../../firmware/ibt-19-0-4.ddc");

/// Secure Sends per poll while the firmware goes in.
const FW_CHUNKS_PER_POLL: usize = 48;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    /// No session, switched off, or no controller: the radio is untouched (or was reset).
    Idle,
    Firmware,
    Running,
    Failed,
}

enum FwStage {
    Send,
    WaitResult(u64),
    WaitBoot(u64),
}

struct FwJob {
    name: String,
    data: Cow<'static, [u8]>,
    plan: btintel::Plan,
    idx: usize,
    stage: FwStage,
    ddc: Option<String>,
}

struct Audio {
    addr: BdAddr,
    enc: ling_bt::sbc::Encoder,
    mtu: usize,
    t0_us: u64,
    sent_frames: u64,
    pcm: Vec<i16>,
}

/// A Bluetooth gamepad handed to drivers::gamepad.
struct PadLink {
    addr: BdAddr,
    handle: u32,
    lay: crate::drivers::hidpad::Layout,
    /// Xbox BLE button order: rewrite each report before decoding.
    remap: bool,
    le: bool,
    /// "Xbox layout, Bluetooth LE", "Switch Pro layout"...
    label: &'static str,
}

struct Bt {
    pads: Vec<PadLink>,
    phase: Phase,
    host: Option<Host>,
    user: Option<String>,
    store: Option<Store>,
    fw_ok: bool,
    fw: Option<FwJob>,
    gen: u32,
    /// Event-endpoint bytes, bulk-IN bytes (ACL; events too in the Intel bootloader).
    evt: hci::Stream,
    acl: hci::Stream,
    boot_evt: hci::Stream,
    bootloader: bool,
    vendor: Vec<Vec<u8>>,
    log: VecDeque<String>,
    /// (time, sequence, text, the device it is about)
    notices: VecDeque<(u64, u32, String, Option<BdAddr>)>,
    notice_seq: u32,
    notice_shown: u32,
    kbd_prev: [u8; 8],
    version: String,
    error: String,
    audio: Option<Audio>,
    gate_logged: Option<bool>,
}

static mut BT: Option<Bt> = None;
static mut IN_POLL: bool = false;
/// The desktop's lock state (os_bt_session). Locked until the desktop says otherwise.
static mut SCREEN_LOCKED: bool = true;

fn st() -> &'static mut Bt {
    unsafe {
        let b = &mut *(&raw mut BT);
        b.get_or_insert_with(|| Bt {
            pads: Vec::new(),
            phase: Phase::Idle,
            host: None,
            user: None,
            store: None,
            fw_ok: false,
            fw: None,
            gen: u32::MAX,
            evt: hci::Stream::new(hci::Kind::Event),
            acl: hci::Stream::new(hci::Kind::Acl),
            boot_evt: hci::Stream::new(hci::Kind::Event),
            bootloader: false,
            vendor: Vec::new(),
            log: VecDeque::new(),
            notices: VecDeque::new(),
            notice_seq: 0,
            notice_shown: 0,
            kbd_prev: [0; 8],
            version: String::new(),
            error: String::new(),
            audio: None,
            gate_logged: None,
        })
    }
}

fn log(b: &mut Bt, s: &str) {
    crate::drivers::serial::write_str(&format!("bluetooth: {s}\n"));
    if b.log.len() >= 60 {
        b.log.pop_front();
    }
    b.log.push_back(s.to_string());
}

fn notice(b: &mut Bt, s: String) {
    notice_about(b, s, None);
}

fn notice_about(b: &mut Bt, s: String, addr: Option<BdAddr>) {
    log(b, &s);
    b.notice_seq = b.notice_seq.wrapping_add(1);
    if b.notices.len() >= 12 {
        b.notices.pop_front();
    }
    b.notices.push_back((now_ms(), b.notice_seq, s, addr));
}

/// Called by the desktop every frame (`os_bt_session`).
pub fn set_screen_locked(locked: bool) {
    unsafe { SCREEN_LOCKED = locked };
}

/// The user Bluetooth may run for: signed in, keyring open, screen unlocked.
fn session() -> Option<String> {
    if unsafe { SCREEN_LOCKED } {
        return None;
    }
    crate::keyring::user()
}

// ── per-user store ──────────────────────────────────────────────────────────

fn store_path(user: &str) -> String {
    format!("home/{user}/.bluetooth")
}

fn load_store(user: &str) -> Store {
    let p = store_path(user);
    let Some(size) = crate::fs::lingfs::file_size(&p) else { return Store::default() };
    let mut raw = alloc::vec![0u8; size.clamp(1, 256 * 1024)];
    let Ok(Some(n)) = crate::fs::lingfs::read_file_all(&p, &mut raw) else { return Store::default() };
    match crate::keyring::open("bluetooth", p.as_bytes(), &raw[..n]) {
        Some(text) => Store::parse(&String::from_utf8_lossy(&text)),
        None => Store::default(),
    }
}

fn save_store(b: &mut Bt) {
    let (Some(user), Some(store)) = (b.user.clone(), b.store.as_ref()) else { return };
    let p = store_path(&user);
    let ok = match crate::keyring::seal("bluetooth", p.as_bytes(), store.to_text().as_bytes()) {
        Some(sealed) => crate::fs::lingfs::write_path_any(&p, &sealed).is_ok(),
        None => false,
    };
    if !ok {
        log(b, "could not save the Bluetooth settings (keyring locked or storage not writable)");
    }
}

// ── transport ───────────────────────────────────────────────────────────────

fn collect(b: &mut Bt) {
    xhci::bt_pump();
    let boot = b.bootloader;
    let (evt, acl, boot_evt) = (&mut b.evt, &mut b.acl, &mut b.boot_evt);
    xhci::bt_recv(&mut |kind, d| {
        if kind == 0 {
            evt.push(d);
        } else if boot {
            boot_evt.push(d);
        } else {
            acl.push(d);
        }
    });
}

fn next_event(b: &mut Bt) -> Option<Vec<u8>> {
    b.evt.next().or_else(|| b.boot_evt.next())
}

/// Send a command (EP0, or bulk OUT for the bootloader's Secure Send) and wait
/// for its Command Complete: the return parameters. Used during firmware
/// download only (the host stack is asynchronous).
fn cmd_sync(b: &mut Bt, pkt: &[u8], bulk: bool, timeout_ms: u64) -> Result<Vec<u8>, String> {
    let opcode = hci::command_opcode(pkt).unwrap_or(0);
    let sent = if bulk { xhci::bt_bulk_out(pkt) } else { xhci::bt_cmd(pkt) };
    if !sent {
        return Err(format!("command {opcode:#06x} could not be sent"));
    }
    let deadline = now_ms() + timeout_ms;
    loop {
        collect(b);
        while let Some(e) = next_event(b) {
            match hci::parse_event(&e) {
                Some(hci::Event::CommandComplete { opcode: o, ret, .. }) if o == opcode => return Ok(ret),
                Some(hci::Event::CommandStatus { status, opcode: o, .. }) if o == opcode && status != 0 => return Err(format!("command {opcode:#06x} failed: status {status:#04x}")),
                Some(hci::Event::Vendor(p)) => b.vendor.push(p),
                _ => {},
            }
        }
        if now_ms() > deadline {
            return Err(format!("command {opcode:#06x}: no answer"));
        }
        crate::arch::timer::delay_us(100);
    }
}

// ── firmware ────────────────────────────────────────────────────────────────

fn fw_file(name: &str) -> Option<Cow<'static, [u8]>> {
    match btintel::canonical(name) {
        "ibt-19-0-4.sfi" => return Some(Cow::Borrowed(FW_SFI)),
        "ibt-19-0-4.ddc" => return Some(Cow::Borrowed(FW_DDC)),
        _ => {},
    }
    if crate::vfs::ready() {
        for p in [format!("firmware/intel/{name}"), format!("firmware/{name}"), format!("media/usb0/firmware/intel/{name}"), format!("media/disk1/firmware/intel/{name}")] {
            if let Some(d) = crate::vfs::read(&p) {
                return Some(Cow::Owned(d));
            }
        }
    }
    None
}

fn fail(b: &mut Bt, why: String) {
    b.error = why.clone();
    notice(b, format!("Bluetooth could not start: {why}"));
    b.phase = Phase::Failed;
    b.fw = None;
    b.bootloader = false;
}

/// Open the controller and look at its firmware state.
fn fw_begin(b: &mut Bt) {
    if let Err(e) = xhci::bt_open() {
        return fail(b, String::from(e));
    }
    b.evt.clear();
    b.acl.clear();
    b.boot_evt.clear();
    b.vendor.clear();
    // the bootloader may answer on either endpoint until we know
    b.bootloader = true;
    let ret = match cmd_sync(b, &btintel::read_version_cmd(), false, 2000) {
        Ok(r) => r,
        Err(e) => return fail(b, e),
    };
    if btintel::is_tlv(&ret) {
        let name = btintel::tlv_fw_name(&ret).unwrap_or_else(|| String::from("?"));
        return fail(b, format!("this Intel controller uses the newer TLV firmware format ({name}), not supported yet"));
    }
    let Some(v) = btintel::parse_version(&ret) else { return fail(b, String::from("bad Read Version answer")) };
    b.version = v.describe();
    log(b, &b.version.clone());
    if let Err(e) = v.supported() {
        return fail(b, e);
    }
    if v.fw_variant == btintel::FW_VARIANT_OPERATIONAL {
        b.bootloader = false;
        b.fw_ok = true;
        log(b, "operational firmware already running");
        return start_host(b);
    }
    if v.fw_variant != btintel::FW_VARIANT_BOOTLOADER {
        return fail(b, format!("unexpected firmware variant {:#04x}", v.fw_variant));
    }
    let ret = match cmd_sync(b, &btintel::read_boot_params_cmd(), false, 2000) {
        Ok(r) => r,
        Err(e) => return fail(b, e),
    };
    let Some(p) = btintel::parse_boot_params(&ret) else { return fail(b, String::from("bad Read Boot Params answer")) };
    if p.limited_cce != 0 {
        return fail(b, String::from("the controller is in limited-CCE mode (unsupported)"));
    }
    let (Some(name), Some(ddc)) = (btintel::fw_name(&v, &p, "sfi"), btintel::fw_name(&v, &p, "ddc")) else {
        return fail(b, String::from("no firmware name for this controller"));
    };
    let Some(data) = fw_file(&name) else {
        return fail(b, format!("firmware {name} not found (put the linux-firmware file in firmware/intel/)"));
    };
    if let Err(e) = btintel::verify(&name, &data) {
        return fail(b, e);
    }
    let plan = match btintel::plan_sfi(&data) {
        Ok(p) => p,
        Err(e) => return fail(b, format!("{name}: {e}")),
    };
    log(b, &format!("loading {name} ({} bytes, SHA-256 verified, {} fragments)", data.len(), plan.chunks.len()));
    b.fw = Some(FwJob { name, data, plan, idx: 0, stage: FwStage::Send, ddc: Some(ddc) });
    b.phase = Phase::Firmware;
}

fn fw_step(b: &mut Bt) {
    let Some(mut job) = b.fw.take() else {
        b.phase = Phase::Idle;
        return;
    };
    let now = now_ms();
    match job.stage {
        FwStage::Send => {
            for _ in 0..FW_CHUNKS_PER_POLL {
                if job.idx >= job.plan.chunks.len() {
                    job.stage = FwStage::WaitResult(now);
                    break;
                }
                let cmd = btintel::secure_send_cmd(&job.data, &job.plan.chunks[job.idx]);
                if let Err(e) = cmd_sync(b, &cmd, true, 3000) {
                    return fail(b, format!("{}: fragment {} of {}: {e}", job.name, job.idx, job.plan.chunks.len()));
                }
                job.idx += 1;
            }
        },
        FwStage::WaitResult(t0) => {
            collect(b);
            while let Some(e) = next_event(b) {
                if let Some(hci::Event::Vendor(p)) = hci::parse_event(&e) {
                    b.vendor.push(p);
                }
            }
            let res = b.vendor.iter().find_map(|p| match btintel::parse_vendor(p) {
                Some(btintel::VendorEvt::SecureSendResult { result, .. }) => Some(result),
                _ => None,
            });
            match res {
                Some(0) => {
                    b.vendor.clear();
                    log(b, "firmware accepted; booting it");
                    if !xhci::bt_cmd(&btintel::reset_cmd(job.plan.boot_addr)) {
                        return fail(b, String::from("Intel Reset could not be sent"));
                    }
                    job.stage = FwStage::WaitBoot(now);
                },
                Some(r) => return fail(b, format!("the controller rejected {} (result {r:#04x})", job.name)),
                None if now.saturating_sub(t0) > 5000 => return fail(b, String::from("no firmware download result from the controller")),
                None => {},
            }
        },
        FwStage::WaitBoot(t0) => {
            collect(b);
            while let Some(e) = next_event(b) {
                if let Some(hci::Event::Vendor(p)) = hci::parse_event(&e) {
                    b.vendor.push(p);
                }
            }
            let booted = b.vendor.iter().any(|p| btintel::parse_vendor(p) == Some(btintel::VendorEvt::Bootup));
            if booted {
                b.vendor.clear();
                b.bootloader = false;
                b.fw_ok = true;
                if let Some(ddc) = job.ddc.take() {
                    load_ddc(b, &ddc);
                }
                log(b, "operational firmware running");
                return start_host(b);
            }
            if now.saturating_sub(t0) > 3000 {
                return fail(b, String::from("the controller did not boot its firmware"));
            }
        },
    }
    b.fw = Some(job);
}

fn load_ddc(b: &mut Bt, name: &str) {
    let Some(d) = fw_file(name) else {
        log(b, &format!("{name} not found: default device configuration"));
        return;
    };
    if let Err(e) = btintel::verify(name, &d) {
        log(b, &e);
        return;
    }
    match btintel::ddc_cmds(&d) {
        Ok(cmds) => {
            for c in cmds {
                if let Err(e) = cmd_sync(b, &c, false, 2000) {
                    log(b, &format!("{name}: {e}"));
                    return;
                }
            }
            log(b, &format!("{name} applied"));
        },
        Err(e) => log(b, &format!("{name}: {e}")),
    }
}

fn start_host(b: &mut Bt) {
    let mut rnd = [0u8; 6];
    if !crate::crypto::random_bytes(&mut rnd) {
        let t = crate::arch::timer::now_us().to_le_bytes();
        rnd.copy_from_slice(&t[..6]);
    }
    let mut h = Host::new(b.store.clone().unwrap_or_default());
    let mut seed = [0u8; 32];
    if !crate::crypto::random_bytes(&mut seed) {
        // no RDRAND: the TSC and the clock (weak; LE pairing keys come from here)
        for (i, c) in seed.chunks_mut(8).enumerate() {
            c.copy_from_slice(&(crate::arch::timer::now_us().wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(i as u32 * 13)).to_le_bytes());
        }
    }
    h.seed(&seed);
    h.audio_rate = crate::drivers::mixer::sample_rate();
    h.start(now_ms(), rnd);
    b.host = Some(h);
    b.evt.clear();
    b.acl.clear();
    b.phase = Phase::Running;
    b.error.clear();
    run(b);
}

// ── running ─────────────────────────────────────────────────────────────────

fn run(b: &mut Bt) {
    collect(b);
    let Some(mut h) = b.host.take() else { return };
    h.tick(now_ms());
    while let Some(e) = b.evt.next() {
        h.on_event(&e);
        let o = h.take_output();
        handle(b, &mut h, o);
    }
    while let Some(a) = b.acl.next() {
        h.on_acl(&a);
        let o = h.take_output();
        handle(b, &mut h, o);
    }
    let o = h.take_output();
    handle(b, &mut h, o);
    if h.state == State::Failed && b.phase == Phase::Running {
        b.error = String::from("the controller failed (see `bt log`)");
    }
    b.host = Some(h);
    audio_feed(b);
    pad_out(b);
    if let Some(mut h) = b.host.take() {
        let o = h.take_output();
        handle(b, &mut h, o);
        b.host = Some(h);
    }
}

fn handle(b: &mut Bt, h: &mut Host, outs: Vec<Output>) {
    for o in outs {
        match o {
            Output::Cmd(c) => {
                if !xhci::bt_cmd(&c) {
                    log(b, &format!("command {:#06x} could not be sent", hci::command_opcode(&c).unwrap_or(0)));
                }
            },
            Output::Acl(p) => {
                let _ = xhci::bt_bulk_out(&p);
            },
            Output::Input(a, r) => input(b, h, a, r),
            Output::Notice(s) => notice(b, s),
            Output::DeviceNotice(a, s) => notice_about(b, s, Some(a)),
            Output::StoreChanged => {
                b.store = Some(h.store.clone());
                save_store(b);
            },
            Output::AudioStart(a, cfg, mtu) => {
                b.audio = Some(Audio { addr: a, enc: ling_bt::sbc::Encoder::new(cfg), mtu: mtu as usize, t0_us: crate::arch::timer::now_us(), sent_frames: 0, pcm: Vec::new() });
            },
            Output::AudioStop(a) => {
                if b.audio.as_ref().is_some_and(|x| x.addr == a) {
                    b.audio = None;
                }
            },
            Output::PadAttach { addr, map, vid, pid, name, le } => pad_attach(b, addr, &map, vid, pid, &name, le),
            Output::PadReport { addr, data } => {
                // pads read as disconnected while locked: nothing reaches them without a session
                if session().is_none() {
                    continue;
                }
                if let Some(p) = b.pads.iter().find(|p| p.addr == addr) {
                    let mut r = data;
                    if p.remap {
                        let off = p.lay.buttons_off as usize + if p.lay.report_id != 0 { 8 } else { 0 };
                        ling_bt::gatt::xbox_ble_remap(&mut r, off);
                    }
                    crate::drivers::gamepad::report(p.handle, &r);
                }
            },
            Output::PadDetach(addr) => pad_detach(b, &addr),
        }
    }
}

/// A gamepad connected: its personality (PnP / Device ID first, then the name
/// it paired with: "Xbox Wireless Controller", "Pro Controller"...), the
/// layout drivers::hidpad picks for it, a slot in drivers::gamepad.
fn pad_attach(b: &mut Bt, addr: BdAddr, map: &[u8], vid: u16, pid: u16, name: &str, le: bool) {
    use ling_bt::gatt::{personality, xbox_fix_map, Personality};
    pad_detach(b, &addr);
    let (p, vid, pid) = personality(vid, pid, name);
    let map = if p == Personality::Xbox { xbox_fix_map(map) } else { map.to_vec() };
    let Some(mut lay) = crate::drivers::hidpad::identify(vid, pid, 3, 0, 0, &map) else {
        notice(b, format!("{name}: its report descriptor has no gamepad layout this computer can read"));
        return;
    };
    let remap = p == Personality::Xbox && lay.kind == crate::drivers::hidpad::PadKind::Generic && lay.n_buttons >= 15;
    if remap {
        lay.n_buttons = 11;
    }
    // an Xbox personality (BLE or classic One S): rumble through output report 3
    if p == Personality::Xbox && lay.kind == crate::drivers::hidpad::PadKind::Generic {
        lay.kind = crate::drivers::hidpad::PadKind::XboxBle;
    }
    let label = p.label(le);
    match crate::drivers::gamepad::attach(lay, vid, pid) {
        Some(handle) => {
            log(b, &format!("gamepad {name} ({vid:04x}:{pid:04x}): {label}"));
            b.pads.push(PadLink { addr, handle, lay, remap, le, label });
        },
        None => notice(b, String::from("too many gamepads connected")),
    }
}

fn pad_detach(b: &mut Bt, addr: &BdAddr) {
    if let Some(i) = b.pads.iter().position(|p| &p.addr == addr) {
        let p = b.pads.remove(i);
        crate::drivers::gamepad::detach(p.handle);
    }
}

/// Packets drivers::gamepad wants sent (the Switch subcommands, LEDs, rumble):
/// output reports on a classic pad's interrupt channel.
fn pad_out(b: &mut Bt) {
    let Some(h) = b.host.as_mut() else { return };
    let now = now_ms();
    for p in b.pads.iter() {
        for _ in 0..4 {
            let Some(pkt) = crate::drivers::gamepad::next_out(p.handle, now) else { break };
            let bytes = pkt.bytes();
            // the Switch's USB-only handshake (0x80 ...) means nothing over Bluetooth
            if bytes.first() == Some(&0x80) {
                continue;
            }
            h.pad_output(&p.addr, bytes);
        }
    }
}

/// The personality label of a paired pad ("Switch Pro layout"...), for the lists.
fn pad_label(b: &Bt, d: &ling_bt::store::Bond) -> Option<String> {
    if let Some(p) = b.pads.iter().find(|p| p.addr == d.addr) {
        return Some(String::from(p.label));
    }
    d.gamepad.then(|| String::from(ling_bt::gatt::personality(d.vid, d.pid, &d.name).0.label(d.le)))
}

fn input(b: &mut Bt, _h: &Host, _a: BdAddr, r: ling_bt::hidp::Report) {
    // belt and braces: never a key while locked or signed out
    if session().is_none() {
        return;
    }
    match r {
        ling_bt::hidp::Report::Keyboard(cur) => {
            xhci::bt_key_report(&b.kbd_prev, &cur);
            b.kbd_prev = cur;
        },
        ling_bt::hidp::Report::Mouse { buttons, dx, dy, wheel } => xhci::bt_mouse_report(buttons, dx, dy, wheel),
    }
}

/// Everything off: links, scans, the controller reset; keys forgotten if the session ended.
fn shutdown(b: &mut Bt, why: &str) {
    if let Some(mut h) = b.host.take() {
        for c in h.stop() {
            let _ = xhci::bt_cmd(&c);
        }
        log(b, &format!("radio off ({why})"));
    }
    if b.kbd_prev != [0; 8] {
        xhci::bt_key_report(&b.kbd_prev, &[0; 8]);
        b.kbd_prev = [0; 8];
    }
    b.audio = None;
    b.fw = None;
    let addrs: Vec<BdAddr> = b.pads.iter().map(|p| p.addr).collect();
    for a in addrs {
        pad_detach(b, &a);
    }
    if b.phase != Phase::Failed {
        b.phase = Phase::Idle;
    }
}

/// The scheduler's hook (from `xhci::poll`). Cheap when nothing is on.
pub fn poll() {
    unsafe {
        if IN_POLL {
            return;
        }
        IN_POLL = true;
    }
    poll_inner();
    unsafe { IN_POLL = false };
}

fn poll_inner() {
    let b = st();
    let present = xhci::bt_info();
    let gen = xhci::bt_generation();
    if gen != b.gen {
        b.gen = gen;
        if b.host.is_some() || b.phase != Phase::Idle {
            b.host = None;
            b.audio = None;
            b.fw = None;
            b.phase = Phase::Idle;
        }
        b.fw_ok = false;
        if let Some(i) = present.as_ref() {
            log(b, &format!("controller {:04x}:{:04x} on {} port {} (off until a user signs in and turns Bluetooth on)", i.vid, i.pid, String::from_utf8_lossy(i.controller), i.port));
        }
    }
    let user = session();
    if user != b.user {
        if b.user.is_some() {
            shutdown(b, if user.is_none() { "screen locked or signed out" } else { "user changed" });
            if b.phase == Phase::Failed {
                b.phase = Phase::Idle;
            }
        }
        b.store = user.as_deref().map(load_store);
        b.user = user.clone();
    }
    if b.gate_logged != Some(user.is_some()) {
        b.gate_logged = Some(user.is_some());
        log(b, if user.is_some() { "session open: the radio may start (per-user switch)" } else { "radio off until a user signs in (no HCI traffic)" });
    }
    if b.user.is_none() {
        return;
    }
    let want_on = b.store.as_ref().is_some_and(|s| s.power) && present.is_some();
    match b.phase {
        Phase::Idle if want_on => {
            if b.fw_ok {
                if xhci::bt_open().is_ok() {
                    start_host(b);
                }
            } else {
                fw_begin(b);
            }
        },
        Phase::Firmware if want_on => fw_step(b),
        Phase::Running if want_on => run(b),
        Phase::Firmware | Phase::Running => shutdown(b, "switched off"),
        _ => {},
    }
}

// ── A2DP: the mixer feeds the headset ───────────────────────────────────────

/// Bluetooth headphones are streaming: the mixer's active output.
pub fn audio_active() -> bool {
    unsafe { (*(&raw const BT)).as_ref().is_some_and(|b| b.audio.is_some()) }
}

/// The mixer's pump while a headset streams: render what is due (about 60 ms
/// ahead of the wall clock), encode SBC, send it.
pub fn audio_pump(render: &mut dyn FnMut(&mut [i16])) {
    let b = st();
    let Some(a) = b.audio.as_mut() else { return };
    let rate = a.enc.cfg.rate as u64;
    let now = crate::arch::timer::now_us();
    let due = (now.saturating_sub(a.t0_us) * rate / 1_000_000) + rate * 60 / 1000;
    let have = a.sent_frames + (a.pcm.len() / 2) as u64;
    if due > have {
        let n = ((due - have) as usize).min(rate as usize / 5);
        let mut buf = alloc::vec![0i16; n * 2];
        render(&mut buf);
        a.pcm.extend_from_slice(&buf);
    }
}

fn audio_feed(b: &mut Bt) {
    let Some(mut h) = b.host.take() else { return };
    if let Some(a) = b.audio.as_mut() {
        let spf = a.enc.cfg.samples();
        let flen = a.enc.cfg.frame_len();
        let per_packet = (a.mtu.saturating_sub(13) / flen).clamp(1, 15);
        while a.pcm.len() >= spf * 2 * per_packet {
            let mut frames = Vec::with_capacity(flen * per_packet);
            let ts = a.sent_frames as u32;
            for i in 0..per_packet {
                frames.extend(a.enc.encode(&a.pcm[i * spf * 2..(i + 1) * spf * 2]));
            }
            if !h.send_media(&a.addr, &frames, per_packet as u8, ts) {
                break; // the controller is backed up: keep the PCM for the next poll
            }
            a.pcm.drain(..spf * 2 * per_packet);
            a.sent_frames += (spf * per_packet) as u64;
        }
        let rate = a.enc.cfg.rate as usize;
        if a.pcm.len() > rate * 2 {
            // a second behind: drop the oldest rather than drift further
            let n = a.pcm.len() - rate;
            a.pcm.drain(..n & !1);
        }
    }
    let outs = h.take_output();
    handle(b, &mut h, outs);
    b.host = Some(h);
}

// ── the `bt` command and the Settings builtins ─────────────────────────────

fn with_host<R>(f: impl FnOnce(&mut Bt, &mut Host) -> R) -> Result<R, String> {
    let b = st();
    if b.user.is_none() {
        return Err(String::from("Bluetooth works only while you are signed in with the screen unlocked"));
    }
    let Some(mut h) = b.host.take() else {
        if xhci::bt_info().is_none() {
            return Err(String::from("no Bluetooth controller found on USB"));
        }
        return Err(String::from(if b.store.as_ref().is_some_and(|s| s.power) { "Bluetooth is starting..." } else { "Bluetooth is off (`bt on`)" }));
    };
    if h.state != State::Ready {
        b.host = Some(h);
        return Err(String::from("Bluetooth is starting..."));
    }
    let r = f(b, &mut h);
    let o = h.take_output();
    handle(b, &mut h, o);
    b.host = Some(h);
    Ok(r)
}

/// Change the store whether or not the radio runs (block lists work while off).
fn with_store<R>(f: impl FnOnce(&mut Store) -> R) -> Result<R, String> {
    let b = st();
    if b.user.is_none() {
        return Err(String::from("sign in (and unlock) first"));
    }
    if let Some(h) = b.host.as_mut() {
        let r = f(&mut h.store);
        b.store = Some(h.store.clone());
        save_store(b);
        return Ok(r);
    }
    let s = b.store.get_or_insert_with(Store::default);
    let r = f(s);
    save_store(b);
    Ok(r)
}

pub fn set_power(on: bool) -> Result<String, String> {
    with_store(|s| s.power = on)?;
    let b = st();
    if !on {
        shutdown(b, "switched off");
        if b.phase == Phase::Failed {
            b.phase = Phase::Idle;
        }
        return Ok(String::from("Bluetooth off"));
    }
    if b.phase == Phase::Failed {
        b.phase = Phase::Idle;
    }
    if xhci::bt_info().is_none() {
        return Ok(String::from("Bluetooth on (no Bluetooth controller found on USB)"));
    }
    Ok(String::from("Bluetooth on: starting"))
}

fn addr_arg(a: &str) -> Result<BdAddr, String> {
    parse_addr(a).ok_or_else(|| format!("not a Bluetooth address: {a} (AA:BB:CC:DD:EE:FF)"))
}

fn kind(s: &ling_bt::host::Seen) -> String {
    let base = if s.le { hci::appearance_kind(s.appearance) } else { hci::cod_kind(s.cod) };
    // what a multi-mode pad will most likely present itself as
    let (p, ..) = ling_bt::gatt::personality(0, 0, s.name.as_deref().unwrap_or(""));
    if p != ling_bt::gatt::Personality::Generic {
        return format!("gamepad ({})", p.label(s.le));
    }
    String::from(base)
}

/// Status line for Settings / `bt`.
pub fn status() -> String {
    let b = st();
    let ctl = match xhci::bt_info() {
        Some(i) => format!("{:04x}:{:04x}", i.vid, i.pid),
        None => String::from("none"),
    };
    if b.user.is_none() {
        return format!("off -- signed out or locked (controller: {ctl})");
    }
    let power = b.store.as_ref().is_some_and(|s| s.power);
    let state = match (b.phase, power) {
        (_, false) => String::from("off"),
        (Phase::Failed, _) => format!("failed: {}", b.error),
        (Phase::Firmware, _) => {
            let pct = b.fw.as_ref().map(|j| j.idx * 100 / j.plan.chunks.len().max(1)).unwrap_or(0);
            format!("loading firmware {pct}%")
        },
        (Phase::Running, _) => match b.host.as_ref().map(|h| h.state) {
            Some(State::Ready) => {
                let h = b.host.as_ref().unwrap();
                let mut s = format!("on, {}", addr_str(&h.local_addr));
                if h.scanning {
                    s.push_str(", scanning");
                }
                if h.pairable_until > now_ms() {
                    s.push_str(&format!(", discoverable for {} s", (h.pairable_until - now_ms()) / 1000));
                }
                s
            },
            Some(State::Failed) => String::from("failed (see `bt log`)"),
            _ => String::from("starting"),
        },
        (Phase::Idle, true) if xhci::bt_info().is_none() => String::from("on, but no controller found"),
        (Phase::Idle, true) => String::from("starting"),
    };
    format!("{state} (controller: {ctl})")
}

/// Scan results, blocked ones included (the desktop hides them unless "show
/// blocked" is on): "addr\tname\tkind\trssi\tflags" (flags: b blocked, p paired, l LE).
pub fn devices() -> Vec<String> {
    let b = st();
    let Some(h) = b.host.as_ref() else { return Vec::new() };
    h.seen
        .iter()
        .map(|s| {
            let mut f = String::new();
            if s.blocked {
                f.push('b');
            }
            if h.store.bond(&s.addr).is_some() {
                f.push('p');
            }
            if s.le {
                f.push('l');
            }
            format!("{}\t{}\t{}\t{}\t{}", addr_str(&s.addr), s.name.clone().unwrap_or_default(), kind(s), s.rssi.map(|r| r as i32).unwrap_or(0), f)
        })
        .collect()
}

/// Paired devices: "addr\tname\tkind\tsecurity\tinput\tstate".
pub fn paired() -> Vec<String> {
    let b = st();
    let Some(store) = b.host.as_ref().map(|h| &h.store).or(b.store.as_ref()) else { return Vec::new() };
    store
        .bonds
        .iter()
        .map(|d| {
            let mut inp = String::new();
            if d.allow_kbd {
                inp.push_str("keys ");
            }
            if d.allow_ptr {
                inp.push_str("pointer ");
            }
            if d.audio {
                inp.push_str("audio");
            }
            if d.gamepad {
                inp.push_str("gamepad");
            }
            let state = b.host.as_ref().and_then(|h| h.conn_state(&d.addr)).unwrap_or("not connected");
            let kind = match pad_label(b, d) {
                Some(l) => format!("gamepad ({l})"),
                None if d.le => String::from("LE device"),
                None => String::from(hci::cod_kind(d.cod)),
            };
            format!("{}\t{}\t{}\t{}{}\t{}\t{}", addr_str(&d.addr), d.name, kind, d.level(), if d.le { ", LE" } else { "" }, inp.trim(), state)
        })
        .collect()
}

/// Blocked: "addr\tname" rows, then "*\tpattern" rows for name patterns.
pub fn blocked() -> Vec<String> {
    let b = st();
    let Some(store) = b.host.as_ref().map(|h| &h.store).or(b.store.as_ref()) else { return Vec::new() };
    let mut v: Vec<String> = store.blocked.iter().map(|x| format!("{}\t{}", addr_str(&x.addr), x.name)).collect();
    v.extend(store.blocked_names.iter().map(|p| format!("*\t{p}")));
    v
}

/// The pairing question for the user, if any: "confirm\t<addr>\t<name>\t<number>" or "passkey\t...".
pub fn prompt() -> String {
    let b = st();
    let Some(h) = b.host.as_ref() else { return String::new() };
    let name = |a: &BdAddr| h.store.bond(a).map(|x| x.name.clone()).filter(|n| !n.is_empty()).or_else(|| h.seen.iter().find(|s| &s.addr == a).and_then(|s| s.name.clone())).unwrap_or_else(|| addr_str(a));
    match &h.prompt {
        Some(Prompt::Confirm { addr, value, .. }) => format!("confirm\t{}\t{}\t{value:06}", addr_str(addr), name(addr)),
        Some(Prompt::Passkey { addr, passkey }) => format!("passkey\t{}\t{}\t{passkey:06}", addr_str(addr), name(addr)),
        None => String::new(),
    }
}

/// The next notice not shown yet (the desktop's banner): "text" or
/// "text	addr" when it is about one device; "" when none.
pub fn take_notice() -> String {
    let b = st();
    let shown = b.notice_shown;
    match b.notices.iter().find(|n| n.1.wrapping_sub(shown) as i32 > 0) {
        Some((_, seq, s, a)) => {
            let out = match a {
                Some(a) => format!("{s}	{}", addr_str(a)),
                None => s.clone(),
            };
            b.notice_shown = *seq;
            out
        },
        None => String::new(),
    }
}

pub fn show_blocked() -> bool {
    let b = st();
    b.host.as_ref().map(|h| h.store.show_blocked).or(b.store.as_ref().map(|s| s.show_blocked)).unwrap_or(false)
}

/// Settings / desktop actions: op + argument. Returns a message ("!" first = error).
pub fn action(op: &str, arg: &str) -> String {
    let r: Result<String, String> = (|| match op {
        "on" => set_power(true),
        "off" => set_power(false),
        "scan" => with_host(|_, h| h.scan(arg.parse().unwrap_or(10)))?.map(|_| String::from("scanning...")),
        "pairable" => with_host(|_, h| h.set_pairable(arg != "off")).map(|_| String::from(if arg == "off" { "not discoverable" } else { "discoverable for 2 minutes" })),
        "pair" | "connect" => {
            let a = addr_arg(arg)?;
            with_host(|_, h| h.pair(&a))?.map(|_| format!("connecting to {arg}..."))
        },
        "disconnect" => {
            let a = addr_arg(arg)?;
            with_host(|_, h| h.disconnect(&a)).map(|ok| String::from(if ok { "disconnecting" } else { "not connected" }))
        },
        "remove" | "unpair" | "forget" => {
            let a = addr_arg(arg)?;
            if let Ok(ok) = with_host(|_, h| h.remove(&a)) {
                return Ok(String::from(if ok { "removed (keys deleted)" } else { "not paired" }));
            }
            with_store(|s| s.remove_bond(&a)).map(|ok| String::from(if ok { "removed (keys deleted)" } else { "not paired" }))
        },
        "block" => {
            let a = addr_arg(arg)?;
            if let Ok(r) = with_host(|_, h| h.block(&a)) {
                return r.map(|n| format!("{} {arg}", if n { "blocked" } else { "already blocked:" }));
            }
            with_store(|s| s.block(&a, ""))?.map(|n| format!("{} {arg}", if n { "blocked" } else { "already blocked:" }))
        },
        "unblock" => {
            let a = addr_arg(arg)?;
            if let Ok(ok) = with_host(|_, h| h.unblock(&a)) {
                return Ok(format!("{} {arg}", if ok { "unblocked" } else { "not blocked:" }));
            }
            with_store(|s| s.unblock(&a)).map(|ok| format!("{} {arg}", if ok { "unblocked" } else { "not blocked:" }))
        },
        "block-name" => {
            if let Ok(ok) = with_host(|_, h| h.block_name(arg)) {
                return Ok(format!("{} \"{arg}\" (names are not authenticated: a device can change its name)", if ok { "blocking names containing" } else { "already blocked:" }));
            }
            with_store(|s| s.block_name(arg)).map(|ok| format!("{} \"{arg}\"", if ok { "blocking names containing" } else { "already blocked:" }))
        },
        "unblock-name" => {
            if let Ok(ok) = with_host(|_, h| h.unblock_name(arg)) {
                return Ok(format!("{} \"{arg}\"", if ok { "unblocked names containing" } else { "no such pattern:" }));
            }
            with_store(|s| s.unblock_name(arg)).map(|ok| format!("{} \"{arg}\"", if ok { "unblocked names containing" } else { "no such pattern:" }))
        },
        "show-blocked" => with_store(|s| s.show_blocked = arg != "off").map(|_| String::from("ok")),
        "confirm" => with_host(|_, h| h.confirm(matches!(arg, "yes" | "y" | "1" | "ok"))).map(|ok| String::from(if ok { "answered" } else { "nothing to confirm" })),
        "allow-keys" | "allow-pointer" => {
            let a = addr_arg(arg)?;
            let kbd = op == "allow-keys";
            with_host(|_, h| h.allow_input(&a, kbd, !kbd))?.map(|_| String::from("allowed"))
        },
        "rumble" => {
            let mut it = arg.split_whitespace();
            let a = addr_arg(it.next().unwrap_or(""))?;
            let s: f32 = it.next().and_then(|x| x.parse().ok()).unwrap_or(1.0);
            let w: f32 = it.next().and_then(|x| x.parse().ok()).unwrap_or(s);
            let ms: u32 = it.next().and_then(|x| x.parse().ok()).unwrap_or(300);
            with_host(|_, h| h.pad_rumble(&a, s, w, ms))?.map(|_| String::from("rumble sent"))
        },
        "audio" => {
            let a = addr_arg(arg)?;
            with_host(|_, h| h.audio_connect(&a))?.map(|_| String::from("connecting audio..."))
        },
        _ => Err(format!("unknown Bluetooth action {op}")),
    })();
    match r {
        Ok(s) => s,
        Err(e) => format!("!{e}"),
    }
}

/// `bt [status | on | off | scan [secs] | list | paired | pair <addr> | connect <addr> |
///  disconnect <addr> | remove <addr> | confirm yes|no | pairable [off] | block <addr> |
///  unblock <addr> | block-name <text> | unblock-name <text> | blocked | allow-keys <addr> |
///  allow-pointer <addr> | audio <addr> | log]`
pub fn command(arg: &str) -> String {
    let arg = arg.trim();
    let (op, rest) = arg.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((arg, ""));
    match op {
        "" | "status" => {
            let mut s = format!("bluetooth: {}\n", status());
            let b = st();
            if !b.version.is_empty() {
                s.push_str(&format!("  {}\n", b.version));
            }
            let p = prompt();
            if !p.is_empty() {
                let f: Vec<&str> = p.split('\t').collect();
                s.push_str(&format!("  pairing {}: {} {}\n", f.get(2).unwrap_or(&""), if f[0] == "confirm" { "does it show" } else { "type on it, then Enter:" }, f.get(3).unwrap_or(&"")));
            }
            for l in paired() {
                let f: Vec<&str> = l.split('\t').collect();
                s.push_str(&format!("  paired {} {:20} {:16} [{}] {} {}\n", f[0], f[1], f[2], f[3], f[4], f[5]));
            }
            s
        },
        "list" | "devices" => {
            let mut s = String::new();
            for l in devices() {
                let f: Vec<&str> = l.split('\t').collect();
                s.push_str(&format!("{} {:24} {:16} {:>4} dBm {}\n", f[0], f[1], f[2], f[3], f[4]));
            }
            if s.is_empty() {
                s.push_str("no devices (bt scan)\n");
            }
            s
        },
        "paired" => paired().iter().map(|l| l.replace('\t', "  ") + "\n").collect::<String>(),
        "blocked" => {
            let v = blocked();
            if v.is_empty() {
                return String::from("no blocked devices\n");
            }
            let mut s: String = v.iter().map(|l| l.replace('\t', "  ") + "\n").collect();
            s.push_str("(a Bluetooth LE device that changes its address is recognised by its identity key once it has been paired: block it after pairing, or by name -- weaker)\n");
            s
        },
        "log" => {
            let b = st();
            let mut s: String = b.log.iter().map(|l| format!("{l}\n")).collect();
            if let Some(i) = xhci::bt_info() {
                s.push_str(&format!("(receive ring overflows: {})\n", i.dropped));
            }
            s
        },
        "selftest" => selftest(),
        "help" => String::from(
            "bt [status] | on | off | scan [secs] | list | paired | pair <addr> | connect <addr> | disconnect <addr>\n\
             remove <addr> | confirm yes|no | pairable [off] | block <addr> | unblock <addr> | blocked\n\
             block-name <text> | unblock-name <text> | allow-keys <addr> | allow-pointer <addr> | audio <addr>\n\
             rumble <addr> [strong 0-1] [weak 0-1] [ms] | log | selftest\n",
        ),
        "scan" => {
            let r = action("scan", if rest.is_empty() { "10" } else { rest });
            if r.starts_with('!') {
                return format!("bt: {}\n", &r[1..]);
            }
            format!("{r} (bt list in a few seconds)\n")
        },
        _ => {
            let r = action(op, rest);
            match r.strip_prefix('!') {
                Some(e) => format!("bt: {e}\n"),
                None => format!("{r}\n"),
            }
        },
    }
}

/// `bt selftest`: the stack inside this kernel against a simulated controller
/// (QEMU has no Bluetooth): firmware pin + plan, init, the connection policy,
/// SBC encode / decode, the keyring seal of the store. Touches no radio.
fn selftest() -> String {
    let mut out = String::new();
    let mut fails = 0;
    let mut check = |out: &mut String, name: &str, ok: bool, detail: String| {
        if !ok {
            fails += 1;
        }
        out.push_str(&format!("selftest: {name}: {} {detail}\n", if ok { "ok" } else { "FAIL" }));
    };
    // 1. the embedded AX201 firmware: pinned hash, a download plan
    let t0 = crate::arch::timer::now_us();
    let pin = btintel::verify("ibt-19-0-4.sfi", FW_SFI);
    let dt = crate::arch::timer::now_us().saturating_sub(t0);
    check(&mut out, "firmware SHA-256 pin", pin.is_ok(), format!("({} bytes, {} us)", FW_SFI.len(), dt));
    let mut bad = FW_SFI[..4096].to_vec();
    bad[700] ^= 1;
    check(&mut out, "tampered firmware refused", btintel::verify("ibt-19-0-4.sfi", &bad).is_err(), String::new());
    match btintel::plan_sfi(FW_SFI) {
        Ok(p) => check(&mut out, "firmware plan", p.boot_addr != 0, format!("({} secure sends, boot address {:#x})", p.chunks.len(), p.boot_addr)),
        Err(e) => check(&mut out, "firmware plan", false, e),
    }
    check(&mut out, "ddc", btintel::verify("ibt-19-0-4.ddc", FW_DDC).is_ok() && btintel::ddc_cmds(FW_DDC).is_ok_and(|c| c.len() == 2), String::new());
    // 2. the host against a simulated controller: every command answered "success"
    let mut h = Host::new(Store::default());
    h.start(0, [7; 6]);
    let mut sent = Vec::new();
    for _ in 0..64 {
        let outs = h.take_output();
        let Some(c) = outs.iter().rev().find_map(|o| if let Output::Cmd(c) = o { Some(c.clone()) } else { None }) else { break };
        let op = hci::command_opcode(&c).unwrap_or(0);
        sent.push(op);
        let mut p = alloc::vec![1u8];
        p.extend_from_slice(&op.to_le_bytes());
        p.extend_from_slice(&[0, 1, 2, 3, 4, 5, 6, 8, 0]);
        let mut e = alloc::vec![0x0E, p.len() as u8];
        e.extend(p);
        h.on_event(&e);
    }
    check(&mut out, "controller init", h.state == State::Ready && sent.first() == Some(&hci::op::RESET) && sent.contains(&hci::op::WRITE_SC_HOST_SUPPORT), format!("({} commands)", sent.len()));
    let _ = h.take_output();
    let unknown = [0x66, 0x55, 0x44, 0x33, 0x22, 0x11];
    let mut req = alloc::vec![0x04, 10];
    req.extend_from_slice(&unknown);
    req.extend_from_slice(&[0x40, 0x25, 0x00, 1]);
    h.on_event(&req);
    let rejected = h.take_output().iter().any(|o| matches!(o, Output::Cmd(c) if hci::command_opcode(c) == Some(hci::op::REJECT_CONNECTION)));
    check(&mut out, "unknown device refused", rejected, String::new());
    let stop = h.stop();
    check(&mut out, "stop resets the radio", stop.last().and_then(|c| hci::command_opcode(c)) == Some(hci::op::RESET), String::new());
    // 2b. LE Secure Connections in this kernel: the spec's f4 vector, P-256 ECDH both ways, an RPA
    {
        use ling_bt::smp;
        let u = [0xe6, 0x9d, 0x35, 0x0e, 0x48, 0x01, 0x03, 0xcc, 0xdb, 0xfd, 0xf4, 0xac, 0x11, 0x91, 0xf4, 0xef, 0xb9, 0xa5, 0xf9, 0xe9, 0xa7, 0x83, 0x2c, 0x5e, 0x2c, 0xbe, 0x97, 0xf2, 0xd2, 0x03, 0xb0, 0x20];
        let v = [0xfd, 0xc5, 0x7f, 0xf4, 0x49, 0xdd, 0x4f, 0x6b, 0xfb, 0x7c, 0x9d, 0xf1, 0xc2, 0x9a, 0xcb, 0x59, 0x2a, 0xe7, 0xd4, 0xee, 0xfb, 0xfc, 0x0a, 0x90, 0x9a, 0xbb, 0xf6, 0x32, 0x3d, 0x8b, 0x18, 0x55];
        let x = [0xab, 0xae, 0x2b, 0x71, 0xec, 0xb2, 0xff, 0xff, 0x3e, 0x73, 0x77, 0xd1, 0x54, 0x84, 0xcb, 0xd5];
        let exp = [0x2d, 0x87, 0x74, 0xa9, 0xbe, 0xa1, 0xed, 0xf1, 0x1c, 0xbd, 0xa9, 0x07, 0xf1, 0x16, 0xc9, 0xf2];
        check(&mut out, "SMP f4 (Core spec vector)", smp::f4(&u, &v, &x, 0) == exp, String::new());
        let t0 = crate::arch::timer::now_us();
        let a = smp::keypair(&[7u8; 32]);
        let b2 = smp::keypair(&[9u8; 32]);
        let agree = match (a, b2) {
            (Some((ask, apk)), Some((bsk, bpk))) => smp::dhkey(&ask, &bpk).is_some() && smp::dhkey(&ask, &bpk) == smp::dhkey(&bsk, &apk),
            _ => false,
        };
        let dt = crate::arch::timer::now_us().saturating_sub(t0);
        check(&mut out, "P-256 ECDH agreement", agree, format!("(2 key pairs + 2 DH, {dt} us)"));
        let irk = [0x9b, 0x7d, 0x39, 0x0a, 0xa6, 0x10, 0x10, 0x34, 0x05, 0xad, 0xc8, 0x57, 0xa3, 0x34, 0x02, 0xec];
        check(&mut out, "RPA resolves with its IRK", smp::rpa_matches(&irk, &[0xaa, 0xfb, 0x0d, 0x94, 0x81, 0x70]), String::new());
        // the personalities a multi-mode pad (AceGamer Aurora...) can take
        let (p, vid, pid) = ling_bt::gatt::personality(0, 0, "Pro Controller");
        let sw = crate::drivers::hidpad::identify(vid, pid, 3, 0, 0, &[]).map(|l| l.kind) == Some(crate::drivers::hidpad::PadKind::SwitchPro);
        check(&mut out, "\"Pro Controller\" -> Switch Pro layout", sw && p == ling_bt::gatt::Personality::SwitchPro, String::new());
        let (p, ..) = ling_bt::gatt::personality(0, 0, "Xbox Wireless Controller");
        check(&mut out, "\"Xbox Wireless Controller\" -> Xbox layout", p == ling_bt::gatt::Personality::Xbox, String::new());
    }
    // 3. SBC: a 1 kHz tone through the encoder and back
    let cfg = ling_bt::sbc::Config::default();
    let mut enc = ling_bt::sbc::Encoder::new(cfg);
    let mut st = [[0f32; 160]; 2];
    let n = cfg.samples();
    let mut ok = true;
    let t0 = crate::arch::timer::now_us();
    for f in 0..20 {
        let pcm: Vec<i16> = (0..n * 2).map(|i| (libm::sinf(((f * n + i / 2) as f32) * 2.0 * core::f32::consts::PI * 1000.0 / 48000.0) * 8000.0) as i16).collect();
        let fr = enc.encode(&pcm);
        ok &= fr.len() == cfg.frame_len() && ling_bt::sbc::decode(&fr, &mut st).is_some();
    }
    let dt = crate::arch::timer::now_us().saturating_sub(t0);
    check(&mut out, "SBC encode / decode", ok, format!("(20 frames of {} bytes, {} us)", cfg.frame_len(), dt));
    // 4. the per-user store, sealed with the keyring
    match crate::keyring::user() {
        Some(u) => {
            let mut s = Store::default();
            s.power = true;
            let _ = s.block(&unknown, "selftest");
            let aad = format!("home/{u}/.bluetooth-selftest");
            let sealed = crate::keyring::seal("bluetooth", aad.as_bytes(), s.to_text().as_bytes());
            let back = sealed.as_ref().and_then(|b| crate::keyring::open("bluetooth", aad.as_bytes(), b)).map(|t| Store::parse(&String::from_utf8_lossy(&t)));
            check(&mut out, "store sealed with the keyring", back.as_ref() == Some(&s) && sealed.is_some_and(|b| !b.windows(8).any(|w| w == b"selftest")), String::new());
        },
        None => out.push_str("selftest: store seal: skipped (no keyring open: sign in first)\n"),
    }
    out.push_str(if fails == 0 { "selftest: PASS\n" } else { "selftest: FAILED\n" });
    out
}
