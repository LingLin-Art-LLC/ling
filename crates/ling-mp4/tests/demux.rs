//! Compare ling-mp4 against `ffprobe -show_packets` on files made by
//! `tools/gen-testdata.sh` (tests skip with a note when the data is missing).

use std::path::PathBuf;

use ling_mp4::*;

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

#[derive(Debug, Clone, PartialEq)]
struct Pkt {
    stream: usize,
    pts: i64,
    dts: i64,
    duration: i64,
    size: u32,
    pos: u64,
    key: bool,
}

fn packets(name: &str) -> Vec<Pkt> {
    let text = String::from_utf8(data(&format!("{}.packets.csv", name)).unwrap()).unwrap();
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            Pkt {
                stream: f[0].parse().unwrap(),
                pts: f[1].parse().unwrap(),
                dts: f[2].parse().unwrap(),
                duration: f[3].parse().unwrap(),
                size: f[4].parse().unwrap(),
                pos: f[5].parse().unwrap(),
                key: f[6].starts_with('K'),
            }
        })
        .collect()
}

/// Extradata per stream from `ffprobe -show_data -of flat` output.
fn extradata(name: &str) -> Vec<Vec<u8>> {
    let text = String::from_utf8(data(&format!("{}.streams.txt", name)).unwrap()).unwrap();
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.split_once(".extradata=").map(|x| x.1) {
            let mut bytes = Vec::new();
            for row in rest.trim_matches('"').split("\\n") {
                if row.len() < 10 || !row.as_bytes()[8].eq(&b':') {
                    continue;
                }
                let hex: String = row[10..row.len().min(49)].chars().filter(|c| !c.is_whitespace()).collect();
                for i in (0..hex.len() / 2 * 2).step_by(2) {
                    bytes.push(u8::from_str_radix(&hex[i..i + 2], 16).unwrap());
                }
            }
            out.push(bytes);
        }
    }
    out
}

fn to_pkt(stream: usize, s: &Sample, base: u64) -> Pkt {
    Pkt {
        stream,
        pts: s.pts,
        dts: s.dts as i64,
        duration: s.duration as i64,
        size: s.size,
        pos: base + s.offset,
        key: s.keyframe,
    }
}

fn movie_of(file: &[u8]) -> Movie {
    let boxes = scan_boxes(file, 0).unwrap();
    let moov = boxes.iter().find(|b| &b.kind == b"moov").expect("moov");
    Movie::parse_moov(moov.payload(file).unwrap()).unwrap()
}

fn check_codecs(name: &str, movie: &Movie) {
    let extra = extradata(name);
    assert_eq!(extra.len(), movie.tracks.len());
    for (t, x) in movie.tracks.iter().zip(extra.iter()) {
        match &t.codec {
            Codec::Avc { avcc, .. } => {
                assert_eq!(avcc, x, "avcC bytes");
                assert_eq!((t.width, t.height), (320, 240));
                assert_eq!(t.kind, TrackKind::Video);
            }
            Codec::Mp4a { asc, object_type_indication } => {
                assert_eq!(asc, x, "AudioSpecificConfig bytes");
                assert_eq!(*object_type_indication, 0x40);
                assert_eq!(t.kind, TrackKind::Audio);
                assert!(t.sample_rate == 44100 || t.sample_rate == 48000);
            }
            c => panic!("unexpected codec {:?}", c),
        }
        assert!(t.timescale > 0);
    }
}

fn compare(name: &str, ours: &[Pkt], theirs: &[Pkt], streams: usize) {
    for st in 0..streams {
        let a: Vec<&Pkt> = ours.iter().filter(|p| p.stream == st).collect();
        let b: Vec<&Pkt> = theirs.iter().filter(|p| p.stream == st).collect();
        assert_eq!(a.len(), b.len(), "{} stream {}: sample count", name, st);
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            assert_eq!(x, y, "{} stream {} sample {}", name, st, i);
        }
        println!("{} stream {}: {} samples match ffprobe (size/pos/dts/pts/duration/key)", name, st, a.len());
    }
}

fn check_regular(name: &str) {
    let Some(file) = data(name) else { return };
    let movie = movie_of(&file);
    assert!(!movie.fragmented);
    check_codecs(name, &movie);
    let mut ours = Vec::new();
    for (i, t) in movie.tracks.iter().enumerate() {
        assert_eq!(t.sample_count() as usize, t.samples().count());
        ours.extend(t.samples().map(|s| to_pkt(i, &s, 0)));
        // the edit list carries the encoder delay / B-frame shift
        assert!(t.edit_media_time.is_some());
    }
    compare(name, &ours, &packets(name), movie.tracks.len());

    // seeking helper: the sync sample at or before 2 s of video
    let v = &movie.tracks[0];
    let (i, s) = v.seek_sample(2 * v.timescale as i64).unwrap();
    assert!(s.keyframe && s.pts <= 2 * v.timescale as i64);
    println!("{}: seek to 2 s -> sample {} (pts {})", name, i, s.pts);
}

