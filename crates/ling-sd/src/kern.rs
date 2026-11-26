//! The hooks the host gives the engine, and the inner loops everything else is built on.
//!
//! * `set_parallel(f, cores)`: how to run `f(ctx, 0..n)` over several cores (LingOS: `par::for_each`; a host test: std threads).
//!   Jobs never allocate, print or yield: they only read the inputs and write their own part of the output.
//! * `set_yield(f)`: called between rounds of jobs, so a kernel coroutine can hand the CPU to the desktop.
//! * `set_avx2(on)`: the caller says AVX2 + FMA + F16C are usable (CPUID *and* the OS saved the YMM state). Off: SSE (x86-64's
//!   baseline) or portable code. `set_avx512(on)`: AVX-512F too (CPUID *and* XCR0 opmask + ZMM state).
//! * `set_clock(f)`: a nanosecond clock, for `profile()` (time per kind of layer) and `gemm_flops()`.
//!
//! The one GEMM shape: `C[r][0..16 np] (+)= sum_k A[r][k] * B[k][..]`, where `B` is one or two *panels*: 16 columns of a weight
//! matrix laid out k-major (`K x 16`, contiguous; f32, or f16 widened in registers). Every weight is stored as panels, so the
//! micro-kernel streams B linearly and broadcasts A; the rows of A are given as pointers, which lets a 3x3 convolution read shifted
//! input rows in place (no im2col). `gemm_job` is a job's whole GEMM: K blocks sized for L1, every panel used by all of the job's
//! row strips before the next, strips cut evenly. Micro-kernels: AVX-512 12 rows x 2 panels (24 zmm accumulators), AVX2 6 x 1
//! (12 ymm), SSE / portable the 4 x 16 `tile`.
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};

pub type ParFn = fn(n: usize, ctx: *const (), f: unsafe fn(*const (), usize));

static PAR: AtomicUsize = AtomicUsize::new(0);
static CORES: AtomicUsize = AtomicUsize::new(1);
static YIELD: AtomicUsize = AtomicUsize::new(0);
static AVX2: AtomicBool = AtomicBool::new(false);
static AVX512: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);

pub fn set_parallel(f: ParFn, cores: usize) {
    PAR.store(f as usize, Relaxed);
    CORES.store(cores.clamp(1, 64), Relaxed);
}

pub fn set_yield(f: fn()) {
    YIELD.store(f as usize, Relaxed);
}

/// Use the AVX2 + FMA + F16C kernels (only when the CPU has them AND the OS enabled the YMM state).
pub fn set_avx2(on: bool) {
    AVX2.store(on, Relaxed);
}

pub fn avx2() -> bool {
    AVX2.load(Relaxed)
}

/// Use the AVX-512F kernels (16-wide zmm GEMM micro-kernel, 16-wide f16 widening and exp). Not a default: on an i9-11900K
/// (one 512-bit FMA port) they measured 25-35% slower than the AVX2 ones on every UNet layer (one core: 108 vs 156 GFLOPS). Only when the CPU has AVX-512F, the OS
/// enabled the opmask + ZMM state in XCR0 (bits 5-7), AND AVX2 + FMA + F16C are on too (`set_avx2(true)`): the AVX-512 paths
/// lean on them for the edges. LingOS's interrupt handlers are compiled for SSE (legacy encodings leave the upper ZMM bits and
/// zmm16-31 alone) and its task switches are function calls (no vector register is live across one), so no ZMM state is lost.
pub fn set_avx512(on: bool) {
    AVX512.store(on, Relaxed);
}

pub fn avx512() -> bool {
    AVX512.load(Relaxed) && AVX2.load(Relaxed)
}

/// What the engine runs on right now, for status lines: "AVX-512", "AVX2 + FMA" or "SSE".
pub fn simd_name() -> &'static str {
    if avx512() {
        "AVX-512"
    } else if avx2() {
        "AVX2 + FMA"
    } else {
        "SSE"
    }
}

/// Rows of A one micro-kernel call takes (12 with AVX-512: 24 zmm accumulators for 12 x 32; 6 with AVX2: 12 ymm for 6 x 16;
/// 4 on SSE / portable).
#[inline]
pub fn mr() -> usize {
    if avx512() {
        12
    } else if avx2() {
        6
    } else {
        4
    }
}

/// Panels (16 columns each) one micro-kernel call takes: 2 with AVX-512, else 1.
#[inline]
pub fn np_max() -> usize {
    if avx512() {
        2
    } else {
        1
    }
}

/// Ask a running generation to stop at the next round of work.
pub fn set_cancel(on: bool) {
    CANCEL.store(on, Relaxed);
}

pub fn cancelled() -> bool {
    CANCEL.load(Relaxed)
}

pub fn cores() -> usize {
    CORES.load(Relaxed)
}

// ── profiling (optional): where a generation's time goes ─────────────────────

static CLOCK: AtomicUsize = AtomicUsize::new(0);

/// What the profile is broken down into.
pub const PROF_NAMES: [&str; 6] = ["widen f16", "conv 3x3", "linear", "attention", "norms", "element-wise"];
pub const PROF_WIDEN: usize = 0;
pub const PROF_CONV: usize = 1;
pub const PROF_LINEAR: usize = 2;
pub const PROF_ATTN: usize = 3;
pub const PROF_NORM: usize = 4;
pub const PROF_ELEM: usize = 5;
static PROF: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
static FLOPS: AtomicU64 = AtomicU64::new(0);

/// Floating-point operations the GEMMs did since the last reset (2 per multiply-add; counted only while a clock is set).
pub fn gemm_flops() -> u64 {
    FLOPS.load(Relaxed)
}

static TRACE: AtomicUsize = AtomicUsize::new(0);

/// Per-call trace for tuning: `f(bucket, m, n, k, ns)` after every GEMM-backed op (m = rows, n = columns, k = depth).
pub type TraceFn = fn(usize, usize, usize, usize, u64);

pub fn set_trace(f: TraceFn) {
    TRACE.store(f as usize, Relaxed);
}

#[inline]
pub fn trace(bucket: usize, m: usize, n: usize, k: usize, t0: u64) {
    let f = TRACE.load(Relaxed);
    if f != 0 && t0 != 0 {
        // SAFETY: only set_trace stores here, and it stores a TraceFn
        let f: TraceFn = unsafe { core::mem::transmute(f) };
        f(bucket, m, n, k, now_ns().saturating_sub(t0));
    }
}

/// A nanosecond clock (host: `Instant`, LingOS: the TSC). Without one the profile stays empty and costs nothing.
pub fn set_clock(f: fn() -> u64) {
    CLOCK.store(f as usize, Relaxed);
}

#[inline]
pub fn now_ns() -> u64 {
    let c = CLOCK.load(Relaxed);
    if c == 0 {
        return 0;
    }
    // SAFETY: only set_clock stores here, and it stores an fn() -> u64
    let f: fn() -> u64 = unsafe { core::mem::transmute(c) };
    f()
}

