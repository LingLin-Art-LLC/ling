//! Decode an Annex-B .264 file: `cargo run --release --example h264dec -- in.264 [out.yuv]`.
//! Prints decode speed; writes raw yuv420p frames in display order when an output path is given.
//! `h264dec in.264 out.yuv|- repeat drain` feeds the stream one NAL unit at a time and drops each
//! frame as soon as it is output (like a player) instead of keeping every frame until the end.
use std::io::Write;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: h264dec in.264 [out.yuv] [repeat] [drain]");
        std::process::exit(2);
    }
    let data = std::fs::read(&args[1]).expect("read input");
    let mut out = args.get(2).filter(|s| *s != "-").map(|p| std::io::BufWriter::new(std::fs::File::create(p).expect("create output")));
    let repeat: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1);
    let drain = args.get(4).is_some_and(|s| s == "drain");
    // NAL units (each with its start code) for drain mode
    let mut chunks = Vec::new();
    if drain {
        let mut last = 0;
        let mut k = 0;
        while k + 3 <= data.len() {
            if data[k] == 0 && data[k + 1] == 0 && data[k + 2] == 1 {
                if k > last {
                    chunks.push(&data[last..k]);
                }
                last = k;
                k += 3;
            } else {
                k += 1;
            }
        }
        chunks.push(&data[last..]);
    }
    let mut best = f64::MAX;
    let mut frames = 0;
    let (mut w, mut h) = (0, 0);
    for r in 0..repeat {
        let t = Instant::now();
        let mut d = ling_h264::Decoder::new();
        frames = 0;
        let mut take = |d: &mut ling_h264::Decoder, frames: &mut usize| {
            while let Some(f) = d.next_frame() {
                *frames += 1;
                w = f.width;
                h = f.height;
                if r == 0 {
                    if let Some(o) = out.as_mut() {
                        o.write_all(&f.y).unwrap();
                        o.write_all(&f.u).unwrap();
                        o.write_all(&f.v).unwrap();
                    }
                }
            }
        };
        if drain {
            for c in &chunks {
                let _ = d.decode_annexb(c);
                take(&mut d, &mut frames);
            }
        } else if let Err(e) = d.decode_annexb(&data) {
            eprintln!("decode error: {:?}", e);
        }
        d.flush();
        take(&mut d, &mut frames);
        best = best.min(t.elapsed().as_secs_f64());
    }
    println!("{}x{}: {} frames in {:.3} s = {:.1} fps", w, h, frames, best, frames as f64 / best);
}
