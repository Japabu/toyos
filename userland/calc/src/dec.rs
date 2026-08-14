//! Decimal approximations carried to a fixed number of significant digits.
//!
//! Everything the rationals cannot hold exactly lands here: √2, every
//! logarithm, every trigonometric value, and any power with a fractional
//! exponent. A value that has been through this module is approximate for
//! good, which is what the `≈` on the display says.
//!
//! The working precision is the displayed precision plus guard digits, because
//! the series below reduce their argument by halving and then square the answer
//! back up — every squaring doubles the relative error, and the guard is what
//! pays for it.

use core::cmp::Ordering;

use crate::bigint::Int;
use crate::error::EvalError;
use crate::rational::Rational;

/// Significant digits a result is displayed to.
pub const DISPLAY_DIGITS: usize = 40;
/// Digits computed beyond the displayed ones.
pub const GUARD_DIGITS: usize = 25;
/// The precision every public entry point here works at.
pub const PREC: usize = DISPLAY_DIGITS + GUARD_DIGITS;

/// π to 200 decimals. More than [`PREC`] needs, because reducing a large angle
/// spends digits: the reduction runs at `PREC` plus the argument's own
/// magnitude, and [`Dec::sin_cos`] refuses an argument this cannot cover.
const PI_DIGITS: &str = "3\
14159265358979323846\
26433832795028841971\
69399375105820974944\
59230781640628620899\
86280348253421170679\
82148086513282306647\
09384460955058223172\
53594081284811174502\
84102701938521105559\
64462294895493038196";

/// e to 100 decimals.
const E_DIGITS: &str = "2\
71828182845904523536\
02874713526624977572\
47093699959574966967\
62772407663035354759\
45713821785251664274";

/// The largest decimal exponent a result may have. Beyond it the answer is not
/// a number anyone reads, and computing it is not free.
const MAX_EXP10: i64 = 100_000;

/// The largest `sin`/`cos`/`tan` argument, as a decimal exponent. Reduction
/// needs `PREC` digits of π below the argument's leading digit, and [`PI_DIGITS`]
/// carries 201.
const MAX_ANGLE_EXP10: i64 = 30;

/// `sig · 10^exp`.
///
/// Canonical: zero is `sig = 0, exp = 0`; every other value has no trailing
/// decimal zero in `sig` and no more than the precision it was rounded at.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Dec {
    sig: Int,
    exp: i64,
}

impl Dec {
    pub fn zero() -> Dec {
        Dec { sig: Int::zero(), exp: 0 }
    }

    pub fn one() -> Dec {
        Dec { sig: Int::one(), exp: 0 }
    }

    pub fn from_int(v: Int) -> Dec {
        Dec { sig: v, exp: 0 }.stripped()
    }

    pub fn from_i64(v: i64) -> Dec {
        Dec::from_int(Int::from_i64(v))
    }

    pub fn from_rational(v: &Rational, prec: usize) -> Dec {
        Dec::from_int(v.numerator().clone())
            .div(&Dec::from_int(v.denominator().clone()), prec)
            .expect("a rational's denominator is never zero")
    }

    /// π, rounded to `prec` significant digits.
    pub fn pi(prec: usize) -> Dec {
        Dec::from_constant(PI_DIGITS, prec)
    }

    /// e, rounded to `prec` significant digits.
    pub fn e(prec: usize) -> Dec {
        Dec::from_constant(E_DIGITS, prec)
    }

    fn from_constant(digits: &str, prec: usize) -> Dec {
        assert!(
            digits.len() > prec,
            "the stored constant has {} digits and {prec} were asked for",
            digits.len()
        );
        let sig = Int::parse_radix(digits, 10).expect("the stored constant is decimal");
        Dec { sig, exp: -((digits.len() - 1) as i64) }.rounded(prec)
    }

    pub fn is_zero(&self) -> bool {
        self.sig.is_zero()
    }

    pub fn is_negative(&self) -> bool {
        self.sig.is_negative()
    }

    pub fn neg(&self) -> Dec {
        Dec { sig: -&self.sig, exp: self.exp }
    }

    pub fn abs(&self) -> Dec {
        Dec { sig: self.sig.abs(), exp: self.exp }
    }

    /// `k` such that `10^k <= |self| < 10^(k+1)`. Zero for zero, which no
    /// caller reads without checking first.
    pub fn exp10(&self) -> i64 {
        if self.is_zero() {
            return 0;
        }
        self.sig.decimal_len() as i64 - 1 + self.exp
    }

