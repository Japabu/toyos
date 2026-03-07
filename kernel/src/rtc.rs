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

/// Read full date+time from CMOS RTC, return seconds since Unix epoch (1970-01-01 00:00:00 UTC).
pub fn read_epoch_secs() -> u64 {
    // Wait for update-in-progress to clear
    while cmos_read(0x0A) & 0x80 != 0 {}

    let sec = cmos_read(0x00);
    let min = cmos_read(0x02);
    let hour = cmos_read(0x04);
    let day = cmos_read(0x07);
    let month = cmos_read(0x08);
    let year_lo = cmos_read(0x09);
    let century = cmos_read(0x32); // Century register (ACPI FADT says 0x32 for most hardware)

    let status_b = cmos_read(0x0B);
    let binary = status_b & 0x04 != 0;

    let (s, m, h, d, mo, y_lo, cent) = if binary {
        (sec, min, hour, day, month, year_lo, century)
    } else {
        (
            bcd_to_bin(sec), bcd_to_bin(min), bcd_to_bin(hour),
            bcd_to_bin(day), bcd_to_bin(month), bcd_to_bin(year_lo), bcd_to_bin(century),
        )
    };

    let year = if cent > 0 { cent as u64 * 100 + y_lo as u64 } else { 2000 + y_lo as u64 };
    datetime_to_epoch(year, mo as u64, d as u64, h as u64, m as u64, s as u64)
}

/// Convert a UTC date+time to seconds since Unix epoch.
fn datetime_to_epoch(year: u64, month: u64, day: u64, hour: u64, min: u64, sec: u64) -> u64 {
    // Days from 1970-01-01 to start of given year
    let mut days = 0u64;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    // Days from start of year to start of given month
    const MONTH_DAYS: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += MONTH_DAYS[(m - 1) as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day - 1;
    days * 86400 + hour * 3600 + min * 60 + sec
}

fn is_leap(y: u64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}
