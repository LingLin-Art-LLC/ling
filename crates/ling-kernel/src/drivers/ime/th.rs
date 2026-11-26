//! Thai typed phonetically (an RTGS-based romanization), or on the Kedmanee layout.
//!
//! A word is read as syllables of initial (+ r / l / w cluster) + vowel + optional final,
//! then spelled in Thai: leading vowels (เ แ โ ใ ไ) go before the initial, above / below
//! vowels after it, a tone mark (typed as a digit: 1 ่ 2 ้ 3 ๊ 4 ๋) after those, then the
//! rest of the vowel and the final. `!` adds the silent mark ์ to the syllable.
//!
//! Initials: k ก, kh ค/ข/ฆ, g ก, ng ง, j จ, ch ช/ฉ/ฌ, s ส/ซ/ศ/ษ, y ย/ญ, d ด/ฎ, t ต/ฏ,
//! th ท/ถ/ธ/ฐ/ฑ/ฒ, n น/ณ, b บ, p ป, ph พ/ผ/ภ, f ฟ/ฝ, m ม, r ร, l ล/ฬ, w ว, h ห/ฮ,
//! x อ (a word that starts with a vowel gets อ by itself); h before n m ng y r l w is the
//! leading ห (hma หมา).
//! Vowels: a (ะ / ั), aa า, i ิ, ii ี, ue ือ / ึ, uee ื, u ุ, uu ู, e เ-ะ / เ-็, ee เ-,
//! ae แ-, o โ-ะ / (none), oo โ-, aw or -อ, oe เ-อ / เ-ิ, ia เ-ีย, uea เ-ือ, ua -ัว / -ว,
//! ai ไ-, ao เ-า, am -ำ, aai -าย, aao -าว, ui -ุย, oi โ-ย, oei เ-ย, uai -วย, iu -ิว,
//! eo เ-็ว, aeo แ-ว, iao เ-ียว.
//! Finals: k ก, t ด, p บ, m ม, n น, ng ง, y ย, w ว (and s ส, l ล, r ร, f ฟ, ch ช, j จ).
//! The candidate list offers the other letters for the same sound (k: ค ข ฆ, s: ซ ศ ษ ...)
//! and the other vowel spellings, for the last syllable typed.
use alloc::string::String;
use alloc::vec::Vec;

const INITIALS: &[(&str, &str)] = &[
    ("k", "ก"), ("kh", "คขฆฅ"), ("g", "ก"), ("ng", "ง"), ("j", "จ"), ("ch", "ชฉฌ"), ("s", "สซศษ"),
    ("y", "ยญ"), ("d", "ดฎ"), ("t", "ตฏ"), ("th", "ทถธฐฑฒ"), ("n", "นณ"), ("b", "บ"), ("p", "ป"),
    ("ph", "พผภ"), ("f", "ฟฝ"), ("m", "ม"), ("r", "ร"), ("l", "ลฬ"), ("w", "ว"), ("h", "หฮ"),
    ("x", "อ"), ("q", "อ"),
];

/// Second consonants of a cluster: kr, pl, khw ...
const CLUSTER2: &[(&str, char)] = &[("r", 'ร'), ("l", 'ล'), ("w", 'ว')];
/// Initials that take a cluster.
const CLUSTER1: &[&str] = &["k", "kh", "p", "ph", "t", "d", "b", "f", "g"];
/// Sonorants a leading ห makes high class.
const HSON: &[(&str, char)] = &[("ng", 'ง'), ("n", 'น'), ("m", 'ม'), ("y", 'ย'), ("r", 'ร'), ("l", 'ล'), ("w", 'ว')];

