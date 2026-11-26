//! The USB pen as the whole system's pointer, and its live state for programs and the `pen` command.
//!
//! `arch::xhci` hands every decoded pen report to [`pointer`]: the [`hiddigi::Clicker`] turns it into the
//! system pointer's position, buttons and wheel (tip = left with a pressure threshold, lower barrel =
//! right, upper = middle or scroll, tap freeze, double-tap snap), so the sign-in screen, the desktop, its
//! menus and windows and every app click with the pen exactly as with a mouse. Hover (in range, not
//! touching) moves the pointer without clicking. The pen maps to the whole screen.
//!
//! Programs read pressure, tilt and the rest through the `os_pen_*` builtins ([`dispatch`]); the SSH / console
//! `pen` command ([`command`]) shows the same live state, for checking a pen display from a photo.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::drivers::hiddigi::{self, Clicker, Device, PenState, PointerOut};

/// The last pen sample and what it did to the pointer.
#[derive(Clone, Copy, Default)]
struct Live {
    s: PenState,
    /// where the pen points, screen pixels (not frozen)
    sx: f32,
    sy: f32,
    /// what the system pointer got
    out: PointerOut,
    reports: u64,
    last_ms: u64,
}

static mut CLICKER: Clicker = Clicker::NEW;
static mut LIVE: Option<Live> = None;
/// the device the last report came from (name, ids, mode, pressure range)
static mut DEV: Option<Device> = None;

/// One decoded pen report at screen position (`fx`, `fy`) on a `h`-pixel-high screen: the pointer's
/// position / buttons / wheel, and the live state.
pub fn pointer(dev: &Device, s: &PenState, fx: f32, fy: f32, h: i32, now_ms: u64) -> PointerOut {
    let scale = h as f32 / 1080.0;
    let out = unsafe { (*(&raw mut CLICKER)).step(s, fx, fy, now_ms, scale) };
    unsafe {
        let reports = (*(&raw const LIVE)).map_or(0, |l| l.reports) + 1;
        *(&raw mut LIVE) = Some(Live { s: *s, sx: fx, sy: fy, out, reports, last_ms: now_ms });
        *(&raw mut DEV) = Some(*dev);
    }
    out
}

/// The pen was unplugged: nothing is in range any more.
pub fn removed() {
    unsafe {
        if let Some(l) = (*(&raw mut LIVE)).as_mut() {
            l.s.in_range = false;
            l.s.contact = false;
            l.out.buttons = 0;
        }
        *(&raw mut CLICKER) = Clicker::NEW;
    }
}

/// The model line (`Device::describe`) of the pen just set up, kept for the "first report" log line,
/// which must not allocate (it can be printed from inside a GC pass).
static mut LINE: String = String::new();

/// A pen was set up: remember its model line.
pub fn announce(dev: &Device) {
    unsafe {
        *(&raw mut LINE) = dev.describe();
        *(&raw mut DEV) = Some(*dev);
    }
}

/// The last announced pen's model line.
pub fn device_line() -> &'static [u8] {
    unsafe { (*(&raw const LINE)).as_bytes() }
}

fn live() -> Option<Live> {
    unsafe { *(&raw const LIVE) }
}

