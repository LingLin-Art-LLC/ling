//! The `.lcron` automation language: one job per file.
//!
//! ```text
//! job "Screenshot to SFTP"
//!     every 2h between 08:00 and 22:00      # = cron 0 8-22/2 * * *
//!     at_boot  yes
//!     missed   skip
//! step shot   = screenshot(screen: "all")
//! step upload = sftp.put(shot, "ling@nas.local:/shots/{date}/",
//!                        key: keyring("nas"), retries: 3)
//! step clean  = delete(shot)               when upload.ok
//! on_fail  notify("Screenshot upload failed: {error}", sound: "soft")
//! on_ok    log("saved {shot.name} ({shot.size})")
//! ```
//!
//! The parser is strict and says where: every problem is `line N: ...`. It
//! never accepts a secret written into the file -- a `password:` / `key:` /
//! `token:` must be `keyring("name")`, a private key block or a password
//! inside an address is refused -- so a `.lcron` can be shared, synced or
//! committed without leaking anything.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::cron::{self, Cron, Trigger};

#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Str(String),
    Num(f64),
    Dur(u64),
    Bool(bool),
    /// Another step's result: `shot` or `shot.path`.
    Ref(String, String),
    /// `keyring("name")`: looked up in the owner's keyring at run time.
    Secret(String),
}

