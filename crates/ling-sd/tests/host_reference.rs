//! ling-sd against the host engine (crates/ling-diffusion, candle on the CPU in f32), same checkpoint, same inputs.
//!
//!   cargo test -p ling-sd --release --test host_reference -- --nocapture --test-threads 1
//!
//! SD_CKPT = an SD 1.5 single-file checkpoint (default: the WebUI install's "A.I. Love You"), SD_CLIP = the folder with vocab.json
//! + merges.txt (default: ../LingDiffusion/models/clip). SD_SSE=1 forces the SSE kernels. Nothing is written next to the model.
use candle_core::{DType, Device, Tensor};
use ling_diffusion::pipeline::{self, Denoiser, LoadOpts};
use ling_diffusion::sampling::{Host, Table};
use ling_sd::act::Act;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

struct FileSrc {
    f: std::fs::File,
    len: u64,
}

impl ling_sd::Source for FileSrc {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, off: u64, buf: &mut [u8]) -> bool {
        #[cfg(windows)]
        use std::os::windows::fs::FileExt;
        #[cfg(unix)]
        use std::os::unix::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            #[cfg(windows)]
            let r = self.f.seek_read(&mut buf[done..], off + done as u64);
            #[cfg(unix)]
            let r = self.f.read_at(&mut buf[done..], off + done as u64);
            match r {
                Ok(0) | Err(_) => return false,
                Ok(n) => done += n,
            }
        }
        true
    }
}

