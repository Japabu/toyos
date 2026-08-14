//! One value, either exact or approximate, and the rule that decides how it is
//! written down.
//!
//! Exactness is sticky in one direction only: four operations on two exact
//! values stay exact, and everything that touches an approximation is an
//! approximation from then on. The display says which it is holding.

use crate::dec::{Dec, DISPLAY_DIGITS, PREC};
use crate::error::EvalError;
use crate::rational::Rational;

/// The marker an approximate value is written with.
pub const APPROX: char = '≈';

/// The widest exact power this will compute rather than approximate. A power
/// whose exact form runs past it is still answered — with an `≈`.
const MAX_EXACT_POW_DIGITS: usize = 2000;

#[derive(Clone, PartialEq, Debug)]
pub enum Num {
    Exact(Rational),
    Approx(Dec),
}

/// Radians or degrees, which only the three trigonometric functions read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Angle {
    Rad,
    Deg,
}

impl Num {
    pub fn from_i64(v: i64) -> Num {
        Num::Exact(Rational::from_i64(v))
    }

    pub fn is_approx(&self) -> bool {
        matches!(self, Num::Approx(_))
    }

    pub fn is_zero(&self) -> bool {
        match self {
            Num::Exact(r) => r.is_zero(),
            Num::Approx(d) => d.is_zero(),
        }
    }

    pub fn to_dec(&self) -> Dec {
        match self {
            Num::Exact(r) => Dec::from_rational(r, PREC),
            Num::Approx(d) => d.clone(),
        }
    }

    /// The exact whole number this holds, when it is one. An approximation
    /// never is: nothing here proved it landed on an integer.
    pub fn to_i64_exact(&self) -> Option<i64> {
        match self {
            Num::Exact(r) => r.to_int()?.to_i64(),
            Num::Approx(_) => None,
        }
    }

    pub fn neg(&self) -> Num {
        match self {
            Num::Exact(r) => Num::Exact(r.neg()),
            Num::Approx(d) => Num::Approx(d.neg()),
        }
    }

    pub fn add(&self, other: &Num) -> Num {
        match (self, other) {
            (Num::Exact(a), Num::Exact(b)) => Num::Exact(a.add(b)),
            _ => Num::Approx(self.to_dec().add(&other.to_dec(), PREC)),
        }
    }

    pub fn sub(&self, other: &Num) -> Num {
        match (self, other) {
            (Num::Exact(a), Num::Exact(b)) => Num::Exact(a.sub(b)),
            _ => Num::Approx(self.to_dec().sub(&other.to_dec(), PREC)),
        }
    }

    pub fn mul(&self, other: &Num) -> Num {
        match (self, other) {
            (Num::Exact(a), Num::Exact(b)) => Num::Exact(a.mul(b)),
            _ => Num::Approx(self.to_dec().mul(&other.to_dec(), PREC)),
        }
    }

    pub fn div(&self, other: &Num) -> Result<Num, EvalError> {
        match (self, other) {
            (Num::Exact(a), Num::Exact(b)) => {
                a.div(b).map(Num::Exact).ok_or(EvalError::DivisionByZero)
            }
            _ => Ok(Num::Approx(self.to_dec().div(&other.to_dec(), PREC)?)),
        }
    }

    /// Postfix per cent: a division by a hundred, exact when the value is.
    pub fn percent(&self) -> Num {
        self.div(&Num::from_i64(100)).expect("a hundred is not zero")
    }

    pub fn sqrt(&self) -> Result<Num, EvalError> {
        if let Num::Exact(r) = self {
            if r.is_negative() {
                return Err(EvalError::NegativeRoot);
            }
            if let Some(root) = r.sqrt_exact() {
                return Ok(Num::Exact(root));
            }
        }
        let d = self.to_dec();
        if d.is_negative() {
            return Err(EvalError::NegativeRoot);
        }
        Ok(Num::Approx(d.sqrt(PREC)))
    }

