//! Every way an evaluation can refuse, and the sentence the display shows for
//! it.
//!
//! A refusal is a named value rather than a panic: the expression line is
//! whatever anyone typed, and no string may end the program.

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EvalError {
    /// What the parser could not make sense of, in its own words.
    Parse(String),
    DivisionByZero,
    NegativeRoot,
    LogOfNonPositive,
    ZeroToNonPositivePower,
    NegativeBaseFractionalExponent,
    /// A result too large to hold, or an exponent too large to raise to.
    Overflow,
    /// A trigonometric argument past where the stored π can reduce it.
    ArgumentTooLarge,
    /// Programmer mode divided and the answer was not a whole number.
    NotAnInteger,
    /// A literal, or a value crossing into programmer mode, outside 64 bits.
    OutOfRange,
    NegativeShift,
    /// Parentheses nested past the parser's bound.
    TooDeep,
    /// An expression longer than the field accepts.
    TooLong,
}

impl EvalError {
    pub fn message(&self) -> String {
        match self {
            EvalError::Parse(what) => what.clone(),
            EvalError::DivisionByZero => "division by zero".into(),
            EvalError::NegativeRoot => "square root of a negative number".into(),
            EvalError::LogOfNonPositive => "logarithm of a non-positive number".into(),
            EvalError::ZeroToNonPositivePower => "zero to a non-positive power".into(),
            EvalError::NegativeBaseFractionalExponent => {
                "a negative base needs a whole exponent".into()
            }
            EvalError::Overflow => "result too large".into(),
            EvalError::ArgumentTooLarge => "angle too large to reduce".into(),
            EvalError::NotAnInteger => "not a whole number".into(),
            EvalError::OutOfRange => "outside 64 bits".into(),
            EvalError::NegativeShift => "shift count is negative".into(),
            EvalError::TooDeep => "nested too deeply".into(),
            EvalError::TooLong => "expression too long".into(),
        }
    }
}