/// (romanization, open spelling, closed spelling, other spellings, takes a final)
/// Patterns: C = the initial (cluster), T = the tone mark, ~ = ็ when there is no tone mark.
const VOWELS: &[(&str, &str, &str, &[&str], bool)] = &[
    ("a", "CTะ", "CัT", &["CTา"], true),
    ("aa", "CTา", "CTา", &[], true),
    ("i", "CิT", "CิT", &["CีT"], true),
    ("ii", "CีT", "CีT", &["CิT"], true),
    ("ue", "CืTอ", "CึT", &["CืT", "CึT"], true),
    ("uee", "CืTอ", "CืT", &["CึT"], true),
    ("u", "CุT", "CุT", &["CูT"], true),
    ("uu", "CูT", "CูT", &["CุT"], true),
    ("e", "เCTะ", "เC~", &["เCT"], true),
    ("ee", "เCT", "เCT", &["เC~"], true),
    ("ae", "แCT", "แCT", &["แCTะ", "แC~"], true),
    ("o", "โCTะ", "CT", &["โCT"], true),
    ("oo", "โCT", "โCT", &["CT"], true),
    ("aw", "CTอ", "CTอ", &["เCTาะ", "C~อ"], true),
    ("or", "CTอ", "CTอ", &["เCTาะ"], true),
    ("oe", "เCTอ", "เCิT", &["เCTอะ"], true),
    ("ia", "เCีTย", "เCีTย", &["เCียะ"], true),
    ("uea", "เCืTอ", "เCืTอ", &[], true),
    ("ua", "CัTว", "CTว", &["CัวะT"], true),
    ("ai", "ไCT", "ไCT", &["ใCT", "CัTย", "ไCTย"], false),
    ("ao", "เCTา", "เCTา", &["CTาว"], false),
    ("am", "CTำ", "CTำ", &["CัTม"], false),
    ("aai", "CTาย", "CTาย", &[], false),
    ("aao", "CTาว", "CTาว", &[], false),
    ("ui", "CุTย", "CุTย", &[], false),
    ("oi", "โCTย", "โCTย", &["CTอย"], false),
    ("oy", "CTอย", "CTอย", &["โCTย"], false),
    ("oei", "เCTย", "เCTย", &[], false),
    ("uai", "CTวย", "CTวย", &[], false),
    ("iu", "CิTว", "CิTว", &[], false),
    ("eo", "เC~ว", "เC~ว", &["เCTว"], false),
    ("eeo", "เCTว", "เCTว", &[], false),
    ("aeo", "แCTว", "แCTว", &[], false),
    ("iao", "เCีTยว", "เCีTยว", &[], false),
];

const FINALS: &[(&str, &str)] = &[
    ("k", "กขค"), ("g", "ก"), ("t", "ดตทสจชศษธ"), ("d", "ด"), ("p", "บปพฟ"), ("b", "บ"), ("m", "ม"),
    ("n", "นญณรล"), ("ng", "ง"), ("y", "ย"), ("w", "ว"), ("s", "สศษ"), ("l", "ลฬ"), ("r", "ร"), ("f", "ฟ"),
    ("ch", "ช"), ("j", "จ"),
];

const TONES: [char; 4] = ['\u{0E48}', '\u{0E49}', '\u{0E4A}', '\u{0E4B}'];

#[derive(Clone)]
struct Syl {
    /// initial letters: the class-varying first consonant's choices, then the rest of the cluster
    first: &'static str,
    rest: String,
    vowel: usize,
    fin: Option<&'static str>,
    tone: Option<char>,
    karan: bool,
}

fn starts(s: &[u8], at: usize, pat: &str) -> bool {
    s.len() >= at + pat.len() && &s[at..at + pat.len()] == pat.as_bytes()
}

