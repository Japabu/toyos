use crate::io::{inb, outb};

const PORT: u16 = 0x3f8; // COM1

pub fn init_serial() -> i32 {
    outb(PORT + 1, 0x00); // Disable all interrupts
    outb(PORT + 3, 0x80); // Enable DLAB (set baud rate divisor)
    outb(PORT + 0, 0x03); // Set divisor to 3 (lo byte) 38400 baud
    outb(PORT + 1, 0x00); //                  (hi byte)
    outb(PORT + 3, 0x03); // 8 bits, no parity, one stop bit
    outb(PORT + 2, 0xC7); // Enable FIFO, clear them, with 14-byte threshold
    outb(PORT + 4, 0x0B); // IRQs enabled, RTS/DSR set
    outb(PORT + 4, 0x1E); // Set in loopback mode, test the serial chip
    outb(PORT + 0, 0xAE); // Test serial chip (send byte 0xAE and check if serial returns same byte)

    // Check if serial is faulty (i.e: not same byte as sent)
    if inb(PORT + 0) != 0xAE {
        return 1;
    }

    // If serial is not faulty set it in normal operation mode
    // (not-loopback with IRQs enabled and OUT#1 and OUT#2 bits enabled)
    outb(PORT + 4, 0x0F);
    return 0;
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

pub fn print_u64(val: u64) {
    if val == 0 {
        write_serial('0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 0;
    let mut n = val;
    while n > 0 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        write_serial(buf[i] as char);
    }
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

/// Print a label followed by a hex u64, no allocations.
pub fn print_hex(label: &str, val: u64) {
    for c in label.chars() { write_serial(c); }
    write_serial('0'); write_serial('x');
    for i in (0..16).rev() {
        let nibble = ((val >> (i * 4)) & 0xF) as u8;
        write_serial(if nibble < 10 { (b'0' + nibble) as char } else { (b'a' + nibble - 10) as char });
    }
    write_serial('\n');
}
