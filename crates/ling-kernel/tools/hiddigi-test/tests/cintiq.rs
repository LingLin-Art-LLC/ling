//! Real Cintiq report descriptors (dumped by linuxwacom/wacom-hid-descriptors on Linux) through the kernel's
//! parser, and pen reports laid out the way those descriptors say (HID models) or the way Linux's
//! wacom_wac.c reads them (the 24HD's Intuos packets), checked for position, pressure, tilt and buttons.

use hiddigi_test::hiddigi::*;

const PRO16_PEN: &[u8] = include_bytes!("../data/cintiq-pro-16-056a-0350-pen.bin");
const PRO16_TOUCH: &[u8] = include_bytes!("../data/cintiq-pro-16-056a-0354-touch.bin");
const PRO27_PEN: &[u8] = include_bytes!("../data/cintiq-pro-27-056a-03c0-pen.bin");
const PRO27_TOUCH: &[u8] = include_bytes!("../data/cintiq-pro-27-056a-03c0-touch.bin");
const HD24_PEN: &[u8] = include_bytes!("../data/cintiq-24hd-touch-056a-00f8.bin");

fn close(a: f32, b: f32, eps: f32) -> bool {
    (a - b).abs() <= eps
}

fn put(rep: &mut [u8], at: usize, v: u32, bytes: usize) {
    for k in 0..bytes {
        rep[at + k] = (v >> (8 * k)) as u8;
    }
}

#[test]
fn cintiq_pro_16_pen() {
    let (ls, modes) = parse_full(PRO16_PEN);
    // both pen reports: the vendor one (16, X 0..69920, pressure 0..8191) first, then the standard one (6)
    let ids: Vec<Option<u8>> = ls.iter().map(|l| l.report_id).collect();
    assert_eq!(ids, vec![Some(16), Some(6)], "{ls:?}");
    assert!(ls.iter().all(|l| l.pen));
    let v = &ls[0];
    assert!(v.vendor && v.x.lmax == 69920 && v.y.lmax == 39680 && v.pressure.lmax == 8191);
    assert!(v.tilt_x.ok && v.tilt_y.ok && v.barrel.ok && v.barrel2.ok && v.eraser.ok && v.invert.ok && v.sense.ok && v.distance.ok);
    assert_eq!((v.x.bit, v.x.size, v.y.bit, v.pressure.bit, v.tilt_x.bit, v.tilt_y.bit), (8, 24, 32, 56, 72, 80));
    // the mode switch: data mode report 2 -> 2, sensor margins in report 12, report 2 is 2 bytes long
    assert_eq!(modes.mode, Some((2, 2)));
    assert_eq!(modes.offsets, Some((12, 0)));
    assert_eq!(modes.len(2), Some(2));
    assert_eq!(modes.len(12), Some(9));

    let mut d = Device::hid(WACOM_VID, 0x0350, &ls);
    assert!(d.pen());
    assert_eq!(d.pressure_max(), 8191);
    assert!(d.has_tilt());
    assert!(d.describe().contains("Wacom Cintiq Pro 16 056a:0350"), "{}", d.describe());

    // vendor report: tip + in range + in proximity, X 3/4, Y 1/2, pressure 4096 (~0.5), tilt +32 / -16 degrees
    let mut rep = [0u8; 27];
    rep[0] = 16;
    rep[1] = 0b0110_0001;
    put(&mut rep, 2, 52440, 3);
    put(&mut rep, 5, 19840, 3);
    put(&mut rep, 8, 4096, 2);
    rep[10] = 32u8;
    rep[11] = (-16i8) as u8;
    let s = decode_dev(&mut d, &rep).expect("decoded");
    assert!(s.in_range && s.contact && !s.eraser && !s.barrel && !s.barrel2, "{s:?}");
    assert!(close(s.nx, 0.75, 0.001) && close(s.ny, 0.5, 0.001), "{s:?}");
    assert!(close(s.p, 0.5, 0.001) && s.has_pressure);
    assert!(close(s.tx, 32.0, 0.01) && close(s.ty, -16.0, 0.01), "{s:?}");
    assert!(s.tilt > 0.1);
    assert_eq!(s.buttons, 1);

    // hovering with the lower side button: right button, no pressure
    rep[1] = 0b0110_0010;
    put(&mut rep, 8, 0, 2);
    let s = decode_dev(&mut d, &rep).unwrap();
    assert!(s.in_range && !s.contact && s.barrel && s.p == 0.0);
    assert_eq!(s.buttons, 2);
    // upper side button: middle
    rep[1] = 0b0110_0100;
    assert_eq!(decode_dev(&mut d, &rep).unwrap().buttons, 4);
    // the eraser end touching: a left click, flagged as the eraser
    rep[1] = 0b0111_1001;
    put(&mut rep, 8, 3000, 2);
    let s = decode_dev(&mut d, &rep).unwrap();
    assert!(s.contact && s.eraser && s.buttons == 1);
    // in range but out of proximity (too high): not pointing
    rep[1] = 0b0010_0000;
    assert!(!decode_dev(&mut d, &rep).unwrap().in_range);

    // the standard report (6) decodes too: X 0..34560 24-bit, pressure 0..2047, tilt -64..63
    let mut rep = [0u8; 20];
    rep[0] = 6;
    rep[1] = 0b0010_0001;
    put(&mut rep, 2, 17280, 3);
    put(&mut rep, 5, 4860, 3);
    put(&mut rep, 8, 2047, 2);
    rep[10] = (-64i8) as u8;
    let s = decode_dev(&mut d, &rep).unwrap();
    assert!(close(s.nx, 0.5, 0.001) && close(s.ny, 0.25, 0.001) && close(s.p, 1.0, 0.001) && close(s.tx, -64.0, 0.01), "{s:?}");

    // the sensor margins shrink only the vendor report's range
    d.set_offsets(200, 100, 200, 100);
    assert_eq!((d.reps[0].x.lmin, d.reps[0].x.lmax, d.reps[0].y.lmin, d.reps[0].y.lmax), (200, 69720, 100, 39580));
    assert_eq!(d.reps[1].x.lmax, 34560);
}

