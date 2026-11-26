//! Tokens per second at each SIMD level this CPU has (portable / AVX2 / AVX-512 VNNI), on a persistent thread pool like LingOS's
//! worker cores.
//!   LLM_THREADS=16 cargo test -p ling-llm --features std --release --test bench -- --ignored --nocapture
//! LING_MODEL picks the GGUF (default: LingStationX's Qwen3-0.6B-Q8_0).
#![cfg(feature = "std")]
use ling_llm::chat::build_prompt;
use ling_llm::gguf::Source;
use ling_llm::{Error, Llm};
use std::fs::File;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

struct FileSrc {
    f: File,
    len: u64,
}
impl Source for FileSrc {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        #[cfg(windows)]
        use std::os::windows::fs::FileExt;
        #[cfg(unix)]
        use std::os::unix::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            #[cfg(windows)]
            let r = self.f.seek_read(&mut buf[done..], offset + done as u64);
            #[cfg(unix)]
            let r = self.f.read_at(&mut buf[done..], offset + done as u64);
            match r {
                Ok(0) | Err(_) => return Err(Error::Io),
                Ok(n) => done += n,
            }
        }
        Ok(())
    }
}

fn threads() -> usize {
    std::env::var("LLM_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(16)
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

#[test]
#[ignore]
fn tokens_per_second() {
    let path = std::env::var("LING_MODEL").unwrap_or_else(|_| "models/Qwen3-0.6B-Q8_0.gguf".into());
    let Ok(f) = File::open(&path) else { return println!("no model at {path}: skipped") };
    let src = FileSrc { len: f.metadata().unwrap().len(), f };
    ling_llm::quant::set_parallel(par);
    let llm = Llm::load(&src, &mut |_, _| {}).unwrap();
    let prompt = build_prompt("You are terse.", "<|user|>\nWrite a long story about a lighthouse keeper.\n");
    let ids = llm.tok.encode(&prompt);
    let mut levels = vec![ling_llm::quant::SIMD_NONE];
    if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") && is_x86_feature_detected!("f16c") {
        levels.push(ling_llm::quant::SIMD_AVX2);
        if is_x86_feature_detected!("avx512vnni") && is_x86_feature_detected!("avx512vl") && is_x86_feature_detected!("avx512bw") {
            levels.push(ling_llm::quant::SIMD_VNNI);
        }
    }
    let n = std::env::var("LLM_TOKENS").ok().and_then(|v| v.parse().ok()).unwrap_or(32);
    // the output layer alone (one matrix-vector product over all its rows): what the dot products and the cores give
    {
        let g = ling_llm::gguf::Gguf::parse(&src).unwrap();
        let t = g.tensor("token_embd.weight").unwrap();
        let (rows, cols) = t.shape2();
        let w = ling_llm::quant::QTensor::load(&src, g.data_start + t.offset, rows, cols, t.dtype_or_err().unwrap()).unwrap();
        let mut x = ling_llm::quant::XBatch::new(1, cols);
        for (i, v) in x.x.iter_mut().enumerate() {
            *v = ((i * 37) % 11) as f32 * 0.1 - 0.5;
        }
        let mut out = vec![0f32; rows];
        for &l in &levels {
            ling_llm::quant::set_simd(l);
            x.set_len(1);
            ling_llm::quant::matmul(&w, &mut x, &mut out);
            let t0 = Instant::now();
            for _ in 0..5 {
                x.set_len(1);
                ling_llm::quant::matmul(&w, &mut x, &mut out);
            }
            let s = t0.elapsed().as_secs_f64() / 5.0;
            println!("LLM {} {} threads: output layer {rows} x {cols} in {:.1} ms = {:.1} GB/s of weights", ling_llm::quant::simd_name(), threads(), s * 1e3, w.bytes() as f64 / s * 1e-9);
        }
    }
    for l in levels {
        ling_llm::quant::set_simd(l);
        let mut st = llm.model.new_state(512);
        let t0 = Instant::now();
        llm.prefill(&mut st, &ids, &mut |_, _| {}).unwrap();
        let tp = t0.elapsed().as_secs_f64();
        let argmax = |v: &[f32]| v.iter().enumerate().fold((0usize, f32::NEG_INFINITY), |a, (i, x)| if *x > a.1 { (i, *x) } else { a }).0 as u32;
        let mut next = argmax(&st.logits);
        let mut out = vec![];
        let t1 = Instant::now();
        for _ in 0..n {
            out.push(next);
            llm.model.forward(&mut st, &[next], true).unwrap();
            next = argmax(&st.logits);
        }
        let td = t1.elapsed().as_secs_f64();
        println!(
            "LLM {} {} threads: prefill {} tokens {:.1} tok/s, decode {:.2} tok/s ({:.0} ms a token) -> {:?}",
            ling_llm::quant::simd_name(),
            threads(),
            ids.len(),
            ids.len() as f64 / tp,
            n as f64 / td,
            td * 1000.0 / n as f64,
            llm.tok.decode(&out).chars().take(60).collect::<String>()
        );
    }
}
