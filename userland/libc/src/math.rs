use core::f64::consts::PI;

#[no_mangle]
pub extern "C" fn sin(x: f64) -> f64 {
    // Taylor series for sin(x), normalized to [-PI, PI]
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

#[no_mangle]
pub extern "C" fn atan(x: f64) -> f64 {
    atan2(x, 1.0)
}

#[no_mangle]
pub extern "C" fn atan2(y: f64, x: f64) -> f64 {
    // CORDIC-style approximation
    if x == 0.0 {
        if y > 0.0 { return PI / 2.0; }
        if y < 0.0 { return -PI / 2.0; }
        return 0.0;
    }
    let a = y.abs().min(x.abs()) / y.abs().max(x.abs());
    let s = a * a;
    let mut r = ((-0.0464964749 * s + 0.15931422) * s - 0.327622764) * s * a + a;
    if y.abs() > x.abs() { r = PI / 2.0 - r; }
    if x < 0.0 { r = PI - r; }
    if y < 0.0 { r = -r; }
    r
}

#[no_mangle]
pub extern "C" fn sqrt(x: f64) -> f64 {
    if x < 0.0 { return f64::NAN; }
    if x == 0.0 { return 0.0; }
    let mut guess = x;
    for _ in 0..64 {
        guess = (guess + x / guess) * 0.5;
    }
    guess
}

#[no_mangle]
pub extern "C" fn fabs(x: f64) -> f64 {
    if x < 0.0 { -x } else { x }
}

#[no_mangle]
pub extern "C" fn ceil(x: f64) -> f64 {
    let i = x as i64;
    if x > i as f64 { (i + 1) as f64 } else { i as f64 }
}

#[no_mangle]
pub extern "C" fn floor(x: f64) -> f64 {
    let i = x as i64;
    if (i as f64) > x { (i - 1) as f64 } else { i as f64 }
}

#[no_mangle]
pub extern "C" fn round(x: f64) -> f64 {
    floor(x + 0.5)
}

#[no_mangle]
pub extern "C" fn pow(base: f64, exp: f64) -> f64 {
    if exp == 0.0 { return 1.0; }
    if base == 0.0 { return 0.0; }
    // Integer exponent fast path
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
        // exp(exp * ln(base))
        let ln_base = log(base);
        exp_approx(exp * ln_base)
    }
}

#[no_mangle]
pub extern "C" fn fmod(x: f64, y: f64) -> f64 {
    x - (x / y) as i64 as f64 * y
}

#[no_mangle]
pub extern "C" fn log(x: f64) -> f64 {
    if x <= 0.0 { return f64::NAN; }
    // Reduce x to [1, 2) and compute ln(x) = ln(m * 2^e) = ln(m) + e * ln(2)
    let bits = x.to_bits();
    let e = ((bits >> 52) & 0x7FF) as i64 - 1023;
    let m = f64::from_bits((bits & 0x000FFFFFFFFFFFFF) | 0x3FF0000000000000);
    // ln(m) for m in [1, 2): Padé approximation
    let t = (m - 1.0) / (m + 1.0);
    let t2 = t * t;
    let ln_m = 2.0 * t * (1.0 + t2 / 3.0 + t2 * t2 / 5.0 + t2 * t2 * t2 / 7.0);
    ln_m + e as f64 * core::f64::consts::LN_2
}

fn exp_approx(x: f64) -> f64 {
    if x > 709.0 { return f64::INFINITY; }
    if x < -709.0 { return 0.0; }
    // Reduce: e^x = 2^k * e^r where r = x - k*ln2
    let k = (x / core::f64::consts::LN_2) as i64;
    let r = x - k as f64 * core::f64::consts::LN_2;
    // Taylor for e^r, r in [-ln2/2, ln2/2]
    let mut sum = 1.0;
    let mut term = 1.0;
    for i in 1..16 {
        term *= r / i as f64;
        sum += term;
    }
    // Multiply by 2^k
    f64::from_bits(((k + 1023) as u64) << 52) * sum
}

#[inline(never)]
pub fn _libc_math_init() {}
