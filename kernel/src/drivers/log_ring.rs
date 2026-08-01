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
//! synchronously: the console is the test-harness protocol channel and must be
//! lossless.
//!
//! **A drain moves the serial cursor; it does not erase.** `tail`/`len` are
//! what serial still owes the host; `retained` is what is still *readable*
//! behind `head`, and only a wrap past 64 KiB shortens it. The difference is
//! the on-screen console's whole existence: it has no cursor of its own, and
//! before this it read `len` — so on a machine with a UART the idle loop
//! emptied the ring within milliseconds and a fatal panic painted the one line
//! logged since the last drain, while on a machine *without* one the drain
//! threw the boot log into a backend that discards it. Both are the same bug
//! from opposite ends, and neither is visible to a host that has the serial
//! stream anyway.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

const RING_SIZE: usize = 64 * 1024;

/// Bytes a drain moves per pass, and the size of the stack buffer it does it
/// through.
///
/// It was 4096, and the buffer is a local on the panic path — which on a
/// double fault runs on IST1, a 4096-byte stack. Measured with `ist1_report`:
/// the report used **9968 bytes**, so it ran 5872 bytes past the end of the
/// stack and into the heap underneath while writing the explanation for the
/// fault that had just happened. (Known issues estimated ~1.4 KiB; it was
/// four times that.)
///
/// 512 is what `drain_chunk_to_serial` already used for the bounded-latency
/// callers, so this is one number where there were two. The cost is 128 passes
/// over a full ring instead of 16, each a brief lock and a memcpy — nothing
/// against a per-byte UART wait.
pub const DRAIN_CHUNK: usize = 512;

struct LogRing {
    buf: [u8; RING_SIZE],
    head: usize,
    /// Where the next drain reads from, and `len` how much it owes. Both are
    /// serial's bookkeeping alone.
    tail: usize,
    len: usize,
    /// The same pair for the file sink (`esp_log`). Two consumers, two
    /// cursors, one buffer: serial and the boot volume are at different points
    /// in the stream and neither may consume the other's bytes. Only advanced
    /// while [`FILE_SINK`] is set, so a machine with no `/boot` pays nothing
    /// for them and can never report bytes pending that nobody will collect.
    file_tail: usize,
    file_len: usize,
    /// Bytes behind `head` that have not been overwritten, so at most
    /// `RING_SIZE`. Independent of draining, which is what makes it the
    /// window the on-screen console reads.
    retained: usize,
}

impl LogRing {
    const fn new() -> Self {
        Self { buf: [0; RING_SIZE], head: 0, tail: 0, len: 0, file_tail: 0, file_len: 0, retained: 0 }
    }