impl Val {
    pub fn show(&self) -> String {
        match self {
            Val::Str(s) => format!("\"{s}\""),
            Val::Num(n) => {
                if *n == (*n as i64) as f64 {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            },
            Val::Dur(d) => cron::fmt_duration(*d),
            Val::Bool(b) => String::from(if *b { "yes" } else { "no" }),
            Val::Ref(s, f) if f.is_empty() => format!("${s}"),
            Val::Ref(s, f) => format!("${s}.{f}"),
            Val::Secret(n) => format!("keyring(\"{n}\")"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Call {
    pub action: String,
    pub pos: Vec<Val>,
    pub named: Vec<(String, Val)>,
    pub line: usize,
}

impl Call {
    /// Positional argument `i`, or the named one.
    pub fn arg(&self, i: usize, name: &str) -> Option<&Val> {
        self.named.iter().find(|(n, _)| n == name).map(|(_, v)| v).or_else(|| {
            let spec = spec_of(&self.action)?;
            let p = spec.pos.iter().position(|n| *n == name)?;
            if p == i {
                self.pos.get(i)
            } else {
                None
            }
        })
    }
    pub fn named(&self, name: &str) -> Option<&Val> {
        self.named.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
}

#[derive(Clone, Debug)]
pub enum Cond {
    Ok(String),
    Failed(String),
    Skipped(String),
    Contains(String, String),
    Not(alloc::boxed::Box<Cond>),
}

impl Cond {
    pub fn show(&self) -> String {
        match self {
            Cond::Ok(s) => format!("{s}.ok"),
            Cond::Failed(s) => format!("{s}.failed"),
            Cond::Skipped(s) => format!("{s}.skipped"),
            Cond::Contains(s, t) => format!("{s}.out contains \"{t}\""),
            Cond::Not(c) => format!("not {}", c.show()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Step {
    pub name: String,
    pub call: Call,
    pub when: Option<Cond>,
    pub line: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Missed {
    Skip,
    CatchUp,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub name: String,
    pub trigger: Trigger,
    pub at_boot: bool,
    pub at_login: bool,
    pub missed: Missed,
    pub enabled: bool,
    /// Runs allowed per hour (the rate limit; the scheduler also keeps 60 s between runs).
    pub limit_per_hour: u32,
    pub steps: Vec<Step>,
    pub on_fail: Vec<Call>,
    pub on_ok: Vec<Call>,
}

#[derive(Clone, Debug)]
pub struct Error {
    pub line: usize,
    pub msg: String,
    /// A credential written into the file: such a file is never saved.
    pub secret: bool,
}

// ── actions ──────────────────────────────────────────────────────────────────

pub struct Spec {
    pub name: &'static str,
    pub pos: &'static [&'static str],
    pub required: usize,
    pub named: &'static [&'static str],
    /// Needs the owner's desktop session (screen, sound, notifications, apps).
    pub session: bool,
    pub net: bool,
    pub help: &'static str,
}

pub const SPECS: &[Spec] = &[
    Spec { name: "screenshot", pos: &[], required: 0, named: &["screen", "to"], session: true, net: false, help: "take a screenshot (PNG) -> .path .name .size" },
    Spec { name: "screenrec", pos: &["secs"], required: 0, named: &["secs", "to"], session: true, net: false, help: "record the screen for secs seconds -> .path" },
    Spec { name: "sftp.put", pos: &["file", "target"], required: 2, named: &["key", "password", "retries", "port", "timeout"], session: false, net: true, help: "upload a file over SFTP to user@host:/dir/" },
    Spec { name: "ssh.run", pos: &["target", "command"], required: 2, named: &["key", "password", "retries", "port", "timeout"], session: false, net: true, help: "run a command over SSH -> .out .code" },
    Spec { name: "shell", pos: &["command"], required: 1, named: &[], session: false, net: false, help: "run a LingOS shell command (hwinfo, hitches, report, df, ls ...) -> .out" },
    Spec { name: "app.open", pos: &["name"], required: 1, named: &[], session: true, net: false, help: "open an app on the desktop" },
    Spec { name: "notify", pos: &["text"], required: 1, named: &["sound", "title"], session: true, net: false, help: "show a desktop notice" },
    Spec { name: "remind", pos: &["text"], required: 1, named: &["sound", "title"], session: true, net: false, help: "show a reminder with Snooze / Dismiss and a sound" },
    Spec { name: "play", pos: &["sound"], required: 0, named: &[], session: true, net: false, help: "play a sound: chime, soft, alarm" },
    Spec { name: "copy", pos: &["from", "to"], required: 2, named: &[], session: false, net: false, help: "copy a file or folder" },
    Spec { name: "move", pos: &["from", "to"], required: 2, named: &[], session: false, net: false, help: "move a file or folder" },
    Spec { name: "delete", pos: &["path"], required: 1, named: &[], session: false, net: false, help: "delete a file, or files matching a * pattern" },
    Spec { name: "write", pos: &["text", "path"], required: 2, named: &["append"], session: false, net: false, help: "write text to a file" },
    Spec { name: "http.get", pos: &["url"], required: 1, named: &["retries", "save"], session: false, net: true, help: "fetch a URL -> .out .code" },
    Spec { name: "wait", pos: &["duration"], required: 1, named: &[], session: false, net: false, help: "wait, e.g. wait(30s)" },
    Spec { name: "log", pos: &["text"], required: 1, named: &[], session: false, net: false, help: "write a line to the run log" },
];

pub fn spec_of(name: &str) -> Option<&'static Spec> {
    let n = match name {
        "sound" | "play.sound" | "beep" => "play",
        "sftp" | "sftp.upload" => "sftp.put",
        "ssh" | "ssh.exec" => "ssh.run",
        "open" | "app" => "app.open",
        "screencap" | "capture" => "screenshot",
        "record" | "screen.record" => "screenrec",
        "rm" | "remove" => "delete",
        "mv" => "move",
        "cp" => "copy",
        "sleep" | "delay" => "wait",
        "http" | "fetch" => "http.get",
        o => o,
    };
    SPECS.iter().find(|s| s.name == n)
}

/// The canonical action name for an alias.
pub fn canon(name: &str) -> &'static str {
    spec_of(name).map(|s| s.name).unwrap_or("")
}

/// Argument names that hold credentials: their value must be `keyring("...")`.
const SECRET_ARGS: &[&str] = &[
    "password", "passwd", "pass", "pw", "pwd", "secret", "token", "key", "api_key", "apikey", "auth", "passphrase", "private_key", "privkey", "credential", "credentials", "bearer",
];

fn is_secret_word(w: &str) -> bool {
    SECRET_ARGS.contains(&w.to_ascii_lowercase().as_str())
}

/// Does a string carry something that must never sit in a file in clear?
fn secret_smell(s: &str) -> Option<&'static str> {
    let l = s.to_ascii_lowercase();
    if l.contains("-----begin") || l.contains("private key") {
        return Some("a private key block");
    }
    // user:password@host in an address
    let addr = l.split("://").nth(1).unwrap_or(&l);
    if let Some(at) = addr.find('@') {
        let userinfo = &addr[..at];
        if !userinfo.contains('/') && userinfo.contains(':') {
            return Some("a password inside an address (user:password@host)");
        }
    }
    if l.contains("password=") || l.contains("passwd=") || l.contains("token=") || l.contains("apikey=") || l.contains("api_key=") {
        return Some("a password or token inside the text");
    }
    None
}

// ── lexer ────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Word(String), // starts with a digit: 3, 30s, 08:00, 2026-11-20, 1.5h
    P(char),
}

fn lex(s: &str, line: usize) -> Result<Vec<Tok>, Error> {
    let mut out = Vec::new();
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '#' {
            break;
        }
        if c == '"' || c == '\'' {
            let q = c;
            let mut v = String::new();
            i += 1;
            let mut closed = false;
            while i < b.len() {
                let d = b[i];
                if d == '\\' && i + 1 < b.len() {
                    v.push(match b[i + 1] {
                        'n' => '\n',
                        't' => '\t',
                        o => o,
                    });
                    i += 2;
                    continue;
                }
                if d == q {
                    closed = true;
                    i += 1;
                    break;
                }
                v.push(d);
                i += 1;
            }
            if !closed {
                return Err(Error { secret: false, line, msg: String::from("this text is missing its closing quote") });
            }
            out.push(Tok::Str(v));
            continue;
        }
        if c.is_ascii_digit() {
            let st = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == ':' || b[i] == '.' || b[i] == '-') {
                i += 1;
            }
            out.push(Tok::Word(b[st..i].iter().collect()));
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let st = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '.') {
                i += 1;
            }
            out.push(Tok::Ident(b[st..i].iter().collect()));
            continue;
        }
        if "(),=:-".contains(c) {
            out.push(Tok::P(c));
            i += 1;
            continue;
        }
        return Err(Error { secret: false, line, msg: format!("unexpected `{c}`") });
    }
    Ok(out)
}

/// The line without its comment (a `#` outside quotes).
fn strip_comment(s: &str) -> &str {
    let mut q: Option<char> = None;
    for (i, c) in s.char_indices() {
        match (q, c) {
            (Some(x), c) if c == x => q = None,
            (None, '"') | (None, '\'') => q = Some(c),
            (None, '#') => return &s[..i],
            _ => {},
        }
    }
    s
}

fn paren_depth(s: &str) -> i32 {
    let mut q: Option<char> = None;
    let mut d = 0;
    for c in s.chars() {
        match (q, c) {
            (Some(x), c) if c == x => q = None,
            (None, '"') | (None, '\'') => q = Some(c),
            (None, '(') => d += 1,
            (None, ')') => d -= 1,
            _ => {},
        }
    }
    d
}

/// Logical lines: (first line number, text without comments), a call's
/// arguments may continue over several lines while a `(` is open.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut cur: Option<(usize, String, i32)> = None;
    for (i, raw) in text.lines().enumerate() {
        let l = strip_comment(raw).trim_end();
        match cur.take() {
            Some((n, mut acc, depth)) => {
                acc.push(' ');
                acc.push_str(l.trim());
                let d = depth + paren_depth(l);
                if d > 0 {
                    cur = Some((n, acc, d));
                } else {
                    out.push((n, acc));
                }
            },
            None => {
                if l.trim().is_empty() {
                    continue;
                }
                let d = paren_depth(l);
                if d > 0 {
                    cur = Some((i + 1, String::from(l.trim()), d));
                } else {
                    out.push((i + 1, String::from(l.trim())));
                }
            },
        }
    }
    if let Some((n, acc, _)) = cur {
        out.push((n, acc)); // unclosed: the call parser reports it
    }
    out
}

// ── calls ────────────────────────────────────────────────────────────────────

struct P<'a> {
    t: &'a [Tok],
    i: usize,
    line: usize,
}

