//! Stall recorder: what held the desktop when a frame came late.
//!
//! The screen froze for half a second every few seconds on the user's PC and
//! nothing said why. Now every frame is timed from one present to the next;
//! a frame later than [`HITCH_US`] is logged with the parts of the frame loop
//! (the app's own code, the collector, the isolates, the background tasks...)
//! and the longest single things that ran in it ([`span`]: a builtin call, a
//! coroutine's turn, a task's turn, a disk write, a database flush, a USB
//! port scan, DHCP, the clock sync). Always on: a span costs two timer reads,
//! and only spans over [`SPAN_MIN_US`] are kept. `hitches` (Terminal / SSH)
//! lists the last ones; each also goes to the console as one line.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// A frame this late is a hitch.
const HITCH_US: u64 = 150_000;
/// Shorter spans are not worth naming.
const SPAN_MIN_US: u64 = 10_000;
const TOP: usize = 4;
const KEEP: usize = 24;

#[derive(Clone, Copy)]
struct Span {
    name: [u8; 40],
    len: u8,
    us: u64,
}

const EMPTY: Span = Span { name: [0; 40], len: 0, us: 0 };

static mut SPANS: [Span; TOP] = [EMPTY; TOP];
static mut LAST_END_US: u64 = 0;
static mut LOG: Vec<String> = Vec::new();
static mut TOTAL: u64 = 0;
/// When the previous hitch ended (us): a stall that comes back on a beat
/// shows the beat ("2.5 s after the last").
static mut LAST_HITCH_US: u64 = 0;
/// The firmware's SMI count and the 100 Hz timer's tick count at the end of
/// the last frame (x86): see [`machine_note`].
static mut SMI_AT: u64 = 0;
static mut TICKS_AT: u64 = 0;
/// The longest frame since [`worst_reset`] (us), hitch or not: what a job's
/// test reports as "the desktop's worst frame while it ran".
static mut WORST_US: u64 = 0;

/// Start measuring the worst frame afresh.
pub fn worst_reset() {
    unsafe { WORST_US = 0 };
}

/// The longest frame (us) since [`worst_reset`].
pub fn worst_us() -> u64 {
    unsafe { WORST_US }
}

/// What the machine itself did during a late frame, outside any code of ours:
/// System Management Interrupts (the firmware stopping every core), and timer
/// interrupts that never came (interrupts held off that long). Also moves the
/// counters on for the next frame.
fn machine_note(total_us: u64, late: bool) -> String {
    #[cfg(target_arch = "x86_64")]
    {
        let smi = crate::arch::cpudrv::smi_count();
        let ticks = crate::arch::trap::ticks();
        let (smi0, ticks0) = unsafe { (SMI_AT, TICKS_AT) };
        unsafe {
            SMI_AT = smi.unwrap_or(0);
            TICKS_AT = ticks;
        }
        if !late {
            return String::new();
        }
        let mut t = String::new();
        if let Some(n) = smi {
            let d = n.wrapping_sub(smi0) & 0xFFFF_FFFF;
            if smi0 != 0 && d > 0 {
                t.push_str(&format!(" | firmware SMIs {d}"));
            }
        }
        // Far fewer heartbeat ticks than usual means interrupts were off (a
        // long cli section) for most of the frame. "Usual" is this machine's
        // own long-run tick rate, not the nominal 100 Hz: on the Z590 every
        // late frame showed about a third of 100 Hz, which is the rate the
        // timer really delivers there, not cli.
        let up = crate::arch::timer::now_us().max(1);
        let want = (total_us as u128 * ticks as u128 / up as u128) as u64;
        let got = ticks.wrapping_sub(ticks0);
        if ticks0 != 0 && want >= 10 && got < want / 2 {
            t.push_str(&format!(" | timer IRQs {got} of ~{want} (interrupts held off)"));
        }
        t
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (total_us, late);
        String::new()
    }
}

