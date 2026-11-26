//! Subtitles for the media player (`mediaplay.rs`): SubRip (`.srt`, Matroska
//! `S_TEXT/UTF8`) and SubStation Alpha / Advanced SSA (`.ass` / `.ssa`,
//! `S_TEXT/ASS` / `S_TEXT/SSA`), drawn over the picture in the UI font
//! (`lingapp::ui_font`, DaemonSans, plus its fallback fonts) at any size.
//!
//! What is honoured: styles (font size, primary / outline / back colours and
//! alpha, bold, italic, underline, border, shadow, alignment, margins), and
//! the override tags `\b \i \u \s \c \1c \3c \4c \alpha \1a \3a \4a \fs \bord
//! \shad \an \a \pos \move (its start) \fad \fade (alpha) \r \N \n \h`. Karaoke
//! (`\k`...), transforms (`\t`), clips, rotation, font names and vector
//! drawings (`\p1` .. `\p0`, skipped) are ignored without leaving tag text on
//! screen. SRT's `<b> <i> <u> <font color>` and `{\an8}` work too.
//! Embedded fonts (Matroska attachments) are not used.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

// ── styles and events ────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Style {
    pub name: String,
    pub size: f32,
    /// 0xAARRGGBB with AA = opacity (255 opaque)
    pub primary: u32,
    pub outline_c: u32,
    pub back_c: u32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub border: f32,
    pub shadow: f32,
    /// numpad layout: 1-3 bottom, 4-6 middle, 7-9 top
    pub align: u8,
    pub ml: f32,
    pub mr: f32,
    pub mv: f32,
    /// BorderStyle 3: an opaque box behind the text
    pub boxed: bool,
}

impl Style {
    fn default_for(play_y: f32) -> Style {
        Style {
            name: "Default".to_string(),
            size: play_y / 14.0,
            primary: 0xFFFF_FFFF,
            outline_c: 0xFF00_0000,
            back_c: 0x8000_0000,
            bold: false,
            italic: false,
            underline: false,
            border: (play_y / 180.0).max(1.0),
            shadow: (play_y / 360.0).max(0.5),
            align: 2,
            ml: play_y / 24.0,
            mr: play_y / 24.0,
            mv: play_y / 24.0,
            boxed: false,
        }
    }
}

#[derive(Clone)]
pub struct Event {
    pub start: f64,
    pub end: f64,
    pub layer: i32,
    pub style: usize,
    pub ml: Option<f32>,
    pub mr: Option<f32>,
    pub mv: Option<f32>,
    pub text: String,
    /// order it came in (ASS ReadOrder for Matroska)
    pub order: u64,
}

/// One subtitle track's events and styles.
pub struct Subs {
    pub styles: Vec<Style>,
    pub events: Vec<Event>,
    pub play_x: f32,
    pub play_y: f32,
    pub ass: bool,
    /// Matroska ASS packets: field order after ReadOrder
    seen: BTreeMap<(i64, u64), ()>,
    next_order: u64,
}

/// ASS colour `&HAABBGGRR&` (or decimal) -> 0xAARRGGBB with AA as opacity.
fn ass_colour(s: &str) -> Option<u32> {
    let t = s.trim().trim_start_matches('&').trim_start_matches(['H', 'h']).trim_end_matches('&');
    let v = if s.trim().starts_with('&') || s.trim().starts_with(['H', 'h']) {
        u32::from_str_radix(t, 16).ok()?
    } else {
        let n: i64 = t.parse().ok()?;
        n as u32
    };
    let (a, b, g, r) = ((v >> 24) & 255, (v >> 16) & 255, (v >> 8) & 255, v & 255);
    Some(((255 - a) << 24) | (r << 16) | (g << 8) | b)
}

fn parse_f(s: &str) -> Option<f32> {
    s.trim().parse::<f32>().ok()
}

/// "h:mm:ss.cc" (ASS) or "hh:mm:ss,mmm" (SRT) -> seconds.
fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim().replace(',', ".");
    let mut parts = s.split(':');
    let a: Vec<&str> = (&mut parts).collect();
    let (h, m, sec) = match a.len() {
        3 => (a[0].trim().parse::<f64>().ok()?, a[1].trim().parse::<f64>().ok()?, a[2].trim().parse::<f64>().ok()?),
        2 => (0.0, a[0].trim().parse::<f64>().ok()?, a[1].trim().parse::<f64>().ok()?),
        _ => return None,
    };
    Some(h * 3600.0 + m * 60.0 + sec)
}

