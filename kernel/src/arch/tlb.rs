//! The machine-wide TLB shootdown.
//!
//! `arch::idt::tlb` owns vector 0xFE's entry stub; this owns what the vector is
//! *for*. The protocol itself — the generation counter and the per-CPU
//! publication — is `crate::shootdown`, which has no hardware in it so that
//! `kernel-loom` can drive the real code.
//!
//! **A shootdown returns when every other CPU has flushed, not when the IPI has
//! been written.** Until this stage it returned after one ICR write with no
//! acknowledgement path anywhere, which made every unmap-then-free in the kernel
//! a use-after-free with a short window: `MappedPages::release` dropped pages
//! back to the PMM while a sibling could still write through a translation for
//! them. The same gap is what lets one CPU hold a page write-combining while a
//! sibling's stale entry still calls it write-back, which SDM Vol. 3A §11.12.4
//! leaves undefined and permits a machine to hang on.
//!
//! ## The deadlock a synchronous shootdown opens, and why this one does not
//!
//! A target spinning with `IF` clear cannot take the IPI. An initiator that
//! waits for it while holding what it is spinning on never finishes — and it
//! would look exactly like the freeze this stage is a candidate for.
//!
//! **`IF` is clear for the whole of every syscall** — `arch::syscall`'s
//! `MSR_FMASK` masks it on the `SYSCALL` gate and nothing sets it again before
//! `sysretq` — and every IDT gate is entered with it clear too. So the class is
//! not a window somebody could enumerate: it is every unmap-then-free the kernel
//! performs, since all of them are reached from a syscall or a fault, plus every
//! lock any handler takes.
//!
//! **So the target answers instead of the initiator abstaining.** `Lock::lock`'s
//! spin calls [`poll`] on every turn, and the wait below calls
//! `Shootdown::wait_turn`, which answers before it asks. A flush is safe from
//! anywhere — it takes no lock, allocates nothing, and a CPU that flushes more
//! often than asked is merely slower — so a CPU that cannot take the interrupt
//! acknowledges as promptly as one that did. That closes the class structurally,
//! for locks nobody has written yet as much as for the ones in the tree today.
//!
//! **The initiator's own wait was outside that closure until 2026-08-07 and is
//! the reason this paragraph is longer than its author left it.** Two CPUs that
//! issued concurrently each spun for the other's acknowledgement with `IF`
//! clear and no path left by which either could give one, so both died at the
//! deadline — reproduced as a double kernel panic and as seven different
//! wide-phase test failures carrying one signature (`issues/kernel/`).
//!
//! What is left is an `IF=0` spin that is *not* a `Lock` and not this wait: a
//! driver waiting on a device register inside a handler. Those are latency, not
//! deadlock, because each carries its own deadline — but the deadline can be
//! seconds (`issues/kernel/`, xHCI inside `drain_irqs`), so
//! [`ACK_TIMEOUT`] is set above the largest of them and a CPU past it is
//! named in a panic rather than waited for forever.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::shootdown::{Generation, Shootdown};
use crate::time::{Duration, Tripwire};

use super::{apic, percpu, smp};

static SHOOTDOWN: Shootdown = Shootdown::new();

/// Whether a shootdown waits for its siblings yet.
///
/// False for the whole of SMP bring-up, and that is not an optimisation. An AP
/// that has been counted by `CPU_COUNT` is spinning on `SMP_READY` with `IF`
/// clear — the trampoline's `cli` is never undone until the idle loop — so it
/// cannot take the IPI, and a driver's `map_mmio` between `boot_aps` and
/// `set_ready` would wait for a CPU that is structurally unable to answer.
///
/// What makes skipping the wait sound is [`join`]: every AP flushes and
/// publishes on the far side of `SMP_READY`, so a shootdown issued while this
/// was false is answered retroactively by the join of every CPU that could have
/// been holding a stale entry for it.
static SIBLINGS_ANSWER: AtomicBool = AtomicBool::new(false);

/// How long a CPU gets to acknowledge before the machine is declared broken.
///
/// Generous on purpose: a target inside `drain_irqs` may be in xHCI enumeration
/// or endpoint recovery, which spin on `USB_TIMEOUT_NS` = 2 s with `IF` clear
/// (`issues/kernel/`). Anything past that is not a slow CPU, it is a
/// CPU that will never answer, and a panic naming it is worth more than a hang
/// that looks like every other freeze.
///
/// A [`Tripwire`]: it panics below, and neither a register nor a specification
/// publishes it. Its *derivation* is `USB_TIMEOUT_NS`, which splits at C10 —
/// so the number owes a new reason to whichever chunk does that, and the kind
/// does not change with it.
const ACK_TIMEOUT: Tripwire = Tripwire::absurd(
    Duration::from_secs(5),
    "above the longest IF-clear device spin a target can be inside",
);