impl P<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.t.get(self.i).cloned();
        self.i += 1;
        t
    }
    fn err(&self, msg: String) -> Error {
        Error { secret: false, line: self.line, msg }
    }
    fn expect(&mut self, c: char) -> Result<(), Error> {
        match self.next() {
            Some(Tok::P(x)) if x == c => Ok(()),
            Some(t) => Err(self.err(format!("expected `{c}` here, found {}", tok_show(&t)))),
            None => Err(self.err(format!("expected `{c}` before the end of the line"))),
        }
    }

    fn value(&mut self) -> Result<Val, Error> {
        match self.next() {
            Some(Tok::Str(s)) => Ok(Val::Str(s)),
            Some(Tok::P('-')) => match self.next() {
                Some(Tok::Word(w)) => w.parse::<f64>().map(|v| Val::Num(-v)).map_err(|_| self.err(format!("`-{w}` is not a number"))),
                _ => Err(self.err(String::from("a lone `-`"))),
            },
            Some(Tok::Word(w)) => {
                if let Ok(v) = w.parse::<f64>() {
                    return Ok(Val::Num(v));
                }
                if let Some(d) = cron::parse_duration(&w) {
                    return Ok(Val::Dur(d));
                }
                Err(self.err(format!("`{w}` is not a number or a duration (like 30s, 5m, 2h)")))
            },
            Some(Tok::Ident(id)) => {
                let l = id.to_ascii_lowercase();
                if l == "keyring" || l == "secret" {
                    self.expect('(')?;
                    let name = match self.next() {
                        Some(Tok::Str(s)) => s,
                        _ => return Err(self.err(String::from("keyring(...) takes the secret's name in quotes, e.g. keyring(\"nas\")"))),
                    };
                    self.expect(')')?;
                    if name.trim().is_empty() || name.len() > 48 {
                        return Err(self.err(String::from("a keyring name is 1-48 characters")));
                    }
                    return Ok(Val::Secret(name));
                }
                match l.as_str() {
                    "yes" | "true" | "on" => return Ok(Val::Bool(true)),
                    "no" | "false" | "off" => return Ok(Val::Bool(false)),
                    _ => {},
                }
                if matches!(self.peek(), Some(Tok::P('('))) {
                    return Err(self.err(format!("`{id}(...)` cannot be used as a value here")));
                }
                let (s, f) = match id.split_once('.') {
                    Some((s, f)) => (String::from(s), String::from(f)),
                    None => (id.clone(), String::new()),
                };
                Ok(Val::Ref(s, f))
            },
            Some(t) => Err(self.err(format!("expected a value, found {}", tok_show(&t)))),
            None => Err(self.err(String::from("expected a value before the end of the line"))),
        }
    }

    fn call(&mut self) -> Result<Call, Error> {
        let action = match self.next() {
            Some(Tok::Ident(a)) => a,
            Some(t) => return Err(self.err(format!("expected an action like screenshot(...), found {}", tok_show(&t)))),
            None => return Err(self.err(String::from("expected an action like notify(\"...\")"))),
        };
        self.expect('(')?;
        let mut c = Call { action, pos: Vec::new(), named: Vec::new(), line: self.line };
        if matches!(self.peek(), Some(Tok::P(')'))) {
            self.i += 1;
            return Ok(c);
        }
        loop {
            // name: value ?
            let named = match (self.t.get(self.i), self.t.get(self.i + 1)) {
                (Some(Tok::Ident(n)), Some(Tok::P(':'))) => Some(n.clone()),
                _ => None,
            };
            if let Some(n) = named {
                self.i += 2;
                let v = self.value()?;
                if c.named.iter().any(|(x, _)| *x == n) {
                    return Err(self.err(format!("`{n}:` is given twice")));
                }
                c.named.push((n, v));
            } else {
                if !c.named.is_empty() {
                    return Err(self.err(String::from("plain arguments go before the `name: value` ones")));
                }
                c.pos.push(self.value()?);
            }
            match self.next() {
                Some(Tok::P(',')) => continue,
                Some(Tok::P(')')) => break,
                Some(t) => return Err(self.err(format!("expected `,` or `)` after an argument, found {}", tok_show(&t)))),
                None => return Err(self.err(String::from("this call is missing its closing `)`"))),
            }
        }
        Ok(c)
    }
}

