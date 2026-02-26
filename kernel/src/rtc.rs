// CMOS Real-Time Clock (RTC) reader.

use crate::arch::cpu;

const CMOS_ADDR: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

fn cmos_read(reg: u8) -> u8 {
    cpu::outb(CMOS_ADDR, reg);
    cpu::inb(CMOS_DATA)
}

fn bcd_to_bin(bcd: u8) -> u8 {
    (bcd & 0x0F) + (bcd >> 4) * 10
}

/// Read current wall-clock time from CMOS RTC.
/// Returns packed: (hours << 16) | (minutes << 8) | seconds.
pub fn read_time() -> u64 {
    // Wait for update-in-progress to clear
    while cmos_read(0x0A) & 0x80 != 0 {}

    let sec = cmos_read(0x00);
    let min = cmos_read(0x02);
    let hour = cmos_read(0x04);

    let status_b = cmos_read(0x0B);
    let binary = status_b & 0x04 != 0;

    let (s, m, h) = if binary {
        (sec, min, hour)
    } else {
        (bcd_to_bin(sec), bcd_to_bin(min), bcd_to_bin(hour))
    };

    ((h as u64) << 16) | ((m as u64) << 8) | (s as u64)
}