#[test]
fn cintiq_pro_27_pen() {
    let (ls, modes) = parse_full(PRO27_PEN);
    let ids: Vec<Option<u8>> = ls.iter().map(|l| l.report_id).collect();
    assert_eq!(ids, vec![Some(30), Some(6)], "{ls:?}");
    assert_eq!(modes.mode, Some((2, 2)));
    let mut d = Device::hid(WACOM_VID, 0x03c0, &ls);
    assert!(d.describe().starts_with("Wacom Cintiq Pro 27"));
    // vendor report 30: contact count, then tip / barrel / barrel2 / barrel3 / eraser / invert / in range / sense
    let mut rep = [0u8; 40];
    rep[0] = 30;
    rep[1] = 1;
    rep[2] = 0b1100_0001;
    put(&mut rep, 3, 60016, 3);
    put(&mut rep, 6, 16967, 3);
    put(&mut rep, 9, 8191, 2);
    put(&mut rep, 11, (-20i16) as u16 as u32, 2);
    put(&mut rep, 13, 40, 2);
    let s = decode_dev(&mut d, &rep).unwrap();
    assert!(s.contact && s.in_range && close(s.nx, 0.5, 0.001) && close(s.ny, 0.25, 0.001) && close(s.p, 1.0, 0.001), "{s:?}");
    assert!(close(s.tx, -20.0, 0.01) && close(s.ty, 40.0, 0.01), "{s:?}");
    // the Pro Pen 3's third button counts as the upper one
    rep[2] = 0b1100_1000;
    let s = decode_dev(&mut d, &rep).unwrap();
    assert!(s.barrel2 && s.buttons == 4, "{s:?}");
    // standard report 6: X 0..32767 16-bit after a contact-count byte, tilt -9000..9000 hundredths of a degree
    let mut rep = [0u8; 40];
    rep[0] = 6;
    rep[1] = 1;
    rep[2] = 0b0100_0011; // tip + barrel + in range
    put(&mut rep, 3, 32767, 2);
    put(&mut rep, 6, 0, 2);
    put(&mut rep, 9, 1024, 2);
    put(&mut rep, 11, 4500, 2);
    put(&mut rep, 13, (-3000i16) as u16 as u32, 2);
    let s = decode_dev(&mut d, &rep).unwrap();
    assert!(close(s.nx, 1.0, 0.001) && close(s.ny, 0.0, 0.001) && close(s.p, 0.5, 0.001), "{s:?}");
    assert!(close(s.tx, 45.0, 0.1) && close(s.ty, -30.0, 0.1), "{s:?}");
    assert_eq!(s.buttons, 2); // the side button wins over the tip, as the Wacom driver does
}

