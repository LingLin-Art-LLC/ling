//! MP4 / ISO-BMFF writer for an H.264 video track plus an optional AAC
//! audio track (no_std + alloc).
//!
//! Two layouts:
//!
//! * **Fragmented** (while recording): [`ftyp`] + [`moov_fragmented_av`]
//!   once, then [`fragment_header_av`] + the samples' bytes for every
//!   fragment (a `moof` + `mdat` pair, each track's samples in turn).
//! * **Regular** ("fast start", best compatibility): [`layout_av`] gives
//!   `ftyp + moov + mdat header` once the samples are known, with the chunk
//!   offsets already pointing behind it; the sample bytes follow.
//!
//! Durations are in each track's timescale; there are no B-frames, so
//! composition time = decode time. The audio track's edit list skips the
//! encoder's priming samples, so audio and video start together.

use alloc::vec::Vec;

/// The video track: picture size, timescale and the `avcC` record.
#[derive(Clone, Debug)]
pub struct VideoTrack {
    pub width: u32,
    pub height: u32,
    /// ticks per second (e.g. 90000)
    pub timescale: u32,
    /// AVCDecoderConfigurationRecord
    pub avcc: Vec<u8>,
    /// BT.709 limited range `colr` box
    pub bt709: bool,
}

/// The audio track: AAC (`mp4a`), timescale = sample rate.
#[derive(Clone, Debug)]
pub struct AudioTrack {
    pub sample_rate: u32,
    pub channels: u16,
    /// AudioSpecificConfig
    pub asc: Vec<u8>,
    /// average bit rate (bit/s) for the `esds`
    pub bitrate: u32,
    /// encoder delay to skip (samples), e.g. 1024 for AAC
    pub priming: u32,
}

/// One sample (an access unit): its size, duration and whether it is a keyframe.
#[derive(Clone, Copy, Debug, Default)]
pub struct SampleInfo {
    pub size: u32,
    pub duration: u32,
    pub key: bool,
}

/// An audio track's samples and chunks for [`moov_av`] / [`layout_av`].
#[derive(Clone, Copy)]
pub struct AudioPart<'a> {
    pub track: &'a AudioTrack,
    pub samples: &'a [SampleInfo],
    /// (offset, samples) per chunk
    pub chunks: &'a [(u64, u32)],
}

fn put32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_be_bytes());
}
fn put16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_be_bytes());
}
fn put64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_be_bytes());
}

/// A box: size + type + body.
fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + body.len());
    put32(&mut v, 8 + body.len() as u32);
    v.extend_from_slice(kind);
    v.extend_from_slice(body);
    v
}

/// A full box: size + type + version + flags + body.
fn fbx(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + body.len());
    put32(&mut b, ((version as u32) << 24) | (flags & 0xFF_FFFF));
    b.extend_from_slice(body);
    bx(kind, &b)
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];
const MOVIE_TIMESCALE: u32 = 1000;
const VIDEO_ID: u32 = 1;
const AUDIO_ID: u32 = 2;

/// The `ftyp` box.
pub fn ftyp() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"isom");
    put32(&mut b, 0x200);
    for c in [b"isom", b"iso2", b"avc1", b"mp41", b"iso6"] {
        b.extend_from_slice(c);
    }
    bx(b"ftyp", &b)
}

fn mvhd(duration_ms: u64, next_id: u32) -> Vec<u8> {
    let mut b = Vec::new();
    put32(&mut b, 0);
    put32(&mut b, 0);
    put32(&mut b, MOVIE_TIMESCALE);
    put32(&mut b, duration_ms.min(u32::MAX as u64) as u32);
    put32(&mut b, 0x0001_0000); // rate 1.0
    put16(&mut b, 0x0100); // volume 1.0
    b.extend_from_slice(&[0; 10]);
    for m in MATRIX {
        put32(&mut b, m);
    }
    b.extend_from_slice(&[0; 24]);
    put32(&mut b, next_id);
    fbx(b"mvhd", 0, 0, &b)
}