/// Spins between deadline checks. `nanos_since_boot` is an HPET read on the
/// machines that have no invariant TSC, and reading it every iteration would
/// make the wait's own cost the thing being measured.
const SPINS_PER_DEADLINE_CHECK: u32 = 1024;

/// Flush this CPU and every other one, and do not return until they have.
///
/// Callers pair this with the page-table write it publishes: write first, then
/// shoot down, then free. [`crate::mm::Unmapped`] is the type that makes the
/// pairing hard to get wrong; this is what it calls.
///
/// The local flush is the whole TLB and not the one address the caller changed,
/// because a shootdown answers mutations that are not one address: a direct-map
/// leaf changing memory type is a 2 MiB window this CPU may hold under any tag,
/// and a recycled PCID is every address there is. It is also exactly what the
/// targets do, so the initiator and its siblings end in the same state.
///
/// The single address is `mm::paging`'s own and it is derived there, in the
/// address space that was written — which until 2026-08-19 was whatever `CR3`
/// held instead, and on `shared_memory`'s and `virtio_gpu`'s cross-process
/// unmaps that is the caller's process rather than the one being unmapped.
pub fn shootdown() {
    let cpus = smp::cpu_count();
    if !SIBLINGS_ANSWER.load(Ordering::Acquire) || cpus <= 1 {
        crate::mm::paging::flush_tlb_all();
        return;
    }
    let me = percpu::cpu_id() as usize;
    let generation = SHOOTDOWN.issue();
    // The local flush and this CPU's own acknowledgement are one act: a sibling
    // that issued while this CPU was writing page tables has the same claim on
    // an answer from here as this one has on an answer from it.
    SHOOTDOWN.serve(me, crate::mm::paging::flush_tlb_all);
    apic::tlb_ipi();
    for cpu in 0..cpus {
        if cpu as usize != me {
            wait_for(me, cpu, generation);
        }
    }
}

/// Spin until `cpu` has flushed for `generation`, or declare it lost.
///
/// **Nothing here may log.** `drivers::serial` takes its backend lock under
/// `save_and_cli`, so a line printed between the ICR write and the last
/// acknowledgement is the deadlock this module's rule exists to prevent — the
/// initiator would hold the one lock a target cannot wait for. The panic at the
/// deadline is the exception and it is deliberate: by then the wait has already
/// failed and the machine is going down either way.
fn wait_for(me: usize, cpu: u32, generation: Generation) {
    let mut spins = 0u32;
    let mut deadline = None;
    while !SHOOTDOWN.wait_turn(me, cpu as usize, generation, crate::mm::paging::flush_tlb_all) {
        core::hint::spin_loop();
        spins += 1;
        if spins == SPINS_PER_DEADLINE_CHECK {
            spins = 0;
            let now = crate::clock::nanos_since_boot();
            match deadline {
                None => deadline = Some(now.saturating_add(ACK_TIMEOUT.nanos())),
                Some(at) if now >= at => panic!(
                    "tlb: cpu {cpu} has not flushed for generation {generation:?} in {}ns — \
                     it is not taking interrupts",
                    ACK_TIMEOUT.nanos(),
                ),
                Some(_) => {}
            }
        }
    }
}

/// Vector 0xFE's whole body: flush this CPU and say which generation it covers.
pub fn serve_ipi() {
    let cpu = percpu::cpu_id() as usize;
    SHOOTDOWN.serve(cpu, || {
        crate::mm::paging::flush_tlb_all();
        stage_ack_delay();
    });
}

/// Answer a shootdown from a CPU that is not going to take the interrupt.
///
/// Called from `Lock::lock`'s spin, which is the one unbounded wait in the
/// kernel that runs with `IF` clear often enough to matter — see this module's
/// header. It takes no lock and allocates nothing, so it is safe from inside the
/// lock primitive itself; the `SIBLINGS_ANSWER` check doubles as the guard that
/// `percpu::cpu_id` is readable, since GS is set long before `set_ready`.
#[inline]
pub fn poll() {
    if !SIBLINGS_ANSWER.load(Ordering::Relaxed) {
        return;
    }
    let cpu = percpu::cpu_id() as usize;
    SHOOTDOWN.serve_if_owed(cpu, crate::mm::paging::flush_tlb_all);
}

