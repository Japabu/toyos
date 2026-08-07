//! Loom harness for the kernel's two memory-ordering primitives.
//!
//! `kernel/src/sync.rs` and `kernel/src/shootdown.rs` are compiled into this
//! crate with `feature = "loom"` on, so their atomics and cells resolve to
//! loom's instrumented ones and the models drive the real primitives rather than
//! transliterations of them — a transliteration is exactly the divergence risk a
//! model checker is meant to remove. What the kernel files name through
//! `crate::` is supplied below: the lock takes a preempt count and a log macro
//! from its environment, and neither is what the models are about;
//! `shootdown.rs` names nothing at all, which is why it has no shim here.
//!
//! Scope for the lock, stated because it is narrower than the file: the models
//! drive `try_lock` and `LockGuard::drop`. `lock()`'s spin cannot be modelled —
//! loom explores a spin as an unbounded branch and gives up ("Model exceeded
//! maximum number of branches"), and the `yield_now` that would fix it belongs
//! to loom rather than to a kernel that really does spin. The shootdown's spins
//! *are* modelled, because they live in the caller — `arch::tlb` — and the model
//! writes its own.

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

/// `Lock::lock`'s spin serves TLB shootdowns for a CPU that is not taking
/// interrupts. Empty here: the models do not drive that spin at all (see the
/// scope note above), and the protocol it would call has its own models.
pub mod arch {
    pub mod tlb {
        pub fn poll() {}
    }
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

#[path = "../../kernel/src/shootdown.rs"]
pub mod shootdown;
