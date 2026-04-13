use core::sync::atomic::{AtomicBool, Ordering};
use crate::arch::cpu::{inb, outb};

const PORT: u16 = 0x3f8; // COM1

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
    assert!(inb(PORT + 0) == 0xAE, "serial: loopback test failed");
    outb(PORT + 4, 0x0F); // Normal operation mode
}

// IRQ-safe spinlock for the serial port. Must span the full log line —
// byte-level locking isn't enough because a single log! emits the prefix
// and body as separate writes, and two CPUs would still interleave there.
// Interrupts must be disabled while held because log! is called from IRQ
// handlers (same-CPU re-entry would deadlock).
static LOCKED: AtomicBool = AtomicBool::new(false);

/// RAII handle that owns the serial port exclusively and implements
/// `core::fmt::Write`, so `log!` can stream formatted output across multiple
/// `write!` calls without risk of interleaving.
pub struct SerialWriter {
    rflags: u64,
}

impl SerialWriter {
    pub fn lock() -> Self {
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
        while LOCKED
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while LOCKED.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        Self { rflags }
    }

    /// Write bytes to the UART, stripping ANSI CSI escape sequences.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() { i += 1; }
            } else {
                while inb(PORT + 5) & 0x20 == 0 {}
                outb(PORT, bytes[i]);
                i += 1;
            }
        }
    }
}

impl Drop for SerialWriter {
    fn drop(&mut self) {
        LOCKED.store(false, Ordering::Release);
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

impl core::fmt::Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write_bytes(s.as_bytes());
        Ok(())
    }
}

pub fn has_data() -> bool {
    inb(PORT + 5) & 0x01 != 0
}

pub fn try_read_byte() -> Option<u8> {
    if has_data() { Some(inb(PORT)) } else { None }
}