/// A CPU joining the machine settles every shootdown issued while it could not
/// answer one.
///
/// Called once, after the AP observes `SMP_READY` and before it can run
/// anything else — so the flush covers every page-table write the BSP made
/// during bring-up, and the generation it publishes is the one those writes
/// produced.
pub fn join() {
    let cpu = percpu::cpu_id() as usize;
    SHOOTDOWN.serve(cpu, crate::mm::paging::flush_tlb_all);
}

/// From here on a shootdown waits. Called by `smp::set_ready`, which is also
/// what releases the APs from the spin that made them unable to answer.
pub fn siblings_answer() {
    SIBLINGS_ANSWER.store(true, Ordering::Release);
}

#[cfg(not(feature = "test-actuators"))]
fn stage_ack_delay() {}

#[cfg(feature = "test-actuators")]
mod delay {
    use core::sync::atomic::{AtomicU32, AtomicU64};

    pub static NANOS: AtomicU64 = AtomicU64::new(0);
    pub static CPU: AtomicU32 = AtomicU32::new(u32::MAX);
    /// Absolute nanoseconds past which the arming lapses.
    pub static UNTIL: AtomicU64 = AtomicU64::new(0);
}

/// How long an arming stays live.
///
/// It expires rather than being one-shot for two reasons, and the second is the
/// one that matters. One-shot would be spent by whatever shootdown happened to
/// come first — a daemon exiting, a `dlopen` — and the syscall under measurement
/// would read zero and fail for a reason that is not the defect. And a window
/// cannot outlive a test that panicked before disarming, which a latch would.
#[cfg(feature = "test-actuators")]
const ARM_WINDOW_NANOS: u64 = 2_000_000_000;

/// Hold this CPU's acknowledgement back, without holding its flush back.
///
/// The delay is *after* the flush and before the publication, so what it stages
/// is a slow answer and never an incorrect one — a target that skipped its flush
/// would be the defect rather than an instrument for measuring the fix. What
/// nothing else can stage: QEMU has no way to make one vCPU answer an IPI late,
/// and without a late answer the initiator's wait is unobservable, because a
/// correct wait and no wait at all take the same measurable zero.
#[cfg(feature = "test-actuators")]
fn stage_ack_delay() {
    use core::sync::atomic::Ordering;
    if delay::CPU.load(Ordering::Relaxed) != percpu::cpu_id() {
        return;
    }
    let now = crate::clock::nanos_since_boot();
    if now >= delay::UNTIL.load(Ordering::Relaxed) {
        return;
    }
    let until = now.saturating_add(delay::NANOS.load(Ordering::Relaxed));
    while crate::clock::nanos_since_boot() < until {
        core::hint::spin_loop();
    }
}

/// The CPU an initiator on this one waits for *last*.
///
/// The last rather than any, because [`shootdown`] walks its targets in order: a
/// wait that covered only the first would still measure long if the delay sat on
/// cpu 1, and what the gate is about is that every online CPU is waited for.
#[cfg(feature = "test-actuators")]
fn last_target() -> Option<u32> {
    let top = smp::cpu_count().checked_sub(1)?;
    match percpu::cpu_id() {
        me if me == top => top.checked_sub(1),
        _ => Some(top),
    }
}

/// Make the last CPU this one waits for answer `nanos` late for the next
/// [`ARM_WINDOW_NANOS`], take one shootdown, and report what it cost.
///
/// The return value is the gate on the primitive; the arming outlives it so the
/// caller can then time an ordinary syscall and gate the *paths*.
#[cfg(feature = "test-actuators")]
pub fn debug_arm_ack_delay(nanos: u64) -> u64 {
    use core::sync::atomic::Ordering;
    let Some(target) = last_target() else { return 0 };
    delay::CPU.store(target, Ordering::Relaxed);
    delay::NANOS.store(nanos, Ordering::Relaxed);
    delay::UNTIL.store(
        crate::clock::nanos_since_boot().saturating_add(ARM_WINDOW_NANOS),
        Ordering::Relaxed,
    );
    let start = crate::clock::nanos_since_boot();
    shootdown();
    crate::clock::nanos_since_boot() - start
}

/// Give the machine its ordinary latency back before the window lapses.
#[cfg(feature = "test-actuators")]
pub fn debug_disarm_ack_delay() -> u64 {
    use core::sync::atomic::Ordering;
    delay::UNTIL.store(0, Ordering::Relaxed);
    delay::CPU.store(u32::MAX, Ordering::Relaxed);
    0
}
