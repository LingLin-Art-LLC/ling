//! The AceGamer Aurora's two PC Bluetooth personalities (and pads like it),
//! through the kernel's real gamepad decoders (drivers/hidpad.rs, included
//! here as it is pure code):
//! - X-input mode, "Xbox Wireless Controller": the Xbox BLE HOGP report map --
//!   16-bit sticks, 10-bit Simulation-page triggers, a 4-bit hat, 15 buttons
//!   in the Xbox BLE order -- fixed up by gatt::xbox_fix_map / xbox_ble_remap;
//! - Switch Pro mode, "Pro Controller" 057E:2009: the simple 0x3F report and
//!   the full 0x30 report.

#[allow(dead_code, clippy::all)]
#[path = "../../ling-kernel/src/drivers/hidpad.rs"]
mod hidpad;
#[allow(dead_code, clippy::all)]
#[path = "../../ling-kernel/src/drivers/gamepad.rs"]
mod gamepad;

use crate::gatt::{self, Personality};
use alloc::vec;
use alloc::vec::Vec;

/// The Xbox Wireless Controller's BLE report map, input part (report 1), as
/// in Linux hid-microsoft's notes / xpadneo dumps: X Y and Z Rz in Pointer
/// collections (0..65535), Brake and Accelerator (0..1023, 10 bits + 6 pad),
/// the hat (1..8, null 0), 15 buttons + 1 pad, the Share (Record) bit; then
/// the rumble output report 3.
pub const XBOX_BLE_MAP: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x01, //
    0x09, 0x01, 0xA1, 0x00, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x27, 0xFF, 0xFF, 0x00, 0x00, 0x95, 0x02, 0x75, 0x10, 0x81, 0x02, 0xC0, //
    0x09, 0x01, 0xA1, 0x00, 0x09, 0x32, 0x09, 0x35, 0x15, 0x00, 0x27, 0xFF, 0xFF, 0x00, 0x00, 0x95, 0x02, 0x75, 0x10, 0x81, 0x02, 0xC0, //
    0x05, 0x02, 0x09, 0xC5, 0x15, 0x00, 0x26, 0xFF, 0x03, 0x95, 0x01, 0x75, 0x0A, 0x81, 0x02, //
    0x15, 0x00, 0x25, 0x00, 0x75, 0x06, 0x95, 0x01, 0x81, 0x03, //
    0x05, 0x02, 0x09, 0xC4, 0x15, 0x00, 0x26, 0xFF, 0x03, 0x95, 0x01, 0x75, 0x0A, 0x81, 0x02, //
    0x15, 0x00, 0x25, 0x00, 0x75, 0x06, 0x95, 0x01, 0x81, 0x03, //
    0x05, 0x01, 0x09, 0x39, 0x15, 0x01, 0x25, 0x08, 0x35, 0x00, 0x46, 0x3B, 0x01, 0x66, 0x14, 0x00, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, //
    0x75, 0x04, 0x95, 0x01, 0x15, 0x00, 0x25, 0x00, 0x35, 0x00, 0x45, 0x00, 0x65, 0x00, 0x81, 0x03, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x0F, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0F, 0x81, 0x02, //
    0x15, 0x00, 0x25, 0x00, 0x75, 0x01, 0x95, 0x01, 0x81, 0x03, //
    0x05, 0x0C, 0x0A, 0xB2, 0x00, 0x15, 0x00, 0x25, 0x01, 0x95, 0x01, 0x75, 0x01, 0x81, 0x02, //
    0x15, 0x00, 0x25, 0x00, 0x75, 0x07, 0x95, 0x01, 0x81, 0x03, //
    0x05, 0x0F, 0x09, 0x21, 0x85, 0x03, 0xA1, 0x02, 0x09, 0x97, 0x15, 0x00, 0x25, 0x01, 0x75, 0x04, 0x95, 0x01, 0x91, 0x02, //
    0x15, 0x00, 0x25, 0x00, 0x75, 0x04, 0x95, 0x01, 0x91, 0x03, 0x09, 0x70, 0x15, 0x00, 0x25, 0x64, 0x75, 0x08, 0x95, 0x04, 0x91, 0x02, //
    0x09, 0x50, 0x66, 0x01, 0x10, 0x55, 0x0E, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x01, 0x91, 0x02, 0x09, 0xA7, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x01, 0x91, 0x02, //
    0x65, 0x00, 0x55, 0x00, 0x09, 0x7C, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x01, 0x91, 0x02, 0xC0, 0xC0,
];

