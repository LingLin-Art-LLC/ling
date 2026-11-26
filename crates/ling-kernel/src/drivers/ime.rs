//! Input methods: typing any script on a keyboard that sends ASCII.
//!
//! The keyboard drivers (PS/2 `keyboard.rs`, USB `xhci.rs`, the serial line) all deliver US-layout
//! ASCII. This layer sits between them and whoever takes text (the Ling app runtime's `text_poll`,
//! the desktop's windows) and turns that ASCII into Unicode in the current input method:
//!
//!   English    passes through (the default)
//!   ไทย        Thai, phonetic (ime/th.rs) or the Kedmanee layout
//!   한국어      Korean, phonetic (Revised Romanization, ime/ko.rs) or 2-set (dubeolsik)
//!   日本語      romaji to kana (ime/kana.rs), Space converts to kanji (ime/dict.rs, mozc OSS data)
//!   中文        pinyin to hanzi (ime/dict.rs, CC-CEDICT + jieba frequencies)
//!   Daemon     the stacked Daemon script: syllables built like Hangul, kept at private-use
//!              code points the Daemon stacked font draws (ime/ko.rs `to_daemon`)
//!   Accents    US-International dead keys: ' ` ^ ~ " then a letter
//!   Русский    the ЙЦУКЕН layout;  Ελληνικά  the Greek layout;  U+  a hex code point
//!
//! Which methods are on, their order, the default and the switch keys are per user
//! (Settings > Input, saved in `~/Programs/LingOS/input.conf`). What is still being composed
//! is held back as the pre-edit (drawn at the caret by the app runtime) and only finished text
//! goes to the program, so programs that edit their text with Backspace never see characters
//! replaced under them.
use alloc::string::String;
use alloc::vec::Vec;

pub mod dict;
pub mod kana;
pub mod ko;
pub mod th;

pub use ko::is_daemon;

/// The Daemon stacked font: DaemonChesilin's Hangul-block glyphs at U+F0000.. (see `ko::to_daemon`).
pub static DAEMON_TTF: &[u8] = include_bytes!("../../assets/ime/daemon_stacked.ttf");
static JA_Z: &[u8] = include_bytes!("../../assets/ime/ja_dict.z");
static ZH_Z: &[u8] = include_bytes!("../../assets/ime/zh_dict.z");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Latin,
    Thai,
    Korean,
    Japanese,
    Chinese,
    Daemon,
    Accents,
    Russian,
    Greek,
    Hex,
}

const ALL: [Mode; 10] =
    [Mode::Latin, Mode::Thai, Mode::Korean, Mode::Japanese, Mode::Chinese, Mode::Daemon, Mode::Accents, Mode::Russian, Mode::Greek, Mode::Hex];

fn mode_id(m: Mode) -> &'static str {
    match m {
        Mode::Latin => "en",
        Mode::Thai => "th",
        Mode::Korean => "ko",
        Mode::Japanese => "ja",
        Mode::Chinese => "zh",
        Mode::Daemon => "daemon",
        Mode::Accents => "intl",
        Mode::Russian => "ru",
        Mode::Greek => "el",
        Mode::Hex => "hex",
    }
}

fn mode_of(id: &str) -> Option<Mode> {
    ALL.iter().copied().find(|&m| mode_id(m) == id)
}

/// A key the input method may take.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Char(char),
    Backspace,
    Enter,
    Escape,
    Tab,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    /// a function key (5..12)
    F(u8),
}

/// What became of a key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Took {
    /// not the input method's: the program gets the key as usual
    Pass,
    /// ordinary typing: the text (if any) is in `out`, the program may see the key too
    Text,
    /// the input method used it (composing, picking a candidate): the program must not see it
    Composed,
}

/// The switch shortcuts.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    SuperSpace,
    CtrlShift,
    CtrlSpace,
    FKey(u8),
}

pub const KEY_SUPER_SPACE: u8 = 1;
pub const KEY_CTRL_SHIFT: u8 = 2;
pub const KEY_CTRL_SPACE: u8 = 4;

struct Config {
    order: [Mode; 10],
    enabled: u16,
    default: Mode,
    keys: u8,
    /// an extra switch key: 0 none, or F5 / F8 / F9 / F10 / F12
    fkey: u8,
    ko_2set: bool,
    ja_kata: bool,
    ja_kanji: bool,
    zh_trad: bool,
    zh_tones: bool,
    th_kedmanee: bool,
    /// the candidate box moved from its place at the caret (`box_offset`)
    box_dx: i32,
    box_dy: i32,
}

const fn bit(m: Mode) -> u16 {
    1 << (m as u16)
}

const DEFAULT_ENABLED: u16 =
    bit(Mode::Latin) | bit(Mode::Thai) | bit(Mode::Korean) | bit(Mode::Japanese) | bit(Mode::Chinese) | bit(Mode::Daemon);

static mut CFG: Config = Config {
    order: ALL,
    enabled: DEFAULT_ENABLED,
    default: Mode::Latin,
    keys: KEY_SUPER_SPACE | KEY_CTRL_SHIFT | KEY_CTRL_SPACE,
    fkey: 0,
    ko_2set: false,
    ja_kata: false,
    ja_kanji: true,
    zh_trad: false,
    zh_tones: false,
    th_kedmanee: false,
    box_dx: 0,
    box_dy: 0,
};

fn cfg() -> &'static mut Config {
    unsafe { &mut *(&raw mut CFG) }
}

struct State {
    mode: Mode,
    /// what has been typed for the composition: romanized text, a dead key, hex digits
    raw: String,
    /// Hangul 2-set syllable being built: initial, medial, final (compatibility jamo)
    l: Option<char>,
    v: Option<char>,
    t: Option<char>,
    /// Japanese after Space: the words and the spelling picked for each
    segs: Vec<dict::Seg>,
    pick: Vec<usize>,
    focus: usize,
    /// Japanese F6 / F7: the kana shown as hiragana (1) or katakana (2), not converted
    kana_form: u8,
    /// Chinese and Thai: the live candidates and the highlighted one
    cands: Vec<String>,
    cand_syls: Vec<usize>,
    sel: usize,
    /// when the method last changed (the on-screen hint)
    hint_at: u64,
}

static mut ST: State = State {
    mode: Mode::Latin,
    raw: String::new(),
    l: None,
    v: None,
    t: None,
    segs: Vec::new(),
    pick: Vec::new(),
    focus: 0,
    kana_form: 0,
    cands: Vec::new(),
    cand_syls: Vec::new(),
    sel: 0,
    hint_at: 0,
};

fn st() -> &'static mut State {
    unsafe { &mut *(&raw mut ST) }
}

const PAGE: usize = 9;

fn now_ms() -> u64 {
    crate::arch::timer::now_ms()
}

// ── dictionaries (decompressed the first time they are needed) ──

static mut JA: Option<dict::Dict> = None;
static mut ZH: Option<dict::Dict> = None;

fn load_dict(z: &[u8], zh: bool) -> Option<dict::Dict> {
    let t0 = now_ms();
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib(z).ok()?;
    let d = dict::Dict::new(raw, zh);
    crate::console_write(
        alloc::format!("ime: {} dictionary ready ({} entries, {} ms)\n", if zh { "Chinese" } else { "Japanese" }, d.len(), now_ms().saturating_sub(t0)).as_bytes(),
    );
    Some(d)
}

fn ja_dict() -> Option<&'static dict::Dict> {
    unsafe {
        let p = &mut *(&raw mut JA);
        if p.is_none() {
            *p = load_dict(JA_Z, false);
        }
        p.as_ref()
    }
}

