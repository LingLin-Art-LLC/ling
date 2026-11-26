//! Activations: an `h x w` grid of tokens, `c` f32 channels each (channels innermost, "HWC"), held in pieces of whole grid rows,
//! each under 32 MiB. LingOS's allocator hands out at most 64 MiB in one piece and rounds every large request up to a power of
//! two, so one 512x512x256 VAE activation (256 MiB) has to be several pieces, and pieces just under 32 MiB waste nothing.
use alloc::vec::Vec;

const SEG_FLOATS: usize = (32 << 20) / 4 - 4096;

pub struct Act {
    pub h: usize,
    pub w: usize,
    pub c: usize,
    /// grid rows per piece
    rps: usize,
    segs: Vec<Vec<f32>>,
    ptrs: Vec<*mut f32>,
}

// Jobs on other cores read and write disjoint tokens through the raw piece pointers.
unsafe impl Sync for Act {}
unsafe impl Send for Act {}

// ── recycled pieces ──────────────────────────────────────────────────────────

/// At most this many floats wait in the pool (160 MiB): enough for a UNet pass's churn (its biggest activations are ~40 MiB),
/// and modest next to the VAE's (on LingOS all of this is RAM below 4 GiB).
const POOL_MAX: usize = (160 << 20) / 4;

struct Pool {
    lock: core::sync::atomic::AtomicBool,
    v: core::cell::UnsafeCell<Vec<Vec<f32>>>,
    floats: core::cell::UnsafeCell<usize>,
}
unsafe impl Sync for Pool {}

static POOL: Pool = Pool { lock: core::sync::atomic::AtomicBool::new(false), v: core::cell::UnsafeCell::new(Vec::new()), floats: core::cell::UnsafeCell::new(0) };

fn with_pool<R>(f: impl FnOnce(&mut Vec<Vec<f32>>, &mut usize) -> R) -> R {
    use core::sync::atomic::Ordering;
    while POOL.lock.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
    // SAFETY: the flag above serializes every access
    let r = unsafe { f(&mut *POOL.v.get(), &mut *POOL.floats.get()) };
    POOL.lock.store(false, Ordering::Release);
    r
}

/// A recycled piece of exactly `n` initialized floats (taken from one at most 1.5x as long), if one is free.
fn pool_take(n: usize) -> Option<Vec<f32>> {
    with_pool(|v, floats| {
        let i = v.iter().position(|s| s.len() >= n && s.len() <= n + n / 2)?;
        let mut s = v.swap_remove(i);
        *floats -= s.len();
        s.truncate(n);
        Some(s)
    })
}

fn pool_give(s: Vec<f32>) {
    if s.len() < 4096 {
        return;
    }
    let s = with_pool(|v, floats| {
        if *floats + s.len() > POOL_MAX {
            return Some(s);
        }
        *floats += s.len();
        v.push(s);
        None
    });
    drop(s); // a piece the pool had no room for is freed outside the lock
}

/// Free every recycled piece (between generations, to give the RAM back).
pub fn pool_trim() {
    let all = with_pool(|v, floats| {
        *floats = 0;
        core::mem::take(v)
    });
    drop(all);
}

impl Drop for Act {
    fn drop(&mut self) {
        for s in self.segs.drain(..) {
            pool_give(s);
        }
    }
}

impl Act {
    pub fn new(h: usize, w: usize, c: usize) -> Act {
        let row = (w * c).max(1);
        let rps = (SEG_FLOATS / row).max(1).min(h.max(1));
        let mut segs = Vec::new();
        let mut y = 0;
        while y < h.max(1) {
            let rows = rps.min(h.max(1) - y);
            // a recycled piece (zeroed here) before a fresh allocation: the pool never holds RAM a new activation then lacks
            segs.push(match pool_take(rows * row) {
                Some(mut v) => {
                    v.fill(0.0);
                    v
                },
                None => alloc::vec![0f32; rows * row],
            });
            y += rows;
        }
        let ptrs = segs.iter_mut().map(|s| s.as_mut_ptr()).collect();
        Act { h, w, c, rps, segs, ptrs }
    }

