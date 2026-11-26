//! The tokenizer. The parser drives it one token at a time and re-scans where the grammar
//! decides what a character means: a `/` that starts a regular expression, and the `}` that
//! continues a template literal.

use crate::strs::{from_units, JsStr};
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Num(f64),
    /// a BigInt literal, as decimal digits
    BigInt(String),
    Str(JsStr),
    /// a piece of a template literal: cooked text (None when an escape is invalid), raw text,
    /// and whether it ends the literal (`...` or `}...`) instead of opening `${`
    Template(Option<JsStr>, JsStr, bool),
    /// an identifier or keyword (`escaped` when it was spelled with \u escapes: never a keyword)
    Ident(String, bool),
    PrivateName(String),
    Punct(&'static str),
    Regex(String, String),
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub nl_before: bool,
    pub start: usize,
    pub end: usize,
    pub line: u32,
}

#[derive(Clone)]
pub struct Lexer {
    src: alloc::rc::Rc<[char]>,
    pub pos: usize,
    pub line: u32,
}

const PUNCTS: &[&str] = &[
    ">>>=", "...", "===", "!==", "**=", "<<=", ">>=", ">>>", "&&=", "||=", "??=", "=>", "==", "!=", "<=", ">=", "&&", "||",
    "??", "?.", "++", "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<", ">>", "**", "{", "}", "(", ")", "[", "]",
    ";", ",", "<", ">", "+", "-", "*", "/", "%", "&", "|", "^", "!", "~", "?", ":", "=", ".", "@",
];

pub type LexResult<T> = Result<T, String>;

fn is_id_start(c: char) -> bool {
    c == '$' || c == '_' || c.is_ascii_alphabetic() || (c as u32 > 127 && c.is_alphabetic())
}
fn is_id_part(c: char) -> bool {
    is_id_start(c) || c.is_ascii_digit() || c == '\u{200C}' || c == '\u{200D}' || (c as u32 > 127 && c.is_alphanumeric())
}
fn is_line_term(c: char) -> bool {
    c == '\n' || c == '\r' || c == '\u{2028}' || c == '\u{2029}'
}

impl Lexer {
    pub fn new(src: &str) -> Self {
        let v: Vec<char> = src.chars().collect();
        Lexer { src: v.into(), pos: 0, line: 1 }
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }
    fn at(&self, i: usize) -> Option<char> {
        self.src.get(self.pos + i).copied()
    }

    pub fn slice(&self, a: usize, b: usize) -> String {
        self.src[a.min(self.src.len())..b.min(self.src.len())].iter().collect()
    }

