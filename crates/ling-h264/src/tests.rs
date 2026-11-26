//! Host tests: bit-exact comparison against ffmpeg (framemd5) for streams made by libx264,
//! plus robustness against truncated / corrupted input. Vectors: tools/gen_vectors.sh.

use crate::*;
use std::format;
use std::string::String;
use std::vec::Vec;
use std::{eprintln, println};

fn data_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("data")
}

// ---------------------------------------------------------------- md5 (RFC 1321)

pub fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4,
        11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64).map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32).collect();
    let mut h: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    let mut msg = data.to_vec();
    let bitlen = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_le_bytes());
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = (0..16).map(|i| u32::from_le_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]])).collect();
        let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f2 = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f2.rotate_left(S[i]));
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
    }
    let mut out = [0u8; 16];
    for i in 0..4 {
        out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_le_bytes());
    }
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn frame_bytes(f: &Frame) -> Vec<u8> {
    let mut v = Vec::with_capacity(f.y.len() + f.u.len() * 2);
    v.extend_from_slice(&f.y);
    v.extend_from_slice(&f.u);
    v.extend_from_slice(&f.v);
    v
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse: f64 = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum::<f64>() / a.len().max(1) as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

fn read_md5(name: &str) -> Vec<String> {
    let s = std::fs::read_to_string(data_dir().join(format!("{}.md5", name))).expect("md5 file");
    s.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).map(|l| String::from(l.rsplit(',').next().unwrap().trim())).collect()
}

/// Compare decoded frames to the reference; returns a description of the first mismatch.
fn compare(name: &str, frames: &[Frame]) -> Result<(), String> {
    let refs = read_md5(name);
    let yuv = std::fs::read(data_dir().join("yuv").join(format!("{}.yuv", name))).ok();
    let mut err = None;
    for (i, f) in frames.iter().enumerate() {
        if i >= refs.len() {
            break;
        }
        let got = hex(&md5(&frame_bytes(f)));
        if got != refs[i] && err.is_none() {
            let mut msg = format!("{}: frame {} (poc {}) differs from ffmpeg", name, i, f.poc);
            if let Some(y) = &yuv {
                let fsz = frame_bytes(f).len();
                if y.len() >= (i + 1) * fsz {
                    let r = &y[i * fsz..(i + 1) * fsz];
                    let b = frame_bytes(f);
                    let ysz = f.y.len();
                    msg += &format!(
                        "; PSNR Y {:.2} dB, U {:.2} dB, V {:.2} dB",
                        psnr(&b[..ysz], &r[..ysz]),
                        psnr(&b[ysz..ysz + f.u.len()], &r[ysz..ysz + f.u.len()]),
                        psnr(&b[ysz + f.u.len()..], &r[ysz + f.u.len()..])
                    );
                    // first differing macroblock
                    if let Some(p) = (0..ysz).find(|&k| b[k] != r[k]) {
                        let (x, yy) = (p % f.width as usize, p / f.width as usize);
                        msg += &format!("; first luma diff at ({}, {}) mb ({}, {})", x, yy, x / 16, yy / 16);
                    }
                }
            }
            err = Some(msg);
        }
    }
    if frames.len() != refs.len() && err.is_none() {
        err = Some(format!("{}: decoded {} frames, ffmpeg {}", name, frames.len(), refs.len()));
    }
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn decode_file(name: &str) -> (Vec<Frame>, Vec<Error>) {
    let data = std::fs::read(data_dir().join(format!("{}.264", name))).expect("vector");
    let mut d = Decoder::new();
    let mut errs = Vec::new();
    if let Err(e) = d.decode_annexb(&data) {
        errs.push(e);
    }
    d.flush();
    let mut frames = Vec::new();
    while let Some(f) = d.next_frame() {
        frames.push(f);
    }
    (frames, errs)
}

fn check(name: &str) {
    let (frames, errs) = decode_file(name);
    if !errs.is_empty() {
        eprintln!("{}: decoder errors {:?}", name, errs);
    }
    match compare(name, &frames) {
        Ok(()) => println!("{}: {} frames bit-exact", name, frames.len()),
        Err(e) => panic!("{}", e),
    }
    assert!(errs.is_empty(), "{}: errors {:?}", name, errs);
}

macro_rules! vectors {
    ($($n:ident),* $(,)?) => { $( #[test] fn $n() { check(stringify!($n)); } )* };
}

vectors!(
    baseline_cavlc,
    baseline_slices,
    main_cabac_b,
    main_cavlc_b,
    high_8x8,
    high_cqm_jvt,
    high_cqm_custom,
    high_cavlc_8x8,
    weightp,
    weightb_implicit,
    direct_temporal,
    direct_spatial,
    direct_temporal_cavlc,
    partitions_all,
    slices_cabac,
    odd_426x240,
    crop_640x360,
    nodeblock,
    lowqp,
    highqp,
    mixed_ref16,
    constrained_intra,
    youtube_like,
    yt_bbb_480p,
);

// ---------------------------------------------------------------- MP4 (avcC + samples)

struct Mp4 {
    avcc: Vec<u8>,
    samples: Vec<Vec<u8>>,
}

fn be32(b: &[u8], p: usize) -> u32 {
    u32::from_be_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]])
}

fn find_box<'a>(b: &'a [u8], path: &[&str]) -> Option<&'a [u8]> {
    let mut p = 0;
    while p + 8 <= b.len() {
        let size = be32(b, p) as usize;
        let ty = std::str::from_utf8(&b[p + 4..p + 8]).unwrap_or("");
        if size < 8 || p + size > b.len() {
            return None;
        }
        if ty == path[0] {
            let body = &b[p + 8..p + size];
            if path.len() == 1 {
                return Some(body);
            }
            let skip = match ty {
                "stsd" => 8,
                "avc1" => 78,
                _ => 0,
            };
            return find_box(&body[skip..], &path[1..]);
        }
        p += size;
    }
    None
}

