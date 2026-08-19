//! Loom harness for the kernel's memory-ordering primitives.
//!
//! `kernel/src/sync.rs`, `kernel/src/shootdown.rs`,
//! `kernel/src/sched/reap_gate.rs` and `kernel/src/drivers/i8042/tally.rs` are
//! compiled into this crate with `feature = "loom"` on, so their atomics and
//! cells resolve to loom's instrumented ones and the models drive the real
//! primitives rather than transliterations of them — a transliteration is
//! exactly the divergence risk a model checker is meant to remove. What the
//! kernel files name through `crate::` is supplied below: the lock takes a
//! preempt count and a log macro from its environment, and neither is what the
//! models are about; `shootdown.rs`, `reap_gate.rs` and `tally.rs` name nothing
//! at all, which is why none of the three has a shim here.
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

/// What the kernel has been told to break, and the models never are.
///
/// A shim rather than a `cfg` at the call site, so `commit` is one statement in
/// every build and the model drives the same line the kernel does.
pub mod actuator {
    /// The one `shard.rs` names: the nesting gate's mid-body injection point.
    /// Loom has no CPU flags and no interrupts, which is exactly why that gate
    /// exists on a machine instead — so the models drive the loop with nothing
    /// in it.
    pub const fn log_nested_emit() -> bool {
        false
    }
}

/// `shard.rs` calls into this from the mid-body point; in the kernel it is
/// `crate::log::nested`, and here `super` is the crate root.
pub mod nested {
    pub fn mid_body() {}
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

#[path = "../../kernel/src/sched/reap_gate.rs"]
pub mod reap_gate;

/// The duration kinds and the two time types. It names nothing outside `core`,
/// which is what lets a record carry an `Instant` into a model at all — and it
/// is compiled here so that constraint is checked rather than remembered.
#[path = "../../kernel/src/time.rs"]
pub mod time;

/// The completion core's record and inbox. It reaches `crate::time` and
/// `crate::cell` and nothing else, and that narrowness is load-bearing: a file
/// that named a pipe end or a device claim could not be compiled here at all,
/// and the ordering would stop being checked by anything.
#[path = "../../kernel/src/completion/inbox.rs"]
pub mod inbox;

/// What `sleeplock.rs` names of the completion core, and nothing more.
///
/// **The park is shimmed, and that is the scope statement for
/// `tests/sleep_lock.rs`.** Loom has no scheduler, so there is nothing here to
/// park *on*; [`completion::wait_uncancellable_until`] yields instead, which is
/// what lets the model drive the real contended acquire at all. What the model
/// therefore proves is the ticket arithmetic and the acquire/release edge —
/// mutual exclusion, ordering, and the order contenders are served in. What it
/// does **not** prove is the wake handshake: the record-then-claim pair that
/// makes a post reach a parked waiter is `tests/inbox.rs`'s, and it is modelled
/// there against the real `Inbox`.
///
/// `Watch` is a stand-in for the same reason: nothing arms here, so [`post_n`]
/// has nobody to tell and answers zero. The kernel's `Watch` owns a `Lock<Vec<_>>`
/// and could not be compiled into this crate anyway.
pub mod completion {
    pub use crate::inbox::{Outcome, Record, Token};
    use crate::scheduler::Parkable;

    /// The waiters armed on one object — a stand-in, because the park above it
    /// is shimmed and nothing ever arms.
    pub struct Watch;

    impl Watch {
        pub const fn new() -> Self {
            Self
        }
    }

    #[derive(Clone, Copy)]
    pub struct Subject<'a>(#[allow(dead_code)] &'a Watch);

    impl<'a> Subject<'a> {
        pub const fn of(watch: &'a Watch) -> Self {
            Self(watch)
        }
    }

    /// Nobody is armed here, so nobody is told. The kernel's answer is how many
    /// waiters this call claimed; the models never read it.
    pub fn post_n(_subject: Subject<'_>, _outcome: Outcome, _token: Token, _limit: usize) -> usize {
        0
    }

