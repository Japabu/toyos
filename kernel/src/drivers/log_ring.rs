//! Non-blocking kernel log buffer.
//!
//! `log!()` must never block while holding scheduler / mm / IPC locks: the
//! UART path stalls per byte and the virtio-console path waits for a host
//! roundtrip, so doing serial I/O under e.g. `cpus[N]` serializes the entire
//! kernel on host throughput. Even ~100 lines/sec is enough to freeze the
//! system because every line stops the world for milliseconds.
//!
//! This module decouples the two phases:
//!   - `write_chunk` — fast-path append, taken under a CLI-aware spinlock.
//!     Pure memcpy + head/tail bookkeeping. Microseconds.
//!   - `drain_to_serial` — runs off the critical path (idle loop, panic).
//!     Holds the backend lock for the slow I/O; the ring lock is only taken
//!     briefly to copy bytes out, so concurrent log!() calls don't block on
//!     I/O.
//!
//! The ring is a fixed 64 KiB byte buffer. For `log!()` overflow drops the
//! *oldest* bytes — debug logs in a frozen kernel are almost always more
//! useful at the head than at the tail — and every drain reports the
//! accumulated drop count so loss is never silent. Userland console output
//! (`write_chunk_blocking`) instead throttles on a full ring, draining
//! synchronously: the console is the test-harness protocol channel and must
//! be lossless, like the pre-ring synchronous UART writes were.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const RING_SIZE: usize = 64 * 1024;

struct LogRing {
    buf: [u8; RING_SIZE],
    head: usize,
    tail: usize,
    len: usize,
}

impl LogRing {
    const fn new() -> Self {
        Self { buf: [0; RING_SIZE], head: 0, tail: 0, len: 0 }
    }

    fn append(&mut self, data: &[u8]) -> usize {
        let mut dropped = 0;
        for &b in data {
            if self.len == RING_SIZE {
                self.tail = (self.tail + 1) % RING_SIZE;
                self.len -= 1;
                dropped += 1;
            }
            self.buf[self.head] = b;
            self.head = (self.head + 1) % RING_SIZE;
            self.len += 1;
        }
        dropped
    }

    fn drain_into(&mut self, out: &mut [u8]) -> usize {
        let n = self.len.min(out.len());
        for i in 0..n {
            out[i] = self.buf[self.tail];
            self.tail = (self.tail + 1) % RING_SIZE;
        }
        self.len -= n;
        n
    }
}

struct RingCell(UnsafeCell<LogRing>);
unsafe impl Sync for RingCell {}

static RING: RingCell = RingCell(UnsafeCell::new(LogRing::new()));
static RING_LOCKED: AtomicBool = AtomicBool::new(false);
static DROPPED_BYTES: AtomicU64 = AtomicU64::new(0);

/// CLI-aware spinlock for ring access. Same pattern as the legacy
/// `serial::LOCKED` — log!() can be called from IRQ handlers, so the
/// spinning side must run with interrupts disabled to prevent same-CPU
/// re-entry deadlock.
struct RingGuard {
    rflags: u64,
}

impl RingGuard {
    fn lock() -> Self {
        let rflags: u64;
        unsafe {
            core::arch::asm!(
                "pushfq",
                "pop {}",
                "cli",
                out(reg) rflags,
                options(nomem),
            );
        }
        while RING_LOCKED
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while RING_LOCKED.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        Self { rflags }
    }

    #[inline]
    fn ring(&mut self) -> &mut LogRing {
        unsafe { &mut *RING.0.get() }
    }
}

impl Drop for RingGuard {
    fn drop(&mut self) {
        RING_LOCKED.store(false, Ordering::Release);
        unsafe {
            core::arch::asm!(
                "push {}",
                "popfq",
                in(reg) self.rflags,
                options(nomem),
            );
        }
    }
}

/// Append bytes to the ring. Fast — memcpy under a brief CLI spinlock.
/// On overflow, oldest bytes are dropped; count accumulates in `DROPPED_BYTES`.
pub fn write_chunk(bytes: &[u8]) {
    let mut g = RingGuard::lock();
    let dropped = g.ring().append(bytes);
    drop(g);
    if dropped > 0 {
        DROPPED_BYTES.fetch_add(dropped as u64, Ordering::Relaxed);
    }
}

