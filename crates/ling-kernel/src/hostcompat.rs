//! Host-runtime builtins that desktop apps (Ling Arcade) call and the LingOS app
//! runtime ([`crate::lingapp`]) didn't have yet: `grad_rect`, the bitmap-font
//! `font_text_px` / `font_width_px` pair, binary file I/O (`read_file_bytes` /
//! `write_file_bytes`), `list_dir` / `path_name`, a few string/maths helpers,
//! the host's gamepad APIs (`pad_poll`, `pad_pressed(i, "a")`, `pad_lx` ...,
//! `pad_rumble`, and gilrs' `gamepad_poll` / `gamepad_button` / `gamepad_axis`
//! / `gamepad_rumble` / `gamepad_list` / `gamepad_any`), the `audio_tone` synth voices, and `emu_*` answering "no native
//! emulator cores here". Semantics follow `ling/src/runtime/mod.rs`.
//!
//! Reached from the tail of [`crate::lingapp::dispatch`], so anything the app
//! runtime already answers keeps its own meaning.

use alloc::string::String;
use alloc::vec::Vec;

use crate::lingrt::{boolean, is_str, list_from, list_items, mk_str, num, to_f64, to_string, UNIT};

fn f(args: &[u64], i: usize, d: f64) -> f64 {
    args.get(i).map(|&v| to_f64(v)).unwrap_or(d)
}

fn s(args: &[u64], i: usize) -> String {
    args.get(i).map(|&v| to_string(v)).unwrap_or_default()
}

fn path(args: &[u64], i: usize) -> String {
    s(args, i).replace('\\', "/")
}

// ── gamepad: the host's pad API over drivers::gamepad ───────────────────────
//
// Two host APIs, same semantics as the desktop runtime (ling/src/runtime):
//   pad_*      (ling-input "Sensorium"): pad index first, sticks with a 0.12
//              radial deadzone and Y SCREEN-DOWN positive, triggers 0..1,
//              pad_pressed / pad_released edges between two pad_poll calls;
//   gamepad_*  (gilrs): the most recently active pad, Y UP positive.
// drivers::gamepad keeps every pad with Y up positive.
//
// Security: pads only feed apps while a user is signed in and the session is
// unlocked (keyring open): before sign-in, on the lock screen and after
// logging out every pad reads as disconnected. Nothing maps pad input onto
// the keyboard or the pointer.

use crate::drivers::gamepad::MAX_PADS;
use crate::drivers::hidpad::PadState;

/// Buttons as of the last `pad_poll` and the poll before it (edge detection), per pad.
static mut PAD_NOW: [u32; MAX_PADS] = [0; MAX_PADS];
static mut PAD_PREV: [u32; MAX_PADS] = [0; MAX_PADS];
/// The `gamepad_*` API's pad: the last one a button was pressed on.
static mut GP_ACTIVE: usize = 0;

/// May pads reach apps now? Only in a signed-in, unlocked session.
pub(crate) fn pads_allowed() -> bool {
    crate::keyring::is_unlocked()
}

const NO_PAD: PadState = PadState { connected: false, axes: [0.0; crate::drivers::hidpad::MAX_AXES], buttons: 0, hat: -1, xbox: false };

/// Player `i`'s pad, fresh (pending USB reports delivered first).
pub(crate) fn pad_of(i: usize) -> PadState {
    if !pads_allowed() {
        return NO_PAD;
    }
    // one USB service per millisecond is plenty (a frame reads a pad a dozen times)
    static mut LAST_US: u64 = 0;
    let now = crate::arch::timer::now_us();
    if now.wrapping_sub(unsafe { LAST_US }) >= 1000 {
        unsafe { LAST_US = now };
        crate::arch::xhci::pad_poll();
    }
    crate::drivers::gamepad::state(i)
}

fn pad_idx(args: &[u64], k: usize) -> usize {
    f(args, k, 0.0).clamp(0.0, (MAX_PADS - 1) as f64) as usize
}

