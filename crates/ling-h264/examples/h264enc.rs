//! Encoder test bench: raw BGRA frames in, H.264 (Annex-B and/or MP4) out,
//! with the reconstruction checked bit-exact against this crate's decoder and
//! PSNR against the (converted) source.
//!
//! `cargo run --release --example h264enc -- --w 1920 --h 1080 --in frames.bgra|- [--srcw W --srch H]
//!   [--fps 30] [--qp 24] [--maxqp 38] [--kbps 10000] [--slices 16] [--threads 16] [--frames N] [--dup K]
//!   [--mp4 out.mp4] [--frag out-frag.mp4] [--h264 out.264] [--noverify]`
//!
//! With no input (`--in synth`) it generates a synthetic desktop: text, a
//! scrolling document, a dragged window and a moving cursor.
use ling_h264::enc::{EncConfig, Encoder, Source};
use ling_mp4::writer;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Barrier;
use std::time::Instant;

struct P(*const Encoder, *const Source<'static>);
unsafe impl Send for P {}
unsafe impl Sync for P {}

fn arg<T: std::str::FromStr>(a: &[String], k: &str, d: T) -> T {
    a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse: f64 = a.iter().zip(b).map(|(&x, &y)| ((x as f64) - (y as f64)).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// A synthetic desktop frame `i`.
fn synth(px: &mut [u32], w: usize, h: usize, i: usize) {
    // wallpaper gradient
    for y in 0..h {
        for x in 0..w {
            px[y * w + x] = (((x * 255 / w) as u32) << 16) | (((y * 255 / h) as u32) << 8) | 0x60;
        }
    }
    let glyph = |px: &mut [u32], x0: usize, y0: usize, c: u32, col: u32| {
        // 6x10 pseudo glyph from the character code bits
        for gy in 0..10 {
            for gx in 0..6 {
                let bit = (c.wrapping_mul(2654435761).rotate_left((gy * 6 + gx) as u32)) & 1;
                if bit == 1 && gy % 9 != 0 && gx != 5 {
                    let (x, y) = (x0 + gx, y0 + gy);
                    if x < w && y < h {
                        px[y * w + x] = col;
                    }
                }
            }
        }
    };
    // a document window scrolling by 3 lines every 10 frames
    let (wx, wy, ww, wh) = (100, 80, 900, 800);
    for y in wy..(wy + wh).min(h) {
        for x in wx..(wx + ww).min(w) {
            px[y * w + x] = 0xF8F8F8;
        }
    }
    let scroll = (i / 10) * 3 * 14;
    for line in 0..(wh / 14) {
        let doc_line = line + scroll / 14;
        for ch in 0..((doc_line * 7) % 60 + 40) {
            glyph(px, wx + 10 + ch * 7, wy + 6 + line * 14, (doc_line * 131 + ch) as u32, 0x202020);
        }
    }
    // a dragged window
    let (dx, dy) = (1000 + (i * 7) % 600, 200 + (i * 3) % 300);
    for y in dy..(dy + 300).min(h) {
        for x in dx..(dx + 500).min(w) {
            px[y * w + x] = if y < dy + 30 { 0x3050A0 } else { 0xE0E0E8 };
        }
    }
    for line in 0..18 {
        for ch in 0..60 {
            glyph(px, dx + 10 + ch * 7, dy + 40 + line * 14, (line * 61 + ch) as u32, 0x101040);
        }
    }
    // cursor
    let (cx, cy) = ((i * 23) % w, (i * 11) % h);
    for y in 0..20 {
        for x in 0..=y / 2 {
            if cx + x < w && cy + y < h {
                px[(cy + y) * w + cx + x] = 0xFFFFFF;
            }
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let w: usize = arg(&a, "--w", 1920);
    let h: usize = arg(&a, "--h", 1080);
    let sw: usize = arg(&a, "--srcw", w);
    let sh: usize = arg(&a, "--srch", h);
    let input: String = arg(&a, "--in", String::from("synth"));
    let fps: u32 = arg(&a, "--fps", 30);
    let frames: usize = arg(&a, "--frames", 90);
    let dup: usize = arg(&a, "--dup", 1);
    let threads: usize = arg(&a, "--threads", 1);
    let verify = !a.iter().any(|x| x == "--noverify");
    let cfg = EncConfig {
        width: w,
        height: h,
        fps,
        qp: arg(&a, "--qp", 24),
        max_qp: arg(&a, "--maxqp", 38),
        bitrate_kbps: arg(&a, "--kbps", 10_000),
        gop: arg(&a, "--gop", 0),
        slices: arg(&a, "--slices", threads.max(1)),
    };
    let mut enc = Encoder::new(cfg.clone());
    let mut dec = ling_h264::Decoder::from_avcc(&enc.avcc()).expect("avcC");
    let mut reader: Box<dyn Read> = match input.as_str() {
        "synth" => Box::new(std::io::empty()),
        "-" => Box::new(std::io::stdin()),
        p => Box::new(std::io::BufReader::with_capacity(1 << 24, std::fs::File::open(p).expect("open input"))),
    };
    let mut raw = vec![0u8; sw * sh * 4];
    let mut px = vec![0u32; sw * sh];
    let mut annexb = Vec::new();
    let mut samples: Vec<writer::SampleInfo> = Vec::new();
    let mut data: Vec<u8> = Vec::new();
    let tick = 90000 / fps;
    let (mut t_enc, mut n) = (0f64, 0usize);
    let (mut ps_y, mut ps_u, mut ps_v, mut worst) = (0f64, 0f64, 0f64, 99f64);
    let mut maxf = 0usize;
    let mut stats = ling_h264::enc::SliceStats::default();
    let counter = AtomicUsize::new(0);
    let start = Barrier::new(threads.max(1));
    let done = Barrier::new(threads.max(1));
    let shared = std::sync::Mutex::new(P(std::ptr::null(), std::ptr::null()));
    let quit = std::sync::atomic::AtomicBool::new(false);
    let mut fi = 0usize;
    std::thread::scope(|s| {
        for _ in 1..threads.max(1) {
            s.spawn(|| loop {
                start.wait();
                if quit.load(Ordering::Acquire) {
                    break;
                }
                let (e, src) = {
                    let g = shared.lock().unwrap();
                    (g.0, g.1)
                };
                let e = unsafe { &*e };
                loop {
                    let i = counter.fetch_add(1, Ordering::AcqRel);
                    if i >= e.slice_count() {
                        break;
                    }
                    unsafe { e.encode_slice(i, &*src) };
                }
                done.wait();
            });
        }
        'frames: while n < frames {
            if input == "synth" {
                synth(&mut px, sw, sh, fi);
            } else {
                if reader.read_exact(&mut raw).is_err() {
                    break 'frames;
                }
                for (i, c) in raw.chunks_exact(4).enumerate() {
                    px[i] = u32::from_le_bytes([c[0], c[1], c[2], 0]);
                }
            }
            fi += 1;
            for _ in 0..dup {
                if n >= frames {
                    break;
                }
                let src = Source { px: &px, stride: sw, width: sw, height: sh };
                let t0 = Instant::now();
                enc.begin_frame(1_000_000 / fps as u64);
                {
                    let mut g = shared.lock().unwrap();
                    g.0 = &enc as *const Encoder;
                    g.1 = &src as *const Source as *const Source<'static>;
                }
                counter.store(0, Ordering::Release);
                if threads > 1 {
                    start.wait();
                }
                loop {
                    let i = counter.fetch_add(1, Ordering::AcqRel);
                    if i >= enc.slice_count() {
                        break;
                    }
                    unsafe { enc.encode_slice(i, &src) };
                }
                if threads > 1 {
                    done.wait();
                }
                let pkt = enc.end_frame(&src);
                t_enc += t0.elapsed().as_secs_f64();
                let (key, qp, st) = (pkt.key, pkt.qp, pkt.stats);
                let sample = pkt.data.to_vec();
                stats.add(&st);
                maxf = maxf.max(sample.len());
                if key {
                    for ps in [enc.sps(), enc.pps()] {
                        annexb.extend_from_slice(&[0, 0, 0, 1]);
                        annexb.extend_from_slice(ps);
                    }
                }
                ling_h264::enc::to_annexb(&sample, &mut annexb);
                samples.push(writer::SampleInfo { size: sample.len() as u32, duration: tick, key });
                data.extend_from_slice(&sample);
                if verify {
                    dec.decode_sample(&sample).unwrap_or_else(|e| panic!("frame {n}: decode error {e:?}"));
                    let f = dec.next_frame().or_else(|| {
                        dec.flush();
                        dec.next_frame()
                    });
                    let f = f.unwrap_or_else(|| panic!("frame {n}: no picture"));
                    let (ry, ru, rv, ys, cs) = enc.recon();
                    let (sy, su, sv, _, _) = enc.source();
                    let (mut cy, mut cu, mut cv, mut oy, mut ou, mut ov) = (vec![], vec![], vec![], vec![], vec![], vec![]);
                    for y in 0..h {
                        cy.extend_from_slice(&ry[y * ys..y * ys + w]);
                        oy.extend_from_slice(&sy[y * ys..y * ys + w]);
                    }
                    for y in 0..h / 2 {
                        cu.extend_from_slice(&ru[y * cs..y * cs + w / 2]);
                        cv.extend_from_slice(&rv[y * cs..y * cs + w / 2]);
                        ou.extend_from_slice(&su[y * cs..y * cs + w / 2]);
                        ov.extend_from_slice(&sv[y * cs..y * cs + w / 2]);
                    }
                    let mut dy = vec![];
                    let mut du = vec![];
                    let mut dv = vec![];
                    for y in 0..h {
                        dy.extend_from_slice(&f.y[y * f.y_stride..y * f.y_stride + w]);
                    }
                    for y in 0..h / 2 {
                        du.extend_from_slice(&f.u[y * f.c_stride..y * f.c_stride + w / 2]);
                        dv.extend_from_slice(&f.v[y * f.c_stride..y * f.c_stride + w / 2]);
                    }
                    if dy != cy || du != cu || dv != cv {
                        let pos = dy.iter().zip(&cy).position(|(a, b)| a != b);
                        panic!("frame {n} (key {key}): decoder output differs from the encoder's reconstruction (first luma diff at {:?})", pos.map(|p| (p % w, p / w)));
                    }
                    let p = psnr(&oy, &cy);
                    ps_y += p;
                    ps_u += psnr(&ou, &cu);
                    ps_v += psnr(&ov, &cv);
                    worst = worst.min(p);
                }
                if n % 30 == 0 || key {
                    eprintln!("frame {n}: {} bytes key={key} qp={qp} {:?}", sample.len(), st);
                }
                n += 1;
            }
        }
        quit.store(true, Ordering::Release);
        if threads > 1 {
            start.wait();
        }
    });
    let secs = n as f64 / fps as f64;
    println!(
        "{n} frames {w}x{h} (src {sw}x{sh}), {} slices, {threads} threads: encode {:.2} ms/frame = {:.1} fps",
        enc.slice_count(),
        t_enc * 1000.0 / n.max(1) as f64,
        n as f64 / t_enc
    );
    println!(
        "size {} bytes = {:.0} kbit/s at {fps} fps, max frame {} bytes; MBs {:?}",
        data.len(),
        data.len() as f64 * 8.0 / secs / 1000.0,
        maxf,
        stats
    );
    if verify {
        println!(
            "decoder == encoder reconstruction on all {n} frames; PSNR Y {:.2} U {:.2} V {:.2} dB (worst Y {:.2})",
            ps_y / n as f64,
            ps_u / n as f64,
            ps_v / n as f64,
            worst
        );
    }
    let track = writer::VideoTrack { width: w as u32, height: h as u32, timescale: 90000, avcc: enc.avcc(), bt709: true };
    if let Some(p) = a.iter().position(|x| x == "--h264").and_then(|i| a.get(i + 1)) {
        std::fs::write(p, &annexb).unwrap();
    }
    if let Some(p) = a.iter().position(|x| x == "--mp4").and_then(|i| a.get(i + 1)) {
        // one chunk per keyframe interval
        let mut chunks = vec![];
        let mut off = 0u64;
        for s in &samples {
            if s.key || chunks.is_empty() {
                chunks.push((off, 0u32));
            }
            chunks.last_mut().unwrap().1 += 1;
            off += s.size as u64;
        }
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(&writer::layout(&track, &samples, &chunks)).unwrap();
        f.write_all(&data).unwrap();
    }
    if let Some(p) = a.iter().position(|x| x == "--frag").and_then(|i| a.get(i + 1)) {
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(&writer::ftyp()).unwrap();
        f.write_all(&writer::moov_fragmented(&track)).unwrap();
        let (mut i, mut off, mut seq, mut t) = (0usize, 0usize, 1u32, 0u64);
        while i < samples.len() {
            let mut j = i + 1;
            while j < samples.len() && !samples[j].key {
                j += 1;
            }
            let len: usize = samples[i..j].iter().map(|s| s.size as usize).sum();
            f.write_all(&writer::fragment_header(seq, t, &samples[i..j])).unwrap();
            f.write_all(&data[off..off + len]).unwrap();
            t += samples[i..j].iter().map(|s| s.duration as u64).sum::<u64>();
            off += len;
            seq += 1;
            i = j;
        }
    }
}