#[test]
fn touch_interfaces_are_not_pens() {
    assert!(parse_full(PRO16_TOUCH).0.is_empty());
    assert!(parse_full(PRO27_TOUCH).0.is_empty());
}

/// Encode an Intuos-protocol pen packet the way wacom_intuos_general reads one (17-bit X / Y, 11-bit
/// pressure, 7-bit tilts, 6-bit distance).
fn intuos_packet(x: u32, y: u32, p: u32, tx: i32, ty: i32, dist: u32, side: u8) -> [u8; 10] {
    let (tx, ty) = ((tx + 64) as u32, (ty + 64) as u32);
    let mut r = [0u8; 10];
    r[0] = 2;
    r[1] = 0xE0 | side << 1 | (p & 1) as u8;
    r[2] = (x >> 9) as u8;
    r[3] = (x >> 1) as u8;
    r[4] = (y >> 9) as u8;
    r[5] = (y >> 1) as u8;
    r[6] = (p >> 3) as u8;
    r[7] = (((p >> 1) & 3) << 6) as u8 | (tx >> 1) as u8 & 0x3f;
    r[8] = ((tx & 1) << 7) as u8 | (ty & 0x7f) as u8;
    r[9] = (dist << 2) as u8 | ((x & 1) << 1) as u8 | (y & 1) as u8;
    r
}

#[test]
fn cintiq_24hd_wacom_packets() {
    // the HID descriptor shows only a relative mouse (report 1) and opaque vendor bytes (report 2): no pen
    // until the mode switch, feature report 2 (2 bytes)
    let (ls, modes) = parse_full(HD24_PEN);
    assert!(ls.is_empty(), "{ls:?}");
    assert_eq!(modes.len(2), Some(2));
    assert_eq!(modes.mode, None);
    let m = lookup(0x00f8).expect("24HD touch on the table");
    assert_eq!((m.name, m.p_max, m.family), ("Wacom Cintiq 24HD touch", 2047, Family::Intuos));
    assert!(touch_only(0x00f6) && !touch_only(0x00f8));
    let mut d = Device::wacom(m);
    assert!(d.pen());
    assert!(d.describe().contains("056a:00f8: Wacom packets"), "{}", d.describe());

    // entering proximity with the grip pen (tool 0x802): no position yet
    let mut prox = [0u8; 10];
    prox[0] = 2;
    prox[1] = 0xC0;
    prox[2] = 0x80;
    prox[3] = 0x20;
    let s = decode_dev(&mut d, &prox).unwrap();
    assert!(s.in_range && !s.contact && !s.eraser);
    assert_eq!(d.tool, 0x802);

    // touching mid-screen (the 400-unit margins cut off), half pressure, tilted 30 right / 10 back
    let cx = 400 + (104480 - 800) / 2;
    let cy = 400 + (65600 - 800) / 4;
    let s = decode_dev(&mut d, &intuos_packet(cx, cy, 1024, 30, -10, 0, 0)).unwrap();
    assert!(s.contact && s.in_range && !s.eraser, "{s:?}");
    assert!(close(s.nx, 0.5, 0.0001) && close(s.ny, 0.25, 0.0001), "{s:?}");
    assert!(close(s.p, 1024.0 / 2047.0, 0.001));
    assert!(close(s.tx, 30.15, 0.05) && close(s.ty, -10.05, 0.05), "{s:?}");
    assert_eq!(s.buttons, 1);
    // hovering 20 units up with the lower side switch: right button
    let s = decode_dev(&mut d, &intuos_packet(cx, cy, 0, 0, 0, 20, 1)).unwrap();
    assert!(!s.contact && s.barrel && s.buttons == 2 && close(s.dist, 20.0 / 63.0, 0.001), "{s:?}");
    // upper side switch: middle
    assert_eq!(decode_dev(&mut d, &intuos_packet(cx, cy, 0, 0, 0, 20, 2)).unwrap().buttons, 4);
    // pressure under Linux's 10-unit touch threshold: not touching
    assert!(!decode_dev(&mut d, &intuos_packet(cx, cy, 8, 0, 0, 0, 0)).unwrap().contact);
    // the pad's report (12) is not the pen
    let mut pad = [0u8; 10];
    pad[0] = 12;
    assert!(decode_dev(&mut d, &pad).is_none());

    // out of proximity, then the eraser end comes in (tool 0x80A)
    let mut out = [0u8; 10];
    out[0] = 2;
    out[1] = 0x80;
    let s = decode_dev(&mut d, &out).unwrap();
    assert!(!s.in_range && !s.contact && d.tool == 0);
    prox[3] = 0xA0;
    decode_dev(&mut d, &prox).unwrap();
    assert_eq!(d.tool, 0x80A);
    let s = decode_dev(&mut d, &intuos_packet(cx, cy, 600, 0, 0, 0, 0)).unwrap();
    assert!(s.eraser && s.contact && s.buttons == 1, "{s:?}");
}

