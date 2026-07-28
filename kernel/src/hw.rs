//! `KernelHw` — the kernel's side of the scheduler-core hardware boundary
//! (spec §10.1, migration stage 6).
//!
//! Everything here is x2APIC, TSC or a single instruction. Nothing here
//! decides anything: no queue is consulted, no state machine advances, no
//! ordering-sensitive protocol lives below this line. That is the whole
//! contract — the simulator replaces this file and nothing else.
//!
//! Stage 6 implements [`Machine`], not [`toyos_sched::hw::Hw`]. The two
//! members `Hw` adds — the context switch and the finalize sink — are the two
//! that name a task, and the kernel has no `SchedPayload` until the cutover.
//! Implementing them now would mean importing stage 7's task record to leave
//! two methods unreachable.

use core::arch::asm;

use toyos_sched::hw::{CpuId, Kicker, Machine, Nanos, TraceEvent};

use crate::arch::{apic, percpu};

/// The one instance. Zero-sized: every effect is on a model-specific register
/// of the CPU that calls it, or a targeted ICR write addressed by argument —
/// there is no per-machine state a second value could hold.
pub static HW: KernelHw = KernelHw;

pub struct KernelHw;

/// The scheduler's clock reads, as raw nanoseconds.
///
/// The old scheduler timestamps in `u64` and samples the clock about a dozen
/// times per pass; stage 7 samples [`Machine::now`] once per pass and threads
/// the [`Nanos`] as a value. Until then this is where the two meet, and its
/// call count is the honest size of that debt.
pub fn now_ns() -> u64 {
    HW.now().0
}

/// RAII interrupt gate. Restores the caller's `IF` rather than setting it
/// unconditionally, so nesting inside an already-closed region is safe.
#[must_use = "the interrupt gate closes when the guard drops"]
pub struct IrqGuard {
    rflags: u64,
}

impl IrqGuard {
    fn close() -> Self {
        let rflags: u64;
        unsafe {
            asm!("pushfq", "pop {}", "cli", out(reg) rflags, options(nomem));
        }
        Self { rflags }
    }
}

impl Drop for IrqGuard {
    fn drop(&mut self) {
        unsafe {
            asm!("push {}", "popfq", in(reg) self.rflags, options(nomem));
        }
    }
}

impl Kicker for KernelHw {
    fn kick(&self, target: CpuId) {
        apic::kick_cpu(target.0);
    }
}

impl Machine for KernelHw {
    type IrqGuard = IrqGuard;

    fn now(&self) -> Nanos {
        Nanos(crate::clock::nanos_since_boot())
    }

    /// The trait's deadline is absolute; the LAPIC one-shot's initial-count
    /// register is relative, so this samples the clock a second time to
    /// subtract. That second sample is the cost of the mismatch, and it is
    /// the mismatch TSC-deadline mode removes — `IA32_TSC_DEADLINE` takes an
    /// absolute value, so the conversion here becomes ns→TSC scaling with no
    /// clock read at all.
    ///
    /// A deadline already in the past arms the one-tick minimum and fires
    /// immediately (`arm_one_shot` clamps), which is what a past-due deadline
    /// should do.
    fn set_timer(&self, deadline: Nanos) {
        apic::arm_one_shot(deadline.0.saturating_sub(self.now().0));
    }

    fn stop_timer(&self) {
        apic::stop_timer();
    }

    fn irq_guard(&self) -> IrqGuard {
        IrqGuard::close()
    }

    fn halt(&self) {
        unsafe { asm!("sti; hlt", options(nomem, nostack)); }
    }

    /// A remote CPU's `need_resched` byte is not writable from here: `PerCpu`
    /// is reachable only through this CPU's `GS` base, and there is no
    /// registry of sibling `PerCpu` pointers. The kick IPI is the way to say
    /// it — the timer vector's Ring 0 stub sets `need_resched` on arrival and
    /// its Ring 3 path runs the preempt check directly, so a kick *is* a
    /// remote resched request, with an interrupt as the delivery mechanism.
    fn need_resched(&self, cpu: CpuId) {
        if cpu.0 == percpu::cpu_id() {
            crate::preempt::set_need_resched();
        } else {
            self.kick(cpu);
        }
    }

    fn trace(&self, ev: TraceEvent) {
        crate::trace::record(ev);
    }
}
