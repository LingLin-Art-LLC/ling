//! Symmetric multiprocessing: wake the machine's other cores (the application
//! processors, APs) and use them as a worker pool for data-parallel work --
//! 3-D rasterizing in screen bands, scaling and copying frames to the screen.
//!
//! The kernel itself stays single-core: the scheduler, the Ling runtime, the
//! collector, every driver and the heap run on the boot core (BSP) only. A
//! worker only ever runs a job function handed to [`parallel_for`] while the
//! BSP waits inside that same call, so the job may borrow anything the caller
//! could. Job functions must not allocate, print, take locks or call into the
//! Ling runtime: the heap's lock only masks interrupts on its own core.
//!
//! Bring-up: a 16-bit trampoline copied to 0x8000 (below 1 MiB, clear of the
//! disk-boot E820 / LFB handoffs at 0x5000 / 0x6000) goes real -> protected ->
//! long mode on the boot page tables, takes an index and a stack, and calls
//! [`ap_main`], which switches to the kernel's own page tables, GDT (with a
//! TSS per core for its fault stacks) and IDT, enables its local APIC and
//! waits for work. Wake-up is INIT-SIPI-SIPI broadcast to all other cores;
//! idle workers sleep in `hlt` and are woken by a fixed IPI.
use super::{cpu, gdt, idt, paging, timer};
use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Logical processors (hyperthreads count) including the boot core. Cores
/// past this stay parked in `hlt`. Was 16 -- exactly an i9-11900K's 16
/// threads, so any bigger part lost cores; the per-core arrays are small.
pub const MAX_CPUS: usize = 64;
pub const MAX_APS: usize = MAX_CPUS - 1;
/// The vector of the "work is ready" IPI (idt.rs installs its handler).
pub const WAKE_VECTOR: u8 = 0xF0;
const TRAMP: u64 = 0x8000;
const AP_STACK: usize = 256 * 1024;
const FAULT_STACK: usize = 16 * 1024;

const LAPIC_ID: u64 = 0x20;
const LAPIC_TPR: u64 = 0x80;
const LAPIC_EOI: u64 = 0xB0;
const LAPIC_SVR: u64 = 0xF0;
const LAPIC_ICR_LO: u64 = 0x300;
const LAPIC_ICR_HI: u64 = 0x310;
const LAPIC_LVT_TIMER: u64 = 0x320;
const LAPIC_LINT0: u64 = 0x350;
const LAPIC_LINT1: u64 = 0x360;
const LAPIC_LVT_ERR: u64 = 0x370;

global_asm!(
    r#"
.section .rodata
.set LAPT_COUNT, ling_ap_count - ling_ap_tramp_start
.set LAPT_CR3, ling_ap_cr3 - ling_ap_tramp_start
.set LAPT_ENTRY, ling_ap_entry - ling_ap_tramp_start
.set LAPT_GDTP, ling_ap_gdtp - ling_ap_tramp_start
.set LAPT_STACKS, ling_ap_stacks - ling_ap_tramp_start
.set LAPT_LIMIT, ling_ap_limit - ling_ap_tramp_start
.align 16
.global ling_ap_tramp_start
ling_ap_tramp_start:
.code16
    cli
    cld
    xor ax, ax
    mov ds, ax
    lgdt [0x8000 + LAPT_GDTP]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    // jmp dword 0x08:ap_pm (66 EA = far jump with a 32-bit offset in 16-bit code)
    .byte 0x66, 0xEA
    .long 0x8000 + (ling_ap_pm - ling_ap_tramp_start)
    .short 0x08
.code32
ling_ap_pm:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax
    // PAE + OSFXSR + OSXMMEXCPT
    mov eax, cr4
    or eax, 0x620
    mov cr4, eax
    mov eax, [0x8000 + LAPT_CR3]
    mov cr3, eax
    // EFER: long mode + no-execute (the kernel's tables use NX bits)
    mov ecx, 0xC0000080
    rdmsr
    or eax, 0x900
    wrmsr
    // paging + write-protect + MP, no x87 emulation
    mov eax, cr0
    and eax, 0xFFFFFFFB
    or eax, 0x80010002
    mov cr0, eax
    .byte 0xEA
    .long 0x8000 + (ling_ap_lm - ling_ap_tramp_start)
    .short 0x18
.code64
ling_ap_lm:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov eax, 1
    lock xadd dword ptr [0x8000 + LAPT_COUNT], eax
    // only as many cores as init() made stacks for; the rest park below
    cmp eax, dword ptr [0x8000 + LAPT_LIMIT]
    jae 2f
    mov rsp, qword ptr [0x8000 + LAPT_STACKS + rax*8]
    mov edi, eax
    mov rax, qword ptr [0x8000 + LAPT_ENTRY]
    call rax
2:
    cli
    hlt
    jmp 2b
.align 8
ling_ap_gdt:
    .quad 0
    .quad 0x00CF9A000000FFFF
    .quad 0x00CF92000000FFFF
    .quad 0x00AF9A000000FFFF
ling_ap_gdtp:
    .short 31
    .long 0x8000 + (ling_ap_gdt - ling_ap_tramp_start)
.align 8
.global ling_ap_cr3
ling_ap_cr3:
    .quad 0
.global ling_ap_entry
ling_ap_entry:
    .quad 0
.global ling_ap_count
ling_ap_count:
    .quad 0
.global ling_ap_limit
ling_ap_limit:
    .quad 0
.global ling_ap_stacks
ling_ap_stacks:
    .skip 8 * {aps}
.global ling_ap_tramp_end
ling_ap_tramp_end:
.text
"#,
    aps = const MAX_APS,
);

