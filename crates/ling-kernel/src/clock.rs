//! Wall-clock time (seconds since 1970) for everything that needs a date:
//! Ling's `now()`, the wallet's P2P layer, signed lists with an issue date.
//!
//! Sources, best first: the network (SNTP, asked once DHCP gives us an
//! address), the board's clock (the PC's CMOS RTC; the Pi 5's firmware RTC,
//! which only keeps time with a backup battery and otherwise restarts at
//! 1970), and a floor near this kernel's build -- a clock that reads earlier
//! than the software it runs makes every recently signed thing look like it
//! is from the future (the wallet called its seed list "Unverified").

use crate::arch::timer::now_ms;

/// Never earlier than this: 2026-10-30 00:00 UTC, about when this was written.
const FLOOR: u64 = 1_793_318_400;

/// The network time, once a time server answered: the last answer (unix ms) and the uptime (us)
/// it came at, the first answer (for the longest baseline), and `scale` -- how many real
/// microseconds one microsecond of the CPU timer is. The timer's rate is estimated at boot
/// (PIT, or the CPU's own figure when the board gates the PIT off) and can be well off: on the
/// user's Z590 (2026-11-25) the clock ran about 56 min fast within hours of syncing, and every
/// two-factor code failed. Asking the server again and comparing measures the real rate.
#[derive(Clone, Copy)]
struct Net {
    at_ms: u64,
    at_us: u64,
    first_ms: u64,
    first_us: u64,
    scale: f64,
    /// When the next ask is due (uptime ms, by the uncorrected timer).
    next_ms: u64,
}
static mut NET: Option<Net> = None;
static mut BOARD_BASE: Option<u64> = None;
static mut BOARD_TRIED: bool = false;

fn uptime_s() -> u64 {
    now_ms() / 1000
}

/// The board's own clock as unix seconds, if it has a believable one.
fn board_time() -> Option<u64> {
    #[cfg(target_arch = "x86_64")]
    {
        let t = crate::arch::rtc::unix_timestamp();
        return if t > 1_600_000_000 { Some(t as u64) } else { None };
    }
    #[cfg(target_arch = "aarch64")]
    {
        return crate::arch::mailbox::rtc_time().map(|t| t as u64);
    }
    #[allow(unreachable_code)]
    None
}

/// Milliseconds since 1970 from the network time, rate-corrected (None before a server answered).
fn net_now_ms() -> Option<u64> {
    let n = unsafe { NET }?;
    let us = crate::arch::timer::now_us().saturating_sub(n.at_us);
    Some(n.at_ms + (us as f64 * n.scale / 1000.0) as u64)
}

/// Seconds since 1970, UTC.
pub fn unix_now() -> u64 {
    let up = uptime_s();
    if let Some(ms) = net_now_ms() {
        return ms / 1000;
    }
    #[cfg(target_arch = "x86_64")]
    {
        // the PC's CMOS clock keeps running and is read fresh
        return board_time().unwrap_or(0).max(FLOOR + up);
    }
    #[allow(unreachable_code)]
    unsafe {
        if !BOARD_TRIED {
            BOARD_TRIED = true;
            BOARD_BASE = board_time().map(|t| t.saturating_sub(up));
        }
        BOARD_BASE.map(|b| b + up).unwrap_or(0).max(FLOOR + up)
    }
}

/// Whether the time came from the network (or a working board clock), not the floor.
pub fn is_set() -> bool {
    from_network() || board_time().is_some()
}

/// Ask the network for the time (SNTP) and use it from now on. Called when
/// DHCP brings an interface up; true when a server answered.
pub fn sync_from_network() -> bool {
    crate::hitch::timed(b"clock sync (SNTP)", sync_untimed)
}

/// Ask again 2 minutes after the first answer (a first rate), then every 15 minutes.
const RESYNC_FIRST_MS: u64 = 120_000;
const RESYNC_MS: u64 = 15 * 60_000;

fn sync_untimed() -> bool {
    let Some(t_ms) = crate::drivers::netstack::sntp_time_ms() else {
        if !from_network() {
            crate::console_write(b"clock: no time server answered -- using the board clock\n");
        }
        return false;
    };
    let us = crate::arch::timer::now_us();
    let before = net_now_ms();
    let prev = unsafe { NET };
    let n = match prev {
        None => Net { at_ms: t_ms, at_us: us, first_ms: t_ms, first_us: us, scale: 1.0, next_ms: now_ms() + RESYNC_FIRST_MS },
        Some(p) => {
            let (real, timer) = (t_ms.saturating_sub(p.first_ms) as f64 * 1000.0, us.saturating_sub(p.first_us) as f64);
            // a baseline under a minute is too short to trust (the server answer is good to ~ms)
            let scale = if real >= 60_000_000.0 && timer > 0.0 { (real / timer).clamp(0.1, 10.0) } else { p.scale };
            Net { at_ms: t_ms, at_us: us, first_ms: p.first_ms, first_us: p.first_us, scale, next_ms: now_ms() + RESYNC_MS }
        },
    };
    unsafe { NET = Some(n) };
    match (prev, before) {
        (None, _) => {
            let mut buf = [0u8; 48];
            let len = fmt_utc(t_ms / 1000, &mut buf);
            crate::console_write(b"clock: set from the network: ");
            crate::console_write(&buf[..len]);
            crate::console_write(b" UTC\n");
        },
        (Some(p), Some(b)) => {
            let off = t_ms as i64 - b as i64;
            // say so when the timer turned out off, or the clock had drifted noticeably
            if (n.scale - p.scale).abs() > 0.0002 || off.abs() > 1000 {
                crate::console_write(
                    alloc::format!(
                        "clock: resynced, the clock was {} ms {}; the CPU timer runs {:+.3}% vs real time (corrected)\n",
                        off.abs(),
                        if off > 0 { "slow" } else { "fast" },
                        (1.0 / n.scale - 1.0) * 100.0
                    )
                    .as_bytes(),
                );
            }
        },
        _ => {},
    }
    true
}