fn tok_show(t: &Tok) -> String {
    match t {
        Tok::Ident(s) | Tok::Word(s) => format!("`{s}`"),
        Tok::Str(s) => format!("\"{s}\""),
        Tok::P(c) => format!("`{c}`"),
    }
}

/// Close spellings of an action, for "did you mean".
fn suggest(name: &str) -> String {
    let l = name.to_ascii_lowercase();
    let mut best: Option<(usize, &str)> = None;
    for s in SPECS {
        let d = edit_distance(&l, s.name);
        if d <= 3 && best.map(|b| d < b.0).unwrap_or(true) {
            best = Some((d, s.name));
        }
    }
    match best {
        Some((_, n)) => format!(" (did you mean {n}?)"),
        None => String::new(),
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = alloc::vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let sub = prev[j - 1] + if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Check one call against its action's spec; `steps` = the step names defined before it.
fn check_call(c: &mut Call, steps: &[String], errs: &mut Vec<Error>) {
    let line = c.line;
    let Some(spec) = spec_of(&c.action) else {
        errs.push(Error { secret: false, line, msg: format!("unknown action `{}`{}", c.action, suggest(&c.action)) });
        return;
    };
    c.action = String::from(spec.name);
    if c.pos.len() > spec.pos.len() {
        errs.push(Error { secret: false, line, msg: format!("{} takes {} plain argument(s), got {}", spec.name, spec.pos.len(), c.pos.len()) });
    }
    let given = |n: &str, c: &Call| -> bool { c.named.iter().any(|(x, _)| x == n) || spec.pos.iter().position(|p| *p == n).map(|i| i < c.pos.len()).unwrap_or(false) };
    for (i, p) in spec.pos.iter().enumerate() {
        if i < spec.required && !given(p, c) {
            errs.push(Error { secret: false, line, msg: format!("{} needs `{p}`", spec.name) });
        }
    }
    for (n, v) in &c.named {
        if is_secret_word(n) {
            match v {
                Val::Secret(_) => {},
                Val::Str(_) | Val::Num(_) => {
                    errs.push(Error {
                        secret: true,
                        line,
                        msg: format!("`{n}:` must name a keyring secret, like {n}: keyring(\"nas\") -- passwords, keys and tokens are never written into a .lcron file (add them in LingCal > Keyring)"),
                    });
                    continue;
                },
                _ => {
                    errs.push(Error { secret: true, line, msg: format!("`{n}:` must be keyring(\"name\")") });
                    continue;
                },
            }
        }
        if !spec.named.contains(&n.as_str()) && !spec.pos.contains(&n.as_str()) {
            errs.push(Error { secret: false, line, msg: format!("{} has no `{n}:` (it takes {})", spec.name, list_or_none(spec)) });
        }
    }
    let all: Vec<&Val> = c.pos.iter().chain(c.named.iter().map(|(_, v)| v)).collect();
    for v in &all {
        match v {
            Val::Str(s) => {
                if let Some(what) = secret_smell(s) {
                    errs.push(Error { secret: true, line, msg: format!("this text holds {what} -- keep secrets in the keyring and refer to them with keyring(\"name\")") });
                }
                for r in placeholders(s) {
                    let head = r.split('.').next().unwrap_or("");
                    if !BUILTIN_VARS.contains(&head) && !steps.iter().any(|s| s == head) {
                        errs.push(Error { secret: false, line, msg: format!("`{{{r}}}` names no earlier step (known: {})", known_vars(steps)) });
                    }
                }
            },
            Val::Ref(s, f) => {
                if !steps.iter().any(|x| x == s) {
                    errs.push(Error { secret: false, line, msg: format!("`{s}` is not an earlier step (steps so far: {})", if steps.is_empty() { String::from("none") } else { steps.join(", ") }) });
                } else if !f.is_empty() && !REF_FIELDS.contains(&f.as_str()) {
                    errs.push(Error { secret: false, line, msg: format!("a step result has no `.{f}` (use .path .name .size .out .code .ok)") });
                }
            },
            _ => {},
        }
    }
    // keyring() only where a credential goes
    for (i, v) in c.pos.iter().enumerate() {
        if matches!(v, Val::Secret(_)) {
            let n = spec.pos.get(i).copied().unwrap_or("?");
            errs.push(Error { secret: false, line, msg: format!("keyring(...) can only be given to key:, password: or token:, not to `{n}`") });
        }
    }
    if spec.name == "ssh.run" || spec.name == "sftp.put" {
        // (a literal password was already reported above)
        let has = c.named.iter().any(|(n, _)| n == "key" || n == "password");
        if !has {
            errs.push(Error { secret: false, line, msg: format!("{} needs key: keyring(\"name\") (an SSH key from LingCal > Keyring) or password: keyring(\"name\")", spec.name) });
        }
        let target = c.arg(if spec.name == "ssh.run" { 0 } else { 1 }, "target");
        if let Some(Val::Str(t)) = target {
            if parse_target(t).is_none() {
                errs.push(Error { secret: false, line, msg: format!("`{t}` is not user@host or user@host:/path") });
            }
        }
    }
    if spec.name == "wait" {
        match c.pos.first() {
            Some(Val::Dur(d)) if *d <= 3600 => {},
            Some(Val::Num(n)) if *n >= 0.0 && *n <= 3600.0 => {},
            _ => errs.push(Error { secret: false, line, msg: String::from("wait takes a duration up to 1h, e.g. wait(30s)") }),
        }
    }
    if spec.name == "screenrec" {
        let secs = match c.arg(0, "secs") {
            None => 30.0,
            Some(Val::Num(n)) => *n,
            Some(Val::Dur(d)) => *d as f64,
            Some(_) => -1.0,
        };
        if !(1.0..=3600.0).contains(&secs) {
            errs.push(Error { secret: false, line, msg: String::from("screenrec(secs: N) records 1 to 3600 seconds") });
        }
    }
}

fn list_or_none(spec: &Spec) -> String {
    if spec.named.is_empty() {
        String::from("no named arguments")
    } else {
        spec.named.iter().map(|n| format!("{n}:")).collect::<Vec<_>>().join(" ")
    }
}

pub const BUILTIN_VARS: &[&str] = &["date", "time", "datetime", "user", "job", "error", "host", "year", "month", "day", "hour", "minute"];
pub const REF_FIELDS: &[&str] = &["path", "name", "size", "out", "code", "ok", "failed"];

fn known_vars(steps: &[String]) -> String {
    let mut v: Vec<String> = BUILTIN_VARS.iter().map(|s| String::from(*s)).collect();
    v.extend(steps.iter().cloned());
    v.join(", ")
}

/// `{name}` / `{step.field}` references in a text.
pub fn placeholders(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(a) = rest.find('{') {
        let after = &rest[a + 1..];
        match after.find('}') {
            Some(b) => {
                let r = after[..b].trim();
                if !r.is_empty() && r.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.') {
                    out.push(String::from(r));
                }
                rest = &after[b + 1..];
            },
            None => break,
        }
    }
    out
}

/// `user@host[:port][:/path]` -> (user, host, port, path).
pub fn parse_target(t: &str) -> Option<(String, String, u16, String)> {
    let t = t.trim().strip_prefix("sftp://").or_else(|| t.trim().strip_prefix("ssh://")).unwrap_or(t.trim());
    let (user, rest) = t.split_once('@')?;
    if user.is_empty() || user.contains(':') || user.contains('/') {
        return None;
    }
    let (hostport, path) = match rest.find(":/").or_else(|| rest.find(":~")) {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => match rest.split_once(':') {
                // host:relative/path (scp style) unless it is a port
                Some((h, p)) if !p.chars().all(|c| c.is_ascii_digit()) => (h, p),
                _ => (rest, ""),
            },
        },
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()?),
        None => (hostport, 22),
    };
    if host.is_empty() || host.contains(' ') {
        return None;
    }
    Some((String::from(user), String::from(host), port, String::from(path)))
}