/// The `os_pen_*` builtins (and `os_dblclick_time`); `None` for any other name.
///
/// `os_pen_present()` a pen is plugged in; `os_pen_in_range()` it points (hovering or touching);
/// `os_pen_down()` the tip / eraser touches; `os_pen_pressure()` 0..1; `os_pen_tilt_x()` /
/// `os_pen_tilt_y()` degrees (+ = leaning right / toward the user); `os_pen_tilt()` 0 upright .. 1 flat;
/// `os_pen_azimuth()` / `os_pen_twist()` radians; `os_pen_eraser()` the eraser end is the one in use;
/// `os_pen_buttons()` bit 0 tip, 1 lower barrel, 2 upper barrel; `os_pen_distance()` hover height 0..1;
/// `os_pen_x()` / `os_pen_y()` where it points in the app window's pixels; `os_pen_state()` all of it as
/// one list [x, y, pressure, tilt_x, tilt_y, in_range, down, eraser, buttons, tilt, azimuth, twist, distance];
/// `os_dblclick_time()` the system double-click time in seconds.
pub fn dispatch(name: &[u8], _args: &[u64]) -> Option<u64> {
    use crate::lingrt::{boolean, list_from, num};
    if name == b"os_dblclick_time" {
        return Some(num(hiddigi::DBL_CLICK_MS as f64 / 1000.0));
    }
    let rest = name.strip_prefix(b"os_pen_")?;
    let l = live().unwrap_or_default();
    let s = l.s;
    let near = s.in_range || s.contact;
    let pen_buttons = |s: &PenState| (s.contact as u8) | (s.barrel as u8) << 1 | (s.barrel2 as u8) << 2;
    let xy = || crate::lingapp::screen_to_app(l.sx as f64, l.sy as f64);
    Some(match rest {
        b"present" => boolean(hiddigi::present()),
        b"in_range" => boolean(near),
        b"down" | b"contact" => boolean(s.contact),
        b"pressure" => num(s.p as f64),
        b"tilt_x" => num(s.tx as f64),
        b"tilt_y" => num(s.ty as f64),
        b"tilt" => num(s.tilt as f64),
        b"azimuth" => num(s.az as f64),
        b"twist" => num(s.twist as f64),
        b"eraser" => boolean(s.eraser && near),
        b"buttons" => num(pen_buttons(&s) as f64),
        b"distance" => num(s.dist as f64),
        b"x" => num(xy().0),
        b"y" => num(xy().1),
        b"state" => {
            let (x, y) = xy();
            let v: Vec<u64> = alloc::vec![
                num(x),
                num(y),
                num(s.p as f64),
                num(s.tx as f64),
                num(s.ty as f64),
                boolean(near),
                boolean(s.contact),
                boolean(s.eraser && near),
                num(pen_buttons(&s) as f64),
                num(s.tilt as f64),
                num(s.az as f64),
                num(s.twist as f64),
                num(s.dist as f64),
            ];
            list_from(v)
        },
        _ => return None,
    })
}

fn line() -> String {
    let dev = unsafe { *(&raw const DEV) };
    let Some(l) = live() else {
        return String::from(if hiddigi::present() { "pen: plugged in, no report yet (bring the pen near the screen)\n" } else { "pen: no USB pen / pen display\n" });
    };
    let s = l.s;
    let b = |on: bool, t: &'static str| if on { t } else { "-" };
    format!(
        "{}\n  {} x {:.1} y {:.1}  pressure {:.3}  tilt x {:+.1} y {:+.1} ({:.2})  dist {:.2}\n  tip {}  lower {}  upper {}  eraser {}  -> pointer {:.0},{:.0} buttons {}{}  ({} reports, {} ms ago)\n",
        dev.map(|d| d.describe()).unwrap_or_default(),
        if s.contact { "TOUCHING" } else if s.in_range { "hovering" } else { "out of range" },
        l.sx,
        l.sy,
        s.p,
        s.tx,
        s.ty,
        s.tilt,
        s.dist,
        b(s.contact, "DOWN"),
        b(s.barrel, "DOWN"),
        b(s.barrel2, "DOWN"),
        b(s.eraser, "yes"),
        l.out.x,
        l.out.y,
        l.out.buttons,
        if l.out.wheel != 0 { format!(" wheel {}", l.out.wheel) } else { String::new() },
        l.reports,
        crate::arch::timer::now_ms().saturating_sub(l.last_ms),
    )
}

/// `pen` (the live state now) or `pen <seconds>` (a line whenever it changes, for up to a minute).
pub fn command(arg: &str) -> String {
    let secs = arg.trim().parse::<u64>().unwrap_or(0).min(60);
    if secs == 0 {
        return line();
    }
    let t0 = crate::arch::timer::now_ms();
    let mut out = String::new();
    let mut seen = u64::MAX;
    while crate::arch::timer::now_ms().saturating_sub(t0) < secs * 1000 && out.len() < 60_000 {
        let n = live().map_or(0, |l| l.reports);
        if n != seen {
            seen = n;
            out.push_str(&line());
        }
        // 10 lines a second at most
        let t = crate::arch::timer::now_ms();
        while crate::arch::timer::now_ms().saturating_sub(t) < 100 {
            crate::proc::sched::yield_now();
        }
    }
    out
}