/// How the clock stands, for `date`-style reports: (from network, timer rate error in %, last
/// server answer age in s).
pub fn status() -> (bool, f64, u64) {
    match unsafe { NET } {
        Some(n) => (true, (1.0 / n.scale - 1.0) * 100.0, (crate::arch::timer::now_us().saturating_sub(n.at_us) as f64 * n.scale / 1_000_000.0) as u64),
        None => (false, 0.0, 0),
    }
}

/// Whether the time came from a time server (not the board clock or the floor).
/// A PC's CMOS clock is often hours off -- Windows and Linux set to "RTC in local
/// time" keep local time in it, and one seen 2026-11-25 was 18 h behind UTC --
/// so anything that needs the real time (two-factor codes) should check this.
pub fn from_network() -> bool {
    unsafe { (*(&raw const NET)).is_some() }
}

/// While the network has not given us the time: try again now and then -- once
/// a minute for the first ten tries (the first can come before the link or DNS
/// works), then every ten minutes. Called about once a second (startup::tick,
/// the Wi-Fi poll); the try itself runs on its own coroutine, since a failed
/// one waits out the DNS and time-server timeouts and must not stall the screen.
pub fn retry_sync() {
    static mut LAST_MS: u64 = 0;
    static mut TRIES: u32 = 0;
    static mut TASK: u64 = 0;
    if crate::drivers::netstack::self_ip() == [0; 4] {
        return;
    }
    let now = now_ms();
    // synced: ask again on schedule, to measure the timer's real rate and keep drift down
    if let Some(n) = unsafe { NET } {
        if now < n.next_ms || unsafe { TASK != 0 && crate::coro::alive(TASK) } {
            return;
        }
        unsafe {
            // a failed ask tries again in a minute, not at once
            if let Some(m) = (*(&raw mut NET)).as_mut() {
                m.next_ms = now + 60_000;
            }
            TASK = crate::coro::spawn_owned("clock-sync", false, "system", alloc::boxed::Box::new(|| {
                sync_from_network();
            }));
        }
        return;
    }
    let gap = if unsafe { TRIES } < 10 { 60_000 } else { 600_000 };
    // the boot's own try (from the network setup) counts as the first
    if now < 30_000 || (unsafe { LAST_MS } != 0 && now.saturating_sub(unsafe { LAST_MS }) < gap) {
        return;
    }
    if unsafe { TASK != 0 && crate::coro::alive(TASK) } {
        return;
    }
    unsafe {
        LAST_MS = now;
        TRIES += 1;
        TASK = crate::coro::spawn_owned("clock-sync", false, "system", alloc::boxed::Box::new(|| {
            sync_from_network();
        }));
    }
}

/// Ask a time server right now unless the time already came from one (or the
/// last such ask was under 20 s ago). Blocks for the exchange: for a screen that
/// is about to need the real time, like setting up two-factor sign-in.
pub fn sync_if_unsure() -> bool {
    static mut LAST_MS: u64 = 0;
    // a server answer under a minute old is good to well under a second
    if from_network() && status().2 < 60 {
        return true;
    }
    let now = now_ms();
    if unsafe { LAST_MS } != 0 && now.saturating_sub(unsafe { LAST_MS }) < 20_000 {
        return false;
    }
    unsafe { LAST_MS = now };
    crate::drivers::netstack::self_ip() != [0; 4] && sync_from_network()
}

/// "YYYY-MM-DD HH:MM:SS" of the time now, UTC (for messages).
pub fn now_utc_string() -> alloc::string::String {
    let mut buf = [0u8; 48];
    let n = fmt_utc(unix_now(), &mut buf);
    alloc::string::String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// "YYYY-MM-DD HH:MM:SS" for `t`.
fn fmt_utc(t: u64, out: &mut [u8; 48]) -> usize {
    let days = (t / 86_400) as i64;
    let secs = t % 86_400;
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    let parts = [(y as u64, 4), (m as u64, 2), (d as u64, 2), (secs / 3600, 2), (secs / 60 % 60, 2), (secs % 60, 2)];
    let seps = [b'-', b'-', b' ', b':', b':', 0];
    let mut n = 0;
    for (i, &(v, w)) in parts.iter().enumerate() {
        for k in (0..w).rev() {
            out[n] = b'0' + ((v / 10u64.pow(k)) % 10) as u8;
            n += 1;
        }
        if seps[i] != 0 {
            out[n] = seps[i];
            n += 1;
        }
    }
    n
}
