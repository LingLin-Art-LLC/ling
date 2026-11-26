//! The SSH shell's line editor and prompt, zsh style: a two-line ROYGBIV prompt
//! (`╭─ user@host /path (miao:soul +)` / `╰─❯`), cursor editing (arrows, Home/End,
//! Ctrl+A/E/B/F/U/K/W/L/D, Alt+B/F), history (Up/Down, Ctrl+R-free prefix search: typing
//! then Up finds the last line starting with it), Tab completion of command names and
//! paths, UTF-8 input (Hangul, Chinese, Thai... count as characters, wide ones as two
//! columns), plus `~`, `cd -`, and `a ; b` / `a && b` chaining (sshd runs the pieces).
//!
//! Pure state machine: `key` takes the client's bytes one at a time and says what
//! happened; `render` gives the bytes that redraw the input line. No I/O here.

use crate::drivers::ansi;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub enum Key {
    /// Nothing to do yet (part of an escape sequence or a UTF-8 character).
    None,
    /// The line changed: redraw it.
    Redraw,
    /// One character typed at the end of the line: just echo it (no whole-line redraw).
    Echo(char),
    /// Enter: run this line (already added to the history).
    Line(String),
    /// Ctrl+C: drop the line.
    Interrupt,
    /// Ctrl+D on an empty line: log out.
    Eof,
    /// Ctrl+L: clear the screen, then the prompt.
    Clear,
    /// Tab: complete the word before the cursor.
    Tab,
    /// The key does nothing here (ring the bell).
    Bell,
}

pub struct Editor {
    buf: Vec<char>,
    cur: usize,
    hist: Vec<String>,
    /// Browsing the history: index into `hist` (hist.len() = the line being typed).
    hpos: usize,
    /// The line being typed when browsing started, and the prefix Up searches for.
    draft: Vec<char>,
    /// Escape sequence state: 0 none, 1 after ESC, 2 in CSI (ESC [), 3 after ESC O.
    esc: u8,
    csi: String,
    /// A UTF-8 character still arriving.
    utf8: Vec<u8>,
    /// Columns of the terminal (pty-req / window-change), for long lines.
    pub cols: usize,
}

const HIST_MAX: usize = 500;

impl Editor {
    pub fn new(history: Vec<String>) -> Editor {
        let hpos = history.len();
        Editor { buf: Vec::new(), cur: 0, hist: history, hpos, draft: Vec::new(), esc: 0, csi: String::new(), utf8: Vec::new(), cols: 80 }
    }

    pub fn history(&self) -> &[String] {
        &self.hist
    }

    pub fn line(&self) -> String {
        self.buf.iter().collect()
    }

    /// Replace the word before the cursor (`start..cur`) with `with`.
    pub fn replace_word(&mut self, start: usize, with: &str) {
        let start = start.min(self.cur);
        self.buf.splice(start..self.cur, with.chars());
        self.cur = start + with.chars().count();
    }

    /// The word before the cursor: (start index, text).
    pub fn word_before(&self) -> (usize, String) {
        let mut s = self.cur;
        while s > 0 && !self.buf[s - 1].is_whitespace() {
            s -= 1;
        }
        (s, self.buf[s..self.cur].iter().collect())
    }

    /// Is the cursor in the command word (the first word of the line, or after ; / &&)?
    pub fn in_command_word(&self) -> bool {
        let (s, _) = self.word_before();
        let before: String = self.buf[..s].iter().collect();
        let t = before.trim_end();
        t.is_empty() || t.ends_with(';') || t.ends_with("&&")
    }