extern "C" {
    static ling_ap_tramp_start: u8;
    static ling_ap_tramp_end: u8;
    static ling_ap_cr3: u8;
    static ling_ap_entry: u8;
    static ling_ap_count: u8;
    static ling_ap_stacks: u8;
    static ling_ap_limit: u8;
    static ling_boot_pml4: u8;
}

static mut LAPIC: u64 = 0;
/// Application processors that reached `ap_main` and are waiting for work.
static ONLINE: AtomicUsize = AtomicUsize::new(0);
/// How many workers may take part (`cores N`); starts at every online AP.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
/// The current job: generation (24 bits) | task count (20) | next task (20).
static JOB: AtomicU64 = AtomicU64::new(0);
static JOB_FN: AtomicUsize = AtomicUsize::new(0);
static JOB_CTX: AtomicUsize = AtomicUsize::new(0);
static DONE: AtomicUsize = AtomicUsize::new(0);
static BUSY: AtomicBool = AtomicBool::new(false);
static mut GEN: u64 = 0;
/// Per-worker tasks done (index 0 = the boot core), for `cores` / the profiler.
static TASKS_BY: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static JOBS: AtomicU64 = AtomicU64::new(0);
static JOB_US: AtomicU64 = AtomicU64::new(0);

const MASK20: u64 = (1 << 20) - 1;

#[inline]
fn abs_sym(s: &u8) -> u64 {
    s as *const u8 as u64
}

/// The local APICs run in x2APIC mode (registers are MSRs 0x800 + reg/16,
/// not memory). Firmware on many-core machines often leaves it on, and once on
/// it stays on until reset, so every core follows the boot core's mode.
static mut X2: bool = false;

unsafe fn lapic_read(reg: u64) -> u32 {
    if X2 {
        cpu::read_msr(0x800 + (reg >> 4) as u32) as u32
    } else {
        core::ptr::read_volatile((LAPIC + reg) as *const u32)
    }
}

unsafe fn lapic_write(reg: u64, v: u32) {
    if X2 {
        cpu::write_msr(0x800 + (reg >> 4) as u32, v as u64);
    } else {
        core::ptr::write_volatile((LAPIC + reg) as *mut u32, v);
    }
}

unsafe fn icr_wait() {
    if X2 {
        return; // x2APIC has no delivery-status bit: the MSR write is the send
    }
    let mut n = 0u32;
    while lapic_read(LAPIC_ICR_LO) & (1 << 12) != 0 && n < 1_000_000 {
        cpu::pause();
        n += 1;
    }
}

/// Send an IPI to every core but this one (destination shorthand 0b11).
unsafe fn ipi_others(low: u32) {
    let low = low | (0b11 << 18);
    if X2 {
        cpu::write_msr(0x830, low as u64);
        return;
    }
    icr_wait();
    lapic_write(LAPIC_ICR_HI, 0);
    lapic_write(LAPIC_ICR_LO, low);
    icr_wait();
}

/// Wake every sleeping worker (they re-read the HWP request and temperature).
pub fn kick() {
    unsafe {
        if X2 || LAPIC != 0 {
            ipi_others(WAKE_VECTOR as u32);
        }
    }
}

/// End-of-interrupt for the wake IPI (called from its handler in idt.rs).
pub fn eoi() {
    unsafe {
        if X2 || LAPIC != 0 {
            lapic_write(LAPIC_EOI, 0);
        }
    }
}

/// This core's local APIC id.
pub fn apic_id() -> u32 {
    unsafe {
        if X2 {
            lapic_read(LAPIC_ID)
        } else if LAPIC != 0 {
            lapic_read(LAPIC_ID) >> 24
        } else {
            cpu::cpuid(1, 0).1 >> 24
        }
    }
}

/// Whether the APICs run in x2APIC mode.
pub fn x2apic() -> bool {
    unsafe { X2 }
}

/// Cores in use for parallel work, the boot core included (1 = no SMP).
pub fn cores() -> usize {
    ACTIVE.load(Ordering::Relaxed) + 1
}

/// Cores that came up at boot, the boot core included.
pub fn cores_online() -> usize {
    ONLINE.load(Ordering::Relaxed) + 1
}

/// Limit parallel work to `n` cores (1 = everything on the boot core again).
pub fn set_cores(n: usize) -> usize {
    let n = n.clamp(1, cores_online());
    ACTIVE.store(n - 1, Ordering::Relaxed);
    n
}

/// (jobs run, microseconds inside them, tasks per core) since boot.
pub fn stats() -> (u64, u64, [u64; MAX_CPUS]) {
    let mut by = [0u64; MAX_CPUS];
    for (i, t) in TASKS_BY.iter().enumerate() {
        by[i] = t.load(Ordering::Relaxed);
    }
    (JOBS.load(Ordering::Relaxed), JOB_US.load(Ordering::Relaxed), by)
}

