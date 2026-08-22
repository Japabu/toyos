//! An interrupt that logs, landing inside another `emit` — at one of two points
//! of the window, and the point is which actuator is armed.
//!
//! **The case loom cannot express.** Loom models threads, not CPU flags and not
//! strict LIFO reentrancy on one CPU, so §2.4's fourth property — that a nested
//! writer cannot collide with the writer it interrupted — has no model. Nothing
//! on the host can stage it either: there is no injection that interrupts a
//! kernel between two instructions of one function.
//!
//! **The stimulus is a self-IPI sent from inside `emit` itself**, so when it
//! arrives is a property of the flags and of nothing else. With §2.3a's bracket
//! it is pending across the whole reservation and body copy and is delivered
//! the instant the guard drops; without the bracket it lands where it was sent.
//!
//! `log-nested-emit` sends it from halfway through the body copy: the burst then
//! commits a whole newer generation into the slot the interrupted writer is
//! standing in, and the resumed writer overwrites a record that has already been
//! published. **That corruption is invisible to every reader in the machine**,
//! and by design rather than by oversight — the resumed writer republishes the
//! *previous* generation's number, which is exactly what a slot whose writer has
//! not published yet looks like, and `Shard::oldest_readable` has already moved
//! past the record it destroyed. What it costs is one record and a stalled
//! shard, and neither is distinguishable from the ring's declared drop-oldest
//! policy.
//!
//! `log-nested-reserve` sends it from **between the shard-pointer read and the
//! unlocked `xadd`** — the window §2.3a's bracket names first — and that one is
//! observable, because the two orders a shard has stop being the same order.
//! `emit` stamps `at_ns` before it reserves, so a handler that logs from inside
//! that window takes the *lower* sequence numbers and carries the *later*
//! timestamps, and the interrupted producer's own record lands above them with a
//! timestamp from before any of them. `read.rs`'s `Descent::advance` is written
//! against exactly that not happening, and `test-runner`'s log gate refuses a
//! shard whose `at_ns` descends.
//!
//! **It runs on a kernel thread and not in the syscall that arms it**, and that
//! is the difference between a gate and a tautology: `IF` is clear for the whole
//! of every syscall, so a record emitted from one is bracketed whether or not
//! the guard exists, and removing the guard would change nothing. A kernel
//! thread's body runs with `IF` set, which is where the guard is the only thing
//! holding the interrupt off.

/// Which producer the burst's records declare themselves as.
///
/// **Past what any storm thread can be**, so the reader's per-producer ledger
/// takes them through exactly the same checks as a storm's — same text, same
/// regeneration, same strictly-increasing indices — with no second parser on
/// either side.
#[cfg(feature = "boot-actuators")]
pub const NEST_PRODUCER: u64 = u64::MAX;

#[cfg(feature = "boot-actuators")]
mod armed {
    use core::sync::atomic::{AtomicBool, Ordering};

    use crate::log::shard::SHARD_RECORDS;
    use crate::sched::kthread::{self, OnPanic};

    /// The body window's one-shot: set around the record the injection is meant
    /// to land inside, and consumed by `mid_body` — or, under
    /// `log-shared-reservation`, by the window that actuator opens inside the
    /// reservation instead.
    static ARMED: AtomicBool = AtomicBool::new(false);

    /// The reservation window's one-shot, consumed by [`reserve_window`].
    ///
    /// **A second flag and not a second setting of the first**, because the two
    /// injection points are on one path and the earlier one would consume
    /// everything: [`reserve_window`] runs before `mid_body` in every single
    /// record, so a shared flag would have made the body window unreachable the
    /// moment this one existed.
    static ARMED_RESERVE: AtomicBool = AtomicBool::new(false);

    /// Set by the injection and cleared by the handler, so a delivery that
    /// arrives for any other reason emits nothing.
    static OWED: AtomicBool = AtomicBool::new(false);

    static STARTED: AtomicBool = AtomicBool::new(false);

    /// Instructions of nothing at whichever injection point sent the IPI.
    ///
    /// **A delivery window, not a delay.** A self-IPI is written to the ICR and
    /// delivered at an instruction boundary; with `IF` set that is within a
    /// handful of instructions, and this is what makes "inside the body copy" —
    /// or "before the `xadd`" — true rather than "shortly after it". With `IF`
    /// clear it costs exactly this many `pause`s and changes nothing at all.
    const WINDOW: usize = 256;

    pub fn start_once() {
        // **The two windows refuse each other by name rather than one quietly
        // winning.** This thread arms one one-shot around one record; a boot
        // asking for both would inject into that record twice and neither gate
        // could say which injection its verdict was about. It is our own boot
        // parameter and crosses no trust boundary, so it dies here like any
        // other bug in this build system.
        assert!(
            !(crate::actuator::log_nested_emit() && crate::actuator::log_nested_reserve()),
            "log-nested-emit and log-nested-reserve both name the one injection this thread arms"
        );
        if STARTED.swap(true, Ordering::Relaxed) {
            return;
        }
        crate::log!("lognest start records={SHARD_RECORDS}");
        // `Halt`: this thread carries the whole stimulus, and a machine that
        // carried on after it died would answer the gate with a run in which
        // nothing was ever injected.
        kthread::spawn("lognest", body, 0, OnPanic::Halt);
    }