/// Add the time since `t0` (from `now_ns`; 0 = no clock) to bucket `i`.
#[inline]
pub fn prof_add(i: usize, t0: u64) {
    if t0 != 0 {
        PROF[i].fetch_add(now_ns().saturating_sub(t0), Relaxed);
    }
}

pub fn prof_reset() {
    for p in PROF.iter() {
        p.store(0, Relaxed);
    }
    FLOPS.store(0, Relaxed);
}

/// (bucket, seconds) since the last reset. The f16 widening is its own bucket (the convolution / linear figures exclude it).
pub fn profile() -> [(&'static str, f64); 6] {
    let mut out = [("", 0.0); 6];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (PROF_NAMES[i], PROF[i].load(Relaxed) as f64 * 1e-9);
    }
    out
}

/// Give the host a chance to run something else (between rounds of jobs).
pub fn pump() {
    let y = YIELD.load(Relaxed);
    if y != 0 {
        // SAFETY: only set_yield stores here, and it stores an fn()
        let f: fn() = unsafe { core::mem::transmute(y) };
        f();
    }
}

unsafe fn par_tramp<F: Fn(usize) + Sync>(ctx: *const (), i: usize) {
    (*(ctx as *const F))(i)
}

/// `f(0) .. f(n-1)`, over the cores the host lends (in order on one core without a hook).
pub fn par_for<F: Fn(usize) + Sync>(n: usize, f: &F) {
    if n == 0 {
        return;
    }
    let p = PAR.load(Relaxed);
    if p == 0 || n == 1 {
        for i in 0..n {
            f(i);
        }
        return;
    }
    // SAFETY: only set_parallel stores here, and it stores a ParFn
    let pf: ParFn = unsafe { core::mem::transmute(p) };
    pf(n, f as *const F as *const (), par_tramp::<F>);
}

/// `f(0) .. f(n-1)` in rounds of a few jobs per core, yielding to the host between rounds (keeps the desktop drawing while a big
/// layer runs, and lets a cancel stop a long layer early).
pub fn run_jobs<F: Fn(usize) + Sync>(n: usize, f: &F) {
    let round = (cores() * 6).max(4);
    let mut i0 = 0;
    while i0 < n {
        let m = round.min(n - i0);
        par_for(m, &|j: usize| f(i0 + j));
        i0 += m;
        pump();
    }
}

/// Per-job scratch buffers: a job claims a free slot for its duration. At most `cores()` jobs run at once, so that many slots.
pub struct Slots {
    busy: AtomicU64,
}

impl Slots {
    pub const fn new() -> Slots {
        Slots { busy: AtomicU64::new(0) }
    }
    pub fn claim(&self) -> usize {
        loop {
            let b = self.busy.load(core::sync::atomic::Ordering::Acquire);
            let free = (!b).trailing_zeros() as usize;
            if free < 64 && self.busy.compare_exchange_weak(b, b | (1 << free), core::sync::atomic::Ordering::AcqRel, Relaxed).is_ok() {
                return free;
            }
            core::hint::spin_loop();
        }
    }
    pub fn release(&self, i: usize) {
        self.busy.fetch_and(!(1u64 << i), core::sync::atomic::Ordering::Release);
    }
}

/// A raw pointer that may be shared with the job cores; each job writes only its own part.
#[derive(Clone, Copy)]
pub struct P<T>(pub *mut T);
unsafe impl<T> Sync for P<T> {}
unsafe impl<T> Send for P<T> {}
impl<T> P<T> {
    #[inline(always)]
    pub fn get(&self) -> *mut T {
        self.0
    }
}

// ── the GEMM tile ─────────────────────────────────────────────────────────────

/// `C[r][0..ncols] = (acc ? C[r] : 0) + bias + sum_k A[r][k] * B[k][..]` for r < rows (<= 4). `B`: a K x 16 panel. `bias`: null or
/// 16 floats (this panel's columns).
#[inline]
pub unsafe fn tile(k: usize, a: &[*const f32; 4], rows: usize, b: *const f32, c: &[*mut f32; 4], ncols: usize, acc: bool, bias: *const f32) {
    #[cfg(target_arch = "x86_64")]
    {
        if AVX2.load(Relaxed) {
            return tile_avx2(k, a, rows, b, c, ncols, acc, bias);
        }
        return tile_sse(k, a, rows, b, c, ncols, acc, bias);
    }
    #[allow(unreachable_code)]
    tile_generic(k, a, rows, b, c, ncols, acc, bias)
}

#[inline(always)]
unsafe fn store_row(c: *mut f32, vals: &[f32; 16], ncols: usize, acc: bool, bias: *const f32) {
    for j in 0..ncols {
        let mut v = vals[j];
        if !bias.is_null() {
            v += *bias.add(j);
        }
        if acc {
            v += *c.add(j);
        }
        *c.add(j) = v;
    }
}

