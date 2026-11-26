//! Decode FFmpeg / libfdk-aac encoded test vectors and compare against FFmpeg's
//! float decoder. Test data comes from `tools/gen-testdata.sh` (tests skip
//! with a note when it has not been generated).

use std::path::PathBuf;

use ling_aac::{AacDecoder, AacError};
use ling_mp4::{scan_boxes, Codec, Movie};

fn data(name: &str) -> Option<Vec<u8>> {
    let p: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests", "data", name].iter().collect();
    match std::fs::read(&p) {
        Ok(d) => Some(d),
        Err(_) => {
            eprintln!("SKIP: {} missing (run tools/gen-testdata.sh)", p.display());
            None
        }
    }
}

fn read_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// (asc, access units) from an .m4a via ling-mp4.
fn m4a_units(file: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let boxes = scan_boxes(file, 0).unwrap();
    let moov = boxes.iter().find(|b| &b.kind == b"moov").unwrap();
    let movie = Movie::parse_moov(moov.payload(file).unwrap()).unwrap();
    let t = &movie.tracks[0];
    let asc = match &t.codec {
        Codec::Mp4a { asc, .. } => asc.clone(),
        c => panic!("not mp4a: {:?}", c),
    };
    let aus = t
        .samples()
        .map(|s| file[s.offset as usize..s.offset as usize + s.size as usize].to_vec())
        .collect();
    (asc, aus)
}

/// (asc, access units) from the fdkenc .aus format.
fn aus_units(file: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let rd = |p: usize| u32::from_le_bytes([file[p], file[p + 1], file[p + 2], file[p + 3]]) as usize;
    let n = rd(0);
    let asc = file[4..4 + n].to_vec();
    let mut pos = 4 + n;
    let mut aus = Vec::new();
    while pos + 4 <= file.len() {
        let l = rd(pos);
        aus.push(file[pos + 4..pos + 4 + l].to_vec());
        pos += 4 + l;
    }
    (asc, aus)
}

fn decode_all(asc: &[u8], aus: &[Vec<u8>]) -> (AacDecoder, Vec<f32>) {
    let t0 = std::time::Instant::now();
    let mut dec = AacDecoder::new(asc).expect("decoder");
    let mut pcm = Vec::new();
    for (i, au) in aus.iter().enumerate() {
        let before = pcm.len();
        let n = dec.decode(au, &mut pcm).unwrap_or_else(|e| panic!("AU {}: {:?}", i, e));
        assert_eq!(pcm.len() - before, n * dec.channels() as usize);
    }
    let secs = pcm.len() as f64 / dec.channels() as f64 / dec.sample_rate() as f64;
    println!("decoded {:.2} s of audio in {:.1} ms ({:.0}x realtime)", secs, t0.elapsed().as_secs_f64() * 1e3,
        secs / t0.elapsed().as_secs_f64());
    (dec, pcm)
}

/// Best alignment of `ours` (channel `oc` of `och`) against `refd` (channel
/// `rc` of `rch`) and the SNR in dB over the overlap.
fn snr(ours: &[f32], och: usize, oc: usize, refd: &[f32], rch: usize, rc: usize, max_lag: usize) -> (f64, isize) {
    let a: Vec<f64> = ours.iter().skip(oc).step_by(och).map(|&x| x as f64).collect();
    let b: Vec<f64> = refd.iter().skip(rc).step_by(rch).map(|&x| x as f64).collect();
    let mut best = (f64::MIN, 0isize);
    // Lag search on a window from the middle: ours[i + lag] ~ ref[i].
    let w0 = 8192.min(b.len() / 4);
    let wl = 8192.min(b.len() / 2);
    for lag in -(max_lag as isize)..=(max_lag as isize) {
        let mut dot = 0.0;
        let mut e = 0.0;
        for i in w0..w0 + wl {
            let j = i as isize + lag;
            if j < 0 || j as usize >= a.len() {
                continue;
            }
            dot += a[j as usize] * b[i];
            e += a[j as usize] * a[j as usize];
        }
        let score = if e > 0.0 { dot / e.sqrt() } else { f64::MIN };
        if score > best.0 {
            best = (score, lag);
        }
    }
    let lag = best.1;
    let (mut sig, mut err) = (0.0, 0.0);
    // Skip the very first / last frame region where encoder priming / flush differ.
    for i in 2048..b.len().saturating_sub(2048) {
        let j = i as isize + lag;
        if j < 0 || j as usize >= a.len() {
            continue;
        }
        let d = a[j as usize] - b[i];
        sig += b[i] * b[i];
        err += d * d;
    }
    (10.0 * (sig / err.max(1e-30)).log10(), lag)
}

