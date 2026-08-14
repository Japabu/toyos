//! Exact rationals over [`Int`], always in lowest terms with a positive
//! denominator.
//!
//! This is what makes `1 ÷ 3 × 3` display `1`: the four operations never leave
//! this type, so nothing rounds until the value is drawn.

use core::cmp::Ordering;

use crate::bigint::Int;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Rational {
    num: Int,
    /// Strictly positive, and coprime with `num`.
    den: Int,
}

impl Rational {
    pub fn zero() -> Rational {
        Rational { num: Int::zero(), den: Int::one() }
    }

    pub fn one() -> Rational {
        Rational { num: Int::one(), den: Int::one() }
    }

    pub fn from_int(num: Int) -> Rational {
        Rational { num, den: Int::one() }
    }

    pub fn from_i64(v: i64) -> Rational {
        Rational::from_int(Int::from_i64(v))
    }

    /// `num / den` in lowest terms, or `None` when `den` is zero.
    pub fn new(num: Int, den: Int) -> Option<Rational> {
        if den.is_zero() {
            return None;
        }
        let mut num = num;
        let mut den = den;
        if den.is_negative() {
            num = -&num;
            den = -&den;
        }
        let g = num.gcd(&den);
        if !g.is_zero() && g != Int::one() {
            num = num.div_rem(&g).expect("a gcd is never zero here").0;
            den = den.div_rem(&g).expect("a gcd is never zero here").0;
        }
        if num.is_zero() {
            den = Int::one();
        }
        Some(Rational { num, den })
    }

    pub fn numerator(&self) -> &Int {
        &self.num
    }

    pub fn denominator(&self) -> &Int {
        &self.den
    }

    pub fn is_zero(&self) -> bool {
        self.num.is_zero()
    }

    pub fn is_negative(&self) -> bool {
        self.num.is_negative()
    }

    pub fn is_integer(&self) -> bool {
        self.den == Int::one()
    }

    /// The value as an integer, when it is one.
    pub fn to_int(&self) -> Option<Int> {
        self.is_integer().then(|| self.num.clone())
    }

    pub fn neg(&self) -> Rational {
        Rational { num: -&self.num, den: self.den.clone() }
    }

    pub fn abs(&self) -> Rational {
        Rational { num: self.num.abs(), den: self.den.clone() }
    }

    pub fn add(&self, other: &Rational) -> Rational {
        let num = &(&self.num * &other.den) + &(&other.num * &self.den);
        let den = &self.den * &other.den;
        Rational::new(num, den).expect("a product of positive denominators is positive")
    }

    pub fn sub(&self, other: &Rational) -> Rational {
        self.add(&other.neg())
    }

    pub fn mul(&self, other: &Rational) -> Rational {
        Rational::new(&self.num * &other.num, &self.den * &other.den)
            .expect("a product of positive denominators is positive")
    }

    /// `None` when `other` is zero — the one refusal this type owns.
    pub fn div(&self, other: &Rational) -> Option<Rational> {
        Rational::new(&self.num * &other.den, &self.den * &other.num)
    }

    /// `self^exp`, exact. `None` when zero is raised to a negative power.
    pub fn pow_i64(&self, exp: i64) -> Option<Rational> {
        if exp == 0 {
            return Some(Rational::one());
        }
        let mag = u32::try_from(exp.unsigned_abs()).ok()?;
        let num = self.num.pow(mag);
        let den = self.den.pow(mag);
        if exp > 0 {
            Rational::new(num, den)
        } else {
            Rational::new(den, num)
        }
    }

    /// The exact square root, when both halves are perfect squares.
    ///
    /// A negative value has none, and neither has `2` — those go to the
    /// approximate path, which is what the `≈` on the display announces.
    pub fn sqrt_exact(&self) -> Option<Rational> {
        if self.is_negative() {
            return None;
        }
        let rn = self.num.isqrt()?;
        let rd = self.den.isqrt()?;
        if &rn * &rn != self.num || &rd * &rd != self.den {
            return None;
        }
        Rational::new(rn, rd)
    }

    /// How many digits the exact decimal expansion has after the point, or
    /// `None` when it does not terminate.
    ///
    /// A reduced denominator terminates exactly when its only prime factors are
    /// two and five, and then the count is the larger of the two exponents.
    pub fn terminating_scale(&self) -> Option<u32> {
        let mut den = self.den.clone();
        let two = Int::from_i64(2);
        let five = Int::from_i64(5);
        let mut e2 = 0u32;
        let mut e5 = 0u32;
        while let Some((q, r)) = den.div_rem(&two) {
            if !r.is_zero() {
                break;
            }
            den = q;
            e2 += 1;
        }
        while let Some((q, r)) = den.div_rem(&five) {
            if !r.is_zero() {
                break;
            }
            den = q;
            e5 += 1;
        }
        (den == Int::one()).then(|| e2.max(e5))
    }

    /// `(sign, digits, exponent)` with `value = ±digits · 10^exponent`, for a
    /// value whose decimal terminates. `None` when it does not.
    pub fn exact_decimal(&self) -> Option<(bool, String, i64)> {
        let scale = self.terminating_scale()?;
        let scaled = self.num.mul_pow10(scale);
        let n = scaled.div_rem(&self.den).expect("a denominator is never zero").0;
        let mut digits = n.decimal_digits();
        let mut exp = -(scale as i64);
        while digits.len() > 1 && digits.ends_with('0') {
            digits.pop();
            exp += 1;
        }
        if digits == "0" {
            exp = 0;
        }
        Some((self.num.is_negative(), digits, exp))
    }

