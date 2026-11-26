//! Romaji to kana (Hepburn and Kunrei spellings both work).
//!
//! `ka` か, `shi`/`si` し, `kya` きゃ, `fa` ふぁ, a doubled consonant gives a small tsu
//! (`kka` っか, `tch` っち), `n'` or `nn` gives ん (`nn` before a vowel or y is ん + the n-row:
//! `konnichiha` こんにちは, `minna` みんな), `n` before another consonant is ん, `-` is ー.
//! Letters typed with Shift come out in katakana.
use alloc::string::String;
use alloc::vec::Vec;

const ROMAJI: &[(&str, &str)] = &[
    ("a", "あ"), ("i", "い"), ("u", "う"), ("e", "え"), ("o", "お"),
    ("ka", "か"), ("ki", "き"), ("ku", "く"), ("ke", "け"), ("ko", "こ"),
    ("ga", "が"), ("gi", "ぎ"), ("gu", "ぐ"), ("ge", "げ"), ("go", "ご"),
    ("sa", "さ"), ("si", "し"), ("shi", "し"), ("su", "す"), ("se", "せ"), ("so", "そ"),
    ("za", "ざ"), ("zi", "じ"), ("ji", "じ"), ("zu", "ず"), ("ze", "ぜ"), ("zo", "ぞ"),
    ("ta", "た"), ("ti", "ち"), ("chi", "ち"), ("tu", "つ"), ("tsu", "つ"), ("te", "て"), ("to", "と"),
    ("da", "だ"), ("di", "ぢ"), ("du", "づ"), ("de", "で"), ("do", "ど"),
    ("na", "な"), ("ni", "に"), ("nu", "ぬ"), ("ne", "ね"), ("no", "の"),
    ("ha", "は"), ("hi", "ひ"), ("hu", "ふ"), ("fu", "ふ"), ("he", "へ"), ("ho", "ほ"),
    ("ba", "ば"), ("bi", "び"), ("bu", "ぶ"), ("be", "べ"), ("bo", "ぼ"),
    ("pa", "ぱ"), ("pi", "ぴ"), ("pu", "ぷ"), ("pe", "ぺ"), ("po", "ぽ"),
    ("ma", "ま"), ("mi", "み"), ("mu", "む"), ("me", "め"), ("mo", "も"),
    ("ya", "や"), ("yu", "ゆ"), ("ye", "いぇ"), ("yo", "よ"),
    ("ra", "ら"), ("ri", "り"), ("ru", "る"), ("re", "れ"), ("ro", "ろ"),
    ("la", "ら"), ("li", "り"), ("lu", "る"), ("le", "れ"), ("lo", "ろ"),
    ("wa", "わ"), ("wi", "うぃ"), ("we", "うぇ"), ("wo", "を"), ("n'", "ん"),
    ("xa", "ぁ"), ("xi", "ぃ"), ("xu", "ぅ"), ("xe", "ぇ"), ("xo", "ぉ"),
    ("xya", "ゃ"), ("xyu", "ゅ"), ("xyo", "ょ"), ("xtu", "っ"), ("xtsu", "っ"), ("xwa", "ゎ"),
    ("ltu", "っ"), ("lya", "ゃ"), ("lyu", "ゅ"), ("lyo", "ょ"),
    ("kya", "きゃ"), ("kyu", "きゅ"), ("kyo", "きょ"), ("gya", "ぎゃ"), ("gyu", "ぎゅ"), ("gyo", "ぎょ"),
    ("sya", "しゃ"), ("syu", "しゅ"), ("syo", "しょ"), ("sha", "しゃ"), ("shu", "しゅ"), ("sho", "しょ"), ("she", "しぇ"),
    ("zya", "じゃ"), ("zyu", "じゅ"), ("zyo", "じょ"), ("ja", "じゃ"), ("ju", "じゅ"), ("jo", "じょ"), ("je", "じぇ"),
    ("jya", "じゃ"), ("jyu", "じゅ"), ("jyo", "じょ"),
    ("tya", "ちゃ"), ("tyu", "ちゅ"), ("tyo", "ちょ"), ("cha", "ちゃ"), ("chu", "ちゅ"), ("cho", "ちょ"), ("che", "ちぇ"),
    ("cya", "ちゃ"), ("cyu", "ちゅ"), ("cyo", "ちょ"),
    ("dya", "ぢゃ"), ("dyu", "ぢゅ"), ("dyo", "ぢょ"), ("nya", "にゃ"), ("nyu", "にゅ"), ("nyo", "にょ"),
    ("hya", "ひゃ"), ("hyu", "ひゅ"), ("hyo", "ひょ"), ("bya", "びゃ"), ("byu", "びゅ"), ("byo", "びょ"),
    ("pya", "ぴゃ"), ("pyu", "ぴゅ"), ("pyo", "ぴょ"), ("mya", "みゃ"), ("myu", "みゅ"), ("myo", "みょ"),
    ("rya", "りゃ"), ("ryu", "りゅ"), ("ryo", "りょ"), ("lya", "りゃ"),
    ("fa", "ふぁ"), ("fi", "ふぃ"), ("fe", "ふぇ"), ("fo", "ふぉ"), ("fyu", "ふゅ"),
    ("tsa", "つぁ"), ("tsi", "つぃ"), ("tse", "つぇ"), ("tso", "つぉ"),
    ("thi", "てぃ"), ("dhi", "でぃ"), ("twu", "とぅ"), ("dwu", "どぅ"),
    ("va", "ゔぁ"), ("vi", "ゔぃ"), ("vu", "ゔ"), ("ve", "ゔぇ"), ("vo", "ゔぉ"),
    ("-", "ー"), (",", "、"), (".", "。"), ("[", "「"), ("]", "」"), ("~", "〜"), ("/", "・"),
];

