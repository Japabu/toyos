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

pub fn println(s: &str) {
    print(s);
    write_serial('\n');
}

fn is_transmit_empty() -> bool {
    inb(PORT + 5) & 0x20 != 0
}

fn write_serial(a: char) {
    while !is_transmit_empty() {}
    outb(PORT, a as u8);
}

pub struct SerialWriter;

impl core::fmt::Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        print(s);
        Ok(())
    }
}