fn zh_dict() -> Option<&'static dict::Dict> {
    unsafe {
        let p = &mut *(&raw mut ZH);
        if p.is_none() {
            *p = load_dict(ZH_Z, true);
        }
        p.as_ref()
    }
}

// ── where text is being typed (reported by the program each frame) ──

#[derive(Clone, Copy)]
pub struct Field {
    pub x: i32,
    pub y: i32,
    pub h: i32,
    pub secret: bool,
}

/// This frame's text field (the caret), if the program reported one.
static mut FIELD: Option<Field> = None;
/// The last one, kept for drawing while the program is between frames.
static mut FIELD_SHOWN: Option<Field> = None;
/// Programs that report their text fields: for them the input method only composes while a
/// field is reported (elsewhere keys are plain keys). Others always get the input method.
static mut AWARE: Vec<String> = Vec::new();
/// Signed out (the sign-in screen): plain typing only.
static mut SUSPENDED: bool = false;
/// Plain typing for this batch of keys (a password box, no text field, signed out).
static mut BYPASS: bool = false;

/// When the field was last reported, and by which program (a field another program reported, or
/// one from long ago, says nothing about where the caret is now).
static mut FIELD_AT: u64 = 0;
static mut FIELD_APP: String = String::new();

/// The program `app` draws its caret at (x, y), `h` tall; `secret`: a password box. (x, y, h) are
/// in screen pixels -- the frame the input method draws on (lingapp.rs maps the program's).
pub fn report_field(app: &str, x: i32, y: i32, h: i32, secret: bool) {
    unsafe {
        FIELD = Some(Field { x, y, h: h.max(8), secret });
        FIELD_AT = now_ms();
        if *(&raw const FIELD_APP) != app {
            *(&raw mut FIELD_APP) = String::from(app);
        }
        let a = &mut *(&raw mut AWARE);
        if !a.iter().any(|n| n == app) {
            a.push(String::from(app));
        }
    }
}

/// Before a frame's keys: decide whether they go through the input method.
pub fn begin_keys(app: &str) {
    unsafe {
        let f = *(&raw const FIELD);
        let aware = (*(&raw const AWARE)).iter().any(|n| n == app);
        BYPASS = SUSPENDED || f.is_some_and(|f| f.secret) || (aware && f.is_none());
    }
}

/// After a frame's keys: the field report is per frame.
pub fn end_frame() {
    unsafe {
        FIELD_SHOWN = *(&raw const FIELD);
        FIELD = None;
    }
}

/// Where to draw the pre-edit: the caret `app` reported this frame (or in the last moment).
pub fn field(app: &str) -> Option<Field> {
    unsafe {
        if *(&raw const FIELD_APP) != app || now_ms().saturating_sub(FIELD_AT) > 600 {
            return None;
        }
        (*(&raw const FIELD)).or(*(&raw const FIELD_SHOWN))
    }
}

// ── finding the caret of a program that does not report it ──
//
// Most programs draw their caret as a thin upright bar (px_rect, 1-3 px wide) that blinks. The app
// runtime hands every such bar here (`caret_rect`, screen pixels); a bar that went away and came
// back at the same place is a blinking caret. Grid lines, waveforms and the like are drawn every
// frame (they never blink) and scrolled content moves (a new place each frame), so neither counts.

struct Seen {
    frame: u64,
    last_ms: u64,
    appear_ms: u64,
    appears: u32,
}

static mut SEEN: alloc::collections::BTreeMap<(i32, i32, i32), Seen> = alloc::collections::BTreeMap::new();
static mut SEEN_FRAME: u64 = 1;
/// Did the program draw anything this frame? (a frame it did not draw again proves no blink)
static mut DREW: bool = false;

pub fn frame_drew() {
    unsafe { DREW = true };
}

/// A thin upright bar with its right edge at x, top y, `h` tall was drawn this frame.
pub fn caret_rect(x: i32, y: i32, h: i32) {
    let seen = unsafe { &mut *(&raw mut SEEN) };
    let frame = unsafe { SEEN_FRAME };
    let now = now_ms();
    let full = seen.len() >= 1024;
    match seen.get_mut(&(x, y, h)) {
        Some(e) => {
            if e.frame + 1 < frame {
                // it was gone for a frame or more and is back: a blink
                e.appears += 1;
                e.appear_ms = now;
            }
            e.frame = frame;
            e.last_ms = now;
        },
        None if !full => {
            seen.insert((x, y, h), Seen { frame, last_ms: now, appear_ms: 0, appears: 0 });
        },
        None => {},
    }
}

/// End of a frame of thin bars: forget the ones long gone.
pub fn caret_frame_end() {
    let now = now_ms();
    unsafe {
        if !DREW {
            return;
        }
        DREW = false;
        SEEN_FRAME += 1;
        (*(&raw mut SEEN)).retain(|_, e| now.saturating_sub(e.last_ms) < if e.appears > 0 { 2500 } else { 1200 });
    }
}

/// A new program (or none): its bars start over.
pub fn caret_forget() {
    unsafe { (*(&raw mut SEEN)).clear() };
}

/// The blinking caret seen most recently, if any.
pub fn auto_caret() -> Option<Field> {
    let now = now_ms();
    let seen = unsafe { &*(&raw const SEEN) };
    seen.iter()
        .filter(|(_, e)| e.appears > 0 && now.saturating_sub(e.last_ms) < 1500 && now.saturating_sub(e.appear_ms) < 2500)
        .max_by_key(|(_, e)| e.appear_ms)
        .map(|(&(x, y, h), _)| Field { x, y, h, secret: false })
}

// ── where the candidate box sits: next to the caret, moved by the user's drag ──

/// The user's offset of the candidate box from its place at the caret (drag its handle;
/// double-click the handle or Settings > Input to put it back). Saved with the settings.
pub fn box_offset() -> (i32, i32) {
    let c = cfg();
    (c.box_dx, c.box_dy)
}

pub fn set_box_offset(dx: i32, dy: i32, keep: bool) {
    let c = cfg();
    c.box_dx = dx.clamp(-8000, 8000);
    c.box_dy = dy.clamp(-8000, 8000);
    if keep && !unsafe { SUSPENDED } {
        save();
    }
}

/// Back to its place at the caret.
pub fn box_reset() {
    set_box_offset(0, 0, true);
}

/// Pick candidate `i` (of the whole list, as `view().cands` numbers them) with the pointer.
pub fn pick(i: usize, out: &mut String) -> bool {
    let s = st();
    match s.mode {
        Mode::Chinese if i < s.cands.len() => {
            zh_take(s, i, out);
            true
        },
        Mode::Japanese if !s.segs.is_empty() => {
            let f = s.focus.min(s.segs.len() - 1);
            if i >= s.segs[f].cands.len() {
                return false;
            }
            s.pick[f] = i;
            if f + 1 < s.segs.len() {
                s.focus = f + 1;
            } else {
                flush(out);
            }
            true
        },
        Mode::Thai if i < s.cands.len() => {
            s.sel = i;
            flush(out);
            true
        },
        _ => false,
    }
}

/// The kernel's own text input (the native Terminal, drivers/terminal.rs): its caret, when
/// it was last drawn, and whether it is a password prompt.
static mut NATIVE: Option<(Field, u64)> = None;

/// The native Terminal draws its input line (caret at x, y, h tall; `secret`: a password prompt).
pub fn report_native(x: i32, y: i32, h: i32, secret: bool) {
    unsafe { NATIVE = Some((Field { x, y, h: h.max(8), secret }, now_ms())) };
}

/// The native caret if it was drawn in the last half second.
pub fn native_field() -> Option<Field> {
    unsafe { (*(&raw const NATIVE)).filter(|(_, t)| now_ms().saturating_sub(*t) < 500).map(|(f, _)| f) }
}

