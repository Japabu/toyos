//! Host harness for `kernel/src/log/elide.rs`.
//!
//! The kernel file is compiled into this crate and brings its own tests, so
//! what runs here is the rendering the kernel runs and not a description of it.
//! Nothing is supplied to it: the file names nothing outside itself, which is
//! the property that lets it be tested at all.

#[path = "../../kernel/src/log/elide.rs"]
pub mod elide;
