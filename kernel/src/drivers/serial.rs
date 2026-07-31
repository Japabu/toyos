//! Kernel serial console.
//!
//! Two responsibilities, kept apart so that `log!()` never blocks while
//! holding kernel locks:
//!
//! 1. **`SerialWriter`** — the formatter `log!()` writes into. Buffers
//!    bytes on the stack and atomically commits to `log_ring` on Drop.
//!    No spinlock, no CLI: each `log!()` invocation owns its own
//!    independent buffer, so multiple CPUs (and IRQ-vs-thread on the
//!    same CPU) can format concurrently with no interaction.
//!
//! 2. **`BackendGuard`** — exclusive access to the underlying serial
//!    backend (virtio-console or 16550 UART). CLI + global spinlock,
//!    held only by drain (`log_ring::drain_to_serial`), input polling,
//!    and panic flush. The slow I/O happens here, off the critical path.
//!
//! See [`log_ring`](super::log_ring) for the buffering rationale.

use core::sync::atomic::{AtomicBool, Ordering};
use crate::arch::cpu::{inb, outb};
use crate::log;

const PORT: u16 = 0x3f8; // COM1

/// Whether a 16550 answered the loopback probe in `init`.
///
/// Modern laptops have no SuperIO, so every port read returns `0xFF`. That
/// is indistinguishable from a UART reporting "receiver ready, data = 0xFF",
/// which would feed the console an endless stream of 0xFF input bytes. The
/// probe is the only place the difference is observable, so it is latched
/// here and every UART access is gated on it.
static UART_PRESENT: AtomicBool = AtomicBool::new(false);

pub fn init() {
    outb(PORT + 1, 0x00); // Disable all interrupts
    outb(PORT + 3, 0x80); // Enable DLAB (set baud rate divisor)
    outb(PORT + 0, 0x03); // Set divisor to 3 (lo byte) 38400 baud
    outb(PORT + 1, 0x00); //                  (hi byte)
    outb(PORT + 3, 0x03); // 8 bits, no parity, one stop bit
    outb(PORT + 2, 0xC7); // Enable FIFO, clear them, with 14-byte threshold
    outb(PORT + 4, 0x0B); // IRQs enabled, RTS/DSR set
    outb(PORT + 4, 0x1E); // Set in loopback mode, test the serial chip
    outb(PORT + 0, 0xAE); // Test serial chip (send byte 0xAE and check if serial returns same byte)
    let loopback = inb(PORT + 0);
    UART_PRESENT.store(loopback == 0xAE, Ordering::Relaxed);
    outb(PORT + 4, 0x0F); // Normal operation mode
    // The byte, not just the verdict. Replacing the old assert with a silent
    // latch collapsed three different situations into one `false`: no SuperIO
    // at all (0xFF), a chip that answered wrongly, and the right chip at the
    // wrong port. They want different next steps, and on a machine with no
    // serial output this line is the difference — it still reaches the
    // virtio-console and the on-screen console.
    log!(
        "serial: 16550 loopback read {:#04x} ({})",
        loopback,
        if loopback == 0xAE { "present" } else { "absent or wrong port" }
    );
}

pub fn uart_present() -> bool {
    UART_PRESENT.load(Ordering::Relaxed)
}

// Backend access — slow path, used by drain / input / panic.

static BACKEND_LOCKED: AtomicBool = AtomicBool::new(false);

/// RAII handle for exclusive access to the serial backend (virtio-console
/// or UART). Disables interrupts because reads and writes touch device
/// state shared with poll callers; same-CPU re-entry from an IRQ handler
/// would otherwise deadlock the spin.
pub struct BackendGuard {
    rflags: u64,
}

impl BackendGuard {
    pub fn lock() -> Self {
        let rflags = save_and_cli();
        while BACKEND_LOCKED
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while BACKEND_LOCKED.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        Self { rflags }
    }