/// Before a key in the native window manager (wm.rs): compose only into the native Terminal's
/// command line (not its password prompts, not when signed out).
pub fn begin_native() {
    unsafe {
        let f = native_field();
        BYPASS = SUSPENDED || f.is_none() || f.is_some_and(|f| f.secret);
    }
}

/// Plain typing right now (no composing): a password box, no text field, signed out.
pub fn bypassed() -> bool {
    unsafe { BYPASS }
}

/// The sign-in screen: plain typing until `load` (after sign-in).
pub fn suspend() {
    unsafe { SUSPENDED = true };
    reset();
}

// ── state queries ──

pub fn mode() -> Mode {
    st().mode
}

/// Anything other than plain English (the indicator shows then).
pub fn active() -> bool {
    st().mode != Mode::Latin && !bypassed()
}

/// Is something being composed?
pub fn composing() -> bool {
    let s = st();
    !s.raw.is_empty() || s.l.is_some() || s.v.is_some() || !s.segs.is_empty()
}

/// Abandon a half-finished composition. The chosen method is kept. Called at session/app
/// boundaries so a stray composition never leaks into the next field.
pub fn reset() {
    let s = st();
    s.raw.clear();
    s.l = None;
    s.v = None;
    s.t = None;
    clear_conv(s);
}

fn clear_conv(s: &mut State) {
    s.segs.clear();
    s.pick.clear();
    s.focus = 0;
    s.kana_form = 0;
    s.cands.clear();
    s.cand_syls.clear();
    s.sel = 0;
}

fn badge_of(m: Mode) -> String {
    let c = cfg();
    String::from(match m {
        Mode::Latin => "EN",
        Mode::Thai => "ไทย",
        Mode::Korean => "한",
        Mode::Japanese => {
            if c.ja_kata {
                "ア"
            } else {
                "あ"
            }
        },
        Mode::Chinese => {
            if c.zh_trad {
                "繁"
            } else {
                "拼"
            }
        },
        Mode::Daemon => return ko::to_daemon("데"),
        Mode::Accents => "Áa",
        Mode::Russian => "Ру",
        Mode::Greek => "Ελ",
        Mode::Hex => "U+",
    })
}

fn name_of(m: Mode) -> &'static str {
    match m {
        Mode::Latin => "English (US)",
        Mode::Thai => "ไทย Thai",
        Mode::Korean => "한국어 Korean",
        Mode::Japanese => "日本語 Japanese",
        Mode::Chinese => "中文 Chinese (pinyin)",
        Mode::Daemon => "Daemon (stacked)",
        Mode::Accents => "US International (accents)",
        Mode::Russian => "Русский Russian",
        Mode::Greek => "Ελληνικά Greek",
        Mode::Hex => "Unicode (hex code point)",
    }
}

fn option_of(m: Mode) -> &'static str {
    let c = cfg();
    match m {
        Mode::Thai => {
            if c.th_kedmanee {
                "Kedmanee keyboard"
            } else {
                "Phonetic (RTGS)"
            }
        },
        Mode::Korean => {
            if c.ko_2set {
                "Dubeolsik keyboard"
            } else {
                "Phonetic (RR)"
            }
        },
        Mode::Japanese => match (c.ja_kata, c.ja_kanji) {
            (false, true) => "Hiragana + kanji",
            (true, true) => "Katakana + kanji",
            (false, false) => "Hiragana only",
            (true, false) => "Katakana only",
        },
        Mode::Chinese => match (c.zh_trad, c.zh_tones) {
            (false, false) => "Simplified",
            (true, false) => "Traditional",
            (false, true) => "Simplified, tone digits",
            (true, true) => "Traditional, tone digits",
        },
        _ => "",
    }
}

/// The current method's short badge for the indicator.
pub fn label() -> String {
    badge_of(st().mode)
}

pub fn name() -> &'static str {
    name_of(st().mode)
}

/// What is being composed, as shown ("" when nothing is).
pub fn preedit() -> String {
    view().text
}

/// The composition as the UI draws it.
pub struct View {
    pub text: String,
    /// the focused part (chars) of `text`: highlighted while converting
    pub focus: (usize, usize),
    pub cands: Vec<String>,
    pub sel: usize,
}

pub fn view() -> View {
    let s = st();
    let mut v = View { text: String::new(), focus: (0, 0), cands: Vec::new(), sel: 0 };
    match s.mode {
        Mode::Korean if cfg().ko_2set => {
            if let Some(c) = hangul_current(s) {
                v.text.push(c);
            }
        },
        Mode::Korean => v.text = ko::romaja_to_hangul(&s.raw),
        Mode::Daemon => v.text = ko::to_daemon(&ko::romaja_to_hangul(&s.raw)),
        Mode::Thai => {
            v.text = s.cands.get(s.sel).cloned().unwrap_or_else(|| s.raw.clone());
            if s.cands.len() > 1 {
                v.cands = s.cands.clone();
                v.sel = s.sel;
            }
        },
        Mode::Japanese => {
            if !s.segs.is_empty() {
                let mut at = 0usize;
                for (k, seg) in s.segs.iter().enumerate() {
                    let w = &seg.cands[s.pick[k].min(seg.cands.len() - 1)];
                    let n = w.chars().count();
                    if k == s.focus {
                        v.focus = (at, at + n);
                    }
                    at += n;
                    v.text.push_str(w);
                }
                if let Some(seg) = s.segs.get(s.focus) {
                    v.cands = seg.cands.clone();
                    v.sel = s.pick[s.focus];
                }
            } else {
                v.text = ja_kana_now(s, false);
            }
        },
        Mode::Chinese => {
            v.text = s.raw.clone();
            v.cands = s.cands.clone();
            v.sel = s.sel;
        },
        Mode::Hex if !s.raw.is_empty() => {
            v.text.push_str("U+");
            v.text.push_str(&s.raw);
        },
        _ => v.text = s.raw.clone(),
    }
    v
}

/// The method just switched to, for an on-screen hint (shown about 1.2 s).
pub fn hint() -> Option<String> {
    let s = st();
    if s.hint_at != 0 && now_ms().saturating_sub(s.hint_at) < 1200 {
        let mut t = badge_of(s.mode);
        t.push_str("  ");
        t.push_str(name_of(s.mode));
        return Some(t);
    }
    None
}

// ── switching ──

fn enabled(m: Mode) -> bool {
    m == Mode::Latin || cfg().enabled & bit(m) != 0
}

/// The next enabled method in the user's order. Whatever was composed is committed first.
pub fn cycle(out: &mut String) {
    flush(out);
    let c = cfg();
    let s = st();
    let i = c.order.iter().position(|&m| m == s.mode).unwrap_or(0);
    for k in 1..=10 {
        let m = c.order[(i + k) % 10];
        if enabled(m) {
            s.mode = m;
            break;
        }
    }
    s.hint_at = now_ms();
}

/// A switch shortcut was pressed: cycle when that shortcut is on. False: it is off.
pub fn shortcut(k: Shortcut, out: &mut String) -> bool {
    let c = cfg();
    let on = match k {
        Shortcut::SuperSpace => c.keys & KEY_SUPER_SPACE != 0,
        Shortcut::CtrlShift => c.keys & KEY_CTRL_SHIFT != 0,
        Shortcut::CtrlSpace => c.keys & KEY_CTRL_SPACE != 0,
        Shortcut::FKey(n) => c.fkey != 0 && c.fkey == n,
    };
    if on && !unsafe { SUSPENDED } {
        cycle(out);
    }
    on
}