/// Something that just took `us`: kept when it is one of this frame's longest.
pub fn span(name: &[u8], us: u64) {
    if us < SPAN_MIN_US {
        return;
    }
    span_parts(&[name], us);
}

/// [`span`] with the name in pieces ("coro:", the coroutine's name).
pub fn span_parts(parts: &[&[u8]], us: u64) {
    if us < SPAN_MIN_US {
        return;
    }
    let spans = unsafe { &mut *(&raw mut SPANS) };
    // the slot with the shortest span
    let mut k = 0;
    for i in 1..TOP {
        if spans[i].us < spans[k].us {
            k = i;
        }
    }
    if spans[k].us >= us {
        return;
    }
    let mut s = EMPTY;
    let mut n = 0;
    for p in parts {
        for &b in p.iter() {
            if n < s.name.len() {
                s.name[n] = b;
                n += 1;
            }
        }
    }
    s.len = n as u8;
    s.us = us;
    spans[k] = s;
}

/// Run `f`, recording it as a span named `name`.
#[inline]
pub fn timed<R>(name: &[u8], f: impl FnOnce() -> R) -> R {
    let t0 = crate::arch::timer::now_us();
    let r = f();
    span(name, crate::arch::timer::now_us().saturating_sub(t0));
    r
}

/// The frame loop reached `present` at `start` (us); `phases` are the parts
/// of present's own prologue already timed. Logs a hitch when this frame
/// (from the last present's end) ran long, then starts the next frame.
pub fn frame(start: u64, phases: &[(&str, u64)]) {
    let last = unsafe { LAST_END_US };
    let now = crate::arch::timer::now_us();
    unsafe { LAST_END_US = now };
    let spans = unsafe { &mut *(&raw mut SPANS) };
    if last == 0 {
        *spans = [EMPTY; TOP];
        return;
    }
    let total = now.saturating_sub(last);
    unsafe { WORST_US = WORST_US.max(total) };
    let machine = machine_note(total, total >= HITCH_US);
    if total >= HITCH_US {
        let mut line = format!("hitch: {} ms -- app {}", total / 1000, start.saturating_sub(last) / 1000);
        for (name, us) in phases {
            if *us >= 1000 {
                line.push_str(&format!(", {name} {}", us / 1000));
            }
        }
        let mut order: Vec<Span> = spans.iter().copied().filter(|s| s.us > 0).collect();
        order.sort_unstable_by(|a, b| b.us.cmp(&a.us));
        if !order.is_empty() {
            line.push_str(" | longest:");
            for s in &order {
                line.push_str(&format!(" {} {} ms;", String::from_utf8_lossy(&s.name[..s.len as usize]), s.us / 1000));
            }
        }
        line.push_str(&machine);
        let prev = unsafe { LAST_HITCH_US };
        if prev != 0 {
            let gap = last.saturating_sub(prev);
            line.push_str(&format!(" | {}.{} s after the last", gap / 1_000_000, gap / 100_000 % 10));
        }
        unsafe { LAST_HITCH_US = now };
        let up = crate::arch::timer::now_ms() / 1000;
        let entry = format!("[{:>5}s] {line}", up);
        crate::console_write(format!("{line}\n").as_bytes());
        let log = unsafe { &mut *(&raw mut LOG) };
        if log.len() >= KEEP {
            log.remove(0);
        }
        log.push(entry);
        unsafe { TOTAL += 1 };
    }
    *spans = [EMPTY; TOP];
}

/// `hitches`: the last late frames, oldest first.
pub fn command(arg: &str) -> String {
    let log = unsafe { &mut *(&raw mut LOG) };
    if arg.trim() == "clear" {
        log.clear();
        return String::from("hitches: cleared\n");
    }
    let mut out = format!(
        "frames later than {} ms since boot: {} (the last {} below; times in ms, 'app' = the program's own code between frames)\n",
        HITCH_US / 1000,
        unsafe { TOTAL },
        log.len()
    );
    for l in log.iter() {
        out.push_str(l);
        out.push('\n');
    }
    out
}