    /// Non-blocking acquire. Returns `None` if another CPU already holds
    /// the backend. For use in IRQ contexts that must not stall — caller
    /// can retry on the next tick.
    pub fn try_lock() -> Option<Self> {
        let rflags = save_and_cli();
        if BACKEND_LOCKED
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(Self { rflags })
        } else {
            unsafe { restore_flags(rflags); }
            None
        }
    }

    /// Write raw bytes directly to the backend, no escape stripping
    /// (already done before they hit the ring). Used by drain.
    pub fn write_raw(&mut self, bytes: &[u8]) {
        if super::virtio_console::is_ready() {
            super::virtio_console::write_bytes_locked(bytes);
        } else {
            uart_write_bytes(bytes);
        }
    }

    pub fn has_data(&self) -> bool {
        if super::virtio_console::is_ready() {
            super::virtio_console::has_data_locked()
        } else {
            uart_present() && inb(PORT + 5) & 0x01 != 0
        }
    }

    pub fn try_read_byte(&mut self) -> Option<u8> {
        if super::virtio_console::is_ready() {
            super::virtio_console::try_read_byte_locked()
        } else if uart_present() && inb(PORT + 5) & 0x01 != 0 {
            Some(inb(PORT))
        } else {
            None
        }
    }
}

impl Drop for BackendGuard {
    fn drop(&mut self) {
        BACKEND_LOCKED.store(false, Ordering::Release);
        unsafe { restore_flags(self.rflags); }
    }
}

#[inline]
fn save_and_cli() -> u64 {
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
    rflags
}

#[inline]
unsafe fn restore_flags(rflags: u64) {
    unsafe {
        core::arch::asm!(
            "push {}",
            "popfq",
            in(reg) rflags,
            options(nomem),
        );
    }
}

pub fn has_data() -> bool {
    let g = BackendGuard::lock();
    g.has_data()
}

pub fn try_read_byte() -> Option<u8> {
    let mut g = BackendGuard::lock();
    g.try_read_byte()
}

/// ~1s of pause-loop spins. Long enough for any live `BackendGuard` holder
/// to finish its drain and release; short enough that a wedged holder does
/// not hang the panic path.
const PANIC_LOCK_SPIN_LIMIT: u64 = 100_000_000;

/// Flush pending logs on the panic path.
///
/// The halt IPI is an ordinary maskable vector: a sibling holding the
/// backend (IF=0, e.g. mid idle-loop drain) keeps running until its guard
/// drops and only then halts — bypassing immediately would race its live
/// ring/virtqueue mutation and lose the panic message. So first wait
/// (bounded) for a clean handoff and drain through the normal locked path.
/// Only when the holder never releases (wedged in a virtio submit, or died
/// holding the lock) bypass — with virtio-console disabled, because its TX
/// queue may be left half-submitted (`tx_slot` taken) and a bypassing
/// writer would panic recursively on it; the UART is pure port-IO and
/// cannot wedge.
///
/// # Safety
/// Panic context only — on the bypass path the ring is read unsynchronized
/// (see `log_ring::drain_unlocked`).
pub unsafe fn panic_flush() {
    // No backend at all means every path below pops the ring into a writer
    // that discards it — `drain_to_serial` consumes first and finds out
    // second. The report is then gone from the one place still holding it,
    // the on-screen console included. This has to be checked before the
    // locked path, not after: that path is the common one.
    if !uart_present() && !super::virtio_console::is_ready() {
        return;
    }
    for _ in 0..PANIC_LOCK_SPIN_LIMIT {
        if let Some(mut g) = BackendGuard::try_lock() {
            super::log_ring::drain_to_serial(&mut g);
            return;
        }
        core::hint::spin_loop();
    }
    // The bypass disables virtio-console, so it can only write to the UART.
    if !uart_present() {
        return;
    }
    super::virtio_console::disable();
    let mut buf = [0u8; super::log_ring::DRAIN_CHUNK];
    loop {
        let n = unsafe { super::log_ring::drain_unlocked(&mut buf) };
        if n == 0 { break; }
        uart_write_bytes(&buf[..n]);
    }
    let mut marker = [0u8; super::log_ring::DROP_MARKER_MAX];
    let n = super::log_ring::take_drop_marker(&mut marker);
    if n > 0 {
        uart_write_bytes(&marker[..n]);
    }
}

// Formatter — fast path, used by every log!() invocation.

const SW_BUF_SIZE: usize = 1024;