    /// skips white space and comments; true when a line break was passed
    fn skip_ws(&mut self) -> LexResult<bool> {
        let mut nl = false;
        while let Some(c) = self.peek() {
            if is_line_term(c) {
                nl = true;
                if c == '\n' || c == '\u{2028}' || c == '\u{2029}' || (c == '\r' && self.at(1) != Some('\n')) {
                    self.line += 1;
                }
                self.pos += 1;
            } else if c == ' ' || c == '\t' || c == '\u{b}' || c == '\u{c}' || c == '\u{a0}' || c == '\u{feff}' || (c as u32 > 127 && c.is_whitespace()) {
                self.pos += 1;
            } else if c == '/' && self.at(1) == Some('/') {
                while let Some(c) = self.peek() {
                    if is_line_term(c) {
                        break;
                    }
                    self.pos += 1;
                }
            } else if c == '/' && self.at(1) == Some('*') {
                self.pos += 2;
                loop {
                    match self.peek() {
                        None => return Err("unterminated comment".into()),
                        Some('*') if self.at(1) == Some('/') => {
                            self.pos += 2;
                            break;
                        },
                        Some(c) => {
                            if is_line_term(c) {
                                nl = true;
                                if c == '\n' {
                                    self.line += 1;
                                }
                            }
                            self.pos += 1;
                        },
                    }
                }
            } else if c == '<' && self.at(1) == Some('!') && self.at(2) == Some('-') && self.at(3) == Some('-') {
                // <!-- an HTML comment in a script: the rest of the line
                while let Some(c) = self.peek() {
                    if is_line_term(c) {
                        break;
                    }
                    self.pos += 1;
                }
            } else if c == '-' && self.at(1) == Some('-') && self.at(2) == Some('>') && (nl || self.pos == 0) {
                while let Some(c) = self.peek() {
                    if is_line_term(c) {
                        break;
                    }
                    self.pos += 1;
                }
            } else if c == '#' && self.at(1) == Some('!') && self.pos == 0 {
                while let Some(c) = self.peek() {
                    if is_line_term(c) {
                        break;
                    }
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
        Ok(nl)
    }

    pub fn next_token(&mut self) -> LexResult<Token> {
        let nl = self.skip_ws()?;
        let start = self.pos;
        let line = self.line;
        let tok = self.scan()?;
        Ok(Token { tok, nl_before: nl, start, end: self.pos, line })
    }

    fn scan(&mut self) -> LexResult<Tok> {
        let c = match self.peek() {
            None => return Ok(Tok::Eof),
            Some(c) => c,
        };
        if is_id_start(c) || c == '\\' {
            let (name, esc) = self.ident()?;
            return Ok(Tok::Ident(name, esc));
        }
        if c == '#' {
            self.pos += 1;
            let (name, _) = self.ident()?;
            return Ok(Tok::PrivateName(name));
        }
        if c.is_ascii_digit() || (c == '.' && self.at(1).is_some_and(|d| d.is_ascii_digit())) {
            return self.number();
        }
        if c == '"' || c == '\'' {
            return self.string(c);
        }
        if c == '`' {
            self.pos += 1;
            return self.template();
        }
        for p in PUNCTS {
            let n = p.len();
            if p.chars().enumerate().all(|(i, pc)| self.at(i) == Some(pc)) {
                // `?.` followed by a digit is `?` then `.5`
                if *p == "?." && self.at(2).is_some_and(|d| d.is_ascii_digit()) {
                    continue;
                }
                self.pos += n;
                return Ok(Tok::Punct(p));
            }
        }
        Err(alloc::format!("unexpected character '{}'", c))
    }

    fn ident(&mut self) -> LexResult<(String, bool)> {
        let mut s = String::new();
        let mut esc = false;
        let mut first = true;
        while let Some(c) = self.peek() {
            if c == '\\' {
                if self.at(1) != Some('u') {
                    return Err("bad escape in identifier".into());
                }
                self.pos += 2;
                let cp = self.unicode_escape_body()?;
                let ch = char::from_u32(cp).ok_or("bad escape in identifier")?;
                s.push(ch);
                esc = true;
            } else if (first && is_id_start(c)) || (!first && is_id_part(c)) {
                s.push(c);
                self.pos += 1;
            } else {
                break;
            }
            first = false;
        }
        if s.is_empty() {
            return Err("expected an identifier".into());
        }
        Ok((s, esc))
    }

    /// after `\u`: XXXX or {X...}
    fn unicode_escape_body(&mut self) -> LexResult<u32> {
        if self.peek() == Some('{') {
            self.pos += 1;
            let mut v: u32 = 0;
            let mut n = 0;
            while let Some(c) = self.peek() {
                if c == '}' {
                    break;
                }
                let d = c.to_digit(16).ok_or("bad \\u{} escape")?;
                v = v.checked_mul(16).ok_or("bad \\u{} escape")? + d;
                n += 1;
                self.pos += 1;
            }
            if self.peek() != Some('}') || n == 0 || v > 0x10FFFF {
                return Err("bad \\u{} escape".into());
            }
            self.pos += 1;
            return Ok(v);
        }
        let mut v = 0;
        for _ in 0..4 {
            let d = self.peek().and_then(|c| c.to_digit(16)).ok_or("bad \\u escape")?;
            v = v * 16 + d;
            self.pos += 1;
        }
        Ok(v)
    }

    fn number(&mut self) -> LexResult<Tok> {
        let c = self.peek().unwrap();
        if c == '0' {
            let radix = match self.at(1) {
                Some('x') | Some('X') => 16,
                Some('o') | Some('O') => 8,
                Some('b') | Some('B') => 2,
                _ => 0,
            };
            if radix != 0 {
                self.pos += 2;
                let mut v: f64 = 0.0;
                let mut n = 0;
                let mut digits = String::new();
                while let Some(c) = self.peek() {
                    if c == '_' {
                        self.pos += 1;
                        continue;
                    }
                    match c.to_digit(radix) {
                        Some(d) => {
                            digits.push(c);
                            v = v * radix as f64 + d as f64;
                            n += 1;
                            self.pos += 1;
                        },
                        None => break,
                    }
                }
                if n == 0 {
                    return Err("missing digits after the number prefix".into());
                }
                if self.peek() == Some('n') {
                    self.pos += 1;
                    self.check_after_number()?;
                    let b = crate::bigint::BigInt::parse_radix(&digits, radix).ok_or("bad BigInt literal")?;
                    return Ok(Tok::BigInt(b.to_string_radix(10)));
                }
                self.check_after_number()?;
                return Ok(Tok::Num(v));
            }
            // legacy octal 017
            if self.at(1).is_some_and(|d| d.is_ascii_digit()) {
                let mut j = 1;
                let mut octal = true;
                while let Some(d) = self.at(j) {
                    if !d.is_ascii_digit() {
                        break;
                    }
                    if d >= '8' {
                        octal = false;
                    }
                    j += 1;
                }
                if octal && self.at(j) != Some('.') {
                    let mut v = 0.0;
                    for k in 1..j {
                        v = v * 8.0 + self.at(k).unwrap().to_digit(8).unwrap() as f64;
                    }
                    self.pos += j;
                    return Ok(Tok::Num(v));
                }
            }
        }
        let mut s = String::new();
        let mut seen_dot = false;
        let mut seen_e = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                s.push(c);
            } else if c == '_' {
            } else if c == '.' && !seen_dot && !seen_e {
                seen_dot = true;
                s.push(c);
            } else if (c == 'e' || c == 'E') && !seen_e {
                seen_e = true;
                s.push('e');
                if matches!(self.at(1), Some('+') | Some('-')) {
                    self.pos += 1;
                    s.push(self.peek().unwrap());
                }
            } else {
                break;
            }
            self.pos += 1;
        }
        if self.peek() == Some('n') && !seen_dot && !seen_e {
            self.pos += 1;
            self.check_after_number()?;
            return Ok(Tok::BigInt(s));
        }
        self.check_after_number()?;
        let v: f64 = s.parse().map_err(|_| alloc::format!("bad number '{}'", s))?;
        Ok(Tok::Num(v))
    }

    fn check_after_number(&self) -> LexResult<()> {
        if let Some(c) = self.peek() {
            if is_id_start(c) || c.is_ascii_digit() {
                return Err("an identifier starts right after a number".into());
            }
        }
        Ok(())
    }

    /// one escape after the backslash, appended to `out`; Err(()) for an invalid escape
    fn escape(&mut self, out: &mut Vec<u16>, template: bool) -> Result<(), ()> {
        let c = match self.peek() {
            Some(c) => c,
            None => return Err(()),
        };
        self.pos += 1;
        match c {
            'n' => out.push(10),
            't' => out.push(9),
            'r' => out.push(13),
            'b' => out.push(8),
            'f' => out.push(12),
            'v' => out.push(11),
            '0' if !self.peek().is_some_and(|d| d.is_ascii_digit()) => out.push(0),
            '1'..='7' | '0' => {
                if template {
                    return Err(());
                }
                // legacy octal escape
                let mut v = c.to_digit(8).unwrap();
                for _ in 0..2 {
                    match self.peek().and_then(|d| d.to_digit(8)) {
                        Some(d) if v * 8 + d < 256 => {
                            v = v * 8 + d;
                            self.pos += 1;
                        },
                        _ => break,
                    }
                }
                out.push(v as u16);
            },
            'x' => {
                let a = self.peek().and_then(|c| c.to_digit(16)).ok_or(())?;
                let b = self.at(1).and_then(|c| c.to_digit(16)).ok_or(())?;
                self.pos += 2;
                out.push((a * 16 + b) as u16);
            },
            'u' => {
                let cp = self.unicode_escape_body().map_err(|_| ())?;
                push_cp(out, cp);
            },
            '\r' => {
                if self.peek() == Some('\n') {
                    self.pos += 1;
                }
                self.line += 1;
            },
            '\n' | '\u{2028}' | '\u{2029}' => {
                self.line += 1;
            },
            other => push_cp(out, other as u32),
        }
        Ok(())
    }

    fn string(&mut self, q: char) -> LexResult<Tok> {
        self.pos += 1;
        let mut out: Vec<u16> = Vec::new();
        loop {
            match self.peek() {
                None => return Err("unterminated string".into()),
                Some(c) if c == q => {
                    self.pos += 1;
                    break;
                },
                Some('\\') => {
                    self.pos += 1;
                    self.escape(&mut out, false).map_err(|_| String::from("bad escape in string"))?;
                },
                Some('\n') | Some('\r') => return Err("unterminated string".into()),
                Some(c) => {
                    push_cp(&mut out, c as u32);
                    self.pos += 1;
                },
            }
        }
        Ok(Tok::Str(from_units(out)))
    }

    /// after "`" or the "}" that closes a substitution
    fn template(&mut self) -> LexResult<Tok> {
        let mut cooked: Vec<u16> = Vec::new();
        let mut raw: Vec<u16> = Vec::new();
        let mut valid = true;
        loop {
            match self.peek() {
                None => return Err("unterminated template".into()),
                Some('`') => {
                    self.pos += 1;
                    return Ok(Tok::Template(if valid { Some(from_units(cooked)) } else { None }, from_units(raw), true));
                },
                Some('$') if self.at(1) == Some('{') => {
                    self.pos += 2;
                    return Ok(Tok::Template(if valid { Some(from_units(cooked)) } else { None }, from_units(raw), false));
                },
                Some('\\') => {
                    let s0 = self.pos;
                    self.pos += 1;
                    if self.escape(&mut cooked, true).is_err() {
                        valid = false;
                    }
                    for c in self.src[s0..self.pos].iter() {
                        push_cp(&mut raw, *c as u32);
                    }
                },
                Some(c) => {
                    self.pos += 1;
                    let c = if c == '\r' {
                        if self.peek() == Some('\n') {
                            self.pos += 1;
                        }
                        '\n'
                    } else {
                        c
                    };
                    if c == '\n' {
                        self.line += 1;
                    }
                    push_cp(&mut cooked, c as u32);
                    push_cp(&mut raw, c as u32);
                },
            }
        }
    }

    /// the parser met `}` while inside `${ }`: read on as template text
    pub fn rescan_template(&mut self, tok: &Token) -> LexResult<Token> {
        self.pos = tok.start + 1;
        let t = self.template()?;
        Ok(Token { tok: t, nl_before: tok.nl_before, start: tok.start, end: self.pos, line: tok.line })
    }

    /// the parser met `/` or `/=` where an expression starts: it is a regular expression
    pub fn rescan_regex(&mut self, tok: &Token) -> LexResult<Token> {
        self.pos = tok.start + 1;
        let mut body = String::new();
        let mut in_class = false;
        loop {
            match self.peek() {
                None => return Err("unterminated regular expression".into()),
                Some(c) if is_line_term(c) => return Err("unterminated regular expression".into()),
                Some('\\') => {
                    body.push('\\');
                    self.pos += 1;
                    match self.peek() {
                        Some(c) if !is_line_term(c) => {
                            body.push(c);
                            self.pos += 1;
                        },
                        _ => return Err("unterminated regular expression".into()),
                    }
                },
                Some('[') => {
                    in_class = true;
                    body.push('[');
                    self.pos += 1;
                },
                Some(']') => {
                    in_class = false;
                    body.push(']');
                    self.pos += 1;
                },
                Some('/') if !in_class => {
                    self.pos += 1;
                    break;
                },
                Some(c) => {
                    body.push(c);
                    self.pos += 1;
                },
            }
        }
        let mut flags = String::new();
        while let Some(c) = self.peek() {
            if is_id_part(c) {
                flags.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        Ok(Token { tok: Tok::Regex(body, flags), nl_before: tok.nl_before, start: tok.start, end: self.pos, line: tok.line })
    }
}

pub fn push_cp(out: &mut Vec<u16>, cp: u32) {
    if cp >= 0x10000 {
        let c = cp - 0x10000;
        out.push(0xD800 + (c >> 10) as u16);
        out.push(0xDC00 + (c & 0x3FF) as u16);
    } else {
        out.push(cp as u16);
    }
}