fn tkhd(id: u32, w: u32, h: u32, volume: u16, duration_ms: u64) -> Vec<u8> {
    let mut b = Vec::new();
    put32(&mut b, 0);
    put32(&mut b, 0);
    put32(&mut b, id);
    put32(&mut b, 0);
    put32(&mut b, duration_ms.min(u32::MAX as u64) as u32);
    b.extend_from_slice(&[0; 8]);
    put16(&mut b, 0); // layer
    put16(&mut b, if volume > 0 { 1 } else { 0 }); // alternate_group
    put16(&mut b, volume);
    put16(&mut b, 0);
    for m in MATRIX {
        put32(&mut b, m);
    }
    put32(&mut b, w << 16);
    put32(&mut b, h << 16);
    fbx(b"tkhd", 0, 3, &b) // enabled | in movie
}

fn mdhd(timescale: u32, duration: u64) -> Vec<u8> {
    let mut b = Vec::new();
    let long = duration > u32::MAX as u64;
    if long {
        put64(&mut b, 0);
        put64(&mut b, 0);
        put32(&mut b, timescale);
        put64(&mut b, duration);
    } else {
        put32(&mut b, 0);
        put32(&mut b, 0);
        put32(&mut b, timescale);
        put32(&mut b, duration as u32);
    }
    put16(&mut b, 0x55C4); // "und"
    put16(&mut b, 0);
    fbx(b"mdhd", long as u8, 0, &b)
}

fn hdlr(kind: &[u8; 4], name: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    put32(&mut b, 0);
    b.extend_from_slice(kind);
    b.extend_from_slice(&[0; 12]);
    b.extend_from_slice(name);
    b.push(0);
    fbx(b"hdlr", 0, 0, &b)
}

fn dinf() -> Vec<u8> {
    let url = fbx(b"url ", 0, 1, &[]);
    let mut d = Vec::new();
    put32(&mut d, 1);
    d.extend_from_slice(&url);
    bx(b"dinf", &fbx(b"dref", 0, 0, &d))
}

fn stsd_video(t: &VideoTrack) -> Vec<u8> {
    let mut e = Vec::new();
    e.extend_from_slice(&[0; 6]);
    put16(&mut e, 1); // data_reference_index
    e.extend_from_slice(&[0; 16]);
    put16(&mut e, t.width as u16);
    put16(&mut e, t.height as u16);
    put32(&mut e, 0x0048_0000);
    put32(&mut e, 0x0048_0000);
    put32(&mut e, 0);
    put16(&mut e, 1); // frame_count
    let mut name = [0u8; 32];
    let n = b"LingOS H.264";
    name[0] = n.len() as u8;
    name[1..1 + n.len()].copy_from_slice(n);
    e.extend_from_slice(&name);
    put16(&mut e, 0x18);
    put16(&mut e, 0xFFFF);
    e.extend_from_slice(&bx(b"avcC", &t.avcc));
    if t.bt709 {
        let mut c = Vec::new();
        c.extend_from_slice(b"nclx");
        put16(&mut c, 1);
        put16(&mut c, 1);
        put16(&mut c, 1);
        c.push(0); // limited range
        e.extend_from_slice(&bx(b"colr", &c));
    }
    let mut p = Vec::new();
    put32(&mut p, 1);
    put32(&mut p, 1);
    e.extend_from_slice(&bx(b"pasp", &p));
    let mut b = Vec::new();
    put32(&mut b, 1);
    b.extend_from_slice(&bx(b"avc1", &e));
    fbx(b"stsd", 0, 0, &b)
}

/// An MPEG-4 descriptor: tag + 4-byte size + body.
fn desc(tag: u8, body: &[u8]) -> Vec<u8> {
    let n = body.len() as u32;
    let mut v = alloc::vec![tag, 0x80 | (n >> 21) as u8 & 0x7F, 0x80 | (n >> 14) as u8 & 0x7F, 0x80 | (n >> 7) as u8 & 0x7F, n as u8 & 0x7F];
    v.extend_from_slice(body);
    v
}

