//! std's float methods for no_std, through libm. Where core already has a
//! stable inherent method (abs, signum, copysign, min, max, clamp ...) that one
//! is used and these are never reached.

pub trait F32Ext: Sized {
    fn sin(self) -> f32;
    fn cos(self) -> f32;
    fn tan(self) -> f32;
    fn asin(self) -> f32;
    fn acos(self) -> f32;
    fn atan(self) -> f32;
    fn atan2(self, o: f32) -> f32;
    fn sin_cos(self) -> (f32, f32);
    fn sinh(self) -> f32;
    fn cosh(self) -> f32;
    fn tanh(self) -> f32;
    fn exp(self) -> f32;
    fn exp2(self) -> f32;
    fn exp_m1(self) -> f32;
    fn ln(self) -> f32;
    fn ln_1p(self) -> f32;
    fn log(self, b: f32) -> f32;
    fn log2(self) -> f32;
    fn log10(self) -> f32;
    fn powf(self, e: f32) -> f32;
    fn powi(self, e: i32) -> f32;
    fn sqrt(self) -> f32;
    fn cbrt(self) -> f32;
    fn hypot(self, o: f32) -> f32;
    fn floor(self) -> f32;
    fn ceil(self) -> f32;
    fn round(self) -> f32;
    fn trunc(self) -> f32;
    fn fract(self) -> f32;
    fn mul_add(self, a: f32, b: f32) -> f32;
    fn rem_euclid(self, r: f32) -> f32;
    fn div_euclid(self, r: f32) -> f32;
    fn abs(self) -> f32;
    fn signum(self) -> f32;
}

impl F32Ext for f32 {
    #[inline]
    fn sin(self) -> f32 {
        libm::sinf(self)
    }
    #[inline]
    fn cos(self) -> f32 {
        libm::cosf(self)
    }
    #[inline]
    fn tan(self) -> f32 {
        libm::tanf(self)
    }
    #[inline]
    fn asin(self) -> f32 {
        libm::asinf(self)
    }
    #[inline]
    fn acos(self) -> f32 {
        libm::acosf(self)
    }
    #[inline]
    fn atan(self) -> f32 {
        libm::atanf(self)
    }
    #[inline]
    fn atan2(self, o: f32) -> f32 {
        libm::atan2f(self, o)
    }
    #[inline]
    fn sin_cos(self) -> (f32, f32) {
        (libm::sinf(self), libm::cosf(self))
    }
    #[inline]
    fn sinh(self) -> f32 {
        libm::sinhf(self)
    }
    #[inline]
    fn cosh(self) -> f32 {
        libm::coshf(self)
    }
    #[inline]
    fn tanh(self) -> f32 {
        libm::tanhf(self)
    }
    #[inline]
    fn exp(self) -> f32 {
        libm::expf(self)
    }
    #[inline]
    fn exp2(self) -> f32 {
        libm::exp2f(self)
    }
    #[inline]
    fn exp_m1(self) -> f32 {
        libm::expm1f(self)
    }
    #[inline]
    fn ln(self) -> f32 {
        libm::logf(self)
    }
    #[inline]
    fn ln_1p(self) -> f32 {
        libm::log1pf(self)
    }
    #[inline]
    fn log(self, b: f32) -> f32 {
        libm::logf(self) / libm::logf(b)
    }
    #[inline]
    fn log2(self) -> f32 {
        libm::log2f(self)
    }
    #[inline]
    fn log10(self) -> f32 {
        libm::log10f(self)
    }
    #[inline]
    fn powf(self, e: f32) -> f32 {
        libm::powf(self, e)
    }
    #[inline]
    fn powi(self, e: i32) -> f32 {
        let mut r = 1.0f32;
        let mut b = self;
        let mut n = e.unsigned_abs();
        while n > 0 {
            if n & 1 == 1 {
                r *= b;
            }
            b *= b;
            n >>= 1;
        }
        if e < 0 {
            1.0 / r
        } else {
            r
        }
    }
    #[inline]
    fn sqrt(self) -> f32 {
        libm::sqrtf(self)
    }
    #[inline]
    fn cbrt(self) -> f32 {
        libm::cbrtf(self)
    }
    #[inline]
    fn hypot(self, o: f32) -> f32 {
        libm::hypotf(self, o)
    }
    #[inline]
    fn floor(self) -> f32 {
        libm::floorf(self)
    }
    #[inline]
    fn ceil(self) -> f32 {
        libm::ceilf(self)
    }
    #[inline]
    fn round(self) -> f32 {
        libm::roundf(self)
    }
    #[inline]
    fn trunc(self) -> f32 {
        libm::truncf(self)
    }
    #[inline]
    fn fract(self) -> f32 {
        self - libm::truncf(self)
    }
    #[inline]
    fn mul_add(self, a: f32, b: f32) -> f32 {
        libm::fmaf(self, a, b)
    }
    #[inline]
    fn rem_euclid(self, r: f32) -> f32 {
        let m = self % r;
        if m < 0.0 {
            m + r.abs()
        } else {
            m
        }
    }
    #[inline]
    fn div_euclid(self, r: f32) -> f32 {
        let q = libm::truncf(self / r);
        if self % r < 0.0 {
            if r > 0.0 {
                q - 1.0
            } else {
                q + 1.0
            }
        } else {
            q
        }
    }
    #[inline]
    fn abs(self) -> f32 {
        libm::fabsf(self)
    }
    #[inline]
    fn signum(self) -> f32 {
        if self.is_nan() {
            f32::NAN
        } else {
            libm::copysignf(1.0, self)
        }
    }
}

