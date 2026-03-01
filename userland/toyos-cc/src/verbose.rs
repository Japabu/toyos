use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub static VERBOSE: AtomicBool = AtomicBool::new(false);
pub static DEPTH: AtomicUsize = AtomicUsize::new(0);

pub fn enabled() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

pub fn set(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed);
}

pub fn depth() -> usize {
    DEPTH.load(Ordering::Relaxed)
}

pub fn enter() -> usize {
    DEPTH.fetch_add(1, Ordering::Relaxed)
}

pub fn leave() {
    DEPTH.fetch_sub(1, Ordering::Relaxed);
}

pub fn reset_depth() {
    DEPTH.store(0, Ordering::Relaxed);
}

/// Print indented verbose message. Indentation matches current depth.
#[macro_export]
macro_rules! verbose {
    ($($arg:tt)*) => {
        if $crate::verbose::enabled() {
            let d = $crate::verbose::depth();
            eprint!("{:indent$}", "", indent = d * 2);
            eprintln!($($arg)*);
        }
    };
}

/// Enter a tracked scope. Returns the depth before entering.
/// Aborts if depth exceeds limit (catches infinite recursion).
#[macro_export]
macro_rules! verbose_enter {
    ($name:expr) => {{
        let d = $crate::verbose::enter();
        if d > 500 {
            eprintln!("FATAL: recursion depth {} in {}", d, $name);
            std::process::abort();
        }
        if $crate::verbose::enabled() {
            eprint!("{:indent$}", "", indent = d * 2);
            eprintln!("-> {}", $name);
        }
        d
    }};
    ($name:expr, $($arg:tt)*) => {{
        let d = $crate::verbose::enter();
        if d > 500 {
            eprintln!("FATAL: recursion depth {} in {}", d, $name);
            std::process::abort();
        }
        if $crate::verbose::enabled() {
            eprint!("{:indent$}", "", indent = d * 2);
            eprint!("-> {} ", $name);
            eprintln!($($arg)*);
        }
        d
    }};
}

#[macro_export]
macro_rules! verbose_leave {
    () => {
        $crate::verbose::leave();
    };
}
