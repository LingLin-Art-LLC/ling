//! The effect modules, generated from ling-daw/src/dsp (see tools/gen.py), plus the two dsp/mod.rs helpers they use.
#![allow(unused, dead_code, clippy::all)]
pub mod articulator;
pub mod bassfx;
pub mod cinefx;
pub mod edmfx;
pub mod fx;
pub mod guitar;
pub mod hypergrowl;
pub mod master;
pub mod vocoder;
pub mod vox;
pub mod warp;

#[allow(unused_imports)]
use crate::prelude::*;

pub const TAU: f32 = core::f32::consts::TAU;

#[inline]
pub fn midi_hz(k: f32) -> f32 {
    440.0 * 2f32.powf((k - 69.0) / 12.0)
}