pub trait F64Ext: Sized {
    fn sin(self) -> f64;
    fn cos(self) -> f64;
    fn tan(self) -> f64;
    fn atan(self) -> f64;
    fn atan2(self, o: f64) -> f64;
    fn sin_cos(self) -> (f64, f64);
    fn tanh(self) -> f64;
    fn exp(self) -> f64;
    fn exp2(self) -> f64;
    fn ln(self) -> f64;
    fn log2(self) -> f64;
    fn log10(self) -> f64;
    fn powf(self, e: f64) -> f64;
    fn powi(self, e: i32) -> f64;
    fn sqrt(self) -> f64;
    fn hypot(self, o: f64) -> f64;
    fn floor(self) -> f64;
    fn ceil(self) -> f64;
    fn round(self) -> f64;
    fn trunc(self) -> f64;
    fn fract(self) -> f64;
    fn rem_euclid(self, r: f64) -> f64;
    fn abs(self) -> f64;
    fn signum(self) -> f64;
}

impl F64Ext for f64 {
    #[inline]
    fn sin(self) -> f64 {
        libm::sin(self)
    }
    #[inline]
    fn cos(self) -> f64 {
        libm::cos(self)
    }
    #[inline]
    fn tan(self) -> f64 {
        libm::tan(self)
    }
    #[inline]
    fn atan(self) -> f64 {
        libm::atan(self)
    }
    #[inline]
    fn atan2(self, o: f64) -> f64 {
        libm::atan2(self, o)
    }
    #[inline]
    fn sin_cos(self) -> (f64, f64) {
        (libm::sin(self), libm::cos(self))
    }
    #[inline]
    fn tanh(self) -> f64 {
        libm::tanh(self)
    }
    #[inline]
    fn exp(self) -> f64 {
        libm::exp(self)
    }
    #[inline]
    fn exp2(self) -> f64 {
        libm::exp2(self)
    }
    #[inline]
    fn ln(self) -> f64 {
        libm::log(self)
    }
    #[inline]
    fn log2(self) -> f64 {
        libm::log2(self)
    }
    #[inline]
    fn log10(self) -> f64 {
        libm::log10(self)
    }
    #[inline]
    fn powf(self, e: f64) -> f64 {
        libm::pow(self, e)
    }
    #[inline]
    fn powi(self, e: i32) -> f64 {
        let mut r = 1.0f64;
        let mut b = self;
        let mut n = e.unsigned_abs();
        while n > 0 {
            if n & 1 == 1 {
                r *= b;
            }
            b *= b;
            n >>= 1;
        }
        if e < 0 {
            1.0 / r
        } else {
            r
        }
    }
    #[inline]
    fn sqrt(self) -> f64 {
        libm::sqrt(self)
    }
    #[inline]
    fn hypot(self, o: f64) -> f64 {
        libm::hypot(self, o)
    }
    #[inline]
    fn floor(self) -> f64 {
        libm::floor(self)
    }
    #[inline]
    fn ceil(self) -> f64 {
        libm::ceil(self)
    }
    #[inline]
    fn round(self) -> f64 {
        libm::round(self)
    }
    #[inline]
    fn trunc(self) -> f64 {
        libm::trunc(self)
    }
    #[inline]
    fn fract(self) -> f64 {
        self - libm::trunc(self)
    }
    #[inline]
    fn rem_euclid(self, r: f64) -> f64 {
        let m = self % r;
        if m < 0.0 {
            m + r.abs()
        } else {
            m
        }
    }
    #[inline]
    fn abs(self) -> f64 {
        libm::fabs(self)
    }
    #[inline]
    fn signum(self) -> f64 {
        if self.is_nan() {
            f64::NAN
        } else {
            libm::copysign(1.0, self)
        }
    }
}