/// SSA's legacy alignment (1-3 sub, +4 top, +8 middle) -> numpad.
fn legacy_align(a: u32) -> u8 {
    let col = match a & 3 {
        1 => 1,
        2 => 2,
        3 => 3,
        _ => 2,
    };
    if a & 4 != 0 {
        6 + col
    } else if a & 8 != 0 {
        3 + col
    } else {
        col
    }
}

impl Subs {
    pub fn new(ass: bool) -> Subs {
        let mut s = Subs { styles: Vec::new(), events: Vec::new(), play_x: 384.0, play_y: 288.0, ass, seen: BTreeMap::new(), next_order: 0 };
        if !ass {
            s.play_x = 1280.0;
            s.play_y = 720.0;
        }
        s.styles.push(Style::default_for(s.play_y));
        s
    }

    /// The `[Script Info]` / `[V4+ Styles]` header (Matroska CodecPrivate) or a whole `.ass` / `.ssa` file.
    pub fn load_ass(&mut self, text: &str) {
        let mut section = String::new();
        let mut style_fmt: Vec<String> = Vec::new();
        let mut event_fmt: Vec<String> = Vec::new();
        let mut v4plus = true;
        let mut have_res_y = false;
        let mut styles: Vec<Style> = Vec::new();
        for raw in text.lines() {
            let line = raw.trim_start_matches('\u{feff}').trim();
            if line.starts_with('[') {
                section = line.to_ascii_lowercase();
                if section.starts_with("[v4 styles") {
                    v4plus = false;
                }
                continue;
            }
            let Some(colon) = line.find(':') else { continue };
            let key = line[..colon].trim();
            let val = line[colon + 1..].trim();
            if section == "[script info]" {
                match key.to_ascii_lowercase().as_str() {
                    "playresx" => self.play_x = parse_f(val).filter(|v| *v > 0.0).unwrap_or(self.play_x),
                    "playresy" => {
                        self.play_y = parse_f(val).filter(|v| *v > 0.0).unwrap_or(self.play_y);
                        have_res_y = true;
                    },
                    _ => {},
                }
            } else if section.contains("styles") {
                if key.eq_ignore_ascii_case("format") {
                    style_fmt = val.split(',').map(|f| f.trim().to_ascii_lowercase()).collect();
                } else if key.eq_ignore_ascii_case("style") {
                    let fields: Vec<&str> = val.splitn(style_fmt.len().max(1), ',').collect();
                    let mut st = Style::default_for(288.0);
                    st.border = 2.0;
                    st.shadow = 2.0;
                    st.ml = 10.0;
                    st.mr = 10.0;
                    st.mv = 10.0;
                    st.size = 20.0;
                    for (k, f) in style_fmt.iter().zip(fields.iter()) {
                        let f = f.trim();
                        match k.as_str() {
                            "name" => st.name = f.to_string(),
                            "fontsize" => st.size = parse_f(f).unwrap_or(st.size),
                            "primarycolour" => st.primary = ass_colour(f).unwrap_or(st.primary),
                            "outlinecolour" | "tertiarycolour" => st.outline_c = ass_colour(f).unwrap_or(st.outline_c),
                            "backcolour" => st.back_c = ass_colour(f).unwrap_or(st.back_c),
                            "bold" => st.bold = f != "0",
                            "italic" => st.italic = f != "0",
                            "underline" => st.underline = f != "0",
                            "outline" => st.border = parse_f(f).unwrap_or(st.border),
                            "shadow" => st.shadow = parse_f(f).unwrap_or(st.shadow),
                            "borderstyle" => st.boxed = f == "3",
                            "alignment" => {
                                let a = f.parse::<u32>().unwrap_or(2);
                                st.align = if v4plus { a.clamp(1, 9) as u8 } else { legacy_align(a) };
                            },
                            "marginl" => st.ml = parse_f(f).unwrap_or(st.ml),
                            "marginr" => st.mr = parse_f(f).unwrap_or(st.mr),
                            "marginv" => st.mv = parse_f(f).unwrap_or(st.mv),
                            _ => {},
                        }
                    }
                    styles.push(st);
                }
            } else if section == "[events]" {
                if key.eq_ignore_ascii_case("format") {
                    event_fmt = val.split(',').map(|f| f.trim().to_ascii_lowercase()).collect();
                } else if key.eq_ignore_ascii_case("dialogue") {
                    if event_fmt.is_empty() {
                        event_fmt = ["layer", "start", "end", "style", "name", "marginl", "marginr", "marginv", "effect", "text"].iter().map(|s| s.to_string()).collect();
                    }
                    let fields: Vec<&str> = val.splitn(event_fmt.len(), ',').collect();
                    let mut ev = Event { start: 0.0, end: 0.0, layer: 0, style: 0, ml: None, mr: None, mv: None, text: String::new(), order: self.next_order };
                    self.next_order += 1;
                    let mut style_name = "";
                    for (k, f) in event_fmt.iter().zip(fields.iter()) {
                        match k.as_str() {
                            "layer" | "marked" => ev.layer = f.trim().trim_start_matches("Marked=").parse().unwrap_or(0),
                            "start" => ev.start = parse_time(f).unwrap_or(0.0),
                            "end" => ev.end = parse_time(f).unwrap_or(0.0),
                            "style" => style_name = f.trim(),
                            "marginl" => ev.ml = parse_f(f).filter(|v| *v != 0.0),
                            "marginr" => ev.mr = parse_f(f).filter(|v| *v != 0.0),
                            "marginv" => ev.mv = parse_f(f).filter(|v| *v != 0.0),
                            "text" => ev.text = f.to_string(),
                            _ => {},
                        }
                    }
                    ev.style = self.style_index_in(&styles, style_name);
                    if ev.end > ev.start {
                        self.events.push(ev);
                    }
                }
            }
        }
        if !have_res_y && self.play_x != 384.0 {
            // PlayResX alone: keep the aspect of the common 4:3 default
            self.play_y = self.play_x * 3.0 / 4.0;
        }
        if !styles.is_empty() {
            // event style indices were into `styles`; the list replaces the default
            self.styles = styles;
        }
        self.events.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(core::cmp::Ordering::Equal));
    }

    fn style_index_in(&self, styles: &[Style], name: &str) -> usize {
        let n = name.trim_start_matches('*');
        styles.iter().position(|s| s.name.eq_ignore_ascii_case(n)).or_else(|| styles.iter().position(|s| s.name.eq_ignore_ascii_case("Default"))).unwrap_or(0)
    }

    /// A whole `.srt` file.
    pub fn load_srt(&mut self, text: &str) {
        let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\r', "\n");
        for block in text.split("\n\n") {
            let mut lines = block.lines().map(|l| l.trim_end()).skip_while(|l| l.trim().is_empty());
            let mut first = lines.next().unwrap_or("");
            if !first.contains("-->") {
                first = lines.next().unwrap_or("");
            }
            let Some(arrow) = first.find("-->") else { continue };
            let (Some(a), Some(b)) = (parse_time(&first[..arrow]), parse_time(first[arrow + 3..].split_whitespace().next().unwrap_or(""))) else { continue };
            let body: Vec<&str> = lines.collect();
            self.push_srt(a, b, &body.join("\n"));
        }
        self.events.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(core::cmp::Ordering::Equal));
    }

    fn push_srt(&mut self, start: f64, end: f64, text: &str) {
        let order = self.next_order;
        self.next_order += 1;
        self.events.push(Event { start, end, layer: 0, style: 0, ml: None, mr: None, mv: None, text: srt_to_ass(text), order });
    }

    /// One Matroska subtitle packet (seconds). Packets seen again after a seek are dropped.
    pub fn add_packet(&mut self, start: f64, dur: f64, data: &[u8]) {
        let key = ((start * 1000.0) as i64, data.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3)));
        if self.seen.insert(key, ()).is_some() {
            return;
        }
        let text = String::from_utf8_lossy(data);
        let end = start + if dur > 0.0 { dur } else { 4.0 };
        if !self.ass {
            self.push_srt(start, end, &text);
        } else {
            // ReadOrder, Layer, Style, Name, MarginL, MarginR, MarginV, Effect, Text
            let f: Vec<&str> = text.splitn(9, ',').collect();
            if f.len() < 9 {
                return;
            }
            let order = f[0].trim().parse().unwrap_or(self.next_order);
            self.next_order = self.next_order.max(order + 1);
            let style = self.style_index_in(&self.styles, f[2]);
            self.events.push(Event {
                start,
                end,
                layer: f[1].trim().parse().unwrap_or(0),
                style,
                ml: parse_f(f[4]).filter(|v| *v != 0.0),
                mr: parse_f(f[5]).filter(|v| *v != 0.0),
                mv: parse_f(f[6]).filter(|v| *v != 0.0),
                text: f[8].to_string(),
                order,
            });
        }
        // keep them in time order (packets arrive in order, so this is a short shuffle)
        let mut i = self.events.len() - 1;
        while i > 0 && self.events[i - 1].start > self.events[i].start {
            self.events.swap(i - 1, i);
            i -= 1;
        }
    }

    /// The events showing at `t`.
    fn active(&self, t: f64) -> Vec<&Event> {
        // events are sorted by start: everything after the first start > t is out
        let end = self.events.partition_point(|e| e.start <= t);
        let mut v: Vec<&Event> = self.events[..end].iter().rev().take(400).filter(|e| e.end > t).collect();
        v.sort_by(|a, b| a.layer.cmp(&b.layer).then(a.order.cmp(&b.order)));
        v
    }
}