// ── Per-core activity (LingSys, `cores`) ────────────────────────────────────
//
// A worker is busy while it runs a job (parallel_for tasks or the background
// slot) and idle while it waits for one (spinning, then halted). The boot core
// is busy except while it idle-waits: the frame-rate cap (lingapp pace_frame,
// framebuffer::present) and timer::poll_until's spins, both of which report
// through `note_idle`. Each core writes only its own slots (relaxed atomics,
// no locks); `sample()`, on the boot core, turns them into a load per window.

/// Microseconds each worker has spent inside jobs (index 0 unused).
static BUSY_US: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// now_us() + 1 when the worker's current job stretch began, 0 while it waits.
static BUSY_FROM: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Microseconds the boot core has spent idle-waiting.
static IDLE0_US: AtomicU64 = AtomicU64::new(0);
/// The last APERF / MPERF each core read on itself (0 = never / unreadable).
static APERF: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static MPERF: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// When each worker last read its temperature (us).
static TEMP_AT: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Results of the last window: load percent (0..100, u32::MAX = no window yet) and MHz (0 = unknown).
static LOAD: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; MAX_CPUS];
static MHZ: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// The sampler's previous readings (boot core only).
static mut WIN_T: u64 = 0;
static mut WIN_BUSY: [u64; MAX_CPUS] = [0; MAX_CPUS];
static mut WIN_A: [u64; MAX_CPUS] = [0; MAX_CPUS];
static mut WIN_M: [u64; MAX_CPUS] = [0; MAX_CPUS];
static mut LAST_KICK: u64 = 0;
/// The load window: at least this long between two samples.
const WINDOW_US: u64 = 500_000;

/// The boot core spent `us` idle-waiting (frame cap, hardware polls).
#[inline]
pub fn note_idle(us: u64) {
    IDLE0_US.fetch_add(us, Ordering::Relaxed);
}

/// A yield inside an idle wait took `t1 - t0`: the part to leave out of that
/// wait's idle time. Another task that ran did real work (and accounts for its
/// own waits); a yield that came straight back (nobody ready) stays idle.
#[inline]
pub fn yield_cost(t0: u64, t1: u64) -> u64 {
    let d = t1.saturating_sub(t0);
    if d < 20 { 0 } else { d }
}

/// This core reads its own APERF / MPERF (a worker: at the end of a job stretch).
#[inline]
fn note_perf(me: usize) {
    if let Some((a, m)) = super::cpudrv::aperf_mperf() {
        APERF[me].store(a, Ordering::Relaxed);
        MPERF[me].store(m, Ordering::Relaxed);
    }
}

#[inline]
fn stretch_begin(me: usize) -> u64 {
    let t = timer::now_us();
    BUSY_FROM[me].store(t + 1, Ordering::Relaxed);
    t
}

#[inline]
fn stretch_end(me: usize, t0: u64) {
    let t = timer::now_us();
    BUSY_US[me].fetch_add(t.saturating_sub(t0), Ordering::Relaxed);
    BUSY_FROM[me].store(0, Ordering::Relaxed);
    note_perf(me);
    // the temperature too, at most a few times a second (an MSR read)
    if t.saturating_sub(TEMP_AT[me].load(Ordering::Relaxed)) > 250_000 {
        TEMP_AT[me].store(t, Ordering::Relaxed);
        super::cpudrv::note_temp(me);
    }
}

/// Close the load window when it is due (boot core; cheap otherwise). Called
/// from the frame pacer, `os_info` and the `cores` command.
pub fn sample() {
    let now = timer::now_us();
    unsafe {
        let last = WIN_T;
        if last != 0 && now.saturating_sub(last) < WINDOW_US {
            return;
        }
        let n = cores_online().min(MAX_CPUS);
        let dt = now.saturating_sub(last).max(1);
        let base = super::cpudrv::base_mhz() as u64;
        for i in 0..n {
            // busy microseconds so far, a job still running included
            let b = if i == 0 {
                now.saturating_sub(IDLE0_US.load(Ordering::Relaxed))
            } else {
                let from = BUSY_FROM[i].load(Ordering::Relaxed);
                let done = BUSY_US[i].load(Ordering::Relaxed);
                done + if from != 0 { now.saturating_sub(from - 1) } else { 0 }
            };
            // never backwards (a stretch ending between the two reads above)
            let b = b.max(WIN_BUSY[i]);
            if last != 0 {
                let pct = (b - WIN_BUSY[i]) * 100 / dt;
                LOAD[i].store(pct.min(100), Ordering::Relaxed);
            }
            WIN_BUSY[i] = b;
            // effective clock while running: base x dAPERF / dMPERF
            if i == 0 {
                note_perf(0);
            }
            let (a, m) = (APERF[i].load(Ordering::Relaxed), MPERF[i].load(Ordering::Relaxed));
            if m != 0 && WIN_M[i] != 0 && m > WIN_M[i] && a > WIN_A[i] && base > 0 {
                MHZ[i].store(base * (a - WIN_A[i]) / (m - WIN_M[i]), Ordering::Relaxed);
            }
            if m != 0 {
                WIN_A[i] = a;
                WIN_M[i] = m;
            }
        }
        WIN_T = now;
    }
}