fn threads() -> usize {
    std::env::var("SD_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
}

// A persistent pool, like LingOS's worker cores (spawning threads per call cost more than some of the layers). STATE packs
// (generation, job count, next job); a claim is a CAS that only succeeds for the current generation, so a worker that wakes late
// can never run an old call's function on a new call's index. CTX / FUN are read after a successful claim: the call is then
// still running (that job is not done), so they still hold its values.
static GEN: AtomicUsize = AtomicUsize::new(0);
static STATE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
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

static TRACE: std::sync::Mutex<Vec<(usize, usize, usize, usize, u64, u64)>> = std::sync::Mutex::new(Vec::new());

fn trace(b: usize, m: usize, n: usize, k: usize, ns: u64) {
    let mut t = TRACE.lock().unwrap();
    if let Some(e) = t.iter_mut().find(|e| e.0 == b && e.1 == m && e.2 == n && e.3 == k) {
        e.4 += ns;
        e.5 += 1;
    } else {
        t.push((b, m, n, k, ns, 1));
    }
}

fn print_trace(scale: f64) {
    let mut t = TRACE.lock().unwrap();
    t.sort_by(|a, b| b.4.cmp(&a.4));
    let names = ling_sd::kern::PROF_NAMES;
    for e in t.iter().take(40) {
        let flops = 2.0 * e.1 as f64 * e.2 as f64 * e.3 as f64 * e.5 as f64 * if e.0 == ling_sd::kern::PROF_ATTN { 2.0 } else { 1.0 };
        println!("TRACE {:<10} m {:>6} n {:>6} k {:>6} x{:<3} {:>7.1} ms  {:>6.0} GFLOPS", names[e.0], e.1, e.2, e.3, e.5 as f64 * scale, e.4 as f64 * 1e-6 * scale, flops / e.4 as f64);
    }
    t.clear();
}

fn clock_ns() -> u64 {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_nanos() as u64 + 1
}

fn setup() {
    ling_sd::kern::set_parallel(par, threads());
    if std::env::var("SD_PROFILE").is_ok() {
        ling_sd::kern::set_clock(clock_ns);
    }
    if std::env::var("SD_TRACE").is_ok() {
        ling_sd::kern::set_trace(trace);
    }
    // SD_F32=1: keep the weights widened to f32 (what LingOS does when the RAM above 4 GiB holds them)
    ling_sd::ops::set_f32_weights(std::env::var("SD_F32").is_ok());
    // SD_AVX512=1: the AVX-512 kernels (only where the CPU has AVX-512F)
    ling_sd::kern::set_avx512(std::env::var("SD_AVX512").is_ok() && is_x86_feature_detected!("avx512f"));
    let avx = std::env::var("SD_SSE").is_err() && is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") && is_x86_feature_detected!("f16c");
    ling_sd::kern::set_avx2(avx);
    println!("kernels: {}, {} threads, weights {}", ling_sd::kern::simd_name(), threads(), if ling_sd::ops::f32_weights() { "f32" } else { "f16 (widened per layer)" });
}

fn ckpt() -> String {
    std::env::var("SD_CKPT").unwrap_or_default()
}

fn clip_dir() -> String {
    std::env::var("SD_CLIP").unwrap_or_else(|_| "models/clip".into())
}

fn diff(name: &str, got: &[f32], want: &[f32]) -> f64 {
    assert_eq!(got.len(), want.len(), "{name}: lengths");
    let mut max = 0f64;
    let (mut d2, mut w2) = (0f64, 0f64);
    for (g, w) in got.iter().zip(want.iter()) {
        let d = (*g as f64 - *w as f64).abs();
        max = max.max(d);
        d2 += d * d;
        w2 += (*w as f64) * (*w as f64);
    }
    let rel = (d2 / w2.max(1e-30)).sqrt();
    println!("  {name}: max |diff| {max:.3e}, relative L2 {rel:.3e} ({} values)", got.len());
    rel
}

fn flat(t: &Tensor) -> Vec<f32> {
    t.to_dtype(DType::F32).unwrap().flatten_all().unwrap().to_vec1::<f32>().unwrap()
}

#[test]
fn noise_matches_rand_stdrng() {
    use rand::SeedableRng;
    for seed in [0u64, 1, 42, 1234567, u64::MAX] {
        let mut a = rand::rngs::StdRng::seed_from_u64(seed);
        let want = pipeline::randn(&mut a, 4 * 64 * 64 + 3);
        let mut b = ling_sd::rng::StdRng::seed_from_u64(seed);
        let mut got = Vec::new();
        b.randn(4 * 64 * 64 + 3, &mut got);
        let same = got.iter().zip(want.iter()).filter(|(g, w)| g == w).count();
        let close = got.iter().zip(want.iter()).all(|(g, w)| (g - w).abs() < 1e-6);
        println!("seed {seed}: {same}/{} bit-identical", want.len());
        assert!(close, "noise differs for seed {seed}");
    }
}

#[test]
fn matches_the_host_engine() {
    let path = ckpt();
    if !std::path::Path::new(&path).exists() {
        println!("no checkpoint at {path}: skipped");
        return;
    }
    setup();
    ling_sd::ops::set_gelu_tanh(true); // candle's gelu() is the tanh form
    let f = std::fs::File::open(&path).unwrap();
    let src = FileSrc { len: f.metadata().unwrap().len(), f };
    let vocab = std::fs::read(format!("{}/vocab.json", clip_dir())).unwrap();
    let merges = std::fs::read(format!("{}/merges.txt", clip_dir())).unwrap();
    let t0 = Instant::now();
    let mut m = ling_sd::Model::load(&src, &vocab, &merges, &mut |_, _| {}).expect("ling-sd load");
    println!("ling-sd: loaded {} MB of weights in {:.1} s", m.weight_bytes >> 20, t0.elapsed().as_secs_f64());

    let t0 = Instant::now();
    let opts = LoadOpts { checkpoint: path.clone(), vae: String::new(), loras: vec![], device: Device::Cpu, dtype: DType::F32, sliced_attention: false };
    let host = pipeline::Model::load(&opts, &|s| println!("  host: {s}")).expect("host load");
    let tok = ling_diffusion::tokenizer::ClipTokenizer::load(std::path::Path::new(&clip_dir())).unwrap();
    println!("host engine loaded in {:.1} s", t0.elapsed().as_secs_f64());

    // ── tokenizer + text encoder ──
    let prompt = "a (watercolor:1.2) painting of a red fox in the snow, highly detailed, golden hour";
    let negative = "blurry, lowres, [bad anatomy]";
    for text in [prompt, negative, "Hello, world! It's 2026's 3rd test -- émigré café"] {
        let a = tok.encode(text);
        let b = m.tok.encode(text);
        assert_eq!(a, b, "tokenizer differs on {text:?}");
    }
    println!("tokenizer: identical ids");
    for skip in [1usize, 2] {
        let t0 = Instant::now();
        let (hc, hu) = host.encode_pair(&tok, prompt, negative, skip).unwrap();
        let th = t0.elapsed().as_secs_f64();
        let t0 = Instant::now();
        let (c, u) = m.encode(prompt, negative, skip);
        let tl = t0.elapsed().as_secs_f64();
        println!("text encoder, clip skip {skip} (host {th:.2} s, ling-sd {tl:.2} s):");
        assert!(diff("cond", &c.to_vec(), &flat(&hc.ctx)) < 1e-4);
        assert!(diff("uncond", &u.to_vec(), &flat(&hu.ctx)) < 1e-4);
    }

    // ── one guided UNet step (the denoiser the samplers call) ──
    let (lh, lw) = (64usize, 64usize);
    let table = Table::sd();
    let mine_table = ling_sd::sample::Table::sd();
    let (hc, hu) = host.encode_pair(&tok, prompt, negative, 1).unwrap();
    let (c, u) = m.encode(prompt, negative, 1);
    let mut r = ling_sd::rng::StdRng::seed_from_u64(42);
    let mut noise = Vec::new();
    r.randn(4 * lh * lw, &mut noise);
    let sigmas = table.sigmas(20, ling_diffusion::sampling::Schedule::Karras);
    for &si in &[0usize, 10] {
        let sigma = sigmas[si];
        let x: Vec<f32> = noise.iter().map(|v| v * sigma as f32).collect();
        let xt = Tensor::from_vec(x.clone(), (1, 4, lh, lw), &Device::Cpu).unwrap();
        let ctx2 = Tensor::cat(&[&hu.ctx, &hc.ctx], 0).unwrap().to_dtype(host.dtype).unwrap();
        let mut on_step = |_: usize, _: &Tensor| true;
        let mut d = Denoiser { model: &host, table: &table, ctx2, added2: None, cfg: 7.0, batch: 1, rngs: vec![], on_step: &mut on_step };
        let t0 = Instant::now();
        let want = flat(&d.denoise(&xt, sigma).unwrap());
        let th = t0.elapsed().as_secs_f64();
        let t0 = Instant::now();
        let got = m.denoise(&x, lh, lw, sigma, &c, &u, 7.0, &mine_table);
        let tl = t0.elapsed().as_secs_f64();
        println!("guided UNet step at sigma {sigma:.3} (2 UNet passes; host {th:.1} s, ling-sd {tl:.1} s):");
        assert!(diff("denoised", &got, &want) < 2e-3);
        // the noise prediction itself is the sensitive part: compare eps too
        let eg: Vec<f32> = x.iter().zip(got.iter()).map(|(a, b)| (a - b) / sigma as f32).collect();
        let ew: Vec<f32> = x.iter().zip(want.iter()).map(|(a, b)| (a - b) / sigma as f32).collect();
        assert!(diff("eps (guided)", &eg, &ew) < 5e-3);
    }

    // ── VAE decoder ──
    let lat: Vec<f32> = noise.iter().map(|v| v * 0.8).collect();
    let lt = Tensor::from_vec(lat.clone(), (1, 4, lh, lw), &Device::Cpu).unwrap();
    let t0 = Instant::now();
    let want = host.decode(&lt).unwrap();
    let th = t0.elapsed().as_secs_f64();
    let t0 = Instant::now();
    let (got, w, h) = m.vae.decode(&mut m.eng, &Act::from_chw(4, lh, lw, &lat));
    let tl = t0.elapsed().as_secs_f64();
    assert_eq!((w, h), (512, 512));
    let off = got.iter().zip(want.iter()).filter(|(a, b)| a != b).count();
    let maxd = got.iter().zip(want.iter()).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
    println!("VAE decode 512x512 (host {th:.1} s, ling-sd {tl:.1} s): {off} of {} bytes differ, by at most {maxd}", got.len());
    assert!(maxd <= 2);
}

/// A whole image both ways from the same seed (the host's noise is reproduced exactly), saved as PNGs in the scratch folder.
#[test]
fn same_seed_same_picture() {
    let path = ckpt();
    let out_dir = match std::env::var("SD_OUT") {
        Ok(d) => d,
        Err(_) => {
            println!("SD_OUT not set: skipped");
            return;
        },
    };
    if !std::path::Path::new(&path).exists() {
        println!("no checkpoint at {path:?} (set SD_CKPT): skipped");
        return;
    }
    setup();
    ling_sd::ops::set_gelu_tanh(true);
    let f = std::fs::File::open(&path).unwrap();
    let src = FileSrc { len: f.metadata().unwrap().len(), f };
    let vocab = std::fs::read(format!("{}/vocab.json", clip_dir())).unwrap();
    let merges = std::fs::read(format!("{}/merges.txt", clip_dir())).unwrap();
    let mut m = ling_sd::Model::load(&src, &vocab, &merges, &mut |_, _| {}).unwrap();
    let steps: usize = std::env::var("SD_STEPS").ok().and_then(|v| v.parse().ok()).unwrap_or(20);
    let sampler = std::env::var("SD_SAMPLER").unwrap_or_else(|_| "Euler a".into());
    let p = ling_sd::Params {
        prompt: "a cozy cabin in a snowy forest at night, warm light in the windows, digital painting".into(),
        negative: "blurry, lowres".into(),
        sampler: sampler.clone(),
        steps,
        cfg: 7.0,
        width: 512,
        height: 512,
        seed: 1234,
        clip_skip: 1,
    };
    let t0 = Instant::now();
    let mut last = Instant::now();
    let img = m
        .txt2img(&p, &mut |pr| {
            if pr.stage == ling_sd::Stage::Sampling {
                println!("  step {}/{} ({:.1} s)", pr.step, pr.steps, last.elapsed().as_secs_f64());
                last = Instant::now();
            }
            true
        })
        .unwrap();
    let tl = t0.elapsed().as_secs_f64();
    save_png(&format!("{out_dir}/lingsd-{}.png", sampler.replace(' ', "_")), &img.rgb, img.w, img.h);

    // the host engine, the same request
    let opts = LoadOpts { checkpoint: path.clone(), vae: String::new(), loras: vec![], device: Device::Cpu, dtype: DType::F32, sliced_attention: false };
    let host = pipeline::Model::load(&opts, &|_| {}).unwrap();
    let tok = ling_diffusion::tokenizer::ClipTokenizer::load(std::path::Path::new(&clip_dir())).unwrap();
    let t0 = Instant::now();
    let (hc, hu) = host.encode_pair(&tok, &p.prompt, &p.negative, 1).unwrap();
    let table = Table::sd();
    let (smp, sch, _, _) = ling_diffusion::sampling::parse(&sampler, "Automatic");
    let sig = table.sigmas(steps, sch);
    use rand::SeedableRng;
    let mut rngs = vec![rand::rngs::StdRng::seed_from_u64(p.seed)];
    let noise = pipeline::randn(&mut rngs[0], 4 * 64 * 64);
    let x = (Tensor::from_vec(noise, (1, 4, 64, 64), &Device::Cpu).unwrap() * sig[0]).unwrap();
    let ctx2 = Tensor::cat(&[&hu.ctx, &hc.ctx], 0).unwrap();
    let mut on_step = |_: usize, _: &Tensor| true;
    let mut d = Denoiser { model: &host, table: &table, ctx2, added2: None, cfg: 7.0, batch: 1, rngs, on_step: &mut on_step };
    let lat = pipeline::run_sampler(smp, &sig, x, &mut d).unwrap();
    let want = host.decode(&lat).unwrap();
    let th = t0.elapsed().as_secs_f64();
    save_png(&format!("{out_dir}/host-{}.png", sampler.replace(' ', "_")), &want, 512, 512);
    let mean = img.rgb.iter().zip(want.iter()).map(|(a, b)| (*a as f64 - *b as f64).abs()).sum::<f64>() / want.len() as f64;
    let maxd = img.rgb.iter().zip(want.iter()).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
    let mse = img.rgb.iter().zip(want.iter()).map(|(a, b)| (*a as f64 - *b as f64).powi(2)).sum::<f64>() / want.len() as f64;
    let psnr = 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
    println!("{sampler}, {steps} steps, 512x512: ling-sd {tl:.1} s, host {th:.1} s; mean |pixel diff| {mean:.3}, max {maxd}, PSNR {psnr:.1} dB");
    // the same picture: an ancestral sampler (Euler a) re-injects noise every step and amplifies 1e-5 float differences into
    // small detail changes (same composition, ~10 levels on average); the deterministic ones stay closer
    let ancestral = sampler.ends_with(" a");
    assert!(mean < if ancestral { 16.0 } else { 6.0 });
}

/// Speed of one guided step (two UNet passes) and one VAE decode at 512x512 with SD_THREADS threads (SD_SSE=1: SSE kernels).
#[test]
fn bench_step() {
    if std::env::var("SD_BENCH").is_err() {
        println!("SD_BENCH not set: skipped");
        return;
    }
    setup();
    let f = std::fs::File::open(ckpt()).unwrap();
    let src = FileSrc { len: f.metadata().unwrap().len(), f };
    let vocab = std::fs::read(format!("{}/vocab.json", clip_dir())).unwrap();
    let merges = std::fs::read(format!("{}/merges.txt", clip_dir())).unwrap();
    let mut m = ling_sd::Model::load(&src, &vocab, &merges, &mut |_, _| {}).unwrap();
    let (c, u) = m.encode("a red fox in the snow", "blurry", 1);
    let table = ling_sd::sample::Table::sd();
    let mut r = ling_sd::rng::StdRng::seed_from_u64(1);
    let mut x = Vec::new();
    r.randn(4 * 64 * 64, &mut x);
    let _ = m.denoise(&x, 64, 64, 5.0, &c, &u, 7.0, &table); // warm-up
    ling_sd::kern::prof_reset();
    let t0 = Instant::now();
    let n = 2;
    for _ in 0..n {
        let _ = m.denoise(&x, 64, 64, 5.0, &c, &u, 7.0, &table);
    }
    let step = t0.elapsed().as_secs_f64() / n as f64;
    if std::env::var("SD_PROFILE").is_ok() {
        print!("PROFILE per guided step:");
        for (name, s) in ling_sd::kern::profile() {
            if s > 0.0 {
                print!(" {name} {:.3} s,", s / n as f64);
            }
        }
        println!(" GEMM {:.0} GFLOP at {:.0} GFLOPS", ling_sd::kern::gemm_flops() as f64 / n as f64 * 1e-9, ling_sd::kern::gemm_flops() as f64 / n as f64 * 1e-9 / step);
        print_trace(1.0 / n as f64);
        ling_sd::kern::prof_reset();
    }
    let t0 = Instant::now();
    let _ = m.vae.decode(&mut m.eng, &Act::from_chw(4, 64, 64, &x));
    let vae = t0.elapsed().as_secs_f64();
    if std::env::var("SD_PROFILE").is_ok() {
        print!("PROFILE VAE decode:");
        for (name, s) in ling_sd::kern::profile() {
            if s > 0.0 {
                print!(" {name} {s:.3} s,");
            }
        }
        println!(" GEMM {:.0} GFLOP at {:.0} GFLOPS", ling_sd::kern::gemm_flops() as f64 * 1e-9, ling_sd::kern::gemm_flops() as f64 * 1e-9 / vae);
    }
    println!(
        "BENCH {} threads {}: {step:.2} s per guided step (2 UNet passes), VAE decode {vae:.2} s, 20-step image ~{:.0} s",
        threads(),
        ling_sd::kern::simd_name(),
        20.0 * step + vae
    );
}

fn save_png(path: &str, rgb: &[u8], w: usize, h: usize) {
    let f = std::fs::File::create(path).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
    e.set_color(png::ColorType::Rgb);
    e.set_depth(png::BitDepth::Eight);
    e.write_header().unwrap().write_image_data(rgb).unwrap();
    println!("  wrote {path}");
}