fn stsd_audio(t: &AudioTrack) -> Vec<u8> {
    let mut e = Vec::new();
    e.extend_from_slice(&[0; 6]);
    put16(&mut e, 1); // data_reference_index
    e.extend_from_slice(&[0; 8]);
    put16(&mut e, t.channels);
    put16(&mut e, 16); // sample size
    put16(&mut e, 0);
    put16(&mut e, 0);
    put32(&mut e, (t.sample_rate.min(65535)) << 16);
    // esds: ES_Descriptor { DecoderConfigDescriptor { DecoderSpecificInfo }, SLConfigDescriptor }
    let mut dcd = Vec::new();
    dcd.push(0x40); // MPEG-4 audio
    dcd.push(0x15); // audio stream
    dcd.extend_from_slice(&[0, 0x18, 0]); // buffer size: 6144 bytes
    put32(&mut dcd, t.bitrate);
    put32(&mut dcd, t.bitrate);
    dcd.extend_from_slice(&desc(5, &t.asc));
    let mut es = Vec::new();
    put16(&mut es, AUDIO_ID as u16); // ES_ID
    es.push(0); // flags
    es.extend_from_slice(&desc(4, &dcd));
    es.extend_from_slice(&desc(6, &[2]));
    e.extend_from_slice(&fbx(b"esds", 0, 0, &desc(3, &es)));
    let mut b = Vec::new();
    put32(&mut b, 1);
    b.extend_from_slice(&bx(b"mp4a", &e));
    fbx(b"stsd", 0, 0, &b)
}

/// stts, stss (when `sync` and not all are keyframes), stsc, stsz, stco / co64.
fn tables(samples: &[SampleInfo], chunks: &[(u64, u32)], wide: bool, sync: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut stts = Vec::new();
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for s in samples {
        match runs.last_mut() {
            Some(r) if r.1 == s.duration => r.0 += 1,
            _ => runs.push((1, s.duration)),
        }
    }
    put32(&mut stts, runs.len() as u32);
    for (n, d) in &runs {
        put32(&mut stts, *n);
        put32(&mut stts, *d);
    }
    out.extend_from_slice(&fbx(b"stts", 0, 0, &stts));
    let keys: Vec<u32> = samples.iter().enumerate().filter(|(_, s)| s.key).map(|(i, _)| i as u32 + 1).collect();
    if sync && keys.len() < samples.len() {
        let mut stss = Vec::new();
        put32(&mut stss, keys.len() as u32);
        for k in &keys {
            put32(&mut stss, *k);
        }
        out.extend_from_slice(&fbx(b"stss", 0, 0, &stss));
    }
    let mut stsc = Vec::new();
    let mut sc: Vec<(u32, u32)> = Vec::new();
    for (i, &(_, n)) in chunks.iter().enumerate() {
        if sc.last().map(|r| r.1) != Some(n) {
            sc.push((i as u32 + 1, n));
        }
    }
    put32(&mut stsc, sc.len() as u32);
    for (first, n) in &sc {
        put32(&mut stsc, *first);
        put32(&mut stsc, *n);
        put32(&mut stsc, 1);
    }
    out.extend_from_slice(&fbx(b"stsc", 0, 0, &stsc));
    let mut stsz = Vec::new();
    put32(&mut stsz, 0);
    put32(&mut stsz, samples.len() as u32);
    for s in samples {
        put32(&mut stsz, s.size);
    }
    out.extend_from_slice(&fbx(b"stsz", 0, 0, &stsz));
    let mut stco = Vec::new();
    put32(&mut stco, chunks.len() as u32);
    for c in chunks {
        if wide {
            put64(&mut stco, c.0);
        } else {
            put32(&mut stco, c.0 as u32);
        }
    }
    out.extend_from_slice(&fbx(if wide { b"co64" } else { b"stco" }, 0, 0, &stco));
    out
}