#[test]
fn cintiq_13hd_halves_pressure() {
    // 13HD (pen 0x304): 1023 pressure levels, the 11-bit field shifted down once
    let mut d = Device::wacom(lookup(0x0304).unwrap());
    let s = decode_dev(&mut d, &intuos_packet(30000, 17000, 2046, 0, 0, 0, 0)).unwrap();
    assert!(close(s.p, 1.0, 0.001), "{s:?}");
}

#[test]
fn cintiq_21ux_needs_rdy() {
    // the 1st-gen 21UX drops packets without the RDY bit (0x40)
    let mut d = Device::wacom(lookup(0x003f).unwrap());
    let mut r = intuos_packet(40000, 30000, 800, 0, 0, 0, 0);
    assert!(decode_dev(&mut d, &r).unwrap().contact);
    r[1] &= !0x40;
    assert!(decode_dev(&mut d, &r).is_none());
}

#[test]
fn pl_and_dtu_pen_displays() {
    // DTU-1031 (DTUS packets, report 17): prox + pen tool, pressure 600 / 511... clamps, X / Y big-endian
    let mut d = Device::wacom(lookup(0x00fb).unwrap());
    let x = 200 + (22096 - 400) / 2;
    let y = 200 + (13960 - 400) / 2;
    let r = [17, 0x80 | 2 << 3 | 0x20 | 0x01, 0x2c, (x >> 8) as u8, x as u8, (y >> 8) as u8, y as u8, 0];
    let s = decode_dev(&mut d, &r).unwrap();
    assert!(s.in_range && s.contact && s.barrel && !s.eraser && close(s.nx, 0.5, 0.001) && close(s.ny, 0.5, 0.001), "{s:?}");
    assert_eq!(s.buttons, 2);
    // DTU-2231 (DTU packets): eraser in prox touching
    let mut d = Device::wacom(lookup(0x00ce).unwrap());
    let r = [2, 0x20 | 0x04 | 0x01, 0, 0, 0, 0, 0xff, 0x01];
    let s = decode_dev(&mut d, &r).unwrap();
    assert!(s.eraser && s.contact && close(s.p, 1.0, 0.003), "{s:?}");
    // PL-550 (PL packets, 511 levels): pressure is signed around the middle
    let mut d = Device::wacom(lookup(0x0034).unwrap());
    let r = [2, 0x40, 0, 0, 0x08, 0, 0, 0x00];
    let s = decode_dev(&mut d, &r).unwrap();
    assert!(s.contact && close(s.p, 256.0 / 511.0, 0.003), "{s:?}");
}
