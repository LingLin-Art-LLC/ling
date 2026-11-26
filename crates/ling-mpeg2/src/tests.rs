//! Host tests. The fixtures in `tests/data` are tiny clips cut from a real DVD stream and
//! encoded with FFmpeg; the expected hashes are of FFmpeg's own decode (`-idct simple`),
//! which this decoder matches bit for bit.

use super::*;
use alloc::vec::Vec;

const ILACE: &[u8] = include_bytes!("../tests/data/ilace.m2v");
const MPEG1: &[u8] = include_bytes!("../tests/data/mpeg1.m1v");
const PROG_ODD: &[u8] = include_bytes!("../tests/data/prog_odd.m2v");

/// Split an elementary stream into picture units (sequence/GOP headers go with the
/// following picture), like a Matroska demuxer would deliver them.
fn split(es: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut start = 0;
    let mut have_pic = false;
    let mut i = 0;
    while i + 3 < es.len() {
        if es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1 {
            let c = es[i + 3];
            if matches!(c, 0x00 | 0xB3 | 0xB8) && have_pic {
                units.push(&es[start..i]);
                start = i;
                have_pic = false;
            }
            if c == 0x00 {
                have_pic = true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    units.push(&es[start..]);
    units
}

fn fnv(h: &mut u64, d: &[u8]) {
    for &b in d {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
}

fn decode_all(es: &[u8]) -> Vec<Frame> {
    let mut d = Decoder::new();
    let mut out = Vec::new();
    for (k, u) in split(es).iter().enumerate() {
        d.decode(u, k as i64).unwrap();
        while let Some(f) = d.next_frame() {
            out.push(f);
        }
    }
    d.flush();
    while let Some(f) = d.next_frame() {
        out.push(f);
    }
    out
}

fn hash_frames(fr: &[Frame]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for f in fr {
        fnv(&mut h, &f.y);
        fnv(&mut h, &f.u);
        fnv(&mut h, &f.v);
    }
    h
}

/// FNV-1a of FFmpeg's yuv420p output for each fixture.
const EXPECT: [(&str, u64); 3] = [
    ("ilace", 0x1845_5579_041d_9adb),
    ("mpeg1", 0xfac6_9116_987a_f44f),
    ("prog_odd", 0x515d_8c96_8756_e96c),
];

fn fixtures() -> [(&'static str, &'static [u8]); 3] {
    [("ilace", ILACE), ("mpeg1", MPEG1), ("prog_odd", PROG_ODD)]
}

#[test]
fn fixtures_match_ffmpeg() {
    for ((name, es), (en, eh)) in fixtures().iter().zip(EXPECT.iter()) {
        assert_eq!(name, en);
        let fr = decode_all(es);
        assert_eq!(fr.len(), 10, "{name}");
        let h = hash_frames(&fr);
        std::println!("{name}: {h:#018x}");
        assert_eq!(h, *eh, "{name} output differs from FFmpeg");
    }
}

#[test]
fn fixture_properties() {
    let fr = decode_all(ILACE);
    assert_eq!((fr[0].width, fr[0].height), (96, 64));
    assert!(!fr[0].progressive && fr[0].top_field_first && fr[0].key);
    assert_eq!(fr[0].dar, (3, 2)); // square pixels
    // Each coded picture keeps its own pts through reordering.
    let mut p: Vec<i64> = fr.iter().map(|f| f.pts).collect();
    assert_ne!(p, (0..10).collect::<Vec<i64>>()); // B-frames: display != decode order
    p.sort();
    assert_eq!(p, (0..10).collect::<Vec<i64>>());
    let fr = decode_all(PROG_ODD);
    assert_eq!((fr[0].width, fr[0].height), (90, 60));
    assert_eq!((fr[0].y.len(), fr[0].u.len()), (90 * 60, 45 * 30));
    assert!(fr[0].progressive);
    let fr = decode_all(MPEG1);
    assert!(fr[0].progressive && fr[0].key);
}

#[test]
fn whole_stream_in_one_call_equals_per_picture() {
    for (name, es) in fixtures() {
        let mut d = Decoder::new();
        d.decode(es, 7).unwrap();
        d.flush();
        let mut fr = Vec::new();
        while let Some(f) = d.next_frame() {
            fr.push(f);
        }
        assert_eq!(hash_frames(&fr), hash_frames(&decode_all(es)), "{name}");
    }
}

#[test]
fn with_header_and_reset() {
    // Sequence header + extension only (what Matroska CodecPrivate holds).
    let end = (4..ILACE.len()).find(|&i| ILACE[i..].starts_with(&[0, 0, 1, 0xB8])).unwrap();
    let d = Decoder::with_header(&ILACE[..end]).unwrap();
    assert_eq!((d.width(), d.height()), (96, 64));
    assert_eq!(d.frame_rate(), (25, 1));
    assert!(Decoder::with_header(&[0, 0, 1, 0xB3, 0]).is_err());
    assert!(Decoder::with_header(&[]).is_ok());

    // Seek simulation: decode some, reset, restart mid-GOP.
    let units = split(ILACE);
    let mut d = Decoder::new();
    for u in &units[..4] {
        d.decode(u, 0).unwrap();
    }
    d.reset();
    assert!(d.next_frame().is_none());
    let mut got = Vec::new();
    for (k, u) in units[4..].iter().enumerate() {
        d.decode(u, 100 + k as i64).unwrap();
        while let Some(f) = d.next_frame() {
            got.push(f);
        }
    }
    d.flush();
    while let Some(f) = d.next_frame() {
        got.push(f);
    }
    assert!(!got.is_empty());
    assert!(got[0].key, "output after reset starts at an I picture");
}

/// Heavier fuzzing over external streams: every file in `$LING_MPEG2_FUZZ_DIR` (at most
/// 4 MB each), `$LING_MPEG2_FUZZ_ITERS` (default 20) mutated copies of each, fed per
/// picture unit. `cargo test -p ling-mpeg2 -- --ignored fuzz_external`
#[test]
#[ignore]
fn fuzz_external() {
    let Ok(dir) = std::env::var("LING_MPEG2_FUZZ_DIR") else { return };
    let iters: usize =
        std::env::var("LING_MPEG2_FUZZ_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let Ok(es) = std::fs::read(&p) else { continue };
        if es.len() > 4 << 20 || es.len() < 16 {
            continue;
        }
        std::println!("fuzzing {}", p.display());
        for iter in 0..iters {
            let mut data = es.clone();
            let flips = 1 + rng.below(es.len() / 2000 + 2);
            for _ in 0..flips {
                let i = rng.below(data.len());
                if iter % 2 == 0 {
                    data[i] ^= 1 << rng.below(8);
                } else {
                    data[i] = rng.next() as u8;
                }
            }
            if iter % 5 == 4 {
                data.truncate(rng.below(data.len()) + 1);
            }
            let mut d = Decoder::new();
            for (k, u) in split(&data).iter().enumerate() {
                let _ = d.decode(u, k as i64);
                while let Some(f) = d.next_frame() {
                    let n = (f.width * f.height) as usize;
                    assert_eq!(f.y.len(), n);
                    out.resize(n, 0);
                    frame_to_rgba(&f, &mut out, true);
                }
                if rng.below(200) == 0 {
                    d.reset();
                }
            }
            d.flush();
            while d.next_frame().is_some() {}
        }
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() as usize) % n.max(1)
    }
}

#[test]
fn fuzz_never_panics() {
    let mut rng = Rng(0x1234_5678);
    let mut out = vec![0u32; 96 * 64];
    for (_, es) in fixtures() {
        for iter in 0..300 {
            let mut data = es.to_vec();
            match iter % 4 {
                0 => {
                    for _ in 0..1 + rng.below(20) {
                        let i = rng.below(data.len());
                        data[i] ^= 1 << rng.below(8);
                    }
                }
                1 => {
                    for _ in 0..1 + rng.below(8) {
                        let i = rng.below(data.len());
                        data[i] = rng.next() as u8;
                    }
                    data.truncate(rng.below(data.len()) + 1);
                }
                2 => {
                    // Insert junk start codes.
                    for _ in 0..1 + rng.below(6) {
                        let i = rng.below(data.len());
                        let code = [0x00, 0x01, 0xB3, 0xB5, 0xB8, 0x55, 0xAF][rng.below(7)];
                        let ins = [0, 0, 1, code, rng.next() as u8, rng.next() as u8];
                        data.splice(i..i, ins.iter().copied());
                    }
                }
                _ => {
                    // Sprinkle random bytes after the sequence header.
                    for b in data[20..].iter_mut() {
                        if rng.below(50) == 0 {
                            *b = rng.next() as u8;
                        }
                    }
                }
            }
            // Feed in random chunks (sometimes splitting pictures).
            let mut d = Decoder::new();
            let mut pos = 0;
            while pos < data.len() {
                let n = 1 + rng.below(6000);
                let end = (pos + n).min(data.len());
                let _ = d.decode(&data[pos..end], pos as i64);
                while let Some(f) = d.next_frame() {
                    let n = (f.width * f.height) as usize;
                    assert_eq!(f.y.len(), n);
                    if n <= out.len() {
                        frame_to_rgba(&f, &mut out[..n], true);
                    }
                }
                if rng.below(20) == 0 {
                    d.reset();
                }
                pos = end;
            }
            d.flush();
            while d.next_frame().is_some() {}
        }
    }
}

#[test]
fn rgba_and_deinterlace() {
    // A combed frame: a bright bar at x 20..40 in the top field and x 40..60 in the bottom
    // field (it moved between the fields), rows 0..40.
    let (w, h) = (96usize, 64usize);
    let mut y = vec![60u8; w * h];
    for r in 0..40 {
        for x in 0..w {
            let bar = if r % 2 == 0 { (20..40).contains(&x) } else { (40..60).contains(&x) };
            if bar {
                y[r * w + x] = 200;
            }
        }
    }
    // Static one-line detail in the bottom field (not repeated in its own field).
    for x in 70..90 {
        y[51 * w + x] = 200;
    }
    let f = Frame {
        width: w as u32,
        height: h as u32,
        y,
        u: vec![128; 48 * 32],
        v: vec![128; 48 * 32],
        pts: 0,
        progressive: false,
        top_field_first: true,
        dar: (4, 3),
        key: true,
    };
    let mut raw = vec![0u32; w * h];
    let mut di = vec![0u32; w * h];
    frame_to_rgba(&f, &mut raw, false);
    frame_to_rgba(&f, &mut di, true);
    let luma = |p: u32| p & 0xff; // grey: r = g = b
    assert!(raw.iter().all(|p| p >> 24 == 0xff));
    // Limited range: Y 60 -> (60-16)*1.164 = 51; Y 200 -> 214.
    assert_eq!(luma(raw[0]), 51);
    assert_eq!(luma(raw[30]), 214);
    // Combing gone: odd rows inside the moving region follow the top field.
    for r in (3..37).step_by(2) {
        assert!(luma(di[r * w + 30]) > 180, "row {r}: {}", luma(di[r * w + 30]));
        assert!(luma(di[r * w + 50]) < 90, "row {r}: {}", luma(di[r * w + 50]));
    }
    // Static thin detail kept.
    assert_eq!(luma(di[51 * w + 80]), 214);
    assert_eq!(luma(di[60 * w + 10]), 51);
    // Progressive frames are converted as-is.
    let mut p = f.clone();
    p.progressive = true;
    let mut a = vec![0u32; w * h];
    frame_to_rgba(&p, &mut a, true);
    assert_eq!(a, raw);
    // Too-small output buffer: no panic, nothing written.
    let mut small = vec![7u32; 10];
    frame_to_rgba(&f, &mut small, true);
    assert!(small.iter().all(|&v| v == 7));
}
