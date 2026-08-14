//! The identities, asserted through the parser rather than under it.
//!
//! **Why this file exists.** `dec.rs` already checks that sin²+cos² is one and
//! that exp(ln x) is x — but it checks them by *calling* `Dec::sin_cos`, which
//! is a claim about the arithmetic and says nothing about the language. The
//! owner typed `cos(2)^2+sin(2)^2` and got `-1.4104…`, because the parser bound
//! the `^` inside the call and asked for `cos 4 + sin 4`. Every arithmetic test
//! in the tree passed while it did.
//!
//! So these go through `eval` on a string, which is the only surface a person
//! touches. An identity is a property of the whole program or it is not pinned.

use calc::error::EvalError;
use calc::num::{Angle, Num};
use calc::parser::eval;

fn value(text: &str) -> Num {
    eval(text, Angle::Rad).unwrap_or_else(|e| panic!("{text} was refused: {}", e.message()))
}

fn shown(text: &str) -> String {
    value(text).display()
}

fn refusal(text: &str) -> EvalError {
    eval(text, Angle::Rad).expect_err(&format!("{text} was answered rather than refused"))
}

/// The report, exactly as typed. Approximate — the values are — but one to
/// every digit the display carries.
#[test]
fn the_pythagorean_identity_holds_through_the_parser() {
    for x in ["0", "1", "2", "-2", "0.5", "7", "100", "(1/3)"] {
        let text = format!("cos({x})^2+sin({x})^2");
        assert_eq!(shown(&text), "≈1", "{text}");
    }
}

/// A call is a finished value, so a power after it squares the answer. Asserted
/// against the product of the call with itself, evaluated the same way, so the
/// two sides share no code path but the one under test.
#[test]
fn a_power_after_a_call_squares_what_the_call_returned() {
    for text in ["cos(2)", "sin(2)", "ln(3)", "sqrt(2)", "log(7)", "tan(1)"] {
        let squared = value(&format!("{text}^2"));
        let multiplied = value(&format!("{text}*{text}"));
        assert_eq!(squared, multiplied, "{text}^2 is not {text}*{text}");
    }
    // The bracket is not what does it.
    assert_eq!(value("sqrt 4^2"), value("4"));
    assert_eq!(value("ln 2^2"), value("ln(2)*ln(2)"));
}

/// The number the owner should have seen, to the precision the display shows.
#[test]
fn the_reported_expressions_have_the_values_they_should() {
    assert!(
        shown("cos(2)^2").starts_with("≈0.1731781895681940"),
        "cos(2)^2 shows {}",
        shown("cos(2)^2")
    );
    assert!(
        shown("sin(2)^2").starts_with("≈0.8268218104318059"),
        "sin(2)^2 shows {}",
        shown("sin(2)^2")
    );
    assert_eq!(shown("cos(2)^2+sin(2)^2"), "≈1");
    // What it used to say, so a regression is loud rather than subtle.
    assert_ne!(shown("cos(2)^2"), shown("cos(4)"));
    assert_ne!(shown("cos(2)^2+sin(2)^2"), shown("cos(4)+sin(4)"));
}

/// The owner's literal typo is a domain error now, not a number.
///
/// `(cos 2)` is negative and `(sin 2)²` is not a whole number, and a negative
/// base under a fractional power is refused by name.
#[test]
fn the_typo_is_refused_by_name() {
    assert_eq!(refusal("cos(2)^+sin(2)^2"), EvalError::NegativeBaseFractionalExponent);
    assert_eq!(refusal("cos(2)^sin(2)^2"), EvalError::NegativeBaseFractionalExponent);
    assert_eq!(
        refusal("cos(2)^+sin(2)^2").message(),
        "a negative base needs a whole exponent"
    );
}

/// Nothing else in the precedence table moved.
#[test]
fn the_rest_of_the_table_is_where_it_was() {
    let table: &[(&str, &str)] = &[
        ("-2^2", "-4"),
        ("(-2)^2", "4"),
        ("2^3^2", "512"),
        ("2^-2", "0.25"),
        ("2+3*4", "14"),
        ("1÷3×3", "1"),
        ("0.1+0.2", "0.3"),
        ("3(1+2)", "9"),
        ("(1+2)(3+4)", "21"),
        ("50%", "0.5"),
        ("(2^2)%", "0.04"),
        ("sqrt(4)", "2"),
        ("√9", "3"),
    ];
    for (input, want) in table {
        assert_eq!(&shown(input), want, "{input}");
    }
    assert_eq!(refusal("√-1"), EvalError::NegativeRoot);
    assert_eq!(refusal("1/0"), EvalError::DivisionByZero);
}

/// exp(ln x) through the parser, for the same reason as the first test: the
/// arithmetic already had this and the language did not.
#[test]
fn a_logarithm_round_trips_through_the_parser() {
    for x in ["2", "10", "0.001", "1000"] {
        assert_eq!(shown(&format!("e^ln({x})")), format!("≈{x}"), "e^ln({x})");
    }
}
