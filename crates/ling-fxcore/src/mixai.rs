//! The K-weighting filters of ling-daw `mixai.rs` (the Limiter's loudness meter uses them).
#[allow(unused_imports)]
use crate::prelude::*;
use core::f32::consts::PI;

#[derive(Clone, Copy)]
pub(crate) struct Bq {
    b: [f32; 3],
    a: [f32; 2],
    z: [f32; 2],
}
impl Bq {
    #[inline]
    pub(crate) fn run(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}
pub(crate) fn k_filters(sr: f32) -> [Bq; 2] {
    // stage 1: the head-related high shelf
    let (f0, g, q) = (1681.9745f32, 3.99984f32, 0.707175f32);
    let a = 10f32.powf(g / 40.0);
    let w0 = 2.0 * PI * f0 / sr;
    let (cw, sw) = (w0.cos(), w0.sin());
    let alpha = sw / (2.0 * q);
    let sa = a.sqrt();
    let b0 = a * ((a + 1.0) + (a - 1.0) * cw + 2.0 * sa * alpha);
    let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cw);
    let b2 = a * ((a + 1.0) + (a - 1.0) * cw - 2.0 * sa * alpha);
    let a0 = (a + 1.0) - (a - 1.0) * cw + 2.0 * sa * alpha;
    let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cw);
    let a2 = (a + 1.0) - (a - 1.0) * cw - 2.0 * sa * alpha;
    let s1 = Bq { b: [b0 / a0, b1 / a0, b2 / a0], a: [a1 / a0, a2 / a0], z: [0.0; 2] };
    // stage 2: the revised low-frequency B weighting (a 38 Hz high-pass)
    let (f0, q) = (38.13547f32, 0.500327f32);
    let w0 = 2.0 * PI * f0 / sr;
    let (cw, sw) = (w0.cos(), w0.sin());
    let alpha = sw / (2.0 * q);
    let a0 = 1.0 + alpha;
    let s2 = Bq { b: [(1.0 + cw) / 2.0 / a0, -(1.0 + cw) / a0, (1.0 + cw) / 2.0 / a0], a: [-2.0 * cw / a0, (1.0 - alpha) / a0], z: [0.0; 2] };
    [s1, s2]
}
