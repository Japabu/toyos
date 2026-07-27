use core::f64::consts::PI;

// --- Transcendentals (hand-rolled: LLVM would emit calls to these same symbols) ---

#[no_mangle]
pub extern "C" fn sin(x: f64) -> f64 {
    // Taylor series, reduced to [-PI, PI]
    let mut x = x % (2.0 * PI);
    if x > PI { x -= 2.0 * PI; }
    if x < -PI { x += 2.0 * PI; }
    let x2 = x * x;
    let mut term = x;
    let mut sum = x;
    for i in 1..12 {
        term *= -x2 / ((2 * i) * (2 * i + 1)) as f64;
        sum += term;
    }
    sum
}

#[no_mangle]
pub extern "C" fn cos(x: f64) -> f64 {
    sin(x + PI / 2.0)
}

#[no_mangle]
pub extern "C" fn tan(x: f64) -> f64 {
    sin(x) / cos(x)
}

/// atan(x) for |x| <= tan(π/8) ≈ 0.4142 — Taylor series converges fast here.
fn atan_small(x: f64) -> f64 {
    let x2 = x * x;
    let mut sum = 0.0;
    let mut term = x;
    for i in 0..16 {
        if i > 0 { term *= -x2; }
        sum += term / (2 * i + 1) as f64;
    }
    sum
}

/// atan(x) for x >= 0, with range reduction.
fn atan_positive(x: f64) -> f64 {
    if x > 1.0 {
        return PI / 2.0 - atan_positive(1.0 / x);
    }
    if x > 0.4142135623730950 {
        return PI / 4.0 + atan_small((x - 1.0) / (x + 1.0));
    }
    atan_small(x)
}

#[no_mangle]
pub extern "C" fn atan(x: f64) -> f64 {
    if x >= 0.0 { atan_positive(x) } else { -atan_positive(-x) }
}

#[no_mangle]
pub extern "C" fn atan2(y: f64, x: f64) -> f64 {
    if x == 0.0 {
        if y > 0.0 { return PI / 2.0; }
        if y < 0.0 { return -PI / 2.0; }
        return 0.0;
    }
    if y == 0.0 {
        return if x > 0.0 { 0.0 } else { PI };
    }
    let mut angle = atan_positive(y.abs() / x.abs());
    if x < 0.0 { angle = PI - angle; }
    if y < 0.0 { angle = -angle; }
    angle
}

#[no_mangle]
pub extern "C" fn asin(x: f64) -> f64 {
    if x < -1.0 || x > 1.0 { return f64::NAN; }
    atan2(x, sqrt(1.0 - x * x))
}

#[no_mangle]
pub extern "C" fn acos(x: f64) -> f64 {
    if x < -1.0 || x > 1.0 { return f64::NAN; }
    atan2(sqrt(1.0 - x * x), x)
}

#[no_mangle]
pub extern "C" fn sinh(x: f64) -> f64 {
    let ex = exp(x);
    (ex - 1.0 / ex) * 0.5
}

#[no_mangle]
pub extern "C" fn cosh(x: f64) -> f64 {
    let ex = exp(x);
    (ex + 1.0 / ex) * 0.5
}

#[no_mangle]
pub extern "C" fn tanh(x: f64) -> f64 {
    let ex = exp(2.0 * x);
    (ex - 1.0) / (ex + 1.0)
}

#[no_mangle]
pub extern "C" fn log(x: f64) -> f64 {
    if x <= 0.0 { return f64::NAN; }
    let bits = x.to_bits();
    let e = ((bits >> 52) & 0x7FF) as i64 - 1023;
    let m = f64::from_bits((bits & 0x000FFFFFFFFFFFFF) | 0x3FF0000000000000);
    let t = (m - 1.0) / (m + 1.0);
    let t2 = t * t;
    let mut sum = 1.0;
    let mut t2k = 1.0;
    for k in 1..16 {
        t2k *= t2;
        sum += t2k / (2 * k + 1) as f64;
    }
    2.0 * t * sum + e as f64 * core::f64::consts::LN_2
}

#[no_mangle]
pub extern "C" fn log2(x: f64) -> f64 {
    log(x) / core::f64::consts::LN_2
}

#[no_mangle]
pub extern "C" fn log10(x: f64) -> f64 {
    log(x) / core::f64::consts::LN_10
}

#[no_mangle]
pub extern "C" fn exp(x: f64) -> f64 {
    exp_approx(x)
}

fn exp_approx(x: f64) -> f64 {
    if x > 709.0 { return f64::INFINITY; }
    if x < -709.0 { return 0.0; }
    let k = (x / core::f64::consts::LN_2) as i64;
    let r = x - k as f64 * core::f64::consts::LN_2;
    let mut sum = 1.0;
    let mut term = 1.0;
    for i in 1..16 {
        term *= r / i as f64;
        sum += term;
    }
    // 2^k via IEEE 754 bit manipulation; clamp to valid biased exponent range
    let biased = k + 1023;
    if biased <= 0 { return 0.0; }
    if biased >= 2047 { return f64::INFINITY; }
    f64::from_bits((biased as u64) << 52) * sum
}

