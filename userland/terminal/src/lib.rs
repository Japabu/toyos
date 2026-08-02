//! The terminal emulator — ANSI state machine, scrollback, selection — over a
//! raw [`window::Framebuffer`].
//!
//! A library because two programs draw a shell on a framebuffer and only one of
//! them has a compositor behind it: `/bin/terminal` gets its framebuffer from a
//! window, `/bin/console` claims the screen itself. `Console::new` never knew
//! the difference, so nothing had to change for the second caller to exist.

pub mod console;

pub use console::Console;