/// The pad's buttons as a mask, with the hat folded onto the d-pad bits 12..15
/// and the triggers as bits 16 / 17.
fn pad_mask(p: &PadState) -> u32 {
    if !p.connected {
        return 0;
    }
    let mut m = p.buttons | crate::drivers::hidpad::hat_bits(p.hat);
    if p.axes[4] > 0.5 {
        m |= 1 << 16;
    }
    if p.axes[5] > 0.5 {
        m |= 1 << 17;
    }
    m
}

/// Host button name -> bit (A B X Y LB RB Back Start LS RS Guide, the extra
/// button 11, d-pad 12..15, triggers 16 / 17). Accepts both APIs' names.
pub(crate) fn pad_bit(name: &str) -> Option<u32> {
    Some(match name.to_ascii_lowercase().as_str() {
        "a" | "south" | "cross" => 0,
        "b" | "east" | "circle" => 1,
        "x" | "west" | "square" => 2,
        "y" | "north" | "triangle" => 3,
        "lb" | "l1" | "left_shoulder" => 4,
        "rb" | "r1" | "right_shoulder" => 5,
        "select" | "back" | "share" | "view" | "create" | "minus" => 6,
        "start" | "menu" | "options" | "plus" => 7,
        "l3" | "left_stick" | "ls" => 8,
        "r3" | "right_stick" | "rs" => 9,
        "guide" | "home" | "ps" | "mode" => 10,
        "touchpad" | "capture" | "misc" => 11,
        "up" | "dpad_up" | "dup" => 12,
        "down" | "dpad_down" | "ddown" => 13,
        "left" | "dpad_left" | "dleft" => 14,
        "right" | "dpad_right" | "dright" => 15,
        "lt" | "l2" | "left_trigger" | "zl" => 16,
        "rt" | "r2" | "right_trigger" | "zr" => 17,
        _ => return None,
    })
}

/// `pad_button(i, "a")` (host form) — also used by lingapp's `pad_button`.
pub(crate) fn pad_button_named(i: usize, name: &str) -> bool {
    let p = pad_of(i);
    pad_bit(name).is_some_and(|b| (pad_mask(&p) >> b) & 1 == 1)
}

/// A stick the way ling-input shapes it: radial deadzone 0.12, saturation
/// 0.95, linear; Y flipped to screen-down.
fn host_stick(x: f32, y_up: f32) -> (f32, f32) {
    let (x, y) = (x, -y_up);
    let mag = libm::sqrtf(x * x + y * y);
    let (dz, sat) = (0.12f32, 0.95f32);
    if mag <= dz {
        return (0.0, 0.0);
    }
    let shaped = ((mag - dz) / (sat - dz)).min(1.0);
    (x / mag * shaped, y / mag * shaped)
}

/// A trigger the way ling-input shapes it: deadzone 0.04, 0..1.
fn host_trigger(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.04 {
        0.0
    } else {
        ((v - 0.04) / 0.96).min(1.0)
    }
}

/// A gilrs axis (`gamepad_axis`): Y up positive, a small axial deadzone.
fn gilrs_axis(v: f32) -> f32 {
    if v.abs() < 0.08 {
        0.0
    } else {
        v
    }
}

/// `gamepad_list()`: one line per pad (name, connection, rumble, ids).
fn pad_list() -> String {
    let mut out = String::new();
    if !pads_allowed() {
        out.push_str("(gamepads are read after sign-in)\n");
        return out;
    }
    crate::arch::xhci::pad_poll();
    let n = crate::drivers::gamepad::count();
    for i in 0..n {
        if let Some((name, kind, vid, pid)) = crate::drivers::gamepad::info(i) {
            let rumble = !matches!(kind, crate::drivers::hidpad::PadKind::Generic | crate::drivers::hidpad::PadKind::SwitchPro);
            out.push_str(&alloc::format!("{}: \"{name}\" connected=true rumble={rumble} map=LingOS ({vid:04x}:{pid:04x})\n", i + 1));
        }
    }
    if n == 0 {
        out.push_str("(no gamepads -- plug one in over USB)\n");
    }
    out
}

