//! ANSI colour for text sent to terminals (the SSH shell, `claude` over SSH): the LingOS
//! design system's ROYGBIV accents (the desktop's ACCENTS) as 24-bit colour. Truecolor is
//! what the clients LingOS meets understand (Windows Terminal, mintty / Git Bash, OpenSSH
//! on Linux and macOS terminals); `NO_COLOR`-style plain output is the caller's choice.

use alloc::format;
use alloc::string::String;

/// Red, orange, yellow, green, blue, indigo, violet -- desktop.ling ACCENTS, exactly.
pub const ROYGBIV: [u32; 7] = [0xED4A37, 0xF7941E, 0xF7D846, 0x45C870, 0x3BAAEB, 0x4A64D8, 0x9A53D6];
pub const RED: u32 = ROYGBIV[0];
pub const ORANGE: u32 = ROYGBIV[1];
pub const YELLOW: u32 = ROYGBIV[2];
pub const GREEN: u32 = ROYGBIV[3];
pub const BLUE: u32 = ROYGBIV[4];
pub const INDIGO: u32 = ROYGBIV[5];
pub const VIOLET: u32 = ROYGBIV[6];

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";

/// Foreground colour 0xRRGGBB.
pub fn fg(rgb: u32) -> String {
    format!("\x1b[38;2;{};{};{}m", (rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF)
}

/// The i-th ROYGBIV colour (wraps).
pub fn roygbiv(i: usize) -> String {
    fg(ROYGBIV[i % 7])
}

/// `text` in `rgb`, then back to the default colour.
pub fn paint(rgb: u32, text: &str) -> String {
    format!("{}{text}{RESET}", fg(rgb))
}

/// `text` with its characters coloured red -> violet across its length (banners, rules).
pub fn gradient(text: &str) -> String {
    let n = text.chars().filter(|c| !c.is_whitespace()).count().max(1);
    let mut out = String::new();
    let mut k = 0usize;
    for c in text.chars() {
        if !c.is_whitespace() {
            // spread the 7 colours over the visible characters
            out.push_str(&roygbiv(k * 7 / n));
            k += 1;
        }
        out.push(c);
    }
    out.push_str(RESET);
    out
}

/// Visible width of a string that may hold ANSI escapes (for cursor arithmetic): escapes
/// count 0, a wide CJK / Hangul / fullwidth character 2, anything else 1.
pub fn width(s: &str) -> usize {
    let mut w = 0usize;
    let mut esc = false;
    for c in s.chars() {
        if esc {
            if c.is_ascii_alphabetic() {
                esc = false;
            }
            continue;
        }
        if c == '\x1b' {
            esc = true;
            continue;
        }
        w += char_width(c);
    }
    w
}

/// Columns a character takes on a terminal (East Asian wide ranges = 2).
pub fn char_width(c: char) -> usize {
    let u = c as u32;
    if u < 0x20 || (0x7F..0xA0).contains(&u) || (0x300..0x370).contains(&u) || u == 0x200B {
        return 0; // control, combining, zero-width
    }
    let wide = (0x1100..=0x115F).contains(&u)
        || (0x2E80..=0x303E).contains(&u)
        || (0x3041..=0x33FF).contains(&u)
        || (0x3400..=0x4DBF).contains(&u)
        || (0x4E00..=0x9FFF).contains(&u)
        || (0xA000..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE30..=0xFE4F).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x1F300..=0x1F64F).contains(&u)
        || (0x1F900..=0x1F9FF).contains(&u)
        || (0x20000..=0x3FFFD).contains(&u);
    if wide { 2 } else { 1 }
}
