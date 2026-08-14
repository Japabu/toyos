//! The Prog layout's arithmetic: 64-bit two's-complement integers and nothing
//! else.
//!
//! `+ − × ÷` wrap the way the hardware does. Division is the exception it has
//! to be — there is no fractional value to wrap into, so a division that does
//! not come out whole is refused by name rather than quietly truncated.

use crate::error::EvalError;

/// Which base a typed digit is read in, and which pane the entry mirrors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Base {
    Hex,
    Dec,
    Bin,
}

impl Base {
    pub fn radix(self) -> u32 {
        match self {
            Base::Hex => 16,
            Base::Dec => 10,
            Base::Bin => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Base::Hex => "HEX",
            Base::Dec => "DEC",
            Base::Bin => "BIN",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Tok {
    Number(i64),
    And,
    Or,
    Xor,
    Not,
    Shl,
    Shr,
    Plus,
    Minus,
    Star,
    Slash,
    Open,
    Close,
}

fn lex(text: &str, base: Base) -> Result<Vec<Tok>, EvalError> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_alphanumeric() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_alphanumeric() {
                i += 1;
            }
            let literal: String = chars[start..i].iter().collect();
            out.push(Tok::Number(parse_literal(&literal, base)?));
            continue;
        }
        let (tok, width) = match (c, chars.get(i + 1)) {
            ('<', Some('<')) => (Tok::Shl, 2),
            ('>', Some('>')) => (Tok::Shr, 2),
            ('&', _) => (Tok::And, 1),
            ('|', _) => (Tok::Or, 1),
            ('^', _) => (Tok::Xor, 1),
            ('~', _) => (Tok::Not, 1),
            ('+', _) => (Tok::Plus, 1),
            ('-', _) | ('−', _) => (Tok::Minus, 1),
            ('*', _) | ('×', _) => (Tok::Star, 1),
            ('/', _) | ('÷', _) => (Tok::Slash, 1),
            ('(', _) => (Tok::Open, 1),
            (')', _) => (Tok::Close, 1),
            (other, _) => return Err(EvalError::Parse(format!("{other} means nothing here"))),
        };
        out.push(tok);
        i += width;
    }
    Ok(out)
}

