//! The expression language the Calc layout reads, lexed and evaluated in one
//! pass.
//!
//! Precedence climbing over the usual table. `^` is right-associative and binds
//! tighter than unary minus, so `-2^2` is `-4`. Two values written next to each
//! other multiply, which is what makes `2π`, `3(1+2)` and `(1+2)(3+4)` mean
//! what they look like.
//!
//! Nothing here can panic on any string: the depth bound below is what stops a
//! wall of open parentheses from ending the program instead of being refused.

use crate::dec::{Dec, PREC};
use crate::error::EvalError;
use crate::num::{Angle, Num};
use crate::rational::Rational;

/// How deeply the grammar may nest before an expression is refused.
pub const MAX_DEPTH: u32 = 64;

/// The longest expression the field accepts.
pub const MAX_LEN: usize = 256;

#[derive(Clone, PartialEq, Debug)]
enum Tok {
    Number(Rational),
    /// A function or constant name, lowercased.
    Name(String),
    Plus,
    Minus,
    Star,
    Slash,
    Caret,
    Percent,
    Open,
    Close,
}

impl Tok {
    /// Whether this token can begin a value, which is what makes the token
    /// before it a multiplication.
    fn starts_value(&self) -> bool {
        matches!(self, Tok::Number(_) | Tok::Name(_) | Tok::Open)
    }
}

fn lex(text: &str) -> Result<Vec<Tok>, EvalError> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || c == '.' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let literal: String = chars[start..i].iter().collect();
            let value = Rational::parse_decimal(&literal)
                .ok_or_else(|| EvalError::Parse(format!("{literal} is not a number")))?;
            out.push(Tok::Number(value));
            continue;
        }
        if c.is_ascii_alphabetic() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_alphabetic() {
                i += 1;
            }
            let name: String = chars[start..i].iter().collect();
            out.push(Tok::Name(name.to_ascii_lowercase()));
            continue;
        }
        let tok = match c {
            '+' => Tok::Plus,
            '-' | '−' => Tok::Minus,
            '*' | '×' => Tok::Star,
            '/' | '÷' => Tok::Slash,
            '^' => Tok::Caret,
            '%' => Tok::Percent,
            '(' => Tok::Open,
            ')' => Tok::Close,
            'π' => Tok::Name("pi".to_string()),
            '√' => Tok::Name("sqrt".to_string()),
            other => return Err(EvalError::Parse(format!("{other} means nothing here"))),
        };
        out.push(tok);
        i += 1;
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    angle: Angle,
    depth: u32,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn enter(&mut self) -> Result<(), EvalError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            Err(EvalError::TooDeep)
        } else {
            Ok(())
        }
    }

    fn expr(&mut self) -> Result<Num, EvalError> {
        self.enter()?;
        let out = self.additive();
        self.depth -= 1;
        out
    }

    fn additive(&mut self) -> Result<Num, EvalError> {
        let mut left = self.multiplicative()?;
        loop {
            match self.peek() {
                Some(Tok::Plus) => {
                    self.pos += 1;
                    left = left.add(&self.multiplicative()?);
                }
                Some(Tok::Minus) => {
                    self.pos += 1;
                    left = left.sub(&self.multiplicative()?);
                }
                _ => return Ok(left),
            }
        }
    }

    fn multiplicative(&mut self) -> Result<Num, EvalError> {
        let mut left = self.unary()?;
        loop {
            match self.peek() {
                Some(Tok::Star) => {
                    self.pos += 1;
                    left = left.mul(&self.unary()?);
                }
                Some(Tok::Slash) => {
                    self.pos += 1;
                    left = left.div(&self.unary()?)?;
                }
                // Two values side by side multiply.
                Some(t) if t.starts_value() => left = left.mul(&self.unary()?),
                _ => return Ok(left),
            }
        }
    }

    fn unary(&mut self) -> Result<Num, EvalError> {
        self.enter()?;
        let out = match self.peek() {
            Some(Tok::Minus) => {
                self.pos += 1;
                self.unary().map(|v| v.neg())
            }
            Some(Tok::Plus) => {
                self.pos += 1;
                self.unary()
            }
            _ => self.power(),
        };
        self.depth -= 1;
        out
    }

    fn power(&mut self) -> Result<Num, EvalError> {
        let base = self.postfix()?;
        if self.peek() == Some(&Tok::Caret) {
            self.pos += 1;
            // The right-hand side is a full unary, so `2^-3` reads and the
            // operator stays right-associative.
            return base.pow(&self.unary()?);
        }
        Ok(base)
    }

    fn postfix(&mut self) -> Result<Num, EvalError> {
        let mut value = self.primary()?;
        while self.peek() == Some(&Tok::Percent) {
            self.pos += 1;
            value = value.percent();
        }
        Ok(value)
    }

    fn primary(&mut self) -> Result<Num, EvalError> {
        self.enter()?;
        let out = self.primary_inner();
        self.depth -= 1;
        out
    }

    fn primary_inner(&mut self) -> Result<Num, EvalError> {
        let tok = match self.peek() {
            Some(t) => t.clone(),
            None => return Err(EvalError::Parse("the expression stops early".into())),
        };
        match tok {
            Tok::Number(r) => {
                self.pos += 1;
                Ok(Num::Exact(r))
            }
            Tok::Open => {
                self.pos += 1;
                let inner = self.expr()?;
                if self.peek() != Some(&Tok::Close) {
                    return Err(EvalError::Parse("a ( was never closed".into()));
                }
                self.pos += 1;
                Ok(inner)
            }
            Tok::Name(name) => {
                self.pos += 1;
                match name.as_str() {
                    "pi" => Ok(Num::Approx(Dec::pi(PREC))),
                    "e" => Ok(Num::Approx(Dec::e(PREC))),
                    _ => {
                        // A function binds tighter than an implicit product, so
                        // `sin 2π` is `sin(2)·π` and `sin(2π)` is what it says.
                        // Its argument is a full unary, so `√-1` reaches the
                        // root rather than the parser.
                        let arg = self.unary()?;
                        apply(&name, &arg, self.angle)
                    }
                }
            }
            Tok::Close => Err(EvalError::Parse("a ) closes nothing".into())),
            other => Err(EvalError::Parse(format!("{} needs a value before it", spell(&other)))),
        }
    }
}