    /// Round to `prec` significant digits, half to even.
    pub fn rounded(&self, prec: usize) -> Dec {
        let mut sig = self.sig.clone();
        let mut exp = self.exp;
        let digits = sig.decimal_len();
        if !sig.is_zero() && digits > prec {
            let drop = (digits - prec) as u32;
            let p = Int::pow10(drop);
            let (q, r) = sig.div_rem(&p).expect("a power of ten is never zero");
            let twice = &r.abs() + &r.abs();
            let up = match twice.cmp(&p) {
                Ordering::Greater => true,
                Ordering::Equal => q.is_odd(),
                Ordering::Less => false,
            };
            sig = if up {
                if self.sig.is_negative() {
                    &q - &Int::one()
                } else {
                    &q + &Int::one()
                }
            } else {
                q
            };
            exp += drop as i64;
        }
        Dec { sig, exp }.stripped()
    }

    /// Drop trailing decimal zeros, which is exact, and pin zero's exponent.
    fn stripped(self) -> Dec {
        let mut sig = self.sig;
        let mut exp = self.exp;
        if sig.is_zero() {
            return Dec { sig, exp: 0 };
        }
        let ten = Int::from_i64(10);
        loop {
            let (q, r) = sig.div_rem(&ten).expect("ten is never zero");
            if !r.is_zero() {
                break;
            }
            sig = q;
            exp += 1;
        }
        Dec { sig, exp }
    }

    pub fn add(&self, other: &Dec, prec: usize) -> Dec {
        if self.is_zero() {
            return other.rounded(prec);
        }
        if other.is_zero() {
            return self.rounded(prec);
        }
        // Digits below this cannot reach the answer's last displayed place.
        let floor = self.exp10().max(other.exp10()) - prec as i64 - 5;
        let lo = self.exp.min(other.exp).max(floor);
        let a = scale_to(&self.sig, self.exp, lo);
        let b = scale_to(&other.sig, other.exp, lo);
        Dec { sig: &a + &b, exp: lo }.rounded(prec)
    }

    pub fn sub(&self, other: &Dec, prec: usize) -> Dec {
        self.add(&other.neg(), prec)
    }

    pub fn mul(&self, other: &Dec, prec: usize) -> Dec {
        Dec { sig: &self.sig * &other.sig, exp: self.exp + other.exp }.rounded(prec)
    }

    pub fn div(&self, other: &Dec, prec: usize) -> Result<Dec, EvalError> {
        if other.is_zero() {
            return Err(EvalError::DivisionByZero);
        }
        if self.is_zero() {
            return Ok(Dec::zero());
        }
        let want = prec as i64 + 2 + other.sig.decimal_len() as i64 - self.sig.decimal_len() as i64;
        let k = u32::try_from(want.max(0)).map_err(|_| EvalError::Overflow)?;
        let num = self.sig.mul_pow10(k);
        let q = num.div_rem(&other.sig).expect("a non-zero divisor").0;
        Ok(Dec { sig: q, exp: self.exp - other.exp - k as i64 }.rounded(prec))
    }

    /// The non-negative square root. Callers refuse a negative value first.
    pub fn sqrt(&self, prec: usize) -> Dec {
        debug_assert!(!self.is_negative());
        if self.is_zero() {
            return Dec::zero();
        }
        // An even exponent is what lets the root's exponent be exact.
        let (sig, exp) = if self.exp % 2 == 0 {
            (self.sig.clone(), self.exp)
        } else {
            (&self.sig * &Int::from_i64(10), self.exp - 1)
        };
        let want = 2 * (prec + 2);
        let mut k = want.saturating_sub(sig.decimal_len()) as u32;
        if k % 2 != 0 {
            k += 1;
        }
        let root = sig.mul_pow10(k).isqrt().expect("a non-negative significand");
        Dec { sig: root, exp: exp / 2 - (k / 2) as i64 }.rounded(prec)
    }

    /// Nearest integer, halves away from zero.
    pub fn round_int(&self) -> Int {
        if self.is_zero() {
            return Int::zero();
        }
        if self.exp >= 0 {
            return self.sig.mul_pow10(u32::try_from(self.exp).unwrap_or(u32::MAX));
        }
        let drop = -self.exp;
        if drop > self.sig.decimal_len() as i64 + 1 {
            return Int::zero();
        }
        let p = Int::pow10(drop as u32);
        let (q, r) = self.sig.div_rem(&p).expect("a power of ten is never zero");
        let twice = &r.abs() + &r.abs();
        if twice < p {
            q
        } else if self.sig.is_negative() {
            &q - &Int::one()
        } else {
            &q + &Int::one()
        }
    }

