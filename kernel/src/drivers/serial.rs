//! The 16550 and the virtio-console, and the one lock that serialises them.
//!
//! **There is one thing here now where there were two.** `SerialWriter` was a
//! per-invocation stack buffer that every `log!` formatted into and committed
//! to a 64 KiB byte ring, which something else drained later; the ring is gone
//! and what reaches this file is whole units — a rendered record from
//! `log::console`, a userland `write`, a panic
//! report — each of which takes [`BackendGuard`] once and holds it for its own
//! whole unit. That is where line atomicity comes from, and it is the only
//! place it could come from: two producers of half-lines cannot be made
//! atomic by anything downstream of them.
//!
//! [`BackendGuard`] is CLI plus a global spinlock, so an interrupts-off window
//! is one unit long. The slow I/O happens inside it, which is why **every unit
//! is bounded and no holder may take its length from userland** — a rendered
//! record is one 1 KiB `LogRecord`, the live drain takes eight of them, the
//! panic report is a fixed buffer, and a `write` of arbitrary length is cut
//! into [`MAX_CONSOLE_LINE`] pieces by [`ConsoleLine`]. The one deliberate
//! exception is the panic path's `drain_locked`, which takes the whole backlog
//! under one hold — a machine that is dying pays latency to say why, and
//! `log/console.rs` argues it at the site. Nothing that holds a kernel lock
//! formats here.

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
    console_changed();
}

/// A backend has arrived, or the machine has switched to a better one.
///
/// Called from the two places [`backend`] can change its answer — this module's
/// probe, and virtio-console coming up in phase 6. What it does is
/// `log::console`'s and the argument lives there: everything said so far went
/// to whichever backend existed then, and the new one has heard none of it.
pub fn console_changed() {
    crate::log::console::backend_changed();
}

pub fn uart_present() -> bool {
    UART_PRESENT.load(Ordering::Relaxed)
}

/// Whether anything can carry a byte off this machine. False is the T14's
/// shape: the shards still fill and still hold their tails, but nothing drains
/// them off the machine, so the framebuffer is the only surface a diagnostic
/// can reach.
///
/// The predicate a caller wants before falling back to the screen, and the same
/// one [`panic_flush`] refuses on.
pub fn has_console() -> bool {
    !matches!(backend(), Backend::None)
}

/// Where a write goes right now.
///
/// **One answer, and [`BackendGuard::write_raw`] is written in terms of it**, so
/// the drain's "which backend has already heard this" question cannot disagree
/// with where the bytes actually went. The order is the preference: a
/// virtio-console is the host's own channel and a 16550 is what is left when
/// there is none.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Backend {
    /// Nothing can carry a byte off this machine. The T14's shape: records
    /// stay in their shards, where the panel can still read them.
    None = 0,
    Uart = 1,
    Virtio = 2,
}