fn check_lc(name: &str, min_snr: f64) {
    let Some(file) = data(&format!("{}.m4a", name)) else { return };
    let refd = read_f32(&data(&format!("{}.ref.f32", name)).unwrap());
    let (asc, aus) = m4a_units(&file);
    let (dec, pcm) = decode_all(&asc, &aus);
    let ch = dec.channels() as usize;
    assert!(!dec.sbr_active());
    for c in 0..ch {
        let (s, lag) = snr(&pcm, ch, c, &refd, ch, c, 4096);
        println!("{} ch{}: {} Hz, SNR {:.1} dB (lag {})", name, c, dec.sample_rate(), s, lag);
        assert_eq!(lag, 1024, "FFmpeg drops the 1024-sample encoder delay (edit list)");
        assert!(s > min_snr, "{} ch{}: SNR {:.1} dB < {}", name, c, s, min_snr);
    }
}

#[test]
fn lc_sweep44() {
    check_lc("lc_sweep44", 60.0);
}
#[test]
fn lc_noise44() {
    check_lc("lc_noise44", 60.0);
}
#[test]
fn lc_music44() {
    check_lc("lc_music44", 60.0);
}
#[test]
fn lc_music48() {
    check_lc("lc_music48", 60.0);
}
#[test]
fn lc_music48_96k() {
    check_lc("lc_music48_96k", 60.0);
}
#[test]
fn lc_music44_mono() {
    check_lc("lc_music44_mono", 60.0);
}
#[test]
fn lc_music44_nopns() {
    check_lc("lc_music44_nopns", 60.0);
}

fn check_he(name: &str, explicit: bool, min_snr: f64) {
    let (asc, aus) = if explicit {
        let Some(f) = data(&format!("{}.aus", name)) else { return };
        aus_units(&f)
    } else {
        let Some(f) = data(&format!("{}_implicit.m4a", name)) else { return };
        m4a_units(&f)
    };
    let Some(r) = data(&format!("{}.ref.f32", name)) else { return };
    let refd = read_f32(&r);
    let (dec, pcm) = decode_all(&asc, &aus);
    assert!(dec.sbr_active(), "SBR should be active");
    let ch = dec.channels() as usize;
    // FFmpeg outputs stereo for mono HE-AAC in ADTS (PS cannot be ruled out).
    let rch = if name.contains("mono") || name.starts_with("hev2") { 2 } else { ch };
    for c in 0..ch {
        let (s, lag) = snr(&pcm, ch, c, &refd, rch, c.min(rch - 1), 6000);
        println!(
            "{} ({}) ch{}: {} Hz, SNR {:.1} dB (lag {})",
            name,
            if explicit { "explicit" } else { "implicit" },
            c,
            dec.sample_rate(),
            s,
            lag
        );
        assert!(s > min_snr, "{} ch{}: SNR {:.1} dB < {}", name, c, s, min_snr);
    }
}

#[test]
fn he_music44() {
    check_he("he_music44", true, 40.0);
}
#[test]
fn he_music44_implicit() {
    check_he("he_music44", false, 40.0);
}
#[test]
fn he_sweep44() {
    check_he("he_sweep44", true, 40.0);
}
#[test]
fn he_music48() {
    check_he("he_music48", true, 40.0);
}
#[test]
fn he_music44_mono() {
    check_he("he_music44_mono", true, 40.0);
}

#[test]
fn hev2_ps_decodes_as_mono() {
    let Some(f) = data("hev2_music44.aus") else { return };
    let (asc, aus) = aus_units(&f);
    let (dec, pcm) = decode_all(&asc, &aus);
    assert!(dec.sbr_active());
    assert!(dec.ps_ignored());
    assert_eq!(dec.channels(), 2);
    assert_eq!(dec.sample_rate(), 44100);
    let peak = pcm.iter().fold(0f32, |m, &x| m.max(x.abs()));
    println!("hev2: {} samples, peak {:.3}", pcm.len(), peak);
    assert!(peak > 0.05 && peak < 2.0);
}

// ---------------------------------------------------------------------------
// Robustness: garbage / truncated / bit-flipped input must never panic.

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn garbage_never_panics() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut out = Vec::new();
    // Random configs.
    for _ in 0..2000 {
        let n = (rng.next() % 8) as usize;
        let asc: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
        let _ = AacDecoder::new(&asc);
    }
    // Valid configs, random access units.
    for asc in [[0x12u8, 0x10], [0x11, 0x90], [0x13, 0x10], [0x11, 0x88], [0x2b, 0x92]] {
        let Ok(mut dec) = AacDecoder::new(&asc) else { continue };
        for _ in 0..3000 {
            let n = (rng.next() % 900) as usize;
            let au: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
            out.clear();
            let _ = dec.decode(&au, &mut out);
        }
    }
    // Explicit HE-AAC config + garbage (exercises the SBR parser).
    let he = [0x2b, 0x92, 0x08, 0x00];
    if let Ok(mut dec) = AacDecoder::new(&he) {
        for _ in 0..2000 {
            let n = (rng.next() % 600) as usize;
            let au: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
            let _ = dec.decode(&au, &mut out);
        }
    }
}

