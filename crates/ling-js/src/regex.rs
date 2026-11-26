//! Regular expressions (ECMAScript syntax, with the web's Annex B leniency outside `u` mode).
//! Compiled to a small program run by a backtracking VM with an explicit stack (no recursion
//! per character), a step limit against catastrophic patterns, lookaround and back references.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

#[derive(Clone, Debug, Default)]
pub struct Flags {
    pub global: bool,
    pub ignore_case: bool,
    pub multiline: bool,
    pub dot_all: bool,
    pub unicode: bool,
    pub sticky: bool,
    pub has_indices: bool,
}

#[derive(Clone, Debug)]
struct Class {
    ranges: Vec<(u32, u32)>,
    neg: bool,
}

#[derive(Clone, Debug)]
enum Node {
    Empty,
    Char(u32),
    Any,
    Class(Class),
    Start,
    End,
    WordB(bool),
    Group(Box<Node>, Option<usize>),
    Cat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat(Box<Node>, u32, Option<u32>, bool),
    Backref(usize),
    NamedBackref(String),
    Look(Box<Node>, bool, bool),
}

#[derive(Clone, Debug)]
enum Inst {
    Char(u32),
    Any,
    Class(usize),
    Start,
    End,
    WordB(bool),
    Save(usize),
    /// try the first, keep the second for backtracking
    Split(usize, usize),
    Jmp(usize),
    Backref(usize),
    /// a lookaround: sub-program index, ahead?, negative?
    Look(usize, bool, bool),
    /// remember the position in a register (empty-loop guard)
    Mark(usize),
    /// fail if no progress since the Mark
    Progress(usize),
    /// counted loops: set a counter to 0 / count up and loop while below n
    Match,
}

pub struct Regex {
    prog: Vec<Inst>,
    subs: Vec<Vec<Inst>>,
    classes: Vec<Class>,
    pub ncaps: usize,
    pub names: Vec<(String, usize)>,
    pub flags: Flags,
    nregs: usize,
}

pub fn parse_flags(f: &str) -> Result<Flags, String> {
    let mut fl = Flags::default();
    for c in f.chars() {
        let slot = match c {
            'g' => &mut fl.global,
            'i' => &mut fl.ignore_case,
            'm' => &mut fl.multiline,
            's' => &mut fl.dot_all,
            'u' | 'v' => &mut fl.unicode,
            'y' => &mut fl.sticky,
            'd' => &mut fl.has_indices,
            _ => return Err(format!("invalid regular expression flag '{}'", c)),
        };
        if *slot {
            return Err(format!("duplicate regular expression flag '{}'", c));
        }
        *slot = true;
    }
    Ok(fl)
}

struct P<'a> {
    s: &'a [u32],
    i: usize,
    ncaps: usize,
    names: Vec<(String, usize)>,
    unicode: bool,
}

const DIGIT: &[(u32, u32)] = &[(48, 57)];
const WORD: &[(u32, u32)] = &[(48, 57), (65, 90), (95, 95), (97, 122)];
const SPACE: &[(u32, u32)] = &[
    (9, 13),
    (32, 32),
    (0xA0, 0xA0),
    (0x1680, 0x1680),
    (0x2000, 0x200A),
    (0x2028, 0x2029),
    (0x202F, 0x202F),
    (0x205F, 0x205F),
    (0x3000, 0x3000),
    (0xFEFF, 0xFEFF),
];

fn negate(r: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut v = r.to_vec();
    v.sort();
    let mut out = Vec::new();
    let mut next = 0u32;
    for (a, b) in v {
        if a > next {
            out.push((next, a - 1));
        }
        next = next.max(b + 1);
    }
    if next <= 0x10FFFF {
        out.push((next, 0x10FFFF));
    }
    out
}