/// Walk every top-level moof (+ its mdat) like a streaming client would.
fn fragment_packets(file: &[u8], movie: &mut Movie) -> Vec<Pkt> {
    let boxes = scan_boxes(file, 0).unwrap();
    let mut out = Vec::new();
    for (bi, b) in boxes.iter().enumerate() {
        if &b.kind != b"moof" {
            continue;
        }
        let end = boxes.get(bi + 1).filter(|n| &n.kind == b"mdat").and_then(|n| n.end()).unwrap_or(b.end().unwrap());
        let buf = &file[b.offset as usize..end as usize];
        let frag = movie.parse_fragment(buf, b.offset).unwrap();
        assert_eq!(frag.moof_size, b.size.unwrap());
        for s in &frag.samples {
            assert!(s.offset + s.size as u64 <= buf.len() as u64, "sample inside the fetched range");
        }
        for (ti, t) in movie.tracks.iter().enumerate() {
            for s in frag.track_samples(t.id) {
                out.push(to_pkt(ti, s, b.offset));
            }
        }
    }
    out
}

fn check_fragmented(name: &str) -> Option<(Vec<u8>, Movie)> {
    let file = data(name)?;
    let mut movie = Movie::parse_init(&file).unwrap();
    assert!(movie.fragmented);
    check_codecs(name, &movie);
    for t in &movie.tracks {
        assert_eq!(t.sample_count(), 0);
        assert!(t.defaults.sample_description_index >= 1);
    }
    let mut ours = fragment_packets(&file, &mut movie);
    // ffprobe lists packets in file order; ours are per fragment / per track.
    ours.sort_by_key(|p| p.pos);
    compare(name, &ours, &packets(name), movie.tracks.len());
    Some((file, movie))
}

#[test]
fn regular_moov_at_end() {
    check_regular("av.mp4");
}

#[test]
fn regular_faststart() {
    check_regular("av_faststart.mp4");
}

#[test]
fn fragmented_default_base_moof() {
    check_fragmented("av_frag.mp4");
}

#[test]
fn fragmented_absolute_base_offset() {
    check_fragmented("av_frag_abs.mp4");
}

fn check_sidx(name: &str) {
    let Some((file, mut movie)) = check_fragmented(name) else { return };
    let boxes = scan_boxes(&file, 0).unwrap();
    let sidx_box = boxes.iter().find(|b| &b.kind == b"sidx").expect("sidx");
    let sidx = Sidx::parse(sidx_box.bytes(&file).unwrap(), sidx_box.offset).unwrap();
    let track = movie.track(sidx.reference_id).unwrap().clone();
    assert_eq!(sidx.timescale, track.timescale);
    movie.reset_fragment_times();
    let moofs: Vec<&BoxInfo> = boxes.iter().filter(|b| &b.kind == b"moof").collect();
    assert_eq!(sidx.entries.len(), moofs.len());
    let mut total = 0u64;
    for (e, m) in sidx.entries.iter().zip(moofs.iter()) {
        assert!(!e.is_sidx);
        assert_eq!(e.offset, m.offset, "segment offset points at its moof");
        // Fetch exactly the byte range the index names and demux it.
        let seg = &file[e.offset as usize..(e.offset + e.size as u64) as usize];
        let frag = movie.parse_fragment(seg, e.offset).unwrap();
        let first = frag.samples.first().unwrap();
        let earliest = frag.samples.iter().map(|s| s.pts).min().unwrap();
        assert_eq!(earliest as u64, e.start_time, "segment earliest presentation time");
        let dur: u64 = frag.samples.iter().map(|s| s.duration as u64).sum();
        assert_eq!(dur, e.duration as u64, "segment duration");
        assert!(frag.samples.iter().all(|s| s.offset + s.size as u64 <= seg.len() as u64));
        assert!(first.keyframe && e.starts_with_sap);
        total += e.size as u64;
    }
    let last_mdat = boxes.iter().filter(|b| &b.kind == b"mdat").last().unwrap();
    assert_eq!(sidx.entries.last().map(|e| e.offset + e.size as u64), last_mdat.end());
    let t = sidx.entries[1].start_time + 1;
    assert_eq!(sidx.segment_at(t), Some(1));
    println!("{}: sidx with {} segments covering {} bytes verified", name, sidx.entries.len(), total);
}

