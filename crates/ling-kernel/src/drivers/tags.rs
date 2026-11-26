//! Audio file tags for the music library (apps/lingmusic): title, artist, album, album
//! artist, track / disc number, year, genre, duration and embedded cover art, read from the
//! file's header and tail only -- a library scan must not decode whole songs.
//!
//! Formats (what the kernel plays: codec/mod.rs + sfx): MP3 (ID3v2.2/2.3/2.4 frames, ID3v1
//! at the end; duration from the Xing/Info frame count, else the bitrate), FLAC (STREAMINFO,
//! VORBIS_COMMENT, PICTURE), Ogg Vorbis (identification + comment headers, duration from the
//! last page's granule position), WAV (fmt / data sizes, LIST INFO, an embedded "id3 " chunk).
//!
//! `os_audio_tags(path, cover_out)` (lingapp.rs) returns the fields as a list and, when the
//! file carries a picture, writes it to `cover_out` as a small 24-bit BMP thumbnail that
//! `px_image` draws from its cache.

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Default)]
pub struct Tags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track: u32,
    pub disc: u32,
    pub year: String,
    pub genre: String,
    pub duration: f64,
    pub cover: Option<Vec<u8>>,
}

/// How much of a file's head is read: tags and a front cover sit there (a big embedded
/// picture past it is skipped, the rest of the tags still read).
const HEAD: usize = 1 << 20;
const TAIL: usize = 64 * 1024;

/// (head bytes, tail bytes, file size): through a range read on media/* volumes, else the
/// whole file (home folders on lingfs are small).
fn read_parts(path: &str) -> Option<(Vec<u8>, Vec<u8>, u64)> {
    let real = crate::vfs::resolve(path);
    if let Some(f) = crate::fs::media::open_range(&real) {
        let size = f.size();
        let hn = (size as usize).min(HEAD);
        let mut head = alloc::vec![0u8; hn];
        if !f.read_at(0, &mut head) {
            return None;
        }
        let tn = (size as usize).min(TAIL);
        let mut tail = alloc::vec![0u8; tn];
        if !f.read_at(size - tn as u64, &mut tail) {
            tail.clear();
        }
        return Some((head, tail, size));
    }
    let all = crate::vfs::read(path)?;
    let size = all.len() as u64;
    let tail = all[all.len().saturating_sub(TAIL)..].to_vec();
    let mut head = all;
    head.truncate(HEAD);
    Some((head, tail, size))
}

fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
fn le32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
fn get(b: &[u8], from: usize, len: usize) -> Option<&[u8]> {
    b.get(from..from.checked_add(len)?)
}

pub fn read(path: &str) -> Option<Tags> {
    let (head, tail, size) = read_parts(path)?;
    let mut t = Tags::default();
    match crate::codec::sniff(&head) {
        Some("flac") => flac(&head, &mut t),
        Some("ogg") => ogg(&head, &tail, &mut t),
        Some("wav") => wav(&head, size, &mut t),
        Some("mp3") => mp3(&head, &tail, size, &mut t),
        _ => return None,
    }
    // a file without a title tag shows its name
    if t.title.trim().is_empty() {
        let base = crate::vfs::basename(path);
        t.title = String::from(base.rsplit_once('.').map(|(a, _)| a).unwrap_or(&base));
    }
    for s in [&mut t.title, &mut t.artist, &mut t.album, &mut t.album_artist, &mut t.genre, &mut t.year] {
        *s = String::from(s.trim_matches(|c: char| c == '\0' || c.is_whitespace()));
    }
    Some(t)
}

// ── text decoding ────────────────────────────────────────────────────────────

fn latin1(b: &[u8]) -> String {
    b.iter().take_while(|&&c| c != 0).map(|&c| c as char).collect()
}

fn utf16(b: &[u8], big: bool) -> String {
    let units: Vec<u16> = b.chunks_exact(2).map(|c| if big { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }).take_while(|&u| u != 0).collect();
    char::decode_utf16(units).map(|r| r.unwrap_or('\u{FFFD}')).collect()
}

/// An ID3 text field: its encoding byte, then the text.
fn id3_text(b: &[u8]) -> String {
    let Some((&enc, rest)) = b.split_first() else { return String::new() };
    match enc {
        1 => {
            if rest.len() >= 2 && rest[0] == 0xFE && rest[1] == 0xFF {
                utf16(&rest[2..], true)
            } else if rest.len() >= 2 && rest[0] == 0xFF && rest[1] == 0xFE {
                utf16(&rest[2..], false)
            } else {
                utf16(rest, false)
            }
        },
        2 => utf16(rest, true),
        3 => String::from_utf8_lossy(rest.split(|&c| c == 0).next().unwrap_or(&[])).into_owned(),
        _ => latin1(rest),
    }
}