fn empty_tables() -> Vec<u8> {
    let mut v = Vec::new();
    for k in [b"stts", b"stsc", b"stco"] {
        v.extend_from_slice(&fbx(k, 0, 0, &[0, 0, 0, 0]));
    }
    v.extend_from_slice(&fbx(b"stsz", 0, 0, &[0; 8]));
    v
}

fn ms(duration: u64, timescale: u32) -> u64 {
    duration * MOVIE_TIMESCALE as u64 / timescale.max(1) as u64
}

fn video_trak(t: &VideoTrack, tables: &[u8], duration: u64) -> Vec<u8> {
    let mut stbl = stsd_video(t);
    stbl.extend_from_slice(tables);
    let mut minf = fbx(b"vmhd", 0, 1, &[0; 8]);
    minf.extend_from_slice(&dinf());
    minf.extend_from_slice(&bx(b"stbl", &stbl));
    let mut mdia = mdhd(t.timescale, duration);
    mdia.extend_from_slice(&hdlr(b"vide", b"VideoHandler"));
    mdia.extend_from_slice(&bx(b"minf", &minf));
    let mut tr = tkhd(VIDEO_ID, t.width, t.height, 0, ms(duration, t.timescale));
    tr.extend_from_slice(&bx(b"mdia", &mdia));
    bx(b"trak", &tr)
}

/// The audio `trak`; `duration` = all coded samples (priming included).
fn audio_trak(t: &AudioTrack, tables: &[u8], duration: u64) -> Vec<u8> {
    let mut stbl = stsd_audio(t);
    stbl.extend_from_slice(tables);
    let mut minf = fbx(b"smhd", 0, 0, &[0; 4]);
    minf.extend_from_slice(&dinf());
    minf.extend_from_slice(&bx(b"stbl", &stbl));
    let mut mdia = mdhd(t.sample_rate, duration);
    mdia.extend_from_slice(&hdlr(b"soun", b"SoundHandler"));
    mdia.extend_from_slice(&bx(b"minf", &minf));
    let shown = duration.saturating_sub(t.priming as u64);
    let mut tr = tkhd(AUDIO_ID, 0, 0, 0x0100, ms(shown, t.sample_rate));
    // edit list: play from the first real sample (skip the priming)
    let mut elst = Vec::new();
    put32(&mut elst, 1);
    put32(&mut elst, ms(shown, t.sample_rate) as u32);
    put32(&mut elst, t.priming);
    put32(&mut elst, 0x0001_0000);
    tr.extend_from_slice(&bx(b"edts", &fbx(b"elst", 0, 0, &elst)));
    tr.extend_from_slice(&bx(b"mdia", &mdia));
    bx(b"trak", &tr)
}

/// `moov` for a fragmented file: empty sample tables plus `mvex`.
pub fn moov_fragmented_av(v: &VideoTrack, a: Option<&AudioTrack>) -> Vec<u8> {
    let mut moov = mvhd(0, if a.is_some() { 3 } else { 2 });
    moov.extend_from_slice(&video_trak(v, &empty_tables(), 0));
    if let Some(a) = a {
        moov.extend_from_slice(&audio_trak(a, &empty_tables(), 0));
    }
    let mut mvex = Vec::new();
    for id in if a.is_some() { &[VIDEO_ID, AUDIO_ID][..] } else { &[VIDEO_ID][..] } {
        let mut trex = Vec::new();
        put32(&mut trex, *id);
        put32(&mut trex, 1); // default_sample_description_index
        put32(&mut trex, 0);
        put32(&mut trex, 0);
        put32(&mut trex, 0);
        mvex.extend_from_slice(&fbx(b"trex", 0, 0, &trex));
    }
    moov.extend_from_slice(&bx(b"mvex", &mvex));
    bx(b"moov", &moov)
}

/// `moov` for a fragmented video-only file.
pub fn moov_fragmented(t: &VideoTrack) -> Vec<u8> {
    moov_fragmented_av(t, None)
}