impl P<'_> {
    fn peek(&self) -> Option<u32> {
        self.s.get(self.i).copied()
    }
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c as u32) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn disjunction(&mut self) -> Result<Node, String> {
        let mut alts = vec![self.alternative()?];
        while self.eat('|') {
            alts.push(self.alternative()?);
        }
        Ok(if alts.len() == 1 { alts.pop().unwrap() } else { Node::Alt(alts) })
    }

    fn alternative(&mut self) -> Result<Node, String> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' as u32 || c == ')' as u32 {
                break;
            }
            let atom = self.term()?;
            items.push(atom);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap(),
            _ => Node::Cat(items),
        })
    }

    fn term(&mut self) -> Result<Node, String> {
        let c = self.peek().unwrap();
        // assertions
        if c == '^' as u32 {
            self.i += 1;
            return Ok(Node::Start);
        }
        if c == '$' as u32 {
            self.i += 1;
            return Ok(Node::End);
        }
        if c == '\\' as u32 && matches!(self.s.get(self.i + 1), Some(&b) if b == 'b' as u32 || b == 'B' as u32) {
            let b = self.s[self.i + 1] == 'b' as u32;
            self.i += 2;
            return Ok(Node::WordB(b));
        }
        if c == '(' as u32 && self.s.get(self.i + 1) == Some(&('?' as u32)) {
            let k = self.s.get(self.i + 2).copied();
            let look = match k {
                Some(x) if x == '=' as u32 => Some((true, false, 3)),
                Some(x) if x == '!' as u32 => Some((true, true, 3)),
                Some(x) if x == '<' as u32 && self.s.get(self.i + 3) == Some(&('=' as u32)) => Some((false, false, 4)),
                Some(x) if x == '<' as u32 && self.s.get(self.i + 3) == Some(&('!' as u32)) => Some((false, true, 4)),
                _ => None,
            };
            if let Some((ahead, neg, skip)) = look {
                self.i += skip;
                let inner = self.disjunction()?;
                if !self.eat(')') {
                    return Err("unterminated group".into());
                }
                let node = Node::Look(Box::new(inner), ahead, neg);
                // Annex B: a lookahead may take a quantifier
                if ahead && !self.unicode {
                    return self.quantifier(node);
                }
                return Ok(node);
            }
        }
        let atom = self.atom()?;
        self.quantifier(atom)
    }

    fn quantifier(&mut self, atom: Node) -> Result<Node, String> {
        let (min, max) = match self.peek() {
            Some(c) if c == '*' as u32 => {
                self.i += 1;
                (0, None)
            },
            Some(c) if c == '+' as u32 => {
                self.i += 1;
                (1, None)
            },
            Some(c) if c == '?' as u32 => {
                self.i += 1;
                (0, Some(1))
            },
            Some(c) if c == '{' as u32 => match self.braces() {
                Some((a, b, len)) => {
                    self.i += len;
                    (a, b)
                },
                None => {
                    if self.unicode {
                        return Err("incomplete quantifier".into());
                    }
                    return Ok(atom);
                },
            },
            _ => return Ok(atom),
        };
        let greedy = !self.eat('?');
        if matches!(atom, Node::Start | Node::End | Node::WordB(_)) {
            return Err("nothing to repeat".into());
        }
        if let Some(m) = max {
            if m < min {
                return Err("numbers out of order in {} quantifier".into());
            }
        }
        Ok(Node::Repeat(Box::new(atom), min, max, greedy))
    }

    /// {n}, {n,}, {n,m} at self.i: (min, max, length)
    fn braces(&self) -> Option<(u32, Option<u32>, usize)> {
        let mut j = self.i + 1;
        let num = |j: &mut usize| -> Option<u32> {
            let st = *j;
            let mut v: u64 = 0;
            while let Some(&c) = self.s.get(*j) {
                if (48..=57).contains(&c) {
                    v = (v * 10 + (c - 48) as u64).min(u32::MAX as u64);
                    *j += 1;
                } else {
                    break;
                }
            }
            if *j == st { None } else { Some(v as u32) }
        };
        let a = num(&mut j)?;
        let mut b = Some(a);
        if self.s.get(j) == Some(&(',' as u32)) {
            j += 1;
            b = num(&mut j);
        }
        if self.s.get(j) != Some(&('}' as u32)) {
            return None;
        }
        Some((a, b, j + 1 - self.i))
    }

    fn atom(&mut self) -> Result<Node, String> {
        let c = self.peek().unwrap();
        self.i += 1;
        match char::from_u32(c).unwrap_or('\0') {
            '.' => Ok(Node::Any),
            '(' => {
                let mut cap = None;
                if self.eat('?') {
                    if self.eat(':') {
                    } else if self.eat('<') {
                        let mut name = String::new();
                        while let Some(c) = self.peek() {
                            self.i += 1;
                            if c == '>' as u32 {
                                break;
                            }
                            name.push(char::from_u32(c).unwrap_or('?'));
                        }
                        self.ncaps += 1;
                        cap = Some(self.ncaps);
                        self.names.push((name, self.ncaps));
                    } else {
                        return Err("invalid group".into());
                    }
                } else {
                    self.ncaps += 1;
                    cap = Some(self.ncaps);
                }
                let inner = self.disjunction()?;
                if !self.eat(')') {
                    return Err("unterminated group".into());
                }
                Ok(Node::Group(Box::new(inner), cap))
            },
            ')' => Err("unmatched ')'".into()),
            '[' => self.class(),
            '\\' => self.escape(false).map(|e| match e {
                Esc::Char(c) => Node::Char(c),
                Esc::Class(r) => Node::Class(Class { ranges: r, neg: false }),
                Esc::Backref(n) => Node::Backref(n),
                Esc::Named(n) => Node::NamedBackref(n),
            }),
            '*' | '+' | '?' => Err("nothing to repeat".into()),
            '{' if self.unicode => Err("lone quantifier brackets".into()),
            _ => {
                // a surrogate pair in u mode is one character
                if self.unicode && (0xD800..0xDC00).contains(&c) {
                    if let Some(lo) = self.peek() {
                        if (0xDC00..0xE000).contains(&lo) {
                            self.i += 1;
                            return Ok(Node::Char(0x10000 + ((c - 0xD800) << 10) + (lo - 0xDC00)));
                        }
                    }
                }
                Ok(Node::Char(c))
            },
        }
    }

    fn class(&mut self) -> Result<Node, String> {
        let neg = self.eat('^');
        let mut ranges = Vec::new();
        loop {
            let c = match self.peek() {
                None => return Err("unterminated character class".into()),
                Some(c) => c,
            };
            if c == ']' as u32 {
                self.i += 1;
                break;
            }
            let lo = self.class_atom()?;
            if self.peek() == Some('-' as u32) && self.s.get(self.i + 1) != Some(&(']' as u32)) && self.s.get(self.i + 1).is_some() {
                self.i += 1;
                let hi = self.class_atom()?;
                match (lo, hi) {
                    (Ok(a), Ok(b)) => {
                        if b < a {
                            return Err("range out of order in character class".into());
                        }
                        ranges.push((a, b));
                    },
                    (lo, hi) => {
                        if self.unicode {
                            return Err("invalid character class".into());
                        }
                        for x in [lo, Ok('-' as u32), hi] {
                            match x {
                                Ok(c) => ranges.push((c, c)),
                                Err(r) => ranges.extend(r),
                            }
                        }
                    },
                }
            } else {
                match lo {
                    Ok(c) => ranges.push((c, c)),
                    Err(r) => ranges.extend(r),
                }
            }
        }
        Ok(Node::Class(Class { ranges, neg }))
    }

    /// one class member: Ok(char) or Err(ranges) for \d \w \s ...
    fn class_atom(&mut self) -> Result<Result<u32, Vec<(u32, u32)>>, String> {
        let c = self.peek().unwrap();
        self.i += 1;
        if c == '\\' as u32 {
            if self.peek() == Some('b' as u32) {
                self.i += 1;
                return Ok(Ok(8));
            }
            if self.peek() == Some('-' as u32) {
                self.i += 1;
                return Ok(Ok('-' as u32));
            }
            return match self.escape(true)? {
                Esc::Char(c) => Ok(Ok(c)),
                Esc::Class(r) => Ok(Err(r)),
                _ => Ok(Ok(0)),
            };
        }
        if self.unicode && (0xD800..0xDC00).contains(&c) {
            if let Some(lo) = self.peek() {
                if (0xDC00..0xE000).contains(&lo) {
                    self.i += 1;
                    return Ok(Ok(0x10000 + ((c - 0xD800) << 10) + (lo - 0xDC00)));
                }
            }
        }
        Ok(Ok(c))
    }

    fn hex(&mut self, n: usize) -> Option<u32> {
        let mut v = 0;
        for k in 0..n {
            let c = char::from_u32(*self.s.get(self.i + k)?)?.to_digit(16)?;
            v = v * 16 + c;
        }
        self.i += n;
        Some(v)
    }

    /// after a backslash
    fn escape(&mut self, in_class: bool) -> Result<Esc, String> {
        let c = match self.peek() {
            Some(c) => c,
            None => return Err("\\ at end of pattern".into()),
        };
        self.i += 1;
        let ch = char::from_u32(c).unwrap_or('\0');
        Ok(match ch {
            'd' => Esc::Class(DIGIT.to_vec()),
            'D' => Esc::Class(negate(DIGIT)),
            'w' => Esc::Class(WORD.to_vec()),
            'W' => Esc::Class(negate(WORD)),
            's' => Esc::Class(SPACE.to_vec()),
            'S' => Esc::Class(negate(SPACE)),
            'n' => Esc::Char(10),
            'r' => Esc::Char(13),
            't' => Esc::Char(9),
            'v' => Esc::Char(11),
            'f' => Esc::Char(12),
            '0' if !self.peek().is_some_and(|d| (48..=57).contains(&d)) => Esc::Char(0),
            'c' => match self.peek() {
                Some(l) if (65..=90).contains(&l) || (97..=122).contains(&l) => {
                    self.i += 1;
                    Esc::Char(l % 32)
                },
                _ => {
                    self.i -= 1;
                    Esc::Char('\\' as u32)
                },
            },
            'x' => {
                let save = self.i;
                match self.hex(2) {
                    Some(v) => Esc::Char(v),
                    None => {
                        self.i = save;
                        Esc::Char('x' as u32)
                    },
                }
            },
            'u' => {
                let save = self.i;
                if self.unicode && self.peek() == Some('{' as u32) {
                    self.i += 1;
                    let mut v: u32 = 0;
                    while let Some(d) = self.peek().and_then(|d| char::from_u32(d)).and_then(|d| d.to_digit(16)) {
                        v = v.saturating_mul(16).saturating_add(d);
                        self.i += 1;
                    }
                    if !self.eat('}') || v > 0x10FFFF {
                        return Err("invalid unicode escape".into());
                    }
                    Esc::Char(v)
                } else {
                    match self.hex(4) {
                        Some(hi) => {
                            // 😀 in u mode is one character
                            if self.unicode && (0xD800..0xDC00).contains(&hi) && self.s.get(self.i) == Some(&('\\' as u32)) && self.s.get(self.i + 1) == Some(&('u' as u32)) {
                                let s2 = self.i;
                                self.i += 2;
                                match self.hex(4) {
                                    Some(lo) if (0xDC00..0xE000).contains(&lo) => Esc::Char(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)),
                                    _ => {
                                        self.i = s2;
                                        Esc::Char(hi)
                                    },
                                }
                            } else {
                                Esc::Char(hi)
                            }
                        },
                        None => {
                            if self.unicode {
                                return Err("invalid unicode escape".into());
                            }
                            self.i = save;
                            Esc::Char('u' as u32)
                        },
                    }
                }
            },
            'k' if self.peek() == Some('<' as u32) && !in_class => {
                self.i += 1;
                let mut name = String::new();
                while let Some(c) = self.peek() {
                    self.i += 1;
                    if c == '>' as u32 {
                        break;
                    }
                    name.push(char::from_u32(c).unwrap_or('?'));
                }
                Esc::Named(name)
            },
            '1'..='9' => {
                // a back reference, or (Annex B) an octal escape / the digit itself
                let mut n = c - 48;
                while let Some(d) = self.peek() {
                    if (48..=57).contains(&d) {
                        n = n * 10 + (d - 48);
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                if !in_class {
                    Esc::Backref(n as usize)
                } else {
                    Esc::Char(n)
                }
            },
            '0' => {
                let mut v = 0;
                self.i -= 1;
                for _ in 0..3 {
                    match self.peek() {
                        Some(d) if (48..=55).contains(&d) && v * 8 + (d - 48) < 256 => {
                            v = v * 8 + (d - 48);
                            self.i += 1;
                        },
                        _ => break,
                    }
                }
                Esc::Char(v)
            },
            'p' | 'P' if self.unicode => {
                // \p{...}: a rough version (letters / numbers / whitespace); exact tables later
                let mut name = String::new();
                if self.eat('{') {
                    while let Some(c) = self.peek() {
                        self.i += 1;
                        if c == '}' as u32 {
                            break;
                        }
                        name.push(char::from_u32(c).unwrap_or('?'));
                    }
                }
                let r = unicode_property(&name);
                Esc::Class(if ch == 'P' { negate(&r) } else { r })
            },
            _ => Esc::Char(c),
        })
    }
}