pub fn backend() -> Backend {
    if super::virtio_console::is_ready() {
        Backend::Virtio
    } else if uart_present() {
        Backend::Uart
    } else {
        Backend::None
    }
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

    /// Write raw bytes straight to the backend, no escape stripping — the
    /// record drain's lines carry none, and a userland write is stripped by
    /// [`write_console`] before it gets here.
    pub fn write_raw(&mut self, bytes: &[u8]) {
        match backend() {
            Backend::Virtio => super::virtio_console::write_bytes_locked(bytes),
            Backend::Uart => uart_write_bytes(bytes),
            Backend::None => {}
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
/// Panic context only — on the bypass path the drain's position is read with no
/// lock held (see `log::console::drain_bypassed`).
pub unsafe fn panic_flush() {
    // No backend at all means every path below hands the report to a writer
    // that discards it, and the record drain would move its position over it.
    // The report is then gone from the one place still holding it, the
    // on-screen console included. This has to be checked before the locked
    // path, not after: that path is the common one.
    if !has_console() {
        return;
    }
    for _ in 0..PANIC_LOCK_SPIN_LIMIT {
        if let Some(mut g) = BackendGuard::try_lock() {
            crate::log::console::drain_locked(&mut g);
            return;
        }
        core::hint::spin_loop();
    }
    // The bypass disables virtio-console, so it can only write to the UART.
    if !uart_present() {
        return;
    }
    super::virtio_console::disable();
    // SAFETY: this is the bypass the function's own clause describes — a
    // bounded wait for a clean handoff has already failed, so the holder is
    // wedged and will not publish.
    unsafe { crate::log::console::drain_bypassed() };
}

/// Drain the ring before the machine stops.
///
/// `acpi::shutdown()` cuts power with whatever is still queued, so the tail of
/// every clean shutdown was unobservable — including the line that says how far
/// a filesystem sync got before it died, which is the one diagnostic a shutdown
/// failure has. On a machine with no serial there is no other channel at all.
///
/// Bounded on the lock rather than blocking, for the same reason `panic_flush`
/// is: a shutdown must not hang because another CPU is wedged holding the
/// backend. It does *not* take that function's bypass — every CPU is still live
/// here, and reading the ring unsynchronized is only defensible when nothing
/// else will ever run. Losing the tail is better than not powering off.
pub fn flush_final() {
    for _ in 0..PANIC_LOCK_SPIN_LIMIT {
        if let Some(mut g) = BackendGuard::try_lock() {
            crate::log::console::drain_locked(&mut g);
            return;
        }
        core::hint::spin_loop();
    }
}

/// A userland `write` to a console object, **unbuffered**.
///
/// **One backend acquisition per [`MAX_CONSOLE_LINE`] of output, ANSI stripped,
/// no buffering.** It replaced `SerialWriter::console()` and the lossless
/// byte-ring append underneath it, whose unit of interleaving was a `write`
/// syscall — and two measured splices to show for it, each a kernel record
/// landing inside a userland line: `hda_tone` red 1 of 3 on a loaded dev host
/// with `soundd: hda codec0 vendor=1af4` cut between `codec` and `0`, and
/// `desktop_audio_client` red 1 of 10 on CI with `soundd: client ` and
/// `1 removed` either side of the kernel's four `exit:` accounting lines.
/// Taking the guard here is what makes this write whole against a kernel record
/// and against another process; what it does *not* fix is `println!` handing
/// the kernel half a line at a time.
///
/// **That is what [`ConsoleLine`] fixes, and this function is now the thing it
/// is measured against.** Every ordinary write goes through the line buffer;
/// this path survives as the `console-unbuffered` actuator's behaviour, which
/// is the state the tree shipped between L3 and L5 rather than an invented one.
///
/// **The guard is taken and released per chunk, and the bound is the reason
/// this function may be called with a userland length at all.** `BackendGuard`
/// is `cli` plus a global spinlock and the device write happens inside it, so a
/// single acquisition around the whole call would mask interrupts for a window
/// userland chooses: `SYS_WRITE` puts no cap on its buffer, and a UART pays a
/// [`THRE_SPIN_LIMIT`]-bounded spin *per byte* of it. That is the shape
/// `kernel/CLAUDE.md`'s `BackendGuard` caveat refuses, and it is what the byte
/// ring this replaced never did — it appended under its own short lock and
/// something else drained.
///
/// **[`MAX_CONSOLE_LINE`] rather than a number invented here.** A console
/// *line* is bounded by it and a longer one is emitted in pieces of it, so the
/// interleaving unit this chunking creates is the same one [`ConsoleLine`]
/// already has: anything whole through the line buffer is whole here too, and
/// nothing that is atomic today stops being so.
/// The tradeoff is re-acquisition against latency — a write of `n` bytes now
/// pays `ceil(n/1024)` `cli`/`compare_exchange_weak`/`popfq` triples instead of
/// one, which is a handful of uncontended atomics against an interrupts-off
/// window that was otherwise unbounded. Latency wins; the acquisitions are
/// paid once per kilobyte of output and a kilobyte of output is already a
/// device write two orders of magnitude more expensive.
///
/// The bytes live in user memory, so they arrive a chunk at a time with the
/// filter's state carried across: a CSI sequence straddling a chunk boundary
/// must come out the same as one that does not, and a fresh filter per chunk
/// would emit its head. That is true of the *output* chunking too — [`Csi`] and
/// [`Stripped`] both outlive every guard this function takes, so a sequence
/// split by a flush is stripped exactly as one that is not.
pub fn write_console(src: &crate::user_ptr::UserBytes) {
    let mut line = ConsoleLine::new();
    line.out.on_newline = false;
    line.write(src);
    // Nothing is held back: a lone trailing ESC is the caller's byte and the
    // buffer is not carried anywhere, so both are emitted here.
    line.finish();
}

/// One console holder's partly-written line.
///
/// **This is where line atomicity comes from, and it is per holder.** The unit
/// that reaches the backend under one [`BackendGuard`] is what other producers
/// cannot get inside, and `println!` does not hand the kernel one:
/// `LineWriter` issues `flush_buf()` and then `inner.write(rest)`, two syscalls
/// per line. So the whole of a line is accumulated here and leaves on the
/// newline that ends it, whatever number of `write`s built it.
///
/// **Per holder is the whole of it.** One buffer shared by two processes is two
/// half-lines spliced inside the very mechanism that exists to stop splicing,
/// so this lives on a `ConsoleObject` and every process that has a console has
/// its own — `loader::start::build_child_handles` mints one per spawn rather
/// than duplicating its parent's. The *backend* is still one, and
/// [`BackendGuard`] is still its only serialiser.
///
/// **A line longer than [`MAX_CONSOLE_LINE`] is emitted in pieces of it**, and
/// that bound is an interrupt latency before it is a line bound: the guard
/// masks interrupts for whatever is written under it and a userland `write` has
/// no length. So the claim is "whole up to `MAX_CONSOLE_LINE`", the same claim
/// [`write_console`] already made for its chunking, and nothing that was atomic
/// before this existed stops being so.
///
/// The CSI filter's state lives here for the same reason the buffer does: a
/// sequence split across two `write`s must come out the same as one that is
/// not.
pub struct ConsoleLine {
    out: Stripped,
    csi: Csi,
}

impl ConsoleLine {
    pub const fn new() -> Self {
        Self {
            out: Stripped { buf: [0; MAX_CONSOLE_LINE], len: 0, on_newline: true },
            csi: Csi::Text,
        }
    }

    /// Accumulate a userland write, emitting every whole line it completes.
    pub fn write(&mut self, src: &crate::user_ptr::UserBytes) {
        let mut chunk = [0u8; STRIP_CHUNK];
        let mut off = 0;
        while off < src.len() {
            let n = chunk.len().min(src.len() - off);
            src.read_at(off, &mut chunk[..n]);
            self.csi.feed(&mut self.out, &chunk[..n]);
            off += n;
        }
    }

    /// Emit whatever is held back, whether or not a newline ever came.
    ///
    /// The last handle to a console going away is the one moment a partial line
    /// stops being "not finished yet" and becomes "all there will ever be", and
    /// a process that exits mid-line said those bytes: dropping them would make
    /// the buffer a way to lose output rather than a way to keep it whole.
    pub fn finish(&mut self) {
        let csi = core::mem::replace(&mut self.csi, Csi::Text);
        csi.finish(&mut self.out);
        self.out.flush();
    }
}

impl Default for ConsoleLine {
    fn default() -> Self {
        Self::new()
    }
}

/// How much of a user write is copied out of user memory at a time.
///
/// The same 256 the old user-memory reader used, for the same reason: a user
/// window cannot be a slice, so it is copied in pieces, and this is one piece.
/// It is **not** the backend's unit — [`MAX_CONSOLE_LINE`] is — because the
/// filter can consume a whole piece and emit nothing.
const STRIP_CHUNK: usize = 256;

/// The most that reaches the backend under one [`BackendGuard`], and therefore
/// the longest interrupts-off window a userland `write` can buy.
///
/// §4.4's console-line bound, 1024, which is what `SerialWriter::SW_BUF_SIZE`
/// was. [`ConsoleLine`] emits a longer line in pieces of the same size, so a
/// whole line is one acquisition either side of the buffer arriving.
const MAX_CONSOLE_LINE: usize = 1024;

/// Bytes on their way to the backend, buffered so that a per-byte filter does
/// not become a per-byte device write — and so that the guard is taken once per
/// buffer rather than once per call.
///
/// It holds no guard of its own: [`Stripped::flush`] takes one, writes, and
/// drops it, so between two chunks of one write interrupts are on.
struct Stripped {
    buf: [u8; MAX_CONSOLE_LINE],
    len: usize,
    /// Whether a newline ends a unit. True is [`ConsoleLine`]'s line buffer —
    /// the buffer is the line and it leaves when the line does; false is
    /// [`write_console`]'s unbuffered chunking, where the only reason to stop
    /// is a full buffer.
    on_newline: bool,
}

impl Stripped {
    fn push_byte(&mut self, b: u8) {
        if self.len == MAX_CONSOLE_LINE {
            self.flush();
        }
        self.buf[self.len] = b;
        self.len += 1;
        if self.on_newline && b == b'\n' {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.len > 0 {
            BackendGuard::lock().write_raw(&self.buf[..self.len]);
            self.len = 0;
        }
    }
}

/// Strips ANSI CSI sequences, so the backend never carries bytes it would drop.
///
/// A state machine rather than an index walk because the bytes arrive 256 at a
/// time out of a user window and leave 1024 at a time under a guard taken for
/// each, and only a machine that survives the gap between two chunks — either
/// gap — gives the same answer as one that saw the write whole.
enum Csi {
    Text,
    /// An ESC held back: it is only the start of a sequence if `[` follows, and
    /// it is emitted as itself if anything else does.
    Esc,
    Body,
}

impl Csi {
    fn feed(&mut self, out: &mut Stripped, bytes: &[u8]) {
        for &b in bytes {
            match self {
                Self::Text if b == 0x1B => *self = Self::Esc,
                Self::Text => out.push_byte(b),
                Self::Esc if b == b'[' => *self = Self::Body,
                Self::Esc => {
                    out.push_byte(0x1B);
                    *self = Self::Text;
                    if b == 0x1B { *self = Self::Esc } else { out.push_byte(b) }
                }
                Self::Body if (0x40..=0x7E).contains(&b) => *self = Self::Text,
                Self::Body => {}
            }
        }
    }

    /// A sequence the input ended in the middle of. The lone ESC is the caller's
    /// byte and is emitted; a started CSI body is not, and its terminator was
    /// never going to arrive.
    fn finish(self, out: &mut Stripped) {
        if matches!(self, Self::Esc) {
            out.push_byte(0x1B);
        }
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
