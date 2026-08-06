//! Host harness for `kernel/src/mm/user_span.rs`.
//!
//! The kernel file is compiled into this crate and brings its own tests, so
//! what runs here is the arithmetic the kernel runs and not a description of
//! it. Nothing is supplied to it: the file names nothing outside itself, which
//! is the property that lets it be tested at all.

#[path = "../../kernel/src/mm/user_span.rs"]
pub mod user_span;