    extern "C" fn body(_arg: u64) -> ! {
        // One record, and which window it is interrupted in is the boot's to
        // say. `start_once` has already refused a boot that named both.
        if crate::actuator::log_nested_reserve() {
            ARMED_RESERVE.store(true, Ordering::Relaxed);
            crate::log!(
                "lognest outer, and an interrupt is due between this record's shard read and its \
                 xadd"
            );
            ARMED_RESERVE.store(false, Ordering::Relaxed);
        } else {
            ARMED.store(true, Ordering::Relaxed);
            crate::log!("lognest outer, and an interrupt is due inside this record's body");
            // Whatever happened, the one-shot does not outlive the record it was
            // armed for: a later injection would nest inside an unrelated line.
            ARMED.store(false, Ordering::Relaxed);
        }
        crate::log!("lognest done emitted={SHARD_RECORDS}");

        crate::completion::park_forever();
    }

    /// Consume the one-shot and send this CPU its own IPI. `true` when it was
    /// this call that sent one.
    pub fn inject() -> bool {
        if !ARMED.swap(false, Ordering::Relaxed) {
            return false;
        }
        OWED.store(true, Ordering::Relaxed);
        crate::arch::apic::send_self(crate::arch::idt::LOG_NEST_VECTOR);
        true
    }

    /// The injection point inside the body copy: send, then stand still long
    /// enough for the delivery to be *inside* the copy rather than after it.
    pub fn mid_body() {
        if !inject() {
            return;
        }
        for _ in 0..WINDOW {
            core::hint::spin_loop();
        }
    }

    /// The injection point between the shard-pointer read and the unlocked
    /// `xadd`, with the same delivery window behind it.
    ///
    /// **It consumes its own one-shot and sends its own IPI** rather than
    /// calling [`inject`], which belongs to the body window and to
    /// `log-shared-reservation`.
    pub fn reserve_window() {
        if !ARMED_RESERVE.swap(false, Ordering::Relaxed) {
            return;
        }
        OWED.store(true, Ordering::Relaxed);
        crate::arch::apic::send_self(crate::arch::idt::LOG_NEST_VECTOR);
        for _ in 0..WINDOW {
            core::hint::spin_loop();
        }
    }

    /// The handler's whole body: a patterned burst of exactly one shard
    /// generation.
    ///
    /// **Exactly `SHARD_RECORDS`, and the number is half the verdict.** One
    /// generation is what makes the outer record's disappearance the ring's
    /// declared drop-oldest policy rather than a corruption — and what puts the
    /// resumed outer writer, on a tree with no bracket, on top of a record that
    /// has already committed.
    pub fn deliver() {
        if !OWED.swap(false, Ordering::Relaxed) {
            return;
        }
        for index in 0..SHARD_RECORDS as u64 {
            crate::log::storm::emit_patterned(super::NEST_PRODUCER, index);
        }
    }
}

/// Arm the injection on a kernel thread of its own, once.
///
/// Not compiled into a shipping kernel: its callers are the `log-nested-emit`
/// and `log-nested-reserve` arms in `log::user`, which are `#[cfg]`'d away with
/// the actuators. `mid_body` below is the opposite case and says why it is the
/// opposite.
#[cfg(feature = "boot-actuators")]
pub fn start_once() {
    #[cfg(feature = "boot-actuators")]
    armed::start_once();
}

/// Consume the one-shot at the reservation, for `log-shared-reservation`'s
/// window. `true` when an IPI went out.
pub fn inject() -> bool {
    #[cfg(feature = "boot-actuators")]
    return armed::inject();
    #[cfg(not(feature = "boot-actuators"))]
    false
}

/// The injection point halfway through a record's body copy.
///
/// Compiled in every build so `log::shard` — which `kernel-loom` compiles a
/// second time — names one path, and empty in every build but the test
/// kernel's. `kernel-loom`'s own shim is the third.
pub fn mid_body() {
    #[cfg(feature = "boot-actuators")]
    armed::mid_body();
}

/// The injection point between a record's shard-pointer read and its `xadd`.
///
/// Its one caller is `arch::percpu::reserve_log_slot`, which is not a file
/// `kernel-loom` compiles — so unlike [`mid_body`] this needs no third shim, and
/// it is empty in every build but the test kernel's.
pub fn reserve_window() {
    #[cfg(feature = "boot-actuators")]
    armed::reserve_window();
}

/// The interrupt handler's body.
///
/// Its caller is the `log_nest` interrupt handler, which no shipping kernel
/// installs.
#[cfg(feature = "boot-actuators")]
pub fn deliver() {
    #[cfg(feature = "boot-actuators")]
    armed::deliver();
}
