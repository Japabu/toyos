//! `log-nested-emit`: an interrupt that logs, landing inside another `emit`.
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
//! the instant the guard drops: the handler's burst then laps the shard, and
//! the outer record goes by the ring's own drop-oldest policy. Without the
//! bracket the same IPI lands *inside* the copy, the burst commits a whole
//! newer generation into the slot, and the resumed outer writer overwrites a
//! record that has already been published.
//!
//! **It runs on a kernel thread and not in the syscall that arms it**, and that
//! is the difference between a gate and a tautology: `IF` is clear for the whole
//! of every syscall, so a record emitted from one is bracketed whether or not
//! the guard exists, and removing the guard would change nothing. A kernel
//! thread's body runs with `IF` set, which is where the guard is the only thing
//! holding the interrupt off.
//!
//! `specs/log-architecture-spec.md` §9.2.

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

    /// The one-shot: set around the record the injection is meant to land
    /// inside, and consumed by whichever injection point reaches it first.
    static ARMED: AtomicBool = AtomicBool::new(false);

    /// Set by the injection and cleared by the handler, so a delivery that
    /// arrives for any other reason emits nothing.
    static OWED: AtomicBool = AtomicBool::new(false);

    static STARTED: AtomicBool = AtomicBool::new(false);

    /// Instructions of nothing at the mid-body point.
    ///
    /// **A delivery window, not a delay.** A self-IPI is written to the ICR and
    /// delivered at an instruction boundary; with `IF` set that is within a
    /// handful of instructions, and this is what makes "inside the body copy"
    /// true rather than "shortly after it". With `IF` clear it costs exactly
    /// this many `pause`s and changes nothing at all.
    const WINDOW: usize = 256;

    pub fn start_once() {
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
        ARMED.store(true, Ordering::Relaxed);
        crate::log!("lognest outer, and an interrupt is due inside this record's body");
        // Whatever happened, the one-shot does not outlive the record it was
        // armed for: a later injection would nest inside an unrelated line.
        ARMED.store(false, Ordering::Relaxed);
        crate::log!("lognest done emitted={SHARD_RECORDS}");

        loop {
            let ticket = crate::scheduler::prepare_wait(crate::scheduler::park_lot());
            crate::scheduler::block_on(ticket, crate::time::Deadline::never());
        }
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
/// Not compiled into a shipping kernel: its one caller is the `log-nested-emit`
/// arm in `log::user`, which is `#[cfg]`'d away with the actuators. `mid_body`
/// below is the opposite case and says why it is the opposite.
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

/// The interrupt handler's body.
///
/// Its caller is the `log_nest` interrupt handler, which no shipping kernel
/// installs.
#[cfg(feature = "boot-actuators")]
pub fn deliver() {
    #[cfg(feature = "boot-actuators")]
    armed::deliver();
}