/// SRT markup -> ASS override tags.
fn srt_to_ass(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s.replace("\r", "");
    rest = rest.replace('\n', "\\N");
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'<' {
            if let Some(j) = rest[i..].find('>') {
                let tag = rest[i + 1..i + j].trim().to_ascii_lowercase();
                let t = match tag.as_str() {
                    "b" => "{\\b1}".to_string(),
                    "/b" => "{\\b0}".to_string(),
                    "i" => "{\\i1}".to_string(),
                    "/i" => "{\\i0}".to_string(),
                    "u" => "{\\u1}".to_string(),
                    "/u" => "{\\u0}".to_string(),
                    "/font" => "{\\c}".to_string(),
                    _ if tag.starts_with("font") => match tag.find("color=") {
                        Some(k) => {
                            let v = tag[k + 6..].trim_matches(|c| c == '"' || c == '\'' || c == ' ');
                            let v = v.split(['"', '\'', ' ']).next().unwrap_or("");
                            match named_or_hex(v) {
                                Some(rgb) => alloc::format!("{{\\c&H{:02X}{:02X}{:02X}&}}", rgb & 255, (rgb >> 8) & 255, (rgb >> 16) & 255),
                                None => String::new(),
                            }
                        },
                        None => String::new(),
                    },
                    _ => String::new(),
                };
                out.push_str(&t);
                i += j + 1;
                continue;
            }
        }
        let ch = rest[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn named_or_hex(v: &str) -> Option<u32> {
    if let Some(h) = v.strip_prefix('#') {
        return u32::from_str_radix(h, 16).ok().map(|x| if h.len() == 3 { ((x >> 8 & 15) * 17) << 16 | ((x >> 4 & 15) * 17) << 8 | (x & 15) * 17 } else { x & 0xFF_FFFF });
    }
    Some(match v {
        "white" => 0xFFFFFF,
        "black" => 0x000000,
        "red" => 0xFF0000,
        "green" => 0x00FF00,
        "lime" => 0x00FF00,
        "blue" => 0x0000FF,
        "yellow" => 0xFFFF00,
        "cyan" | "aqua" => 0x00FFFF,
        "magenta" | "fuchsia" => 0xFF00FF,
        "orange" => 0xFFA500,
        "gray" | "grey" => 0x808080,
        "silver" => 0xC0C0C0,
        _ => return u32::from_str_radix(v, 16).ok(),
    })
}

// ── layout ───────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
struct Fmt {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    primary: u32,
    outline_c: u32,
    back_c: u32,
    /// script pixels
    size: f32,
    border: f32,
    shadow: f32,
}

struct Run {
    fmt: Fmt,
    text: String,
}

/// An event's line-level settings and its runs (lines split at `\N`).
struct Parsed {
    align: u8,
    pos: Option<(f32, f32)>,
    fade: Option<(f64, f64)>,
    lines: Vec<Vec<Run>>,
}

fn tag_args(s: &str) -> Vec<f32> {
    let inner = s.trim().trim_start_matches('(').trim_end_matches(')');
    inner.split(',').filter_map(|x| x.trim().parse::<f32>().ok()).collect()
}

/// Split an event's text at its `{...}` override blocks.
fn parse_event(ev: &Event, st: &Style) -> Parsed {
    let base = Fmt {
        bold: st.bold,
        italic: st.italic,
        underline: st.underline,
        strike: false,
        primary: st.primary,
        outline_c: st.outline_c,
        back_c: st.back_c,
        size: st.size,
        border: st.border,
        shadow: st.shadow,
    };
    let mut p = Parsed { align: st.align, pos: None, fade: None, lines: alloc::vec![Vec::new()] };
    let mut f = base;
    let mut drawing = false;
    let mut cur = String::new();
    let text = &ev.text;
    let mut i = 0;
    let flush = |p: &mut Parsed, cur: &mut String, f: Fmt| {
        if !cur.is_empty() {
            p.lines.last_mut().unwrap().push(Run { fmt: f, text: core::mem::take(cur) });
        }
    };
    while i < text.len() {
        let rest = &text[i..];
        if rest.starts_with('{') {
            let Some(close) = rest.find('}') else {
                break;
            };
            flush(&mut p, &mut cur, f);
            let block = &rest[1..close];
            for tag in block.split('\\').skip(1) {
                let tag = tag.trim();
                // the name: letters (and a leading digit for \1c ...)
                let name_len = tag.char_indices().find(|(k, c)| !(c.is_ascii_alphabetic() || (*k == 0 && c.is_ascii_digit()))).map(|(k, _)| k).unwrap_or(tag.len());
                let (name, arg) = tag.split_at(name_len);
                let arg = arg.trim();
                let on = |a: &str| a.trim().parse::<i32>().map(|v| v != 0).unwrap_or(true);
                match name {
                    "b" => {
                        if arg.is_empty() {
                            f.bold = base.bold
                        } else {
                            f.bold = arg.parse::<i32>().map(|v| v == 1 || v >= 500).unwrap_or(f.bold)
                        }
                    },
                    "i" => f.italic = if arg.is_empty() { base.italic } else { on(arg) },
                    "u" => f.underline = if arg.is_empty() { base.underline } else { on(arg) },
                    "s" => f.strike = if arg.is_empty() { false } else { on(arg) },
                    "c" | "1c" => f.primary = (f.primary & 0xFF00_0000) | if arg.is_empty() { base.primary & 0xFF_FFFF } else { ass_colour(arg).map(|c| c & 0xFF_FFFF).unwrap_or(f.primary & 0xFF_FFFF) },
                    "3c" => f.outline_c = (f.outline_c & 0xFF00_0000) | ass_colour(arg).map(|c| c & 0xFF_FFFF).unwrap_or(base.outline_c & 0xFF_FFFF),
                    "4c" => f.back_c = (f.back_c & 0xFF00_0000) | ass_colour(arg).map(|c| c & 0xFF_FFFF).unwrap_or(base.back_c & 0xFF_FFFF),
                    "alpha" | "1a" | "3a" | "4a" => {
                        let h = arg.trim_start_matches('&').trim_start_matches(['H', 'h']).trim_end_matches('&');
                        let a = 255 - u32::from_str_radix(h, 16).unwrap_or(0).min(255);
                        let set = |c: &mut u32| *c = (*c & 0xFF_FFFF) | (a << 24);
                        match name {
                            "alpha" => {
                                set(&mut f.primary);
                                set(&mut f.outline_c);
                                set(&mut f.back_c);
                            },
                            "1a" => set(&mut f.primary),
                            "3a" => set(&mut f.outline_c),
                            _ => set(&mut f.back_c),
                        }
                    },
                    "fs" => {
                        if let Some(v) = parse_f(arg).filter(|v| *v > 0.0) {
                            f.size = v
                        } else if arg.is_empty() {
                            f.size = base.size
                        }
                    },
                    "bord" => f.border = parse_f(arg).unwrap_or(base.border).max(0.0),
                    "shad" => f.shadow = parse_f(arg).unwrap_or(base.shadow).max(0.0),
                    "an" => {
                        if let Ok(a) = arg.parse::<u8>() {
                            if (1..=9).contains(&a) {
                                p.align = a
                            }
                        }
                    },
                    "a" => {
                        if let Ok(a) = arg.parse::<u32>() {
                            p.align = legacy_align(a)
                        }
                    },
                    "pos" | "move" => {
                        let v = tag_args(arg);
                        if v.len() >= 2 && p.pos.is_none() {
                            p.pos = Some((v[0], v[1]));
                        }
                    },
                    "fad" => {
                        let v = tag_args(arg);
                        if v.len() >= 2 {
                            p.fade = Some((v[0] as f64 / 1000.0, v[1] as f64 / 1000.0));
                        }
                    },
                    "fade" => {
                        let v = tag_args(arg);
                        if v.len() >= 7 {
                            p.fade = Some(((v[4] - v[3]).max(0.0) as f64 / 1000.0, (v[6] - v[5]).max(0.0) as f64 / 1000.0));
                        }
                    },
                    "r" => f = base,
                    "p" => drawing = arg.parse::<i32>().unwrap_or(0) > 0,
                    _ => {}, // \k \K \kf \ko \t \clip \frz \fscx \fn \blur \be ...: ignored
                }
            }
            i += close + 1;
            continue;
        }
        if rest.starts_with("\\N") || rest.starts_with("\\n") {
            // \n is a soft break (only with wrap style 2): a space here
            if rest.starts_with("\\N") {
                flush(&mut p, &mut cur, f);
                p.lines.push(Vec::new());
            } else if !drawing {
                cur.push(' ');
            }
            i += 2;
            continue;
        }
        if rest.starts_with("\\h") {
            if !drawing {
                cur.push('\u{a0}');
            }
            i += 2;
            continue;
        }
        let ch = rest.chars().next().unwrap_or(' ');
        if !drawing {
            cur.push(ch);
        }
        i += ch.len_utf8();
    }
    flush(&mut p, &mut cur, f);
    p
}

// ── glyphs ───────────────────────────────────────────────────────────────────

struct Glyph {
    w: usize,
    h: usize,
    xmin: i32,
    ymin: i32,
    adv: f32,
    cov: Vec<u8>,
}

static mut CACHE: BTreeMap<(char, u16, bool), Glyph> = BTreeMap::new();

fn cache() -> &'static mut BTreeMap<(char, u16, bool), Glyph> {
    unsafe { &mut *(&raw mut CACHE) }
}

fn glyph(c: char, px: u16, bold: bool) -> Option<&'static Glyph> {
    let key = (c, px, bold);
    if !cache().contains_key(&key) {
        if cache().len() > 3000 {
            cache().clear();
        }
        let id = crate::lingapp::ui_font()?;
        let fd = crate::lingapp::font_fd_for(id as f64, c)?;
        let (m, mut cov) = fd.rasterize(c, px as f32);
        let mut w = m.width;
        let mut adv = m.advance_width;
        if bold && m.width > 0 {
            // fake bold: widen each row by a pixel or two
            let k = ((px as usize) / 22).max(1);
            let nw = w + k;
            let mut out = alloc::vec![0u8; nw * m.height];
            for y in 0..m.height {
                for x in 0..nw {
                    let mut v = 0u8;
                    for d in 0..=k {
                        if x >= d && x - d < w {
                            v = v.max(cov[y * w + x - d]);
                        }
                    }
                    out[y * nw + x] = v;
                }
            }
            cov = out;
            w = nw;
            adv += k as f32;
        }
        cache().insert(key, Glyph { w, h: m.height, xmin: m.xmin, ymin: m.ymin, adv, cov });
    }
    cache().get(&key)
}