fn parse_cond(words: &str, line: usize, steps: &[String]) -> Result<Cond, Error> {
    let w = words.trim();
    if let Some(rest) = w.strip_prefix("not ") {
        return Ok(Cond::Not(alloc::boxed::Box::new(parse_cond(rest, line, steps)?)));
    }
    let check = |s: &str| -> Result<String, Error> {
        if steps.iter().any(|x| x == s) {
            Ok(String::from(s))
        } else {
            Err(Error { secret: false, line, msg: format!("`{s}` is not an earlier step") })
        }
    };
    if let Some((lhs, rhs)) = w.split_once(" contains ") {
        let st = lhs.trim().strip_suffix(".out").unwrap_or(lhs.trim());
        let t = rhs.trim().trim_matches('"');
        return Ok(Cond::Contains(check(st)?, String::from(t)));
    }
    if let Some(s) = w.strip_suffix(".ok") {
        return Ok(Cond::Ok(check(s.trim())?));
    }
    if let Some(s) = w.strip_suffix(".failed").or_else(|| w.strip_suffix(".fail")) {
        return Ok(Cond::Failed(check(s.trim())?));
    }
    if let Some(s) = w.strip_suffix(".skipped") {
        return Ok(Cond::Skipped(check(s.trim())?));
    }
    Err(Error { secret: false, line, msg: format!("`{w}` is not a condition: use step.ok, step.failed, not step.ok or step.out contains \"text\"") })
}

// ── triggers ─────────────────────────────────────────────────────────────────

