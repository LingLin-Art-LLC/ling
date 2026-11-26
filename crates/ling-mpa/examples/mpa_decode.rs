//! Host test tool: decode an .mp1/.mp2/.mp3 file.
//!
//! `cargo run --release -p ling-mpa --example mpa_decode -- IN [OUT.f32 [OUT.s16 [CHUNK]]]`
//!
//! Writes the streaming `Decoder` output (fed in CHUNK-byte pieces, default 4096) as raw f32le and
//! the `decode_file` output as raw s16le, and prints the decode speed.

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).expect("read input");
    let chunk: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(4096);

    let t = Instant::now();
    let mut dec = ling_mpa::Decoder::new();
    let mut pcm = Vec::new();
    let mut frames = 0;
    for c in data.chunks(chunk.max(1)) {
        match dec.decode(c, &mut pcm) {
            Ok(n) => frames += n,
            Err(e) => eprintln!("decode error: {e:?}"),
        }
    }
    let dt = t.elapsed().as_secs_f64();
    let secs = frames as f64 / dec.sample_rate().max(1) as f64;
    println!(
        "stream: {} Hz, {} ch, {} frames ({:.2} s), {} bad frames, lame {:?}, {:.1} ms = {:.0}x realtime",
        dec.sample_rate(),
        dec.channels(),
        frames,
        secs,
        dec.error_count(),
        dec.lame_delay_padding(),
        dt * 1e3,
        secs / dt
    );
    if let Some(p) = args.get(2) {
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        std::fs::write(p, bytes).expect("write f32");
    }

    let t = Instant::now();
    let (rate, ch, s16) = ling_mpa::decode_file(&data).expect("decode_file");
    println!(
        "decode_file: {} Hz, {} ch, {} frames, {:.1} ms",
        rate,
        ch,
        s16.len() / ch as usize,
        t.elapsed().as_secs_f64() * 1e3
    );
    if let Some(p) = args.get(3) {
        let bytes: Vec<u8> = s16.iter().flat_map(|s| s.to_le_bytes()).collect();
        std::fs::write(p, bytes).expect("write s16");
    }
}
