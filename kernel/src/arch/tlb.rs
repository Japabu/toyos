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
//! The `IF=0` windows in this kernel are `drivers::serial`'s `save_and_cli`
//! around the backend lock, and every IDT gate, which the CPU enters with `IF`
//! clear. The second is the wide one: **every lock any interrupt or exception
//! handler takes is in the class**, and that includes the page-fault handler's
//! address-space and process-data locks and everything a scheduler pass touches
//! from the timer. So "do not hold a lock a target could be spinning on" cannot
//! be enumerated once and stay true; it would have to be re-derived every time
//! somebody added a lock to a handler.
//!
//! **So the target answers instead of the initiator abstaining.** `Lock::lock`'s
//! spin calls [`poll`] on every turn, which serves any outstanding shootdown.
//! A flush is safe from anywhere — it takes no lock, allocates nothing, and a
//! CPU that flushes more often than asked is merely slower — so a CPU waiting
//! for a lock with interrupts disabled acknowledges as promptly as one that took
//! the interrupt. That closes the class structurally, for locks nobody has
//! written yet as much as for the ones in the tree today.
//!
//! What is left is an `IF=0` spin that is *not* a `Lock`: a driver waiting on a
//! device register inside a handler. Those are latency, not deadlock, because
//! each carries its own deadline — but the deadline can be seconds
//! (`specs/known-issues.md` §3, xHCI inside `drain_irqs`), so [`ACK_TIMEOUT_NS`]
//! is set above the largest of them and a CPU past it is named in a panic
//! rather than waited for forever.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::shootdown::{Generation, Shootdown};

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
/// (`specs/known-issues.md` §3). Anything past that is not a slow CPU, it is a
/// CPU that will never answer, and a panic naming it is worth more than a hang
/// that looks like every other freeze.
const ACK_TIMEOUT_NS: u64 = 5_000_000_000;

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
/// The local flush is the whole TLB rather than the `invlpg` the unmap already
/// did, and that is the fix for the wrong-PCID half of the defect: `invlpg`
/// tags the *current* CR3's PCID, while `shared_memory` and `virtio_gpu` unmap
/// from a process that is not the one running here. A CPU-wide flush is correct
/// under every PCID configuration and is what the targets do anyway.
pub fn shootdown() {
    crate::mm::paging::flush_tlb_all();

    if !SIBLINGS_ANSWER.load(Ordering::Acquire) {
        return;
    }
    let cpus = smp::cpu_count();
    if cpus <= 1 {
        return;
    }
    let me = percpu::cpu_id();
    let generation = SHOOTDOWN.issue();
    apic::tlb_ipi();
    for cpu in 0..cpus {
        if cpu != me {
            wait_for(cpu, generation);
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
fn wait_for(cpu: u32, generation: Generation) {
    let mut spins = 0u32;
    let mut deadline = None;
    while !SHOOTDOWN.served(cpu as usize, generation) {
        core::hint::spin_loop();
        spins += 1;
        if spins == SPINS_PER_DEADLINE_CHECK {
            spins = 0;
            let now = crate::clock::nanos_since_boot();
            match deadline {
                None => deadline = Some(now.saturating_add(ACK_TIMEOUT_NS)),
                Some(at) if now >= at => panic!(
                    "tlb: cpu {cpu} has not flushed for generation {generation:?} in \
                     {ACK_TIMEOUT_NS}ns — it is not taking interrupts",
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
    if SHOOTDOWN.owes(cpu) {
        SHOOTDOWN.serve(cpu, crate::mm::paging::flush_tlb_all);
    }
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

#[cfg(not(feature = "test-tlb-ack-delay"))]
fn stage_ack_delay() {}

#[cfg(feature = "test-tlb-ack-delay")]
mod delay {
    use core::sync::atomic::{AtomicU32, AtomicU64};

    pub static NANOS: AtomicU64 = AtomicU64::new(0);
    pub static CPU: AtomicU32 = AtomicU32::new(u32::MAX);
}

/// Hold this CPU's acknowledgement back, without holding its flush back.
///
/// The delay is *after* the flush and before the publication, so what it stages
/// is a slow answer and never an incorrect one — a target that skipped its flush
/// would be the defect rather than an instrument for measuring the fix. What
/// nothing else can stage: QEMU has no way to make one vCPU answer an IPI late,
/// and without a late answer the initiator's wait is unobservable, because a
/// correct wait and no wait at all take the same measurable zero.
#[cfg(feature = "test-tlb-ack-delay")]
fn stage_ack_delay() {
    use core::sync::atomic::Ordering;
    if delay::CPU.load(Ordering::Relaxed) != percpu::cpu_id() {
        return;
    }
    let nanos = delay::NANOS.swap(0, Ordering::Relaxed);
    if nanos == 0 {
        return;
    }
    let until = crate::clock::nanos_since_boot().saturating_add(nanos);
    while crate::clock::nanos_since_boot() < until {
        core::hint::spin_loop();
    }
}

/// Arm the highest-numbered CPU to answer its next shootdown `nanos` late, then
/// take one and report how long it took, in nanoseconds.
///
/// The *highest*-numbered CPU rather than any one, because the initiator walks
/// its targets in order: a wait that only covered the first would still measure
/// long if the delay were armed on cpu 1, and the point of the gate is that
/// every online CPU is waited for.
#[cfg(feature = "test-tlb-ack-delay")]
pub fn debug_timed_shootdown(nanos: u64) -> u64 {
    use core::sync::atomic::Ordering;
    let last = smp::cpu_count() - 1;
    if last == percpu::cpu_id() || last == 0 {
        return 0;
    }
    delay::CPU.store(last, Ordering::Relaxed);
    delay::NANOS.store(nanos, Ordering::Relaxed);
    let start = crate::clock::nanos_since_boot();
    shootdown();
    let elapsed = crate::clock::nanos_since_boot() - start;
    delay::CPU.store(u32::MAX, Ordering::Relaxed);
    delay::NANOS.store(0, Ordering::Relaxed);
    elapsed
}
