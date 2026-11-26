//! Host tests: reference comparison against ffmpeg, chunked streaming, ID3 skipping and a
//! truncation / bit-flip / random-frame fuzz loop (a panic would halt the LingOS kernel).

use alloc::vec::Vec;

use crate::{decode_file, frame_info, Decoder};

const L2_32K: &[u8] = include_bytes!("../tests/data/l2_32k_st.mp2");
const L2_32K_REF: &[u8] = include_bytes!("../tests/data/l2_32k_st.mp2.s16");
const L3_22K: &[u8] = include_bytes!("../tests/data/l3_22k_js.mp3");
const L3_22K_REF: &[u8] = include_bytes!("../tests/data/l3_22k_js.mp3.s16");
const L3_44K_VBR: &[u8] = include_bytes!("../tests/data/l3_44k_mono_vbr.mp3");
const L3_44K_VBR_REF: &[u8] = include_bytes!("../tests/data/l3_44k_mono_vbr.mp3.s16");

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn s16(b: &[u8]) -> Vec<i16> {
    b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
}

fn snr_db(reference: &[i16], x: &[i16]) -> f64 {
    let n = reference.len().min(x.len());
    let (mut s, mut e) = (0f64, 0f64);
    for i in 0..n {
        let r = f64::from(reference[i]);
        let d = r - f64::from(x[i]);
        s += r * r;
        e += d * d;
    }
    if e == 0.0 {
        f64::INFINITY
    }
    else {
        10.0 * (s / e).log10()
    }
}

fn stream(data: &[u8], chunk: usize) -> (Vec<f32>, Decoder) {
    let mut dec = Decoder::new();
    let mut out = Vec::new();
    for c in data.chunks(chunk.max(1)) {
        let _ = dec.decode(c, &mut out);
    }
    (out, dec)
}

#[test]
fn matches_ffmpeg() {
    for (name, file, reference, rate, ch) in [
        ("l2 32k stereo", L2_32K, L2_32K_REF, 32_000, 2),
        ("l3 22k joint", L3_22K, L3_22K_REF, 22_050, 2),
        ("l3 44k mono vbr", L3_44K_VBR, L3_44K_VBR_REF, 44_100, 1),
    ] {
        let (r, c, pcm) = decode_file(file).expect("decode_file");
        assert_eq!((r, c), (rate, ch), "{name}");
        let reference = s16(reference);
        assert_eq!(pcm.len(), reference.len(), "{name}: length");
        let snr = snr_db(&reference, &pcm);
        assert!(snr > 90.0, "{name}: SNR {snr:.1} dB");
    }
}

#[test]
fn info_frame_and_lame_tag() {
    let mut dec = Decoder::new();
    let mut out = Vec::new();
    dec.decode(L3_22K, &mut out).unwrap();
    // LAME: 576 samples encoder delay; the Info frame itself produces no output.
    assert_eq!(dec.lame_delay_padding().map(|d| d.0), Some(576));
    assert_eq!(out.len() % (576 * 2), 0);
}

#[test]
fn chunked_streaming_is_identical() {
    for file in [L2_32K, L3_22K, L3_44K_VBR] {
        let (whole, _) = stream(file, file.len());
        for chunk in [1, 3, 7, 100, 417, 1000] {
            let (part, _) = stream(file, chunk);
            assert!(part == whole, "chunk {chunk} differs");
        }
    }
}

#[test]
fn skips_id3_and_garbage() {
    for file in [L2_32K, L3_22K] {
        let (plain, _) = stream(file, file.len());
        let mut tagged = Vec::new();
        tagged.extend_from_slice(b"ID3\x04\x00\x00\x00\x00\x02\x2c"); // 300-byte tag
        tagged.extend(core::iter::repeat(0xffu8).take(300));
        tagged.extend_from_slice(&[0x12, 0xff, 0xfb, 0x00, 0xff, 0xf3, 0x55, 0x00, 0x01]);
        tagged.extend_from_slice(file);
        for chunk in [5, 64, 333, tagged.len()] {
            let (out, _) = stream(&tagged, chunk);
            assert!(out == plain, "chunk {chunk}: tagged output differs");
        }
    }
}