fn line_metrics(px: f32) -> (f32, f32) {
    let lm = crate::lingapp::ui_font().and_then(|id| crate::lingapp::font_fd_for(id as f64, 'A')).and_then(|fd| fd.horizontal_line_metrics(px));
    match lm {
        Some(m) => (m.ascent, m.new_line_size.max(px)),
        None => (px * 0.8, px * 1.2),
    }
}

// ── drawing ──────────────────────────────────────────────────────────────────

/// Where to draw: the window buffer and its clip.
pub struct Canvas<'a> {
    pub buf: &'a mut [u32],
    pub stride: usize,
    pub clip: [i32; 4],
}

impl Canvas<'_> {
    #[inline]
    fn blend(&mut self, x: i32, y: i32, rgb: u32, a: u32) {
        if a == 0 || x < self.clip[0] || y < self.clip[1] || x >= self.clip[2] || y >= self.clip[3] {
            return;
        }
        let i = y as usize * self.stride + x as usize;
        let Some(d) = self.buf.get_mut(i) else { return };
        if a >= 255 {
            *d = rgb;
            return;
        }
        let ia = 255 - a;
        let dr = (*d >> 16) & 255;
        let dg = (*d >> 8) & 255;
        let db = *d & 255;
        let r = ((rgb >> 16) & 255) * a + dr * ia;
        let g = ((rgb >> 8) & 255) * a + dg * ia;
        let b = (rgb & 255) * a + db * ia;
        *d = ((r / 255) << 16) | ((g / 255) << 8) | (b / 255);
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, rgb: u32, a: u32) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.blend(xx, yy, rgb, a);
            }
        }
    }

    /// A glyph's coverage at (x, y) (its origin on the baseline), italic shear `sh` px per px up.
    fn glyph(&mut self, g: &Glyph, x: f32, base: f32, sh: f32, rgb: u32, alpha: u32) {
        let top = base as i32 - g.ymin - g.h as i32;
        for gy in 0..g.h {
            let yy = top + gy as i32;
            let lift = (base - yy as f32) * sh;
            let x0 = libm::roundf(x + g.xmin as f32 + lift) as i32;
            for gx in 0..g.w {
                let c = g.cov[gy * g.w + gx] as u32;
                if c != 0 {
                    self.blend(x0 + gx as i32, yy, rgb, c * alpha / 255);
                }
            }
        }
    }
}