/// What the kernel glue does with a pad: personality -> ids -> (fixed) map ->
/// layout; an Xbox map with the 15-button BLE order is read through the remap.
fn prepare(vid: u16, pid: u16, name: &str, map: &[u8]) -> (Personality, hidpad::Layout, bool) {
    let (p, vid, pid) = gatt::personality(vid, pid, name);
    let map = if p == Personality::Xbox { gatt::xbox_fix_map(map) } else { map.to_vec() };
    let mut lay = hidpad::identify(vid, pid, 3, 0, 0, &map).expect("a pad");
    let remap = p == Personality::Xbox && lay.kind == hidpad::PadKind::Generic && lay.n_buttons >= 15;
    if remap {
        lay.n_buttons = 11;
    }
    if p == Personality::Xbox && lay.kind == hidpad::PadKind::Generic {
        lay.kind = hidpad::PadKind::XboxBle;
    }
    (p, lay, remap)
}

fn feed(lay: &hidpad::Layout, remap: bool, report: &[u8]) -> hidpad::PadState {
    let mut r = report.to_vec();
    if remap {
        let off = lay.buttons_off as usize + if lay.report_id != 0 { 8 } else { 0 };
        gatt::xbox_ble_remap(&mut r, off);
    }
    let mut st = hidpad::PadState::default();
    assert!(hidpad::decode(lay, &r, &mut st));
    st
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.02
}

#[test]
fn personalities() {
    assert_eq!(gatt::personality(0x045E, 0x0B13, "").0, Personality::Xbox);
    assert_eq!(gatt::personality(0x057E, 0x2009, "").0, Personality::SwitchPro);
    assert_eq!(gatt::personality(0x054C, 0x09CC, "").0, Personality::DualShock4);
    // no PnP / Device ID: the name it pairs with
    assert_eq!(gatt::personality(0, 0, "Xbox Wireless Controller"), (Personality::Xbox, 0x045E, 0x0B13));
    assert_eq!(gatt::personality(0, 0, "Pro Controller"), (Personality::SwitchPro, 0x057E, 0x2009));
    assert_eq!(gatt::personality(0x1234, 0x5678, "Aurora").0, Personality::Generic);
    assert_eq!(Personality::Xbox.label(true), "Xbox layout, Bluetooth LE");
}

#[test]
fn aurora_xinput_mode_xbox_ble_report() {
    let (p, lay, remap) = prepare(0, 0, "Xbox Wireless Controller", XBOX_BLE_MAP);
    assert_eq!(p, Personality::Xbox);
    assert!(remap);
    assert_eq!(lay.report_id, 1);
    assert_eq!(lay.n_axes, 6, "X Y Z Rz + the two triggers");
    assert!(lay.hat.is_some());
    assert_eq!(lay.buttons_off, 104);
    assert!(gatt::hid_kinds(XBOX_BLE_MAP).2);
    // LX right, LY up (HID 0), right stick centred, LT full, RT off, hat up, A + Menu
    let rep = [1, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x80, 0x00, 0x80, 0xFF, 0x03, 0x00, 0x00, 0x01, 0x01, 0x08, 0x00];
    let st = feed(&lay, remap, &rep);
    assert!(near(st.axes[0], 1.0) && near(st.axes[1], 1.0), "{:?}", st.axes);
    assert!(near(st.axes[2], 0.0) && near(st.axes[3], 0.0));
    assert!(near(st.axes[4], 1.0) && near(st.axes[5], 0.0), "triggers {:?}", st.axes);
    assert_eq!(st.hat, 0);
    assert_eq!(st.buttons, (1 << hidpad::B_A) | (1 << hidpad::B_START) | (1 << hidpad::B_UP));
    // X, Y, LB, RB, View, LS, RS, Guide in the BLE order
    let b: u16 = (1 << 3) | (1 << 4) | (1 << 6) | (1 << 7) | (1 << 10) | (1 << 13) | (1 << 14) | (1 << 12);
    let rep = [1, 0, 0x80, 0, 0x80, 0, 0x80, 0, 0x80, 0, 0, 0, 0, 0, b as u8, (b >> 8) as u8, 0];
    let st = feed(&lay, remap, &rep);
    let want = [hidpad::B_X, hidpad::B_Y, hidpad::B_LB, hidpad::B_RB, hidpad::B_BACK, hidpad::B_LS, hidpad::B_RS, hidpad::B_GUIDE].iter().fold(0u32, |m, &k| m | 1 << k);
    assert_eq!(st.buttons, want);
    assert_eq!(st.hat, -1);
    assert_eq!(lay.kind, hidpad::PadKind::XboxBle);
    // rumble: output report 3 -- enable strong | weak, triggers off, magnitudes 0..100, 2.55 s
    let pk = hidpad::rumble_packet(&lay, 1.0, 0.5, 0).expect("Xbox BT pads rumble");
    assert_eq!(pk.bytes(), &[0x03, 0x03, 0, 0, 100, 50, 0xFF, 0, 0]);
    assert_eq!(gatt::xbox_ble_rumble(1.0, 0.5, 0), [0x03, 0, 0, 100, 50, 0xFF, 0, 0]);
    assert_eq!(&pk.bytes()[1..], &gatt::xbox_ble_rumble(1.0, 0.5, 0));
}