pub fn set_mode(m: Mode, out: &mut String) {
    flush(out);
    st().mode = m;
    st().hint_at = now_ms();
}

// ── typing ──

/// Commit what is being composed (before Tab, a click, a method change).
pub fn flush(out: &mut String) {
    let s = st();
    match s.mode {
        Mode::Korean if cfg().ko_2set => hangul_commit(s, out),
        Mode::Korean => out.push_str(&ko::romaja_to_hangul(&s.raw)),
        Mode::Daemon => out.push_str(&ko::to_daemon(&ko::romaja_to_hangul(&s.raw))),
        Mode::Thai => match s.cands.get(s.sel) {
            Some(c) => out.push_str(c),
            None => out.push_str(&s.raw),
        },
        Mode::Japanese => out.push_str(&ja_text(s)),
        Mode::Chinese => {
            // the best candidate when it takes everything typed, else the letters
            match (s.cands.first(), s.cand_syls.first()) {
                (Some(c), Some(&k)) if k == zh_syl_count(s) => out.push_str(c),
                _ => out.push_str(&s.raw),
            }
        },
        Mode::Hex => {
            if let Some(c) = hex_char(&s.raw) {
                out.push(c);
            } else if !s.raw.is_empty() {
                out.push_str("U+");
                out.push_str(&s.raw);
            }
        },
        _ => out.push_str(&s.raw),
    }
    s.raw.clear();
    s.l = None;
    s.v = None;
    s.t = None;
    clear_conv(s);
}

/// Backspace: true when it only took a step back in the composition (the program must not see it).
pub fn backspace() -> bool {
    let mut out = String::new();
    key(Key::Backspace, &mut out) == Took::Composed
}

/// One printable ASCII character typed; whatever is finished is appended to `out`.
pub fn feed(c: char, out: &mut String) {
    key(Key::Char(c), out);
}

/// The text `bytes` (printable ASCII as the keyboard sends it) through the current method.
pub fn feed_str(bytes: &[u8], out: &mut String) {
    for &b in bytes {
        feed(b as char, out);
    }
}

/// The length of `buf[..len]` without its last UTF-8 character (Backspace removes a whole one).
pub fn utf8_pop(buf: &[u8], len: usize) -> usize {
    let mut n = len.min(buf.len());
    while n > 0 && buf[n - 1] & 0xC0 == 0x80 {
        n -= 1;
    }
    n.saturating_sub(1)
}

/// One key through the current input method; finished text is appended to `out`.
pub fn key(k: Key, out: &mut String) -> Took {
    if bypassed() {
        return match k {
            Key::Char(c) => {
                out.push(c);
                Took::Text
            },
            _ => Took::Pass,
        };
    }
    let s = st();
    match s.mode {
        Mode::Latin => match k {
            Key::Char(c) => {
                out.push(c);
                Took::Text
            },
            _ => Took::Pass,
        },
        Mode::Korean if cfg().ko_2set => key_2set(s, k, out),
        Mode::Korean | Mode::Daemon => key_romaja(s, k, out),
        Mode::Thai if cfg().th_kedmanee => match k {
            Key::Char(c) => {
                out.push(th::kedmanee(c));
                Took::Text
            },
            _ => Took::Pass,
        },
        Mode::Thai => key_thai(s, k, out),
        Mode::Japanese => key_ja(s, k, out),
        Mode::Chinese => key_zh(s, k, out),
        Mode::Accents | Mode::Greek | Mode::Russian | Mode::Hex => key_simple(s, k, out),
    }
}

/// The common keys while composing: Enter commits, Esc drops, Tab and the arrows commit and
/// pass on. None when the key is the method's own business.
fn common(s: &mut State, k: Key, out: &mut String) -> Option<Took> {
    if !composing() {
        return match k {
            Key::Char(_) => None,
            _ => Some(Took::Pass),
        };
    }
    match k {
        Key::Enter => {
            flush(out);
            Some(Took::Composed)
        },
        Key::Escape => {
            reset_keep(s);
            Some(Took::Composed)
        },
        Key::Tab | Key::Left | Key::Right | Key::Up | Key::Down | Key::PageUp | Key::PageDown | Key::F(_) => {
            flush(out);
            Some(Took::Pass)
        },
        _ => None,
    }
}

fn reset_keep(s: &mut State) {
    s.raw.clear();
    s.l = None;
    s.v = None;
    s.t = None;
    clear_conv(s);
}

/// A key the method does not compose with: commit, then the key as typed.
fn commit_then(c: char, out: &mut String) -> Took {
    flush(out);
    out.push(c);
    Took::Text
}

fn key_romaja(s: &mut State, k: Key, out: &mut String) -> Took {
    if let Some(t) = common(s, k, out) {
        return t;
    }
    match k {
        Key::Backspace => {
            s.raw.pop();
            Took::Composed
        },
        Key::Char(c) if c.is_ascii_alphabetic() || ((c == '\'' || c == '-') && !s.raw.is_empty()) => {
            s.raw.push(c);
            Took::Composed
        },
        Key::Char(c) => commit_then(c, out),
        _ => Took::Pass,
    }
}

fn thai_refresh(s: &mut State) {
    s.cands = th::candidates(&s.raw, PAGE);
    s.sel = 0;
}

fn key_thai(s: &mut State, k: Key, out: &mut String) -> Took {
    // Up / Down pick among the spellings
    if composing() && s.cands.len() > 1 {
        match k {
            Key::Up => {
                s.sel = (s.sel + s.cands.len() - 1) % s.cands.len();
                return Took::Composed;
            },
            Key::Down => {
                s.sel = (s.sel + 1) % s.cands.len();
                return Took::Composed;
            },
            _ => {},
        }
    }
    if let Some(t) = common(s, k, out) {
        return t;
    }
    match k {
        Key::Backspace => {
            s.raw.pop();
            thai_refresh(s);
            Took::Composed
        },
        // Space commits the syllable (Thai writes no spaces between words); on nothing, a space
        Key::Char(' ') if composing() => {
            flush(out);
            Took::Composed
        },
        Key::Char(c) if th::is_input(c) || (('1'..='4').contains(&c) && !s.raw.is_empty()) => {
            s.raw.push(c);
            thai_refresh(s);
            Took::Composed
        },
        Key::Char(c) => commit_then(c, out),
        _ => Took::Pass,
    }
}

// Japanese

fn ja_kana_now(s: &State, finish: bool) -> String {
    let (k, rest) = kana::convert(&s.raw, cfg().ja_kata, finish);
    let mut t = match s.kana_form {
        1 => kana::to_hiragana(&k),
        2 => kana::to_katakana(&k),
        _ => k,
    };
    t.push_str(&rest);
    t
}

fn ja_text(s: &State) -> String {
    if s.segs.is_empty() {
        return ja_kana_now(s, true);
    }
    let mut t = String::new();
    for (k, seg) in s.segs.iter().enumerate() {
        t.push_str(&seg.cands[s.pick[k].min(seg.cands.len() - 1)]);
    }
    t
}

fn ja_push_run(run: &mut String, hira: bool, segs: &mut Vec<dict::Seg>) {
    if run.is_empty() {
        return;
    }
    let d = if hira { ja_dict() } else { None };
    match d {
        Some(d) => segs.extend(dict::ja_convert(d, run)),
        None => segs.push(dict::Seg { reading: run.clone(), cands: alloc::vec![run.clone()] }),
    }
    run.clear();
}