    fn word_left(&self) -> usize {
        let mut i = self.cur;
        while i > 0 && !self.buf[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && self.buf[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    }

    fn word_right(&self) -> usize {
        let mut i = self.cur;
        while i < self.buf.len() && !self.buf[i].is_alphanumeric() {
            i += 1;
        }
        while i < self.buf.len() && self.buf[i].is_alphanumeric() {
            i += 1;
        }
        i
    }

    fn hist_up(&mut self) -> Key {
        if self.hpos == self.hist.len() {
            self.draft = self.buf.clone();
        }
        // with text typed, Up finds the previous line that starts with it
        let prefix: String = self.draft.iter().collect();
        let mut i = self.hpos;
        while i > 0 {
            i -= 1;
            if self.hist[i].starts_with(&prefix) {
                self.hpos = i;
                self.buf = self.hist[i].chars().collect();
                self.cur = self.buf.len();
                return Key::Redraw;
            }
        }
        Key::Bell
    }

    fn hist_down(&mut self) -> Key {
        if self.hpos >= self.hist.len() {
            return Key::Bell;
        }
        let prefix: String = self.draft.iter().collect();
        let mut i = self.hpos + 1;
        while i < self.hist.len() {
            if self.hist[i].starts_with(&prefix) {
                self.hpos = i;
                self.buf = self.hist[i].chars().collect();
                self.cur = self.buf.len();
                return Key::Redraw;
            }
            i += 1;
        }
        self.hpos = self.hist.len();
        self.buf = self.draft.clone();
        self.cur = self.buf.len();
        Key::Redraw
    }

    fn insert(&mut self, c: char) -> Key {
        let at_end = self.cur == self.buf.len();
        self.buf.insert(self.cur, c);
        self.cur += 1;
        if at_end { Key::Echo(c) } else { Key::Redraw }
    }

    /// One byte from the client.
    pub fn key(&mut self, b: u8) -> Key {
        // escape sequences: ESC [ A..D / H / F / 1~ 3~ 4~ 7~ 8~ / 1;5C (Ctrl+arrow), ESC O A..D/H/F, ESC b / ESC f
        match self.esc {
            1 => {
                self.esc = 0;
                return match b {
                    b'[' => {
                        self.esc = 2;
                        self.csi.clear();
                        Key::None
                    },
                    b'O' => {
                        self.esc = 3;
                        Key::None
                    },
                    b'b' => {
                        self.cur = self.word_left();
                        Key::Redraw
                    },
                    b'f' => {
                        self.cur = self.word_right();
                        Key::Redraw
                    },
                    0x7f => {
                        // Alt+Backspace: delete the word before the cursor
                        let s = self.word_left();
                        self.buf.drain(s..self.cur);
                        self.cur = s;
                        Key::Redraw
                    },
                    _ => Key::Bell,
                };
            },
            2 => {
                if (0x30..=0x3F).contains(&b) {
                    self.csi.push(b as char);
                    return Key::None;
                }
                self.esc = 0;
                let ctrl = self.csi.contains(";5") || self.csi.contains(";3");
                return match (b, self.csi.as_str()) {
                    (b'A', _) => self.hist_up(),
                    (b'B', _) => self.hist_down(),
                    (b'C', _) if ctrl => {
                        self.cur = self.word_right();
                        Key::Redraw
                    },
                    (b'D', _) if ctrl => {
                        self.cur = self.word_left();
                        Key::Redraw
                    },
                    (b'C', _) => {
                        if self.cur < self.buf.len() {
                            self.cur += 1;
                            Key::Redraw
                        } else {
                            Key::Bell
                        }
                    },
                    (b'D', _) => {
                        if self.cur > 0 {
                            self.cur -= 1;
                            Key::Redraw
                        } else {
                            Key::Bell
                        }
                    },
                    (b'H', _) | (b'~', "1") | (b'~', "7") => {
                        self.cur = 0;
                        Key::Redraw
                    },
                    (b'F', _) | (b'~', "4") | (b'~', "8") => {
                        self.cur = self.buf.len();
                        Key::Redraw
                    },
                    (b'~', "3") => {
                        if self.cur < self.buf.len() {
                            self.buf.remove(self.cur);
                            Key::Redraw
                        } else {
                            Key::Bell
                        }
                    },
                    (b'~', "200") | (b'~', "201") => Key::None, // bracketed paste markers
                    _ => Key::None,
                };
            },
            3 => {
                self.esc = 0;
                return match b {
                    b'A' => self.hist_up(),
                    b'B' => self.hist_down(),
                    b'C' if self.cur < self.buf.len() => {
                        self.cur += 1;
                        Key::Redraw
                    },
                    b'D' if self.cur > 0 => {
                        self.cur -= 1;
                        Key::Redraw
                    },
                    b'H' => {
                        self.cur = 0;
                        Key::Redraw
                    },
                    b'F' => {
                        self.cur = self.buf.len();
                        Key::Redraw
                    },
                    _ => Key::None,
                };
            },
            _ => {},
        }
        // a UTF-8 character arriving byte by byte
        if b >= 0x80 {
            self.utf8.push(b);
            if let Ok(s) = core::str::from_utf8(&self.utf8) {
                let cs: Vec<char> = s.chars().collect();
                self.utf8.clear();
                let mut last = Key::None;
                for c in cs {
                    last = self.insert(c);
                }
                return last;
            }
            if self.utf8.len() >= 4 {
                self.utf8.clear(); // not UTF-8: drop it
            }
            return Key::None;
        }
        self.utf8.clear();
        match b {
            0x1B => {
                self.esc = 1;
                Key::None
            },
            b'\r' | b'\n' => {
                let l = self.line();
                self.buf.clear();
                self.cur = 0;
                self.draft.clear();
                if !l.trim().is_empty() && self.hist.last().map(|h| h != &l).unwrap_or(true) {
                    self.hist.push(l.clone());
                    if self.hist.len() > HIST_MAX {
                        self.hist.remove(0);
                    }
                }
                self.hpos = self.hist.len();
                Key::Line(l)
            },
            0x7F | 0x08 => {
                if self.cur > 0 {
                    self.cur -= 1;
                    self.buf.remove(self.cur);
                    Key::Redraw
                } else {
                    Key::Bell
                }
            },
            0x03 => {
                self.buf.clear();
                self.cur = 0;
                self.hpos = self.hist.len();
                Key::Interrupt
            },
            0x04 => {
                if self.buf.is_empty() {
                    Key::Eof
                } else if self.cur < self.buf.len() {
                    self.buf.remove(self.cur);
                    Key::Redraw
                } else {
                    Key::Bell
                }
            },
            0x01 => {
                self.cur = 0;
                Key::Redraw
            },
            0x05 => {
                self.cur = self.buf.len();
                Key::Redraw
            },
            0x02 => {
                self.cur = self.cur.saturating_sub(1);
                Key::Redraw
            },
            0x06 => {
                self.cur = (self.cur + 1).min(self.buf.len());
                Key::Redraw
            },
            0x15 => {
                self.buf.drain(..self.cur);
                self.cur = 0;
                Key::Redraw
            },
            0x0B => {
                self.buf.truncate(self.cur);
                Key::Redraw
            },
            0x17 => {
                // Ctrl+W: the whitespace-separated word before the cursor
                let mut s = self.cur;
                while s > 0 && self.buf[s - 1].is_whitespace() {
                    s -= 1;
                }
                while s > 0 && !self.buf[s - 1].is_whitespace() {
                    s -= 1;
                }
                self.buf.drain(s..self.cur);
                self.cur = s;
                Key::Redraw
            },
            0x0C => Key::Clear,
            0x09 => Key::Tab,
            0x20..=0x7E => self.insert(b as char),
            _ => Key::None,
        }
    }

    /// Bytes that redraw the input line after `tail` (the prompt's last line): back to the
    /// line start, clear to the end, prompt, text, then the cursor back where it belongs.
    pub fn render(&self, tail: &str) -> Vec<u8> {
        let text: String = self.buf.iter().collect();
        let mut out = String::from("\r\x1b[K");
        out.push_str(tail);
        out.push_str(&text);
        let behind: usize = self.buf[self.cur..].iter().map(|c| ansi::char_width(*c)).sum();
        if behind > 0 {
            out.push_str(&format!("\x1b[{behind}D"));
        }
        out.into_bytes()
    }
}

/// The two prompt lines: (header printed once, tail redrawn with the input).
/// `ok`: the last command succeeded (the arrow turns red after a failure).
pub fn prompt(user: &str, cwd: &str, ok: bool) -> (String, String) {
    let host = crate::lingapp::host_name();
    let shown = if cwd == format!("home/{user}") {
        String::from("~")
    } else if let Some(rest) = cwd.strip_prefix(&format!("home/{user}/")) {
        format!("~/{rest}")
    } else {
        format!("/{cwd}")
    };
    let mut head = format!(
        "{dim}╭─{r} {g}{user}{r}{dim}@{r}{b}{host}{r} {o}{shown}{r}",
        dim = ansi::DIM,
        r = ansi::RESET,
        g = ansi::fg(ansi::GREEN),
        b = ansi::fg(ansi::BLUE),
        o = ansi::fg(ansi::ORANGE)
    );
    // miao (LingOS's version control) in this folder: (miao:soul +), like zsh's git segment
    let soul_path = if cwd.is_empty() { String::from(".miao/SOUL") } else { format!("{cwd}/.miao/SOUL") };
    if let Some(bytes) = crate::vfs::read(&soul_path) {
        let soul = core::str::from_utf8(&bytes).unwrap_or("").split_whitespace().nth(1).unwrap_or("main").to_string();
        let altar = if cwd.is_empty() { String::from(".miao/altar") } else { format!("{cwd}/.miao/altar") };
        let dirty = crate::vfs::read(&altar).is_some_and(|a| !a.is_empty());
        head.push_str(&format!(
            " {dim}({r}{v}miao:{r}{i}{soul}{r}{y}{plus}{r}{dim}){r}",
            dim = ansi::DIM,
            r = ansi::RESET,
            v = ansi::fg(ansi::VIOLET),
            i = ansi::fg(ansi::INDIGO),
            y = ansi::fg(ansi::YELLOW),
            plus = if dirty { " +" } else { "" }
        ));
    }
    let tail = format!("{}╰─{}{}❯{} ", ansi::DIM, ansi::RESET, ansi::fg(if ok { ansi::VIOLET } else { ansi::RED }), ansi::RESET);
    (head, tail)
}

/// Commands the shell knows (for Tab completion; sshcmds' and sshd's own).
pub const COMMANDS: &[&str] = &[
    "apps", "appkeys", "audio", "bench3d", "board", "bt", "cat", "cd", "chmod", "claude", "clear", "close", "cores", "cp", "cpu", "date", "daw", "del",
    "df", "dir", "echo", "exit", "free", "get", "gpu", "help", "hitches", "hostname", "hwinfo", "idle-lock", "info", "lingfu", "lingsock", "llm", "lock",
    "logout", "ls", "media", "miao", "mkdir", "models", "motd", "mv", "net", "nvme", "open", "pen", "perf", "poweroff", "prof", "ps", "pwd", "reboot",
    "rename-user", "report", "rm", "running", "screenrec", "sha256sum", "simd", "specs", "spotify", "ssh-keys", "stat", "touch", "uname", "update-boot",
    "uptime", "video", "whoami", "wifi", "wifi-intel", "ytsave",
];

/// What Tab should do with `word`: complete it (one match / a longer common prefix) or list
/// the candidates. `cwd` and `user` resolve paths like the shell does (`~`, `/abs`, relative).
pub fn complete(word: &str, command: bool, cwd: &str, user: &str) -> (Option<String>, Vec<String>) {
    let cands: Vec<String> = if command && !word.contains('/') {
        COMMANDS.iter().filter(|c| c.starts_with(word)).map(|c| String::from(*c)).collect()
    } else {
        // the directory part and the name being typed
        let (dir_part, stem) = match word.rfind('/') {
            Some(i) => (&word[..=i], &word[i + 1..]),
            None => ("", word),
        };
        let dir = if dir_part.is_empty() {
            String::from(cwd)
        } else if let Some(abs) = dir_part.strip_prefix('/') {
            crate::vfs::norm(abs)
        } else if dir_part == "~/" {
            format!("home/{user}")
        } else if let Some(rest) = dir_part.strip_prefix("~/") {
            crate::vfs::join(&format!("home/{user}"), rest)
        } else {
            crate::vfs::join(cwd, dir_part)
        };
        let mut v: Vec<String> = crate::vfs::list(&dir)
            .into_iter()
            .filter(|(n, _, _)| n.starts_with(stem) && (!n.starts_with('.') || stem.starts_with('.')))
            .map(|(n, is_dir, _)| format!("{dir_part}{n}{}", if is_dir { "/" } else { "" }))
            .collect();
        v.sort();
        v
    };
    match cands.len() {
        0 => (None, cands),
        1 => {
            let c = &cands[0];
            let done = if c.ends_with('/') { c.clone() } else { format!("{c} ") };
            (Some(done), Vec::new())
        },
        _ => {
            // the longest common prefix, if it adds anything; else list them
            let mut p: Vec<char> = cands[0].chars().collect();
            for c in &cands[1..] {
                let cc: Vec<char> = c.chars().collect();
                let n = p.iter().zip(cc.iter()).take_while(|(a, b)| a == b).count();
                p.truncate(n);
            }
            let p: String = p.into_iter().collect();
            if p.chars().count() > word.chars().count() { (Some(p), Vec::new()) } else { (None, cands) }
        },
    }
}

/// Split a line on `;` and `&&` outside quotes: (piece, run only if the previous succeeded).
pub fn split_chain(line: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut need_ok = false;
    let cs: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            cur.push(c);
        } else if c == '"' || c == '\'' {
            quote = Some(c);
            cur.push(c);
        } else if c == ';' {
            out.push((cur.trim().to_string(), need_ok));
            cur.clear();
            need_ok = false;
        } else if c == '&' && i + 1 < cs.len() && cs[i + 1] == '&' {
            out.push((cur.trim().to_string(), need_ok));
            cur.clear();
            need_ok = true;
            i += 1;
        } else {
            cur.push(c);
        }
        i += 1;
    }
    out.push((cur.trim().to_string(), need_ok));
    out.retain(|(p, _)| !p.is_empty());
    out
}