fn sample_flags(key: bool) -> u32 {
    if key {
        0x0200_0000 // depends on nothing
    } else {
        0x0101_0000 // depends on others, not a sync sample
    }
}

/// `moof` + the `mdat` header of one fragment with a run per track:
/// (track id: 1 video / 2 audio, first decode time, samples). Write these,
/// then each run's sample bytes in the same order.
pub fn fragment_header_av(seq: u32, runs: &[(u32, u64, &[SampleInfo])]) -> Vec<u8> {
    let traf_len = |n: usize| 8 + (8 + 4 + 4) + (8 + 4 + 8) + (8 + 4 + 4 + 4 + 12 * n);
    let moof_len = 8 + (8 + 4 + 4) + runs.iter().map(|r| traf_len(r.2.len())).sum::<usize>();
    let mut mfhd = Vec::new();
    put32(&mut mfhd, seq);
    let mut moof = fbx(b"mfhd", 0, 0, &mfhd);
    let mut data_off = moof_len as u64 + 8;
    let mut data = 0u64;
    for &(id, base, samples) in runs {
        let mut tfhd = Vec::new();
        put32(&mut tfhd, id);
        let mut tfdt = Vec::new();
        put64(&mut tfdt, base);
        let mut trun = Vec::new();
        put32(&mut trun, samples.len() as u32);
        put32(&mut trun, data_off as u32);
        for s in samples {
            put32(&mut trun, s.duration);
            put32(&mut trun, s.size);
            put32(&mut trun, sample_flags(s.key));
        }
        let mut traf = fbx(b"tfhd", 0, 0x02_0000, &tfhd); // default-base-is-moof
        traf.extend_from_slice(&fbx(b"tfdt", 1, 0, &tfdt));
        traf.extend_from_slice(&fbx(b"trun", 0, 0x701, &trun));
        moof.extend_from_slice(&bx(b"traf", &traf));
        let n: u64 = samples.iter().map(|s| s.size as u64).sum();
        data_off += n;
        data += n;
    }
    let mut v = bx(b"moof", &moof);
    debug_assert_eq!(v.len(), moof_len);
    v.extend_from_slice(&mdat_header(data));
    v
}

/// Video-only [`fragment_header_av`].
pub fn fragment_header(seq: u32, base_time: u64, samples: &[SampleInfo]) -> Vec<u8> {
    fragment_header_av(seq, &[(VIDEO_ID, base_time, samples)])
}

/// The `mdat` header for `len` bytes of sample data (8 bytes, or 16 above 4 GiB).
pub fn mdat_header(len: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(16);
    if len + 8 > u32::MAX as u64 {
        put32(&mut v, 1);
        v.extend_from_slice(b"mdat");
        put64(&mut v, len + 16);
    } else {
        put32(&mut v, len as u32 + 8);
        v.extend_from_slice(b"mdat");
    }
    v
}

fn moov_impl(t: &VideoTrack, samples: &[SampleInfo], chunks: &[(u64, u32)], audio: Option<AudioPart>, wide: bool) -> Vec<u8> {
    let vdur: u64 = samples.iter().map(|s| s.duration as u64).sum();
    let mut total_ms = ms(vdur, t.timescale);
    let mut tracks = video_trak(t, &tables(samples, chunks, wide, true), vdur);
    if let Some(a) = audio {
        let adur: u64 = a.samples.iter().map(|s| s.duration as u64).sum();
        total_ms = total_ms.max(ms(adur.saturating_sub(a.track.priming as u64), a.track.sample_rate));
        tracks.extend_from_slice(&audio_trak(a.track, &tables(a.samples, a.chunks, wide, false), adur));
    }
    let mut moov = mvhd(total_ms, if audio.is_some() { 3 } else { 2 });
    moov.extend_from_slice(&tracks);
    bx(b"moov", &moov)
}

