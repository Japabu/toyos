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
    /// The kernel implementation masks IF and TF across reservation and
    /// publication. Loom has no per-CPU flags; the model's sole-writer
    /// precondition is the corresponding witness here.
    pub struct LogCommitGuard;

    impl LogCommitGuard {
        pub fn close() -> Self {
            Self
        }
    }

    pub mod tlb {
        pub fn poll() {}
    }

    /// **A strictly stronger model than the instruction, and the direction is
    /// the whole argument.**
    ///
    /// The kernel's `percpu_fetch_add` is one `xadd` with no `lock` prefix
    /// inside a `cli` bracket. The only behaviour it has that a real
    /// `fetch_add` does not is non-atomicity against *another CPU's* write to
    /// the same word — and the bracket is what makes "no other CPU writes
    /// `head`" true rather than hopeful. So every interleaving the real code
    /// can produce, loom explores here; the shim cannot hide a race.
    ///
    /// Stated with its precondition, because without the bracket this shim is
    /// the thing hiding the bug rather than the thing modelling around it.
    ///
    /// # Safety
    /// Same contract as the kernel's: a word only one CPU writes.
    #[cfg(feature = "loom")]
    pub unsafe fn percpu_fetch_add(
        counter: &loom::sync::atomic::AtomicU64,
        _guard: &LogCommitGuard,
    ) -> u64 {
        counter.fetch_add(1, loom::sync::atomic::Ordering::Relaxed)
    }

    /// Host-fast form used to exercise the real zero-allocation constructor.
    #[cfg(not(feature = "loom"))]
    pub unsafe fn percpu_fetch_add(
        counter: &core::sync::atomic::AtomicU64,
        _guard: &LogCommitGuard,
    ) -> u64 {
        counter.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
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

#[path = "../../kernel/src/log/shard.rs"]
pub mod log_shard;

/// `registry.rs` names its shard as `super::shard`, which in the kernel is
/// `crate::log::shard`. Here `super` is the crate root, so this is what makes
/// the one path resolve in both builds — and it holds whether or not the `loom`
/// feature is on, which the crate's other invocation depends on.
pub use log_shard as shard;

#[path = "../../kernel/src/log/registry.rs"]
pub mod log_registry;