fn parse_trigger(words: &[&str], line: usize) -> Result<Trigger, Error> {
    let e = |m: String| Error { secret: false, line, msg: m };
    let kw = words[0].to_ascii_lowercase();
    // a day filter and a time: `on mon,wed` / `at 09:00`
    let find = |k: &str| words.iter().position(|w| w.eq_ignore_ascii_case(k));
    let days_after = |i: usize| -> Result<u8, Error> {
        let mut s = String::new();
        for w in &words[i + 1..] {
            if w.eq_ignore_ascii_case("at") || w.eq_ignore_ascii_case("between") {
                break;
            }
            s.push_str(w);
            s.push(' ');
        }
        cron::parse_days(&s).ok_or_else(|| e(format!("`{}` is not a list of weekdays (mon,tue,... / weekdays / weekends)", s.trim())))
    };
    let time_after = |i: usize| -> Result<u32, Error> {
        let t = words.get(i + 1).ok_or_else(|| e(String::from("a time like 09:00 is missing after `at`")))?;
        cron::parse_hm(t).ok_or_else(|| e(format!("`{t}` is not a time (use HH:MM, 24-hour)")))
    };
    match kw.as_str() {
        "cron" => {
            let rest = words[1..].join(" ");
            let expr = rest.trim().trim_matches('"').trim();
            if expr.is_empty() {
                return Err(e(String::from("cron needs an expression in quotes: cron \"m h dom mon dow\"")));
            }
            let c = Cron::parse(expr).map_err(|m| e(format!("cron: {m}")))?;
            Ok(Trigger::Cron(c, String::from(expr)))
        },
        "once" => {
            // once at 2026-11-20 14:00
            let i = find("at").ok_or_else(|| e(String::from("write `once at YYYY-MM-DD HH:MM`")))?;
            let d = words.get(i + 1).and_then(|w| cron::parse_date(w)).ok_or_else(|| e(String::from("`once at` needs a date YYYY-MM-DD then a time HH:MM")))?;
            let t = words.get(i + 2).and_then(|w| cron::parse_hm(w)).ok_or_else(|| e(String::from("`once at` needs a time HH:MM after the date")))?;
            Ok(Trigger::Once(d * cron::MIN_PER_DAY + t as i64))
        },
        "every" => {
            let d = words.get(1).ok_or_else(|| e(String::from("every needs an interval: every 30m, every 2h, every 1d")))?;
            let secs = cron::parse_duration(d).ok_or_else(|| e(format!("`{d}` is not an interval (like 15m, 2h, 1d)")))?;
            if secs < 60 {
                return Err(e(String::from("the shortest interval is 1m (rate limit)")));
            }
            if secs % 60 != 0 {
                return Err(e(String::from("an interval is a whole number of minutes")));
            }
            if secs > 86400 {
                return Err(e(String::from("intervals longer than a day: use `weekly on sun at 03:00` or cron")));
            }
            let mut start = 0u32;
            let mut end = 1439u32;
            let mut days = 0x7Fu8;
            if let Some(i) = find("between") {
                let a = words.get(i + 1).and_then(|w| cron::parse_hm(w)).ok_or_else(|| e(String::from("write `between HH:MM and HH:MM`")))?;
                if !words.get(i + 2).map(|w| w.eq_ignore_ascii_case("and")).unwrap_or(false) {
                    return Err(e(String::from("write `between HH:MM and HH:MM`")));
                }
                let b = words.get(i + 3).and_then(|w| cron::parse_hm(w)).ok_or_else(|| e(String::from("write `between HH:MM and HH:MM`")))?;
                if b < a {
                    return Err(e(String::from("the window must end after it starts (no overnight windows; use two jobs)")));
                }
                start = a;
                end = b;
            }
            if let Some(i) = find("at") {
                let t = time_after(i)?;
                start = t;
                if secs == 86400 {
                    end = t;
                }
            }
            if let Some(i) = find("on") {
                days = days_after(i)?;
            }
            if secs == 86400 && find("at").is_none() && find("between").is_none() {
                end = 0;
            }
            Ok(Trigger::Every { secs, start, end, days })
        },
        "daily" | "weekdays" | "weekends" | "weekly" => {
            let i = find("at").ok_or_else(|| e(format!("write `{kw} at HH:MM`")))?;
            let t = time_after(i)?;
            let days = match kw.as_str() {
                "daily" => 0x7F,
                "weekdays" => 0x1F,
                "weekends" => 0x60,
                _ => {
                    let j = find("on").ok_or_else(|| e(String::from("write `weekly on sun at 03:00`")))?;
                    days_after(j)?
                },
            };
            let days = if kw == "daily" { find("on").map(days_after).transpose()?.unwrap_or(days) } else { days };
            Ok(Trigger::Every { secs: 86400, start: t, end: t, days })
        },
        "monthly" => {
            // monthly on day 1 at 09:00
            let i = find("day").ok_or_else(|| e(String::from("write `monthly on day 1 at 09:00`")))?;
            let d: u32 = words.get(i + 1).and_then(|w| w.parse().ok()).filter(|d| (1..=31).contains(d)).ok_or_else(|| e(String::from("the day of the month is 1-31")))?;
            let t = time_after(find("at").ok_or_else(|| e(String::from("write `monthly on day 1 at 09:00`")))?)?;
            let expr = format!("{} {} {d} * *", t % 60, t / 60);
            let c = Cron::parse(&expr).map_err(e)?;
            Ok(Trigger::Cron(c, expr))
        },
        _ => Err(e(format!("unknown timer `{kw}`"))),
    }
}

