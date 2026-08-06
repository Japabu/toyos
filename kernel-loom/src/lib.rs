//! Loom harness for `kernel/src/sync.rs`.
//!
//! The kernel file is compiled into this crate with `feature = "loom"` on, so
//! `Lock`'s atomics and its cell resolve to loom's instrumented ones. What the
//! kernel file names through `crate::` is supplied below: the lock takes a
//! preempt count and a log macro from its environment, and neither is what the
//! models are about.
//!
//! Scope, stated because it is narrower than the file: the models drive
//! `try_lock` and `LockGuard::drop`. `lock()`'s spin cannot be modelled —
//! loom explores a spin as an unbounded branch and gives up ("Model exceeded
//! maximum number of branches"), and the `yield_now` that would fix it belongs
//! to loom rather than to a kernel that really does spin.

/// Loom's cell with the `get(&self) -> *mut T` shape `Lock` uses.
///
/// Every access is recorded as a mutable one. That is conservative in the safe
/// direction: loom reports a pair only when they are *not* causally ordered, so
/// a correctly synchronized lock still passes.
pub mod cell {
    pub struct UnsafeCell<T>(loom::cell::UnsafeCell<T>);

    impl<T> UnsafeCell<T> {
        pub fn new(value: T) -> Self {
            Self(loom::cell::UnsafeCell::new(value))
        }

        pub fn get(&self) -> *mut T {
            self.0.with_mut(|ptr| ptr)
        }
    }
}

/// The kernel's per-CPU preempt count has no bearing on the memory ordering
/// these models check, and loom has no per-CPU state to hang one on.
pub mod preempt {
    pub fn disable() {}
    pub fn enable() {}
}

/// The contention and deadlock reports are unreachable in these models — the
/// spin they fire from is what loom cannot explore — but the arguments are
/// consumed so the kernel file's bindings are still live code here.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{ let _ = format_args!($($arg)*); }};
}

#[path = "../../kernel/src/sync.rs"]
pub mod sync;
