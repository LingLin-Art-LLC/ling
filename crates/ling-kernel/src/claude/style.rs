//! How the session's output looks: each piece of text has a role; SSH turns roles into
//! ANSI colours (24-bit, 256 or 16 colours, or none), the kernel Terminal into its line
//! classes. The palette is the desktop's ROYGBIV accents (desktop.ling ACCENTS):
//! red #ed4a37, orange #f7941e, yellow #f7d846, green #45c870, blue #3baaeb,
//! indigo #4a64d8, violet #9a53d6 -- the same the SSH prompt uses.
//! (When drivers/ansi.rs lands, `fg` can move onto it; the values are the same.)

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Role {
    /// Claude's own words (the terminal's default colour)
    Text,
    /// thinking marks, token / cost footers, hints
    Dim,
    /// the "> " input prompt
    Prompt,
    /// the cwd / model line
    Where,
    /// a shell command Claude runs
    Cmd,
    /// a file path in a tool call
    Path,
    /// a diff's added / removed lines
    Add,
    Del,
    /// an approval question
    Ask,
    /// errors, refusals
    Error,
    /// the welcome box (a ROYGBIV gradient)
    Banner,
    /// confirmations ("approved", "saved")
    Ok,
}

pub const ROYGBIV: [(u8, u8, u8); 7] = [(0xED, 0x4A, 0x37), (0xF7, 0x94, 0x1E), (0xF7, 0xD8, 0x46), (0x45, 0xC8, 0x70), (0x3B, 0xAA, 0xEB), (0x4A, 0x64, 0xD8), (0x9A, 0x53, 0xD6)];
/// the 256-colour fallbacks of the same seven (red 196, orange 208, yellow 226, green 46, blue 33, indigo 63, violet 135)
const XTERM256: [u8; 7] = [196, 208, 226, 46, 33, 63, 135];
/// and the 16-colour ones (bright red, yellow, bright yellow, bright green, bright cyan, blue, magenta)
const ANSI16: [u8; 7] = [91, 33, 93, 92, 96, 34, 35];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Colors {
    True,
    C256,
    C16,
    None,
}

fn fg(c: Colors, i: usize) -> String {
    let i = i.min(6);
    match c {
        Colors::True => {
            let (r, g, b) = ROYGBIV[i];
            alloc::format!("\x1b[38;2;{r};{g};{b}m")
        },
        Colors::C256 => alloc::format!("\x1b[38;5;{}m", XTERM256[i]),
        Colors::C16 => alloc::format!("\x1b[{}m", ANSI16[i]),
        Colors::None => String::new(),
    }
}

fn dim(c: Colors) -> &'static str {
    if c == Colors::None { "" } else { "\x1b[2m" }
}

fn reset(c: Colors) -> &'static str {
    if c == Colors::None { "" } else { "\x1b[0m" }
}

/// Text in a ROYGBIV gradient, one colour step per character run (for the banner).
pub fn gradient(c: Colors, s: &str) -> String {
    if c == Colors::None {
        return String::from(s);
    }
    let n = s.chars().count().max(1);
    let mut out = String::new();
    for (k, ch) in s.chars().enumerate() {
        out.push_str(&fg(c, k * 7 / n));
        out.push(ch);
    }
    out.push_str(reset(c));
    out
}

/// One styled piece as ANSI text (lines end in "\n"; sshd turns them into "\r\n").
pub fn ansi(c: Colors, role: Role, s: &str) -> String {
    if c == Colors::None {
        return String::from(s);
    }
    match role {
        Role::Text => String::from(s),
        Role::Banner => {
            let mut out = String::new();
            for line in s.split_inclusive('\n') {
                let (body, nl) = match line.strip_suffix('\n') {
                    Some(b) => (b, "\n"),
                    None => (line, ""),
                };
                out.push_str(&gradient(c, body));
                out.push_str(nl);
            }
            out
        },
        _ => {
            let (pre, bold) = match role {
                Role::Dim => (String::from(dim(c)), false),
                Role::Prompt => (fg(c, 6), true),
                Role::Where => (fg(c, 1), false),
                Role::Cmd => (fg(c, 1), true),
                Role::Path => (fg(c, 4), false),
                Role::Add => (fg(c, 3), false),
                Role::Del => (fg(c, 0), false),
                Role::Ask => (fg(c, 2), true),
                Role::Error => (fg(c, 0), true),
                Role::Ok => (fg(c, 3), false),
                Role::Text | Role::Banner => (String::new(), false),
            };
            let mut out = String::new();
            out.push_str(&pre);
            if bold {
                out.push_str("\x1b[1m");
            }
            out.push_str(s);
            out.push_str(reset(c));
            out
        },
    }
}

/// A run of output pieces, rendered for SSH.
pub fn render(c: Colors, segs: &[(Role, String)]) -> String {
    let mut out = String::new();
    for (r, s) in segs {
        out.push_str(&ansi(c, *r, s));
    }
    out
}

/// The welcome box, sized to its longest line, plus the cwd / model line under it.
pub fn banner(cwd_shown: &str, model: &str) -> Vec<(Role, String)> {
    let lines = ["Welcome to LingOS Claude", "Enter a prompt to start."];
    let w = lines.iter().map(|l| l.len()).max().unwrap_or(0) + 6;
    let bar: String = core::iter::repeat('#').take(w).collect();
    let mut s = String::new();
    s.push_str(&bar);
    s.push('\n');
    for (k, l) in lines.iter().enumerate() {
        let pad = w - 6 - l.len();
        s.push_str("## ");
        s.push_str(l);
        for _ in 0..pad {
            s.push(' ');
        }
        s.push_str(" ##\n");
        if k == 0 {
            s.push_str(&bar);
            s.push('\n');
        }
    }
    s.push_str(&bar);
    s.push('\n');
    alloc::vec![
        (Role::Banner, s),
        (Role::Where, alloc::format!("{cwd_shown}")),
        (Role::Dim, alloc::format!("  {model}  -- /help for commands, Ctrl+C stops a reply, /exit leaves\n")),
    ]
}