    /// `(negative, significant digits, exponent)` rounded to `digits` places,
    /// with `value = ±digits · 10^exponent`.
    pub fn to_digits(&self, digits: usize) -> (bool, String, i64) {
        let r = self.rounded(digits);
        if r.is_zero() {
            return (false, "0".to_string(), 0);
        }
        (r.sig.is_negative(), r.sig.decimal_digits(), r.exp)
    }

    // --- the series ---

    /// `e^self`.
    pub fn exp(&self, prec: usize) -> Result<Dec, EvalError> {
        if self.is_zero() {
            return Ok(Dec::one());
        }
        if self.exp10() >= 6 {
            return Err(EvalError::Overflow);
        }
        // Halve until the argument is below 1/64, where the Taylor series
        // converges in a couple of dozen terms, then square the answer back.
        let halvings = (self.exp10() + 1).max(0) as u32 * 4 + 6;
        let wp = prec + halvings as usize / 3 + 8;
        let r = self.div(&Dec::from_int(Int::pow2(halvings as usize)), wp)?;

        let mut term = Dec::one();
        let mut sum = Dec::one();
        for n in 1..1000u32 {
            term = term.mul(&r, wp).div(&Dec::from_i64(n as i64), wp)?;
            if negligible(&term, &sum, wp) {
                break;
            }
            sum = sum.add(&term, wp);
        }
        for _ in 0..halvings {
            sum = sum.mul(&sum, wp);
        }
        if sum.exp10().abs() > MAX_EXP10 {
            return Err(EvalError::Overflow);
        }
        Ok(sum.rounded(prec))
    }

    /// The natural logarithm. `self` must be strictly positive.
    pub fn ln(&self, prec: usize) -> Result<Dec, EvalError> {
        if self.is_negative() || self.is_zero() {
            return Err(EvalError::LogOfNonPositive);
        }
        if self == &Dec::one() {
            return Ok(Dec::zero());
        }
        let wp = prec + 20;
        // Repeated square roots pull the argument towards one, where
        // ln t = 2·atanh((t−1)/(t+1)) converges four digits a term.
        let limit = Dec { sig: Int::from_i64(15625), exp: -6 }; // 1/64
        let mut t = self.rounded(wp);
        let mut roots = 0u32;
        while roots < 64 && t.sub(&Dec::one(), wp).abs().cmp(&limit) == Ordering::Greater {
            t = t.sqrt(wp);
            roots += 1;
        }

        let z = t.sub(&Dec::one(), wp).div(&t.add(&Dec::one(), wp), wp)?;
        let z2 = z.mul(&z, wp);
        let mut power = z.clone();
        let mut sum = z.clone();
        for k in 1..500u32 {
            power = power.mul(&z2, wp);
            let term = power.div(&Dec::from_i64(2 * k as i64 + 1), wp)?;
            if negligible(&term, &sum, wp) {
                break;
            }
            sum = sum.add(&term, wp);
        }
        let scale = Dec::from_int(Int::pow2(roots as usize + 1));
        Ok(sum.mul(&scale, wp).rounded(prec))
    }

    /// The base-ten logarithm.
    pub fn log10(&self, prec: usize) -> Result<Dec, EvalError> {
        let wp = prec + 10;
        let num = self.ln(wp)?;
        if num.is_zero() {
            return Ok(Dec::zero());
        }
        let den = Dec::from_i64(10).ln(wp)?;
        Ok(num.div(&den, wp)?.rounded(prec))
    }

    /// Sine and cosine of the same reduced angle, in radians.
    pub fn sin_cos(&self, prec: usize) -> Result<(Dec, Dec), EvalError> {
        if self.is_zero() {
            return Ok((Dec::zero(), Dec::one()));
        }
        let magnitude = self.exp10();
        if magnitude > MAX_ANGLE_EXP10 {
            return Err(EvalError::ArgumentTooLarge);
        }
        let wp = prec + 15;
        let rp = wp + magnitude.max(0) as usize;
        let half_pi = Dec::pi(rp).div(&Dec::from_i64(2), rp)?;
        let quadrant = self.div(&half_pi, rp)?.round_int();
        let r = self.sub(&Dec::from_int(quadrant.clone()).mul(&half_pi, rp), rp);

        let (sin_r, cos_r) = taylor_sin_cos(&r, wp)?;
        let four = Int::from_i64(4);
        let mut q = quadrant.div_rem(&four).expect("four is never zero").1.to_i64().unwrap_or(0);
        if q < 0 {
            q += 4;
        }
        let (sin, cos) = match q {
            0 => (sin_r, cos_r),
            1 => (cos_r, sin_r.neg()),
            2 => (sin_r.neg(), cos_r.neg()),
            _ => (cos_r.neg(), sin_r),
        };
        Ok((sin.rounded(prec), cos.rounded(prec)))
    }