/// "3/12" -> 3
fn first_num(s: &str) -> u32 {
    s.trim().split(|c: char| !c.is_ascii_digit()).next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// ID3v1 genre numbers (the common first 80), for "(17)" style TCON values.
const GENRES: [&str; 80] = [
    "Blues", "Classic Rock", "Country", "Dance", "Disco", "Funk", "Grunge", "Hip-Hop", "Jazz", "Metal", "New Age", "Oldies", "Other", "Pop", "R&B", "Rap",
    "Reggae", "Rock", "Techno", "Industrial", "Alternative", "Ska", "Death Metal", "Pranks", "Soundtrack", "Euro-Techno", "Ambient", "Trip-Hop", "Vocal",
    "Jazz+Funk", "Fusion", "Trance", "Classical", "Instrumental", "Acid", "House", "Game", "Sound Clip", "Gospel", "Noise", "AlternRock", "Bass", "Soul", "Punk",
    "Space", "Meditative", "Instrumental Pop", "Instrumental Rock", "Ethnic", "Gothic", "Darkwave", "Techno-Industrial", "Electronic", "Pop-Folk", "Eurodance",
    "Dream", "Southern Rock", "Comedy", "Cult", "Gangsta", "Top 40", "Christian Rap", "Pop/Funk", "Jungle", "Native American", "Cabaret", "New Wave",
    "Psychadelic", "Rave", "Showtunes", "Trailer", "Lo-Fi", "Tribal", "Acid Punk", "Acid Jazz", "Polka", "Retro", "Musical", "Rock & Roll", "Hard Rock",
];

fn genre_name(s: &str) -> String {
    let t = s.trim();
    // "(17)", "(17)Rock", "17"
    let num = t.trim_start_matches('(').split(')').next().unwrap_or("");
    if !num.is_empty() && num.bytes().all(|c| c.is_ascii_digit()) {
        let rest = t.split(')').nth(1).unwrap_or("").trim();
        if !rest.is_empty() {
            return String::from(rest);
        }
        if let Some(g) = num.parse::<usize>().ok().and_then(|i| GENRES.get(i)) {
            return String::from(*g);
        }
    }
    String::from(t)
}

// ── ID3 ──────────────────────────────────────────────────────────────────────

fn syncsafe(b: &[u8], i: usize) -> usize {
    ((b[i] as usize & 0x7F) << 21) | ((b[i + 1] as usize & 0x7F) << 14) | ((b[i + 2] as usize & 0x7F) << 7) | (b[i + 3] as usize & 0x7F)
}

/// An ID3v2 tag at the start of `b`; returns its total length (0 if none).
fn id3v2(b: &[u8], t: &mut Tags) -> usize {
    if b.len() < 10 || &b[..3] != b"ID3" {
        return 0;
    }
    let ver = b[3];
    let flags = b[5];
    let size = syncsafe(b, 6);
    let total = 10 + size + if flags & 0x10 != 0 { 10 } else { 0 };
    let end = (10 + size).min(b.len());
    let mut p = 10;
    if flags & 0x40 != 0 && ver >= 3 && p + 4 <= end {
        // extended header
        let ext = if ver == 4 { syncsafe(b, p) } else { be32(b, p) as usize + 4 };
        p += ext;
    }
    let (idlen, hdr) = if ver == 2 { (3, 6) } else { (4, 10) };
    while p + hdr <= end {
        let id = &b[p..p + idlen];
        if id[0] == 0 {
            break; // padding
        }
        let fsize = if ver == 2 {
            ((b[p + 3] as usize) << 16) | ((b[p + 4] as usize) << 8) | b[p + 5] as usize
        } else if ver == 4 {
            syncsafe(b, p + 4)
        } else {
            be32(b, p + 4) as usize
        };
        let body_at = p + hdr;
        let Some(body) = get(b, body_at, fsize) else { break };
        let s = || id3_text(body);
        match id {
            b"TIT2" | b"TT2" => t.title = s(),
            b"TPE1" | b"TP1" => t.artist = s(),
            b"TALB" | b"TAL" => t.album = s(),
            b"TPE2" | b"TP2" => t.album_artist = s(),
            b"TRCK" | b"TRK" => t.track = first_num(&s()),
            b"TPOS" | b"TPA" => t.disc = first_num(&s()),
            b"TYER" | b"TYE" | b"TDRC" | b"TDOR" => {
                if t.year.is_empty() {
                    t.year = s().chars().take(4).collect();
                }
            },
            b"TCON" | b"TCO" => t.genre = genre_name(&s()),
            b"APIC" if t.cover.is_none() => {
                // enc, mime\0, type, description (in enc)\0, data
                let enc = body.first().copied().unwrap_or(0);
                let mut q = 1;
                while q < body.len() && body[q] != 0 {
                    q += 1;
                }
                q += 1; // mime's 0
                q += 1; // picture type
                q = skip_text(body, q, enc);
                if q < body.len() {
                    t.cover = Some(body[q..].to_vec());
                }
            },
            b"PIC" if t.cover.is_none() => {
                // enc, 3-byte format, type, description\0, data
                let enc = body.first().copied().unwrap_or(0);
                let q = skip_text(body, 5, enc);
                if q < body.len() {
                    t.cover = Some(body[q..].to_vec());
                }
            },
            _ => {},
        }
        p = body_at + fsize;
    }
    total
}

/// Past a 0-terminated string in ID3 encoding `enc` (UTF-16 ends with two zero bytes).
fn skip_text(b: &[u8], mut q: usize, enc: u8) -> usize {
    if enc == 1 || enc == 2 {
        while q + 1 < b.len() && !(b[q] == 0 && b[q + 1] == 0) {
            q += 2;
        }
        q + 2
    } else {
        while q < b.len() && b[q] != 0 {
            q += 1;
        }
        q + 1
    }
}

/// ID3v1 in the last 128 bytes: fills only what ID3v2 left empty.
fn id3v1(tail: &[u8], t: &mut Tags) {
    if tail.len() < 128 {
        return;
    }
    let b = &tail[tail.len() - 128..];
    if &b[..3] != b"TAG" {
        return;
    }
    let f = |from: usize, n: usize| String::from(latin1(&b[from..from + n]).trim());
    if t.title.is_empty() {
        t.title = f(3, 30);
    }
    if t.artist.is_empty() {
        t.artist = f(33, 30);
    }
    if t.album.is_empty() {
        t.album = f(63, 30);
    }
    if t.year.is_empty() {
        t.year = f(93, 4);
    }
    if t.track == 0 && b[125] == 0 && b[126] != 0 {
        t.track = b[126] as u32;
    }
    if t.genre.is_empty() {
        if let Some(g) = GENRES.get(b[127] as usize) {
            t.genre = String::from(*g);
        }
    }
}

// ── MP3 ──────────────────────────────────────────────────────────────────────

fn mp3(head: &[u8], tail: &[u8], size: u64, t: &mut Tags) {
    let tag_len = id3v2(head, t);
    id3v1(tail, t);
    // the first frame: its rate and samples per frame; a Xing / Info frame gives the count
    let mut p = tag_len.min(head.len());
    while p + 4 < head.len() && ling_mpa::frame_info(&head[p..]).is_none() {
        p += 1;
        if p > tag_len + 64 * 1024 {
            return;
        }
    }
    let Some((flen, rate, ch, spf)) = ling_mpa::frame_info(&head[p..]) else { return };
    if rate == 0 || spf == 0 {
        return;
    }
    let frame = &head[p..(p + flen).min(head.len())];
    // Xing sits after the side info: 4 + 32 (MPEG-1 stereo) / 17 / 9 bytes in; just search
    let xing = frame.windows(4).position(|w| w == b"Xing" || w == b"Info");
    if let Some(x) = xing {
        if x + 12 <= frame.len() && be32(frame, x + 4) & 1 != 0 {
            let frames = be32(frame, x + 8) as f64;
            t.duration = frames * spf as f64 / rate as f64;
            return;
        }
    }
    let _ = ch;
    // constant bitrate: (audio bytes / frame bytes) frames
    let audio = size.saturating_sub(p as u64).saturating_sub(if tail.len() >= 128 && &tail[tail.len() - 128..tail.len() - 125] == b"TAG" { 128 } else { 0 });
    t.duration = audio as f64 / flen.max(1) as f64 * spf as f64 / rate as f64;
}

// ── FLAC / Vorbis comments ───────────────────────────────────────────────────

fn vorbis_comments(b: &[u8], t: &mut Tags) {
    if b.len() < 8 {
        return;
    }
    let vlen = le32(b, 0) as usize;
    let mut p = 4 + vlen;
    let Some(nb) = get(b, p, 4) else { return };
    let n = le32(nb, 0) as usize;
    p += 4;
    for _ in 0..n.min(512) {
        let Some(lb) = get(b, p, 4) else { return };
        let len = le32(lb, 0) as usize;
        p += 4;
        let Some(c) = get(b, p, len) else { return };
        p += len;
        let s = String::from_utf8_lossy(c);
        let Some((k, v)) = s.split_once('=') else { continue };
        let v = String::from(v);
        match k.to_ascii_uppercase().as_str() {
            "TITLE" => t.title = v,
            "ARTIST" => {
                if t.artist.is_empty() {
                    t.artist = v
                }
            },
            "ALBUM" => t.album = v,
            "ALBUMARTIST" | "ALBUM ARTIST" | "ALBUM_ARTIST" => t.album_artist = v,
            "TRACKNUMBER" => t.track = first_num(&v),
            "DISCNUMBER" => t.disc = first_num(&v),
            "DATE" | "YEAR" => {
                if t.year.is_empty() {
                    t.year = v.chars().take(4).collect()
                }
            },
            "GENRE" => t.genre = v,
            "METADATA_BLOCK_PICTURE" if t.cover.is_none() => {
                if let Some(raw) = base64(&v) {
                    flac_picture(&raw, t);
                }
            },
            _ => {},
        }
    }
}

/// A FLAC PICTURE block: type, mime, description, w, h, depth, colours, data.
fn flac_picture(b: &[u8], t: &mut Tags) {
    let Some(ml) = get(b, 4, 4) else { return };
    let mut p = 8 + be32(ml, 0) as usize;
    let Some(dl) = get(b, p, 4) else { return };
    p += 4 + be32(dl, 0) as usize + 16;
    let Some(nl) = get(b, p, 4) else { return };
    let n = be32(nl, 0) as usize;
    if let Some(data) = get(b, p + 4, n) {
        t.cover = Some(data.to_vec());
    }
}

fn flac(b: &[u8], t: &mut Tags) {
    let mut p = 4;
    loop {
        let Some(h) = get(b, p, 4) else { return };
        let last = h[0] & 0x80 != 0;
        let kind = h[0] & 0x7F;
        let len = ((h[1] as usize) << 16) | ((h[2] as usize) << 8) | h[3] as usize;
        let body = get(b, p + 4, len);
        match (kind, body) {
            (0, Some(s)) if s.len() >= 18 => {
                // sample rate: 20 bits at byte 10; total samples: 36 bits from byte 13 (low 4 bits) on
                let rate = ((s[10] as u32) << 12) | ((s[11] as u32) << 4) | ((s[12] as u32) >> 4);
                let total = (((s[13] & 0x0F) as u64) << 32) | be32(s, 14) as u64;
                if rate > 0 {
                    t.duration = total as f64 / rate as f64;
                }
            },
            (4, Some(s)) => vorbis_comments(s, t),
            (6, Some(s)) if t.cover.is_none() => flac_picture(s, t),
            _ => {},
        }
        if last || body.is_none() {
            return;
        }
        p += 4 + len;
    }
}

// ── Ogg Vorbis ───────────────────────────────────────────────────────────────

/// The first `want` packets of the first logical stream (pages reassembled by lacing).
fn ogg_packets(b: &[u8], want: usize) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut p = 0;
    while p + 27 <= b.len() && out.len() < want {
        if &b[p..p + 4] != b"OggS" {
            return out;
        }
        let nseg = b[p + 26] as usize;
        let Some(segs) = get(b, p + 27, nseg) else { return out };
        let mut d = p + 27 + nseg;
        for &s in segs {
            let Some(chunk) = get(b, d, s as usize) else { return out };
            cur.extend_from_slice(chunk);
            d += s as usize;
            if s < 255 {
                out.push(core::mem::take(&mut cur));
                if out.len() >= want {
                    return out;
                }
            }
        }
        p = d;
    }
    out
}