#[test]
fn frame_info_works() {
    let (len, rate, ch, spf) = frame_info(L2_32K).unwrap();
    assert_eq!((rate, ch, spf), (32_000, 2, 1152));
    assert_eq!(len, 144 * 128_000 / 32_000);
    assert!(frame_info(&[0xff, 0xff, 0xff, 0xff]).is_none());
    assert!(frame_info(&[0xff]).is_none());
}

#[test]
fn reset_mid_stream() {
    let mut dec = Decoder::new();
    let mut out = Vec::new();
    dec.decode(&L3_22K[..L3_22K.len() / 2], &mut out).unwrap();
    dec.reset();
    let before = out.len();
    dec.decode(&L3_22K[L3_22K.len() / 2 + 13..], &mut out).unwrap();
    assert!(out.len() > before);
    check(&out);
}

fn check(out: &[f32]) {
    assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
}

#[test]
fn fuzz_truncation_and_bitflips() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for file in [L2_32K, L3_22K, L3_44K_VBR] {
        // Truncations.
        let mut n = 0;
        while n < file.len() {
            let _ = decode_file(&file[..n]);
            let (out, _) = stream(&file[n / 2..n], 1 + n % 517);
            check(&out);
            n += 1 + rng.below(61);
        }
        // Bit flips, byte smears and cut-outs, fed in random chunk sizes.
        for _ in 0..400 {
            let mut d = file.to_vec();
            for _ in 0..1 + rng.below(40) {
                if d.is_empty() {
                    break;
                }
                let i = rng.below(d.len());
                match rng.below(4) {
                    0 => d[i] ^= 1 << rng.below(8),
                    1 => d[i] = rng.next() as u8,
                    2 => {
                        let e = (i + rng.below(64)).min(d.len());
                        d.drain(i..e);
                    }
                    _ => {
                        let e = (i + rng.below(32)).min(d.len());
                        for b in &mut d[i..e] {
                            *b = 0xff;
                        }
                    }
                }
            }
            let mut dec = Decoder::new();
            let mut out = Vec::new();
            let mut p = 0;
            while p < d.len() {
                let e = (p + 1 + rng.below(3000)).min(d.len());
                let _ = dec.decode(&d[p..e], &mut out);
                p = e;
            }
            check(&out);
            let _ = decode_file(&d);
        }
    }
}

#[test]
fn fuzz_random_frames() {
    // Frames with valid headers and random payloads drive the layer decoders through every
    // corrupt side-info / allocation / Huffman path.
    let mut rng = Rng(std::env::var("LING_MPA_FUZZ_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(0x1234_5678_9abc_def1));
    let mut dec = Decoder::new();
    let mut out = Vec::new();
    // LING_MPA_FUZZ_ROUNDS=1000000 for a long run.
    let rounds: usize =
        std::env::var("LING_MPA_FUZZ_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000);
    for round in 0..rounds {
        let word = loop {
            let w = 0xffe0_0000 | (rng.next() as u32 & 0x001f_ffff);
            if frame_info(&w.to_be_bytes()).is_some() {
                break w;
            }
        };
        let (len, ..) = frame_info(&word.to_be_bytes()).unwrap();
        let mut f = word.to_be_bytes().to_vec();
        for _ in 4..len {
            f.push(match rng.below(4) {
                0 => 0,
                1 => 0xff,
                _ => rng.next() as u8,
            });
        }
        // Repeat the frame so the look-ahead sync check accepts it, sometimes truncated.
        let mut d = f.clone();
        d.extend_from_slice(&f);
        if rng.below(3) == 0 {
            d.truncate(rng.below(d.len()));
        }
        let _ = dec.decode(&d, &mut out);
        check(&out);
        out.clear();
        if round % 1000 == 0 {
            dec = Decoder::new();
        }
    }
    // Pure random bytes, dense in sync-like values.
    for _ in 0..50 {
        let d: Vec<u8> = (0..20_000)
            .map(|_| if rng.below(3) == 0 { 0xff } else { rng.next() as u8 })
            .collect();
        let (out, _) = stream(&d, 1 + rng.below(5000));
        check(&out);
        let _ = decode_file(&d);
    }
}