enum Esc {
    Char(u32),
    Class(Vec<(u32, u32)>),
    Backref(usize),
    Named(String),
}

fn unicode_property(name: &str) -> Vec<(u32, u32)> {
    let n = name.split('=').last().unwrap_or(name);
    match n {
        "L" | "Letter" | "Alphabetic" | "Alpha" => vec![(65, 90), (97, 122), (0xAA, 0xAA), (0xB5, 0xB5), (0xBA, 0xBA), (0xC0, 0xD6), (0xD8, 0xF6), (0xF8, 0x2FF), (0x370, 0x3FF), (0x400, 0x52F), (0x531, 0x58F), (0x5D0, 0x5EA), (0x620, 0x64A), (0x900, 0x97F), (0xE00, 0xE7F), (0x1100, 0x11FF), (0x3040, 0x30FF), (0x3400, 0x9FFF), (0xAC00, 0xD7AF), (0x20000, 0x2FFFF)],
        "Lu" | "Uppercase_Letter" | "Uppercase" => vec![(65, 90), (0xC0, 0xD6), (0xD8, 0xDE), (0x391, 0x3A9), (0x410, 0x42F)],
        "Ll" | "Lowercase_Letter" | "Lowercase" => vec![(97, 122), (0xDF, 0xF6), (0xF8, 0xFF), (0x3B1, 0x3C9), (0x430, 0x44F)],
        "N" | "Nd" | "Number" | "Decimal_Number" => vec![(48, 57), (0x660, 0x669), (0x966, 0x96F), (0xFF10, 0xFF19)],
        "White_Space" | "space" => SPACE.to_vec(),
        "Emoji" | "Emoji_Presentation" | "Extended_Pictographic" => vec![(0x231A, 0x231B), (0x2600, 0x27BF), (0x1F300, 0x1FAFF)],
        "Script=Latin" | "Latin" | "Latn" => vec![(65, 90), (97, 122), (0xC0, 0x24F)],
        "Han" | "Hani" => vec![(0x2E80, 0x2FDF), (0x3005, 0x3007), (0x3400, 0x9FFF), (0xF900, 0xFAFF), (0x20000, 0x2FFFF)],
        "Hiragana" => vec![(0x3041, 0x309F)],
        "Katakana" => vec![(0x30A0, 0x30FF)],
        "Hangul" => vec![(0x1100, 0x11FF), (0x3130, 0x318F), (0xAC00, 0xD7AF)],
        "Cyrillic" => vec![(0x400, 0x52F)],
        "Greek" => vec![(0x370, 0x3FF)],
        "Arabic" => vec![(0x600, 0x6FF)],
        "Hebrew" => vec![(0x591, 0x5F4)],
        "P" | "Punctuation" => vec![(33, 35), (37, 42), (44, 47), (58, 59), (63, 64), (91, 93), (95, 95), (123, 123), (125, 125), (0x2010, 0x2027), (0x3000, 0x303F)],
        _ => vec![(0x10FFFF, 0x10FFFF)],
    }
}