    /// `self^other` through `exp(other · ln self)`. Callers handle an exact
    /// integer exponent before reaching here.
    pub fn pow(&self, other: &Dec, prec: usize) -> Result<Dec, EvalError> {
        let wp = prec + 10;
        if self.is_zero() {
            return if other.is_negative() || other.is_zero() {
                Err(EvalError::ZeroToNonPositivePower)
            } else {
                Ok(Dec::zero())
            };
        }
        if self.is_negative() {
            return Err(EvalError::NegativeBaseFractionalExponent);
        }
        let l = self.ln(wp)?;
        Ok(other.mul(&l, wp).exp(wp)?.rounded(prec))
    }
}

impl Ord for Dec {
    fn cmp(&self, other: &Dec) -> Ordering {
        match (self.is_zero(), other.is_zero()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return if other.is_negative() { Ordering::Greater } else { Ordering::Less },
            (false, true) => return if self.is_negative() { Ordering::Less } else { Ordering::Greater },
            (false, false) => {}
        }
        if self.is_negative() != other.is_negative() {
            return if self.is_negative() { Ordering::Less } else { Ordering::Greater };
        }
        let by_magnitude = self.exp10().cmp(&other.exp10());
        if by_magnitude != Ordering::Equal {
            return if self.is_negative() { by_magnitude.reverse() } else { by_magnitude };
        }
        let lo = self.exp.min(other.exp);
        scale_to(&self.sig, self.exp, lo).cmp(&scale_to(&other.sig, other.exp, lo))
    }
}