fn ogg(head: &[u8], tail: &[u8], t: &mut Tags) {
    let pk = ogg_packets(head, 2);
    let mut rate = 0u32;
    if let Some(id) = pk.first() {
        if id.len() >= 16 && id[0] == 1 && &id[1..7] == b"vorbis" {
            rate = le32(id, 12);
        }
    }
    if let Some(c) = pk.get(1) {
        if c.len() > 7 && c[0] == 3 && &c[1..7] == b"vorbis" {
            vorbis_comments(&c[7..], t);
        }
    }
    // the last page's granule position is the stream's length in samples
    if rate > 0 {
        if let Some(i) = tail.windows(4).rposition(|w| w == b"OggS") {
            if let Some(g) = get(tail, i + 6, 8) {
                let gran = u64::from_le_bytes([g[0], g[1], g[2], g[3], g[4], g[5], g[6], g[7]]);
                if gran != u64::MAX {
                    t.duration = gran as f64 / rate as f64;
                }
            }
        }
    }
}

// ── WAV ──────────────────────────────────────────────────────────────────────

fn wav(b: &[u8], size: u64, t: &mut Tags) {
    let mut p = 12;
    let mut byte_rate = 0u32;
    while p + 8 <= b.len() {
        let id = &b[p..p + 4];
        let len = le32(b, p + 4) as usize;
        let body_at = p + 8;
        match id {
            b"fmt " if len >= 12 && body_at + 12 <= b.len() => byte_rate = le32(b, body_at + 8),
            b"data" => {
                // the data chunk may run past what was read: its size is in its header
                let n = (len as u64).min(size.saturating_sub(body_at as u64));
                if byte_rate > 0 {
                    t.duration = n as f64 / byte_rate as f64;
                }
            },
            b"LIST" if len >= 4 && get(b, body_at, 4) == Some(&b"INFO"[..]) => {
                let end = (body_at + len).min(b.len());
                let mut q = body_at + 4;
                while q + 8 <= end {
                    let sid = &b[q..q + 4];
                    let sl = le32(b, q + 4) as usize;
                    let Some(v) = get(b, q + 8, sl) else { break };
                    let v = String::from_utf8_lossy(v.split(|&c| c == 0).next().unwrap_or(&[])).into_owned();
                    match sid {
                        b"INAM" => t.title = v,
                        b"IART" => t.artist = v,
                        b"IPRD" => t.album = v,
                        b"IGNR" => t.genre = v,
                        b"ICRD" => t.year = v.chars().take(4).collect(),
                        b"ITRK" | b"IPRT" => t.track = first_num(&v),
                        _ => {},
                    }
                    q += 8 + sl + (sl & 1);
                }
            },
            b"id3 " | b"ID3 " => {
                if let Some(tag) = get(b, body_at, len) {
                    id3v2(tag, t);
                }
            },
            _ => {},
        }
        if id == b"data" {
            // the chunks after the audio are past the head we read, except for small files
            if body_at + len > b.len() {
                break;
            }
        }
        p = body_at + len + (len & 1);
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\r' | b'\n' | b' ' => continue,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The cover as a small BMP (longest side `side` px) written to `out`; false when it is
/// not a JPEG / PNG the kernel decodes.
pub fn save_cover_bmp(cover: &[u8], out: &str, side: u32) -> bool {
    let img = if cover.len() > 3 && cover[0] == 0xFF && cover[1] == 0xD8 {
        crate::drivers::image::decode_jpeg(cover, side as usize * 4)
    } else if cover.len() > 8 && &cover[1..4] == b"PNG" {
        crate::drivers::image::decode_png_rgba(cover)
    } else {
        None
    };
    let Some(img) = img else { return false };
    if img.w == 0 || img.h == 0 {
        return false;
    }
    let sc = side as f64 / img.w.max(img.h) as f64;
    let (w, h) = if sc < 1.0 { (((img.w as f64 * sc) as u32).max(1), ((img.h as f64 * sc) as u32).max(1)) } else { (img.w, img.h) };
    let small = if (w, h) == (img.w, img.h) { img } else { crate::drivers::image::resize_rgba(&img, w, h) };
    // 24-bit BMP, bottom-up rows padded to 4 bytes
    let row = ((w * 3 + 3) & !3) as usize;
    let data_len = row * h as usize;
    let mut f = Vec::with_capacity(54 + data_len);
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&((54 + data_len) as u32).to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&54u32.to_le_bytes());
    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&(w as i32).to_le_bytes());
    f.extend_from_slice(&(h as i32).to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&24u16.to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&(data_len as u32).to_le_bytes());
    f.extend_from_slice(&[0u8; 16]);
    for y in (0..h as usize).rev() {
        let start = f.len();
        for x in 0..w as usize {
            let c = small.px[y * w as usize + x];
            f.push((c & 0xFF) as u8);
            f.push(((c >> 8) & 0xFF) as u8);
            f.push(((c >> 16) & 0xFF) as u8);
        }
        while f.len() - start < row {
            f.push(0);
        }
    }
    crate::vfs::write(out, &f)
}