// ── drawing ──────────────────────────────────────────────────────────────────

/// `grad_rect(x, y, w, h, r0, g0, b0, r1, g1, b1, dir)`: dir 0 = left→right,
/// otherwise top→bottom. Equal end colours are a plain solid fill (the arcade
/// draws the NES picture that way, one span per run of a colour).
fn grad_rect(args: &[u64]) {
    let Some((buf, bw, bh)) = crate::lingapp::surface() else { return };
    let (x, y, w, h) = (f(args, 0, 0.0), f(args, 1, 0.0), f(args, 2, 0.0), f(args, 3, 0.0));
    let c = |i: usize, d: f64| f(args, i, d).clamp(0.0, 255.0);
    let (r0, g0, b0) = (c(4, 255.0), c(5, 255.0), c(6, 255.0));
    let (r1, g1, b1) = (c(7, 0.0), c(8, 0.0), c(9, 0.0));
    let vertical = f(args, 10, 1.0) as i64 != 0;
    let x0 = (libm::round(x) as i64).max(0);
    let y0 = (libm::round(y) as i64).max(0);
    let x1 = (libm::round(x + w) as i64).min(bw as i64);
    let y1 = (libm::round(y + h) as i64).min(bh as i64);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let pack = |r: f64, g: f64, b: f64| ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
    let lerp = |t: f64| pack(r0 + (r1 - r0) * t, g0 + (g1 - g0) * t, b0 + (b1 - b0) * t);
    let solid = r0 == r1 && g0 == g1 && b0 == b1;
    let (fx, fy, fw, fh) = (libm::round(x), libm::round(y), w.max(1.0), h.max(1.0));
    for yy in y0..y1 {
        let row = yy as usize * bw;
        let line = &mut buf[row + x0 as usize..row + x1 as usize];
        if solid {
            line.fill(pack(r0, g0, b0));
        } else if vertical {
            line.fill(lerp(((yy as f64 - fy) / fh).clamp(0.0, 1.0)));
        } else {
            for (k, px) in line.iter_mut().enumerate() {
                *px = lerp((((x0 + k as i64) as f64 - fx) / fw).clamp(0.0, 1.0));
            }
        }
    }
}

// ── dispatch ─────────────────────────────────────────────────────────────────