struct Placed {
    g: &'static Glyph,
    x: f32,
    fmt: Fmt,
}

/// Draw the subtitles showing at `t` over the picture at `rect` (x, y, w, h).
pub fn draw(s: &Subs, t: f64, cv: &mut Canvas, rect: (i32, i32, i32, i32)) {
    let (rx, ry, rw, rh) = rect;
    if rw < 16 || rh < 16 {
        return;
    }
    let sx = rw as f32 / s.play_x;
    let sy = rh as f32 / s.play_y;
    // stacking: how far each numpad alignment has filled up (bottom rows grow upward)
    let mut used_bottom = [0f32; 3];
    let mut used_top = [0f32; 3];
    for ev in s.active(t) {
        let st = s.styles.get(ev.style).unwrap_or(&s.styles[0]);
        let p = parse_event(ev, st);
        // fade alpha
        let mut fade = 1.0f64;
        if let Some((fi, fo)) = p.fade {
            if fi > 0.0 && t - ev.start < fi {
                fade = fade.min((t - ev.start) / fi);
            }
            if fo > 0.0 && ev.end - t < fo {
                fade = fade.min((ev.end - t) / fo);
            }
        }
        let fade = (fade.clamp(0.0, 1.0) * 255.0) as u32;
        let ml = ev.ml.unwrap_or(st.ml) * sx;
        let mr = ev.mr.unwrap_or(st.mr) * sx;
        let mv = ev.mv.unwrap_or(st.mv) * sy;
        let max_w = (rw as f32 - ml - mr).max(rw as f32 * 0.3);
        // lay out: wrap each \N line to max_w (at spaces; anywhere in CJK text)
        let mut lines: Vec<(Vec<Placed>, f32, f32, f32)> = Vec::new(); // glyphs, width, ascent, height
        for runs in &p.lines {
            let mut items: Vec<(char, &'static Glyph, Fmt, f32, f32)> = Vec::new();
            for r in runs {
                let px = (r.fmt.size * sy).clamp(6.0, 400.0);
                let (a, h) = line_metrics(px);
                for c in r.text.chars() {
                    let c = if c == '\u{a0}' || c == '\t' { ' ' } else { c };
                    if let Some(g) = glyph(c, libm::roundf(px) as u16, r.fmt.bold) {
                        items.push((c, g, r.fmt, a, h));
                    }
                }
            }
            if items.is_empty() {
                let (a, h) = line_metrics((st.size * sy).max(6.0));
                lines.push((Vec::new(), 0.0, a, h));
                continue;
            }
            let mut start = 0;
            while start < items.len() {
                let mut x = 0.0f32;
                let mut end = start;
                let mut brk: Option<usize> = None;
                while end < items.len() {
                    let adv = items[end].1.adv;
                    if x + adv > max_w && end > start {
                        break;
                    }
                    if items[end].0 == ' ' {
                        brk = Some(end);
                    }
                    x += adv;
                    end += 1;
                }
                let (stop, next) = if end >= items.len() {
                    (end, end)
                } else {
                    let cjk = (items[end].0 as u32) >= 0x2E80;
                    match brk {
                        Some(b) if b > start && !cjk => (b, b + 1),
                        _ => (end, end),
                    }
                };
                let mut placed = Vec::new();
                let (mut x, mut lw, mut asc, mut hgt) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
                for it in &items[start..stop] {
                    placed.push(Placed { g: it.1, x, fmt: it.2 });
                    x += it.1.adv;
                    if it.0 != ' ' {
                        lw = x;
                    }
                    asc = asc.max(it.3);
                    hgt = hgt.max(it.4);
                }
                lines.push((placed, lw, asc, hgt));
                start = next;
                while start < items.len() && items[start].0 == ' ' {
                    start += 1;
                }
            }
        }
        let total_h: f32 = lines.iter().map(|l| l.3).sum();
        let block_w = lines.iter().map(|l| l.1).fold(0.0, f32::max);
        let col = ((p.align - 1) % 3) as usize; // 0 left, 1 centre, 2 right
        let row = (p.align - 1) / 3; // 0 bottom, 1 middle, 2 top
        // the block's top-left
        let (bx, mut by) = match p.pos {
            Some((px, py)) => {
                let ax = rx as f32 + px * sx;
                let ay = ry as f32 + py * sy;
                let bx = match col {
                    0 => ax,
                    1 => ax - block_w / 2.0,
                    _ => ax - block_w,
                };
                let by = match row {
                    0 => ay - total_h,
                    1 => ay - total_h / 2.0,
                    _ => ay,
                };
                (bx, by)
            },
            None => {
                let bx = match col {
                    0 => rx as f32 + ml,
                    1 => rx as f32 + ml + (rw as f32 - ml - mr - block_w) / 2.0,
                    _ => rx as f32 + rw as f32 - mr - block_w,
                };
                let by = match row {
                    0 => {
                        let y = ry as f32 + rh as f32 - mv - total_h - used_bottom[col];
                        used_bottom[col] += total_h;
                        y
                    },
                    1 => ry as f32 + (rh as f32 - total_h) / 2.0,
                    _ => {
                        let y = ry as f32 + mv + used_top[col];
                        used_top[col] += total_h;
                        y
                    },
                };
                (bx, by)
            },
        };
        // each line: box / shadow, outline, fill
        for (gl, lw, asc, h) in &lines {
            let lx = match col {
                0 => bx,
                1 => bx + (block_w - lw) / 2.0,
                _ => bx + block_w - lw,
            };
            let base = by + asc;
            if st.boxed && *lw > 0.0 {
                let pad = (st.border * sy).max(2.0);
                let c = gl.first().map(|q| q.fmt.outline_c).unwrap_or(st.outline_c);
                cv.fill((lx - pad) as i32, (by - pad) as i32, (lw + 2.0 * pad) as i32, (h + 2.0 * pad) as i32, c & 0xFF_FFFF, (c >> 24) * fade / 255);
            }
            for pass in 0..3 {
                for q in gl {
                    let f = q.fmt;
                    let sh = if f.italic { 0.2 } else { 0.0 };
                    let gx = lx + q.x;
                    let bord = (f.border * sy).min(12.0);
                    let shad = (f.shadow * sy).min(12.0);
                    match pass {
                        0 if shad > 0.0 && !st.boxed => {
                            let a = (f.back_c >> 24) * fade / 255;
                            draw_outlined(cv, q.g, gx + shad, base + shad, sh, bord, f.back_c & 0xFF_FFFF, a);
                        },
                        1 if bord > 0.0 && !st.boxed => {
                            let a = (f.outline_c >> 24) * fade / 255;
                            draw_outlined(cv, q.g, gx, base, sh, bord, f.outline_c & 0xFF_FFFF, a);
                        },
                        2 => {
                            let a = (f.primary >> 24) * fade / 255;
                            cv.glyph(q.g, gx, base, sh, f.primary & 0xFF_FFFF, a);
                            let thick = ((f.size * sy) / 16.0).max(1.0) as i32;
                            if f.underline {
                                cv.fill(gx as i32, (base + 2.0) as i32, libm::ceilf(q.g.adv) as i32, thick, f.primary & 0xFF_FFFF, a);
                            }
                            if f.strike {
                                cv.fill(gx as i32, (base - asc * 0.3) as i32, libm::ceilf(q.g.adv) as i32, thick, f.primary & 0xFF_FFFF, a);
                            }
                        },
                        _ => {},
                    }
                }
            }
            by += h;
        }
    }
}

/// The glyph thickened by `r` pixels (stamped around a ring), in one colour.
fn draw_outlined(cv: &mut Canvas, g: &Glyph, x: f32, base: f32, sh: f32, r: f32, rgb: u32, a: u32) {
    if r <= 0.0 {
        cv.glyph(g, x, base, sh, rgb, a);
        return;
    }
    // a max-filtered coverage map: (w + 2R) x (h + 2R)
    let rr = libm::ceilf(r) as usize;
    let (w, h) = (g.w + 2 * rr, g.h + 2 * rr);
    if w * h > 600 * 600 {
        return;
    }
    let mut m = alloc::vec![0u8; w * h];
    let r2 = r * r;
    let offs: Vec<(isize, isize)> = (-(rr as isize)..=rr as isize)
        .flat_map(|dy| (-(rr as isize)..=rr as isize).map(move |dx| (dx, dy)))
        .filter(|&(dx, dy)| ((dx * dx + dy * dy) as f32) <= r2 + 0.5)
        .collect();
    for gy in 0..g.h {
        for gx in 0..g.w {
            let c = g.cov[gy * g.w + gx];
            if c == 0 {
                continue;
            }
            for &(dx, dy) in &offs {
                let i = (gy as isize + rr as isize + dy) as usize * w + (gx as isize + rr as isize + dx) as usize;
                if m[i] < c {
                    m[i] = c;
                }
            }
        }
    }
    let og = Glyph { w, h, xmin: g.xmin - rr as i32, ymin: g.ymin - rr as i32, adv: g.adv, cov: m };
    cv.glyph(&og, x, base, sh, rgb, a);
}
