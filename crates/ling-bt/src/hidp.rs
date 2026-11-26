//! HIDP, the Bluetooth HID host side (HID Profile 1.1): the control channel
//! (PSM 0x11) carries requests, the interrupt channel (PSM 0x13) the reports.
//!
//! The host puts the device in the boot protocol (SET_PROTOCOL 0), so a
//! keyboard sends report id 1 with the 8-byte boot keyboard report and a mouse
//! report id 2 with buttons / dx / dy (/ wheel) -- no report descriptor needed.

/// HIDP message types (high nibble).
pub const HANDSHAKE: u8 = 0x0;
pub const HID_CONTROL: u8 = 0x1;
pub const SET_PROTOCOL: u8 = 0x7;
pub const DATA: u8 = 0xA;

/// SET_PROTOCOL(boot) for the control channel.
pub const SET_PROTOCOL_BOOT: [u8; 1] = [(SET_PROTOCOL << 4) | 0];
/// SET_PROTOCOL(report): gamepads keep their own report format.
pub const SET_PROTOCOL_REPORT: [u8; 1] = [(SET_PROTOCOL << 4) | 1];
/// HID_CONTROL(VIRTUAL_CABLE_UNPLUG): the device forgets us (on Remove).
pub const VIRTUAL_CABLE_UNPLUG: [u8; 1] = [(HID_CONTROL << 4) | 0x5];

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Report {
    /// The boot keyboard report: modifiers, reserved, six usages.
    Keyboard([u8; 8]),
    Mouse { buttons: u8, dx: i8, dy: i8, wheel: i8 },
}

/// A message from the interrupt channel: a boot keyboard / mouse input report,
/// or None (anything else: another report id, an output report, garbage).
pub fn parse_input(msg: &[u8]) -> Option<Report> {
    let hdr = *msg.first()?;
    if hdr != (DATA << 4) | 0x1 {
        return None; // DATA | Input
    }
    match *msg.get(1)? {
        1 => {
            let r: [u8; 8] = msg.get(2..10)?.try_into().ok()?;
            Some(Report::Keyboard(r))
        },
        2 => {
            let b = msg.get(2..)?;
            Some(Report::Mouse {
                buttons: *b.first()? & 0x07,
                dx: *b.get(1)? as i8,
                dy: *b.get(2)? as i8,
                wheel: b.get(3).map(|&w| w as i8).unwrap_or(0),
            })
        },
        _ => None,
    }
}

/// A control-channel message: Some(result code) for a HANDSHAKE.
pub fn parse_handshake(msg: &[u8]) -> Option<u8> {
    let h = *msg.first()?;
    (h >> 4 == HANDSHAKE).then_some(h & 0xF)
}

/// The keyboard LED output report (num / caps / scroll lock) for the interrupt channel.
pub fn leds(bits: u8) -> [u8; 3] {
    [(DATA << 4) | 0x2, 1, bits & 0x1F]
}