    /// `self^other`, exact whenever the exponent is a whole number small enough
    /// that the exact answer is worth writing.
    pub fn pow(&self, other: &Num) -> Result<Num, EvalError> {
        if let (Num::Exact(base), Some(exp)) = (self, other.to_i64_exact()) {
            if base.is_zero() && exp <= 0 {
                return Err(EvalError::ZeroToNonPositivePower);
            }
            let width = base
                .numerator()
                .decimal_len()
                .max(base.denominator().decimal_len())
                .saturating_mul(exp.unsigned_abs() as usize);
            if width <= MAX_EXACT_POW_DIGITS {
                return base.pow_i64(exp).map(Num::Exact).ok_or(EvalError::Overflow);
            }
        }
        Ok(Num::Approx(self.to_dec().pow(&other.to_dec(), PREC)?))
    }

    pub fn ln(&self) -> Result<Num, EvalError> {
        Ok(Num::Approx(self.to_dec().ln(PREC)?))
    }

    pub fn log10(&self) -> Result<Num, EvalError> {
        Ok(Num::Approx(self.to_dec().log10(PREC)?))
    }

    pub fn sin(&self, angle: Angle) -> Result<Num, EvalError> {
        Ok(Num::Approx(self.radians(angle)?.sin_cos(PREC)?.0))
    }

    pub fn cos(&self, angle: Angle) -> Result<Num, EvalError> {
        Ok(Num::Approx(self.radians(angle)?.sin_cos(PREC)?.1))
    }

    pub fn tan(&self, angle: Angle) -> Result<Num, EvalError> {
        let (sin, cos) = self.radians(angle)?.sin_cos(PREC)?;
        Ok(Num::Approx(sin.div(&cos, PREC)?))
    }

    /// The angle in radians, converting at the call when the toggle says
    /// degrees. Nothing else in the engine knows degrees exist.
    fn radians(&self, angle: Angle) -> Result<Dec, EvalError> {
        let d = self.to_dec();
        match angle {
            Angle::Rad => Ok(d),
            Angle::Deg => d.mul(&Dec::pi(PREC), PREC).div(&Dec::from_i64(180), PREC),
        }
    }

    /// How the display writes this value.
    ///
    /// An exact value whose decimal terminates inside the displayed precision
    /// is written out in full and carries no marker. Anything else is rounded
    /// and marked, while the value itself stays exactly what it was.
    pub fn display(&self) -> String {
        match self {
            Num::Exact(r) => {
                if let Some((neg, digits, exp)) = r.exact_decimal() {
                    if digits.len() <= DISPLAY_DIGITS {
                        return render(neg, &digits, exp);
                    }
                }
                let (neg, digits, exp) = Dec::from_rational(r, PREC).to_digits(DISPLAY_DIGITS);
                format!("{APPROX}{}", render(neg, &digits, exp))
            }
            Num::Approx(d) => {
                let (neg, digits, exp) = d.to_digits(DISPLAY_DIGITS);
                format!("{APPROX}{}", render(neg, &digits, exp))
            }
        }
    }
}