/// Core `i`'s load over the last window, 0..=100 (None: no such core, or no window yet).
pub fn core_load(i: usize) -> Option<u32> {
    if i >= cores_online().min(MAX_CPUS) {
        return None;
    }
    let v = LOAD[i].load(Ordering::Relaxed);
    if v == u64::MAX { None } else { Some(v as u32) }
}

/// Core `i`'s effective clock in MHz while it ran (None where APERF / MPERF
/// can't be read, e.g. in a virtual machine, or before the core ran a job).
pub fn core_mhz(i: usize) -> Option<u32> {
    if i >= cores_online().min(MAX_CPUS) {
        return None;
    }
    let v = MHZ[i].load(Ordering::Relaxed);
    if v == 0 { None } else { Some(v as u32) }
}

/// Have the sleeping workers read their temperatures again (a wake IPI at most
/// every two seconds; they go straight back to sleep).
pub fn refresh_temps() {
    let now = timer::now_us();
    unsafe {
        if now.saturating_sub(LAST_KICK) > 2_000_000 {
            LAST_KICK = now;
            kick();
        }
    }
}

/// Keep every core busy for about `ms` milliseconds in slices, core `i` at
/// roughly (i + 1) / n of the time, yielding between slices so the desktop
/// keeps drawing: a visible load pattern for `cores burn` and LingSys tests.
pub fn burn(ms: u64, mut between: impl FnMut()) {
    let n = cores();
    let end = timer::now_us() + ms.min(60_000) * 1000;
    while timer::now_us() < end {
        let slice = 20_000u64;
        parallel_for(n, &|i: usize| {
            let t0 = timer::now_us();
            let want = slice * (i as u64 + 1) / n as u64;
            while timer::now_us() - t0 < want {
                unsafe { cpu::pause() };
            }
        });
        between();
    }
}

/// Wake the other cores. Call once, after paging, the GDT/IDT and the heap
/// are up. Returns the number of cores now available (1 when there is no
/// local APIC, x2APIC-only firmware, or no other core answered).
pub fn init() -> usize {
    super::cpudrv::init();
    unsafe {
        let (_, _, _, edx) = cpu::cpuid(1, 0);
        if edx & (1 << 9) == 0 {
            crate::console_write(b"smp: no local APIC, one core\n");
            return 1;
        }
        let base = cpu::read_msr(0x1B);
        if base & (1 << 11) == 0 {
            cpu::write_msr(0x1B, base | (1 << 11));
        }
        X2 = base & (1 << 10) != 0;
        LAPIC = base & 0xF_FFFF_F000;
        if !X2 && LAPIC >= 1 << 32 {
            crate::console_write(b"smp: local APIC above 4 GiB, one core\n");
            return 1;
        }
        // The boot core's APIC must be software-enabled to send IPIs. If the
        // firmware left it off, turn it on in the standard virtual-wire setup
        // so the 8259 PIC's interrupts keep arriving through LINT0.
        let svr = lapic_read(LAPIC_SVR);
        if svr & 0x100 == 0 {
            lapic_write(LAPIC_LINT0, 0x700); // ExtINT
            lapic_write(LAPIC_LINT1, 0x400); // NMI
            lapic_write(LAPIC_SVR, 0x1FF);
        }

        // The other cores' stacks and fault stacks: allocated here, on the
        // boot core, since the heap is not safe to use from another core.
        let start = abs_sym(&ling_ap_tramp_start);
        let len = (abs_sym(&ling_ap_tramp_end) - start) as usize;
        core::ptr::copy_nonoverlapping(start as *const u8, TRAMP as *mut u8, len);
        let at = |label: &u8| (TRAMP + abs_sym(label) - start) as *mut u64;
        core::ptr::write_volatile(at(&ling_ap_cr3), abs_sym(&ling_boot_pml4));
        core::ptr::write_volatile(at(&ling_ap_entry), ap_main as *const () as usize as u64);
        core::ptr::write_volatile(at(&ling_ap_count), 0);
        // Stacks for every logical processor the firmware lists (ACPI MADT,
        // hyperthreads included), else what CPUID says one package holds --
        // never fewer than 15, so a missing or short table still starts a
        // 16-thread part. The INIT-SIPI broadcast wakes every enabled core.
        let madt = madt_cpus();
        let hint = match madt {
            Some((en, _)) if en > 0 => en,
            _ => super::cpudrv::topology().1 as usize,
        };
        let aps = hint.saturating_sub(1).clamp(15, MAX_APS);
        core::ptr::write_volatile(at(&ling_ap_limit), aps as u64);
        let stacks = at(&ling_ap_stacks);
        for i in 0..aps {
            let s: &'static mut [u8] = alloc::vec![0u8; AP_STACK].leak();
            let top = (s.as_ptr() as u64 + AP_STACK as u64) & !0xF;
            core::ptr::write_volatile(stacks.add(i), top);
            let f: &'static mut [u8] = alloc::vec![0u8; FAULT_STACK].leak();
            gdt::prepare_ap(i, (f.as_ptr() as u64 + FAULT_STACK as u64) & !0xF);
        }

        // INIT, then two STARTUPs at the trampoline's page (Intel MP spec).
        ipi_others(0x0000_4500);
        timer::delay_us(10_000);
        for _ in 0..2 {
            ipi_others(0x0000_4600 | (TRAMP >> 12) as u32);
            timer::delay_us(200);
        }
        // Wait until the count of started cores stops changing.
        let count = at(&ling_ap_count) as *const u32;
        let mut last = 0u32;
        let mut stable = 0u32;
        let t0 = timer::now_us();
        while timer::now_us() - t0 < 500_000 {
            timer::delay_us(5_000);
            let c = core::ptr::read_volatile(count);
            let online = ONLINE.load(Ordering::Acquire) as u32;
            if c == last && online >= c.min(aps as u32) {
                stable += 1;
                if stable >= 4 {
                    break;
                }
            } else {
                stable = 0;
            }
            last = c;
        }
        let started = core::ptr::read_volatile(count) as usize;
        let n = ONLINE.load(Ordering::Acquire);
        ACTIVE.store(n, Ordering::Relaxed);
        crate::console_write(b"smp: ");
        crate::print_decimal((n + 1) as u64);
        crate::console_write(b" cores working (boot core + ");
        crate::print_decimal(n as u64);
        crate::console_write(b" of ");
        crate::print_decimal(started as u64);
        crate::console_write(b" started); boot APIC id ");
        crate::print_decimal(apic_id() as u64);
        crate::console_write(if X2 { b" (x2APIC)".as_slice() } else { b" (xAPIC)".as_slice() });
        if let Some((en, listed)) = madt {
            crate::console_write(b"; ACPI MADT lists ");
            crate::print_decimal(en as u64);
            crate::console_write(b" enabled of ");
            crate::print_decimal(listed as u64);
            if en > n + 1 {
                crate::console_write(b" -- some did not start");
            }
        }
        crate::console_write(b"\n");
        n + 1
    }
}

