//! One text field for the whole system: the kernel's file dialogs (path bar, Name box) and,
//! through the `text_field` builtin, the desktop's text boxes. It behaves like a programmer's
//! editor line, the same everywhere:
//!
//!  - caret anywhere: Left / Right, Home / End, Ctrl+Left / Ctrl+Right by word;
//!  - selection: Shift + any of those, Ctrl+A, mouse drag, Shift+click, double-click a word;
//!    the user's case: caret after "this.txt", Shift+Left six times selects "is.txt";
//!  - clipboard: Ctrl+C / Ctrl+X / Ctrl+V (and Ctrl+Insert / Shift+Insert / Shift+Delete), a
//!    right-click menu with Cut / Copy / Paste / Select all / Undo;
//!  - Ctrl+Z / Ctrl+Y (Ctrl+Shift+Z) undo and redo, typing over a selection replaces it,
//!    Backspace / Delete remove the selection (Ctrl+Backspace / Ctrl+Delete a word);
//!  - positions are characters (Unicode scalar values), never bytes, so Korean, Chinese, Thai or
//!    emoji move and select as whole characters; on screen text is laid out one 8-px cell per
//!    UTF-8 byte (`px_text8`), so the highlight and caret are measured in those cells, the width
//!    a wide character really takes;
//!  - the input method composes at the caret (the field reports its caret to drivers/ime.rs
//!    every frame it is drawn focused) and its committed text replaces the selection;
//!  - nothing is "smart": no capitalisation, no substitutions; a paste keeps the text exactly,
//!    only a single-line field turns line breaks into spaces;
//!  - a long value scrolls sideways, keeping the caret in view.

use super::{key_down, key_pressed, rect, text8, Window};
use alloc::string::String;
use alloc::vec::Vec;

/// What a field's keys asked for, besides editing.
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Event {
    None,
    Enter,
    Tab,
    BackTab,
    Escape,
}

pub struct TextField {
    pub text: String,
    /// caret, in characters
    caret: usize,
    /// the other end of the selection (characters), when there is one
    anchor: Option<usize>,
    undo: Vec<(String, usize)>,
    redo: Vec<(String, usize)>,
    /// first visible cell (byte offset) when the value is wider than the box
    scroll: usize,
    last_click: (usize, u64),
    dragging: bool,
    /// the right-click menu, at (x, y), and the item under the pointer
    menu: Option<(i64, i64)>,
    /// a field where a newline cannot go (the paste turns them into spaces)
    pub single_line: bool,
    /// the mouse buttons this field saw last (field_draw makes press edges per field)
    pub prev_buttons: u8,
}

/// A colour that draws nothing: the program drew the field's box itself.
pub const NO_BOX: u32 = 0xFF00_0000;