/// Did a command's output say it failed? LingOS commands have no exit status; their
/// errors are written "name: ..." with one of these words (for `&&` and the red arrow).
pub fn looks_failed(name: &str, out: &str) -> bool {
    let first = out.lines().next().unwrap_or("").to_ascii_lowercase();
    let mine = first.starts_with(&format!("{name}:")) || first.starts_with("usage:");
    mine && ["no such", "not found", "usage", "denied", "cannot", "failed", "error", "invalid", "refused"].iter().any(|w| first.contains(w))
        || first.contains("command not found")
}

/// The history file (per user, plain text, one line each, newest last).
pub fn history_path(user: &str) -> String {
    format!("home/{user}/.lsh_history")
}

pub fn load_history(user: &str) -> Vec<String> {
    crate::vfs::read(&history_path(user))
        .map(|b| String::from_utf8_lossy(&b).lines().filter(|l| !l.trim().is_empty()).map(String::from).collect::<Vec<_>>())
        .map(|mut v| {
            if v.len() > HIST_MAX {
                v.drain(..v.len() - HIST_MAX);
            }
            v
        })
        .unwrap_or_default()
}

pub fn save_history(user: &str, hist: &[String]) {
    let mut s = String::new();
    for l in hist.iter().rev().take(HIST_MAX).rev() {
        // never keep secrets typed into the shell (2fa codes, passwords given as arguments)
        if l.starts_with("2fa ") || l.starts_with("rename-user ") || l.contains("password") {
            continue;
        }
        s.push_str(l);
        s.push('\n');
    }
    let _ = crate::vfs::write(&history_path(user), s.as_bytes());
}