/// Space on kana: split it into words and pick the likeliest spelling of each.
fn ja_convert(s: &mut State) {
    let kana = ja_kana_now(s, true);
    let mut segs: Vec<dict::Seg> = Vec::new();
    if cfg().ja_kanji {
        // runs of hiragana go to the dictionary; katakana and the rest stay as typed
        let mut run = String::new();
        let mut hira = true;
        for c in kana.chars() {
            let h = ('ぁ'..='ゖ').contains(&c) || (c == 'ー' && hira && !run.is_empty());
            if h != hira {
                ja_push_run(&mut run, hira, &mut segs);
                hira = h;
            }
            run.push(c);
        }
        ja_push_run(&mut run, hira, &mut segs);
    } else {
        segs.push(dict::Seg { reading: kana.clone(), cands: alloc::vec![kana.clone(), kana::to_katakana(&kana), kana::to_hiragana(&kana)] });
    }
    if segs.is_empty() {
        return;
    }
    s.pick = alloc::vec![0; segs.len()];
    s.segs = segs;
    s.focus = 0;
}

fn key_ja(s: &mut State, k: Key, out: &mut String) -> Took {
    if !s.segs.is_empty() {
        let n = s.segs.len();
        let f = s.focus.min(n - 1);
        let nc = s.segs[f].cands.len();
        match k {
            Key::Char(' ') | Key::Down => {
                s.pick[f] = (s.pick[f] + 1) % nc;
                return Took::Composed;
            },
            Key::Up => {
                s.pick[f] = (s.pick[f] + nc - 1) % nc;
                return Took::Composed;
            },
            Key::PageDown => {
                s.pick[f] = ((s.pick[f] / PAGE + 1) * PAGE).min(nc - 1);
                return Took::Composed;
            },
            Key::PageUp => {
                s.pick[f] = (s.pick[f] / PAGE).saturating_sub(1) * PAGE;
                return Took::Composed;
            },
            Key::Left => {
                s.focus = (f + n - 1) % n;
                return Took::Composed;
            },
            Key::Right => {
                s.focus = (f + 1) % n;
                return Took::Composed;
            },
            Key::Char(c @ '1'..='9') => {
                let i = (s.pick[f] / PAGE) * PAGE + (c as usize - '1' as usize);
                if i < nc {
                    s.pick[f] = i;
                    if f + 1 < n {
                        s.focus = f + 1;
                    } else {
                        flush(out);
                    }
                }
                return Took::Composed;
            },
            Key::Backspace | Key::Escape => {
                // back to the kana
                s.segs.clear();
                s.pick.clear();
                s.focus = 0;
                return Took::Composed;
            },
            Key::F(6) | Key::F(7) => {
                s.segs.clear();
                s.pick.clear();
            },
            Key::Char(c) if kana::is_romaji(c) => {
                // typing on: the conversion is taken, the new letter starts the next word
                flush(out);
                s.raw.push(c);
                return Took::Composed;
            },
            _ => {},
        }
    }
    if composing() && matches!(k, Key::F(6) | Key::F(7)) {
        s.kana_form = if k == Key::F(7) { 2 } else { 1 };
        return Took::Composed;
    }
    if let Some(t) = common(s, k, out) {
        return t;
    }
    match k {
        Key::Backspace => {
            s.raw.pop();
            if s.raw.is_empty() {
                s.kana_form = 0;
            }
            Took::Composed
        },
        Key::Char(' ') if composing() => {
            if s.kana_form != 0 {
                flush(out);
            } else {
                ja_convert(s);
            }
            Took::Composed
        },
        Key::Char(c) if kana::is_romaji(c) && !(c == '\'' && s.raw.is_empty()) => {
            s.raw.push(c);
            Took::Composed
        },
        Key::Char(c) => commit_then(c, out),
        _ => Took::Pass,
    }
}

// Chinese

fn zh_syl_count(s: &State) -> usize {
    match zh_dict() {
        Some(d) => dict::zh_syllables(d, &s.raw, cfg().zh_tones).len(),
        None => 0,
    }
}

fn zh_refresh(s: &mut State) {
    s.cands.clear();
    s.cand_syls.clear();
    s.sel = 0;
    if s.raw.is_empty() {
        return;
    }
    let Some(d) = zh_dict() else { return };
    let syls = dict::zh_syllables(d, &s.raw, cfg().zh_tones);
    for c in dict::zh_candidates(d, &syls, cfg().zh_trad, 60) {
        s.cands.push(c.text);
        s.cand_syls.push(c.syls);
    }
}

/// Take candidate `i`: its text goes out and the syllables it covered leave the input.
fn zh_take(s: &mut State, i: usize, out: &mut String) {
    let (Some(text), Some(&n)) = (s.cands.get(i).cloned(), s.cand_syls.get(i)) else { return };
    let Some(d) = zh_dict() else { return };
    let syls = dict::zh_syllables(d, &s.raw, cfg().zh_tones);
    out.push_str(&text);
    let cut = if n >= syls.len() { s.raw.len() } else { syls[n - 1].end };
    let rest: String = s.raw[cut..].trim_start_matches('\'').into();
    s.raw = rest;
    zh_refresh(s);
}

fn zh_punct(c: char) -> char {
    match c {
        ',' => '，',
        '.' => '。',
        '?' => '？',
        '!' => '！',
        ':' => '：',
        ';' => '；',
        '\\' => '、',
        _ => c,
    }
}

fn key_zh(s: &mut State, k: Key, out: &mut String) -> Took {
    let tones = cfg().zh_tones;
    let after_letter = s.raw.ends_with(|x: char| x.is_ascii_alphabetic());
    if composing() && !s.cands.is_empty() {
        let n = s.cands.len();
        match k {
            Key::Char(' ') => {
                zh_take(s, s.sel, out);
                return Took::Composed;
            },
            Key::Down | Key::Right => {
                s.sel = (s.sel + 1) % n;
                return Took::Composed;
            },
            Key::Up | Key::Left => {
                s.sel = (s.sel + n - 1) % n;
                return Took::Composed;
            },
            Key::PageDown | Key::Char('=') => {
                s.sel = ((s.sel / PAGE + 1) * PAGE).min(n - 1);
                return Took::Composed;
            },
            Key::PageUp | Key::Char('-') => {
                s.sel = (s.sel / PAGE).saturating_sub(1) * PAGE;
                return Took::Composed;
            },
            Key::Char(c @ '1'..='9') if !(tones && after_letter && c <= '5') => {
                let i = (s.sel / PAGE) * PAGE + (c as usize - '1' as usize);
                if i < n {
                    zh_take(s, i, out);
                }
                return Took::Composed;
            },
            _ => {},
        }
    }
    if composing() && k == Key::Enter {
        // Enter: the letters as typed
        out.push_str(&s.raw);
        reset_keep(s);
        return Took::Composed;
    }
    if let Some(t) = common(s, k, out) {
        return t;
    }
    match k {
        Key::Backspace => {
            s.raw.pop();
            zh_refresh(s);
            Took::Composed
        },
        Key::Char(c) if c.is_ascii_alphabetic() || (c == '\'' && !s.raw.is_empty()) => {
            s.raw.push(c.to_ascii_lowercase());
            zh_refresh(s);
            Took::Composed
        },
        Key::Char(c @ '1'..='5') if tones && after_letter => {
            s.raw.push(c);
            zh_refresh(s);
            Took::Composed
        },
        Key::Char(c) => {
            flush(out);
            out.push(zh_punct(c));
            Took::Text
        },
        _ => Took::Pass,
    }
}

// Accents, Greek, Russian, hex

