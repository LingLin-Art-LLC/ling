// Symphonia
// Copyright (c) 2019-2023 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Ported to LingOS (no_std + libm) from symphonia-core 0.5.5 dsp/mdct. The FFT
// is a plain iterative radix-2 transform written for this crate.

use alloc::vec::Vec;

#[derive(Clone, Copy, Default, Debug)]
pub struct Complex {
    pub re: f32,
    pub im: f32,
}

impl Complex {
    #[inline(always)]
    pub fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }
    #[inline(always)]
    pub fn mul(self, o: Complex) -> Complex {
        Complex::new(self.re * o.re - self.im * o.im, self.re * o.im + self.im * o.re)
    }
    #[inline(always)]
    pub fn conj(self) -> Complex {
        Complex::new(self.re, -self.im)
    }
}

/// Forward complex FFT (e^{-i}) of a fixed power-of-two size.
pub struct Fft {
    n: usize,
    perm: Vec<u32>,
    twiddle: Vec<Complex>,
}

impl Fft {
    pub fn new(n: usize) -> Self {
        assert!(n.is_power_of_two() && n >= 2);
        let bits = n.trailing_zeros();
        let perm = (0..n as u32).map(|i| i.reverse_bits() >> (32 - bits)).collect();
        let twiddle = (0..n / 2)
            .map(|k| {
                let a = -2.0 * core::f64::consts::PI * k as f64 / n as f64;
                Complex::new(libm::cos(a) as f32, libm::sin(a) as f32)
            })
            .collect();
        Self { n, perm, twiddle }
    }

    pub fn size(&self) -> usize {
        self.n
    }

    pub fn fft(&self, x: &[Complex], y: &mut [Complex]) {
        let n = self.n;
        for (i, &p) in self.perm.iter().enumerate() {
            y[i] = x[p as usize];
        }
        let mut len = 2;
        while len <= n {
            let half = len / 2;
            let step = n / len;
            let mut start = 0;
            while start < n {
                for k in 0..half {
                    let w = self.twiddle[k * step];
                    let a = y[start + k];
                    let b = y[start + k + half].mul(w);
                    y[start + k] = Complex::new(a.re + b.re, a.im + b.im);
                    y[start + k + half] = Complex::new(a.re - b.re, a.im - b.im);
                }
                start += len;
            }
            len <<= 1;
        }
    }
}

/// The Inverse Modified Discrete Cosine Transform (IMDCT).
pub struct Imdct {
    fft: Fft,
    fft_in: Vec<Complex>,
    fft_out: Vec<Complex>,
    twiddle: Vec<Complex>,
}

impl Imdct {
    /// Instantiate a N-point IMDCT with scaling (`n` spectral samples, 2N outputs).
    pub fn new_scaled(n: usize, scale: f64) -> Self {
        assert!(n.is_power_of_two());
        let n2 = n / 2;
        let mut twiddle = Vec::with_capacity(n2);
        let alpha = 1.0 / 8.0 + if scale.is_sign_positive() { 0.0 } else { n2 as f64 };
        let pi_n = core::f64::consts::PI / n as f64;
        let sqrt_scale = libm::sqrt(libm::fabs(scale));
        for k in 0..n2 {
            let theta = pi_n * (alpha + k as f64);
            twiddle.push(Complex::new(
                (sqrt_scale * libm::cos(theta)) as f32,
                (sqrt_scale * libm::sin(theta)) as f32,
            ));
        }
        Imdct {
            fft: Fft::new(n2),
            fft_in: alloc::vec![Complex::default(); n2],
            fft_out: alloc::vec![Complex::default(); n2],
            twiddle,
        }
    }

    pub fn imdct(&mut self, spec: &[f32], out: &mut [f32]) {
        let n = self.fft.size() << 1;
        let n2 = n >> 1;
        let n4 = n >> 2;
        assert_eq!(spec.len(), n);
        assert_eq!(out.len(), 2 * n);

        for (i, (&w, t)) in self.twiddle.iter().zip(self.fft_in.iter_mut()).enumerate() {
            let even = spec[i * 2];
            let odd = -spec[n - 1 - i * 2];
            let re = odd * w.im - even * w.re;
            let im = odd * w.re + even * w.im;
            *t = Complex::new(re, im);
        }

        self.fft.fft(&self.fft_in, &mut self.fft_out);

        let (vec0, vec1) = out.split_at_mut(n2);
        let (vec1, vec2) = vec1.split_at_mut(n2);
        let (vec2, vec3) = vec2.split_at_mut(n2);

        for (i, (x, &w)) in self.fft_out[..n4].iter().zip(self.twiddle[..n4].iter()).enumerate() {
            let val = w.mul(x.conj());
            let fi = 2 * i;
            let ri = n2 - 1 - 2 * i;
            vec0[ri] = -val.im;
            vec1[fi] = val.im;
            vec2[ri] = val.re;
            vec3[fi] = val.re;
        }

        for (i, (x, &w)) in self.fft_out[n4..].iter().zip(self.twiddle[n4..].iter()).enumerate() {
            let val = w.mul(x.conj());
            let fi = 2 * i;
            let ri = n2 - 1 - 2 * i;
            vec0[fi] = -val.re;
            vec1[ri] = val.re;
            vec2[fi] = val.im;
            vec3[ri] = val.im;
        }
    }
}