/// Possible initials at `i`: (letters used, first-consonant choices, rest of the cluster)
fn initials(s: &[u8], i: usize) -> Vec<(usize, &'static str, String)> {
    let mut v: Vec<(usize, &'static str, String)> = Vec::new();
    v.push((0, "อ", String::new()));
    for (r, th) in INITIALS {
        if !starts(s, i, r) {
            continue;
        }
        v.push((r.len(), th, String::new()));
        if CLUSTER1.contains(r) {
            for (r2, c2) in CLUSTER2 {
                if starts(s, i + r.len(), r2) {
                    let mut rest = String::new();
                    rest.push(*c2);
                    v.push((r.len() + r2.len(), th, rest));
                }
            }
        }
        if *r == "h" {
            for (r2, c2) in HSON {
                if starts(s, i + 1, r2) {
                    let mut rest = String::new();
                    rest.push(*c2);
                    v.push((1 + r2.len(), "ห", rest));
                }
            }
        }
    }
    v
}

fn parse(s: &[u8]) -> Vec<Syl> {
    let n = s.len();
    const INF: u32 = u32::MAX;
    let mut best: Vec<(u32, usize, Option<Syl>)> = alloc::vec![(INF, 0, None); n + 1];
    best[0] = (0, 0, None);
    for i in 0..n {
        let base = best[i].0;
        if base == INF {
            continue;
        }
        let mut try_set = |j: usize, c: u32, sy: Syl, best: &mut Vec<(u32, usize, Option<Syl>)>| {
            if base + c < best[j].0 {
                best[j] = (base + c, i, Some(sy));
            }
        };
        for (il, first, rest) in initials(s, i) {
            let vi = i + il;
            for (k, (vr, _, _, _, takes_final)) in VOWELS.iter().enumerate() {
                if !starts(s, vi, vr) {
                    continue;
                }
                let after_v = vi + vr.len();
                let mut ends: Vec<(usize, Option<&'static str>, u32)> = alloc::vec![(after_v, None, 0)];
                if *takes_final {
                    for (fr, fth) in FINALS {
                        if starts(s, after_v, fr) {
                            ends.push((after_v + fr.len(), Some(fth), fr.len() as u32));
                        }
                    }
                }
                for (e, fin, fcost) in ends {
                    // a tone digit and / or the silent mark may follow the syllable
                    let mut e2 = e;
                    let mut tone = None;
                    if e2 < n && (b'1'..=b'4').contains(&s[e2]) {
                        tone = Some(TONES[(s[e2] - b'1') as usize]);
                        e2 += 1;
                    }
                    let mut karan = false;
                    if e2 < n && s[e2] == b'!' {
                        karan = true;
                        e2 += 1;
                    }
                    // a vowel-initial syllable after another syllable costs more (prefer onsets)
                    let onset_cost = if il == 0 && i > 0 { 3 } else { 0 };
                    let sy = Syl { first, rest: rest.clone(), vowel: k, fin, tone, karan };
                    try_set(e2, 1000 + fcost + onset_cost, sy, &mut best);
                }
            }
        }
    }
    if best[n].0 == INF {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut j = n;
    while j > 0 {
        let (_, from, sy) = best[j].clone();
        if let Some(sy) = sy {
            out.push(sy);
        }
        j = from;
    }
    out.reverse();
    out
}

fn spell(sy: &Syl, first: char, pat: &str, fin: Option<char>, out: &mut String) {
    let mut c = String::new();
    c.push(first);
    c.push_str(&sy.rest);
    for ch in pat.chars() {
        match ch {
            'C' => out.push_str(&c),
            'T' => {
                if let Some(t) = sy.tone {
                    out.push(t)
                }
            },
            '~' => out.push(sy.tone.unwrap_or('\u{0E47}')),
            _ => out.push(ch),
        }
    }
    if let Some(f) = fin {
        out.push(f);
    }
    if sy.karan {
        out.push('\u{0E4C}');
    }
}

fn spell_default(sy: &Syl, out: &mut String) {
    let v = &VOWELS[sy.vowel];
    let pat = if sy.fin.is_some() { v.2 } else { v.1 };
    let first = sy.first.chars().next().unwrap_or('อ');
    spell(sy, first, pat, sy.fin.and_then(|f| f.chars().next()), out);
}

/// Can `raw` be read as Thai (so far)? Letters, tone digits and `!`.
pub fn is_input(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '!'
}

/// The Thai for `raw`: the default spelling first, then up to `max - 1` others that change
/// the last syllable's initial letter, vowel spelling or final letter.
pub fn candidates(raw: &str, max: usize) -> Vec<String> {
    let lower: Vec<u8> = raw.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let syls = parse(&lower);
    let mut out: Vec<String> = Vec::new();
    if syls.is_empty() {
        return out;
    }
    let mut head = String::new();
    for sy in &syls[..syls.len() - 1] {
        spell_default(sy, &mut head);
    }
    let last = &syls[syls.len() - 1];
    let mut push = |s: String, out: &mut Vec<String>| {
        if !out.contains(&s) && out.len() < max {
            out.push(s);
        }
    };
    let mut d = head.clone();
    spell_default(last, &mut d);
    push(d, &mut out);
    let v = &VOWELS[last.vowel];
    let pat = if last.fin.is_some() { v.2 } else { v.1 };
    let fin0 = last.fin.and_then(|f| f.chars().next());
    // the initial's other letters
    for c in last.first.chars().skip(1) {
        let mut s = head.clone();
        spell(last, c, pat, fin0, &mut s);
        push(s, &mut out);
    }
    let first0 = last.first.chars().next().unwrap_or('อ');
    // the vowel's other spellings
    let mut pats: Vec<&str> = v.3.to_vec();
    if last.fin.is_some() && v.1 != v.2 {
        pats.push(v.1);
    }
    for p in pats {
        let mut s = head.clone();
        spell(last, first0, p, fin0, &mut s);
        push(s, &mut out);
    }
    // the final's other letters
    if let Some(f) = last.fin {
        for c in f.chars().skip(1) {
            let mut s = head.clone();
            spell(last, first0, pat, Some(c), &mut s);
            push(s, &mut out);
        }
    }
    out
}

/// Kedmanee, the standard Thai keyboard, on US key positions.
pub fn kedmanee(c: char) -> char {
    const US: &str = "`1234567890-=qwertyuiop[]\\asdfghjkl;'zxcvbnm,./~!@#$%^&*()_+QWERTYUIOP{}|ASDFGHJKL:\"ZXCVBNM<>?";
    const TH: &str = "_ๅ/-ภถุึคตจขชๆไำพะัีรนยบลฃฟหกดเ้่าสวงผปแอิืทมใฝ%+๑๒๓๔ู฿๕๖๗๘๙๐\"ฎฑธํ๊ณฯญฐ,ฅฤฆฏโฌ็๋ษศซ.()ฉฮฺ์?ฒฬฦ";
    match US.chars().position(|x| x == c) {
        Some(i) => TH.chars().nth(i).unwrap_or(c),
        None => c,
    }
}