#[no_mangle]
pub extern "C" fn pow(base: f64, exp: f64) -> f64 {
    if exp == 0.0 { return 1.0; }
    if base == 0.0 { return 0.0; }
    let ei = exp as i32;
    if exp == ei as f64 {
        let mut result = 1.0;
        let mut b = base;
        let mut e = if ei < 0 { -ei as u32 } else { ei as u32 };
        while e > 0 {
            if e & 1 != 0 { result *= b; }
            b *= b;
            e >>= 1;
        }
        if ei < 0 { 1.0 / result } else { result }
    } else {
        exp_approx(exp * log(base))
    }
}

#[no_mangle]
pub extern "C" fn fmod(x: f64, y: f64) -> f64 {
    x - trunc(x / y) * y
}

// --- Math primitives ---
// WARNING: Do NOT delegate to Rust's .floor()/.ceil()/.round()/.trunc() methods!
// On targets without SSE4.1 (like ToyOS x86-64 base), LLVM lowers the intrinsics
// to calls to these C symbols, creating infinite recursion.

#[no_mangle]
pub extern "C" fn floor(x: f64) -> f64 {
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7FF) as i32 - 1023;
    if exp < 0 {
        return if x < 0.0 { -1.0 } else { 0.0 };
    }
    if exp >= 52 { return x; } // already integer (or NaN/inf)
    let mask = !((1u64 << (52 - exp as u32)) - 1);
    let truncated = f64::from_bits(bits & mask);
    if x < 0.0 && truncated != x { truncated - 1.0 } else { truncated }
}

#[no_mangle]
pub extern "C" fn ceil(x: f64) -> f64 { -floor(-x) }

#[no_mangle]
pub extern "C" fn trunc(x: f64) -> f64 {
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7FF) as i32 - 1023;
    if exp < 0 { return if (bits >> 63) != 0 { -0.0 } else { 0.0 }; }
    if exp >= 52 { return x; }
    let mask = !((1u64 << (52 - exp as u32)) - 1);
    f64::from_bits(bits & mask)
}

#[no_mangle]
pub extern "C" fn round(x: f64) -> f64 {
    floor(x + 0.5)
}

#[no_mangle]
pub extern "C" fn floorf(x: f32) -> f32 {
    let bits = x.to_bits();
    let exp = ((bits >> 23) & 0xFF) as i32 - 127;
    if exp < 0 {
        return if x < 0.0 { -1.0 } else { 0.0 };
    }
    if exp >= 23 { return x; }
    let mask = !((1u32 << (23 - exp as u32)) - 1);
    let truncated = f32::from_bits(bits & mask);
    if x < 0.0 && truncated != x { truncated - 1.0 } else { truncated }
}

#[no_mangle]
pub extern "C" fn ceilf(x: f32) -> f32 { -floorf(-x) }

#[no_mangle]
pub extern "C" fn sqrt(x: f64) -> f64 {
    #[cfg(target_arch = "x86_64")]
    {
        let result: f64;
        unsafe { core::arch::asm!("sqrtsd {0}, {0}", inout(xmm_reg) x => result); }
        return result;
    }
    #[cfg(target_arch = "aarch64")]
    {
        let result: f64;
        unsafe { core::arch::asm!("fsqrt {0:d}, {0:d}", inout(vreg) x => result); }
        return result;
    }
}

#[no_mangle]
pub extern "C" fn sqrtf(x: f32) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        let result: f32;
        unsafe { core::arch::asm!("sqrtss {0}, {0}", inout(xmm_reg) x => result); }
        return result;
    }
    #[cfg(target_arch = "aarch64")]
    {
        let result: f32;
        unsafe { core::arch::asm!("fsqrt {0:s}, {0:s}", inout(vreg) x => result); }
        return result;
    }
}

#[no_mangle]
pub extern "C" fn fabs(x: f64) -> f64 { f64::from_bits(x.to_bits() & !(1u64 << 63)) }

#[no_mangle]
pub extern "C" fn fabsf(x: f32) -> f32 { f32::from_bits(x.to_bits() & !(1u32 << 31)) }

// --- Utility functions ---

#[no_mangle]
pub extern "C" fn ldexp(x: f64, exp: i32) -> f64 {
    // x * 2^exp via bit manipulation
    x * f64::from_bits(((exp as i64 + 1023) as u64) << 52)
}

#[no_mangle]
pub unsafe extern "C" fn frexp(x: f64, exp: *mut i32) -> f64 {
    if x == 0.0 {
        *exp = 0;
        return 0.0;
    }
    let bits = x.to_bits();
    let biased = ((bits >> 52) & 0x7FF) as i32;
    *exp = biased - 1022; // exponent such that x = mantissa * 2^exp, mantissa in [0.5, 1.0)
    f64::from_bits((bits & 0x800FFFFFFFFFFFFF) | 0x3FE0000000000000)
}

#[no_mangle]
pub extern "C" fn isnan(x: f64) -> i32 { x.is_nan() as i32 }

#[no_mangle]
pub extern "C" fn isinf(x: f64) -> i32 { x.is_infinite() as i32 }

#[no_mangle]
pub extern "C" fn isfinite(x: f64) -> i32 { x.is_finite() as i32 }