//! Single-layer speed (synthetic weights, no checkpoint): the UNet's convolution / linear / attention shapes one at a time.
//!   SD_THREADS=16 cargo test -p ling-sd --release --test layer_bench -- --ignored --nocapture
//! SD_AVX512=1 uses the AVX-512 kernels where the CPU has them, SD_SSE=1 the SSE ones.
use ling_sd::act::Act;
use ling_sd::ops::{Conv, Eng, In, Lin, Wts, W16, W32};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

fn threads() -> usize {
    std::env::var("SD_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(16)
}

static GEN: AtomicUsize = AtomicUsize::new(0);
static STATE: AtomicU64 = AtomicU64::new(0);
static DONE: AtomicUsize = AtomicUsize::new(0);
static CTX: AtomicUsize = AtomicUsize::new(0);
static FUN: AtomicUsize = AtomicUsize::new(0);
static POOL: std::sync::Once = std::sync::Once::new();

fn claim_and_run(g: u64) {
    loop {
        let s = STATE.load(Ordering::Acquire);
        let (sg, n, i) = (s >> 48, (s >> 24) & 0xFF_FFFF, s & 0xFF_FFFF);
        if sg != g || i >= n {
            return;
        }
        if STATE.compare_exchange_weak(s, s + 1, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            continue;
        }
        let (ctx, f) = (CTX.load(Ordering::Acquire), FUN.load(Ordering::Acquire));
        let f: unsafe fn(*const (), usize) = unsafe { std::mem::transmute(f) };
        unsafe { f(ctx as *const (), i as usize) };
        DONE.fetch_add(1, Ordering::AcqRel);
    }
}

fn par(n: usize, ctx: *const (), f: unsafe fn(*const (), usize)) {
    POOL.call_once(|| {
        for _ in 1..threads() {
            std::thread::spawn(|| {
                let mut seen = 0;
                loop {
                    let mut spins = 0u32;
                    while GEN.load(Ordering::Acquire) == seen {
                        spins += 1;
                        if spins < 20000 {
                            std::hint::spin_loop();
                        } else {
                            std::thread::yield_now();
                        }
                    }
                    seen = GEN.load(Ordering::Acquire);
                    claim_and_run((seen & 0xFFFF) as u64);
                }
            });
        }
    });
    CTX.store(ctx as usize, Ordering::Release);
    FUN.store(f as usize, Ordering::Release);
    DONE.store(0, Ordering::Release);
    let g = ((GEN.load(Ordering::Acquire) + 1) & 0xFFFF) as u64;
    STATE.store(g << 48 | (n as u64) << 24, Ordering::Release);
    GEN.fetch_add(1, Ordering::AcqRel);
    claim_and_run(g);
    while DONE.load(Ordering::Acquire) < n {
        std::hint::spin_loop();
    }
}

/// f16 weights (as LingOS holds them), or f32 with SD_F32=1
fn wts(n: usize) -> Wts {
    let v: Vec<f32> = (0..n).map(|i| ((i * 7919) % 1000) as f32 * 1e-4 - 0.05).collect();
    if std::env::var("SD_F32").is_ok() {
        let p = Box::leak(v.into_boxed_slice());
        return Wts { h: W16 { ptr: std::ptr::null_mut(), len: 0 }, f: W32 { ptr: p.as_mut_ptr(), len: p.len() } };
    }
    let h: Vec<u16> = v.iter().map(|x| ling_sd::f16::from_f32(*x)).collect();
    let p = Box::leak(h.into_boxed_slice());
    Wts { h: W16 { ptr: p.as_mut_ptr(), len: p.len() }, f: W32::NONE }
}

fn act(h: usize, w: usize, c: usize) -> Act {
    let mut a = Act::new(h, w, c);
    for (i, p) in a.pieces_mut().enumerate() {
        for (j, v) in p.iter_mut().enumerate() {
            *v = (((i * 31 + j) * 13) % 97) as f32 * 0.01 - 0.5;
        }
    }
    a
}

fn time<F: FnMut()>(mut f: F) -> f64 {
    f();
    let mut best = f64::MAX;
    for _ in 0..3 {
        let t0 = Instant::now();
        f();
        best = best.min(t0.elapsed().as_secs_f64());
    }
    best
}

#[test]
#[ignore]
fn layer_speeds() {
    ling_sd::kern::set_parallel(par, threads());
    let avx = std::env::var("SD_SSE").is_err() && is_x86_feature_detected!("avx2");
    ling_sd::kern::set_avx2(avx);
    ling_sd::kern::set_avx512(avx && std::env::var("SD_AVX512").is_ok() && is_x86_feature_detected!("avx512f"));
    if let Some(v) = std::env::var("SD_KC").ok().and_then(|v| v.parse().ok()) {
        ling_sd::kern::set_kc(v);
    }
    println!("{} threads, {}, K block {}", threads(), ling_sd::kern::simd_name(), ling_sd::kern::kc());
    let mut e = Eng::new();
    for &(hw, cin, n) in &[(64usize, 320usize, 320usize), (32, 640, 640), (16, 1280, 1280), (8, 1280, 1280), (16, 2560, 1280)] {
        let npan = n.div_ceil(16);
        let cv = Conv { cin, n, npan, w: wts(9 * npan * cin * 16), bias: vec![0.01; npan * 16] };
        let x = act(hw, hw, cin);
        let s = time(|| drop(e.conv3(&cv, In { x: &x, up: false, aff: None, silu: false }, 1, None, None)));
        let fl = 2.0 * (hw * hw * n * cin * 9) as f64;
        println!("LAYER conv {hw}x{hw} {cin}->{n}: {:.2} ms, {:.0} GFLOPS", s * 1e3, fl / s * 1e-9);
    }
    for &(t, k, n) in &[(4096usize, 320usize, 320usize), (4096, 320, 2560), (1024, 640, 640), (256, 1280, 10240), (256, 5120, 1280)] {
        let npan = n.div_ceil(16);
        let l = Lin { k, n, npan, w: wts(npan * k * 16), bias: Some(vec![0.01; npan * 16]) };
        let x = act(1, t, k);
        let s = time(|| drop(e.linear(&l, &x, None)));
        let fl = 2.0 * (t * n * k) as f64;
        println!("LAYER linear {t} x {k}->{n}: {:.2} ms, {:.0} GFLOPS", s * 1e3, fl / s * 1e-9);
    }
    for &(t, s_n, c) in &[(4096usize, 4096usize, 320usize), (1024, 1024, 640), (4096, 77, 320)] {
        let q = act(1, t, c);
        let k = act(1, s_n, c);
        let v = act(1, s_n, c);
        let s = time(|| drop(e.attention(&q, &k, &v, 8, false)));
        let fl = 4.0 * (t * s_n * c) as f64;
        println!("LAYER attention {t} x {s_n} x {c}: {:.2} ms, {:.0} GFLOPS", s * 1e3, fl / s * 1e-9);
    }
}
