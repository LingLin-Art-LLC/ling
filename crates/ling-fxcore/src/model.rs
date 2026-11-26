//! The note as ling-daw `model.rs` has it, for `gesture` (the piano roll's per-note expression). The kernel copies its
//! project notes into these, runs the edit, and copies the curves and velocities back.
#[allow(unused_imports)]
use crate::prelude::*;

/// (position 0..1 of the note, value) breakpoints
pub type Curve = Vec<(f32, f32)>;

#[derive(Clone, Debug, Default)]
pub struct Note {
    pub start: f64,
    pub len: f64,
    pub key: u8,
    pub vel: f32,
    pub pan: f32,
    pub cutoff: f32,
    pub reso: f32,
    pub lyric: String,
    pub bend: Curve,
    pub press: Curve,
    pub timbre: Curve,
}

impl Note {
    pub fn new(start: f64, len: f64, key: u8, vel: f32) -> Note {
        Note { start, len, key, vel, pan: 0.0, cutoff: 0.5, reso: 0.5, lyric: String::new(), bend: vec![], press: vec![], timbre: vec![] }
    }
    pub fn has_expr(&self) -> bool {
        !self.bend.is_empty() || !self.press.is_empty() || !self.timbre.is_empty()
    }
    /// ling-daw `Note::cut`: keep the part from `a` to `b` beats after the start; the curves keep what sounded there, in place.
    pub fn cut(&mut self, a: f64, b: f64) {
        let (a, b) = (a.max(0.0), b.min(self.len).max(a.max(0.0)));
        if self.has_expr() && self.len > 0.0 && (a > 1e-9 || b < self.len - 1e-9) {
            let (fa, fb) = ((a / self.len) as f32, (b / self.len) as f32);
            for c in [&mut self.bend, &mut self.press, &mut self.timbre] {
                *c = crate::gesture::trim(c, fa, fb);
            }
        }
        self.start += a;
        self.len = b - a;
    }
}
