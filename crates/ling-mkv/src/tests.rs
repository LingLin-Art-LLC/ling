use super::*;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

struct FileSrc(File, u64);

impl Source for FileSrc {
    fn len(&self) -> u64 {
        self.1
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> bool {
        self.0.seek(SeekFrom::Start(off)).is_ok() && self.0.read_exact(buf).is_ok()
    }
}

fn media(name: &str) -> Option<FileSrc> {
    let dir = std::env::var("LING_MEDIA").unwrap_or_else(|_| String::from("tests/media"));
    let p = if name.contains(':') { PathBuf::from(name) } else { PathBuf::from(dir).join(name) };
    let f = File::open(&p).ok()?;
    let n = f.metadata().ok()?.len();
    Some(FileSrc(f, n))
}

#[test]
fn vints() {
    assert_eq!(vint(&[0x81]), Some((1, 1, false)));
    assert_eq!(vint(&[0x40, 0x02]), Some((2, 2, false)));
    assert_eq!(vint(&[0xFF]), Some((127, 1, true)));
    assert_eq!(vint(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]).map(|v| v.2), Some(true));
    assert_eq!(read_id(&[0x1A, 0x45, 0xDF, 0xA3]), Some((id::EBML, 4)));
    assert_eq!(sint(&[0xFF, 0xFE]), -2);
}

#[test]
fn lacing() {
    // Xiph: 3 frames of 2, 300, rest(1)
    let mut b = vec![2u8, 2, 255, 45];
    b.extend(std::iter::repeat(7u8).take(2 + 300 + 1));
    let f = unlace(1, &b).unwrap();
    assert_eq!(f.iter().map(|x| x.len()).collect::<Vec<_>>(), vec![2, 300, 1]);
    // fixed: 4 frames of 3
    let mut b = vec![3u8];
    b.extend(0..12u8);
    let f = unlace(2, &b).unwrap();
    assert_eq!(f.len(), 4);
    assert_eq!(f[3], &[9, 10, 11]);
    // EBML: sizes 10, 12 (+2), 9 (-3), rest
    let mut b = vec![3u8, 0x8A, 0xBF + 2, 0xBF - 3];
    b.extend(std::iter::repeat(1u8).take(10 + 12 + 9 + 5));
    let f = unlace(3, &b).unwrap();
    assert_eq!(f.iter().map(|x| x.len()).collect::<Vec<_>>(), vec![10, 12, 9, 5]);
    // garbage never panics
    for n in 0..64u8 {
        let b: Vec<u8> = (0..n).map(|i| i.wrapping_mul(37)).collect();
        for l in 0..4 {
            let _ = unlace(l, &b);
        }
    }
}

#[test]
fn squirrel_clip() {
    let Some(mut s) = media("squirrel1.mkv") else { return };
    let info = open(&mut s).expect("open");
    std::println!("timescale {} dur {} tracks {:?}", info.timescale, info.duration, info.tracks.iter().map(|t| (&t.codec_id, t.number, t.codec_private.len(), t.video.display_w, t.video.display_h)).collect::<Vec<_>>());
    assert!(info.tracks.iter().any(|t| t.codec_id == "V_MPEG2"));
    assert!(info.tracks.iter().any(|t| t.codec_id == "A_MPEG/L2"));
    let mut r = Reader::new(&info);
    let (mut v, mut a, mut keys) = (0, 0, 0);
    let mut last_a = i64::MIN;
    while let Some(p) = r.next_packet(&mut s, &info) {
        let t = info.track(p.track).unwrap();
        if t.kind == Kind::Video {
            v += 1;
            keys += p.key as u32;
        } else {
            a += 1;
            assert!(p.pts >= last_a);
            last_a = p.pts;
        }
    }
    std::println!("{v} video {a} audio packets, {keys} key; {} cues", info.cues.len());
    assert!(v > 1400 && a > 2000 && keys > 10);
    // seek to 30 s
    let at = r.seek(&mut s, &info, 30_000_000_000);
    let p = r.next_packet(&mut s, &info).unwrap();
    std::println!("seek 30 s -> cluster time {at}, first packet {} key {}", p.pts, p.key);
    assert!(at <= 30_000_000_000 && p.pts as u64 >= at.saturating_sub(1_000_000_000));
}