fn parse_mp4(file: &[u8]) -> Mp4 {
    let stbl = find_box(file, &["moov", "trak", "mdia", "minf", "stbl"]).expect("stbl");
    let avcc = find_box(stbl, &["stsd", "avc1", "avcC"]).expect("avcC").to_vec();
    let stsz = find_box(stbl, &["stsz"]).unwrap();
    let count = be32(stsz, 8) as usize;
    let sizes: Vec<usize> = (0..count).map(|i| be32(stsz, 12 + i * 4) as usize).collect();
    let stco = find_box(stbl, &["stco"]).unwrap();
    let nchunks = be32(stco, 4) as usize;
    let offs: Vec<usize> = (0..nchunks).map(|i| be32(stco, 8 + i * 4) as usize).collect();
    let stsc = find_box(stbl, &["stsc"]).unwrap();
    let n = be32(stsc, 4) as usize;
    let entries: Vec<(usize, usize)> = (0..n).map(|i| (be32(stsc, 8 + i * 12) as usize, be32(stsc, 12 + i * 12) as usize)).collect();
    let mut samples = Vec::new();
    let mut s = 0;
    for c in 0..nchunks {
        let per = entries.iter().rev().find(|e| e.0 <= c + 1).map(|e| e.1).unwrap_or(1);
        let mut o = offs[c];
        for _ in 0..per {
            if s >= count {
                break;
            }
            samples.push(file[o..o + sizes[s]].to_vec());
            o += sizes[s];
            s += 1;
        }
    }
    Mp4 { avcc, samples }
}

#[test]
fn mp4_high() {
    let file = std::fs::read(data_dir().join("mp4_high.mp4")).unwrap();
    let mp4 = parse_mp4(&file);
    let mut d = Decoder::from_avcc(&mp4.avcc).expect("avcC");
    let mut frames = Vec::new();
    for s in &mp4.samples {
        d.decode_sample(s).expect("sample");
        while let Some(f) = d.next_frame() {
            frames.push(f);
        }
    }
    d.flush();
    while let Some(f) = d.next_frame() {
        frames.push(f);
    }
    compare("mp4_high", &frames).unwrap();
    // RGB conversion smoke test
    let f = &frames[0];
    let mut px = std::vec![0u32; (f.width * f.height) as usize];
    frame_to_rgba(f, &mut px, f.width as usize, f.height as usize);
    let mut small = std::vec![0u32; 100 * 50];
    frame_to_rgba(f, &mut small, 100, 50);
    assert!(px.iter().all(|p| p >> 24 == 0xFF));
}

// ---------------------------------------------------------------- robustness

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn drain(d: &mut Decoder) -> usize {
    let mut n = 0;
    while d.next_frame().is_some() {
        n += 1;
    }
    n
}

#[test]
fn garbage_and_truncation_do_not_panic() {
    let names = ["baseline_cavlc", "main_cabac_b", "high_8x8", "high_cavlc_8x8", "weightb_implicit", "direct_temporal", "partitions_all"];
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for name in names {
        let data = std::fs::read(data_dir().join(format!("{}.264", name))).unwrap();
        // truncations
        for k in 0..12 {
            let cut = (data.len() as u64 * (k + 1) / 13) as usize;
            let mut d = Decoder::new();
            let _ = d.decode_annexb(&data[..cut]);
            d.flush();
            drain(&mut d);
        }
        // random bit flips / byte overwrites
        for _ in 0..40 {
            let mut v = data.clone();
            let n = 1 + (rng.next() % 20) as usize;
            for _ in 0..n {
                let p = (rng.next() as usize) % v.len();
                if rng.next() & 1 == 0 {
                    v[p] ^= 1 << (rng.next() % 8);
                } else {
                    v[p] = rng.next() as u8;
                }
            }
            let mut d = Decoder::new();
            let _ = d.decode_annexb(&v);
            d.flush();
            drain(&mut d);
        }
        // the decoder recovers at the next IDR after garbage
        let mut d = Decoder::new();
        let _ = d.decode_annexb(&data);
        let mut junk = std::vec![0u8, 0, 1, 0x65];
        for _ in 0..3000 {
            junk.push(rng.next() as u8);
        }
        let _ = d.decode_annexb(&junk);
        let _ = d.decode_annexb(&data);
        d.flush();
        let n = drain(&mut d);
        assert!(n >= 2, "{}: no frames after recovery", name);
    }
    // pure noise
    for len in [0usize, 1, 3, 5, 100, 10000] {
        let v: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let mut d = Decoder::new();
        let _ = d.decode_annexb(&v);
        let _ = d.decode_sample(&v);
        d.flush();
        drain(&mut d);
        let _ = Decoder::from_avcc(&v);
    }
    // noise NAL units of each type after valid parameter sets
    let data = std::fs::read(data_dir().join("high_8x8.264")).unwrap();
    for ty in [1u8, 5, 7, 8, 0x25, 0x65, 0x41, 0x21] {
        for _ in 0..30 {
            let mut d = Decoder::new();
            let _ = d.decode_annexb(&data[..data.len().min(2000)]);
            let mut v = std::vec![0u8, 0, 1, ty];
            let n = (rng.next() % 400) as usize;
            for _ in 0..n {
                v.push(rng.next() as u8);
            }
            let _ = d.decode_annexb(&v);
            d.flush();
            drain(&mut d);
        }
    }
}