fn key_simple(s: &mut State, k: Key, out: &mut String) -> Took {
    if let Some(t) = common(s, k, out) {
        return t;
    }
    match k {
        Key::Backspace => {
            s.raw.pop();
            Took::Composed
        },
        Key::Char(c) => {
            match s.mode {
                Mode::Accents => feed_accents(s, c, out),
                Mode::Russian => out.push(russian(c)),
                Mode::Greek => feed_greek(s, c, out),
                _ => feed_hex(s, c, out),
            }
            if composing() {
                Took::Composed
            } else {
                Took::Text
            }
        },
        _ => Took::Pass,
    }
}

// Hangul 2-set

fn key_2set(s: &mut State, k: Key, out: &mut String) -> Took {
    if let Some(t) = common(s, k, out) {
        return t;
    }
    match k {
        Key::Backspace => {
            if let Some(t) = s.t {
                s.t = split_final(t).map(|(a, _)| a);
            } else if let Some(v) = s.v {
                s.v = split_vowel(v);
            } else {
                s.l = None;
            }
            Took::Composed
        },
        Key::Char(c) => {
            feed_hangul(s, c, out);
            if composing() {
                Took::Composed
            } else {
                Took::Text
            }
        },
        _ => Took::Pass,
    }
}

// ── settings (Settings > Input) ──

/// One line per method in the user's order: id, badge, name, enabled, option, default.
pub fn info() -> String {
    let c = cfg();
    let mut t = String::new();
    for &m in c.order.iter() {
        t.push_str(mode_id(m));
        t.push('\t');
        t.push_str(&badge_of(m));
        t.push('\t');
        t.push_str(name_of(m));
        t.push('\t');
        t.push(if enabled(m) { '1' } else { '0' });
        t.push('\t');
        t.push_str(option_of(m));
        t.push('\t');
        t.push(if c.default == m { '1' } else { '0' });
        t.push('\n');
    }
    t
}

pub fn current_id() -> &'static str {
    mode_id(st().mode)
}

pub fn toggle(id: &str) {
    if let Some(m) = mode_of(id) {
        if m == Mode::Latin {
            return; // English is always there
        }
        cfg().enabled ^= bit(m);
        if !enabled(m) && st().mode == m {
            let mut o = String::new();
            set_mode(Mode::Latin, &mut o);
        }
        if !enabled(cfg().default) {
            cfg().default = Mode::Latin;
        }
        save();
    }
}

pub fn move_by(id: &str, d: i32) {
    let Some(m) = mode_of(id) else { return };
    let o = &mut cfg().order;
    let Some(i) = o.iter().position(|&x| x == m) else { return };
    let j = i as i32 + d;
    if j >= 0 && (j as usize) < o.len() {
        o.swap(i, j as usize);
        save();
    }
}

pub fn set_default(id: &str) {
    if let Some(m) = mode_of(id) {
        cfg().enabled |= bit(m);
        cfg().default = m;
        save();
    }
}

/// Step the method's option (its typing scheme / script choice).
pub fn next_option(id: &str) {
    let c = cfg();
    match mode_of(id) {
        Some(Mode::Thai) => c.th_kedmanee = !c.th_kedmanee,
        Some(Mode::Korean) => c.ko_2set = !c.ko_2set,
        Some(Mode::Japanese) => {
            // hira+kanji -> kata+kanji -> hira only -> kata only
            let i = (c.ja_kata as u8 + 2 * (!c.ja_kanji) as u8 + 1) % 4;
            c.ja_kata = i & 1 != 0;
            c.ja_kanji = i < 2;
        },
        Some(Mode::Chinese) => {
            let i = (c.zh_trad as u8 + 2 * c.zh_tones as u8 + 1) % 4;
            c.zh_trad = i & 1 != 0;
            c.zh_tones = i >= 2;
        },
        _ => return,
    }
    reset();
    save();
}

pub fn keys() -> u8 {
    cfg().keys
}

pub fn set_keys(k: u8) {
    cfg().keys = k & 7;
    save();
}

pub fn fkey() -> u8 {
    cfg().fkey
}

pub fn set_fkey(n: u8) {
    cfg().fkey = if matches!(n, 5 | 8 | 9 | 10 | 12) { n } else { 0 };
    save();
}

/// Pick a method (the indicator's menu). Unknown or disabled ids do nothing.
pub fn select(id: &str) {
    if let Some(m) = mode_of(id) {
        if enabled(m) {
            let mut o = String::new();
            set_mode(m, &mut o);
        }
    }
}

/// How the method is typed (Settings > Input help).
pub fn help(id: &str) -> &'static str {
    match mode_of(id) {
        Some(Mode::Latin) => "Keys type what is printed on them (US layout). Always available.",
        Some(Mode::Thai) => {
            if cfg().th_kedmanee {
                "Kedmanee: the standard Thai keyboard on the US keys (d = ก, f = ด, k = า, h = ้, j = ่ ...)."
            } else {
                "Type by sound, syllable by syllable: kin กิน, maak มาก, rak รัก, pen เป็น, rian เรียน, mueang เมือง. Initials k kh g ng j ch s y d t th n b p ph f m r l w h, x = อ; h before n m l w y r = leading ห. Vowels a aa i ii ue u uu e ee ae o oo aw/or oe ia uea ua ai ao am aai ui oi iu eo aeo iao. Tone: digit after the syllable, 1 ่ 2 ้ 3 ๊ 4 ๋; ! = ์. Up/Down: other letters for the sound (kh: ค ข ฆ). Space commits; Space again types a space."
            }
        },
        Some(Mode::Korean) => {
            if cfg().ko_2set {
                "Dubeolsik: the standard 2-set Hangul keyboard; jamo join into syllables as you type."
            } else {
                "Revised Romanization, composed live: hangug 한국, annyeong 안녕, seoul 서울, saranghae 사랑해. Onsets g kk n d tt r m b pp s ss j jj ch k t p h; vowels a ae ya yae eo e yeo ye o wa wae oe yo u wo we wi yu eu ui i. A consonant between vowels starts the next syllable; ' forces a break (iss'eoyo 있어요). Space or Enter commits."
            }
        },
        Some(Mode::Japanese) => "Romaji to kana (Hepburn or Kunrei): kka っか, n' or nn ん, - ー; Shift-typed letters give katakana. Space converts to kanji (again: next candidate), Left/Right move between words, 1-9 pick, Enter commits, Esc or Backspace go back to kana. F6 hiragana, F7 katakana. Dictionary: mozc OSS (IPAdic, NAIST license).",
        Some(Mode::Chinese) => "Pinyin to hanzi: zhongguo 中国, nihao 你好; whole phrases convert at once, ' splits syllables (xi'an). Space takes the highlighted candidate, 1-9 pick, -/= or PgUp/PgDn page, Enter types the letters. With tone digits on, 1-5 after a syllable filter by tone. Dictionary: CC-CEDICT (CC BY-SA 4.0) + jieba frequencies (MIT).",
        Some(Mode::Daemon) => "The stacked Daemon script of the boot screen ring: each block is onset + vowel + coda, typed with the Korean romanization (ling, de'mon). Drawn with the Daemon Chesilin font; stored as private-use code points from U+F0000.",
        Some(Mode::Accents) => "Dead keys: ' ` ^ ~ \" then a letter (' e = é, ~ n = ñ, \" u = ü); the mark then Space types it alone.",
        Some(Mode::Russian) => "The ЙЦУКЕН layout on the US keys.",
        Some(Mode::Greek) => "The Greek layout; ; then a vowel adds the tonos (; a = ά).",
        Some(Mode::Hex) => "Hex digits of a code point, then Space (4E2D = 中).",
        None => "",
    }
}

// ── saved per user ──

fn conf_path() -> String {
    let mut p = crate::vfs::home();
    p.push_str("/Programs/LingOS/input.conf");
    p
}