#[test]
fn corrupted_real_streams_never_panic() {
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    let mut sets = Vec::new();
    if let Some(f) = data("lc_music44.m4a") {
        sets.push(m4a_units(&f));
    }
    if let Some(f) = data("he_music44.aus") {
        sets.push(aus_units(&f));
    }
    if let Some(f) = data("hev2_music44.aus") {
        sets.push(aus_units(&f));
    }
    for (asc, aus) in sets {
        let mut dec = AacDecoder::new(&asc).unwrap();
        let mut out = Vec::new();
        let mut errors = 0;
        for au in aus.iter().take(120) {
            // truncations
            for cut in [0, 1, 2, au.len() / 3, au.len() / 2, au.len().saturating_sub(1)] {
                if dec.decode(&au[..cut.min(au.len())], &mut out).is_err() {
                    errors += 1;
                }
            }
            // bit flips
            for _ in 0..6 {
                let mut c = au.clone();
                if c.is_empty() {
                    continue;
                }
                for _ in 0..1 + rng.next() % 4 {
                    let i = (rng.next() as usize) % c.len();
                    c[i] ^= 1 << (rng.next() % 8);
                }
                if dec.decode(&c, &mut out).is_err() {
                    errors += 1;
                }
            }
            // and the real AU still decodes afterwards
            dec.decode(au, &mut out).unwrap();
            assert!(out.iter().all(|x| x.is_finite()));
            out.clear();
        }
        println!("corrupted AUs rejected: {}", errors);
    }
}

#[test]
fn error_type_is_reported() {
    assert!(matches!(AacDecoder::new(&[]), Err(AacError::EndOfData)));
    // AAC Main is not supported
    assert!(matches!(AacDecoder::new(&[0x0a, 0x10]), Err(AacError::Unsupported(_))));
}

/// End to end like the YouTube player: init segment -> sidx -> fetch each
/// segment's byte range -> demux the fragment -> decode.
#[test]
fn dash_stream_end_to_end() {
    use ling_mp4::Sidx;
    let Some(file) = data("lc_dash48.m4a") else { return };
    let refd = read_f32(&data("lc_dash48.ref.f32").unwrap());
    let boxes = scan_boxes(&file, 0).unwrap();
    let mut movie = Movie::parse_init(&file).unwrap();
    let track = movie.tracks[0].clone();
    let Codec::Mp4a { asc, .. } = &track.codec else { panic!("not mp4a") };
    let mut dec = AacDecoder::new(asc).unwrap();
    let sidx_box = boxes.iter().find(|b| &b.kind == b"sidx").unwrap();
    let sidx = Sidx::parse(sidx_box.bytes(&file).unwrap(), sidx_box.offset).unwrap();
    let mut pcm = Vec::new();
    let mut aus = 0;
    for seg in &sidx.entries {
        let bytes = &file[seg.offset as usize..(seg.offset + seg.size as u64) as usize];
        let frag = movie.parse_fragment(bytes, seg.offset).unwrap();
        for s in frag.track_samples(track.id) {
            let au = &bytes[s.offset as usize..(s.offset + s.size as u64) as usize];
            dec.decode(au, &mut pcm).unwrap();
            aus += 1;
        }
    }
    for c in 0..2 {
        let (s, lag) = snr(&pcm, 2, c, &refd, 2, c, 4096);
        println!("dash e2e: {} segments, {} AUs, ch{} SNR {:.1} dB (lag {})", sidx.entries.len(), aus, c, s, lag);
        assert!(s > 60.0);
    }
}

/// Longer fuzz run: `cargo test -p ling-aac -- --ignored fuzz_long`.
#[test]
#[ignore]
fn fuzz_long() {
    let mut rng = Rng(0x5151_aaaa_0f0f_3c3c);
    let mut sets = Vec::new();
    for f in ["lc_music44.m4a", "lc_noise44.m4a", "lc_music44_mono.m4a"] {
        if let Some(d) = data(f) {
            sets.push(m4a_units(&d));
        }
    }
    for f in ["he_music44.aus", "he_sweep44.aus", "he_music44_mono.aus", "hev2_music44.aus"] {
        if let Some(d) = data(f) {
            sets.push(aus_units(&d));
        }
    }
    let mut out = Vec::new();
    for (asc, aus) in &sets {
        let mut dec = AacDecoder::new(asc).unwrap();
        for round in 0..40 {
            for au in aus {
                let mut c = au.clone();
                if !c.is_empty() {
                    for _ in 0..1 + rng.next() % 8 {
                        let i = (rng.next() as usize) % c.len();
                        c[i] ^= 1 << (rng.next() % 8);
                    }
                    if round % 3 == 0 {
                        let cut = (rng.next() as usize) % c.len();
                        c.truncate(cut);
                    }
                }
                out.clear();
                let _ = dec.decode(&c, &mut out);
                assert!(out.iter().all(|x| x.is_finite()));
            }
        }
    }
}
