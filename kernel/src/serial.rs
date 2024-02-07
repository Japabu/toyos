use core::arch::asm;

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

pub fn println(s: &str) {
    for c in s.chars() {
        write_serial(c);
    }
    write_serial('\n');
}

fn outb(port: u16, value: u8) {
    unsafe {
        asm!(
           "out dx, al",
           in("dx") port,
           in("al") value,
        );
    }
}

fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        asm!(
            "in al, dx",
            out("al") value,
            in("dx") port,
        );
    }
    value
}

fn is_transmit_empty() -> bool {
    inb(PORT + 5) & 0x20 != 0
}

fn write_serial(a: char) {
    while !is_transmit_empty() {}
    outb(PORT, a as u8);
}