pub fn save() {
    let c = cfg();
    let ids = |f: &dyn Fn(Mode) -> bool| -> String {
        let v: Vec<&str> = c.order.iter().copied().filter(|&m| f(m)).map(mode_id).collect();
        v.join(",")
    };
    let t = alloc::format!(
        "order={}\nenabled={}\ndefault={}\nkeys={}\nfkey={}\nko={}\nja_kana={}\nja_kanji={}\nzh={}\nzh_tones={}\nth={}\nbox={},{}\n",
        ids(&|_| true),
        ids(&|m| enabled(m)),
        mode_id(c.default),
        c.keys,
        c.fkey,
        if c.ko_2set { "2set" } else { "rr" },
        if c.ja_kata { "kata" } else { "hira" },
        c.ja_kanji as u8,
        if c.zh_trad { "trad" } else { "simp" },
        c.zh_tones as u8,
        if c.th_kedmanee { "kedmanee" } else { "phonetic" },
        c.box_dx,
        c.box_dy,
    );
    let home = crate::vfs::home();
    crate::vfs::mkdir(&alloc::format!("{home}/Programs"));
    crate::vfs::mkdir(&alloc::format!("{home}/Programs/LingOS"));
    if !crate::vfs::write(&conf_path(), t.as_bytes()) {
        crate::console_write(b"ime: could not save input.conf\n");
    }
}

/// The signed-in user's settings (after sign-in): the default method becomes the current one.
pub fn load() {
    unsafe { SUSPENDED = false };
    let c = cfg();
    // (another user's moved box does not carry over)
    c.box_dx = 0;
    c.box_dy = 0;
    if let Some(bytes) = crate::vfs::read(&conf_path()) {
        let text = core::str::from_utf8(&bytes).unwrap_or("");
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k {
                "order" => {
                    let mut o: Vec<Mode> = Vec::new();
                    for m in v.split(',').filter_map(mode_of).chain(ALL.iter().copied()) {
                        if !o.contains(&m) {
                            o.push(m);
                        }
                    }
                    if o.len() == 10 {
                        c.order.copy_from_slice(&o);
                    }
                },
                "enabled" => c.enabled = v.split(',').filter_map(mode_of).fold(bit(Mode::Latin), |a, m| a | bit(m)),
                "default" => c.default = mode_of(v).unwrap_or(Mode::Latin),
                "keys" => c.keys = v.parse::<u8>().unwrap_or(7) & 7,
                "fkey" => c.fkey = v.parse::<u8>().unwrap_or(0),
                "ko" => c.ko_2set = v == "2set",
                "ja_kana" => c.ja_kata = v == "kata",
                "ja_kanji" => c.ja_kanji = v != "0",
                "zh" => c.zh_trad = v == "trad",
                "zh_tones" => c.zh_tones = v == "1",
                "th" => c.th_kedmanee = v == "kedmanee",
                "box" => {
                    if let Some((dx, dy)) = v.split_once(',') {
                        c.box_dx = dx.trim().parse::<i32>().unwrap_or(0).clamp(-8000, 8000);
                        c.box_dy = dy.trim().parse::<i32>().unwrap_or(0).clamp(-8000, 8000);
                    }
                },
                _ => {},
            }
        }
    }
    reset();
    st().mode = if enabled(c.default) { c.default } else { Mode::Latin };
}

// ── dead keys ──

fn dead_accent(dead: char, c: char) -> Option<char> {
    let t: &[(char, char)] = match dead {
        '\'' => &[('a', 'á'), ('e', 'é'), ('i', 'í'), ('o', 'ó'), ('u', 'ú'), ('y', 'ý'), ('c', 'ç'), ('A', 'Á'), ('E', 'É'), ('I', 'Í'), ('O', 'Ó'), ('U', 'Ú'), ('Y', 'Ý'), ('C', 'Ç')],
        '`' => &[('a', 'à'), ('e', 'è'), ('i', 'ì'), ('o', 'ò'), ('u', 'ù'), ('A', 'À'), ('E', 'È'), ('I', 'Ì'), ('O', 'Ò'), ('U', 'Ù')],
        '^' => &[('a', 'â'), ('e', 'ê'), ('i', 'î'), ('o', 'ô'), ('u', 'û'), ('A', 'Â'), ('E', 'Ê'), ('I', 'Î'), ('O', 'Ô'), ('U', 'Û')],
        '~' => &[('a', 'ã'), ('o', 'õ'), ('n', 'ñ'), ('A', 'Ã'), ('O', 'Õ'), ('N', 'Ñ')],
        '"' => &[('a', 'ä'), ('e', 'ë'), ('i', 'ï'), ('o', 'ö'), ('u', 'ü'), ('y', 'ÿ'), ('s', 'ß'), ('A', 'Ä'), ('E', 'Ë'), ('I', 'Ï'), ('O', 'Ö'), ('U', 'Ü')],
        _ => &[],
    };
    t.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)
}

fn feed_accents(s: &mut State, c: char, out: &mut String) {
    if let Some(dead) = s.raw.chars().next() {
        s.raw.clear();
        if let Some(a) = dead_accent(dead, c) {
            out.push(a);
        } else if c == ' ' || c == dead {
            out.push(dead);
        } else {
            out.push(dead);
            feed_accents(s, c, out);
        }
        return;
    }
    if "'`^~\"".contains(c) {
        s.raw.push(c);
    } else {
        out.push(c);
    }
}

fn russian(c: char) -> char {
    const LOW: &str = "qwertyuiop[]asdfghjkl;'zxcvbnm,./`";
    const RU: &str = "йцукенгшщзхъфывапролджэячсмитьбю.ё";
    const UP: &str = "QWERTYUIOP{}ASDFGHJKL:\"ZXCVBNM<>?~";
    const RU_UP: &str = "ЙЦУКЕНГШЩЗХЪФЫВАПРОЛДЖЭЯЧСМИТЬБЮ,Ё";
    if let Some(i) = LOW.chars().position(|x| x == c) {
        return RU.chars().nth(i).unwrap_or(c);
    }
    if let Some(i) = UP.chars().position(|x| x == c) {
        return RU_UP.chars().nth(i).unwrap_or(c);
    }
    c
}

fn greek(c: char) -> char {
    const LAT: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const GR: &str = "αβψδεφγηιξκλμνοπ;ρστθωςχυζΑΒΨΔΕΦΓΗΙΞΚΛΜΝΟΠ:ΡΣΤΘΩΣΧΥΖ";
    match LAT.chars().position(|x| x == c) {
        Some(i) => GR.chars().nth(i).unwrap_or(c),
        None => c,
    }
}

fn greek_tonos(c: char) -> Option<char> {
    let t = [('α', 'ά'), ('ε', 'έ'), ('η', 'ή'), ('ι', 'ί'), ('ο', 'ό'), ('υ', 'ύ'), ('ω', 'ώ'), ('Α', 'Ά'), ('Ε', 'Έ'), ('Η', 'Ή'), ('Ι', 'Ί'), ('Ο', 'Ό'), ('Υ', 'Ύ'), ('Ω', 'Ώ')];
    t.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)
}

fn greek_dialytika(c: char) -> Option<char> {
    let t = [('ι', 'ϊ'), ('υ', 'ϋ'), ('Ι', 'Ϊ'), ('Υ', 'Ϋ')];
    t.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)
}

fn feed_greek(s: &mut State, c: char, out: &mut String) {
    if let Some(dead) = s.raw.chars().next() {
        s.raw.clear();
        let g = greek(c);
        let acc = if dead == ';' { greek_tonos(g) } else { greek_dialytika(g) };
        match acc {
            Some(a) => out.push(a),
            None => {
                out.push(if dead == ';' { '΄' } else { '¨' });
                if c != ' ' {
                    feed_greek(s, c, out);
                }
            },
        }
        return;
    }
    if c == ';' || c == ':' {
        s.raw.push(c);
    } else {
        out.push(greek(c));
    }
}