// ── compiling ──

struct Gen {
    prog: Vec<Inst>,
    subs: Vec<Vec<Inst>>,
    classes: Vec<Class>,
    names: Vec<(String, usize)>,
    nregs: usize,
}

const MAX_EXPAND: u32 = 1000;

impl Gen {
    fn emit(&mut self, i: Inst) -> usize {
        self.prog.push(i);
        self.prog.len() - 1
    }

    fn can_be_empty(n: &Node) -> bool {
        match n {
            Node::Empty | Node::Start | Node::End | Node::WordB(_) | Node::Look(..) | Node::Backref(_) | Node::NamedBackref(_) => true,
            Node::Char(_) | Node::Any | Node::Class(_) => false,
            Node::Group(x, _) => Self::can_be_empty(x),
            Node::Cat(v) => v.iter().all(Self::can_be_empty),
            Node::Alt(v) => v.iter().any(Self::can_be_empty),
            Node::Repeat(x, min, _, _) => *min == 0 || Self::can_be_empty(x),
        }
    }

    fn node(&mut self, n: &Node) -> Result<(), String> {
        match n {
            Node::Empty => {},
            Node::Char(c) => {
                self.emit(Inst::Char(*c));
            },
            Node::Any => {
                self.emit(Inst::Any);
            },
            Node::Class(c) => {
                self.classes.push(c.clone());
                let i = self.classes.len() - 1;
                self.emit(Inst::Class(i));
            },
            Node::Start => {
                self.emit(Inst::Start);
            },
            Node::End => {
                self.emit(Inst::End);
            },
            Node::WordB(b) => {
                self.emit(Inst::WordB(*b));
            },
            Node::Group(x, cap) => {
                if let Some(c) = cap {
                    self.emit(Inst::Save(c * 2));
                }
                self.node(x)?;
                if let Some(c) = cap {
                    self.emit(Inst::Save(c * 2 + 1));
                }
            },
            Node::Cat(v) => {
                for x in v {
                    self.node(x)?;
                }
            },
            Node::Alt(v) => {
                let mut ends = Vec::new();
                for (k, x) in v.iter().enumerate() {
                    if k + 1 < v.len() {
                        let split = self.emit(Inst::Split(0, 0));
                        self.node(x)?;
                        ends.push(self.emit(Inst::Jmp(0)));
                        let next = self.prog.len();
                        self.prog[split] = Inst::Split(split + 1, next);
                    } else {
                        self.node(x)?;
                    }
                }
                let end = self.prog.len();
                for e in ends {
                    self.prog[e] = Inst::Jmp(end);
                }
            },
            Node::Repeat(x, min, max, greedy) => {
                let min = *min;
                if min > MAX_EXPAND || max.is_some_and(|m| m > MAX_EXPAND && m != min) {
                    // very large counts: cap them (the web rarely relies on more)
                }
                let min = min.min(MAX_EXPAND);
                for _ in 0..min {
                    self.node(&clear_caps_none(x))?;
                }
                match max {
                    None => {
                        // x*  : L: split(body, out); [mark] body [progress]; jmp L
                        let guard = Self::can_be_empty(x);
                        let reg = if guard {
                            self.nregs += 1;
                            Some(self.nregs - 1)
                        } else {
                            None
                        };
                        let l = self.emit(Inst::Split(0, 0));
                        if let Some(r) = reg {
                            self.emit(Inst::Mark(r));
                        }
                        self.node(x)?;
                        if let Some(r) = reg {
                            self.emit(Inst::Progress(r));
                        }
                        self.emit(Inst::Jmp(l));
                        let out = self.prog.len();
                        self.prog[l] = if *greedy { Inst::Split(l + 1, out) } else { Inst::Split(out, l + 1) };
                    },
                    Some(m) => {
                        let extra = (*m).min(MAX_EXPAND).saturating_sub(min);
                        let mut splits = Vec::new();
                        for _ in 0..extra {
                            splits.push(self.emit(Inst::Split(0, 0)));
                            self.node(x)?;
                        }
                        let out = self.prog.len();
                        for s in splits {
                            self.prog[s] = if *greedy { Inst::Split(s + 1, out) } else { Inst::Split(out, s + 1) };
                        }
                    },
                }
            },
            Node::Backref(n) => {
                self.emit(Inst::Backref(*n));
            },
            Node::NamedBackref(name) => {
                let n = self.names.iter().find(|(k, _)| k == name).map(|(_, v)| *v).ok_or_else(|| format!("invalid named reference '{}'", name))?;
                self.emit(Inst::Backref(n));
            },
            Node::Look(x, ahead, neg) => {
                let saved = core::mem::take(&mut self.prog);
                self.node(x)?;
                self.emit(Inst::Match);
                let sub = core::mem::replace(&mut self.prog, saved);
                self.subs.push(sub);
                let i = self.subs.len() - 1;
                self.emit(Inst::Look(i, *ahead, *neg));
            },
        }
        Ok(())
    }
}

