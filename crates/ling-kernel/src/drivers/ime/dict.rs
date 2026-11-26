//! The conversion dictionaries (built by the IME build script into `assets/ime/*.z`) and the
//! two converters that use them: Japanese kana to kanji, Chinese pinyin to hanzi.
//!
//! A dictionary is sorted UTF-8 lines, `key \t score \t cand \t cand ...`, searched in place.
//! Japanese: key = hiragana reading, score = mozc cost (lower = more common), cands = surfaces.
//! Chinese: key = toneless syllables joined by `'`, score = 100 * ln(frequency),
//! cands = `simplified|traditional|tones` (traditional empty when it is the same).
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

pub struct Dict {
    data: Vec<u8>,
    lines: Vec<u32>,
    /// Chinese: every single syllable that has an entry
    syls: BTreeSet<String>,
}

impl Dict {
    pub fn new(data: Vec<u8>, chinese: bool) -> Dict {
        let mut lines = Vec::new();
        let mut at = 0usize;
        while at < data.len() {
            lines.push(at as u32);
            match data[at..].iter().position(|&b| b == b'\n') {
                Some(p) => at += p + 1,
                None => break,
            }
        }
        let mut d = Dict { data, lines, syls: BTreeSet::new() };
        if chinese {
            let mut s = BTreeSet::new();
            for i in 0..d.lines.len() {
                let k = d.key(i);
                if !k.contains('\'') && !k.is_empty() {
                    s.insert(String::from(k));
                }
            }
            d.syls = s;
        }
        d
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    fn line(&self, i: usize) -> &str {
        let a = self.lines[i] as usize;
        let b = self.lines.get(i + 1).map(|&x| x as usize - 1).unwrap_or(self.data.len());
        let b = if b > a && self.data[b - 1] == b'\n' { b - 1 } else { b };
        core::str::from_utf8(&self.data[a..b.max(a)]).unwrap_or("")
    }

    fn key(&self, i: usize) -> &str {
        let l = self.line(i);
        l.split('\t').next().unwrap_or("")
    }

    fn lower_bound(&self, key: &str) -> usize {
        let (mut lo, mut hi) = (0usize, self.lines.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.key(mid).as_bytes() < key.as_bytes() {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// (score, candidates) of exactly `key`.
    pub fn find(&self, key: &str) -> Option<(i32, Vec<&str>)> {
        let i = self.lower_bound(key);
        if i < self.lines.len() && self.key(i) == key {
            return Some(self.entry(i));
        }
        None
    }

    fn entry(&self, i: usize) -> (i32, Vec<&str>) {
        let mut it = self.line(i).split('\t');
        it.next();
        let score = it.next().and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
        (score, it.collect())
    }

    /// Entries whose key starts with `prefix` (at most `max`), in key order.
    pub fn with_prefix(&self, prefix: &str, max: usize) -> Vec<(&str, i32, Vec<&str>)> {
        let mut out = Vec::new();
        let mut i = self.lower_bound(prefix);
        while i < self.lines.len() && out.len() < max {
            let k = self.key(i);
            if !k.starts_with(prefix) {
                break;
            }
            let (s, c) = self.entry(i);
            out.push((k, s, c));
            i += 1;
        }
        out
    }
}

// ── Japanese ─────────────────────────────────────────────────────────────

/// One converted stretch of the reading and its candidates (best first).
#[derive(Clone, Debug)]
pub struct Seg {
    pub reading: String,
    pub cands: Vec<String>,
}

/// Readings that read best left in kana (particles, endings, auxiliaries).
const KANA_WORDS: &[&str] = &[
    "は", "が", "を", "に", "へ", "と", "で", "も", "や", "の", "か", "ね", "よ", "な", "た", "て", "だ", "ば",
    "です", "ます", "まし", "ました", "でし", "でした", "ない", "たい", "から", "まで", "より", "など", "して",
    "ので", "のに", "けど", "だっ", "でしょう", "ません", "ている", "いる", "ある", "する", "なる", "この",
    "その", "あの", "これ", "それ", "あれ", "ここ", "そこ", "こと", "もの", "よう", "ください", "ちゃ", "って",
];
/// Kana that start an inflection: a stem before one of these keeps its okurigana form (食べ+ます).
const INFLECT: &str = "たてだでまなるれらせさずぬようば";

fn is_kana_only(s: &str) -> bool {
    s.chars().all(|c| ('ぁ'..='ゖ').contains(&c) || ('ァ'..='ヺ').contains(&c) || c == 'ー')
}

fn has_kanji(s: &str) -> bool {
    s.chars().any(|c| ('\u{3400}'..='\u{9FFF}').contains(&c) || c == '々')
}

/// Split `kana` into words (fewest, most common) and rank each word's spellings.
pub fn ja_convert(d: &Dict, kana: &str) -> Vec<Seg> {
    let cs: Vec<char> = kana.chars().collect();
    let n = cs.len();
    const INF: i64 = i64::MAX;
    // best[i] = (cost, from)
    let mut best: Vec<(i64, usize)> = alloc::vec![(INF, 0); n + 1];
    best[0] = (0, 0);
    for i in 0..n {
        if best[i].0 == INF {
            continue;
        }
        let base = best[i].0;
        // an unknown kana on its own
        if base + 20000 < best[i + 1].0 {
            best[i + 1] = (base + 20000, i);
        }
        let mut key = String::new();
        for j in i..n.min(i + 12) {
            key.push(cs[j]);
            if let Some((cost, _)) = d.find(&key) {
                let c = base + cost.max(0) as i64 + 5000;
                if c < best[j + 1].0 {
                    best[j + 1] = (c, i);
                }
            }
        }
    }
    let mut cuts = Vec::new();
    let mut j = n;
    while j > 0 {
        cuts.push(j);
        j = best[j].1;
    }
    cuts.push(0);
    cuts.reverse();
    let mut segs: Vec<Seg> = Vec::new();
    for w in cuts.windows(2) {
        let reading: String = cs[w[0]..w[1]].iter().collect();
        segs.push(Seg { reading, cands: Vec::new() });
    }
    for k in 0..segs.len() {
        let next_first = segs.get(k + 1).and_then(|s| s.reading.chars().next());
        let r = segs[k].reading.clone();
        segs[k].cands = ja_cands(d, &r, next_first);
    }
    segs
}

/// The spellings of one reading, best first, always ending with its hiragana and katakana.
pub fn ja_cands(d: &Dict, reading: &str, next: Option<char>) -> Vec<String> {
    let kata = super::kana::to_katakana(reading);
    let mut list: Vec<String> = match d.find(reading) {
        Some((_, c)) => c.iter().map(|s| String::from(*s)).collect(),
        None => Vec::new(),
    };
    let kana_word = KANA_WORDS.contains(&reading);
    if !kana_word {
        // the first spelling with a kanji leads; before an inflection, one with okurigana
        let inflect = next.is_some_and(|c| INFLECT.contains(c));
        let pick = if inflect {
            list.iter().take(6).position(|s| has_kanji(s) && s.chars().last().is_some_and(|c| ('ぁ'..='ゖ').contains(&c)))
        } else {
            None
        }
        .or_else(|| list.iter().position(|s| has_kanji(s) || (!is_kana_only(s) && s != reading)));
        if let Some(p) = pick {
            let s = list.remove(p);
            list.insert(0, s);
        }
    } else if let Some(p) = list.iter().position(|s| s == reading) {
        let s = list.remove(p);
        list.insert(0, s);
    }
    // hiragana and katakana are always there
    let mut out: Vec<String> = Vec::new();
    for s in list {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if out.is_empty() {
        out.push(String::from(reading));
    }
    for s in [String::from(reading), kata] {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

// ── Chinese ──────────────────────────────────────────────────────────────

/// One syllable of typed pinyin.
#[derive(Clone, Debug)]
pub struct Syl {
    pub text: String,
    /// where it ends in the typed text (bytes)
    pub end: usize,
    /// a tone digit typed after it (tone-number mode)
    pub tone: Option<u8>,
    /// only the start of a syllable (still being typed)
    pub partial: bool,
}

/// Split typed pinyin into syllables (fewest syllables; `'` forces a break; with `tones`,
/// a digit 1-5 after a syllable is its tone). The last one may be partial.
pub fn zh_syllables(d: &Dict, raw: &str, tones: bool) -> Vec<Syl> {
    let b = raw.as_bytes();
    let n = b.len();
    const INF: u32 = u32::MAX;
    let mut best: Vec<(u32, usize, Option<Syl>)> = alloc::vec![(INF, 0, None); n + 1];
    best[0] = (0, 0, None);
    for i in 0..n {
        if best[i].0 == INF {
            continue;
        }
        let base = best[i].0;
        if b[i] == b'\'' {
            if base < best[i + 1].0 {
                best[i + 1] = (base, i, None);
            }
            continue;
        }
        let mut j = i;
        let mut piece = String::new();
        while j < n && b[j].is_ascii_alphabetic() && j - i < 6 {
            piece.push(b[j].to_ascii_lowercase() as char);
            j += 1;
            let full = d.syls.contains(&piece);
            let partial = !full && j == n && d.syls.range(piece.clone()..).next().is_some_and(|s| s.starts_with(piece.as_str()));
            // a partial piece also may be the start of a longer full one
            let partial = partial || (j == n && !full && piece.len() == 1);
            if !full && !partial {
                continue;
            }
            let mut e = j;
            let mut tone = None;
            if tones && e < n && (b'1'..=b'5').contains(&b[e]) {
                tone = Some(b[e] - b'0');
                e += 1;
            }
            let c = base + 1000 + if partial { 500 } else { 0 };
            if c < best[e].0 {
                best[e] = (c, i, Some(Syl { text: piece.clone(), end: e, tone, partial }));
            }
        }
    }
    // whatever does not parse: cut it off where parsing stopped
    let mut end = n;
    while end > 0 && best[end].0 == INF {
        end -= 1;
    }
    let mut out = Vec::new();
    let mut j = end;
    while j > 0 {
        let (_, from, s) = best[j].clone();
        if let Some(s) = s {
            out.push(s);
        }
        j = from;
    }
    out.reverse();
    out
}

/// A Chinese candidate: its text and how many syllables it uses up.
#[derive(Clone, Debug, PartialEq)]
pub struct ZhCand {
    pub text: String,
    pub syls: usize,
}

fn zh_form<'a>(c: &'a str, trad: bool) -> (&'a str, &'a str) {
    let mut it = c.split('|');
    let s = it.next().unwrap_or("");
    let t = it.next().unwrap_or("");
    let tones = it.next().unwrap_or("");
    (if trad && !t.is_empty() { t } else { s }, tones)
}

fn tones_ok(want: &[Option<u8>], have: &str) -> bool {
    want.iter().zip(have.bytes()).all(|(w, h)| match w {
        Some(t) => *t == h - b'0',
        None => true,
    })
}

/// Candidates for the typed syllables: a whole-sentence guess (when it takes several words),
/// then the words that cover the most syllables from the start, down to single characters.
pub fn zh_candidates(d: &Dict, syls: &[Syl], trad: bool, max: usize) -> Vec<ZhCand> {
    let n = syls.len();
    let mut out: Vec<ZhCand> = Vec::new();
    if n == 0 {
        return out;
    }
    let tones: Vec<Option<u8>> = syls.iter().map(|s| s.tone).collect();
    let mut push = |text: &str, k: usize, out: &mut Vec<ZhCand>| {
        if out.len() < max && !out.iter().any(|c| c.text == text) {
            out.push(ZhCand { text: String::from(text), syls: k });
        }
    };
    // the entries for syllables [a, b)
    let span = |a: usize, b: usize| -> Vec<(i32, Vec<&str>)> {
        let mut key = String::new();
        for (k, s) in syls[a..b].iter().enumerate() {
            if k > 0 {
                key.push('\'');
            }
            key.push_str(&s.text);
        }
        if syls[b - 1].partial {
            let want = key.matches('\'').count();
            let mut v: Vec<(i32, Vec<&str>)> =
                d.with_prefix(&key, 400).into_iter().filter(|(k, _, _)| k.matches('\'').count() == want).map(|(_, s, c)| (s, c)).collect();
            v.sort_by(|x, y| y.0.cmp(&x.0));
            v.truncate(12);
            v
        } else {
            d.find(&key).into_iter().collect()
        }
    };
    // whole-sentence guess: the cheapest split into dictionary words
    if n > 1 {
        const INF: i64 = i64::MAX;
        let mut best: Vec<(i64, usize, String)> = alloc::vec![(INF, 0, String::new()); n + 1];
        best[0] = (0, 0, String::new());
        for a in 0..n {
            if best[a].0 == INF {
                continue;
            }
            for b in a + 1..=n.min(a + 8) {
                for (score, cands) in span(a, b) {
                    let Some(c) = cands.iter().find(|c| tones_ok(&tones[a..b], zh_form(c, trad).1)) else { continue };
                    let cost = best[a].0 + 1600 - score as i64;
                    if cost < best[b].0 {
                        let mut t = best[a].2.clone();
                        t.push_str(zh_form(c, trad).0);
                        best[b] = (cost, a, t);
                    }
                    break;
                }
            }
        }
        if best[n].0 != INF && best[n].1 != 0 {
            let t = best[n].2.clone();
            push(&t, n, &mut out);
        }
    }
    for k in (1..=n).rev() {
        for (_, cands) in span(0, k) {
            for c in cands {
                let (text, tn) = zh_form(c, trad);
                if tones_ok(&tones[..k], tn) {
                    push(text, k, &mut out);
                }
            }
        }
    }
    out
}