fn hex_char(p: &str) -> Option<char> {
    if p.is_empty() {
        return None;
    }
    u32::from_str_radix(p, 16).ok().and_then(char::from_u32)
}

fn feed_hex(s: &mut State, c: char, out: &mut String) {
    if c.is_ascii_hexdigit() && s.raw.len() < 6 {
        s.raw.push(c.to_ascii_uppercase());
        return;
    }
    if c == ' ' && !s.raw.is_empty() {
        let p = core::mem::take(&mut s.raw);
        match hex_char(&p) {
            Some(ch) => out.push(ch),
            None => {
                out.push_str("U+");
                out.push_str(&p);
            },
        }
        return;
    }
    flush(out);
    out.push(c);
}

// ── Hangul (2-set) ──

const CHO: &str = "ㄱㄲㄴㄷㄸㄹㅁㅂㅃㅅㅆㅇㅈㅉㅊㅋㅌㅍㅎ";
const JUNG: &str = "ㅏㅐㅑㅒㅓㅔㅕㅖㅗㅘㅙㅚㅛㅜㅝㅞㅟㅠㅡㅢㅣ";
const JONG: &str = "ㄱㄲㄳㄴㄵㄶㄷㄹㄺㄻㄼㄽㄾㄿㅀㅁㅂㅄㅅㅆㅇㅈㅊㅋㅌㅍㅎ";

fn idx(set: &str, c: char) -> Option<u32> {
    set.chars().position(|x| x == c).map(|i| i as u32)
}

fn jamo_of(c: char) -> Option<char> {
    const KEYS: &str = "qwertyuiopasdfghjklzxcvbnmQWERTOP";
    const JAMO: &str = "ㅂㅈㄷㄱㅅㅛㅕㅑㅐㅔㅁㄴㅇㄹㅎㅗㅓㅏㅣㅋㅌㅊㅍㅠㅜㅡㅃㅉㄸㄲㅆㅒㅖ";
    if let Some(i) = KEYS.chars().position(|x| x == c) {
        return JAMO.chars().nth(i);
    }
    if c.is_ascii_uppercase() {
        return jamo_of(c.to_ascii_lowercase());
    }
    None
}

fn is_vowel(j: char) -> bool {
    idx(JUNG, j).is_some()
}

fn combine_vowel(a: char, b: char) -> Option<char> {
    match (a, b) {
        ('ㅗ', 'ㅏ') => Some('ㅘ'),
        ('ㅗ', 'ㅐ') => Some('ㅙ'),
        ('ㅗ', 'ㅣ') => Some('ㅚ'),
        ('ㅜ', 'ㅓ') => Some('ㅝ'),
        ('ㅜ', 'ㅔ') => Some('ㅞ'),
        ('ㅜ', 'ㅣ') => Some('ㅟ'),
        ('ㅡ', 'ㅣ') => Some('ㅢ'),
        _ => None,
    }
}

fn split_vowel(v: char) -> Option<char> {
    match v {
        'ㅘ' | 'ㅙ' | 'ㅚ' => Some('ㅗ'),
        'ㅝ' | 'ㅞ' | 'ㅟ' => Some('ㅜ'),
        'ㅢ' => Some('ㅡ'),
        _ => None,
    }
}

fn combine_final(a: char, b: char) -> Option<char> {
    match (a, b) {
        ('ㄱ', 'ㅅ') => Some('ㄳ'),
        ('ㄴ', 'ㅈ') => Some('ㄵ'),
        ('ㄴ', 'ㅎ') => Some('ㄶ'),
        ('ㄹ', 'ㄱ') => Some('ㄺ'),
        ('ㄹ', 'ㅁ') => Some('ㄻ'),
        ('ㄹ', 'ㅂ') => Some('ㄼ'),
        ('ㄹ', 'ㅅ') => Some('ㄽ'),
        ('ㄹ', 'ㅌ') => Some('ㄾ'),
        ('ㄹ', 'ㅍ') => Some('ㄿ'),
        ('ㄹ', 'ㅎ') => Some('ㅀ'),
        ('ㅂ', 'ㅅ') => Some('ㅄ'),
        _ => None,
    }
}

fn split_final(t: char) -> Option<(char, char)> {
    match t {
        'ㄳ' => Some(('ㄱ', 'ㅅ')),
        'ㄵ' => Some(('ㄴ', 'ㅈ')),
        'ㄶ' => Some(('ㄴ', 'ㅎ')),
        'ㄺ' => Some(('ㄹ', 'ㄱ')),
        'ㄻ' => Some(('ㄹ', 'ㅁ')),
        'ㄼ' => Some(('ㄹ', 'ㅂ')),
        'ㄽ' => Some(('ㄹ', 'ㅅ')),
        'ㄾ' => Some(('ㄹ', 'ㅌ')),
        'ㄿ' => Some(('ㄹ', 'ㅍ')),
        'ㅀ' => Some(('ㄹ', 'ㅎ')),
        'ㅄ' => Some(('ㅂ', 'ㅅ')),
        _ => None,
    }
}

fn syllable(l: char, v: char, t: Option<char>) -> Option<char> {
    let li = idx(CHO, l)?;
    let vi = idx(JUNG, v)?;
    let ti = match t {
        Some(t) => idx(JONG, t)? + 1,
        None => 0,
    };
    char::from_u32(0xAC00 + (li * 21 + vi) * 28 + ti)
}

fn hangul_current(s: &State) -> Option<char> {
    match (s.l, s.v) {
        (Some(l), Some(v)) => syllable(l, v, s.t),
        (Some(l), None) => Some(l),
        (None, Some(v)) => Some(v),
        _ => None,
    }
}

fn hangul_commit(s: &mut State, out: &mut String) {
    if let Some(c) = hangul_current(s) {
        out.push(c);
    }
    s.l = None;
    s.v = None;
    s.t = None;
}

fn feed_hangul(s: &mut State, c: char, out: &mut String) {
    let Some(j) = jamo_of(c) else {
        hangul_commit(s, out);
        out.push(c);
        return;
    };
    if is_vowel(j) {
        match (s.l, s.v, s.t) {
            (_, None, _) if s.l.is_some() => s.v = Some(j),
            (Some(_), Some(v), None) => match combine_vowel(v, j) {
                Some(cv) => s.v = Some(cv),
                None => {
                    hangul_commit(s, out);
                    s.v = Some(j);
                },
            },
            (Some(_), Some(_), Some(t)) => {
                let (keep, moved) = match split_final(t) {
                    Some((a, b)) => (Some(a), b),
                    None => (None, t),
                };
                s.t = keep;
                hangul_commit(s, out);
                s.l = Some(moved);
                s.v = Some(j);
            },
            (None, Some(v), _) => match combine_vowel(v, j) {
                Some(cv) => s.v = Some(cv),
                None => {
                    hangul_commit(s, out);
                    s.v = Some(j);
                },
            },
            _ => s.v = Some(j),
        }
        return;
    }
    match (s.l, s.v, s.t) {
        (Some(_), Some(_), None) if idx(JONG, j).is_some() => s.t = Some(j),
        (Some(_), Some(_), Some(t)) if combine_final(t, j).is_some() => s.t = combine_final(t, j),
        (None, None, None) => s.l = Some(j),
        _ => {
            hangul_commit(s, out);
            s.l = Some(j);
        },
    }
}
