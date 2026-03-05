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

pub fn print(s: &str) {
    for c in s.chars() {
        write_serial(c);
    }
}

pub fn write_bytes(bytes: &[u8]) {
    for &b in bytes {
        while !is_transmit_empty() {}
        outb(PORT, b);
    }
}

fn is_transmit_empty() -> bool {
    inb(PORT + 5) & 0x20 != 0
}

fn write_serial(a: char) {
    while !is_transmit_empty() {}
    outb(PORT, a as u8);
}

/// Check if a byte is available to read from the serial port.
pub fn has_data() -> bool {
    inb(PORT + 5) & 0x01 != 0
}

/// Read one byte from the serial port (blocks until data available).
pub fn read_byte() -> u8 {
    while !has_data() {
        core::hint::spin_loop();
    }
    inb(PORT)
}

/// Try to read one byte without blocking.
pub fn try_read_byte() -> Option<u8> {
    if has_data() { Some(inb(PORT)) } else { None }
}
