//! Korean typed phonetically: the Revised Romanization of Korean, composed into Hangul
//! syllables as the word is typed.
//!
//! The whole word is re-read on every key, so a later letter can move a consonant into the
//! next syllable (`han` 한, `hang` 항, `hangu` 한구, `hangug` 한국). A word is split into
//! syllables of onset + vowel + optional coda; of the readings that use the fewest syllables,
//! the one that puts the fewest letters into codas wins (a consonant between two vowels starts
//! the next syllable, as Korean does). `'` or `-` forces a syllable break (`iss'eoyo` 있어요).
//! A consonant with no vowel to go with (yet) shows as a lone jamo.
use alloc::string::String;
use alloc::vec::Vec;

const CHO: &str = "ㄱㄲㄴㄷㄸㄹㅁㅂㅃㅅㅆㅇㅈㅉㅊㅋㅌㅍㅎ";

/// Onsets: romanization -> choseong index.
const ONSETS: &[(&str, u32)] = &[
    ("g", 0), ("kk", 1), ("n", 2), ("d", 3), ("tt", 4), ("r", 5), ("l", 5), ("m", 6), ("b", 7), ("pp", 8),
    ("s", 9), ("ss", 10), ("j", 12), ("jj", 13), ("ch", 14), ("k", 15), ("t", 16), ("p", 17), ("h", 18),
    // loose spellings people type for the same sounds
    ("f", 17), ("v", 7), ("z", 12), ("c", 14),
];

/// Vowels: romanization -> jungseong index.
const VOWELS: &[(&str, u32)] = &[
    ("a", 0), ("ae", 1), ("ya", 2), ("yae", 3), ("eo", 4), ("e", 5), ("yeo", 6), ("ye", 7), ("o", 8),
    ("wa", 9), ("wae", 10), ("oe", 11), ("yo", 12), ("u", 13), ("wo", 14), ("weo", 14), ("we", 15),
    ("wi", 16), ("yu", 17), ("eu", 18), ("ui", 19), ("i", 20),
];

/// Codas: romanization -> jongseong index (1-based; 0 = none).
const CODAS: &[(&str, u32)] = &[
    ("k", 1), ("g", 1), ("kk", 2), ("ks", 3), ("gs", 3), ("n", 4), ("nj", 5), ("nh", 6), ("t", 7), ("d", 7),
    ("l", 8), ("r", 8), ("lk", 9), ("lg", 9), ("lm", 10), ("lb", 11), ("lp", 11), ("ls", 12), ("lt", 13),
    ("lh", 15), ("m", 16), ("p", 17), ("b", 17), ("ps", 18), ("bs", 18), ("s", 19), ("ss", 20), ("ng", 21),
    ("j", 22), ("ch", 23), ("h", 27),
];

#[derive(Clone, Copy)]
enum Unit {
    /// a whole syllable (precomposed Hangul)
    Syl(u32),
    /// a consonant with no vowel: the choseong index, shown as a compatibility jamo
    Lone(u32),
}

fn starts(s: &[u8], at: usize, pat: &str) -> bool {
    s.len() >= at + pat.len() && &s[at..at + pat.len()] == pat.as_bytes()
}

/// Parse one chunk (letters only) into units.
fn parse_chunk(s: &[u8]) -> Vec<Unit> {
    let n = s.len();
    // best[i] = (cost, from, unit) for the prefix s[..i]
    const INF: u32 = u32::MAX;
    let mut best: Vec<(u32, usize, Option<Unit>)> = alloc::vec![(INF, 0, None); n + 1];
    best[0] = (0, 0, None);
    for i in 0..n {
        let base = best[i].0;
        if base == INF {
            continue;
        }
        let mut relax = |j: usize, c: u32, u: Unit, best: &mut Vec<(u32, usize, Option<Unit>)>| {
            if base + c < best[j].0 {
                best[j] = (base + c, i, Some(u));
            }
        };
        // onset (or none) + vowel + coda (or none)
        let mut onsets: Vec<(usize, u32)> = Vec::new();
        onsets.push((0, 11)); // no letter: the silent ㅇ
        for (r, ix) in ONSETS {
            if starts(s, i, r) {
                onsets.push((r.len(), *ix));
            }
        }
        for &(ol, oi) in &onsets {
            let vi0 = i + ol;
            for (vr, vix) in VOWELS {
                if !starts(s, vi0, vr) {
                    continue;
                }
                let ci0 = vi0 + vr.len();
                let syl = |coda: u32| 0xAC00 + (oi * 21 + vix) * 28 + coda;
                relax(ci0, 1000, Unit::Syl(syl(0)), &mut best);
                for (cr, cix) in CODAS {
                    if starts(s, ci0, cr) {
                        // letters in a coda cost a little: a consonant prefers the next onset
                        relax(ci0 + cr.len(), 1000 + cr.len() as u32, Unit::Syl(syl(*cix)), &mut best);
                    }
                }
            }
            // a consonant on its own (nothing to pair with yet)
            if ol > 0 {
                relax(i + ol, 5000, Unit::Lone(oi), &mut best);
            }
        }
        // a letter that is nothing in Korean: skip it (keeps the parse total)
        relax(i + 1, 20000, Unit::Lone(u32::MAX), &mut best);
    }
    let mut out = Vec::new();
    let mut j = n;
    while j > 0 {
        let (_, from, u) = best[j];
        if let Some(u) = u {
            out.push(u);
        }
        j = from;
    }
    out.reverse();
    out
}

fn lone_jamo(oi: u32) -> Option<char> {
    CHO.chars().nth(oi as usize)
}

/// Hangul for a romanized word. Letters that are not Korean are dropped; `'` / `-` split syllables.
pub fn romaja_to_hangul(raw: &str) -> String {
    let mut out = String::new();
    for chunk in raw.split(|c| c == '\'' || c == '-') {
        let lower: Vec<u8> = chunk.bytes().map(|b| b.to_ascii_lowercase()).collect();
        for u in parse_chunk(&lower) {
            match u {
                Unit::Syl(cp) => {
                    if let Some(c) = char::from_u32(cp) {
                        out.push(c)
                    }
                },
                Unit::Lone(oi) => {
                    if let Some(c) = lone_jamo(oi) {
                        out.push(c)
                    }
                },
            }
        }
    }
    out
}

/// A letter that can be part of a romanized Korean word.
pub fn is_romaja(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '\'' || c == '-'
}

/// Hangul syllables and compatibility jamo moved to the Plane-15 private-use code points the
/// Daemon stacked font (`daemon_stacked.ttf`) carries: U+F0000 + (syllable - U+AC00), and
/// U+F2C00 + (jamo - U+3131). Anything else is kept.
pub fn to_daemon(s: &str) -> String {
    s.chars()
        .map(|c| {
            let u = c as u32;
            let m = if (0xAC00..=0xD7A3).contains(&u) {
                0xF0000 + u - 0xAC00
            } else if (0x3131..=0x318E).contains(&u) {
                0xF2C00 + u - 0x3131
            } else {
                u
            };
            char::from_u32(m).unwrap_or(c)
        })
        .collect()
}

/// Is `c` one of the Daemon stacked private-use code points?
pub fn is_daemon(c: char) -> bool {
    (0xF0000..=0xF2C5D).contains(&(c as u32))
}
