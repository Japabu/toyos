//! Host harness for `kernel/src/arch/idt/fault_class.rs`.
//!
//! The kernel file is compiled into this crate and brings its own tests, so
//! what runs here is the classification the kernel runs and not a description
//! of it. It names one thing outside itself — the user/kernel bound — and that
//! file is compiled in beside it under `crate::mm::user_span`, the path the
//! kernel gives it, rather than supplied as a constant of this crate's own.
//!
//! `#[path = "."]` on the inline module is what keeps that arrangement to two
//! lines: a `#[path]` inside an inline module resolves under a directory named
//! for the module, and `kernel-fault/src/mm/` is a directory this crate has no
//! other reason to have.

#[path = "."]
pub mod mm {
    #[path = "../../kernel/src/mm/user_span.rs"]
    pub mod user_span;
}

#[path = "../../kernel/src/arch/idt/fault_class.rs"]
pub mod fault_class;
