//! The terminal emulator — ANSI state machine, scrollback, selection — over a
//! [`window::Screen`].
//!
//! A library because two programs draw a shell on a framebuffer and only one of
//! them has a compositor behind it: `/bin/terminal` gets its screen from a
//! window, `/bin/console` claims the panel itself. `Console::new` does not know
//! the difference, which is why the second caller cost nothing — and why the
//! emulator composes as though it were always the second one, since only that
//! caller's mapping can make a read expensive.

pub mod console;

pub use console::Console;