const MENU: [&str; 5] = ["Cut", "Copy", "Paste", "Select all", "Undo"];
const MENU_W: i64 = 112;
const MENU_ROW: i64 = 18;

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl TextField {
    pub fn new(text: &str) -> TextField {
        let n = text.chars().count();
        TextField { text: String::from(text), caret: n, anchor: None, undo: Vec::new(), redo: Vec::new(), scroll: 0, last_click: (usize::MAX, 0), dragging: false, menu: None, single_line: true, prev_buttons: 0 }
    }

    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    /// Replace the whole value (the caret goes to the end, no selection, undo kept).
    pub fn set(&mut self, text: &str) {
        if text != self.text {
            self.text = String::from(text);
            self.caret = self.len();
            self.anchor = None;
            self.scroll = 0;
        }
    }

    /// (start, end) in characters, start < end.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        if a == self.caret {
            return None;
        }
        Some((a.min(self.caret), a.max(self.caret)))
    }

    pub fn selected_text(&self) -> String {
        match self.selection() {
            Some((a, b)) => self.text.chars().skip(a).take(b - a).collect(),
            None => String::new(),
        }
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.len();
    }

    /// Byte offset of character `ci`.
    fn byte_at(&self, ci: usize) -> usize {
        self.text.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(self.text.len())
    }

    /// The character index whose cell starts at or before byte cell `cell` (rounding to nearest).
    fn char_at_cell(&self, cell: f64) -> usize {
        let mut ci = 0usize;
        for (k, (b, ch)) in self.text.char_indices().enumerate() {
            let mid = b as f64 + ch.len_utf8() as f64 / 2.0;
            if cell < mid {
                return k;
            }
            ci = k + 1;
        }
        ci
    }

    fn snapshot(&mut self) {
        self.undo.push((self.text.clone(), self.caret));
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn remove_range(&mut self, a: usize, b: usize) {
        let (ba, bb) = (self.byte_at(a), self.byte_at(b));
        self.text.replace_range(ba..bb, "");
        self.caret = a;
        self.anchor = None;
    }

    /// Type or paste `s` at the caret, over the selection.
    pub fn insert(&mut self, s: &str) {
        let s: String = if self.single_line { s.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect() } else { s.replace('\r', "") };
        if s.is_empty() && self.selection().is_none() {
            return;
        }
        self.snapshot();
        if let Some((a, b)) = self.selection() {
            self.remove_range(a, b);
        }
        let at = self.byte_at(self.caret);
        self.text.insert_str(at, &s);
        self.caret += s.chars().count();
        self.anchor = None;
    }

    fn word_left(&self, from: usize) -> usize {
        let cs: Vec<char> = self.text.chars().collect();
        let mut i = from;
        while i > 0 && cs[i - 1].is_whitespace() {
            i -= 1;
        }
        if i > 0 && is_word(cs[i - 1]) {
            while i > 0 && is_word(cs[i - 1]) {
                i -= 1;
            }
        } else {
            while i > 0 && !is_word(cs[i - 1]) && !cs[i - 1].is_whitespace() {
                i -= 1;
            }
        }
        i
    }

    fn word_right(&self, from: usize) -> usize {
        let cs: Vec<char> = self.text.chars().collect();
        let n = cs.len();
        let mut i = from;
        if i < n && is_word(cs[i]) {
            while i < n && is_word(cs[i]) {
                i += 1;
            }
        } else {
            while i < n && !is_word(cs[i]) && !cs[i].is_whitespace() {
                i += 1;
            }
        }
        while i < n && cs[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Move the caret; with `extend` the selection grows from where it was.
    fn move_to(&mut self, to: usize, extend: bool) {
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.caret);
            }
        } else {
            self.anchor = None;
        }
        self.caret = to.min(self.len());
    }

    pub fn copy(&self) -> bool {
        let t = self.selected_text();
        if t.is_empty() {
            return false;
        }
        crate::drivers::clipboard::set_str(&t);
        true
    }

    pub fn cut(&mut self) {
        if self.copy() {
            if let Some((a, b)) = self.selection() {
                self.snapshot();
                self.remove_range(a, b);
            }
        }
    }

    pub fn paste(&mut self) {
        let clip = String::from(core::str::from_utf8(crate::drivers::clipboard::get()).unwrap_or(""));
        if !clip.is_empty() {
            self.insert(&clip);
        }
    }

    pub fn undo(&mut self) {
        if let Some((t, c)) = self.undo.pop() {
            self.redo.push((self.text.clone(), self.caret));
            self.text = t;
            self.caret = c.min(self.len());
            self.anchor = None;
        }
    }

    pub fn redo(&mut self) {
        if let Some((t, c)) = self.redo.pop() {
            self.undo.push((self.text.clone(), self.caret));
            self.text = t;
            self.caret = c.min(self.len());
            self.anchor = None;
        }
    }

    /// This frame's keys and typed text for a focused field. `allow` filters typed characters
    /// (a code box takes digits only, a file name no slashes). `secret`: no copying out.
    pub fn keys(&mut self, typed: &str, allow: &dyn Fn(char) -> bool, secret: bool) -> Event {
        let shift = key_down("shift");
        let ctrl = key_down("ctrl");
        let alt = key_down("alt");
        let mut ev = Event::None;
        // Ctrl+letter shortcuts (Ctrl+Alt is AltGr on many layouts: that one types)
        if ctrl && !alt {
            if key_pressed("a") {
                self.select_all();
            }
            if key_pressed("c") && !secret {
                self.copy();
            }
            if key_pressed("x") {
                if secret {
                    if let Some((a, b)) = self.selection() {
                        self.snapshot();
                        self.remove_range(a, b);
                    }
                } else {
                    self.cut();
                }
            }
            if key_pressed("v") {
                self.paste();
            }
            if key_pressed("z") {
                if shift {
                    self.redo();
                } else {
                    self.undo();
                }
            }
            if key_pressed("y") {
                self.redo();
            }
        }
        if key_pressed("insert") {
            if ctrl && !secret {
                self.copy();
            } else if shift {
                self.paste();
            }
        }
        let n = self.len();
        if key_pressed("left") {
            match self.selection() {
                Some((a, _)) if !shift => self.move_to(a, false),
                _ => {
                    let to = if ctrl { self.word_left(self.caret) } else { self.caret.saturating_sub(1) };
                    self.move_to(to, shift);
                },
            }
        }
        if key_pressed("right") {
            match self.selection() {
                Some((_, b)) if !shift => self.move_to(b, false),
                _ => {
                    let to = if ctrl { self.word_right(self.caret) } else { (self.caret + 1).min(n) };
                    self.move_to(to, shift);
                },
            }
        }
        if key_pressed("home") {
            self.move_to(0, shift);
        }
        if key_pressed("end") {
            self.move_to(n, shift);
        }
        if key_pressed("backspace") {
            if let Some((a, b)) = self.selection() {
                self.snapshot();
                self.remove_range(a, b);
            } else if self.caret > 0 {
                let from = if ctrl { self.word_left(self.caret) } else { self.caret - 1 };
                self.snapshot();
                self.remove_range(from, self.caret);
            }
        }
        if key_pressed("delete") {
            if shift && !ctrl && !secret && self.selection().is_some() {
                self.cut();
            } else if let Some((a, b)) = self.selection() {
                self.snapshot();
                self.remove_range(a, b);
            } else if self.caret < n {
                let to = if ctrl { self.word_right(self.caret) } else { self.caret + 1 };
                let c = self.caret;
                self.snapshot();
                self.remove_range(c, to);
            }
        }
        if key_pressed("escape") {
            if self.menu.is_some() {
                self.menu = None;
            } else if self.selection().is_some() {
                self.anchor = None;
            } else {
                ev = Event::Escape;
            }
        }
        // typed text (and the input method's committed text) goes in at the caret; while Ctrl
        // is held a letter is a shortcut, not text
        let mut run = String::new();
        for ch in typed.chars() {
            match ch {
                '\n' | '\r' => {
                    if !run.is_empty() {
                        self.insert(&core::mem::take(&mut run));
                    }
                    if self.single_line {
                        ev = Event::Enter;
                    } else {
                        run.push('\n');
                    }
                },
                '\t' => ev = if shift { Event::BackTab } else { Event::Tab },
                c if c.is_control() => {},
                c if ctrl && !alt && c.is_ascii_alphabetic() => {},
                c if allow(c) => run.push(c),
                _ => {},
            }
        }
        if !run.is_empty() {
            self.insert(&run);
        }
        if ev == Event::None && key_pressed("tab") && !typed.contains('\t') {
            ev = if shift { Event::BackTab } else { Event::Tab };
        }
        if ev == Event::None && key_pressed("enter") && !typed.contains('\n') && !typed.contains('\r') {
            ev = Event::Enter;
        }
        ev
    }

    /// The pointer over a field drawn at (x, y), `w` wide and `h` tall (text cells of `cw` px):
    /// press to place the caret (Shift extends), drag to select, double-click a word, right-click
    /// for the menu. `pressed` / `rpressed` are this frame's new button presses. True when the
    /// field took the press (it should take the focus).
    #[allow(clippy::too_many_arguments)]
    pub fn mouse(&mut self, mx: i64, my: i64, held: bool, pressed: bool, rpressed: bool, x: i64, y: i64, w: i64, h: i64, cw: i64, secret: bool) -> bool {
        // the open menu takes the click first
        if let Some((mxm, mym)) = self.menu {
            if pressed || rpressed {
                self.menu = None;
                if mx >= mxm && mx < mxm + MENU_W && my >= mym && my < mym + MENU_ROW * MENU.len() as i64 {
                    match ((my - mym) / MENU_ROW) as usize {
                        0 if !secret => self.cut(),
                        1 if !secret => {
                            self.copy();
                        },
                        2 => self.paste(),
                        3 => self.select_all(),
                        4 => self.undo(),
                        _ => {},
                    }
                    return true;
                }
            }
        }
        let inside = mx >= x && mx < x + w && my >= y && my < y + h;
        let cell = (mx - x - 4).max(0) as f64 / cw.max(1) as f64 + self.scroll as f64;
        if pressed && inside {
            let at = self.char_at_cell(cell);
            let now = crate::arch::timer::now_ms();
            if self.last_click.0 == at && now.saturating_sub(self.last_click.1) < crate::drivers::hiddigi::DBL_CLICK_MS {
                // a double-click selects the word under the pointer
                let cs: Vec<char> = self.text.chars().collect();
                let (mut a, mut b) = (at.min(cs.len()), at.min(cs.len()));
                while a > 0 && is_word(cs[a - 1]) {
                    a -= 1;
                }
                while b < cs.len() && is_word(cs[b]) {
                    b += 1;
                }
                self.anchor = Some(a);
                self.caret = b;
                self.dragging = false;
            } else {
                self.move_to(at, key_down("shift"));
                if self.anchor.is_none() {
                    self.anchor = Some(at);
                }
                self.dragging = true;
            }
            self.last_click = (at, now);
            return true;
        }
        if self.dragging {
            if held {
                self.caret = self.char_at_cell(cell);
            } else {
                self.dragging = false;
            }
        }
        if rpressed && inside {
            self.menu = Some((mx, my));
            return true;
        }
        false
    }

    /// Draw the field: its box, the visible part of the value (scrolled so the caret shows), the
    /// selection, the caret and the input method's composition; report the caret to the IME.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, win: &mut Window, x: i64, y: i64, w: i64, h: i64, focused: bool, secret: bool, colors: (u32, u32, u32, u32)) {
        let (bg, fg, sel_bg, border) = colors;
        if border != NO_BOX {
            rect(win, x, y, w, h, border);
        }
        if bg != NO_BOX {
            rect(win, x + 1, y + 1, w - 2, h - 2, bg);
        }
        let shown: String = if secret { self.text.chars().map(|_| '*').collect() } else { self.text.clone() };
        // cells: secret text is one cell per character
        let cell_of = |ci: usize| -> usize {
            if secret {
                ci
            } else {
                shown.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(shown.len())
            }
        };
        let cols = ((w - 10) / 8).max(1) as usize;
        let caret_cell = cell_of(self.caret);
        if caret_cell < self.scroll {
            self.scroll = caret_cell;
        } else if caret_cell > self.scroll + cols - 1 {
            self.scroll = caret_cell + 1 - cols;
        }
        if shown.len() < cols {
            self.scroll = 0;
        }
        // the visible slice, on character boundaries
        let mut start = self.scroll.min(shown.len());
        while start > 0 && !shown.is_char_boundary(start) {
            start -= 1;
        }
        self.scroll = start;
        let mut end = (start + cols).min(shown.len());
        while end > start && !shown.is_char_boundary(end) {
            end -= 1;
        }
        let tx = x + 5;
        let ty = y + (h - 8) / 2;
        if let Some((a, b)) = self.selection() {
            let (ca, cb) = (cell_of(a).max(start), cell_of(b).min(end));
            if cb > ca {
                rect(win, tx + (ca - start) as i64 * 8, ty - 2, (cb - ca) as i64 * 8, 12, sel_bg);
            }
        }
        text8(win, tx, ty, fg, &shown[start..end]);
        if focused {
            let cx = tx + (caret_cell.saturating_sub(start)) as i64 * 8;
            let pre = crate::drivers::ime::preedit();
            if !pre.is_empty() && !secret {
                // the composition, in line at the caret, underlined
                if bg != NO_BOX {
                    rect(win, cx, ty - 2, pre.len() as i64 * 8, 12, bg);
                }
                text8(win, cx, ty, fg, &pre);
                rect(win, cx, ty + 9, pre.len() as i64 * 8, 1, fg);
            } else if (crate::arch::timer::now_ms() / 500) % 2 == 0 || self.selection().is_some() {
                rect(win, cx, ty - 2, 1, 12, fg);
            }
            let (sx, sy, k) = super::app_to_screen(cx as f64, (ty - 2) as f64);
            crate::drivers::ime::report_field(&super::active_app_name(), sx as i32, sy as i32, (12.0 * k) as i32, secret);
        }
        if let Some((mx, my)) = self.menu {
            rect(win, mx - 1, my - 1, MENU_W + 2, MENU_ROW * MENU.len() as i64 + 2, border);
            rect(win, mx, my, MENU_W, MENU_ROW * MENU.len() as i64, 0x1B_1626);
            for (i, item) in MENU.iter().enumerate() {
                let off = secret && i < 2;
                let can = match i {
                    0 | 1 => self.selection().is_some(),
                    4 => !self.undo.is_empty(),
                    _ => true,
                };
                text8(win, mx + 8, my + 5 + i as i64 * MENU_ROW, if off || !can { 0x6A_6280 } else { 0xE8_E4F0 }, item);
            }
        }
    }
}
