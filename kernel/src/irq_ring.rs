//! Per-CPU IRQ event records — the scheduler-core spec's Stage 2 `irq_ring`
//! (spec §11), generalizing the audio completion pattern to every MSI-X
//! source (B10): the ISR records the IRQ-time timestamp on the CPU that took
//! the interrupt and sets `need_resched`; that CPU's next scheduler entry
//! (`sched::driver::drain_irqs`) consumes the record and converts it into
//! waiter wakes, io_uring CQEs, or controller polls. The audio DATA path
//! (per-completion `(mask, timestamp)` records read by soundd) lives in
//! `crate::audio` and is unrelated — this module only drives *scheduling* off
//! IRQs.
//!
//! Shape: one timestamp slot per `(cpu, source)` instead of a record queue.
//! Consumers re-derive device state at drain time (virtio used rings, xHCI
//! event ring), so back-to-back IRQs coalesce into one record and overflow
//! has no representation — the same resolution the spec applies to mailbox
//! messages (B8). The slot keeps the EARLIEST undrained timestamp, so the
//! traced latency honestly covers the oldest unserviced interrupt.
//!
//! Concurrency contract: every function indexes by `percpu::cpu_id()`, so
//! the API cannot express a cross-CPU access — all traffic on a CPU's slots
//! is same-CPU. Producers are MSI-X ISRs running with IF=0 (their
//! load-then-store pair cannot interleave with a consumer on the same CPU);
//! consumers are thread/timer contexts whose slot access is a single atomic
//! instruction (cannot be torn by an interrupting ISR). Relaxed ordering is
//! therefore sufficient everywhere: same-CPU accesses are program-ordered,
//! and there is no remote observer to publish to. Any future cross-CPU
//! consumer needs a redesign (swap-based publish), not stronger orderings.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::percpu;
use crate::scheduler::MAX_CPUS;

/// Interrupt sources that drive scheduling. Exhaustive — adding a device
/// vector means adding a variant, and every `match` on this must be updated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IrqSource {
    Audio,
    Net,
    Xhci,
}

impl IrqSource {
    pub const COUNT: usize = 3;
}

/// 64-byte aligned so two CPUs' slots never share a cache line — the array
/// is indexed by cpu_id and each CPU hammers only its own entry.
#[repr(align(64))]
struct CpuSlots([AtomicU64; IrqSource::COUNT]);

/// Slot value 0 = no undrained IRQ; nonzero = `nanos_since_boot` of the
/// earliest undrained IRQ for that source.
static SLOTS: [CpuSlots; MAX_CPUS] =
    [const { CpuSlots([const { AtomicU64::new(0) }; IrqSource::COUNT]) }; MAX_CPUS];

/// Record an IRQ on the current CPU. ISR-only: lock-free and heap-free. A
/// second IRQ before the drain coalesces into the existing record, keeping
/// the earlier timestamp.
pub fn isr_publish(source: IrqSource, timestamp_nanos: u64) {
    // 0 is the empty sentinel. MSI-X vectors are configured long after the
    // clock is calibrated, so a real IRQ can never legitimately stamp 0.
    assert!(timestamp_nanos != 0, "irq_ring: zero IRQ timestamp");
    let slot = &SLOTS[percpu::cpu_id() as usize].0[source as usize];
    // Load-then-store is atomic against `take`: this runs with IF=0 on the
    // slot's own CPU (see module doc for why Relaxed suffices).
    if slot.load(Ordering::Relaxed) == 0 {
        slot.store(timestamp_nanos, Ordering::Relaxed);
    }
}

/// Consume the current CPU's pending record for `source`, returning the
/// IRQ-time timestamp. Emits an `IrqDrain` trace event carrying the
/// IRQ→service latency — the observable form of B10's delivery delay.
pub fn take(source: IrqSource) -> Option<u64> {
    let slot = &SLOTS[percpu::cpu_id() as usize].0[source as usize];
    // Single atomic swap: an ISR interrupting mid-consume sees either the
    // old record (about to be consumed) or the cleared slot (re-publishes).
    match slot.swap(0, Ordering::Relaxed) {
        0 => None,
        ts => {
            let latency_us = crate::clock::nanos_since_boot().saturating_sub(ts) / 1_000;
            crate::trace::trace_irq_drain(source, latency_us);
            Some(ts)
        }
    }
}

/// Any undrained IRQ record on the current CPU? Non-consuming — the idle
/// loop's pre-hlt recheck. Records always live on the CPU that took the
/// interrupt, so each CPU only ever needs to check itself before sleeping.
pub fn any_pending_self() -> bool {
    SLOTS[percpu::cpu_id() as usize]
        .0
        .iter()
        .any(|slot| slot.load(Ordering::Relaxed) != 0)
}
