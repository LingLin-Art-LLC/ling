//! Data-parallel helpers over the x86-64 worker cores (`arch::x86_64::smp`).
//! On the Raspberry Pi (whose other cores stay parked) everything runs in
//! order on the boot core, so callers never branch on the platform.
//!
//! Job closures run on other cores: they must not allocate, print, or call
//! into the Ling runtime, the scheduler or a driver. Pure pixel work only.

/// Cores in use for parallel work (1 = none besides the boot core).
pub fn cores() -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        crate::arch::smp::cores()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        1
    }
}

/// `f(0) .. f(n-1)`, spread over the working cores; returns when all are done.
#[inline]
pub fn for_each<F: Fn(usize) + Sync>(n: usize, f: &F) {
    #[cfg(target_arch = "x86_64")]
    {
        // only the boot core hands out jobs: a background job (video decode,
        // a run_elsewhere hash) that reaches a parallel helper runs it in order
        if n > 1 && !crate::arch::smp::on_boot_core() {
            for i in 0..n {
                f(i);
            }
            return;
        }
        crate::arch::smp::parallel_for(n, f)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        for i in 0..n {
            f(i);
        }
    }
}

/// Split `rows` rows into chunks for [`for_each`]: a few per core so a busy
/// band doesn't leave the others idle. Returns (chunks, rows per chunk).
pub fn row_chunks(rows: usize, min_rows: usize) -> (usize, usize) {
    let c = cores();
    if c <= 1 || rows < min_rows * 2 {
        return (1, rows.max(1));
    }
    let want = (c * 4).min(rows / min_rows.max(1)).max(1);
    let per = rows.div_ceil(want);
    (rows.div_ceil(per), per)
}

/// A raw pointer that may be shared with the worker cores. Each job must only
/// touch the part of the memory that is its own (disjoint rows).
#[derive(Clone, Copy)]
pub struct Shared<T>(pub *mut T);
unsafe impl<T> Sync for Shared<T> {}
unsafe impl<T> Send for Shared<T> {}

impl<T> Shared<T> {
    /// The pointer (a method, so closures capture the whole `Sync` wrapper,
    /// not the raw field).
    #[inline(always)]
    pub fn get(&self) -> *mut T {
        self.0
    }
}

/// The `cores` command (console / SSH): show, or limit, the cores used.
pub fn command(arg: &str) -> alloc::string::String {
    use alloc::format;
    #[cfg(target_arch = "x86_64")]
    {
        let a = arg.trim();
        if let Ok(n) = a.parse::<usize>() {
            let got = crate::arch::smp::set_cores(n);
            return format!("cores: parallel work now uses {got} of {} cores\n", crate::arch::smp::cores_online());
        }
        // `cores burn [secs]`: load every core in a staircase (core i ~ (i+1)/n), yielding between
        // slices so the desktop (and LingSys's tile map) keeps drawing; then show the table
        if let Some(rest) = a.strip_prefix("burn") {
            let secs = rest.trim().parse::<u64>().unwrap_or(5).clamp(1, 60);
            crate::arch::smp::burn(secs * 1000, || crate::proc::sched::yield_now());
            return format!("cores: burned {secs} s\n{}", load_table());
        }
        let (jobs, us, by) = crate::arch::smp::stats();
        let mut s = format!(
            "cores: {} in use of {} online; {} parallel jobs so far, {:.1} ms inside them\n  tasks per core:",
            crate::arch::smp::cores(),
            crate::arch::smp::cores_online(),
            jobs,
            us as f64 / 1000.0
        );
        for (i, t) in by.iter().enumerate().take(crate::arch::smp::cores_online()) {
            s.push_str(&format!(" #{i}={t}"));
        }
        s.push('\n');
        s.push_str(&load_table());
        s.push_str("  `cores N` limits parallel work to N cores (1 = boot core only); `cores burn [secs]` loads them\n");
        s
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = arg;
        format!("cores: 1 (the other cores are parked on this board)\n")
    }
}

/// Per-core busy percent over the last ~0.5 s window, clock and temperature
/// where readable: one line per logical CPU (x86-64).
#[cfg(target_arch = "x86_64")]
fn load_table() -> alloc::string::String {
    use crate::arch::smp;
    use alloc::format;
    smp::sample();
    let temps = crate::arch::cpudrv::core_temps();
    let n = smp::cores_online();
    let mut s = alloc::string::String::from("  per-core load (last ~0.5 s window; boot core = time not idle-waiting):\n");
    let mut total = 0u32;
    for i in 0..n {
        let load = smp::core_load(i);
        total += load.unwrap_or(0);
        let bar_n = load.unwrap_or(0) as usize / 5;
        let mut bar = alloc::string::String::new();
        for k in 0..20 {
            bar.push(if k < bar_n { '#' } else { '.' });
        }
        s.push_str(&format!(
            "  cpu{:<3} {:>4} [{}]{}{}{}\n",
            i,
            load.map(|v| format!("{v}%")).unwrap_or_else(|| alloc::string::String::from("--")),
            bar,
            smp::core_mhz(i).map(|m| format!("  {m} MHz")).unwrap_or_default(),
            temps.get(i).filter(|&&t| t > 0).map(|t| format!("  {t} C")).unwrap_or_default(),
            if i == 0 { "  (boot)" } else { "" }
        ));
    }
    s.push_str(&format!("  average {}%\n", total / n.max(1) as u32));
    #[cfg(target_arch = "x86_64")]
    s.push_str(if crate::arch::smp::idle_halts() {
        "  boot core idle: halts (APIC TSC-deadline wake), workers halt between jobs\n"
    } else {
        "  boot core idle: spins (no APIC TSC-deadline timer), workers halt between jobs\n"
    });
    s
}