/// Stack-buffered formatter for `log!()`. Accumulates bytes locally and
/// commits the whole buffer to `log_ring` on Drop. Strips ANSI CSI escape
/// sequences inline so the ring never holds bytes that would be dropped at
/// the backend.
///
/// Despite the name `lock()`, no lock is acquired — each invocation has
/// its own independent buffer. The name is preserved for the `log!` macro.
pub struct SerialWriter {
    buf: [u8; SW_BUF_SIZE],
    len: usize,
    lossless: bool,
}

impl SerialWriter {
    pub fn lock() -> Self {
        Self { buf: [0; SW_BUF_SIZE], len: 0, lossless: false }
    }

    /// Console-output variant: spills throttle on a full ring instead of
    /// dropping, so userland `write()` is lossless — the test harness protocol
    /// depends on it. Syscall context only; `log!()` must keep the lossy
    /// writer, since it runs under arbitrary kernel locks and must never do
    /// I/O.
    pub fn console() -> Self {
        Self { buf: [0; SW_BUF_SIZE], len: 0, lossless: true }
    }

    /// Spill the buffer to the ring and reset. Called when the buffer
    /// fills before Drop — log lines longer than `SW_BUF_SIZE` lose
    /// atomicity across the spill, which is acceptable for huge logs.
    fn spill(&mut self) {
        if self.len > 0 {
            if self.lossless {
                super::log_ring::write_chunk_blocking(&self.buf[..self.len]);
            } else {
                super::log_ring::write_chunk(&self.buf[..self.len]);
            }
            self.len = 0;
        }
    }

    fn push_byte(&mut self, b: u8) {
        if self.len == SW_BUF_SIZE {
            self.spill();
        }
        self.buf[self.len] = b;
        self.len += 1;
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                // Skip CSI escape sequence: ESC '[' ... <terminator 0x40-0x7E>
                i += 2;
                while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() { i += 1; }
            } else {
                self.push_byte(bytes[i]);
                i += 1;
            }
        }
    }
}

impl Drop for SerialWriter {
    fn drop(&mut self) {
        self.spill();
    }
}

impl core::fmt::Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write_bytes(s.as_bytes());
        Ok(())
    }
}

/// Spins per byte for the transmit-holding-register-empty bit, bounded.
///
/// The bound is not belt-and-braces. `uart_present()` says a 16550 answered a
/// loopback probe at boot, not that it is still draining: a UART wedged with
/// THRE clear — flow-controlled by a host that went away, or simply broken —
/// made this loop infinite, and it is on `panic_flush`'s bypass path, which is
/// the last thing standing when the backend lock holder is already wedged. So
/// the one mechanism designed for "everything else has failed" could itself
/// hang forever, on the machine where it matters most: a laptop, where nothing
/// is watching the console to notice. `panic_raw_uart` in `main.rs` has always
/// bounded its wait; this is the same bound, applied where the bytes actually
/// go. Losing a byte to a dead UART beats losing the machine to it.
const THRE_SPIN_LIMIT: u32 = 100_000;

fn uart_write_bytes(bytes: &[u8]) {
    if !uart_present() {
        return;
    }
    for &b in bytes {
        for _ in 0..THRE_SPIN_LIMIT {
            if inb(PORT + 5) & 0x20 != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        outb(PORT, b);
    }
}

/// Write straight to the 16550, bypassing the ring, the backend lock and the
/// virtio console.
///
/// For the two callers that have to report something *about* the machinery
/// they would otherwise report through: the panic-reentry line, and the IST1
/// stack verdict, which is meaningless if it travels through a ring that may
/// be what the overflow corrupted. No lock, no allocation, bounded per byte.
pub fn panic_raw(bytes: &[u8]) {
    uart_write_bytes(bytes);
}

/// `panic_raw` for a number, since the callers cannot format one.
pub fn panic_raw_dec(mut v: u64) {
    let mut digits = [0u8; 20];
    let mut n = 0;
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 || n == digits.len() {
            break;
        }
    }
    let mut out = [0u8; 20];
    for i in 0..n {
        out[i] = digits[n - 1 - i];
    }
    uart_write_bytes(&out[..n]);
}
