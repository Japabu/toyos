//! Kernel event tracing — per-CPU ring buffer of scheduler/timer/IRQ events.
//!
//! Designed to be dumped from LLDB on a wedged kernel. Writer is single-CPU (the
//! ring belongs to the current CPU) but must tolerate interrupt recursion, so
//! slot allocation uses `fetch_add` on the head index. Event payload writes are
//! non-atomic — a reader may observe a torn record for the most recent entry,
//! which is acceptable for a debug tool.
//!
//! Symbol `TRACE_RINGS` is the array of per-CPU rings. From LLDB:
//!   (lldb) p &TRACE_RINGS
//!   (lldb) memory read --size 8 --count 2 <head_addr>
//!
//! Layer 2 of the diagnostics roadmap (see CLAUDE.md). Layer 3 (RIP sampling)
//! builds on this ring once in-kernel call-stack unwinding is available.
//!
//! # Relationship to `toyos_sched::hw::TraceEvent`
//!
//! Spec §10.4 asks for one trace format shared by the kernel and the
//! simulator, so a QEMU capture replays host-side. The two are *one
//! vocabulary, two representations*, and deliberately so:
//!
//! * `hw::TraceEvent` is the vocabulary — a closed Rust enum the simulator
//!   holds by value and asserts on. It has no layout guarantee, so it cannot
//!   be the thing on the wire.
//! * [`Record`] below is the wire form — `repr(C)`, 24 bytes, discriminants
//!   fixed by hand — because the readers are `memory read` in LLDB and, later,
//!   `toyos-sched-sim replay --from-qemu`. Neither can parse a Rust enum.
//!
//! [`record`] is the total mapping from the first onto the second, and is what
//! [`crate::hw::KernelHw`] installs as `Machine::trace`. The ring also keeps
//! kinds the core cannot produce ([`Kind::IrqDrain`], [`Kind::TimerArm`],
//! [`Kind::Preempt`]) — kernel observations from below the boundary.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use toyos_sched::hw::{TraceEvent, TraceKind};

use crate::arch::percpu;

pub const MAX_CPUS: usize = 8;
pub const RING_CAPACITY: usize = 4096;

/// Event kind discriminant. Stable — do not reorder (LLDB and the replay
/// tool read by numeric value).
///
/// 1..=13 are the kernel's own observations. 14.. are the `hw::TraceKind`
/// variants with no kernel-native counterpart, which arrive through
/// [`record`].
#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A task was picked and dispatched. `pid`/`tid` name the *incoming*
    /// task; `data` = 0 — the quantum is in the `TimerArm` that follows.
    SchedPick   = 1,
    SchedIdle   = 2,  // data = next_deadline_ms_low
    Preempt     = 3,  // data = 0
    Block       = 4,  // data = 0
    Wake        = 5,  // data = 0
    TimerArm    = 6,  // data = nanos (low 32)
    TimerStop   = 7,  // data = 0
    TimerFire   = 8,  // data = 0
    Mark        = 9,  // data = user-defined
    IdleEnter   = 10, // data = 0 — about to halt; the armed deadline is in TimerArm
    IdleExit    = 11, // data = 0 — ring observed cpu woke from halt
    /// Burst summary of Ring 0 timer fires since the previous kernel→user
    /// transition. `data` = number of fires. Emitted once per
    /// `kernel_exit_to_user_check` so a long demand-paging burst doesn't
    /// drown the ring buffer.
    TimerFireBurst = 12,
    /// An `irq_ring` record was consumed. `data` = IrqSource discriminant in
    /// the top byte, IRQ→service latency (µs, saturated) in the low 24 bits —
    /// the observable form of B10's completion-delivery delay. Distinct from
    /// [`Kind::Irq`], which is entry rather than consumption.
    IrqDrain = 13,
    /// Two-phase wait commit parked the task (spec §8.1).
    ParkCommit = 14,
    /// `data` = destination cpu id.
    Migrate = 15,
    Adopt = 16,
    Retire = 17,
    /// The core observed an interrupt. `data` = 0.
    Irq = 18,
}

/// The wire record. 24 bytes, `repr(C)`; field order chosen so an LLDB
/// hexdump is easy to read.
#[repr(C)]
pub struct Record {
    pub timestamp_ns: u64,
    pub kind: u16,
    pub cpu: u8,
    pub _pad: u8,
    pub pid: u32,
    pub tid: u32,
    pub data: u32,
}

const _: () = assert!(core::mem::size_of::<Record>() == 24);

const EMPTY_EVENT: Record = Record {
    timestamp_ns: 0,
    kind: 0,
    cpu: 0,
    _pad: 0,
    pid: 0,
    tid: 0,
    data: 0,
};