/// (a repeated group is compiled more than once; captures stay the same numbers)
fn clear_caps_none(n: &Node) -> Node {
    n.clone()
}

impl Regex {
    pub fn new(pattern: &[u16], flags: &str) -> Result<Regex, String> {
        let fl = parse_flags(flags)?;
        let units: Vec<u32> = pattern.iter().map(|&u| u as u32).collect();
        let mut p = P { s: &units, i: 0, ncaps: 0, names: Vec::new(), unicode: fl.unicode };
        let node = p.disjunction()?;
        if p.i < units.len() {
            return Err("unmatched ')'".into());
        }
        let ncaps = p.ncaps;
        // back references to groups that do not exist are octal escapes (Annex B)
        let node = fix_backrefs(node, ncaps, fl.unicode)?;
        let mut g = Gen { prog: Vec::new(), subs: Vec::new(), classes: Vec::new(), names: p.names.clone(), nregs: 0 };
        g.emit(Inst::Save(0));
        g.node(&node)?;
        g.emit(Inst::Save(1));
        g.emit(Inst::Match);
        Ok(Regex { prog: g.prog, subs: g.subs, classes: g.classes, ncaps: ncaps + 1, names: p.names, flags: fl, nregs: g.nregs })
    }

    /// a match at or after `start` (only at `start` when sticky): capture positions in code units
    pub fn exec(&self, s: &[u16], start: usize) -> Result<Option<Vec<Option<(usize, usize)>>>, String> {
        let mut steps: u64 = 0;
        let mut pos = start;
        while pos <= s.len() {
            let mut caps = vec![usize::MAX; self.ncaps * 2];
            let mut regs = vec![usize::MAX; self.nregs];
            if self.run(&self.prog, s, pos, &mut caps, &mut regs, &mut steps)?.is_some() {
                let out = (0..self.ncaps)
                    .map(|k| {
                        let (a, b) = (caps[k * 2], caps[k * 2 + 1]);
                        if a == usize::MAX || b == usize::MAX { None } else { Some((a, b)) }
                    })
                    .collect();
                return Ok(Some(out));
            }
            if self.flags.sticky {
                break;
            }
            pos += 1;
            // in u mode never start inside a surrogate pair
            if self.flags.unicode && pos < s.len() && (0xDC00..0xE000).contains(&s[pos]) && (0xD800..0xDC00).contains(&s[pos - 1]) {
                pos += 1;
            }
        }
        Ok(None)
    }