    fn append(&mut self, data: &[u8]) -> usize {
        let mut dropped = 0;
        let mut file_dropped = 0u64;
        let to_file = FILE_SINK.load(Ordering::Relaxed);
        for &b in data {
            if self.len == RING_SIZE {
                self.tail = (self.tail + 1) % RING_SIZE;
                self.len -= 1;
                dropped += 1;
            }
            if to_file && self.file_len == RING_SIZE {
                self.file_tail = (self.file_tail + 1) % RING_SIZE;
                self.file_len -= 1;
                file_dropped += 1;
            }
            self.buf[self.head] = b;
            self.head = (self.head + 1) % RING_SIZE;
            self.len += 1;
            if to_file {
                self.file_len += 1;
            }
            if self.retained < RING_SIZE {
                self.retained += 1;
            }
        }
        OWED.store(self.len, Ordering::Relaxed);
        if to_file {
            FILE_OWED.store(self.file_len, Ordering::Relaxed);
        }
        if file_dropped > 0 {
            FILE_DROPPED.fetch_add(file_dropped, Ordering::Relaxed);
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
        OWED.store(self.len, Ordering::Relaxed);
        n
    }

    fn drain_into_file(&mut self, out: &mut [u8]) -> usize {
        let n = self.file_len.min(out.len());
        for i in 0..n {
            out[i] = self.buf[self.file_tail];
            self.file_tail = (self.file_tail + 1) % RING_SIZE;
        }
        self.file_len -= n;
        FILE_OWED.store(self.file_len, Ordering::Relaxed);
        n
    }
}

struct RingCell(UnsafeCell<LogRing>);
unsafe impl Sync for RingCell {}

static RING: RingCell = RingCell(UnsafeCell::new(LogRing::new()));
static RING_LOCKED: AtomicBool = AtomicBool::new(false);
static DROPPED_BYTES: AtomicU64 = AtomicU64::new(0);
/// A lock-free mirror of `LogRing::len` — what serial still owes the host.
///
/// It exists for exactly one reader: the pre-halt recheck in
/// `sched::driver::execute`, which runs with interrupts off and must not take
/// the ring lock there. Written only under `RingGuard`, by the two methods that
/// move `len`, so it cannot drift from them.
static OWED: AtomicUsize = AtomicUsize::new(0);

/// Does serial still owe the host bytes? Lock-free, for the interrupts-off
/// pre-halt check; see [`OWED`].
pub fn has_pending() -> bool {
    OWED.load(Ordering::Relaxed) != 0
}

/// Whether `esp_log` is collecting from this ring at all.
///
/// False until a `/boot` exists and `esp_log::install` runs, and false again
/// the moment that sink gives up — which is what keeps `file_has_pending`
/// from reporting bytes owed to a consumer that no longer exists, and the idle
/// loop from declining to sleep on them forever.
static FILE_SINK: AtomicBool = AtomicBool::new(false);
static FILE_OWED: AtomicUsize = AtomicUsize::new(0);
static FILE_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Does the file sink still owe the boot volume bytes? Same shape and same
/// reason as [`has_pending`]: read from the pre-halt check with interrupts off.
pub fn file_has_pending() -> bool {
    FILE_OWED.load(Ordering::Relaxed) != 0
}

/// Start collecting for the file sink, from the oldest byte still readable.
///
/// Seeded from `retained` rather than from `head`, so the file opens with this
/// boot's log from its first line rather than from the moment `/boot` mounted
/// — which is four phases in, and exactly the part a machine that dies early
/// needs.
pub fn enable_file_sink() {
    let mut g = RingGuard::lock();
    let ring = g.ring();
    ring.file_len = ring.retained;
    ring.file_tail = (ring.head + RING_SIZE - ring.retained) % RING_SIZE;
    FILE_OWED.store(ring.file_len, Ordering::Relaxed);
    FILE_SINK.store(true, Ordering::Relaxed);
}

/// Stop collecting, and forget what was owed.
pub fn disable_file_sink() {
    FILE_SINK.store(false, Ordering::Relaxed);
    let mut g = RingGuard::lock();
    g.ring().file_len = 0;
    FILE_OWED.store(0, Ordering::Relaxed);
}

/// Move up to `out.len()` of the bytes the file sink is owed. Thread context
/// only: the caller writes them to a disk.
pub fn drain_to_file(out: &mut [u8]) -> usize {
    let mut g = RingGuard::lock();
    g.ring().drain_into_file(out)
}

/// Bytes the file sink lost to ring overflow since the last call.
///
/// Reported rather than silent, for the reason `take_drop_marker` exists: a
/// log with a hole in it and nothing saying so is worse than one that says
/// where the hole is.
pub fn take_file_drops() -> u64 {
    FILE_DROPPED.swap(0, Ordering::Relaxed)
}

/// CLI-aware spinlock for ring access. `log!()` can be called from IRQ
/// handlers, so the spinning side must run with interrupts disabled to prevent
/// same-CPU re-entry deadlock.
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
    let mut buf = [0u8; DRAIN_CHUNK];
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
    let mut buf = [0u8; DRAIN_CHUNK];
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
    // `retained` is deliberately not touched: this is the panic path's last
    // resort and the on-screen console reads the ring *after* it runs.
    ring.drain_into(out)
}

/// Copy the newest `out.len()` retained bytes out of the ring without taking
/// the lock and without consuming them. Returns the number copied.
///
/// Reads `retained`, not `len`: what the console shows must not depend on
/// whether serial happens to have collected it already.
///
/// Strictly weaker than `drain_unlocked`: it takes no lock for the same
/// reason, but it mutates nothing at all — not the cursors, not the clamps.
/// A torn `head`/`len` therefore costs a garbled line or two and can never
/// leave the ring in a state that changes what a later drain reports. That is
/// what makes it callable from the panic path *ahead* of `panic_flush`
/// without perturbing the serial report.
///
/// # Safety
/// Takes no lock, so concurrent `append`s may be in flight and the bytes near
/// `head` can be a mixture of two lines. Every index is masked, so the read
/// stays in bounds whatever the cursors do; the caller must simply tolerate a
/// torn result. That is why the boot checkpoints call it with interrupts
/// enabled and IRQ handlers logging: a torn line on screen, nothing more.
pub unsafe fn peek_tail(out: &mut [u8]) -> usize {
    let ring = unsafe { &*RING.0.get() };
    let len = ring.retained.min(RING_SIZE);
    let head = if ring.head >= RING_SIZE { 0 } else { ring.head };
    let n = len.min(out.len());
    let start = (head + RING_SIZE - n) % RING_SIZE;
    for (i, slot) in out[..n].iter_mut().enumerate() {
        *slot = ring.buf[(start + i) % RING_SIZE];
    }
    n
}