/// An app's rumble (gamepad::set_rumble, what pad_rumble / gamepad_rumble call)
/// comes out of gamepad::next_out as the report the Bluetooth glue sends.
#[test]
fn app_rumble_reaches_an_xbox_bluetooth_pad() {
    gamepad::reset();
    let (_, lay, _) = prepare(0, 0, "Xbox Wireless Controller", XBOX_BLE_MAP);
    let h = gamepad::attach(lay, 0x045E, 0x0B13).unwrap();
    assert!(gamepad::next_out(h, 0).is_none(), "no init packets for a Bluetooth Xbox pad");
    assert!(gamepad::set_rumble(0, 0.8, 0.2, 100, 1000));
    let p = gamepad::next_out(h, 1000).expect("the rumble report");
    assert_eq!(p.bytes(), &[0x03, 0x03, 0, 0, 80, 20, 0xFF, 0, 0]);
    // after 100 ms: the stop report
    assert!(gamepad::next_out(h, 1050).is_none());
    let p = gamepad::next_out(h, 1101).expect("stop");
    assert_eq!(p.bytes(), &[0x03, 0x03, 0, 0, 0, 0, 0xFF, 0, 0]);
    assert!(gamepad::state(0).xbox);
    gamepad::detach(h);
    // the Switch Pro: HD rumble is a documented no-op
    let sw = hidpad::identify(0x057E, 0x2009, 3, 0, 0, &[]).unwrap();
    assert!(hidpad::rumble_packet(&sw, 1.0, 1.0, 0).is_none());
}

#[test]
fn aurora_switch_pro_mode() {
    let (p, lay, remap) = prepare(0, 0, "Pro Controller", &[]);
    assert_eq!(p, Personality::SwitchPro);
    assert_eq!(lay.kind, hidpad::PadKind::SwitchPro);
    assert!(!remap);
    // the simple 0x3F report clones send until switched: B (south) + Plus, hat neutral
    let st = feed(&lay, false, &[0x3F, 0x01, 0x02, 0x08, 0x00, 0x80, 0x00, 0x80, 0x00, 0x80, 0x00, 0x80]);
    assert_eq!(st.buttons, (1 << hidpad::B_A) | (1 << hidpad::B_START));
    assert_eq!(st.hat, -1);
    // the full 0x30 report after subcommand 03 30: south + Home, sticks centred
    let st = feed(&lay, false, &[0x30, 0x00, 0x91, 0x04, 0x10, 0x00, 0x00, 0x08, 0x80, 0x00, 0x08, 0x80]);
    assert_eq!(st.buttons, (1 << hidpad::B_A) | (1 << hidpad::B_GUIDE));
    assert!(near(st.axes[0], 0.0) && near(st.axes[1], 0.0));
    // its init over Bluetooth: input mode 0x30 and the player lights are among the packets
    let mut pk: Vec<Vec<u8>> = Vec::new();
    hidpad::init_packets(&lay, 0x057E, 0x2009, 0, |o| pk.push(o.bytes().to_vec()));
    assert!(pk.iter().any(|p| p.first() == Some(&0x01) && p.get(10) == Some(&0x03) && p.get(11) == Some(&0x30)), "{pk:?}");
    assert!(pk.iter().any(|p| p.first() == Some(&0x01) && p.get(10) == Some(&0x30)));
    let _ = vec![0u8];
}