/// Logical processors the ACPI MADT lists: (enabled or online-capable, all
/// entries). Local APIC (type 0) and x2APIC (type 9) entries; None without
/// an RSDP / MADT. Read once at boot, on the boot core.
fn madt_cpus() -> Option<(usize, usize)> {
    unsafe fn rd(addr: u64, len: usize) -> Option<&'static [u8]> {
        if addr == 0 || len == 0 || len > (1 << 20) {
            return None;
        }
        let a = paging::map_mmio(addr, len as u64)?;
        Some(core::slice::from_raw_parts(a as *const u8, len))
    }
    fn le32(b: &[u8], o: usize) -> u64 {
        if o + 4 <= b.len() { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as u64 } else { 0 }
    }
    fn le64(b: &[u8], o: usize) -> u64 {
        if o + 8 <= b.len() { le32(b, o) | (le32(b, o + 4) << 32) } else { 0 }
    }
    unsafe {
        // lingboot (UEFI) passes the RSDP on the command line; BIOS boots scan for it
        let from_cmdline = super::bootmodule::cmdline().and_then(|cl| {
            cl.split_whitespace().find_map(|t| {
                let v = t.strip_prefix("acpi=")?;
                match v.strip_prefix("0x") {
                    Some(h) => u64::from_str_radix(h, 16).ok(),
                    None => v.parse::<u64>().ok(),
                }
            })
        });
        let scan = |lo: u64, hi: u64| -> Option<u64> {
            let area = rd(lo, (hi - lo) as usize)?;
            (0..area.len().saturating_sub(8)).step_by(16).find(|&o| &area[o..o + 8] == b"RSD PTR ").map(|o| lo + o as u64)
        };
        let rsdp = match from_cmdline.filter(|&a| a != 0) {
            Some(a) => a,
            None => {
                let ebda = rd(0x40E, 2).map(|b| (u16::from_le_bytes([b[0], b[1]]) as u64) << 4).unwrap_or(0);
                let a = if (0x80000..0xA0000).contains(&ebda) { scan(ebda, ebda + 1024) } else { None };
                a.or_else(|| scan(0xE0000, 0x100000))?
            },
        };
        let p = rd(rsdp, 36)?;
        if &p[0..8] != b"RSD PTR " {
            return None;
        }
        let (root, wide) = if p[15] >= 2 && le64(p, 24) != 0 { (le64(p, 24), true) } else { (le32(p, 16), false) };
        let h = rd(root, 36)?;
        let t = rd(root, (le32(h, 4) as usize).clamp(36, 4096))?;
        let step = if wide { 8 } else { 4 };
        let mut o = 36;
        while o + step <= t.len() {
            let a = if wide { le64(t, o) } else { le32(t, o) };
            o += step;
            let Some(th) = rd(a, 36) else { continue };
            if &th[0..4] != b"APIC" {
                continue;
            }
            let m = rd(a, (le32(th, 4) as usize).clamp(44, 1 << 16))?;
            let (mut listed, mut en) = (0usize, 0usize);
            let mut q = 44;
            while q + 2 <= m.len() {
                let (ty, l) = (m[q], m[q + 1] as usize);
                if l < 2 {
                    break;
                }
                // flags bit 0: enabled (bit 1, online-capable, is hot-plug only)
                let flags = match ty {
                    0 => Some(le32(m, q + 4)),
                    9 => Some(le32(m, q + 8)),
                    _ => None,
                };
                if let Some(f) = flags {
                    listed += 1;
                    if f & 1 != 0 {
                        en += 1;
                    }
                }
                q += l;
            }
            return Some((en, listed));
        }
        None
    }
}

