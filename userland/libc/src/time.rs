use core::ptr;

// C time types
#[allow(non_camel_case_types)]
pub type time_t = i64;
#[allow(non_camel_case_types)]
pub type clock_t = i64;
#[allow(non_camel_case_types)]
pub type suseconds_t = i64;

#[repr(C)]
pub struct Tm {
    pub tm_sec: i32,
    pub tm_min: i32,
    pub tm_hour: i32,
    pub tm_mday: i32,
    pub tm_mon: i32,
    pub tm_year: i32,
    pub tm_wday: i32,
    pub tm_yday: i32,
    pub tm_isdst: i32,
}

#[repr(C)]
pub struct Timeval {
    pub tv_sec: time_t,
    pub tv_usec: suseconds_t,
}

#[repr(C)]
pub struct Timespec {
    pub tv_sec: time_t,
    pub tv_nsec: i64,
}

#[repr(C)]
pub struct Timezone {
    pub tz_minuteswest: i32,
    pub tz_dsttime: i32,
}

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;

fn epoch_secs() -> i64 {
    toyos_abi::syscall::clock_epoch() as i64
}

fn mono_nanos() -> u64 {
    toyos_abi::syscall::clock_nanos()
}

// --- Date/time math ---

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_year(y: i64) -> i64 {
    if is_leap(y) { 366 } else { 365 }
}

const MONTH_DAYS: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

fn days_in_month(month: i32, year: i64) -> i32 {
    if month == 1 && is_leap(year) { 29 } else { MONTH_DAYS[month as usize] }
}

fn epoch_to_tm(epoch: time_t) -> Tm {
    let mut secs = epoch;
    let mut year: i64 = 1970;

    if secs < 0 {
        while secs < 0 {
            year -= 1;
            secs += days_in_year(year) * 86400;
        }
    } else {
        loop {
            let dy = days_in_year(year) * 86400;
            if secs < dy { break; }
            secs -= dy;
            year += 1;
        }
    }

    let yday = (secs / 86400) as i32;
    secs %= 86400;
    let hour = (secs / 3600) as i32;
    secs %= 3600;
    let min = (secs / 60) as i32;
    let sec = (secs % 60) as i32;

    let mut month = 0i32;
    let mut remaining = yday;
    while month < 11 {
        let dm = days_in_month(month, year);
        if remaining < dm { break; }
        remaining -= dm;
        month += 1;
    }
    let mday = remaining + 1;

    // Day of week: 1970-01-01 was Thursday (4)
    let total_days = epoch / 86400;
    let wday = ((total_days % 7 + 4) % 7) as i32;
    let wday = if wday < 0 { wday + 7 } else { wday };

    Tm {
        tm_sec: sec,
        tm_min: min,
        tm_hour: hour,
        tm_mday: mday,
        tm_mon: month,
        tm_year: (year - 1900) as i32,
        tm_wday: wday,
        tm_yday: yday,
        tm_isdst: 0,
    }
}

fn tm_to_epoch(tm: &Tm) -> time_t {
    let year = tm.tm_year as i64 + 1900;
    let mut days: i64 = 0;
    if year >= 1970 {
        for y in 1970..year { days += days_in_year(y); }
    } else {
        for y in year..1970 { days -= days_in_year(y); }
    }
    for m in 0..tm.tm_mon {
        days += days_in_month(m, year) as i64;
    }
    days += (tm.tm_mday - 1) as i64;
    days * 86400 + tm.tm_hour as i64 * 3600 + tm.tm_min as i64 * 60 + tm.tm_sec as i64
}

static mut GM_TM: Tm = Tm {
    tm_sec: 0, tm_min: 0, tm_hour: 0, tm_mday: 0,
    tm_mon: 0, tm_year: 0, tm_wday: 0, tm_yday: 0, tm_isdst: 0,
};

// --- C API ---

#[no_mangle]
pub unsafe extern "C" fn time(tloc: *mut time_t) -> time_t {
    let t = epoch_secs();
    if !tloc.is_null() {
        unsafe { *tloc = t; }
    }
    t
}