/// Append console bytes without loss: when the ring cannot take `bytes`,
/// synchronously drain a chunk to the backend and retry, bounding userland
/// console writes to backend speed instead of silently discarding output.
/// `log!()` keeps the lossy `write_chunk` — it must never do I/O, because it
/// runs under arbitrary kernel locks. Draining one bounded chunk per backend
/// acquisition keeps each interrupts-off window short.
///
/// Syscall context only: the caller must not hold the ring or backend locks.
pub fn write_chunk_blocking(bytes: &[u8]) {
    assert!(bytes.len() <= RING_SIZE);
    loop {
        {
            let mut g = RingGuard::lock();
            let ring = g.ring();
            if RING_SIZE - ring.len >= bytes.len() {
                ring.append(bytes);
                return;
            }
        }
        let mut backend = crate::drivers::serial::BackendGuard::lock();
        drain_chunk_to_serial(&mut backend);
    }
}

/// Drain everything currently buffered into the serial backend. Reads from
/// the ring under the ring lock briefly, releases, then writes to the
/// backend under `backend` — log!() callers don't block on the I/O.
///
/// Caller must hold a `BackendGuard` to serialize backend access.
pub fn drain_to_serial(backend: &mut crate::drivers::serial::BackendGuard) {
    let mut buf = [0u8; 4096];
    loop {
        let n = {
            let mut g = RingGuard::lock();
            g.ring().drain_into(&mut buf)
        };
        if n == 0 {
            break;
        }
        backend.write_raw(&buf[..n]);
    }
    report_dropped(backend);
}

/// Drain at most one 512-byte chunk. For bounded-latency callers: the timer
/// ISR's pre-preemption path and the idle loop, which drops the (IRQs-off)
/// `BackendGuard` between chunks. Returns the number of bytes drained so
/// loop callers know when the ring is empty.
///
/// Caller must hold a `BackendGuard` to serialize backend access.
pub fn drain_chunk_to_serial(backend: &mut crate::drivers::serial::BackendGuard) -> usize {
    let mut buf = [0u8; 512];
    let n = {
        let mut g = RingGuard::lock();
        g.ring().drain_into(&mut buf)
    };
    if n > 0 {
        backend.write_raw(&buf[..n]);
    }
    report_dropped(backend);
    n
}

fn report_dropped(backend: &mut crate::drivers::serial::BackendGuard) {
    let mut marker = [0u8; DROP_MARKER_MAX];
    let n = take_drop_marker(&mut marker);
    if n > 0 {
        backend.write_raw(&marker[..n]);
    }
}

pub const DROP_MARKER_MAX: usize = 64;

/// Take the accumulated overflow-drop count and render it as a marker line.
/// Returns 0 when nothing was dropped. Every drain path emits this so ring
/// overflow is visible on the host instead of silently eating logs.
pub fn take_drop_marker(out: &mut [u8; DROP_MARKER_MAX]) -> usize {
    let dropped = DROPPED_BYTES.swap(0, Ordering::Relaxed);
    if dropped == 0 {
        return 0;
    }
    let mut digits = [0u8; 20];
    let mut ndigits = 0;
    let mut v = dropped;
    loop {
        digits[ndigits] = b'0' + (v % 10) as u8;
        ndigits += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let mut len = 0;
    for &b in b"\n[log_ring: " {
        out[len] = b;
        len += 1;
    }
    while ndigits > 0 {
        ndigits -= 1;
        out[len] = digits[ndigits];
        len += 1;
    }
    for &b in b" bytes dropped]\n" {
        out[len] = b;
        len += 1;
    }
    len
}

/// Read up to `out.len()` bytes from the ring without taking the lock.
///
/// # Safety
/// Panic-path last resort. The halt IPI is an ordinary maskable vector, so
/// a sibling inside a ring critical section (IF=0) may still be running:
/// callers must first wait for a clean lock handoff (see
/// `serial::panic_flush`) and bypass only when the holder is wedged. The
/// indices may then be torn — the clamps keep garbage from spinning the
/// panic path forever.
pub unsafe fn drain_unlocked(out: &mut [u8]) -> usize {
    let ring = unsafe { &mut *RING.0.get() };
    if ring.len > RING_SIZE {
        ring.len = RING_SIZE;
    }
    if ring.tail >= RING_SIZE {
        ring.tail = 0;
    }
    ring.drain_into(out)
}