    /// The shimmed park. `yield_now` is loom's own way of saying "another
    /// thread may run here", which is exactly what a park is from the schedule
    /// explorer's point of view — and unlike `core::hint::spin_loop`, which is
    /// what makes `Lock::lock`'s spin unmodellable, it is a branch loom can
    /// bound.
    pub fn wait_uncancellable_until(
        _p: &Parkable,
        _subject: Subject<'_>,
        _token: Token,
        ready: impl Fn() -> bool,
    ) {
        while !ready() {
            #[cfg(feature = "loom")]
            loom::thread::yield_now();
            #[cfg(not(feature = "loom"))]
            std::hint::spin_loop();
        }
    }
}

/// What `sleeplock.rs` names of the scheduler: the right to park, and who is
/// asking.
///
/// Three items, and the arithmetic in [`TaskId`] is the whole of what is
/// restated here rather than compiled — `kernel/src/scheduler.rs` names the
/// process table, the `toyos-sched` driver and half the kernel besides, so it
/// cannot be a `#[path]` module the way `sync.rs` and `inbox.rs` are.
pub mod scheduler {
    /// The right to give the CPU back. In the kernel its constructor asserts
    /// the context's baseline preempt depth; here there is no preempt count and
    /// no context, so the type carries only the fact that a caller had one.
    pub struct Parkable(());

    impl Parkable {
        pub fn of_current() -> Self {
            Self(())
        }
    }

    /// Process-scoped thread identity, as the two halves of the word the lock
    /// stores.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct TaskId(pub u32, pub u32);

    impl TaskId {
        pub fn pack(self) -> u64 {
            u64::from(self.1) | (u64::from(self.0) << 32)
        }

        pub fn unpack(value: u64) -> Self {
            Self((value >> 32) as u32, value as u32)
        }
    }

    impl core::fmt::Display for TaskId {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "{}:{}", self.0, self.1)
        }
    }

    // Who this thread is, so the lock's holder word and its self-deadlock
    // refusal mean something. The model says; the kernel reads the same fact
    // out of two per-CPU words. A `//` comment and not a doc one: rustdoc
    // documents no item a macro invocation produces, and `///` here is a
    // warning rather than documentation.
    //
    // **`loom::thread_local!` under loom and `std::thread_local!` without it**,
    // and the split is not cosmetic: loom's threads are coroutines on one OS
    // thread, so a `std` slot would be one cell shared by all of them and every
    // task in the model would be the last one to speak.
    #[cfg(feature = "loom")]
    loom::thread_local! {
        static WHO: core::cell::Cell<Option<TaskId>> = core::cell::Cell::new(None);
    }
    #[cfg(not(feature = "loom"))]
    std::thread_local! {
        static WHO: core::cell::Cell<Option<TaskId>> = const { core::cell::Cell::new(None) };
    }

    /// Say who the current thread is. No kernel counterpart: there, the answer
    /// is whatever the CPU is running.
    pub fn become_task(id: TaskId) {
        WHO.with(|who| who.set(Some(id)));
    }

    pub fn current_task() -> Option<TaskId> {
        WHO.with(|who| who.get())
    }
}

/// The sleep lock, compiled a second time against loom's atomics. Its
/// dependency surface is the two shim modules above and nothing else — the same
/// narrowness `inbox.rs` carries, and for the same reason.
#[path = "../../kernel/src/sleeplock.rs"]
pub mod sleeplock;

/// The i8042's interrupt tally. `tests/i8042_tally.rs` is the only model whose
/// subject is a *driver*, and it is here for the reason the others are: the
/// property is "no reader ever sees this pair disagree", which is a claim about
/// instants that no guest test can express and that x86's TSO hides.
#[path = "../../kernel/src/drivers/i8042/tally.rs"]
pub mod i8042_tally;
