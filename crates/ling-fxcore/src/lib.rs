//! LingStationX's insert / master effects for LingOS.
//!
//! On Windows the effects run inside `ling-daw` (std). The LingOS kernel DAW
//! (`ling-kernel/src/lingdaw.rs`) needs the very same sound, so this crate is a
//! no_std + alloc copy of ling-daw's effect sources (`src/dsp/*.rs`, generated
//! by `tools/gen.py` — never edit them here) with:
//!
//! * a prelude giving the float maths std has as inherent methods (`sin`,
//!   `powf`, `tanh` ...) through libm,
//! * `host::load_audio`, the hook the kernel sets to decode a sample file (the
//!   Rainbow Hypergrowl's spectral source),
//! * [`Slot`]: ling-daw `model::EffectSlot` (name, knobs, bypass, dry/wet).
//!
//! `cargo test -p ling-fxcore` renders every effect here and in ling-daw on the
//! same input and checks they agree.
#![no_std]
extern crate alloc;

mod math;
pub mod prelude;
pub mod mixai;
pub mod host;
pub mod dsp;
/// ling-daw gesture.rs (generated): pitch gestures, pressure and timbre curves on notes, the velocity line.
#[allow(unused, dead_code)]
pub mod gesture;
pub mod model;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

pub use dsp::fx::{defaults, make, presets, Effect, EFFECT_NAMES};

/// One effect in a chain: ling-daw's `model::EffectSlot`.
pub struct Slot {
    pub name: String,
    pub params: Vec<(&'static str, f32)>,
    pub bypass: bool,
    /// dry / wet of the whole effect, 0 = only the input, 1 = only the effect
    pub wet: f32,
    pub fx: Box<dyn Effect>,
}

impl Slot {
    pub fn new(name: &str, sr: f32) -> Option<Slot> {
        let fx = make(name, sr)?;
        Some(Slot { name: String::from(name), params: defaults(name).to_vec(), bypass: false, wet: 1.0, fx })
    }

    /// Run the effect on a block (interleaved stereo, in place) and blend it with the untouched input by `wet`.
    pub fn process(&mut self, buf: &mut [f32]) {
        let w = self.wet.clamp(0.0, 1.0);
        if w >= 0.999 {
            self.fx.process(buf);
            return;
        }
        let dry = buf.to_vec();
        self.fx.process(buf);
        let (kd, kw) = (1.0 - w, w);
        for (o, d) in buf.iter_mut().zip(dry.iter()) {
            *o = *d * kd + *o * kw;
        }
    }

    pub fn set(&mut self, idx: usize, v: f32) {
        if !v.is_finite() {
            return;
        }
        if let Some(p) = self.params.get_mut(idx) {
            p.1 = v.clamp(0.0, 1.0);
            self.fx.set_param(p.0, p.1);
        }
    }

    /// Set a knob by name (a saved project's params); false when the effect has no such knob.
    pub fn set_named(&mut self, name: &str, v: f32) -> bool {
        match self.params.iter().position(|(n, _)| *n == name) {
            Some(i) => {
                self.set(i, v);
                true
            },
            None => false,
        }
    }
}

/// NaN / inf -> 0 and a hard clamp far above full scale, after every effect (ling-daw engine `scrub`).
pub fn scrub(buf: &mut [f32]) {
    for x in buf.iter_mut() {
        if !x.is_finite() {
            *x = 0.0;
        } else if x.abs() > 32.0 {
            *x = if *x < 0.0 { -32.0 } else { 32.0 };
        }
    }
}