#[test]
fn dash_video_global_sidx() {
    check_sidx("v_dash.mp4");
}

#[test]
fn dash_audio_global_sidx() {
    check_sidx("a_dash.m4a");
}

#[test]
fn dash_muxer_single_file() {
    for name in ["dash_single_v.mp4", "dash_single_a.mp4"] {
        let Some((file, _)) = check_fragmented(name) else { return };
        // ffmpeg's dash muxer writes one sidx per segment; each must point at the next moof.
        let boxes = scan_boxes(&file, 0).unwrap();
        let mut n = 0;
        for (i, b) in boxes.iter().enumerate() {
            if &b.kind == b"sidx" {
                let s = Sidx::parse(b.bytes(&file).unwrap(), b.offset).unwrap();
                let next = boxes[i + 1];
                assert_eq!(&next.kind, b"moof");
                assert_eq!(s.entries[0].offset, next.offset);
                n += 1;
            }
        }
        assert!(n >= 1);
        println!("{}: {} per-segment sidx boxes verified", name, n);
    }
}

#[test]
fn streaming_partial_data() {
    let Some(file) = data("v_dash.mp4") else { return };
    // Only the first 100 bytes: ftyp complete, moov cut off.
    let head = &file[..100];
    let boxes = scan_boxes(head, 0).unwrap();
    assert_eq!(&boxes[0].kind, b"ftyp");
    assert!(boxes[0].complete);
    let moov = boxes[1];
    assert_eq!(&moov.kind, b"moov");
    assert!(!moov.complete);
    match Movie::parse_init(head) {
        Err(Mp4Error::NeedMoreData { needed }) => assert_eq!(needed, moov.end().unwrap()),
        r => panic!("expected NeedMoreData, got {:?}", r.map(|_| ())),
    }
    // Fetch up to the end of moov + the sidx header: now the moov parses.
    let needed = moov.end().unwrap() as usize;
    let movie = Movie::parse_init(&file[..needed]).unwrap();
    assert_eq!(movie.tracks.len(), 1);
    // A second range fetched separately (as an HTTP Range response) scans at its file offset.
    let sidx_off = needed as u64;
    let range = &file[needed..needed + 4096];
    let b = scan_boxes(range, sidx_off).unwrap();
    assert_eq!(&b[0].kind, b"sidx");
    let sidx = Sidx::parse(b[0].bytes_at(range, sidx_off).unwrap(), b[0].offset).unwrap();
    assert!(sidx.entries.len() > 1);
    // A fragment whose moof is cut off asks for more.
    let e = sidx.entries[0];
    let mut movie = movie;
    match movie.parse_fragment(&file[e.offset as usize..e.offset as usize + 40], e.offset) {
        Err(Mp4Error::NeedMoreData { .. }) => {}
        r => panic!("expected NeedMoreData, got {:?}", r.map(|_| ())),
    }
    // moof only (mdat not yet fetched): sample table still available.
    let moof_end = scan_boxes(&file[e.offset as usize..], e.offset).unwrap()[0].end().unwrap();
    let frag = movie.parse_fragment(&file[e.offset as usize..moof_end as usize], e.offset).unwrap();
    assert!(!frag.samples.is_empty());
}

// ---------------------------------------------------------------------------
// Robustness

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn poke(buf: &[u8]) {
    if let Ok(boxes) = scan_boxes(buf, 0) {
        for b in &boxes {
            if let Some(p) = b.payload(buf) {
                if &b.kind == b"moov" {
                    if let Ok(mut m) = Movie::parse_moov(p) {
                        for t in &m.tracks {
                            let _ = t.samples().take(200_000).count();
                            let _ = t.seek_sample(1000);
                        }
                        for b2 in &boxes {
                            if &b2.kind == b"moof" {
                                let _ = m.parse_fragment(&buf[b2.offset as usize..], b2.offset);
                            }
                        }
                    }
                }
                if &b.kind == b"sidx" {
                    let _ = Sidx::parse(b.bytes(buf).unwrap(), b.offset);
                }
            }
        }
    }
    let _ = Movie::parse_init(buf);
    let _ = Movie::parse_moov(buf);
    let _ = Sidx::parse(buf, 0);
    let _ = parse_esds(buf);
}

