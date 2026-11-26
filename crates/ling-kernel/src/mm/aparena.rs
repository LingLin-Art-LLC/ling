//! A heap of its own for long work a worker core does in the background
//! (decoding video: `drivers/videoplay.rs`). The kernel heap is single-core
//! (its lock only masks interrupts on the boot core), so a job on another core
//! must not touch it. While the arena is active, every allocation made on a
//! worker core comes from here instead, under a spin lock; whatever lies in
//! the arena goes back here when freed, on any core. The boot core keeps using
//! the kernel heap, so the rule for a job is: never free or grow memory the
//! boot core allocated (read it only), and hand results back as new
//! allocations (they are the arena's, and the boot core may free them).
//!
//! Power-of-two size classes with free lists over one block of frames: a
//! decoder asks for the same few sizes again and again, so nothing has to be
//! split or merged. A class with nothing free takes fresh space, else a bigger
//! free block whole. The arena goes back to the frame allocator once nothing
//! in it is in use and no job wants it.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static BASE: AtomicUsize = AtomicUsize::new(0);
static END: AtomicUsize = AtomicUsize::new(0);
static ORDER: AtomicUsize = AtomicUsize::new(0);
/// Route worker-core allocations here.
static ACTIVE: AtomicBool = AtomicBool::new(false);
static LOCK: AtomicBool = AtomicBool::new(false);
/// Blocks handed out and not freed yet.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static mut BUMP: usize = 0;
const CLASSES: usize = 48;
static mut FREE: [usize; CLASSES] = [0; CLASSES];
/// Further blocks of frames after the first (the frame allocator's biggest piece can be
/// smaller than a 1080p decoder needs): base and order of each. The bump pointer moves on to
/// the next block when one is used up (`CUR` = the block it is in, 0 = the first).
const MAX_EXTRA: usize = 7;
static EXTRA_N: AtomicUsize = AtomicUsize::new(0);
static EXTRA_BASE: [AtomicUsize; MAX_EXTRA] = [const { AtomicUsize::new(0) }; MAX_EXTRA];
static EXTRA_ORDER: [AtomicUsize; MAX_EXTRA] = [const { AtomicUsize::new(0) }; MAX_EXTRA];
static mut CUR: usize = 0;
/// Header before each pointer: [block address, class].
const HDR: usize = 16;
const MIN_CLASS: usize = 5;

struct Spin;
impl Spin {
    fn take() -> Spin {
        while LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
        Spin
    }
}
impl Drop for Spin {
    fn drop(&mut self) {
        LOCK.store(false, Ordering::Release);
    }
}