    fn char_at(&self, s: &[u16], i: usize) -> Option<(u32, usize)> {
        let c = *s.get(i)? as u32;
        if self.flags.unicode && (0xD800..0xDC00).contains(&c) {
            if let Some(&lo) = s.get(i + 1) {
                if (0xDC00..0xE000).contains(&(lo as u32)) {
                    return Some((0x10000 + ((c - 0xD800) << 10) + (lo as u32 - 0xDC00), 2));
                }
            }
        }
        Some((c, 1))
    }

    fn char_before(&self, s: &[u16], i: usize) -> Option<(u32, usize)> {
        if i == 0 {
            return None;
        }
        let c = s[i - 1] as u32;
        if self.flags.unicode && (0xDC00..0xE000).contains(&c) && i >= 2 {
            let hi = s[i - 2] as u32;
            if (0xD800..0xDC00).contains(&hi) {
                return Some((0x10000 + ((hi - 0xD800) << 10) + (c - 0xDC00), 2));
            }
        }
        Some((c, 1))
    }

    fn fold(&self, c: u32) -> u32 {
        if !self.flags.ignore_case {
            return c;
        }
        if c < 128 {
            return if (65..=90).contains(&c) { c + 32 } else { c };
        }
        match char::from_u32(c) {
            Some(ch) => {
                let mut l = ch.to_lowercase();
                let first = l.next().unwrap_or(ch);
                if l.next().is_some() { c } else { first as u32 }
            },
            None => c,
        }
    }