#[no_mangle]
pub unsafe extern "C" fn gettimeofday(tv: *mut Timeval, _tz: *mut Timezone) -> i32 {
    if !tv.is_null() {
        unsafe {
            (*tv).tv_sec = epoch_secs();
            (*tv).tv_usec = 0;
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn clock_gettime(clk_id: i32, tp: *mut Timespec) -> i32 {
    if tp.is_null() { return -1; }
    match clk_id {
        CLOCK_REALTIME => unsafe {
            (*tp).tv_sec = epoch_secs();
            (*tp).tv_nsec = 0;
        },
        CLOCK_MONOTONIC => {
            let nanos = mono_nanos();
            unsafe {
                (*tp).tv_sec = (nanos / 1_000_000_000) as i64;
                (*tp).tv_nsec = (nanos % 1_000_000_000) as i64;
            }
        },
        _ => return -1,
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn clock() -> clock_t {
    (mono_nanos() / 1000) as clock_t // CLOCKS_PER_SEC = 1_000_000
}

#[no_mangle]
pub unsafe extern "C" fn gmtime(timep: *const time_t) -> *mut Tm {
    if timep.is_null() { return ptr::null_mut(); }
    unsafe {
        GM_TM = epoch_to_tm(*timep);
        &raw mut GM_TM
    }
}

#[no_mangle]
pub unsafe extern "C" fn gmtime_r(timep: *const time_t, result: *mut Tm) -> *mut Tm {
    if timep.is_null() || result.is_null() { return ptr::null_mut(); }
    unsafe {
        *result = epoch_to_tm(*timep);
        result
    }
}

#[no_mangle]
pub unsafe extern "C" fn localtime(timep: *const time_t) -> *mut Tm {
    unsafe { gmtime(timep) }
}

#[no_mangle]
pub unsafe extern "C" fn localtime_r(timep: *const time_t, result: *mut Tm) -> *mut Tm {
    unsafe { gmtime_r(timep, result) }
}

#[no_mangle]
pub unsafe extern "C" fn mktime(tm: *mut Tm) -> time_t {
    if tm.is_null() { return -1; }
    let t = unsafe { tm_to_epoch(&*tm) };
    unsafe { *tm = epoch_to_tm(t); }
    t
}

#[no_mangle]
pub unsafe extern "C" fn difftime(time1: time_t, time0: time_t) -> f64 {
    (time1 - time0) as f64
}

#[no_mangle]
pub unsafe extern "C" fn nanosleep(req: *const Timespec, _rem: *mut Timespec) -> i32 {
    if req.is_null() { return -1; }
    let nanos = unsafe {
        (*req).tv_sec as u64 * 1_000_000_000 + (*req).tv_nsec as u64
    };
    toyos_abi::syscall::nanosleep(nanos);
    0
}

#[no_mangle]
pub unsafe extern "C" fn sleep(seconds: u32) -> u32 {
    toyos_abi::syscall::nanosleep(seconds as u64 * 1_000_000_000);
    0
}

#[no_mangle]
pub unsafe extern "C" fn usleep(usec: u32) -> i32 {
    toyos_abi::syscall::nanosleep(usec as u64 * 1000);
    0
}

#[no_mangle]
pub unsafe extern "C" fn strftime(
    s: *mut u8,
    maxsize: usize,
    format: *const u8,
    tm: *const Tm,
) -> usize {
    if s.is_null() || format.is_null() || tm.is_null() || maxsize == 0 {
        return 0;
    }

    let tm = unsafe { &*tm };
    let mut out = 0usize;
    let mut i = 0usize;

    let write_str = |bytes: &[u8], out: &mut usize| -> bool {
        for &b in bytes {
            if *out >= maxsize - 1 { return false; }
            unsafe { *s.add(*out) = b; }
            *out += 1;
        }
        true
    };

    let fmt_len = unsafe {
        let mut l = 0;
        while *format.add(l) != 0 { l += 1; }
        l
    };

    while i < fmt_len {
        let c = unsafe { *format.add(i) };
        if c != b'%' {
            if out >= maxsize - 1 { break; }
            unsafe { *s.add(out) = c; }
            out += 1;
            i += 1;
            continue;
        }
        i += 1;
        if i >= fmt_len { break; }
        let spec = unsafe { *format.add(i) };
        i += 1;

        let mut buf = [0u8; 32];
        let bytes: &[u8] = match spec {
            b'Y' => fmt_int(&mut buf, (tm.tm_year + 1900) as i64, 4),
            b'm' => fmt_int(&mut buf, (tm.tm_mon + 1) as i64, 2),
            b'd' => fmt_int(&mut buf, tm.tm_mday as i64, 2),
            b'H' => fmt_int(&mut buf, tm.tm_hour as i64, 2),
            b'M' => fmt_int(&mut buf, tm.tm_min as i64, 2),
            b'S' => fmt_int(&mut buf, tm.tm_sec as i64, 2),
            b'j' => fmt_int(&mut buf, (tm.tm_yday + 1) as i64, 3),
            b'w' => fmt_int(&mut buf, tm.tm_wday as i64, 1),
            b'a' => {
                const DAYS: [&[u8]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
                DAYS[tm.tm_wday as usize % 7]
            }
            b'A' => {
                const DAYS: [&[u8]; 7] = [b"Sunday", b"Monday", b"Tuesday", b"Wednesday", b"Thursday", b"Friday", b"Saturday"];
                DAYS[tm.tm_wday as usize % 7]
            }
            b'b' | b'h' => {
                const MONTHS: [&[u8]; 12] = [b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec"];
                MONTHS[tm.tm_mon as usize % 12]
            }
            b'B' => {
                const MONTHS: [&[u8]; 12] = [b"January", b"February", b"March", b"April", b"May", b"June", b"July", b"August", b"September", b"October", b"November", b"December"];
                MONTHS[tm.tm_mon as usize % 12]
            }
            b'p' => { if tm.tm_hour < 12 { b"AM" } else { b"PM" } }
            b'n' => b"\n",
            b't' => b"\t",
            b'%' => b"%",
            b'Z' => b"UTC",
            b'z' => b"+0000",
            b'e' => {
                let d = tm.tm_mday as i64;
                if d < 10 {
                    buf[0] = b' ';
                    buf[1] = b'0' + d as u8;
                    &buf[..2]
                } else {
                    fmt_int(&mut buf, d, 2)
                }
            }
            b'I' => {
                let h = if tm.tm_hour == 0 { 12 } else if tm.tm_hour > 12 { tm.tm_hour - 12 } else { tm.tm_hour };
                fmt_int(&mut buf, h as i64, 2)
            }
            _ => {
                buf[0] = b'%';
                buf[1] = spec;
                &buf[..2]
            }
        };
        if !write_str(bytes, &mut out) { break; }
    }

    unsafe { *s.add(out) = 0; }
    out
}

fn fmt_int(buf: &mut [u8; 32], val: i64, width: usize) -> &[u8] {
    let mut v = if val < 0 { -val } else { val } as u64;
    let mut pos = 32;
    loop {
        pos -= 1;
        buf[pos] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 { break; }
    }
    while 32 - pos < width && pos > 0 {
        pos -= 1;
        buf[pos] = b'0';
    }
    &buf[pos..32]
}