// ── Interactive commands (`claude`): a program that takes the session over ──────────
// The shell hands the session's input to it and streams its output until it ends. The
// program runs on its own coroutine; the two sides meet in a `Tty` (single core: RefCell).

pub struct Tty {
    /// Bytes the user typed, not yet taken by the program (raw: Ctrl+C is 0x03).
    pub input: Vec<u8>,
    /// Bytes for the client ('\n' is turned into "\r\n" on the way out).
    pub output: Vec<u8>,
    /// Set by the program when it has finished; the shell then prints its prompt.
    pub done: bool,
    /// The client's terminal size.
    pub cols: u16,
    pub rows: u16,
}

pub type TtyRef = alloc::rc::Rc<core::cell::RefCell<Tty>>;

/// Starts an interactive program: (argument text, user, cwd, tty) -> started?
pub type Starter = fn(&str, &str, &str, TtyRef) -> bool;

static mut PROGRAMS: Vec<(&'static str, Starter)> = Vec::new();

/// Register an interactive command (once, at boot or first use).
pub fn register(name: &'static str, start: Starter) {
    let v = unsafe { &mut *(&raw mut PROGRAMS) };
    if !v.iter().any(|(n, _)| *n == name) {
        v.push((name, start));
    }
}

pub fn program(name: &str) -> Option<Starter> {
    unsafe { (*(&raw const PROGRAMS)).iter().find(|(n, _)| *n == name).map(|(_, s)| *s) }
}