/// Whether this allocation should come from the arena (a worker core, while active).
#[inline]
pub fn routes_here() -> bool {
    if !ACTIVE.load(Ordering::Acquire) {
        return false;
    }
    #[cfg(target_arch = "x86_64")]
    {
        !crate::arch::smp::on_boot_core()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Whether `p` lies in the arena.
#[inline]
pub fn contains(p: *mut u8) -> bool {
    let a = p as usize;
    let b = BASE.load(Ordering::Acquire);
    if b == 0 {
        return false;
    }
    if a >= b && a < b + (4096usize << ORDER.load(Ordering::Relaxed)) {
        return true;
    }
    let n = EXTRA_N.load(Ordering::Acquire);
    for i in 0..n {
        let eb = EXTRA_BASE[i].load(Ordering::Relaxed);
        if a >= eb && a < eb + (4096usize << EXTRA_ORDER[i].load(Ordering::Relaxed)) {
            return true;
        }
    }
    false
}

/// The arena's size in bytes (all its blocks).
fn total() -> usize {
    if BASE.load(Ordering::Acquire) == 0 {
        return 0;
    }
    let mut t = 4096usize << ORDER.load(Ordering::Relaxed);
    for i in 0..EXTRA_N.load(Ordering::Acquire) {
        t += 4096usize << EXTRA_ORDER[i].load(Ordering::Relaxed);
    }
    t
}

/// The biggest block of frames up to `want` bytes (at least 16 MiB): (address, order).
fn grab(want: usize) -> Option<(u64, usize)> {
    let mut order = 0;
    while (4096usize << order) < want {
        order += 1;
    }
    loop {
        if let Some(a) = super::frame::alloc_frames(order) {
            return Some((a, order));
        }
        if order <= 12 {
            return None; // not even 16 MiB in one piece
        }
        order -= 1;
    }
}

/// Make sure the arena exists (at least `bytes`: one block of frames, and more blocks when
/// the frame allocator has no piece that big) and route worker allocations to it. Boot core only.
pub fn activate(bytes: usize) -> bool {
    let _irq = super::IrqLock::acquire();
    if BASE.load(Ordering::Acquire) == 0 {
        let Some((addr, order)) = grab(bytes) else { return false };
        let _l = Spin::take();
        unsafe {
            BUMP = addr as usize;
            FREE = [0; CLASSES];
            CUR = 0;
        }
        EXTRA_N.store(0, Ordering::Release);
        ORDER.store(order, Ordering::Relaxed);
        END.store(addr as usize + (4096usize << order), Ordering::Release);
        BASE.store(addr as usize, Ordering::Release);
    }
    // grow by more blocks up to the size asked for (a job may be allocating meanwhile:
    // a block is published before the count that makes it visible)
    while total() < bytes && EXTRA_N.load(Ordering::Acquire) < MAX_EXTRA {
        let Some((addr, order)) = grab(bytes - total()) else { break };
        let _l = Spin::take();
        let n = EXTRA_N.load(Ordering::Relaxed);
        EXTRA_BASE[n].store(addr as usize, Ordering::Relaxed);
        EXTRA_ORDER[n].store(order, Ordering::Relaxed);
        EXTRA_N.store(n + 1, Ordering::Release);
    }
    ACTIVE.store(true, Ordering::Release);
    true
}

/// No job wants the arena now; it is given back once nothing in it is in use.
pub fn deactivate() {
    ACTIVE.store(false, Ordering::Release);
    release_if_idle();
}

/// Give the frames back when nothing lives in the arena and no job wants it.
pub fn release_if_idle() {
    if ACTIVE.load(Ordering::Acquire) || LIVE.load(Ordering::Acquire) != 0 {
        return;
    }
    let b = BASE.load(Ordering::Acquire);
    if b == 0 {
        return;
    }
    let _irq = super::IrqLock::acquire();
    let _l = Spin::take();
    if LIVE.load(Ordering::Acquire) != 0 {
        return;
    }
    BASE.store(0, Ordering::Release);
    END.store(0, Ordering::Release);
    super::frame::free_frames(b as u64, ORDER.load(Ordering::Relaxed));
    let n = EXTRA_N.swap(0, Ordering::AcqRel);
    for i in 0..n {
        super::frame::free_frames(EXTRA_BASE[i].load(Ordering::Relaxed) as u64, EXTRA_ORDER[i].load(Ordering::Relaxed));
    }
    unsafe { CUR = 0 };
}

/// (bytes in the arena, bytes taken from it so far, blocks in use).
pub fn stats() -> (usize, usize, usize) {
    let b = BASE.load(Ordering::Acquire);
    if b == 0 {
        return (0, 0, LIVE.load(Ordering::Relaxed));
    }
    // the blocks the bump pointer has passed, plus how far into the current one
    let cur = unsafe { CUR };
    let mut used = 4096usize << ORDER.load(Ordering::Relaxed);
    let mut base = b;
    for i in 0..cur.min(EXTRA_N.load(Ordering::Acquire)) {
        base = EXTRA_BASE[i].load(Ordering::Relaxed);
        if i + 1 < cur {
            used += 4096usize << EXTRA_ORDER[i].load(Ordering::Relaxed);
        }
    }
    let bump = unsafe { BUMP };
    let used = if cur == 0 { bump - b } else { used + (bump - base) };
    (total(), used, LIVE.load(Ordering::Relaxed))
}

pub unsafe fn alloc(size: usize, align: usize) -> *mut u8 {
    let pad = if align > HDR { align } else { 0 };
    let need = (size + HDR + pad).max(1 << MIN_CLASS);
    let mut class = MIN_CLASS;
    while (1usize << class) < need {
        class += 1;
    }
    if class >= CLASSES {
        return core::ptr::null_mut();
    }
    let _l = Spin::take();
    let free = &mut *(&raw mut FREE);
    let mut got = 0usize;
    let mut got_class = class;
    if free[class] != 0 {
        got = free[class];
        free[class] = *(got as *const usize);
    } else {
        // fresh space (blocks aligned to their size up to a page)
        let al = (1usize << class).min(4096);
        let mut at = (BUMP + al - 1) & !(al - 1);
        // this block is used up: on to the next one
        while at + (1 << class) > END.load(Ordering::Relaxed) && CUR < EXTRA_N.load(Ordering::Acquire) {
            let nb = EXTRA_BASE[CUR].load(Ordering::Relaxed);
            END.store(nb + (4096usize << EXTRA_ORDER[CUR].load(Ordering::Relaxed)), Ordering::Release);
            CUR += 1;
            BUMP = nb;
            at = (BUMP + al - 1) & !(al - 1);
        }
        if at + (1 << class) <= END.load(Ordering::Relaxed) {
            got = at;
            BUMP = at + (1 << class);
        } else {
            for c in class + 1..CLASSES {
                if free[c] != 0 {
                    got = free[c];
                    free[c] = *(got as *const usize);
                    got_class = c;
                    break;
                }
            }
        }
    }
    if got == 0 {
        return core::ptr::null_mut();
    }
    LIVE.fetch_add(1, Ordering::Relaxed);
    let user = if pad > 0 { (got + HDR + align - 1) & !(align - 1) } else { got + HDR };
    *((user - 16) as *mut usize) = got;
    *((user - 8) as *mut usize) = got_class;
    user as *mut u8
}

pub unsafe fn free(p: *mut u8) {
    let user = p as usize;
    let block = *((user - 16) as *const usize);
    let class = *((user - 8) as *const usize);
    if class >= CLASSES {
        return;
    }
    let _l = Spin::take();
    let free = &mut *(&raw mut FREE);
    *(block as *mut usize) = free[class];
    free[class] = block;
    LIVE.fetch_sub(1, Ordering::Relaxed);
}