/// A literal in the active base, as the 64 bits it names. Every pattern a
/// `u64` can hold is a value; anything wider is refused.
fn parse_literal(literal: &str, base: Base) -> Result<i64, EvalError> {
    let radix = base.radix();
    let mut acc: u64 = 0;
    for ch in literal.chars() {
        let d = ch
            .to_digit(radix)
            .ok_or_else(|| EvalError::Parse(format!("{ch} is not a {} digit", base.label())))?;
        acc = acc
            .checked_mul(radix as u64)
            .and_then(|v| v.checked_add(d as u64))
            .ok_or(EvalError::OutOfRange)?;
    }
    Ok(acc as i64)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    depth: u32,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn enter(&mut self) -> Result<(), EvalError> {
        self.depth += 1;
        if self.depth > crate::parser::MAX_DEPTH {
            Err(EvalError::TooDeep)
        } else {
            Ok(())
        }
    }

    fn expr(&mut self) -> Result<i64, EvalError> {
        self.enter()?;
        let out = self.or();
        self.depth -= 1;
        out
    }

    fn or(&mut self) -> Result<i64, EvalError> {
        let mut left = self.xor()?;
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            left |= self.xor()?;
        }
        Ok(left)
    }

    fn xor(&mut self) -> Result<i64, EvalError> {
        let mut left = self.and()?;
        while self.peek() == Some(&Tok::Xor) {
            self.pos += 1;
            left ^= self.and()?;
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<i64, EvalError> {
        let mut left = self.shift()?;
        while self.peek() == Some(&Tok::And) {
            self.pos += 1;
            left &= self.shift()?;
        }
        Ok(left)
    }

    fn shift(&mut self) -> Result<i64, EvalError> {
        let mut left = self.additive()?;
        loop {
            let left_shift = match self.peek() {
                Some(Tok::Shl) => true,
                Some(Tok::Shr) => false,
                _ => return Ok(left),
            };
            self.pos += 1;
            let count = self.additive()?;
            left = shift(left, count, left_shift)?;
        }
    }

    fn additive(&mut self) -> Result<i64, EvalError> {
        let mut left = self.multiplicative()?;
        loop {
            match self.peek() {
                Some(Tok::Plus) => {
                    self.pos += 1;
                    left = left.wrapping_add(self.multiplicative()?);
                }
                Some(Tok::Minus) => {
                    self.pos += 1;
                    left = left.wrapping_sub(self.multiplicative()?);
                }
                _ => return Ok(left),
            }
        }
    }

    fn multiplicative(&mut self) -> Result<i64, EvalError> {
        let mut left = self.unary()?;
        loop {
            match self.peek() {
                Some(Tok::Star) => {
                    self.pos += 1;
                    left = left.wrapping_mul(self.unary()?);
                }
                Some(Tok::Slash) => {
                    self.pos += 1;
                    let right = self.unary()?;
                    if right == 0 {
                        return Err(EvalError::DivisionByZero);
                    }
                    if left.wrapping_rem(right) != 0 {
                        return Err(EvalError::NotAnInteger);
                    }
                    left = left.wrapping_div(right);
                }
                _ => return Ok(left),
            }
        }
    }

    fn unary(&mut self) -> Result<i64, EvalError> {
        self.enter()?;
        let out = match self.peek() {
            Some(Tok::Minus) => {
                self.pos += 1;
                self.unary().map(|v| v.wrapping_neg())
            }
            Some(Tok::Not) => {
                self.pos += 1;
                self.unary().map(|v| !v)
            }
            Some(Tok::Plus) => {
                self.pos += 1;
                self.unary()
            }
            _ => self.primary(),
        };
        self.depth -= 1;
        out
    }

    fn primary(&mut self) -> Result<i64, EvalError> {
        match self.peek().cloned() {
            Some(Tok::Number(v)) => {
                self.pos += 1;
                Ok(v)
            }
            Some(Tok::Open) => {
                self.pos += 1;
                let inner = self.expr()?;
                if self.peek() != Some(&Tok::Close) {
                    return Err(EvalError::Parse("a ( was never closed".into()));
                }
                self.pos += 1;
                Ok(inner)
            }
            Some(Tok::Close) => Err(EvalError::Parse("a ) closes nothing".into())),
            Some(_) => Err(EvalError::Parse("an operator needs a value before it".into())),
            None => Err(EvalError::Parse("the expression stops early".into())),
        }
    }
}

/// A shift past the width of the word is not an error — it is what the word
/// leaves behind. `>>` keeps the sign, which is what a two's-complement value
/// carries in its top bit.
fn shift(value: i64, count: i64, left: bool) -> Result<i64, EvalError> {
    if count < 0 {
        return Err(EvalError::NegativeShift);
    }
    if count >= 64 {
        return Ok(if left {
            0
        } else if value < 0 {
            -1
        } else {
            0
        });
    }
    Ok(if left { value.wrapping_shl(count as u32) } else { value >> count })
}

/// Evaluate one programmer-mode expression, reading bare digits in `base`.
pub fn eval(text: &str, base: Base) -> Result<i64, EvalError> {
    if text.len() > crate::parser::MAX_LEN {
        return Err(EvalError::TooLong);
    }
    let toks = lex(text, base)?;
    if toks.is_empty() {
        return Err(EvalError::Parse("nothing to work out".into()));
    }
    let mut parser = Parser { toks, pos: 0, depth: 0 };
    let value = parser.expr()?;
    match parser.peek() {
        None => Ok(value),
        Some(_) => Err(EvalError::Parse("that has nothing to do here".into())),
    }
}

/// The value written in `base`, with no separators — what an entry line holds.
pub fn write_in_base(value: i64, base: Base) -> String {
    let bits = value as u64;
    match base {
        Base::Hex => format!("{bits:X}"),
        Base::Dec => value.to_string(),
        Base::Bin => format!("{bits:b}"),
    }
}

/// The hexadecimal pane: sixteen digits in groups of four.
pub fn pane_hex(value: i64) -> String {
    let text = format!("{:016X}", value as u64);
    group(&text, 4)
}

/// The decimal pane, signed — the top bit is a sign, not a digit.
pub fn pane_dec(value: i64) -> String {
    value.to_string()
}

/// The binary pane, as the two halves the display puts on two rows.
pub fn pane_bin(value: i64) -> (String, String) {
    let text = format!("{:064b}", value as u64);
    (group(&text[..32], 8), group(&text[32..], 8))
}

fn group(text: &str, size: usize) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / size);
    for (i, ch) in text.chars().enumerate() {
        if i > 0 && i % size == 0 {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Result<i64, EvalError> {
        eval(text, Base::Hex)
    }

    fn dec(text: &str) -> Result<i64, EvalError> {
        eval(text, Base::Dec)
    }

    #[test]
    fn digits_are_read_in_the_active_base() {
        assert_eq!(hex("FF"), Ok(255));
        assert_eq!(hex("ff"), Ok(255));
        assert_eq!(dec("255"), Ok(255));
        assert_eq!(eval("11111111", Base::Bin), Ok(255));
        assert!(dec("FF").is_err());
        assert!(eval("2", Base::Bin).is_err());
        assert_eq!(hex("FFFFFFFFFFFFFFFF"), Ok(-1));
        assert_eq!(dec("18446744073709551615"), Ok(-1));
        assert_eq!(dec("18446744073709551616"), Err(EvalError::OutOfRange));
    }

    #[test]
    fn the_logic_operators_have_c_precedence() {
        assert_eq!(dec("1|2&3"), Ok(3));
        assert_eq!(dec("(1|2)&3"), Ok(3));
        assert_eq!(dec("6^3"), Ok(5));
        assert_eq!(dec("1|2^3"), Ok(1));
        assert_eq!(dec("~0"), Ok(-1));
        assert_eq!(dec("~5"), Ok(-6));
        assert_eq!(dec("1+2&3"), Ok(3));
        assert_eq!(dec("2*3&4"), Ok(4));
    }

    #[test]
    fn arithmetic_wraps_at_the_word() {
        assert_eq!(dec("9223372036854775807+1"), Ok(i64::MIN));
        assert_eq!(dec("-9223372036854775807-2"), Ok(i64::MAX));
        assert_eq!(dec("4611686018427387904*2"), Ok(i64::MIN));
        assert_eq!(dec("-9223372036854775808/-1"), Ok(i64::MIN));
        assert_eq!(dec("-9223372036854775808"), Ok(i64::MIN));
    }

    #[test]
    fn shifts_saturate_rather_than_wrap_the_count() {
        assert_eq!(dec("1<<63"), Ok(i64::MIN));
        assert_eq!(dec("1<<64"), Ok(0));
        assert_eq!(dec("1<<1000"), Ok(0));
        assert_eq!(dec("-1>>1"), Ok(-1));
        assert_eq!(dec("-1>>64"), Ok(-1));
        assert_eq!(dec("256>>4"), Ok(16));
        assert_eq!(dec("256>>64"), Ok(0));
        assert_eq!(dec("1>>-1"), Err(EvalError::NegativeShift));
    }

    #[test]
    fn a_fractional_division_is_refused_rather_than_rounded() {
        assert_eq!(dec("7/2"), Err(EvalError::NotAnInteger));
        assert_eq!(dec("6/2"), Ok(3));
        assert_eq!(dec("-6/4"), Err(EvalError::NotAnInteger));
        assert_eq!(dec("1/0"), Err(EvalError::DivisionByZero));
    }

    #[test]
    fn the_panes_show_the_same_bits_three_ways() {
        assert_eq!(pane_hex(-1), "FFFF FFFF FFFF FFFF");
        assert_eq!(pane_dec(-1), "-1");
        assert_eq!(pane_bin(-1).0, "11111111 11111111 11111111 11111111");
        assert_eq!(pane_bin(-1).1, "11111111 11111111 11111111 11111111");
        assert_eq!(pane_hex(255), "0000 0000 0000 00FF");
        assert_eq!(pane_bin(1).1, "00000000 00000000 00000000 00000001");
        assert_eq!(write_in_base(-1, Base::Hex), "FFFFFFFFFFFFFFFF");
        assert_eq!(write_in_base(-1, Base::Dec), "-1");
        assert_eq!(write_in_base(5, Base::Bin), "101");
    }

    #[test]
    fn nothing_typeable_panics_here_either() {
        let alphabet: Vec<char> = "0123456789ABCDEFabcdef+-*/&|^~<>() xz".chars().collect();
        let mut seed: u64 = 0x9E3779B97F4A7C15;
        for _ in 0..4000 {
            let mut text = String::new();
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let len = (seed >> 33) as usize % 14;
            for _ in 0..len {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                text.push(alphabet[(seed >> 33) as usize % alphabet.len()]);
            }
            for base in [Base::Hex, Base::Dec, Base::Bin] {
                let _ = eval(&text, base);
            }
        }
        assert_eq!(eval(&"(".repeat(200), Base::Dec), Err(EvalError::TooDeep));
        assert_eq!(eval(&"1".repeat(300), Base::Dec), Err(EvalError::TooLong));
    }
}