extern "C" fn ap_main(idx: u32) -> ! {
    unsafe {
        cpu::write_cr3(paging::kernel_pml4());
        if X2 {
            // follow the boot core into x2APIC mode (xAPIC first, then x2APIC)
            let b = cpu::read_msr(0x1B);
            if b & (1 << 10) == 0 {
                cpu::write_msr(0x1B, b | (1 << 11));
                cpu::write_msr(0x1B, b | (1 << 11) | (1 << 10));
            }
        }
        gdt::load_ap(idx as usize);
        idt::load();
        asm!("fninit", options(nomem, nostack));
        let mx: u32 = 0x1F80;
        asm!("ldmxcsr [{}]", in(reg) &mx, options(nostack, readonly));
        lapic_write(LAPIC_TPR, 0);
        lapic_write(LAPIC_LVT_TIMER, 0x1_0000);
        lapic_write(LAPIC_LINT0, 0x1_0000);
        lapic_write(LAPIC_LINT1, 0x1_0000);
        lapic_write(LAPIC_LVT_ERR, 0x1_0000);
        lapic_write(LAPIC_SVR, 0x1FF);
    }
    super::cpudrv::per_core(idx as usize + 1);
    super::cpudrv::note_temp(idx as usize + 1);
    ONLINE.fetch_add(1, Ordering::Release);
    worker(idx as usize + 1)
}

#[inline]
fn claimable(v: u64) -> bool {
    (v & MASK20) < ((v >> 20) & MASK20)
}

/// Take the next task of generation `gen`, or None when it has none left.
#[inline]
fn claim(gen: u64) -> Option<usize> {
    let mut v = JOB.load(Ordering::Acquire);
    loop {
        if v >> 40 != gen || !claimable(v) {
            return None;
        }
        match JOB.compare_exchange_weak(v, v + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some((v & MASK20) as usize),
            Err(now) => v = now,
        }
    }
}

#[inline]
unsafe fn run_task(i: usize) {
    let f: unsafe fn(usize, usize) = core::mem::transmute(JOB_FN.load(Ordering::Acquire));
    f(JOB_CTX.load(Ordering::Acquire), i);
}

// ── One background job: long work (a password hash) a worker runs while the boot core goes on ──
/// 0 free, 1 handed over, 2 a worker runs it, 3 done (the caller collects it).
static BG_STATE: AtomicUsize = AtomicUsize::new(0);
static BG_FN: AtomicUsize = AtomicUsize::new(0);
static BG_CTX: AtomicUsize = AtomicUsize::new(0);

unsafe fn bg_trampoline<F: Fn() + Sync>(ctx: usize, _i: usize) {
    (*(ctx as *const F))()
}

/// Run `f` on another core and return when it is done, calling `wait` meanwhile (a task yields
/// there, so the screen and the other tasks keep going while e.g. a 16 MiB Argon2id hash runs).
/// One such job at a time; a second caller waits its turn the same way. Without another core, `f`
/// runs right here. `f` has the job rules of [`parallel_for`]: no allocating, printing, locks or
/// Ling runtime -- whatever memory it needs, the caller allocates first.
pub fn run_elsewhere<F: Fn() + Sync>(f: &F, mut wait: impl FnMut()) {
    if ONLINE.load(Ordering::Acquire) == 0 {
        f();
        return;
    }
    while BG_STATE.compare_exchange(0, 4, Ordering::Acquire, Ordering::Relaxed).is_err() {
        wait();
    }
    BG_FN.store(bg_trampoline::<F> as unsafe fn(usize, usize) as usize, Ordering::Relaxed);
    BG_CTX.store(f as *const F as usize, Ordering::Relaxed);
    BG_STATE.store(1, Ordering::Release);
    unsafe { ipi_others(WAKE_VECTOR as u32) };
    while BG_STATE.load(Ordering::Acquire) != 3 {
        wait();
    }
    BG_STATE.store(0, Ordering::Release);
}

/// The same background slot without waiting: hand `func(ctx, 0)` to a worker
/// core and return at once (true), or false when there is no other core or
/// the slot is taken. The caller owns `ctx` until [`bg_finish`] says done, so
/// it must not live on a task's stack (a task can be killed while it waits).
/// The job has [`parallel_for`]'s rules, except that it may allocate when the
/// caller set up [`crate::mm::aparena`] for it.
pub fn bg_start(func: unsafe fn(usize, usize), ctx: usize) -> bool {
    if ONLINE.load(Ordering::Acquire) == 0 || BG_STATE.compare_exchange(0, 4, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return false;
    }
    BG_FN.store(func as usize, Ordering::Relaxed);
    BG_CTX.store(ctx, Ordering::Relaxed);
    BG_STATE.store(1, Ordering::Release);
    unsafe { ipi_others(WAKE_VECTOR as u32) };
    true
}