    /// Like `new`, for an output every element of which the caller writes: the pieces come from the recycled ones when a fitting
    /// one is free (their old contents stay), so a layer's output costs neither an allocation nor a zeroing pass. Each UNet pass
    /// makes and drops a few GB of these; zeroing them on the boot core (or faulting fresh pages in, on a host) was measurable.
    pub fn new_uninit(h: usize, w: usize, c: usize) -> Act {
        let row = (w * c).max(1);
        let rps = (SEG_FLOATS / row).max(1).min(h.max(1));
        let mut segs = Vec::new();
        let mut y = 0;
        while y < h.max(1) {
            let rows = rps.min(h.max(1) - y);
            segs.push(pool_take(rows * row).unwrap_or_else(|| alloc::vec![0f32; rows * row]));
            y += rows;
        }
        let ptrs = segs.iter_mut().map(|s| s.as_mut_ptr()).collect();
        Act { h, w, c, rps, segs, ptrs }
    }

    /// A 1 x n grid (a token sequence).
    pub fn seq(n: usize, c: usize) -> Act {
        Act::new(1, n, c)
    }

    pub fn tokens(&self) -> usize {
        self.h * self.w
    }

    /// Token `p`'s channels.
    #[inline(always)]
    pub fn px(&self, p: usize) -> *mut f32 {
        let (y, x) = (p / self.w, p % self.w);
        let s = y / self.rps;
        unsafe { self.ptrs[s].add(((y - s * self.rps) * self.w + x) * self.c) }
    }

    pub fn tok(&self, p: usize) -> &[f32] {
        unsafe { core::slice::from_raw_parts(self.px(p), self.c) }
    }

    pub fn tok_mut(&mut self, p: usize) -> &mut [f32] {
        unsafe { core::slice::from_raw_parts_mut(self.px(p), self.c) }
    }

    /// The pieces, as (first token, slice): for whole-tensor element-wise work.
    pub fn pieces(&self) -> impl Iterator<Item = &[f32]> {
        self.segs.iter().map(|s| s.as_slice())
    }

    pub fn pieces_mut(&mut self) -> impl Iterator<Item = &mut [f32]> {
        self.segs.iter_mut().map(|s| s.as_mut_slice())
    }

    pub fn fill(&mut self, v: f32) {
        for s in self.segs.iter_mut() {
            s.fill(v);
        }
    }

    /// Everything as one flat Vec (tokens x channels), for tests and small tensors.
    pub fn to_vec(&self) -> Vec<f32> {
        let mut v = Vec::with_capacity(self.tokens() * self.c);
        for s in &self.segs {
            v.extend_from_slice(s);
        }
        v
    }

    pub fn from_slice(h: usize, w: usize, c: usize, data: &[f32]) -> Act {
        let mut a = Act::new(h, w, c);
        for p in 0..h * w {
            a.tok_mut(p).copy_from_slice(&data[p * c..(p + 1) * c]);
        }
        a
    }

    /// A copy (same shape).
    pub fn copy(&self) -> Act {
        let mut a = Act::new_uninit(self.h, self.w, self.c);
        for (d, s) in a.segs.iter_mut().zip(self.segs.iter()) {
            d.copy_from_slice(s);
        }
        a
    }

    /// From channel-major planes (CHW, as torch / candle hold them).
    pub fn from_chw(c: usize, h: usize, w: usize, data: &[f32]) -> Act {
        let mut a = Act::new(h, w, c);
        for p in 0..h * w {
            let t = a.tok_mut(p);
            for ch in 0..c {
                t[ch] = data[ch * h * w + p];
            }
        }
        a
    }

    /// To channel-major planes.
    pub fn to_chw(&self) -> Vec<f32> {
        let n = self.tokens();
        let mut v = alloc::vec![0f32; n * self.c];
        for p in 0..n {
            let t = self.tok(p);
            for ch in 0..self.c {
                v[ch * n + p] = t[ch];
            }
        }
        v
    }
}