    fn class_has(&self, cl: &Class, c: u32) -> bool {
        let inr = |x: u32| cl.ranges.iter().any(|&(a, b)| x >= a && x <= b);
        let mut hit = inr(c);
        if !hit && self.flags.ignore_case {
            let l = self.fold(c);
            hit = inr(l);
            if !hit {
                if let Some(ch) = char::from_u32(c) {
                    let mut u = ch.to_uppercase();
                    if let (Some(x), None) = (u.next(), u.next()) {
                        hit = inr(x as u32);
                    }
                }
            }
        }
        hit != cl.neg
    }

    fn is_word(&self, s: &[u16], i: usize) -> bool {
        match s.get(i) {
            Some(&c) => {
                let c = c as u32;
                (48..=57).contains(&c) || (65..=90).contains(&c) || (97..=122).contains(&c) || c == 95
            },
            None => false,
        }
    }

    /// runs a program from `pos`; Some(end) on a match
    fn run(&self, prog: &[Inst], s: &[u16], pos: usize, caps: &mut [usize], regs: &mut [usize], steps: &mut u64) -> Result<Option<usize>, String> {
        enum Bt {
            Alt(usize, usize),
            Cap(usize, usize),
            Reg(usize, usize),
        }
        let mut stack: Vec<Bt> = Vec::new();
        let mut pc = 0usize;
        let mut i = pos;
        loop {
            *steps += 1;
            if *steps > 20_000_000 {
                return Err("regular expression too complex for this input".into());
            }
            let ok = match &prog[pc] {
                Inst::Match => return Ok(Some(i)),
                Inst::Char(c) => match self.char_at(s, i) {
                    Some((x, n)) if x == *c || (self.flags.ignore_case && self.fold(x) == self.fold(*c)) => {
                        i += n;
                        pc += 1;
                        true
                    },
                    _ => false,
                },
                Inst::Any => match self.char_at(s, i) {
                    Some((x, n)) if self.flags.dot_all || !(x == 10 || x == 13 || x == 0x2028 || x == 0x2029) => {
                        i += n;
                        pc += 1;
                        true
                    },
                    _ => false,
                },
                Inst::Class(k) => match self.char_at(s, i) {
                    Some((x, n)) if self.class_has(&self.classes[*k], x) => {
                        i += n;
                        pc += 1;
                        true
                    },
                    _ => false,
                },
                Inst::Start => {
                    let ok = i == 0 || (self.flags.multiline && matches!(s[i - 1], 10 | 13 | 0x2028 | 0x2029));
                    pc += 1;
                    ok
                },
                Inst::End => {
                    let ok = i == s.len() || (self.flags.multiline && matches!(s[i], 10 | 13 | 0x2028 | 0x2029));
                    pc += 1;
                    ok
                },
                Inst::WordB(want) => {
                    let a = i > 0 && self.is_word(s, i - 1);
                    let b = self.is_word(s, i);
                    pc += 1;
                    (a != b) == *want
                },
                Inst::Save(k) => {
                    stack.push(Bt::Cap(*k, caps[*k]));
                    caps[*k] = i;
                    pc += 1;
                    true
                },
                Inst::Split(a, b) => {
                    stack.push(Bt::Alt(*b, i));
                    pc = *a;
                    true
                },
                Inst::Jmp(a) => {
                    pc = *a;
                    true
                },
                Inst::Mark(r) => {
                    stack.push(Bt::Reg(*r, regs[*r]));
                    regs[*r] = i;
                    pc += 1;
                    true
                },
                Inst::Progress(r) => {
                    pc += 1;
                    regs[*r] != i
                },
                Inst::Backref(n) => {
                    let (a, b) = (caps.get(n * 2).copied().unwrap_or(usize::MAX), caps.get(n * 2 + 1).copied().unwrap_or(usize::MAX));
                    if a == usize::MAX || b == usize::MAX {
                        pc += 1;
                        true
                    } else {
                        let len = b - a;
                        if i + len <= s.len() && (0..len).all(|k| s[a + k] == s[i + k] || self.fold(s[a + k] as u32) == self.fold(s[i + k] as u32)) {
                            i += len;
                            pc += 1;
                            true
                        } else {
                            false
                        }
                    }
                },
                Inst::Look(k, ahead, neg) => {
                    let sub = &self.subs[*k];
                    let mut c2 = caps.to_vec();
                    let mut r2 = regs.to_vec();
                    let found = if *ahead {
                        self.run(sub, s, i, &mut c2, &mut r2, steps)?.is_some()
                    } else {
                        // behind: some start j <= i whose match ends exactly at i
                        let mut hit = false;
                        let mut j = i as isize;
                        while j >= 0 {
                            let mut c3 = caps.to_vec();
                            let mut r3 = regs.to_vec();
                            if let Some(e) = self.run(sub, s, j as usize, &mut c3, &mut r3, steps)? {
                                if e == i {
                                    c2 = c3;
                                    hit = true;
                                    break;
                                }
                            }
                            j -= 1;
                        }
                        hit
                    };
                    pc += 1;
                    if found && !*neg {
                        // a positive lookaround keeps its captures
                        for (idx, v) in c2.iter().enumerate() {
                            if caps[idx] != *v {
                                stack.push(Bt::Cap(idx, caps[idx]));
                                caps[idx] = *v;
                            }
                        }
                    }
                    found != *neg
                },
            };
            if !ok {
                // backtrack
                loop {
                    match stack.pop() {
                        None => return Ok(None),
                        Some(Bt::Cap(k, v)) => caps[k] = v,
                        Some(Bt::Reg(k, v)) => regs[k] = v,
                        Some(Bt::Alt(p, at)) => {
                            pc = p;
                            i = at;
                            break;
                        },
                    }
                }
            }
        }
    }
}