fn yes_no(w: Option<&&str>, line: usize, what: &str) -> Result<bool, Error> {
    match w.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("yes") | Some("true") | Some("on") => Ok(true),
        Some("no") | Some("false") | Some("off") => Ok(false),
        _ => Err(Error { secret: false, line, msg: format!("{what} is yes or no") }),
    }
}

// ── the file ─────────────────────────────────────────────────────────────────

/// Parse a whole `.lcron` file. All problems are collected (in line order).
pub fn parse(text: &str) -> Result<Job, Vec<Error>> {
    let mut errs: Vec<Error> = Vec::new();
    let mut job = Job {
        name: String::new(),
        trigger: Trigger::None,
        at_boot: false,
        at_login: false,
        missed: Missed::Skip,
        enabled: true,
        limit_per_hour: 60,
        steps: Vec::new(),
        on_fail: Vec::new(),
        on_ok: Vec::new(),
    };
    let mut seen_job = false;
    let mut trigger_line = 0usize;
    for (line, l) in logical_lines(text) {
        let words: Vec<&str> = l.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        let kw = words[0].trim_end_matches(':').to_ascii_lowercase();
        // a credential written as a setting: `password: "hunter2"`, `token = abc`
        if is_secret_word(&kw) || (words.len() > 1 && is_secret_word(words[0].split([':', '=']).next().unwrap_or(""))) {
            errs.push(Error {
                secret: true,
                line,
                msg: format!("`{}` looks like a credential written into the file -- secrets live in the keyring (LingCal > Keyring) and a step names them with keyring(\"name\")", words[0].trim_end_matches([':', '='])),
            });
            continue;
        }
        if kw != "job" && !seen_job && kw != "step" {
            // settings before `job` are fine too, but a file must name its job
        }
        match kw.as_str() {
            "job" => {
                if seen_job {
                    errs.push(Error { secret: false, line, msg: String::from("one job per file: put the second job in its own .lcron") });
                    continue;
                }
                seen_job = true;
                match lex(&l, line) {
                    Ok(t) => match t.get(1) {
                        Some(Tok::Str(s)) if !s.trim().is_empty() && s.len() <= 64 => job.name = s.clone(),
                        _ => errs.push(Error { secret: false, line, msg: String::from("write the job's name in quotes: job \"Backup\" (1-64 characters)") }),
                    },
                    Err(e) => errs.push(e),
                }
            },
            "every" | "cron" | "once" | "daily" | "weekdays" | "weekends" | "weekly" | "monthly" => {
                if trigger_line != 0 {
                    errs.push(Error { secret: false, line, msg: format!("a job has one timer (the first is on line {trigger_line}); use cron for complex schedules") });
                    continue;
                }
                trigger_line = line;
                match parse_trigger(&words, line) {
                    Ok(t) => job.trigger = t,
                    Err(e) => errs.push(e),
                }
            },
            "at_boot" | "at_login" | "enabled" => match yes_no(words.get(1), line, &kw) {
                Ok(v) => match kw.as_str() {
                    "at_boot" => job.at_boot = v,
                    "at_login" => job.at_login = v,
                    _ => job.enabled = v,
                },
                Err(e) => errs.push(e),
            },
            "missed" => {
                let rest = words[1..].join(" ").to_ascii_lowercase();
                let rest = rest.trim_matches('"');
                job.missed = match rest {
                    "skip" | "ignore" | "no" => Missed::Skip,
                    "catchup" | "catch_up" | "catch up" | "catch up once" | "run" | "run once" | "once" => Missed::CatchUp,
                    _ => {
                        errs.push(Error { secret: false, line, msg: String::from("missed is `skip` or `catchup` (run once when the PC comes back)") });
                        continue;
                    },
                }
            },
            "limit" => {
                // limit 6 per hour
                let n = words.get(1).and_then(|w| w.parse::<u32>().ok());
                match (n, words.get(2), words.get(3)) {
                    (Some(n), Some(p), Some(h)) if p.eq_ignore_ascii_case("per") && h.to_ascii_lowercase().starts_with("hour") && (1..=60).contains(&n) => job.limit_per_hour = n,
                    _ => errs.push(Error { secret: false, line, msg: String::from("write `limit N per hour` (1-60)") }),
                }
            },
            "step" => {
                let toks = match lex(&l, line) {
                    Ok(t) => t,
                    Err(e) => {
                        errs.push(e);
                        continue;
                    },
                };
                let name = match (toks.get(1), toks.get(2)) {
                    (Some(Tok::Ident(n)), Some(Tok::P('='))) => n.clone(),
                    _ => {
                        errs.push(Error { secret: false, line, msg: String::from("write `step <name> = action(...)`") });
                        continue;
                    },
                };
                if name.contains('.') || BUILTIN_VARS.contains(&name.as_str()) || spec_of(&name).is_some() {
                    errs.push(Error { secret: false, line, msg: format!("`{name}` cannot name a step (pick another word)") });
                    continue;
                }
                if job.steps.iter().any(|s| s.name == name) {
                    errs.push(Error { secret: false, line, msg: format!("there is already a step called `{name}`") });
                    continue;
                }
                let mut p = P { t: &toks, i: 3, line };
                let names: Vec<String> = job.steps.iter().map(|s| s.name.clone()).collect();
                match p.call() {
                    Ok(mut c) => {
                        check_call(&mut c, &names, &mut errs);
                        // what follows the call: when / unless
                        let when = match p.peek() {
                            None => None,
                            Some(Tok::Ident(k)) if k == "when" || k == "if" || k == "unless" => {
                                let neg = k == "unless";
                                // the raw text after the keyword
                                let raw = after_keyword(&l, k);
                                match parse_cond(&raw, line, &names) {
                                    Ok(cnd) => Some(if neg { Cond::Not(alloc::boxed::Box::new(cnd)) } else { cnd }),
                                    Err(e) => {
                                        errs.push(e);
                                        None
                                    },
                                }
                            },
                            Some(t) => {
                                errs.push(Error { secret: false, line, msg: format!("unexpected {} after the call (only `when ...` / `unless ...` may follow)", tok_show(t)) });
                                None
                            },
                        };
                        job.steps.push(Step { name, call: c, when, line });
                    },
                    Err(e) => errs.push(e),
                }
                if job.steps.len() > 32 {
                    errs.push(Error { secret: false, line, msg: String::from("at most 32 steps per job") });
                }
            },
            "on_fail" | "on_ok" | "on_failure" | "on_success" => {
                let toks = match lex(&l, line) {
                    Ok(t) => t,
                    Err(e) => {
                        errs.push(e);
                        continue;
                    },
                };
                let mut p = P { t: &toks, i: 1, line };
                let names: Vec<String> = job.steps.iter().map(|s| s.name.clone()).collect();
                match p.call() {
                    Ok(mut c) => {
                        check_call(&mut c, &names, &mut errs);
                        if p.peek().is_some() {
                            errs.push(Error { secret: false, line, msg: String::from("only one action per on_fail / on_ok line") });
                        }
                        if kw == "on_fail" || kw == "on_failure" {
                            job.on_fail.push(c);
                        } else {
                            job.on_ok.push(c);
                        }
                    },
                    Err(e) => errs.push(e),
                }
            },
            _ => {
                // a bare call (`notify("hi")`) reads like a step without a name
                if words[0].contains('(') {
                    errs.push(Error { secret: false, line, msg: format!("write `step <name> = {}` (every action is a named step)", l.trim()) });
                } else {
                    errs.push(Error { secret: false, line, msg: format!("unknown statement `{}` (job, every, daily, weekly, cron, once, at_boot, at_login, missed, limit, enabled, step, on_fail, on_ok)", words[0]) });
                }
            },
        }
    }
    if !seen_job {
        errs.push(Error { secret: false, line: 1, msg: String::from("the file has no `job \"name\"` line") });
    }
    if job.steps.is_empty() && errs.is_empty() {
        errs.push(Error { secret: false, line: 1, msg: String::from("a job needs at least one `step name = action(...)`") });
    }
    if matches!(job.trigger, Trigger::None) && !job.at_boot && !job.at_login && errs.is_empty() {
        // allowed: a manual job (Run now only) -- not an error
    }
    if errs.is_empty() {
        Ok(job)
    } else {
        errs.sort_by_key(|e| e.line);
        Err(errs)
    }
}