fn spell(tok: &Tok) -> &'static str {
    match tok {
        Tok::Plus => "+",
        Tok::Minus => "−",
        Tok::Star => "×",
        Tok::Slash => "÷",
        Tok::Caret => "^",
        Tok::Percent => "%",
        Tok::Open => "(",
        Tok::Close => ")",
        Tok::Number(_) => "a number",
        Tok::Name(_) => "a name",
    }
}

fn apply(name: &str, arg: &Num, angle: Angle) -> Result<Num, EvalError> {
    match name {
        "sin" => arg.sin(angle),
        "cos" => arg.cos(angle),
        "tan" => arg.tan(angle),
        "ln" => arg.ln(),
        "log" => arg.log10(),
        "sqrt" => arg.sqrt(),
        other => Err(EvalError::Parse(format!("{other} is not a function"))),
    }
}

/// Evaluate one expression. Every failure is a named refusal.
pub fn eval(text: &str, angle: Angle) -> Result<Num, EvalError> {
    if text.len() > MAX_LEN {
        return Err(EvalError::TooLong);
    }
    let toks = lex(text)?;
    if toks.is_empty() {
        return Err(EvalError::Parse("nothing to work out".into()));
    }
    let mut parser = Parser { toks, pos: 0, angle, depth: 0 };
    let value = parser.expr()?;
    match parser.peek() {
        None => Ok(value),
        Some(t) => Err(EvalError::Parse(format!("{} has nothing to do here", spell(t)))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(text: &str) -> String {
        eval(text, Angle::Rad).map(|v| v.display()).unwrap_or_else(|e| format!("!{}", e.message()))
    }

    #[test]
    fn the_four_operations_stay_exact() {
        assert_eq!(show("1÷3×3"), "1");
        assert_eq!(show("1/3*3"), "1");
        assert_eq!(show("0.1+0.2"), "0.3");
        assert_eq!(show("(1/7)*7"), "1");
        assert_eq!(show("1-2"), "-1");
        assert_eq!(show("2−3×4"), "-10");
    }

    #[test]
    fn precedence_and_associativity() {
        let table: &[(&str, &str)] = &[
            ("2+3*4", "14"),
            ("(2+3)*4", "20"),
            ("2^3^2", "512"),
            ("-2^2", "-4"),
            ("(-2)^2", "4"),
            ("2^-2", "0.25"),
            ("-2^-2", "-0.25"),
            ("8/4/2", "1"),
            ("8-4-2", "2"),
            ("2*3^2", "18"),
            ("50%", "0.5"),
            ("100+10%", "100.1"),
            // `%` is postfix on the value it follows, so it lands inside an
            // exponent rather than around one.
            ("(2^2)%", "0.04"),
            ("2%^2", "0.0004"),
            ("--5", "5"),
            ("+-5", "-5"),
        ];
        for (input, want) in table {
            assert_eq!(&show(input), want, "{input}");
        }
    }

    #[test]
    fn values_side_by_side_multiply() {
        assert_eq!(show("3(1+2)"), "9");
        assert_eq!(show("(1+2)(3+4)"), "21");
        assert_eq!(show("2(3)(4)"), "24");
        assert_eq!(show("2 3"), "6");
        assert!(show("2π").starts_with("≈6.28318530717958647692"));
        assert!(show("2e").starts_with("≈5.43656365691809047072"));
    }

    #[test]
    fn functions_are_typeable_by_name() {
        assert!(show("sqrt(2)").starts_with("≈1.41421356237309504880"));
        assert_eq!(show("sqrt(4)"), "2");
        assert_eq!(show("√9"), "3");
        assert!(show("ln(2)").starts_with("≈0.69314718055994530941"));
        assert_eq!(show("log(1000)"), "≈3");
        assert!(show("sin(1)").starts_with("≈0.84147098480789650665"));
        assert!(show("cos(1)").starts_with("≈0.54030230586813971740"));
        assert!(show("tan(1)").starts_with("≈1.55740772465490223050"));
        assert_eq!(show("sin(0)"), "≈0");
    }

    #[test]
    fn degrees_are_a_toggle_and_nothing_else() {
        assert_eq!(eval("sin(90)", Angle::Deg).unwrap().display(), "≈1");
        assert_eq!(eval("cos(180)", Angle::Deg).unwrap().display(), "≈-1");
        assert!(eval("sin(90)", Angle::Rad).unwrap().display().starts_with("≈0.89"));
    }

    #[test]
    fn every_refusal_is_named_and_none_is_a_crash() {
        assert_eq!(show("1/0"), "!division by zero");
        assert_eq!(show("ln(-1)"), "!logarithm of a non-positive number");
        assert_eq!(show("ln(0)"), "!logarithm of a non-positive number");
        assert_eq!(show("√-1"), "!square root of a negative number");
        assert_eq!(show("(-8)^0.5"), "!a negative base needs a whole exponent");
        assert_eq!(show("0^-1"), "!zero to a non-positive power");
        assert_eq!(show("2+"), "!the expression stops early");
        assert_eq!(show("(1+2"), "!a ( was never closed");
        assert_eq!(show("1)"), "!) has nothing to do here");
        assert_eq!(show("*3"), "!× needs a value before it");
        assert_eq!(show("1.2.3"), "!1.2.3 is not a number");
        assert_eq!(show("foo(1)"), "!foo is not a function");
        assert_eq!(show("1#2"), "!# means nothing here");
        assert_eq!(show(""), "!nothing to work out");
        assert_eq!(show(&"(".repeat(200)), "!nested too deeply");
        assert_eq!(show(&"1".repeat(300)), "!expression too long");
    }

    /// Nothing typeable may end the program. The alphabet below is every
    /// character a button inserts plus the ones a keyboard can add.
    #[test]
    fn no_string_over_the_alphabet_panics() {
        let alphabet: Vec<char> =
            "0123456789.+-*/^%()πe√ sinclogtaqrbz#×÷−".chars().collect();
        let mut seed: u64 = 0x2545F4914F6CDD1D;
        for _ in 0..4000 {
            let mut text = String::new();
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let len = (seed >> 33) as usize % 14;
            for _ in 0..len {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                text.push(alphabet[(seed >> 33) as usize % alphabet.len()]);
            }
            let _ = eval(&text, Angle::Rad);
            let _ = eval(&text, Angle::Deg);
        }
    }
}