#[allow(dead_code)]
unsafe fn tile_generic(k: usize, a: &[*const f32; 4], rows: usize, b: *const f32, c: &[*mut f32; 4], ncols: usize, acc: bool, bias: *const f32) {
    let mut s = [[0f32; 16]; 4];
    for kk in 0..k {
        let bp = b.add(kk * 16);
        for r in 0..rows {
            let av = *a[r].add(kk);
            for j in 0..16 {
                s[r][j] += av * *bp.add(j);
            }
        }
    }
    for r in 0..rows {
        store_row(c[r], &s[r], ncols, acc, bias);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn tile_avx2(k: usize, a: &[*const f32; 4], rows: usize, b: *const f32, c: &[*mut f32; 4], ncols: usize, acc: bool, bias: *const f32) {
    use core::arch::x86_64::*;
    let a0 = a[0];
    let a1 = if rows > 1 { a[1] } else { a0 };
    let a2 = if rows > 2 { a[2] } else { a0 };
    let a3 = if rows > 3 { a[3] } else { a0 };
    let (mut c00, mut c01, mut c10, mut c11) = (_mm256_setzero_ps(), _mm256_setzero_ps(), _mm256_setzero_ps(), _mm256_setzero_ps());
    let (mut c20, mut c21, mut c30, mut c31) = (_mm256_setzero_ps(), _mm256_setzero_ps(), _mm256_setzero_ps(), _mm256_setzero_ps());
    let mut bp = b;
    for kk in 0..k {
        let b0 = _mm256_loadu_ps(bp);
        let b1 = _mm256_loadu_ps(bp.add(8));
        let x = _mm256_broadcast_ss(&*a0.add(kk));
        c00 = _mm256_fmadd_ps(x, b0, c00);
        c01 = _mm256_fmadd_ps(x, b1, c01);
        let x = _mm256_broadcast_ss(&*a1.add(kk));
        c10 = _mm256_fmadd_ps(x, b0, c10);
        c11 = _mm256_fmadd_ps(x, b1, c11);
        let x = _mm256_broadcast_ss(&*a2.add(kk));
        c20 = _mm256_fmadd_ps(x, b0, c20);
        c21 = _mm256_fmadd_ps(x, b1, c21);
        let x = _mm256_broadcast_ss(&*a3.add(kk));
        c30 = _mm256_fmadd_ps(x, b0, c30);
        c31 = _mm256_fmadd_ps(x, b1, c31);
        bp = bp.add(16);
    }
    let pairs = [(c00, c01), (c10, c11), (c20, c21), (c30, c31)];
    for r in 0..rows {
        let (lo, hi) = pairs[r];
        let cp = c[r];
        if ncols == 16 {
            let mut lo = lo;
            let mut hi = hi;
            if !bias.is_null() {
                lo = _mm256_add_ps(lo, _mm256_loadu_ps(bias));
                hi = _mm256_add_ps(hi, _mm256_loadu_ps(bias.add(8)));
            }
            if acc {
                lo = _mm256_add_ps(lo, _mm256_loadu_ps(cp));
                hi = _mm256_add_ps(hi, _mm256_loadu_ps(cp.add(8)));
            }
            _mm256_storeu_ps(cp, lo);
            _mm256_storeu_ps(cp.add(8), hi);
        } else {
            let mut v = [0f32; 16];
            _mm256_storeu_ps(v.as_mut_ptr(), lo);
            _mm256_storeu_ps(v.as_mut_ptr().add(8), hi);
            store_row(cp, &v, ncols, acc, bias);
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn tile_sse(k: usize, a: &[*const f32; 4], rows: usize, b: *const f32, c: &[*mut f32; 4], ncols: usize, acc: bool, bias: *const f32) {
    use core::arch::x86_64::*;
    let a0 = a[0];
    let a1 = if rows > 1 { a[1] } else { a0 };
    let a2 = if rows > 2 { a[2] } else { a0 };
    let a3 = if rows > 3 { a[3] } else { a0 };
    // two passes of 8 columns: 8 accumulators + 2 B + 1 A fit the 16 xmm registers
    for half in 0..2 {
        if half * 8 >= ncols {
            break;
        }
        let (mut c00, mut c01, mut c10, mut c11) = (_mm_setzero_ps(), _mm_setzero_ps(), _mm_setzero_ps(), _mm_setzero_ps());
        let (mut c20, mut c21, mut c30, mut c31) = (_mm_setzero_ps(), _mm_setzero_ps(), _mm_setzero_ps(), _mm_setzero_ps());
        let mut bp = b.add(half * 8);
        for kk in 0..k {
            let b0 = _mm_loadu_ps(bp);
            let b1 = _mm_loadu_ps(bp.add(4));
            let x = _mm_set1_ps(*a0.add(kk));
            c00 = _mm_add_ps(c00, _mm_mul_ps(x, b0));
            c01 = _mm_add_ps(c01, _mm_mul_ps(x, b1));
            let x = _mm_set1_ps(*a1.add(kk));
            c10 = _mm_add_ps(c10, _mm_mul_ps(x, b0));
            c11 = _mm_add_ps(c11, _mm_mul_ps(x, b1));
            let x = _mm_set1_ps(*a2.add(kk));
            c20 = _mm_add_ps(c20, _mm_mul_ps(x, b0));
            c21 = _mm_add_ps(c21, _mm_mul_ps(x, b1));
            let x = _mm_set1_ps(*a3.add(kk));
            c30 = _mm_add_ps(c30, _mm_mul_ps(x, b0));
            c31 = _mm_add_ps(c31, _mm_mul_ps(x, b1));
            bp = bp.add(16);
        }
        let pairs = [(c00, c01), (c10, c11), (c20, c21), (c30, c31)];
        let n = (ncols - half * 8).min(8);
        let bb = if bias.is_null() { bias } else { bias.add(half * 8) };
        for r in 0..rows {
            let (lo, hi) = pairs[r];
            let cp = c[r].add(half * 8);
            if n == 8 {
                let mut lo = lo;
                let mut hi = hi;
                if !bb.is_null() {
                    lo = _mm_add_ps(lo, _mm_loadu_ps(bb));
                    hi = _mm_add_ps(hi, _mm_loadu_ps(bb.add(4)));
                }
                if acc {
                    lo = _mm_add_ps(lo, _mm_loadu_ps(cp));
                    hi = _mm_add_ps(hi, _mm_loadu_ps(cp.add(4)));
                }
                _mm_storeu_ps(cp, lo);
                _mm_storeu_ps(cp.add(4), hi);
            } else {
                let mut v = [0f32; 16];
                _mm_storeu_ps(v.as_mut_ptr(), lo);
                _mm_storeu_ps(v.as_mut_ptr().add(4), hi);
                store_row(cp, &v, n, acc, bb);
            }
        }
    }
}

// ── the register-blocked micro-kernel ────────────────────────────────────────
//
// A is read in place through row pointers (a 3x3 convolution's shifted input rows, a token's channels). Packing A into a
// contiguous buffer per K block was tried: no gain on the large layers, ~20% lost on the VAE's short-K convolutions.

/// Row tails: `vals` holds the row's sums for 32 columns, `ncols` of them are real.
#[inline(always)]
unsafe fn store_tail(c: *mut f32, vals: &[f32; 32], ncols: usize, acc: bool, bias: *const f32) {
    for j in 0..ncols {
        let mut v = vals[j];
        if !bias.is_null() {
            v += *bias.add(j);
        }
        if acc {
            v += *c.add(j);
        }
        *c.add(j) = v;
    }
}

/// One micro-kernel: R rows (A row r at `a[r]`), VN vectors of W floats per row (VN * W = 16 * panels). The loops over R and VN
/// have constant trip counts, so they unroll and the accumulators live in registers.
/// `$BT` / `$ldb`: the type B is stored as and how W of it become a vector (f32: a load; f16: a load + vcvtph2ps).
macro_rules! gemm_kernel {
    ($name:ident, $feat:literal, $W:literal, $BT:ty, $ldb:ident, $load:ident, $store:ident, $set1:ident, $fma:ident, $zero:ident, $add:ident) => {
        #[cfg(target_arch = "x86_64")]
        #[target_feature(enable = $feat)]
        unsafe fn $name<const R: usize, const VN: usize>(k: usize, a: *const *const f32, b0: *const $BT, b1: *const $BT, c: *const *mut f32, ncols: usize, acc: bool, bias: *const f32) {
            use core::arch::x86_64::*;
            let mut ap = [core::ptr::null::<f32>(); R];
            for r in 0..R {
                ap[r] = *a.add(r);
            }
            let mut s = [[$zero(); VN]; R];
            let (mut p0, mut p1) = (b0, b1);
            for kk in 0..k {
                let mut bv = [$zero(); VN];
                for v in 0..VN {
                    // vectors [0, 16 / W) come from the first panel, the rest from the second
                    bv[v] = if v * $W < 16 { $ldb!(p0.add(v * $W)) } else { $ldb!(p1.add(v * $W - 16)) };
                }
                for r in 0..R {
                    let x = $set1(*ap[r].add(kk));
                    for v in 0..VN {
                        s[r][v] = $fma(x, bv[v], s[r][v]);
                    }
                }
                p0 = p0.add(16);
                p1 = p1.add(16);
            }
            // Out through a stack buffer, every index a constant: indexing `s` with a loop variable here made LLVM keep the
            // accumulators in memory and store all of them on every k step (seen in the assembly).
            let mut t = [[0f32; 32]; R];
            for r in 0..R {
                for v in 0..VN {
                    $store(t[r].as_mut_ptr().add(v * $W), s[r][v]);
                }
            }
            if ncols == VN * $W {
                for r in 0..R {
                    let cp = *c.add(r);
                    let tp = t[r].as_ptr();
                    for v in 0..VN {
                        let mut x = $load(tp.add(v * $W));
                        if !bias.is_null() {
                            x = $add(x, $load(bias.add(v * $W)));
                        }
                        if acc {
                            x = $add(x, $load(cp.add(v * $W)));
                        }
                        $store(cp.add(v * $W), x);
                    }
                }
            } else {
                for r in 0..R {
                    store_tail(*c.add(r), &t[r], ncols, acc, bias);
                }
            }
        }
    };
}

macro_rules! ld_f32x8 {
    ($p:expr) => {
        _mm256_loadu_ps($p)
    };
}
macro_rules! ld_f16x8 {
    ($p:expr) => {
        _mm256_cvtph_ps(_mm_loadu_si128($p as *const __m128i))
    };
}
macro_rules! ld_f32x16 {
    ($p:expr) => {
        _mm512_loadu_ps($p)
    };
}
macro_rules! ld_f16x16 {
    ($p:expr) => {
        _mm512_cvtph_ps(_mm256_loadu_si256($p as *const __m256i))
    };
}

gemm_kernel!(mk_avx2, "avx2,fma", 8, f32, ld_f32x8, _mm256_loadu_ps, _mm256_storeu_ps, _mm256_set1_ps, _mm256_fmadd_ps, _mm256_setzero_ps, _mm256_add_ps);
gemm_kernel!(mk_avx2h, "avx2,fma,f16c", 8, u16, ld_f16x8, _mm256_loadu_ps, _mm256_storeu_ps, _mm256_set1_ps, _mm256_fmadd_ps, _mm256_setzero_ps, _mm256_add_ps);
gemm_kernel!(mk_avx512, "avx512f,avx2,fma", 16, f32, ld_f32x16, _mm512_loadu_ps, _mm512_storeu_ps, _mm512_set1_ps, _mm512_fmadd_ps, _mm512_setzero_ps, _mm512_add_ps);
gemm_kernel!(mk_avx512h, "avx512f,avx2,fma,f16c", 16, u16, ld_f16x16, _mm512_loadu_ps, _mm512_storeu_ps, _mm512_set1_ps, _mm512_fmadd_ps, _mm512_setzero_ps, _mm512_add_ps);

/// The micro-kernel instance for a strip's row count and panel count.
macro_rules! dispatch {
    ($k512:ident, $k2:ident, $k:expr, $a:expr, $rows:expr, $b0:expr, $b1:expr, $two:expr, $cp:expr, $ncols:expr, $acc:expr, $bias:expr) => {{
        let (k, a, b0, b1, cp, ncols, acc, bias) = ($k, $a, $b0, $b1, $cp, $ncols, $acc, $bias);
        if avx512() {
            if !$two {
                match $rows {
                    1 => $k512::<1, 1>(k, a, b0, b0, cp, ncols, acc, bias),
                    2 => $k512::<2, 1>(k, a, b0, b0, cp, ncols, acc, bias),
                    3 => $k512::<3, 1>(k, a, b0, b0, cp, ncols, acc, bias),
                    4 => $k512::<4, 1>(k, a, b0, b0, cp, ncols, acc, bias),
                    5 => $k512::<5, 1>(k, a, b0, b0, cp, ncols, acc, bias),
                    6 => $k512::<6, 1>(k, a, b0, b0, cp, ncols, acc, bias),
                    7 => $k512::<7, 1>(k, a, b0, b0, cp, ncols, acc, bias),
                    8 => $k512::<8, 1>(k, a, b0, b0, cp, ncols, acc, bias),
                    9 => $k512::<9, 1>(k, a, b0, b0, cp, ncols, acc, bias),
                    10 => $k512::<10, 1>(k, a, b0, b0, cp, ncols, acc, bias),
                    11 => $k512::<11, 1>(k, a, b0, b0, cp, ncols, acc, bias),
                    _ => $k512::<12, 1>(k, a, b0, b0, cp, ncols, acc, bias),
                }
            } else {
                match $rows {
                    1 => $k512::<1, 2>(k, a, b0, b1, cp, ncols, acc, bias),
                    2 => $k512::<2, 2>(k, a, b0, b1, cp, ncols, acc, bias),
                    3 => $k512::<3, 2>(k, a, b0, b1, cp, ncols, acc, bias),
                    4 => $k512::<4, 2>(k, a, b0, b1, cp, ncols, acc, bias),
                    5 => $k512::<5, 2>(k, a, b0, b1, cp, ncols, acc, bias),
                    6 => $k512::<6, 2>(k, a, b0, b1, cp, ncols, acc, bias),
                    7 => $k512::<7, 2>(k, a, b0, b1, cp, ncols, acc, bias),
                    8 => $k512::<8, 2>(k, a, b0, b1, cp, ncols, acc, bias),
                    9 => $k512::<9, 2>(k, a, b0, b1, cp, ncols, acc, bias),
                    10 => $k512::<10, 2>(k, a, b0, b1, cp, ncols, acc, bias),
                    11 => $k512::<11, 2>(k, a, b0, b1, cp, ncols, acc, bias),
                    _ => $k512::<12, 2>(k, a, b0, b1, cp, ncols, acc, bias),
                }
            }
        } else {
            match $rows {
                1 => $k2::<1, 2>(k, a, b0, b0, cp, ncols, acc, bias),
                2 => $k2::<2, 2>(k, a, b0, b0, cp, ncols, acc, bias),
                3 => $k2::<3, 2>(k, a, b0, b0, cp, ncols, acc, bias),
                4 => $k2::<4, 2>(k, a, b0, b0, cp, ncols, acc, bias),
                5 => $k2::<5, 2>(k, a, b0, b0, cp, ncols, acc, bias),
                _ => $k2::<6, 2>(k, a, b0, b0, cp, ncols, acc, bias),
            }
        }
    }};
}

/// `C[r][0..ncols] = (acc ? C[r] : 0) + bias + sum_k A[r][k] * B[k][..]` for `rows` rows (<= [`mr`]; row r at `a[r]`) over `np`
/// adjacent panels (<= [`np_max`]; panel q = `b[q]`, K x 16 k-major, lands in columns 16q..16q+16 of each C row),
/// the panels f32 or, with `half`, f16 (`b[q]` then points at u16s, widened in registers). `ncols` (<= 16 * panels) columns are
/// real; the rest are never written. `bias`: null or 16 * panels floats. AVX-512: 12 rows x 2 panels = 24 zmm accumulators;
/// AVX2: 6 x 1 = 12 ymm. Only for those two (SSE uses [`tile`]).
#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn gemm_rows(k: usize, a: *const *const f32, rows: usize, b: [*const u8; 2], np: usize, half: bool, c: &[*mut f32], ncols: usize, acc: bool, bias: *const f32) {
    let cp = c.as_ptr();
    let two = np > 1 && ncols > 16;
    if half {
        let (b0, b1) = (b[0] as *const u16, if two { b[1] as *const u16 } else { b[0] as *const u16 });
        dispatch!(mk_avx512h, mk_avx2h, k, a, rows, b0, b1, two, cp, ncols, acc, bias)
    } else {
        let (b0, b1) = (b[0] as *const f32, if two { b[1] as *const f32 } else { b[0] as *const f32 });
        dispatch!(mk_avx512, mk_avx2, k, a, rows, b0, b1, two, cp, ncols, acc, bias)
    }
}

/// K is cut into blocks so that one block of a panel (pair) -- KC x 16 f32 per panel -- stays in L1 (48 KiB on Rocket Lake /
/// Ice Lake) next to the A rows while every strip of the job passes it: about 20 KiB of B.
#[inline]
pub fn kc() -> usize {
    match KC_OVERRIDE.load(Relaxed) {
        0 => 20480 / (64 * np_max()),
        v => v,
    }
}

static KC_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

/// For tuning: force the K block (0 = the default). Set it before the engine sizes its buffers (`Eng::new`).
pub fn set_kc(v: usize) {
    KC_OVERRIDE.store(v, Relaxed);
}

/// Rows whose A / C pointers a job works out at once (the closures do divisions: once per row, not per tap, K block and panel).
const CH: usize = 256;

/// One job's GEMM: `rows` rows of A (`a(i)` = row i, K floats) times panels `[p0, p1)` (`b(p)` = panel p, K x 16 f32) into
/// `c(i) + 16 p` (`n` = the layer's real column count, for the last panel); with `half` the panels are f16 (`b(p)` then points
/// at u16s; AVX2 / AVX-512 only). Rows go in chunks of [`CH`]; per chunk and K block every panel (pair) is used by all the row
/// strips before the next one is touched; strips are cut evenly (64 rows with 12-row strips: 11+11+11+11+10+10, never a lone
/// 4). `first_acc`: add onto C (else the first K block overwrites it, plus `bias` if not null).
#[inline(always)]
pub unsafe fn gemm_job<A: Fn(usize) -> *const f32, C: Fn(usize) -> *mut f32, B: Fn(usize) -> *const f32>(
    k: usize,
    rows: usize,
    a: &A,
    c: &C,
    b: &B,
    p0: usize,
    p1: usize,
    n: usize,
    first_acc: bool,
    bias: *const f32,
    half: bool,
) {
    if rows == 0 || p0 >= p1 || k == 0 {
        return;
    }
    if CLOCK.load(Relaxed) != 0 {
        FLOPS.fetch_add((2 * k * rows * (n - p0 * 16).min((p1 - p0) * 16)) as u64, Relaxed);
    }
    let simd = avx2();
    let (mr, npm) = (mr(), np_max());
    let nkb = k.div_ceil(if simd { kc() } else { 1 << 30 });
    let kc = k.div_ceil(nkb);
    let mut arow = [core::ptr::null::<f32>(); CH];
    let mut crow = [core::ptr::null_mut::<f32>(); CH];
    let mut r0 = 0;
    while r0 < rows {
        let m_all = (rows - r0).min(CH);
        for i in 0..m_all {
            arow[i] = a(r0 + i);
            crow[i] = c(r0 + i);
        }
        let strips = m_all.div_ceil(mr);
        let (base, extra) = (m_all / strips, m_all % strips);
        let mut k0 = 0;
        while k0 < k {
            let kn = kc.min(k - k0);
            let first = k0 == 0;
            let mut p = p0;
            while p < p1 {
                let np = npm.min(p1 - p);
                let ncols = (n - p * 16).min(16 * np);
                let np = ncols.div_ceil(16);
                // B as bytes: f32 or (half) f16 elements
                let es = if half { 2 } else { 4 };
                let bp = [(b(p) as *const u8).add(k0 * 16 * es), if np > 1 { (b(p + 1) as *const u8).add(k0 * 16 * es) } else { core::ptr::null() }];
                let bias_p = if first && !bias.is_null() { bias.add(p * 16) } else { core::ptr::null() };
                let acc = first_acc || !first;
                let mut r = 0;
                for s in 0..strips {
                    let m = base + (s < extra) as usize;
                    let mut ap = [core::ptr::null::<f32>(); 12];
                    let mut cp = [core::ptr::null_mut::<f32>(); 12];
                    for i in 0..m {
                        ap[i] = arow[r + i].add(k0);
                        cp[i] = crow[r + i].add(p * 16);
                    }
                    #[cfg(target_arch = "x86_64")]
                    if simd {
                        gemm_rows(kn, ap.as_ptr(), m, bp, np, half, &cp[..m], ncols, acc, bias_p);
                        r += m;
                        continue;
                    }
                    // SSE / portable: the 4 x 16 tile on the rows in place
                    let mut q = 0;
                    while q < m {
                        let mm = (m - q).min(4);
                        let mut aq = [core::ptr::null::<f32>(); 4];
                        let mut cq = [core::ptr::null_mut::<f32>(); 4];
                        for i in 0..mm {
                            aq[i] = ap[q + i];
                            cq[i] = cp[q + i];
                        }
                        debug_assert!(!half, "f16 panels need the AVX2 / AVX-512 kernels");
                        tile(kn, &aq, mm, bp[0] as *const f32, &cq, ncols.min(16), acc, bias_p);
                        q += mm;
                    }
                    r += m;
                }
                p += np;
            }
            k0 += kn;
        }
        r0 += m_all;
    }
}

// ── f16 -> f32 ───────────────────────────────────────────────────────────────

static LUT: AtomicUsize = AtomicUsize::new(0);

/// The 64 Ki-entry half -> single table for the non-F16C path (built once, on the calling core).
pub fn init_lut() {
    if LUT.load(Relaxed) != 0 {
        return;
    }
    let mut t = alloc::vec![0f32; 65536];
    for (i, v) in t.iter_mut().enumerate() {
        *v = crate::f16::to_f32(i as u16);
    }
    let p = alloc::boxed::Box::leak(t.into_boxed_slice()).as_ptr();
    LUT.store(p as usize, Relaxed);
}

/// `dst[i] = f32(src[i])` for i < n. `init_lut` must have run.
pub unsafe fn cvt_f16(src: *const u16, dst: *mut f32, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        if avx512() {
            return cvt_f16_avx512(src, dst, n);
        }
        if AVX2.load(Relaxed) {
            return cvt_f16c(src, dst, n);
        }
    }
    let lut = LUT.load(Relaxed) as *const f32;
    if lut.is_null() {
        for i in 0..n {
            *dst.add(i) = crate::f16::to_f32(*src.add(i));
        }
        return;
    }
    for i in 0..n {
        *dst.add(i) = *lut.add(*src.add(i) as usize);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,f16c")]
unsafe fn cvt_f16c(src: *const u16, dst: *mut f32, n: usize) {
    use core::arch::x86_64::*;
    let mut i = 0;
    while i + 8 <= n {
        let h = _mm_loadu_si128(src.add(i) as *const __m128i);
        _mm256_storeu_ps(dst.add(i), _mm256_cvtph_ps(h));
        i += 8;
    }
    while i < n {
        *dst.add(i) = crate::f16::to_f32(*src.add(i));
        i += 1;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx2,f16c")]
unsafe fn cvt_f16_avx512(src: *const u16, dst: *mut f32, n: usize) {
    use core::arch::x86_64::*;
    let mut i = 0;
    while i + 16 <= n {
        let h = _mm256_loadu_si256(src.add(i) as *const __m256i);
        _mm512_storeu_ps(dst.add(i), _mm512_cvtph_ps(h));
        i += 16;
    }
    while i < n {
        *dst.add(i) = crate::f16::to_f32(*src.add(i));
        i += 1;
    }
}

// ── exp over a slice (softmax, SiLU) ──────────────────────────────────────────

/// `x[i] = exp(x[i])`, Cephes' expf (about 1 ulp) four or eight at a time.
pub fn vexp(x: &mut [f32]) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        if avx512() {
            return vexp_avx512(x);
        }
        if AVX2.load(Relaxed) {
            return vexp_avx2(x);
        }
        return vexp_sse(x);
    }
    #[allow(unreachable_code)]
    for v in x.iter_mut() {
        *v = libm::expf(*v);
    }
}

const EXP_HI: f32 = 88.376_26;
const EXP_LO: f32 = -88.376_26;
const LOG2E: f32 = 1.442_695;
const C1: f32 = 0.693_359_4;
const C2: f32 = -2.121_944_4e-4;
const P0: f32 = 1.987_569_1e-4;
const P1: f32 = 1.398_199_9e-3;
const P2: f32 = 8.333_452e-3;
const P3: f32 = 4.166_579_6e-2;
const P4: f32 = 1.666_666_5e-1;
const P5: f32 = 5.000_000_1e-1;

#[inline(always)]
fn exp_scalar(x: f32) -> f32 {
    libm::expf(x)
}

#[cfg(target_arch = "x86_64")]
unsafe fn vexp_sse(x: &mut [f32]) {
    use core::arch::x86_64::*;
    let n = x.len();
    let p = x.as_mut_ptr();
    let mut i = 0;
    while i + 4 <= n {
        let mut v = _mm_loadu_ps(p.add(i));
        v = _mm_min_ps(_mm_max_ps(v, _mm_set1_ps(EXP_LO)), _mm_set1_ps(EXP_HI));
        let mut fx = _mm_add_ps(_mm_mul_ps(v, _mm_set1_ps(LOG2E)), _mm_set1_ps(0.5));
        // floor without SSE4.1: truncate, then step down where that rounded up
        let t = _mm_cvtepi32_ps(_mm_cvttps_epi32(fx));
        let up = _mm_and_ps(_mm_cmpgt_ps(t, fx), _mm_set1_ps(1.0));
        fx = _mm_sub_ps(t, up);
        v = _mm_sub_ps(v, _mm_mul_ps(fx, _mm_set1_ps(C1)));
        v = _mm_sub_ps(v, _mm_mul_ps(fx, _mm_set1_ps(C2)));
        let z = _mm_mul_ps(v, v);
        let mut y = _mm_set1_ps(P0);
        y = _mm_add_ps(_mm_mul_ps(y, v), _mm_set1_ps(P1));
        y = _mm_add_ps(_mm_mul_ps(y, v), _mm_set1_ps(P2));
        y = _mm_add_ps(_mm_mul_ps(y, v), _mm_set1_ps(P3));
        y = _mm_add_ps(_mm_mul_ps(y, v), _mm_set1_ps(P4));
        y = _mm_add_ps(_mm_mul_ps(y, v), _mm_set1_ps(P5));
        y = _mm_add_ps(_mm_mul_ps(y, z), v);
        y = _mm_add_ps(y, _mm_set1_ps(1.0));
        let e = _mm_slli_epi32(_mm_add_epi32(_mm_cvttps_epi32(fx), _mm_set1_epi32(127)), 23);
        _mm_storeu_ps(p.add(i), _mm_mul_ps(y, _mm_castsi128_ps(e)));
        i += 4;
    }
    while i < n {
        *p.add(i) = exp_scalar(*p.add(i));
        i += 1;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn vexp_avx2(x: &mut [f32]) {
    use core::arch::x86_64::*;
    let n = x.len();
    let p = x.as_mut_ptr();
    let mut i = 0;
    while i + 8 <= n {
        let mut v = _mm256_loadu_ps(p.add(i));
        v = _mm256_min_ps(_mm256_max_ps(v, _mm256_set1_ps(EXP_LO)), _mm256_set1_ps(EXP_HI));
        let fx = _mm256_floor_ps(_mm256_fmadd_ps(v, _mm256_set1_ps(LOG2E), _mm256_set1_ps(0.5)));
        v = _mm256_fnmadd_ps(fx, _mm256_set1_ps(C1), v);
        v = _mm256_fnmadd_ps(fx, _mm256_set1_ps(C2), v);
        let z = _mm256_mul_ps(v, v);
        let mut y = _mm256_set1_ps(P0);
        y = _mm256_fmadd_ps(y, v, _mm256_set1_ps(P1));
        y = _mm256_fmadd_ps(y, v, _mm256_set1_ps(P2));
        y = _mm256_fmadd_ps(y, v, _mm256_set1_ps(P3));
        y = _mm256_fmadd_ps(y, v, _mm256_set1_ps(P4));
        y = _mm256_fmadd_ps(y, v, _mm256_set1_ps(P5));
        y = _mm256_fmadd_ps(y, z, v);
        y = _mm256_add_ps(y, _mm256_set1_ps(1.0));
        let e = _mm256_slli_epi32(_mm256_add_epi32(_mm256_cvttps_epi32(fx), _mm256_set1_epi32(127)), 23);
        _mm256_storeu_ps(p.add(i), _mm256_mul_ps(y, _mm256_castsi256_ps(e)));
        i += 8;
    }
    while i < n {
        *p.add(i) = exp_scalar(*p.add(i));
        i += 1;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx2,fma")]
unsafe fn vexp_avx512(x: &mut [f32]) {
    use core::arch::x86_64::*;
    let n = x.len();
    let p = x.as_mut_ptr();
    let mut i = 0;
    while i + 16 <= n {
        let mut v = _mm512_loadu_ps(p.add(i));
        v = _mm512_min_ps(_mm512_max_ps(v, _mm512_set1_ps(EXP_LO)), _mm512_set1_ps(EXP_HI));
        // floor: round toward -inf (imm 0x01), exceptions suppressed (0x08)
        let fx = _mm512_roundscale_ps::<0x09>(_mm512_fmadd_ps(v, _mm512_set1_ps(LOG2E), _mm512_set1_ps(0.5)));
        v = _mm512_fnmadd_ps(fx, _mm512_set1_ps(C1), v);
        v = _mm512_fnmadd_ps(fx, _mm512_set1_ps(C2), v);
        let z = _mm512_mul_ps(v, v);
        let mut y = _mm512_set1_ps(P0);
        y = _mm512_fmadd_ps(y, v, _mm512_set1_ps(P1));
        y = _mm512_fmadd_ps(y, v, _mm512_set1_ps(P2));
        y = _mm512_fmadd_ps(y, v, _mm512_set1_ps(P3));
        y = _mm512_fmadd_ps(y, v, _mm512_set1_ps(P4));
        y = _mm512_fmadd_ps(y, v, _mm512_set1_ps(P5));
        y = _mm512_fmadd_ps(y, z, v);
        y = _mm512_add_ps(y, _mm512_set1_ps(1.0));
        let e = _mm512_slli_epi32::<23>(_mm512_add_epi32(_mm512_cvttps_epi32(fx), _mm512_set1_epi32(127)));
        _mm512_storeu_ps(p.add(i), _mm512_mul_ps(y, _mm512_castsi512_ps(e)));
        i += 16;
    }
    if i < n {
        vexp_avx2(&mut x[i..]);
    }
}

// ── row helpers (softmax, norms, GELU) ────────────────────────────────────────

/// Max of a slice (8 lanes at a time; the compiler vectorizes the lane loop).
pub fn vmax(x: &[f32]) -> f32 {
    let mut m = [f32::MIN; 8];
    let mut ch = x.chunks_exact(8);
    for c in &mut ch {
        for j in 0..8 {
            m[j] = if c[j] > m[j] { c[j] } else { m[j] };
        }
    }
    let mut r = f32::MIN;
    for &v in m.iter().chain(ch.remainder().iter()) {
        if v > r {
            r = v;
        }
    }
    r
}

/// Sum of a slice in 16 partial sums (vectorizes; the order differs from a running sum by rounding only).
pub fn vsum(x: &[f32]) -> f32 {
    let mut s = [0f32; 16];
    let mut ch = x.chunks_exact(16);
    for c in &mut ch {
        for j in 0..16 {
            s[j] += c[j];
        }
    }
    let mut t = 0f32;
    for v in s.iter().chain(ch.remainder().iter()) {
        t += *v;
    }
    t
}

/// (sum, sum of squares) of a slice, 16 lanes at a time.
pub fn vsum2(x: &[f32]) -> (f32, f32) {
    let mut s = [0f32; 16];
    let mut q = [0f32; 16];
    let mut ch = x.chunks_exact(16);
    for c in &mut ch {
        for j in 0..16 {
            s[j] += c[j];
            q[j] += c[j] * c[j];
        }
    }
    let (mut ts, mut tq) = (0f32, 0f32);
    for j in 0..16 {
        ts += s[j];
        tq += q[j];
    }
    for v in ch.remainder() {
        ts += *v;
        tq += *v * *v;
    }
    (ts, tq)
}

/// In place: `row = softmax(row)` (the scale is already in the scores).
pub fn softmax(row: &mut [f32]) {
    if row.is_empty() {
        return;
    }
    let m = vmax(row);
    for e in row.iter_mut() {
        *e -= m;
    }
    vexp(row);
    let inv = 1.0 / vsum(row);
    for e in row.iter_mut() {
        *e *= inv;
    }
}

/// `d[i] = s[i] * gelu(g[i])` with the exact (erf) GELU, 64 at a time: erf from Abramowitz & Stegun 7.1.26 (|error| < 1.5e-7)
/// with the vectorized exp -- libm's erff one element at a time was most of the feed-forward's non-GEMM time.
pub fn geglu_erf(s: &[f32], g: &[f32], d: &mut [f32]) {
    const P: f32 = 0.327_591_1;
    const A1: f32 = 0.254_829_6;
    const A2: f32 = -0.284_496_74;
    const A3: f32 = 1.421_413_7;
    const A4: f32 = -1.453_152_1;
    const A5: f32 = 1.061_405_4;
    let n = d.len().min(s.len()).min(g.len());
    let mut i = 0;
    let mut t = [0f32; 64];
    let mut e = [0f32; 64];
    while i < n {
        let m = (n - i).min(64);
        for j in 0..m {
            let z = g[i + j] * core::f32::consts::FRAC_1_SQRT_2;
            e[j] = -(z * z);
            t[j] = 1.0 / (1.0 + P * if z < 0.0 { -z } else { z });
        }
        vexp(&mut e[..m]);
        for j in 0..m {
            let x = g[i + j];
            let tt = t[j];
            let poly = ((((A5 * tt + A4) * tt + A3) * tt + A2) * tt + A1) * tt;
            let erf_abs = 1.0 - poly * e[j];
            let erf = if x < 0.0 { -erf_abs } else { erf_abs };
            d[i + j] = s[i + j] * (0.5 * x * (1.0 + erf));
        }
        i += m;
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::vec;

    fn check_tile() {
        let k = 37;
        let a: std::vec::Vec<std::vec::Vec<f32>> = (0..4).map(|r| (0..k).map(|i| ((r * 31 + i * 7) % 13) as f32 * 0.1 - 0.5).collect()).collect();
        let b: std::vec::Vec<f32> = (0..k * 16).map(|i| ((i * 11) % 17) as f32 * 0.05 - 0.4).collect();
        let bias: std::vec::Vec<f32> = (0..16).map(|j| j as f32 * 0.01).collect();
        for rows in 1..=4 {
            for ncols in [3usize, 8, 11, 16] {
                let mut c = vec![vec![1.0f32; 16]; 4];
                let ap = [a[0].as_ptr(), a[1].as_ptr(), a[2].as_ptr(), a[3].as_ptr()];
                let cp = [c[0].as_mut_ptr(), c[1].as_mut_ptr(), c[2].as_mut_ptr(), c[3].as_mut_ptr()];
                unsafe { tile(k, &ap, rows, b.as_ptr(), &cp, ncols, true, bias.as_ptr()) };
                for r in 0..4 {
                    for j in 0..16 {
                        let want = if r < rows && j < ncols { 1.0 + bias[j] + (0..k).map(|kk| a[r][kk] * b[kk * 16 + j]).sum::<f32>() } else { 1.0 };
                        assert!((c[r][j] - want).abs() < 1e-4, "rows {rows} ncols {ncols} r {r} j {j}: {} vs {want}", c[r][j]);
                    }
                }
            }
        }
    }

    /// gemm_job against a plain triple loop: every row count up to a few strips, odd panel counts, column tails, K blocks,
    /// bias / accumulate.
    fn check_gemm() {
        for &(k, rows, npan, n) in &[(37usize, 1usize, 1usize, 16usize), (40, 13, 3, 40), (300, 25, 2, 32), (1100, 64, 5, 77), (2100, 7, 4, 64), (16, 30, 1, 3)] {
            let a: std::vec::Vec<f32> = (0..rows * k).map(|i| ((i * 31) % 29) as f32 * 0.03 - 0.4).collect();
            let b32: std::vec::Vec<f32> = (0..npan * k * 16).map(|i| ((i * 11) % 17) as f32 * 0.05 - 0.4).collect();
            let b16: std::vec::Vec<u16> = b32.iter().map(|v| crate::f16::from_f32(*v)).collect();
            let bias: std::vec::Vec<f32> = (0..npan * 16).map(|j| j as f32 * 0.01).collect();
            for (acc, with_bias, half) in [(false, true, false), (true, false, false), (true, true, false), (false, false, false), (true, true, true), (false, false, true)] {
                if half && !avx2() {
                    continue;
                }
                // with f16 panels the reference uses the rounded weights
                let b: std::vec::Vec<f32> = if half { b16.iter().map(|h| crate::f16::to_f32(*h)).collect() } else { b32.clone() };
                let bptr = if half { b16.as_ptr() as *const f32 } else { b32.as_ptr() };
                let es = if half { 2 } else { 4 };
                let ldc = npan * 16 + 5; // C rows wider than n: the columns past n must stay untouched
                let mut c = vec![0.5f32; rows * ldc];
                let cp = P(c.as_mut_ptr());
                unsafe {
                    gemm_job(
                        k,
                        rows,
                        &|i: usize| a.as_ptr().add(i * k),
                        &|i: usize| cp.get().add(i * ldc),
                        &|p: usize| (bptr as *const u8).add(p * k * 16 * es) as *const f32,
                        0,
                        npan,
                        n,
                        acc,
                        if with_bias { bias.as_ptr() } else { core::ptr::null() },
                        half,
                    );
                }
                for r in 0..rows {
                    for j in 0..ldc {
                        let want = if j < n {
                            let (p, jj) = (j / 16, j % 16);
                            let mut s = 0f64;
                            for kk in 0..k {
                                s += a[r * k + kk] as f64 * b[(p * k + kk) * 16 + jj] as f64;
                            }
                            s as f32 + if with_bias { bias[j] } else { 0.0 } + if acc { 0.5 } else { 0.0 }
                        } else {
                            0.5
                        };
                        let got = c[r * ldc + j];
                        assert!((got - want).abs() < 1e-3 * (1.0 + want.abs()), "{} k {k} rows {rows} npan {npan} n {n} acc {acc} bias {with_bias} half {half}: r {r} j {j}: {got} vs {want}", simd_name());
                    }
                }
            }
        }
    }

    fn check_rows() {
        let x: std::vec::Vec<f32> = (0..1000).map(|i| ((i * 37) % 101) as f32 * 0.1 - 5.0).collect();
        let m = x.iter().cloned().fold(f32::MIN, f32::max);
        assert_eq!(vmax(&x), m);
        let s: f64 = x.iter().map(|v| *v as f64).sum();
        assert!((vsum(&x) as f64 - s).abs() < 1e-3);
        let mut sm = x[..77].to_vec();
        softmax(&mut sm);
        let mx = x[..77].iter().cloned().fold(f32::MIN, f32::max) as f64;
        let z: f64 = x[..77].iter().map(|v| (*v as f64 - mx).exp()).sum();
        for i in 0..77 {
            let w = (x[i] as f64 - mx).exp() / z;
            assert!((sm[i] as f64 - w).abs() < 1e-6 + 1e-5 * w, "softmax {i}: {} vs {w}", sm[i]);
        }
        let g: std::vec::Vec<f32> = (0..300).map(|i| i as f32 * 0.04 - 6.0).collect();
        let one = vec![1.0f32; 300];
        let mut d = vec![0f32; 300];
        geglu_erf(&one, &g, &mut d);
        for i in 0..300 {
            let w = 0.5 * g[i] as f64 * (1.0 + erf64(g[i] as f64 / core::f64::consts::SQRT_2));
            assert!((d[i] as f64 - w).abs() < 2e-6 * (1.0 + w.abs()), "gelu {}: {} vs {w}", g[i], d[i]);
        }
    }

    fn erf64(x: f64) -> f64 {
        libm::erf(x)
    }

    /// One core's GEMM speed on cache-resident data: `cargo test -p ling-sd --release --lib gemm_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn gemm_speed() {
        let ev = |n: &str, d: usize| std::env::var(n).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        let (k, rows, npan, lda) = (ev("GK", 640), ev("GROWS", 64), ev("GNPAN", 8), ev("GLDA", 0));
        let lda = if lda == 0 { k } else { lda };
        let a: std::vec::Vec<f32> = (0..rows * lda).map(|i| (i % 7) as f32 * 0.1).collect();
        let b: std::vec::Vec<f32> = (0..npan * k * 16).map(|i| (i % 5) as f32 * 0.1).collect();
        let mut c = vec![0f32; rows * npan * 16];
        let cp = P(c.as_mut_ptr());
        let mut paths = vec![(false, false)];
        if std::is_x86_feature_detected!("avx2") {
            paths.push((true, false));
        }
        if std::is_x86_feature_detected!("avx512f") {
            paths.push((true, true));
        }
        for (p, p512) in paths {
            set_avx2(p);
            set_avx512(p512);
            let reps: usize = std::env::var("REPS").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
            let t0 = std::time::Instant::now();
            for _ in 0..reps {
                unsafe {
                    gemm_job(k, rows, &|i: usize| a.as_ptr().add(i * lda), &|i: usize| cp.get().add(i * npan * 16), &|p: usize| b.as_ptr().add(p * k * 16), 0, npan, npan * 16, false, core::ptr::null(), false);
                }
            }
            let s = t0.elapsed().as_secs_f64();
            std::println!("{}: {:.1} GFLOPS on one core", simd_name(), (2 * k * rows * npan * 16 * reps) as f64 / s * 1e-9);
        }
        set_avx2(false);
        set_avx512(false);
    }

    #[test]
    fn tiles_and_exp_agree_on_every_path() {
        // (avx2, avx512): portable/SSE always; AVX2 and AVX-512 where this CPU has them
        let mut paths = vec![(false, false)];
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma") && std::is_x86_feature_detected!("f16c") {
            paths.push((true, false));
            if std::is_x86_feature_detected!("avx512f") {
                paths.push((true, true));
            } else {
                std::println!("this CPU has no AVX-512F: the AVX-512 kernels are not exercised here");
            }
        }
        for (p, p512) in paths {
            set_avx2(p);
            set_avx512(p512);
            check_tile();
            check_gemm();
            check_rows();
            let mut x: std::vec::Vec<f32> = (0..103).map(|i| i as f32 * 0.37 - 20.0).collect();
            let want: std::vec::Vec<f32> = x.iter().map(|v| v.exp()).collect();
            vexp(&mut x);
            for (g, w) in x.iter().zip(want.iter()) {
                assert!(((g - w) / w).abs() < 2e-6, "{g} vs {w}");
            }
            init_lut();
            let h: std::vec::Vec<u16> = (0..50u16).map(|i| i * 1311).collect();
            let mut out = vec![0f32; 50];
            unsafe { cvt_f16(h.as_ptr(), out.as_mut_ptr(), 50) };
            for i in 0..50 {
                let w = crate::f16::to_f32(h[i]);
                assert!(out[i] == w || (out[i].is_nan() && w.is_nan()));
            }
        }
        set_avx2(false);
        set_avx512(false);
    }
}