/// The `moov` of a regular video-only file. `chunks` = (offset in the file,
/// samples) of each run of consecutive samples stored together, in order.
pub fn moov(t: &VideoTrack, samples: &[SampleInfo], chunks: &[(u64, u32)]) -> Vec<u8> {
    moov_impl(t, samples, chunks, None, chunks.iter().any(|c| c.0 > u32::MAX as u64))
}

/// Header of a regular "fast start" file whose sample data follows it in one
/// `mdat`: returns `ftyp + moov + mdat header`. Chunk offsets (video and
/// audio) are offsets within that sample data; they are rebased here.
pub fn layout_av(t: &VideoTrack, samples: &[SampleInfo], chunks: &[(u64, u32)], audio: Option<AudioPart>) -> Vec<u8> {
    let mut data: u64 = samples.iter().map(|s| s.size as u64).sum();
    if let Some(a) = audio {
        data += a.samples.iter().map(|s| s.size as u64).sum::<u64>();
    }
    let mut v = ftyp();
    let mh = mdat_header(data);
    // 64-bit chunk offsets when the file may pass 4 GiB (the header itself is far below 64 MiB)
    let wide = data + (64 << 20) > u32::MAX as u64;
    let mlen = moov_impl(t, samples, chunks, audio, wide).len() as u64;
    let base = v.len() as u64 + mlen + mh.len() as u64;
    let vr: Vec<(u64, u32)> = chunks.iter().map(|&(o, n)| (o + base, n)).collect();
    let ar: Vec<(u64, u32)> = audio.map(|a| a.chunks.iter().map(|&(o, n)| (o + base, n)).collect()).unwrap_or_default();
    let audio2 = audio.map(|a| AudioPart { track: a.track, samples: a.samples, chunks: &ar });
    v.extend_from_slice(&moov_impl(t, samples, &vr, audio2, wide));
    v.extend_from_slice(&mh);
    v
}