    /// Parse an unsigned decimal literal: digits, at most one point, and at
    /// least one digit somewhere.
    pub fn parse_decimal(text: &str) -> Option<Rational> {
        let (int_part, frac_part) = match text.split_once('.') {
            Some((a, b)) => (a, b),
            None => (text, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return None;
        }
        if frac_part.contains('.') {
            return None;
        }
        let mut digits = String::with_capacity(int_part.len() + frac_part.len());
        digits.push_str(int_part);
        digits.push_str(frac_part);
        let num = Int::parse_radix(&digits, 10)?;
        Rational::new(num, Int::pow10(u32::try_from(frac_part.len()).ok()?))
    }
}

impl Ord for Rational {
    fn cmp(&self, other: &Rational) -> Ordering {
        (&self.num * &other.den).cmp(&(&other.num * &self.den))
    }
}

impl PartialOrd for Rational {
    fn partial_cmp(&self, other: &Rational) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(n: i64, d: i64) -> Rational {
        Rational::new(Int::from_i64(n), Int::from_i64(d)).unwrap()
    }

    #[test]
    fn construction_normalizes() {
        assert_eq!(r(2, 4), r(1, 2));
        assert_eq!(r(-2, -4), r(1, 2));
        assert_eq!(r(2, -4), r(-1, 2));
        assert_eq!(r(0, -7), Rational::zero());
        assert_eq!(*r(0, -7).denominator(), Int::one());
        assert!(Rational::new(Int::one(), Int::zero()).is_none());
    }

    #[test]
    fn a_third_times_three_is_one() {
        assert_eq!(Rational::one().div(&r(3, 1)).unwrap().mul(&r(3, 1)), Rational::one());
        assert_eq!(Rational::one().div(&r(7, 1)).unwrap().mul(&r(7, 1)), Rational::one());
    }

    #[test]
    fn a_tenth_plus_two_tenths_is_three_tenths() {
        let a = Rational::parse_decimal("0.1").unwrap();
        let b = Rational::parse_decimal("0.2").unwrap();
        assert_eq!(a.add(&b), Rational::parse_decimal("0.3").unwrap());
    }

    #[test]
    fn division_by_zero_is_the_only_refusal() {
        assert!(r(1, 2).div(&Rational::zero()).is_none());
        assert_eq!(r(1, 2).div(&r(1, 4)).unwrap(), r(2, 1));
    }

    #[test]
    fn powers_include_the_reciprocal() {
        assert_eq!(r(2, 3).pow_i64(3).unwrap(), r(8, 27));
        assert_eq!(r(2, 3).pow_i64(-3).unwrap(), r(27, 8));
        assert_eq!(r(5, 1).pow_i64(0).unwrap(), Rational::one());
        assert!(Rational::zero().pow_i64(-1).is_none());
        assert_eq!(Rational::zero().pow_i64(0).unwrap(), Rational::one());
    }

    #[test]
    fn exact_roots_only_where_both_halves_are_squares() {
        assert_eq!(r(9, 16).sqrt_exact().unwrap(), r(3, 4));
        assert_eq!(r(0, 1).sqrt_exact().unwrap(), Rational::zero());
        assert!(r(2, 1).sqrt_exact().is_none());
        assert!(r(1, 2).sqrt_exact().is_none());
        assert!(r(-4, 1).sqrt_exact().is_none());
    }

    #[test]
    fn a_decimal_terminates_only_on_twos_and_fives() {
        assert_eq!(r(1, 8).terminating_scale(), Some(3));
        assert_eq!(r(1, 20).terminating_scale(), Some(2));
        assert_eq!(r(3, 1).terminating_scale(), Some(0));
        assert_eq!(r(1, 3).terminating_scale(), None);
        assert_eq!(r(1, 6).terminating_scale(), None);
        assert_eq!(r(1, 8).exact_decimal(), Some((false, "125".into(), -3)));
        assert_eq!(r(-1, 8).exact_decimal(), Some((true, "125".into(), -3)));
        assert_eq!(r(1200, 1).exact_decimal(), Some((false, "12".into(), 2)));
        assert_eq!(Rational::zero().exact_decimal(), Some((false, "0".into(), 0)));
    }

    #[test]
    fn parsing_takes_what_a_literal_may_be() {
        assert_eq!(Rational::parse_decimal("12.25").unwrap(), r(49, 4));
        assert_eq!(Rational::parse_decimal(".5").unwrap(), r(1, 2));
        assert_eq!(Rational::parse_decimal("5.").unwrap(), r(5, 1));
        assert!(Rational::parse_decimal(".").is_none());
        assert!(Rational::parse_decimal("").is_none());
        assert!(Rational::parse_decimal("1.2.3").is_none());
        assert!(Rational::parse_decimal("1e5").is_none());
    }

    #[test]
    fn ordering_is_by_value() {
        assert!(r(1, 3) < r(1, 2));
        assert!(r(-1, 3) < r(1, 1000000));
        assert!(r(2, 4) == r(1, 2));
    }
}
