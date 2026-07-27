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

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::percpu;
use crate::scheduler::EventSource;

pub const MAX_CPUS: usize = 8;
pub const RING_CAPACITY: usize = 4096;

/// Event kind discriminant. Stable — do not reorder (LLDB dumps by numeric value).
#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TraceKind {
    SchedPick   = 1,  // data = quantum_ns (low 32)
    SchedIdle   = 2,  // data = next_deadline_ms_low
    Preempt     = 3,  // data = 0
    Block       = 4,  // data = event_source_tag (see `event_source_tag`)
    Wake        = 5,  // data = event_source_tag
    TimerArm    = 6,  // data = nanos (low 32)
    TimerStop   = 7,  // data = 0
    TimerFire   = 8,  // data = 0
    Mark        = 9,  // data = user-defined
    IdleHalt    = 10, // data = next_deadline_ms_low (0 = stop_timer)
    IdleWake    = 11, // data = 0 — ring observed cpu woke from halt
    /// Burst summary of Ring 0 timer fires since the previous kernel→user
    /// transition. `data` = number of fires. Emitted once per
    /// `kernel_exit_to_user_check` so a long demand-paging burst doesn't
    /// drown the ring buffer.
    TimerFireBurst = 12,
    /// An `irq_ring` record was consumed. `data` = IrqSource discriminant in
    /// the top byte, IRQ→service latency (µs, saturated) in the low 24 bits —
    /// the observable form of B10's completion-delivery delay.
    IrqDrain = 13,
}

/// 24 bytes. Field order chosen so LLDB hexdump is easy to read.
#[repr(C)]
pub struct TraceEvent {
    pub timestamp_ns: u64,
    pub kind: u16,
    pub cpu: u8,
    pub _pad: u8,
    pub pid: u32,
    pub tid: u32,
    pub data: u32,
}

const _: () = assert!(core::mem::size_of::<TraceEvent>() == 24);

const EMPTY_EVENT: TraceEvent = TraceEvent {
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
    pub events: UnsafeCell<[TraceEvent; RING_CAPACITY]>,
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
pub fn trace(kind: TraceKind, data: u32) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let cpu = percpu::cpu_id() as usize;
    if cpu >= MAX_CPUS { return; }
    let ring = &TRACE_RINGS[cpu];
    let slot = ring.head.fetch_add(1, Ordering::Relaxed) as usize % RING_CAPACITY;

    let tid = percpu::current_tid().map_or(u32::MAX, |t| t.raw());
    let pid = percpu::current_pid().map_or(u32::MAX, |p| p.raw());

    // SAFETY: single-CPU writer; the atomic fetch_add guarantees each IRQ-vs-
    // kernel writer gets a distinct slot on this CPU.
    unsafe {
        let slot_ptr = (*ring.events.get()).as_mut_ptr().add(slot);
        core::ptr::write(slot_ptr, TraceEvent {
            timestamp_ns: crate::clock::nanos_since_boot(),
            kind: kind as u16,
            cpu: cpu as u8,
            _pad: 0,
            pid,
            tid,
            data,
        });
    }
}

/// Record consumption of an `irq_ring` record (see `TraceKind::IrqDrain`).
pub fn trace_irq_drain(source: crate::irq_ring::IrqSource, latency_us: u64) {
    let data = ((source as u32) << 24) | (latency_us.min(0x00FF_FFFF) as u32);
    trace(TraceKind::IrqDrain, data);
}

/// Pack an EventSource into a u32 tag for the `data` field of Block/Wake events.
/// Top byte = variant tag, low 24 bits = best-effort id (or 0 for singletons).
pub fn event_source_tag(e: &EventSource) -> u32 {
    match e {
        EventSource::Keyboard        => 0x01_000000,
        EventSource::Mouse           => 0x02_000000,
        EventSource::Network         => 0x03_000000,
        EventSource::Listener(id)    => 0x04_000000 | ((id.raw() as u32) & 0x00FF_FFFF),
        EventSource::PipeReadable(p) => 0x05_000000 | ((p.raw() as u32) & 0x00FF_FFFF),
        EventSource::PipeWritable(p) => 0x06_000000 | ((p.raw() as u32) & 0x00FF_FFFF),
        EventSource::Audio           => 0x07_000000,
        EventSource::Futex(_)        => 0x08_000000,
        EventSource::IoUring(r)      => 0x09_000000 | ((r.raw() as u32) & 0x00FF_FFFF),
    }
}

fn kind_name(k: u16) -> &'static str {
    match k {
        1 => "SchedPick",
        2 => "SchedIdle",
        3 => "Preempt",
        4 => "Block",
        5 => "Wake",
        6 => "TimerArm",
        7 => "TimerStop",
        8 => "TimerFire",
        9 => "Mark",
        10 => "IdleHalt",
        11 => "IdleWake",
        12 => "TimerBurst",
        13 => "IrqDrain",
        _ => "?",
    }
}

/// Dump the last `n` events across all CPUs to the serial log, ordered by
/// timestamp. Safe to call from panic context. `n=0` dumps everything.
pub fn dump(n: usize) {
    // Snapshot each ring's head and determine how many events to collect.
    let mut heads = [0u64; MAX_CPUS];
    let cpu_count = crate::arch::smp::cpu_count() as usize;
    for i in 0..cpu_count.min(MAX_CPUS) {
        heads[i] = TRACE_RINGS[i].head.load(Ordering::Acquire);
    }

    // Collect a flat view of recent events per CPU. Avoid heap allocation —
    // print each CPU's stream separately, caller can sort offline if needed.
    crate::log!("=== TRACE DUMP ({}) ===", if n == 0 { "all" } else { "recent" });
    for cpu in 0..cpu_count.min(MAX_CPUS) {
        let head = heads[cpu];
        if head == 0 {
            continue;
        }
        let total = head.min(RING_CAPACITY as u64) as usize;
        let count = if n == 0 { total } else { n.min(total) };
        let ring = &TRACE_RINGS[cpu];
        let events = unsafe { &*ring.events.get() };

        crate::log!("  cpu {}: head={} dumping {}", cpu, head, count);
        let start = head - count as u64;
        for i in 0..count {
            let idx = ((start + i as u64) as usize) % RING_CAPACITY;
            let e = &events[idx];
            crate::log!(
                "    t={}.{:06} cpu{} {:<10} pid={} tid={} data={:#x}",
                e.timestamp_ns / 1_000_000_000,
                (e.timestamp_ns % 1_000_000_000) / 1000,
                e.cpu,
                kind_name(e.kind),
                e.pid as i32,
                e.tid as i32,
                e.data,
            );
        }
    }
    crate::log!("=== END TRACE ===");
}