#[test]
fn garbage_never_panics() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..3000 {
        let n = (rng.next() % 300) as usize;
        let buf: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
        poke(&buf);
    }
    // Plausible box structure with random contents.
    for _ in 0..3000 {
        let mut buf = Vec::new();
        for _ in 0..(rng.next() % 6) {
            let kinds: [&[u8; 4]; 12] = [b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsz", b"stts", b"stco", b"moof", b"traf", b"trun", b"sidx"];
            let k = kinds[(rng.next() % 12) as usize];
            let len = (rng.next() % 64) as usize;
            buf.extend_from_slice(&((len + 8) as u32).to_be_bytes());
            buf.extend_from_slice(k);
            buf.extend((0..len).map(|_| rng.next() as u8));
        }
        poke(&buf);
    }
}

#[test]
fn corrupted_files_never_panic() {
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    for name in ["av.mp4", "av_faststart.mp4", "av_frag.mp4", "v_dash.mp4", "dash_single_a.mp4"] {
        let Some(file) = data(name) else { return };
        // truncations
        for cut in (0..file.len()).step_by(97).chain([1usize, 7, 8, 9, 15, 16, 17]) {
            poke(&file[..cut.min(file.len())]);
        }
        // byte flips in the box structure (first 2 KiB holds moov / sidx / moof)
        let head = 2048.min(file.len());
        for _ in 0..400 {
            let mut c = file.clone();
            for _ in 0..1 + rng.next() % 4 {
                let i = if rng.next() % 4 == 0 { (rng.next() as usize) % c.len() } else { (rng.next() as usize) % head };
                c[i] = rng.next() as u8;
            }
            poke(&c);
        }
    }
}

#[test]
fn huge_counts_are_bounded() {
    // A trun claiming 4 billion default-sized samples must be rejected, not looped over.
    let mut moof = Vec::new();
    let mut traf = Vec::new();
    let mut tfhd = vec![0, 0x02, 0, 0x38]; // default-base-is-moof + default duration/size/flags
    tfhd.extend_from_slice(&1u32.to_be_bytes());
    tfhd.extend_from_slice(&1u32.to_be_bytes());
    tfhd.extend_from_slice(&1u32.to_be_bytes());
    tfhd.extend_from_slice(&0u32.to_be_bytes());
    push_box(&mut traf, b"tfhd", &tfhd);
    let mut trun = vec![0, 0, 0, 0];
    trun.extend_from_slice(&u32::MAX.to_be_bytes());
    push_box(&mut traf, b"trun", &trun);
    push_box(&mut moof, b"traf", &traf);
    let mut buf = Vec::new();
    push_box(&mut buf, b"moof", &moof);

    let mut moov = Vec::new();
    let mut trak = Vec::new();
    let mut tkhd = vec![0u8; 4 + 8];
    tkhd.extend_from_slice(&1u32.to_be_bytes());
    tkhd.extend_from_slice(&[0u8; 4 + 4 + 8 + 8 + 36 + 8]);
    push_box(&mut trak, b"tkhd", &tkhd);
    let mut mdia = Vec::new();
    push_box(&mut mdia, b"mdhd", &[0u8; 24]);
    let mut minf = Vec::new();
    let mut stbl = Vec::new();
    // stsz: constant size, 4 billion samples (capped to MAX_SAMPLES) but no chunks.
    let mut stsz = vec![0u8; 4];
    stsz.extend_from_slice(&100u32.to_be_bytes());
    stsz.extend_from_slice(&u32::MAX.to_be_bytes());
    push_box(&mut stbl, b"stsz", &stsz);
    push_box(&mut minf, b"stbl", &stbl);
    push_box(&mut mdia, b"minf", &minf);
    push_box(&mut trak, b"mdia", &mdia);
    push_box(&mut moov, b"trak", &trak);
    push_box(&mut moov, b"mvex", &[]);
    let mut movie = Movie::parse_moov(&moov).unwrap();
    assert_eq!(movie.tracks[0].samples().count(), 0, "no chunk offsets -> no samples");
    assert!(matches!(movie.parse_fragment(&buf, 0), Err(Mp4Error::Invalid(_))));
}

fn push_box(out: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
}

/// Longer fuzz run: `cargo test -p ling-mp4 -- --ignored fuzz_long`.
#[test]
#[ignore]
fn fuzz_long() {
    let mut rng = Rng(0x7777_1234_dead_0001);
    for name in ["av.mp4", "av_faststart.mp4", "av_frag.mp4", "av_frag_abs.mp4", "v_dash.mp4", "a_dash.m4a", "dash_single_v.mp4"] {
        let Some(file) = data(name) else { return };
        for _ in 0..3000 {
            let mut c = file.clone();
            for _ in 0..1 + rng.next() % 16 {
                let i = (rng.next() as usize) % c.len().min(4096);
                c[i] = rng.next() as u8;
            }
            poke(&c);
        }
    }
}