#[test]
fn makemkv_head() {
    // the first 64 MB of the user's 7 GB MakeMKV rip: Cues / Chapters sit beyond it
    let Some(mut s) = media("disc1-head.mkv") else { return };
    let info = open(&mut s).expect("open");
    std::println!("title {:?} dur {} s, {} chapters, {} cues, first cluster {}", info.title, info.duration / 1_000_000_000, info.chapters.len(), info.cues.len(), info.first_cluster);
    for t in &info.tracks {
        std::println!("track {} {:?} {} lang {} cp {} strip {} zlib {} {:?} {:?}", t.number, t.kind, t.codec_id, t.language, t.codec_private.len(), t.strip.len(), t.zlib, t.video, t.audio);
    }
    let mut r = Reader::new(&info);
    let mut n = 0;
    while let Some(p) = r.next_packet(&mut s, &info) {
        if n < 6 {
            std::println!("pkt track {} pts {} key {} len {} dur {:?}", p.track, p.pts, p.key, p.data.len(), p.duration);
        }
        n += 1;
    }
    std::println!("{n} packets in the head");
    assert!(n > 100);
}

#[test]
fn makemkv_full() {
    // the whole rip, when the drive is there: chapters, cues and a deep seek past 4 GiB
    let Some(mut s) = media("D:/Video/a-squirrel-and-a-hedgehog-parts-1-30/Disc 1 (Episode 1-15).mkv") else { return };
    let info = open(&mut s).expect("open");
    std::println!("dur {} s, {} chapters, {} cues", info.duration / 1_000_000_000, info.chapters.len(), info.cues.len());
    for c in info.chapters.iter().take(3) {
        std::println!("chapter {:?} at {} s", c.title, c.start / 1_000_000_000);
    }
    assert!(info.chapters.len() >= 15);
    assert!(!info.cues.is_empty());
    let mut r = Reader::new(&info);
    let at = r.seek(&mut s, &info, 17_000_000_000_000);
    let p = r.next_packet(&mut s, &info).unwrap();
    std::println!("seek 17000 s -> {} s, offset {}, packet {} s key {}", at / 1_000_000_000, r.position(), p.pts / 1_000_000_000, p.key);
    assert!(r.position() > (4u64 << 30));
}

#[test]
fn synthetic_samples() {
    for name in ["t-h264-aac-srt.mkv", "t-h264-opus-ass.mkv", "t-multi.mkv", "t-hevc.mkv", "t-vp9.webm"] {
        let Some(mut s) = media(name) else { continue };
        let info = open(&mut s).expect(name);
        std::println!("{name}: {:?}", info.tracks.iter().map(|t| (t.number, t.kind, t.codec_id.clone(), t.language.clone(), t.name.clone(), t.zlib, t.codec_private.len())).collect::<Vec<_>>());
        let mut r = Reader::new(&info);
        let mut per: std::collections::BTreeMap<u64, usize> = Default::default();
        while let Some(p) = r.next_packet(&mut s, &info) {
            *per.entry(p.track).or_default() += 1;
            if info.track(p.track).unwrap().kind == Kind::Subtitle {
                std::println!("  sub {} ms +{:?}: {:?}", p.pts / 1_000_000, p.duration.map(|d| d / 1_000_000), String::from_utf8_lossy(&p.data));
            }
        }
        std::println!("  packets per track {per:?}");
    }
}

#[test]
fn fuzz_no_panic() {
    let Some(mut s) = media("squirrel1.mkv") else { return };
    let mut head = vec![0u8; 1 << 20];
    assert!(s.read_at(0, &mut head));
    let mut seed = 12345u32;
    for round in 0..300 {
        let mut b = head.clone();
        for _ in 0..(1 + round % 20) {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let i = (seed as usize >> 4) % b.len().min(if round % 2 == 0 { 4096 } else { b.len() });
            b[i] ^= (seed >> 24) as u8 | 1;
        }
        let cut = if round % 3 == 0 { (seed as usize) % b.len() } else { b.len() };
        let mut src: &[u8] = &b[..cut];
        if let Ok(info) = open(&mut src) {
            let mut r = Reader::new(&info);
            let mut n = 0;
            while r.next_packet(&mut src, &info).is_some() && n < 5000 {
                n += 1;
            }
            r.seek(&mut src, &info, 5_000_000_000);
            let _ = r.next_packet(&mut src, &info);
        }
    }
}