/// `±digits · 10^exp` as plain decimal where that is readable, and as one
/// scientific form where it is not. No digit is ever dropped without the `e`
/// that says so.
fn render(neg: bool, digits: &str, exp: i64) -> String {
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if digits == "0" {
        out.push('0');
        return out;
    }
    // How many digits stand before the point.
    let point = digits.len() as i64 + exp;
    if point > DISPLAY_DIGITS as i64 + 1 || point < -5 {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        let e = point - 1;
        if e >= 0 {
            out.push('+');
        }
        out.push_str(&e.to_string());
        return out;
    }
    if point <= 0 {
        out.push_str("0.");
        for _ in 0..-point {
            out.push('0');
        }
        out.push_str(digits);
    } else if point as usize >= digits.len() {
        out.push_str(digits);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bigint::Int;

    fn exact(s: &str) -> Num {
        Num::Exact(Rational::parse_decimal(s).unwrap())
    }

    fn ratio(n: i64, d: i64) -> Num {
        Num::Exact(Rational::new(Int::from_i64(n), Int::from_i64(d)).unwrap())
    }

    #[test]
    fn exact_arithmetic_stays_exact() {
        let third = Num::from_i64(1).div(&Num::from_i64(3)).unwrap();
        assert_eq!(third.mul(&Num::from_i64(3)).display(), "1");
        assert_eq!(exact("0.1").add(&exact("0.2")).display(), "0.3");
        let seventh = Num::from_i64(1).div(&Num::from_i64(7)).unwrap();
        assert_eq!(seventh.mul(&Num::from_i64(7)).display(), "1");
        assert!(!third.is_approx());
    }

    #[test]
    fn a_non_terminating_exact_value_is_shown_rounded_and_marked() {
        let third = Num::from_i64(1).div(&Num::from_i64(3)).unwrap();
        assert_eq!(third.display(), "≈0.3333333333333333333333333333333333333333");
        assert_eq!(ratio(2, 3).display(), "≈0.6666666666666666666666666666666666666667");
        // Terminating but longer than the display: still marked, never cut.
        let tiny = Num::from_i64(2).pow(&Num::from_i64(-200)).unwrap();
        assert!(tiny.display().starts_with(APPROX));
        assert!(!tiny.is_approx(), "the value itself is still exact");
    }

    #[test]
    fn the_marker_propagates_once_it_is_earned() {
        let root = Num::from_i64(2).sqrt().unwrap();
        assert!(root.is_approx());
        assert!(root.add(&Num::from_i64(1)).is_approx());
        assert!(root.mul(&Num::from_i64(0)).is_approx());
        assert!(Num::from_i64(4).sqrt().unwrap().display() == "2");
        assert!(!Num::from_i64(4).sqrt().unwrap().is_approx());
    }

    #[test]
    fn percent_is_an_exact_hundredth() {
        assert_eq!(exact("50").percent().display(), "0.5");
        assert_eq!(Num::from_i64(1).percent().display(), "0.01");
        assert_eq!(Num::from_i64(1).percent().percent().display(), "0.0001");
    }

    #[test]
    fn powers_are_exact_while_that_is_worth_doing() {
        assert_eq!(Num::from_i64(2).pow(&Num::from_i64(10)).unwrap().display(), "1024");
        assert_eq!(ratio(2, 3).pow(&Num::from_i64(-2)).unwrap().display(), "2.25");
        assert!(!Num::from_i64(2).pow(&Num::from_i64(10)).unwrap().is_approx());
        // Past the exact bound the answer still arrives, marked.
        let huge = Num::from_i64(9).pow(&Num::from_i64(100000)).unwrap();
        assert!(huge.is_approx());
        assert_eq!(
            Num::from_i64(0).pow(&Num::from_i64(-1)),
            Err(EvalError::ZeroToNonPositivePower)
        );
    }

    #[test]
    fn degrees_are_converted_at_the_call() {
        let ninety = Num::from_i64(90).sin(Angle::Deg).unwrap();
        assert_eq!(ninety.display(), "≈1");
        let half = Num::from_i64(30).sin(Angle::Deg).unwrap();
        assert!(half.display().starts_with("≈0.5"));
        assert_eq!(Num::from_i64(0).cos(Angle::Deg).unwrap().display(), "≈1");
    }

    #[test]
    fn rendering_switches_to_a_scientific_form_rather_than_dropping_digits() {
        assert_eq!(Num::from_i64(1).div(&Num::from_i64(8)).unwrap().display(), "0.125");
        assert_eq!(exact("0.000001").display(), "0.000001");
        assert_eq!(exact("0.0000001").display(), "1e-7");
        assert_eq!(Num::from_i64(10).pow(&Num::from_i64(41)).unwrap().display(), "1e+41");
        assert_eq!(Num::from_i64(-5).div(&Num::from_i64(2)).unwrap().display(), "-2.5");
        assert_eq!(Num::from_i64(0).display(), "0");
    }

    #[test]
    fn division_by_zero_is_refused_on_both_paths() {
        assert_eq!(Num::from_i64(1).div(&Num::from_i64(0)), Err(EvalError::DivisionByZero));
        let approx_zero = Num::from_i64(0).sin(Angle::Rad).unwrap();
        assert_eq!(Num::from_i64(1).div(&approx_zero), Err(EvalError::DivisionByZero));
        assert_eq!(Num::from_i64(-1).sqrt(), Err(EvalError::NegativeRoot));
        assert_eq!(Num::from_i64(0).ln(), Err(EvalError::LogOfNonPositive));
    }
}
