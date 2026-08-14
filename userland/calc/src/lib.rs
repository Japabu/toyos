//! The calculator, without a window.
//!
//! The layers stack: [`bigint`] under [`rational`] and [`dec`], those two under
//! [`num`], and [`parser`] and [`prog`] over that. [`app`] is the calculator as
//! a state machine — every button and every key ends up there — and the binary
//! beside this file is the only part that knows what a pixel is.

pub mod app;
pub mod bigint;
pub mod dec;
pub mod error;
pub mod num;
pub mod parser;
pub mod prog;
pub mod rational;