/// The job [`bg_start`] handed over is done (and the slot free again): true; still running: false.
/// Only the caller whose `bg_start` returned true may ask.
pub fn bg_finish() -> bool {
    BG_STATE.compare_exchange(3, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok()
}

// -- A background batch: n tasks the workers take while the boot core goes on -----------
//
// The screen recorder's H.264 slices. Workers prefer `parallel_for` jobs and
// take one batch task at a time, so a batch never holds up the desktop's own
// parallel work; the boot core never runs batch tasks.

/// generation (24 bits) | task count (20) | next task (20), as JOB
static BATCH: AtomicU64 = AtomicU64::new(0);
static BATCH_FN: AtomicUsize = AtomicUsize::new(0);
static BATCH_CTX: AtomicUsize = AtomicUsize::new(0);
/// Tasks of the current batch not finished yet (0 = none running).
static BATCH_LEFT: AtomicUsize = AtomicUsize::new(0);

/// Hand `func(ctx, i)` for every `i` in `0..n` to the worker cores and return
/// at once (true), or false when there is no worker or a batch still runs.
/// Tasks follow [`parallel_for`]'s rules (no allocating, printing or locks);
/// the caller owns `ctx` until [`batch_done`] says so.
pub fn batch_start(func: unsafe fn(usize, usize), ctx: usize, n: usize) -> bool {
    if n == 0 || n > MASK20 as usize || ONLINE.load(Ordering::Acquire) == 0 {
        return false;
    }
    if BATCH_LEFT.compare_exchange(0, n, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return false;
    }
    BATCH_FN.store(func as usize, Ordering::Relaxed);
    BATCH_CTX.store(ctx, Ordering::Relaxed);
    let gen = ((BATCH.load(Ordering::Relaxed) >> 40) + 1) & 0xFF_FFFF;
    BATCH.store((gen << 40) | ((n as u64) << 20), Ordering::Release);
    unsafe { ipi_others(WAKE_VECTOR as u32) };
    true
}

/// Every task of the batch started last has finished.
pub fn batch_done() -> bool {
    BATCH_LEFT.load(Ordering::Acquire) == 0
}

/// A worker runs one task of the current batch, if one is left.
fn batch_take(me: usize) -> bool {
    let mut v = BATCH.load(Ordering::Acquire);
    loop {
        if !claimable(v) {
            return false;
        }
        match BATCH.compare_exchange_weak(v, v + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(now) => v = now,
        }
    }
    let t0 = stretch_begin(me);
    unsafe {
        let f: unsafe fn(usize, usize) = core::mem::transmute(BATCH_FN.load(Ordering::Acquire));
        f(BATCH_CTX.load(Ordering::Acquire), (v & MASK20) as usize);
    }
    stretch_end(me, t0);
    TASKS_BY[me].fetch_add(1, Ordering::Relaxed);
    BATCH_LEFT.fetch_sub(1, Ordering::Release);
    true
}

// -- Real idle on the boot core ------------------------------------------------
//
// The frame pacer used to spin out the wait for the next refresh slot
// (`pause` loops): a whole core at 100% doing nothing, two thirds of every
// 60 Hz frame. With the local APIC timer in TSC-deadline mode the boot core
// can `hlt` instead and be woken exactly at the deadline (or sooner by any
// other interrupt: keyboard, PS/2 mouse, the 100 Hz heartbeat). The timer's
// vector is the wake IPI's, whose handler only sends the EOI.

/// 0 = not tried yet, 1 = TSC-deadline timer armed on the boot core, 2 = unavailable.
static mut DEADLINE_STATE: u8 = 0;
const IA32_TSC_DEADLINE: u32 = 0x6E0;

unsafe fn deadline_setup() -> bool {
    if DEADLINE_STATE == 0 {
        DEADLINE_STATE = 2;
        let (_, _, ecx, _) = cpu::cpuid(1, 0);
        let lapic_on = X2 || LAPIC != 0;
        if ecx & (1 << 24) != 0 && lapic_on && on_boot_core() && lapic_read(LAPIC_SVR) & 0x100 != 0 {
            // TSC-deadline mode (LVT timer bits 18:17 = 10), unmasked
            lapic_write(LAPIC_LVT_TIMER, (0b10 << 17) | WAKE_VECTOR as u32);
            asm!("mfence", options(nostack));
            DEADLINE_STATE = 1;
        }
    }
    DEADLINE_STATE == 1
}

/// Idle the boot core for about `us` microseconds: halted until the APIC
/// timer (or any interrupt) wakes it, so it can come back early -- callers
/// loop on their own deadline. Falls back to a timed spin where the APIC
/// timer can't be used, on another core, or with interrupts off.
pub fn idle_us(us: u64) {
    if us == 0 {
        return;
    }
    unsafe {
        let flags = cpu::read_flags();
        if flags & (1 << 9) == 0 || !deadline_setup() || !on_boot_core() {
            timer::delay_us(us);
            return;
        }
        let target = cpu::rdtsc() + us * timer::tsc_mhz().max(1);
        cpu::cli();
        cpu::write_msr(IA32_TSC_DEADLINE, target);
        // `sti; hlt` is atomic (the interrupt shadow): a deadline already
        // past fires right after the sti and wakes the hlt at once
        asm!("sti; hlt", options(nomem, nostack));
        // woken early by something else: disarm, so a stale deadline can't
        // interrupt whatever runs next
        cpu::write_msr(IA32_TSC_DEADLINE, 0);
    }
}

/// Whether [`idle_us`] really halts (the APIC TSC-deadline timer is usable).
pub fn idle_halts() -> bool {
    unsafe { deadline_setup() }
}

/// Whether other cores can take background jobs.
pub fn has_workers() -> bool {
    ONLINE.load(Ordering::Acquire) > 0
}

/// Whether this core is the boot core (IA32_APIC_BASE bit 8, per core).
#[inline]
pub fn on_boot_core() -> bool {
    unsafe { cpu::read_msr(0x1B) & (1 << 8) != 0 }
}

/// A worker takes the handed-over background job, if there is one, and runs it.
fn bg_take(me: usize) -> bool {
    if BG_STATE.compare_exchange(1, 2, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return false;
    }
    let t0 = stretch_begin(me);
    unsafe {
        let f: unsafe fn(usize, usize) = core::mem::transmute(BG_FN.load(Ordering::Acquire));
        f(BG_CTX.load(Ordering::Acquire), 0);
    }
    stretch_end(me, t0);
    BG_STATE.store(3, Ordering::Release);
    true
}

fn worker(me: usize) -> ! {
    loop {
        let mut spins = 0u32;
        let v = loop {
            if BG_STATE.load(Ordering::Acquire) == 1 && bg_take(me) {
                spins = 0;
                continue;
            }
            let v = JOB.load(Ordering::Acquire);
            if claimable(v) && me <= ACTIVE.load(Ordering::Relaxed) {
                break v;
            }
            if me <= ACTIVE.load(Ordering::Relaxed).max(1) && batch_take(me) {
                spins = 0;
                continue;
            }
            spins += 1;
            if spins < 50_000 {
                unsafe { cpu::pause() };
            } else {
                // Sleep until the next wake IPI. `sti; hlt` is atomic (the
                // interrupt shadow), so an IPI sent after the check still wakes us.
                unsafe {
                    asm!("cli", options(nomem, nostack));
                    let v2 = JOB.load(Ordering::Acquire);
                    if (claimable(v2) && me <= ACTIVE.load(Ordering::Relaxed)) || BG_STATE.load(Ordering::Acquire) == 1 || claimable(BATCH.load(Ordering::Acquire)) {
                        asm!("sti", options(nomem, nostack));
                        continue;
                    }
                    asm!("sti; hlt", options(nomem, nostack));
                }
                spins = 0;
                // woken: take a new energy preference, note the temperature
                super::cpudrv::apply_hwp(me);
                super::cpudrv::note_temp(me);
            }
        };
        let gen = v >> 40;
        super::cpudrv::apply_hwp(me);
        let t0 = stretch_begin(me);
        while let Some(i) = claim(gen) {
            unsafe { run_task(i) };
            TASKS_BY[me].fetch_add(1, Ordering::Relaxed);
            DONE.fetch_add(1, Ordering::Release);
        }
        stretch_end(me, t0);
    }
}

unsafe fn trampoline<F: Fn(usize) + Sync>(ctx: usize, i: usize) {
    (*(ctx as *const F))(i)
}

/// Run `f(0) .. f(n-1)` spread over every working core (the boot core takes
/// part) and return when all are done. Falls back to a plain loop with one
/// core, one task, or when called from inside another job. `f` must not
/// allocate, print or call into the Ling runtime (see the module doc).
pub fn parallel_for<F: Fn(usize) + Sync>(n: usize, f: &F) {
    if n == 0 {
        return;
    }
    let n = n.min(MASK20 as usize);
    if n == 1 || ACTIVE.load(Ordering::Relaxed) == 0 || BUSY.swap(true, Ordering::Acquire) {
        for i in 0..n {
            f(i);
        }
        return;
    }
    let t0 = timer::now_us();
    let gen = unsafe {
        GEN = (GEN + 1) & 0xFF_FFFF;
        GEN
    };
    JOB_FN.store(trampoline::<F> as unsafe fn(usize, usize) as usize, Ordering::Relaxed);
    JOB_CTX.store(f as *const F as usize, Ordering::Relaxed);
    DONE.store(0, Ordering::Relaxed);
    JOB.store((gen << 40) | ((n as u64) << 20), Ordering::Release);
    unsafe { ipi_others(WAKE_VECTOR as u32) };
    while let Some(i) = claim(gen) {
        f(i);
        TASKS_BY[0].fetch_add(1, Ordering::Relaxed);
        DONE.fetch_add(1, Ordering::Release);
    }
    while DONE.load(Ordering::Acquire) < n {
        unsafe { cpu::pause() };
    }
    BUSY.store(false, Ordering::Release);
    JOBS.fetch_add(1, Ordering::Relaxed);
    JOB_US.fetch_add(timer::now_us().saturating_sub(t0), Ordering::Relaxed);
}