fn fix_backrefs(n: Node, ncaps: usize, unicode: bool) -> Result<Node, String> {
    Ok(match n {
        Node::Backref(k) if k > ncaps => {
            if unicode {
                return Err("invalid escape".into());
            }
            // \8 -> "8"; \12 with < 12 groups -> octal
            let ds = format!("{}", k);
            let mut v: u32 = 0;
            let mut out = Vec::new();
            let mut octal = true;
            for c in ds.chars() {
                let d = c.to_digit(10).unwrap();
                if d > 7 {
                    octal = false;
                }
                let _ = &mut v;
                out.push(Node::Char(c as u32));
            }
            if octal {
                let mut v = 0u32;
                for c in ds.chars() {
                    v = v * 8 + c.to_digit(8).unwrap();
                }
                if v < 256 {
                    return Ok(Node::Char(v));
                }
            }
            Node::Cat(out)
        },
        Node::Group(x, c) => Node::Group(Box::new(fix_backrefs(*x, ncaps, unicode)?), c),
        Node::Cat(v) => Node::Cat(v.into_iter().map(|x| fix_backrefs(x, ncaps, unicode)).collect::<Result<_, _>>()?),
        Node::Alt(v) => Node::Alt(v.into_iter().map(|x| fix_backrefs(x, ncaps, unicode)).collect::<Result<_, _>>()?),
        Node::Repeat(x, a, b, g) => Node::Repeat(Box::new(fix_backrefs(*x, ncaps, unicode)?), a, b, g),
        Node::Look(x, a, b) => Node::Look(Box::new(fix_backrefs(*x, ncaps, unicode)?), a, b),
        other => other,
    })
}

impl core::fmt::Debug for Regex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Regex({} insts)", self.prog.len())
    }
}