fn exact(p: &str) -> Option<&'static str> {
    ROMAJI.iter().find(|(r, _)| *r == p).map(|(_, k)| *k)
}

fn is_prefix(p: &str) -> bool {
    ROMAJI.iter().any(|(r, _)| r.len() > p.len() && r.starts_with(p))
}

/// Hiragana to katakana (other characters kept).
pub fn to_katakana(s: &str) -> String {
    s.chars()
        .map(|c| if ('ぁ'..='ゖ').contains(&c) { char::from_u32(c as u32 + 0x60).unwrap_or(c) } else { c })
        .collect()
}

/// Katakana to hiragana (other characters kept).
pub fn to_hiragana(s: &str) -> String {
    s.chars()
        .map(|c| if ('ァ'..='ヶ').contains(&c) { char::from_u32(c as u32 - 0x60).unwrap_or(c) } else { c })
        .collect()
}

/// The characters romaji input takes (the rest commit what is composed).
pub fn is_romaji(c: char) -> bool {
    c.is_ascii_alphabetic() || "-,.[]~'/".contains(c)
}

fn is_vowel(c: u8) -> bool {
    matches!(c, b'a' | b'i' | b'u' | b'e' | b'o')
}

/// Convert romaji `raw` to kana; returns (kana, romaji still pending at the end). `kata` makes
/// everything katakana; Shifted letters make their own kana katakana either way.
/// With `finish`, a pending lone `n` becomes ん (what Space / Enter / F7 do).
pub fn convert(raw: &str, kata: bool, finish: bool) -> (String, String) {
    let bytes: Vec<u8> = raw.bytes().collect();
    let mut out = String::new();
    let mut i = 0usize;
    let n = bytes.len();
    let emit = |out: &mut String, k: &str, upper: bool| {
        if kata || upper {
            out.push_str(&to_katakana(k));
        } else {
            out.push_str(k);
        }
    };
    while i < n {
        let lc = |j: usize| bytes[j].to_ascii_lowercase();
        let up = |a: usize, b: usize| bytes[a..b].iter().any(|c| c.is_ascii_uppercase());
        // ん: n' / nn (before a vowel or y: ん, and the second n starts the next kana)
        if lc(i) == b'n' && i + 1 < n {
            let c1 = lc(i + 1);
            if c1 == b'\'' {
                emit(&mut out, "ん", up(i, i + 1));
                i += 2;
                continue;
            }
            if c1 == b'n' {
                if i + 2 < n && (is_vowel(lc(i + 2)) || lc(i + 2) == b'y') {
                    emit(&mut out, "ん", up(i, i + 1));
                    i += 1;
                    continue;
                }
                if i + 2 >= n && !finish {
                    break; // wait: nn + a vowel would be ん + な..
                }
                emit(&mut out, "ん", up(i, i + 2));
                i += 2;
                continue;
            }
            if !is_vowel(c1) && c1 != b'y' {
                emit(&mut out, "ん", up(i, i + 1));
                i += 1;
                continue;
            }
        }
        // a doubled consonant: っ, the second starts the next kana (tch: っち)
        if i + 1 < n {
            let (a, b) = (lc(i), lc(i + 1));
            if a.is_ascii_alphabetic() && !is_vowel(a) && a != b'n' && (a == b || (a == b't' && b == b'c')) {
                emit(&mut out, "っ", up(i, i + 1));
                i += 1;
                continue;
            }
        }
        // the longest table entry that matches here
        let mut matched = 0usize;
        let mut pending = false;
        for len in (1..=4usize.min(n - i)).rev() {
            let p: String = (i..i + len).map(|j| lc(j) as char).collect();
            if let Some(k) = exact(&p) {
                // `n` alone is only ん once we know what follows
                emit(&mut out, k, up(i, i + len));
                matched = len;
                break;
            }
            if i + len == n && is_prefix(&p) {
                pending = true;
                break;
            }
        }
        if matched > 0 {
            i += matched;
            continue;
        }
        if pending {
            break;
        }
        // nothing in the table starts here
        if lc(i) == b'n' && i + 1 == n {
            break;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    let mut rest: String = bytes[i..].iter().map(|&b| b as char).collect();
    if finish {
        if rest.eq_ignore_ascii_case("n") || rest.eq_ignore_ascii_case("nn") {
            emit(&mut out, "ん", rest.bytes().any(|c| c.is_ascii_uppercase()));
            rest.clear();
        } else {
            out.push_str(&rest);
            rest.clear();
        }
    }
    (out, rest)
}