pub fn dispatch(name: &[u8], args: &[u64]) -> Option<u64> {
    Some(match name {
        b"grad_rect" => {
            grad_rect(args);
            UNIT
        },
        // Same argument layout as the app runtime's font_text / font_width
        // (handle, x, y, px, text) / (handle, px, text), drawn in set_color's colour.
        b"font_text_px" => return crate::lingapp::dispatch(b"font_text", args),
        b"font_width_px" => return crate::lingapp::dispatch(b"font_width", args),
        b"monitor_height" | b"screen_height" => num(crate::drivers::framebuffer::height() as f64),
        b"monitor_width" | b"screen_width" => num(crate::drivers::framebuffer::width() as f64),
        // LingOS windows are always the whole screen: "maximized" is a no-op and
        // the work area is the screen (safe area).
        b"window_maximize" | b"maximize_window" => UNIT,
        b"window_work_area" | b"work_area" => list_from(alloc::vec![
            num(crate::drivers::framebuffer::width() as f64),
            num(crate::drivers::framebuffer::height() as f64),
        ]),

        // Files.
        b"read_file_bytes" => {
            let bytes = crate::vfs::read(&path(args, 0)).unwrap_or_default();
            let off = (f(args, 1, 0.0).max(0.0) as usize).min(bytes.len());
            let want = f(args, 2, -1.0);
            let end = if want < 0.0 { bytes.len() } else { (off + want as usize).min(bytes.len()) };
            list_from(bytes[off..end].iter().map(|&b| num(b as f64)).collect())
        },
        b"write_file_bytes" => {
            let data: Vec<u8> = args
                .get(1)
                .map(|&l| list_items(l).iter().map(|&v| to_f64(v) as i64 as u8).collect())
                .unwrap_or_default();
            boolean(crate::vfs::write(&path(args, 0), &data))
        },
        // The host's list_dir: full paths ("roms/x.nes"), sorted.
        b"list_dir" => {
            let dir = {
                let d = path(args, 0);
                if d.is_empty() { String::from(".") } else { d }
            };
            let base = dir.trim_end_matches('/');
            let mut out: Vec<String> = crate::vfs::list(if base == "." { "" } else { base })
                .into_iter()
                .map(|(n, _, _)| if base == "." || base.is_empty() { n } else { alloc::format!("{base}/{n}") })
                .collect();
            out.sort();
            list_from(out.iter().map(|p| mk_str(p)).collect())
        },
        b"path_name" => {
            let p = path(args, 0);
            mk_str(p.trim_end_matches('/').rsplit('/').next().unwrap_or(""))
        },
        b"path_ext" => {
            let p = path(args, 0);
            let n = p.rsplit('/').next().unwrap_or("");
            mk_str(match n.rfind('.') {
                Some(i) if i > 0 => &n[i + 1..],
                _ => "",
            })
        },

        // Strings / maths.
        b"ends_with" | b"str_ends_with" => boolean(s(args, 0).ends_with(s(args, 1).as_str())),
        b"starts_with" | b"str_starts_with" => boolean(s(args, 0).starts_with(s(args, 1).as_str())),
        b"chr" => {
            let c = char::from_u32(f(args, 0, 0.0) as u32).unwrap_or(char::REPLACEMENT_CHARACTER);
            let mut b = [0u8; 4];
            mk_str(c.encode_utf8(&mut b))
        },
        b"ord" => num(s(args, 0).chars().next().map(|c| c as u32 as f64).unwrap_or(-1.0)),
        b"clamp" => {
            let (x, lo, hi) = (f(args, 0, 0.0), f(args, 1, 0.0), f(args, 2, 1.0));
            num(if x < lo { lo } else if x > hi { hi } else { x })
        },

        // Gamepad, host API (ling-input): pad index first, up to MAX_PADS pads.
        b"pad_poll" => {
            let mut n = 0;
            for i in 0..MAX_PADS {
                let p = pad_of(i);
                n += p.connected as i32;
                unsafe {
                    PAD_PREV[i] = PAD_NOW[i];
                    PAD_NOW[i] = pad_mask(&p);
                }
            }
            num(n as f64)
        },
        b"pad_count" => num(if pads_allowed() { crate::drivers::gamepad::count() as f64 } else { 0.0 }),
        b"pad_pressed" | b"pad_released" => {
            let i = pad_idx(args, 0);
            let Some(b) = pad_bit(&s(args, 1)) else { return Some(boolean(false)) };
            let (now, prev) = unsafe { ((PAD_NOW[i] >> b) & 1, (PAD_PREV[i] >> b) & 1) };
            boolean(if name == b"pad_pressed" { now == 1 && prev == 0 } else { now == 0 && prev == 1 })
        },
        b"pad_lx" | b"pad_ly" => {
            let p = pad_of(pad_idx(args, 0));
            let (x, y) = host_stick(p.axes[0], p.axes[1]);
            num(if name == b"pad_lx" { x } else { y } as f64)
        },
        b"pad_rx" | b"pad_ry" => {
            let p = pad_of(pad_idx(args, 0));
            let (x, y) = host_stick(p.axes[2], p.axes[3]);
            num(if name == b"pad_rx" { x } else { y } as f64)
        },
        b"pad_lt" => num(host_trigger(pad_of(pad_idx(args, 0)).axes[4]) as f64),
        b"pad_rt" => num(host_trigger(pad_of(pad_idx(args, 0)).axes[5]) as f64),
        // pad_rumble(i, low, high): motor amplitudes 0..1 until changed
        b"pad_rumble" => {
            if pads_allowed() {
                crate::drivers::gamepad::set_rumble(pad_idx(args, 0), f(args, 1, 0.0) as f32, f(args, 2, f(args, 1, 0.0)) as f32, 0, crate::arch::timer::now_ms());
                crate::arch::xhci::pad_poll();
            }
            UNIT
        },

        // Gamepad, host API (gilrs): the most recently active pad, Y up.
        b"gamepad_poll" => {
            if pads_allowed() {
                crate::arch::xhci::pad_poll();
                for i in 0..crate::drivers::gamepad::count() {
                    if pad_mask(&crate::drivers::gamepad::state(i)) != 0 {
                        unsafe { GP_ACTIVE = i };
                        break;
                    }
                }
                if unsafe { GP_ACTIVE } >= crate::drivers::gamepad::count() {
                    unsafe { GP_ACTIVE = 0 };
                }
            }
            UNIT
        },
        b"gamepad_button" => {
            let p = pad_of(unsafe { GP_ACTIVE });
            num(pad_bit(&s(args, 0)).is_some_and(|b| (pad_mask(&p) >> b) & 1 == 1) as i32 as f64)
        },
        b"gamepad_axis" => {
            let p = pad_of(unsafe { GP_ACTIVE });
            let v = match s(args, 0).as_str() {
                "lx" => gilrs_axis(p.axes[0]),
                "ly" => gilrs_axis(p.axes[1]),
                "rx" => gilrs_axis(p.axes[2]),
                "ry" => gilrs_axis(p.axes[3]),
                "l2" | "lt" => p.axes[4],
                "r2" | "rt" => p.axes[5],
                _ => 0.0,
            };
            num(v as f64)
        },
        // gamepad_rumble(low, high, ms)
        b"gamepad_rumble" => {
            if pads_allowed() {
                let ms = f(args, 2, 200.0).clamp(0.0, 60_000.0) as u32;
                crate::drivers::gamepad::set_rumble(unsafe { GP_ACTIVE }, f(args, 0, 0.0) as f32, f(args, 1, 0.0) as f32, ms.max(1), crate::arch::timer::now_ms());
                crate::arch::xhci::pad_poll();
            }
            UNIT
        },
        b"gamepad_list" => mk_str(&pad_list()),
        b"gamepad_any" => num((pad_mask(&pad_of(unsafe { GP_ACTIVE })) & 0xFFFF != 0) as i32 as f64),

        // audio_tone(idx, x, y, z, w, freq, amp, lfo_rate, lfo_depth [, wave, duty])
        b"audio_tone" => {
            crate::lingapp::note_tone_use();
            let wave = match s(args, 9).as_str() {
                "square" | "pulse" => 1,
                "saw" | "sawtooth" => 2,
                "triangle" | "tri" => 3,
                "noise" => 4,
                _ => 0,
            };
            crate::drivers::sfx::set_tone(
                f(args, 0, 0.0).max(0.0) as usize,
                f(args, 5, 220.0) as f32,
                f(args, 6, 0.15) as f32,
                wave,
                f(args, 10, 0.5) as f32,
                f(args, 1, 0.0) as f32,
            );
            UNIT
        },
        b"audio_glide" => {
            crate::drivers::sfx::set_tone_glide(f(args, 0, 0.0) as f32, f(args, 1, 0.0) as f32);
            UNIT
        },

        // No native emulator cores (ling-emu) on LingOS: the arcade hides SNES/N64.
        b"emu_available" | b"emu_loaded" | b"emu_load" => boolean(false),
        b"emu_status" => mk_str(""),
        b"emu_fb_size" => list_from(alloc::vec![num(0.0), num(0.0)]),
        b"emu_frame" | b"emu_blit" | b"emu_button" | b"emu_stick" | b"emu_flush" | b"emu_stop" => UNIT,

        _ => {
            let _ = is_str;
            return None;
        },
    })
}