/// Video-only [`layout_av`].
pub fn layout(t: &VideoTrack, samples: &[SampleInfo], chunks: &[(u64, u32)]) -> Vec<u8> {
    layout_av(t, samples, chunks, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{scan_boxes, Codec, Movie};
    use alloc::vec;

    fn track() -> VideoTrack {
        VideoTrack { width: 1920, height: 1080, timescale: 90000, avcc: vec![1, 66, 0xC0, 41, 0xFF, 0xE1, 0, 0, 1, 0, 0], bt709: true }
    }

    fn samples() -> (Vec<SampleInfo>, Vec<u8>) {
        let s = vec![
            SampleInfo { size: 5, duration: 3000, key: true },
            SampleInfo { size: 3, duration: 6000, key: false },
            SampleInfo { size: 4, duration: 3000, key: false },
            SampleInfo { size: 2, duration: 3000, key: true },
            SampleInfo { size: 6, duration: 3000, key: false },
        ];
        let data: Vec<u8> = (0..20u8).collect();
        (s, data)
    }

    #[test]
    fn regular_file_round_trips() {
        let (s, data) = samples();
        let chunks = [(0u64, 3u32), (12, 2)];
        let mut file = layout(&track(), &s, &chunks);
        file.extend_from_slice(&data);
        let movie = Movie::parse_init(&file).unwrap();
        assert!(!movie.fragmented);
        let t = &movie.tracks[0];
        assert!(matches!(&t.codec, Codec::Avc { avcc, .. } if avcc == &track().avcc));
        assert_eq!((t.width, t.height, t.timescale, t.duration), (1920, 1080, 90000, 18000));
        let got = t.sample_table();
        assert_eq!(got.len(), 5);
        let mut dts = 0;
        let mut at = 0usize;
        for (g, w) in got.iter().zip(&s) {
            assert_eq!((g.size, g.duration, g.keyframe, g.dts), (w.size, w.duration, w.key, dts));
            assert_eq!(&file[g.offset as usize..g.offset as usize + g.size as usize], &data[at..at + w.size as usize]);
            dts += w.duration as u64;
            at += w.size as usize;
        }
        // moov before mdat ("fast start")
        let boxes = scan_boxes(&file, 0).unwrap();
        let kinds: Vec<[u8; 4]> = boxes.iter().map(|b| b.kind).collect();
        assert_eq!(kinds, vec![*b"ftyp", *b"moov", *b"mdat"]);
    }

    #[test]
    fn fragmented_file_round_trips() {
        let (s, data) = samples();
        let mut file = ftyp();
        file.extend_from_slice(&moov_fragmented(&track()));
        let mut frag_at = vec![];
        let (mut t, mut off) = (0u64, 0usize);
        for (seq, range) in [(1u32, 0..3usize), (2, 3..5)] {
            frag_at.push(file.len());
            file.extend_from_slice(&fragment_header(seq, t, &s[range.clone()]));
            let n: usize = s[range.clone()].iter().map(|x| x.size as usize).sum();
            file.extend_from_slice(&data[off..off + n]);
            off += n;
            t += s[range].iter().map(|x| x.duration as u64).sum::<u64>();
        }
        let mut movie = Movie::parse_init(&file).unwrap();
        assert!(movie.fragmented);
        let mut i = 0;
        let mut at = 0usize;
        let mut dts = 0u64;
        for &fa in &frag_at {
            let f = movie.parse_fragment(&file[fa..], fa as u64).unwrap();
            for g in f.track_samples(1) {
                let w = &s[i];
                assert_eq!((g.size, g.duration, g.keyframe, g.dts), (w.size, w.duration, w.key, dts));
                let o = fa + g.offset as usize;
                assert_eq!(&file[o..o + g.size as usize], &data[at..at + w.size as usize]);
                at += w.size as usize;
                dts += w.duration as u64;
                i += 1;
            }
        }
        assert_eq!(i, 5);
    }

    #[test]
    fn audio_and_video_round_trip() {
        let (s, data) = samples();
        let at = AudioTrack { sample_rate: 48000, channels: 2, asc: vec![0x11, 0x90], bitrate: 160_000, priming: 1024 };
        let a = [SampleInfo { size: 7, duration: 1024, key: true }, SampleInfo { size: 9, duration: 1024, key: true }, SampleInfo { size: 4, duration: 1024, key: true }];
        let adata: Vec<u8> = (100..120u8).collect();
        // data stream: video chunk 1 (3 samples, 12 bytes), audio chunk (3 samples, 20), video chunk 2 (2 samples, 8)
        let vchunks = [(0u64, 3u32), (32, 2)];
        let achunks = [(12u64, 3u32)];
        let mut file = layout_av(&track(), &s, &vchunks, Some(AudioPart { track: &at, samples: &a, chunks: &achunks }));
        file.extend_from_slice(&data[..12]);
        file.extend_from_slice(&adata);
        file.extend_from_slice(&data[12..]);
        let movie = Movie::parse_init(&file).unwrap();
        assert_eq!(movie.tracks.len(), 2);
        let au = movie.tracks.iter().find(|t| t.id == 2).unwrap();
        assert!(matches!(&au.codec, Codec::Mp4a { asc, .. } if asc == &vec![0x11, 0x90]));
        assert_eq!((au.timescale, au.channels, au.edit_media_time), (48000, 2, Some(1024)));
        let mut at_ = 0;
        for (g, w) in au.sample_table().iter().zip(&a) {
            assert_eq!(&file[g.offset as usize..g.offset as usize + g.size as usize], &adata[at_..at_ + w.size as usize]);
            at_ += w.size as usize;
        }
        let vt = movie.tracks.iter().find(|t| t.id == 1).unwrap();
        let mut at_ = 0;
        for (g, w) in vt.sample_table().iter().zip(&s) {
            assert_eq!(&file[g.offset as usize..g.offset as usize + g.size as usize], &data[at_..at_ + w.size as usize]);
            at_ += w.size as usize;
        }
    }
}