#[repr(C, align(64))]
pub struct TraceRing {
    pub head: AtomicU64, // monotonic; slot index = head % RING_CAPACITY
    pub events: UnsafeCell<[Record; RING_CAPACITY]>,
}

// SAFETY: writer uses atomic slot allocation; torn reads on the most recent
// entry are acceptable for a debug ring.
unsafe impl Sync for TraceRing {}

impl TraceRing {
    const fn new() -> Self {
        Self {
            head: AtomicU64::new(0),
            events: UnsafeCell::new([EMPTY_EVENT; RING_CAPACITY]),
        }
    }
}

#[no_mangle]
pub static TRACE_RINGS: [TraceRing; MAX_CPUS] = [
    TraceRing::new(), TraceRing::new(), TraceRing::new(), TraceRing::new(),
    TraceRing::new(), TraceRing::new(), TraceRing::new(), TraceRing::new(),
];

/// Globally enable/disable tracing. Starts off; set to true once clock is up.
static ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn enable() {
    ENABLED.store(true, Ordering::Release);
}

/// Record a trace event on the current CPU. Wait-free, safe from any context
/// (including interrupt handlers). No-op until `enable()` has been called.
#[inline]
pub fn trace(kind: Kind, data: u32) {
    let tid = percpu::current_tid().map_or(u32::MAX, |t| t.raw());
    let pid = percpu::current_pid().map_or(u32::MAX, |p| p.raw());
    push(percpu::cpu_id(), crate::clock::nanos_since_boot(), kind, pid, tid, data);
}

/// Write one [`Record`] into `cpu`'s ring.
///
/// `cpu` is a parameter rather than an ambient `cpu_id()` read because the
/// scheduler core carries CPU identity in the event (spec §10.1) — and
/// because the ring's single-writer property is per-CPU, so the caller
/// naming the wrong one is the only way to break it.
#[inline]
fn push(cpu: u32, timestamp_ns: u64, kind: Kind, pid: u32, tid: u32, data: u32) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let cpu = cpu as usize;
    if cpu >= MAX_CPUS { return; }
    let ring = &TRACE_RINGS[cpu];
    let slot = ring.head.fetch_add(1, Ordering::Relaxed) as usize % RING_CAPACITY;

    // SAFETY: single-CPU writer; the atomic fetch_add guarantees each IRQ-vs-
    // kernel writer gets a distinct slot on this CPU.
    unsafe {
        let slot_ptr = (*ring.events.get()).as_mut_ptr().add(slot);
        core::ptr::write(slot_ptr, Record {
            timestamp_ns,
            kind: kind as u16,
            cpu: cpu as u8,
            _pad: 0,
            pid,
            tid,
            data,
        });
    }
}

/// Encode a scheduler-core event into the ring. `Machine::trace`'s body.
///
/// Total by construction — no wildcard arm, so a new core variant is a
/// compile error here rather than a silently dropped event.
pub fn record(ev: TraceEvent) {
    let (kind, task, data) = match ev.kind {
        TraceKind::Schedule { task } => (Kind::SchedPick, Some(task), 0),
        TraceKind::Wake { task } => (Kind::Wake, Some(task), 0),
        TraceKind::Block { task } => (Kind::Block, Some(task), 0),
        TraceKind::ParkCommit { task } => (Kind::ParkCommit, Some(task), 0),
        TraceKind::Migrate { task, to } => (Kind::Migrate, Some(task), to.0),
        TraceKind::Adopt { task } => (Kind::Adopt, Some(task), 0),
        TraceKind::Retire { task } => (Kind::Retire, Some(task), 0),
        TraceKind::IdleEnter => (Kind::IdleEnter, None, 0),
        TraceKind::IdleExit => (Kind::IdleExit, None, 0),
        TraceKind::Irq => (Kind::Irq, None, 0),
        TraceKind::TimerFire => (Kind::TimerFire, None, 0),
    };
    // A `TaskKey` is a packed `TaskId` (pid in the high half) — the same
    // encoding `TaskId::pack` writes. Events with no task carry whatever is
    // loaded on the CPU, which for an idle-enter is nothing.
    let (pid, tid) = match task {
        Some(key) => ((key.0 >> 32) as u32, key.0 as u32),
        None => (
            percpu::current_pid().map_or(u32::MAX, |p| p.raw()),
            percpu::current_tid().map_or(u32::MAX, |t| t.raw()),
        ),
    };
    push(ev.cpu.0, ev.ts.0, kind, pid, tid, data);
}

/// Record consumption of an `irq_ring` record (see [`Kind::IrqDrain`]).
pub fn trace_irq_drain(source: crate::irq_ring::IrqSource, latency_us: u64) {
    let data = ((source as u32) << 24) | (latency_us.min(0x00FF_FFFF) as u32);
    trace(Kind::IrqDrain, data);
}