impl PartialOrd for Dec {
    fn partial_cmp(&self, other: &Dec) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// `sig · 10^from` re-expressed at `10^to`, truncating what falls off the end.
fn scale_to(sig: &Int, from: i64, to: i64) -> Int {
    match from.cmp(&to) {
        Ordering::Equal => sig.clone(),
        Ordering::Greater => sig.mul_pow10(u32::try_from(from - to).unwrap_or(u32::MAX)),
        Ordering::Less => {
            let drop = to - from;
            if drop > sig.decimal_len() as i64 {
                return Int::zero();
            }
            sig.div_rem(&Int::pow10(drop as u32)).expect("a power of ten is never zero").0
        }
    }
}

/// Whether adding `term` to `sum` could still move a digit inside `wp`.
fn negligible(term: &Dec, sum: &Dec, wp: usize) -> bool {
    if term.is_zero() {
        return true;
    }
    if sum.is_zero() {
        return false;
    }
    term.exp10() < sum.exp10() - wp as i64 - 2
}

/// Both series at once for an argument already reduced into `[-π/4, π/4]`.
fn taylor_sin_cos(r: &Dec, wp: usize) -> Result<(Dec, Dec), EvalError> {
    let r2 = r.mul(r, wp);

    let mut term = r.clone();
    let mut sin = r.clone();
    for k in 1..500u32 {
        let d = Dec::from_i64((2 * k as i64) * (2 * k as i64 + 1));
        term = term.mul(&r2, wp).div(&d, wp)?.neg();
        if negligible(&term, &sin, wp) {
            break;
        }
        sin = sin.add(&term, wp);
    }

    let mut term = Dec::one();
    let mut cos = Dec::one();
    for k in 1..500u32 {
        let d = Dec::from_i64((2 * k as i64 - 1) * (2 * k as i64));
        term = term.mul(&r2, wp).div(&d, wp)?.neg();
        if negligible(&term, &cos, wp) {
            break;
        }
        cos = cos.add(&term, wp);
    }

    Ok((sin, cos))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly forty significant digits, trailing zeros and all — a published
    /// digit string carries the zeros the canonical form drops.
    fn shown(v: &Dec) -> String {
        let (neg, mut digits, mut exp) = v.to_digits(DISPLAY_DIGITS);
        if digits != "0" {
            while digits.len() < DISPLAY_DIGITS {
                digits.push('0');
                exp -= 1;
            }
        }
        let point = digits.len() as i64 + exp;
        let mut out = String::new();
        if neg {
            out.push('-');
        }
        if point <= 0 {
            out.push_str("0.");
            for _ in 0..-point {
                out.push('0');
            }
            out.push_str(&digits);
        } else if (point as usize) >= digits.len() {
            out.push_str(&digits);
            for _ in 0..point as usize - digits.len() {
                out.push('0');
            }
        } else {
            out.push_str(&digits[..point as usize]);
            out.push('.');
            out.push_str(&digits[point as usize..]);
        }
        out
    }

    fn dec(s: &str) -> Dec {
        let (neg, body) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let v = Dec::from_rational(&Rational::parse_decimal(body).unwrap(), PREC);
        if neg {
            v.neg()
        } else {
            v
        }
    }

    /// Two values agree to `digits` places, give or take one in the last one.
    fn agree(a: &Dec, b: &Dec, digits: usize) -> bool {
        let diff = a.sub(b, PREC).abs();
        if diff.is_zero() {
            return true;
        }
        let scale = a.abs().max(b.abs());
        // one unit in the last displayed place of the larger operand
        let ulp = Dec { sig: Int::one(), exp: scale.exp10() - digits as i64 + 1 };
        diff.cmp(&ulp) != Ordering::Greater
    }

    #[test]
    fn rounding_is_half_to_even_and_canonical() {
        assert_eq!(dec("0").to_digits(5), (false, "0".to_string(), 0));
        assert_eq!(dec("1.2500").rounded(3).to_digits(3), (false, "125".to_string(), -2));
        assert_eq!(dec("1.25").rounded(2).to_digits(2), (false, "12".to_string(), -1));
        assert_eq!(dec("1.35").rounded(2).to_digits(2), (false, "14".to_string(), -1));
        assert_eq!(dec("-1.25").rounded(2).to_digits(2), (true, "12".to_string(), -1));
        assert_eq!(dec("999.9").rounded(3).to_digits(3), (false, "1".to_string(), 3));
    }

    #[test]
    fn arithmetic_survives_a_wide_exponent_gap() {
        let big = dec("1").mul(&Dec { sig: Int::one(), exp: 100 }, PREC);
        let small = dec("1").mul(&Dec { sig: Int::one(), exp: -100 }, PREC);
        assert_eq!(big.add(&small, PREC), big);
        assert_eq!(big.sub(&big, PREC), Dec::zero());
        assert!(big.cmp(&small) == Ordering::Greater);
        assert!(small.neg().cmp(&big.neg()) == Ordering::Greater);
        assert_eq!(dec("1").div(&dec("8"), PREC), Ok(dec("0.125")));
        assert!(dec("1").div(&Dec::zero(), PREC).is_err());
    }

    #[test]
    fn pi_matches_its_published_digits_and_machin() {
        assert_eq!(
            shown(&Dec::pi(PREC)),
            "3.141592653589793238462643383279502884197"
        );
        // 4·arctan(1/5) − arctan(1/239) reaches the same constant by a road
        // that shares nothing with the stored string.
        let wp = 150;
        let machin = arctan_inv(5, wp)
            .mul(&Dec::from_i64(4), wp)
            .sub(&arctan_inv(239, wp), wp)
            .mul(&Dec::from_i64(4), wp);
        assert!(agree(&machin, &Dec::pi(140), 140), "machin gave {}", shown(&machin));
    }

    /// `arctan(1/n)` by its alternating series, for the check above only.
    fn arctan_inv(n: i64, wp: usize) -> Dec {
        let x = Dec::one().div(&Dec::from_i64(n), wp).unwrap();
        let x2 = x.mul(&x, wp);
        let mut power = x.clone();
        let mut sum = x;
        for k in 1..2000u32 {
            power = power.mul(&x2, wp);
            let term = power.div(&Dec::from_i64(2 * k as i64 + 1), wp).unwrap();
            let term = if k % 2 == 1 { term.neg() } else { term };
            if negligible(&term, &sum, wp) {
                break;
            }
            sum = sum.add(&term, wp);
        }
        sum
    }

    #[test]
    fn e_matches_its_published_digits_and_the_factorial_series() {
        assert_eq!(
            shown(&Dec::e(PREC)),
            "2.718281828459045235360287471352662497757"
        );
        let wp = 90;
        let mut term = Dec::one();
        let mut sum = Dec::one();
        for n in 1..200u32 {
            term = term.div(&Dec::from_i64(n as i64), wp).unwrap();
            if negligible(&term, &sum, wp) {
                break;
            }
            sum = sum.add(&term, wp);
        }
        assert!(agree(&sum, &Dec::e(85), 85), "the series gave {}", shown(&sum));
        // And the routine that computes it agrees with the constant.
        assert!(agree(&Dec::one().exp(PREC).unwrap(), &Dec::e(PREC), DISPLAY_DIGITS));
    }

    #[test]
    fn ln_two_matches_its_published_digits() {
        assert_eq!(
            shown(&Dec::from_i64(2).ln(PREC).unwrap()),
            "0.6931471805599453094172321214581765680755"
        );
        assert!(Dec::zero().ln(PREC).is_err());
        assert!(Dec::from_i64(-1).ln(PREC).is_err());
        assert_eq!(Dec::one().ln(PREC).unwrap(), Dec::zero());
    }

    #[test]
    fn root_two_matches_its_published_digits() {
        assert_eq!(
            shown(&Dec::from_i64(2).sqrt(PREC)),
            "1.414213562373095048801688724209698078570"
        );
        let root = Dec::from_i64(2).sqrt(PREC);
        assert!(agree(&root.mul(&root, PREC), &Dec::from_i64(2), DISPLAY_DIGITS));
        assert_eq!(Dec::zero().sqrt(PREC), Dec::zero());
        assert_eq!(Dec::from_i64(4).sqrt(PREC), Dec::from_i64(2));
    }

    #[test]
    fn sine_and_cosine_of_one_match_their_published_digits() {
        let (sin, cos) = Dec::one().sin_cos(PREC).unwrap();
        assert_eq!(shown(&sin), "0.8414709848078965066525023216302989996226");
        assert_eq!(shown(&cos), "0.5403023058681397174009366074429766037323");
    }

    #[test]
    fn the_identities_hold_to_the_displayed_precision() {
        for x in ["0.5", "1", "-2.25", "7", "100"] {
            let v = dec(x);
            let (s, c) = v.sin_cos(PREC).unwrap();
            let one = s.mul(&s, PREC).add(&c.mul(&c, PREC), PREC);
            assert!(agree(&one, &Dec::one(), DISPLAY_DIGITS), "sin²+cos² at {x} gave {}", shown(&one));

            // The triple-angle identity reaches sin(3x) without the reduction
            // path that produced it.
            let (s3, _) = v.mul(&Dec::from_i64(3), PREC).sin_cos(PREC).unwrap();
            let by_identity = s
                .mul(&Dec::from_i64(3), PREC)
                .sub(&s.mul(&s, PREC).mul(&s, PREC).mul(&Dec::from_i64(4), PREC), PREC);
            assert!(agree(&s3, &by_identity, DISPLAY_DIGITS), "triple angle at {x}");
        }

        for x in ["0.001", "1", "2.5", "1000"] {
            let v = dec(x);
            let back = v.ln(PREC).unwrap().exp(PREC).unwrap();
            assert!(agree(&back, &v, DISPLAY_DIGITS), "exp(ln {x}) gave {}", shown(&back));
        }

        assert!(agree(
            &Dec::from_i64(1000).log10(PREC).unwrap(),
            &Dec::from_i64(3),
            DISPLAY_DIGITS
        ));
        assert!(agree(
            &Dec::from_i64(2).pow(&dec("0.5"), PREC).unwrap(),
            &Dec::from_i64(2).sqrt(PREC),
            DISPLAY_DIGITS
        ));
    }

    #[test]
    fn the_refusals_are_named_rather_than_taken() {
        assert_eq!(Dec::from_i64(-1).ln(PREC), Err(EvalError::LogOfNonPositive));
        assert_eq!(
            Dec::from_i64(-2).pow(&dec("0.5"), PREC),
            Err(EvalError::NegativeBaseFractionalExponent)
        );
        assert_eq!(
            Dec::zero().pow(&dec("-1"), PREC),
            Err(EvalError::ZeroToNonPositivePower)
        );
        assert_eq!(dec("1").mul(&Dec { sig: Int::one(), exp: 40 }, PREC).sin_cos(PREC), Err(EvalError::ArgumentTooLarge));
        assert_eq!(dec("1").mul(&Dec { sig: Int::one(), exp: 8 }, PREC).exp(PREC), Err(EvalError::Overflow));
    }
}