/// The text after the first ` <kw> ` outside quotes and parens.
fn after_keyword(l: &str, kw: &str) -> String {
    let mut q: Option<char> = None;
    let mut depth = 0;
    let b = l.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        match (q, c) {
            (Some(x), c) if c == x => q = None,
            (Some(_), _) => {},
            (None, '"') | (None, '\'') => q = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            (None, _) if depth == 0 && l[i..].starts_with(kw) && i > 0 && b[i - 1] == b' ' && l[i + kw.len()..].starts_with(' ') => {
                return String::from(l[i + kw.len()..].trim());
            },
            _ => {},
        }
        i += 1;
    }
    String::new()
}

impl Job {
    pub fn calls(&self) -> impl Iterator<Item = &Call> {
        self.steps.iter().map(|s| &s.call).chain(self.on_fail.iter()).chain(self.on_ok.iter())
    }
    /// Keyring names the job uses.
    pub fn secrets(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for c in self.calls() {
            for (_, val) in &c.named {
                if let Val::Secret(n) = val {
                    if !v.contains(n) {
                        v.push(n.clone());
                    }
                }
            }
        }
        v
    }
    /// Does it need its owner signed in (keyring open)? Anything touching the
    /// screen, sound, notifications or apps, or using a secret.
    pub fn needs_session(&self) -> bool {
        !self.secrets().is_empty() || self.calls().any(|c| spec_of(&c.action).map(|s| s.session || s.net).unwrap_or(false))
    }
}

/// One line for a call, for the Blocks view / dry run: `sftp.put($shot, "ling@nas...", key: keyring("nas"))`.
pub fn show_call(c: &Call) -> String {
    let mut parts: Vec<String> = c.pos.iter().map(|v| v.show()).collect();
    for (n, v) in &c.named {
        parts.push(format!("{n}: {}", v.show()));
    }
    format!("{}({})", c.action, parts.join(", "))
}

pub fn errors_text(errs: &[Error]) -> String {
    errs.iter().map(|e| format!("line {}: {}", e.line, e.msg)).collect::<Vec<_>>().join("\n")
}

#[allow(dead_code)]
pub fn job_summary(j: &Job) -> String {
    format!("{} -- {} -- {} step(s)", j.name, j.trigger.describe(), j.steps.len()).to_string()
}
