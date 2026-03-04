use core::ffi::VaList;
use core::fmt::Write;
use std::ptr;

/// Internal buffer for formatting.
struct BufWriter {
    buf: *mut u8,
    pos: usize,
    cap: usize, // 0 = unlimited (for sprintf without n)
}

impl Write for BufWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            if self.cap > 0 && self.pos >= self.cap - 1 {
                // Leave room for null terminator
                continue;
            }
            if !self.buf.is_null() {
                unsafe { *self.buf.add(self.pos) = b; }
            }
            self.pos += 1;
        }
        Ok(())
    }
}

fn write_padded(w: &mut BufWriter, s: &str, width: usize, pad: char, left: bool) {
    let len = s.len();
    if !left && len < width {
        for _ in 0..(width - len) { let _ = w.write_char(pad); }
    }
    let _ = w.write_str(s);
    if left && len < width {
        for _ in 0..(width - len) { let _ = w.write_char(' '); }
    }
}

fn format_signed<'a>(val: i64, buf: &'a mut [u8; 24], plus: bool, space: bool) -> &'a str {
    let negative = val < 0;
    let abs = if negative { (val as i128).unsigned_abs() as u64 } else { val as u64 };
    let mut pos = buf.len();
    if abs == 0 {
        pos -= 1;
        buf[pos] = b'0';
    } else {
        let mut v = abs;
        while v > 0 {
            pos -= 1;
            buf[pos] = b'0' + (v % 10) as u8;
            v /= 10;
        }
    }
    if negative {
        pos -= 1;
        buf[pos] = b'-';
    } else if plus {
        pos -= 1;
        buf[pos] = b'+';
    } else if space {
        pos -= 1;
        buf[pos] = b' ';
    }
    unsafe { core::str::from_utf8_unchecked(&buf[pos..]) }
}

fn format_unsigned<'a>(val: u64, base: u64, upper: bool, buf: &'a mut [u8]) -> &'a str {
    let digits = if upper { b"0123456789ABCDEF" } else { b"0123456789abcdef" };
    let mut pos = buf.len();
    if val == 0 {
        pos -= 1;
        buf[pos] = b'0';
    } else {
        let mut v = val;
        while v > 0 {
            pos -= 1;
            buf[pos] = digits[(v % base) as usize];
            v /= base;
        }
    }
    unsafe { core::str::from_utf8_unchecked(&buf[pos..]) }
}

/// Core printf formatting engine.
unsafe fn do_printf(buf: *mut u8, n: usize, fmt: *const u8, ap: &mut VaList<'_>) -> i32 {
    let mut w = BufWriter { buf, pos: 0, cap: n };
    let mut i = 0;

    while *fmt.add(i) != 0 {
        if *fmt.add(i) != b'%' {
            let _ = w.write_char(*fmt.add(i) as char);
            i += 1;
            continue;
        }
        i += 1;

        // Flags
        let mut left_align = false;
        let mut zero_pad = false;
        let mut plus_sign = false;
        let mut space_sign = false;
        loop {
            match *fmt.add(i) {
                b'-' => { left_align = true; i += 1; }
                b'0' => { zero_pad = true; i += 1; }
                b'+' => { plus_sign = true; i += 1; }
                b' ' => { space_sign = true; i += 1; }
                b'#' => { i += 1; }
                _ => break,
            }
        }

        // Width
        let mut width: usize = 0;
        if *fmt.add(i) == b'*' {
            width = ap.arg::<i32>() as usize;
            i += 1;
        } else {
            while (*fmt.add(i)).is_ascii_digit() {
                width = width * 10 + (*fmt.add(i) - b'0') as usize;
                i += 1;
            }
        }

        // Precision
        let mut precision: Option<usize> = None;
        if *fmt.add(i) == b'.' {
            i += 1;
            let mut prec = 0;
            if *fmt.add(i) == b'*' {
                prec = ap.arg::<i32>() as usize;
                i += 1;
            } else {
                while (*fmt.add(i)).is_ascii_digit() {
                    prec = prec * 10 + (*fmt.add(i) - b'0') as usize;
                    i += 1;
                }
            }
            precision = Some(prec);
        }

        // Length
        let mut long = false;
        let mut long_long = false;
        match *fmt.add(i) {
            b'l' => {
                i += 1;
                if *fmt.add(i) == b'l' { long_long = true; i += 1; } else { long = true; }
            }
            b'h' => { i += 1; if *fmt.add(i) == b'h' { i += 1; } }
            b'z' | b't' | b'j' => { long = true; i += 1; }
            _ => {}
        }

        let pad_char = if zero_pad && !left_align { '0' } else { ' ' };
        match *fmt.add(i) {
            b'd' | b'i' => {
                let val: i64 = if long_long || long { ap.arg::<i64>() } else { ap.arg::<i32>() as i64 };
                let mut tmp = [0u8; 24];
                let s = format_signed(val, &mut tmp, plus_sign, space_sign);
                write_padded(&mut w, s, width, pad_char, left_align);
            }
            b'u' => {
                let val: u64 = if long_long || long { ap.arg::<u64>() } else { ap.arg::<u32>() as u64 };
                let mut tmp = [0u8; 24];
                let s = format_unsigned(val, 10, false, &mut tmp);
                write_padded(&mut w, s, width, pad_char, left_align);
            }
            b'x' => {
                let val: u64 = if long_long || long { ap.arg::<u64>() } else { ap.arg::<u32>() as u64 };
                let mut tmp = [0u8; 20];
                let s = format_unsigned(val, 16, false, &mut tmp);
                write_padded(&mut w, s, width, pad_char, left_align);
            }
            b'X' => {
                let val: u64 = if long_long || long { ap.arg::<u64>() } else { ap.arg::<u32>() as u64 };
                let mut tmp = [0u8; 20];
                let s = format_unsigned(val, 16, true, &mut tmp);
                write_padded(&mut w, s, width, pad_char, left_align);
            }
            b'o' => {
                let val: u64 = if long_long || long { ap.arg::<u64>() } else { ap.arg::<u32>() as u64 };
                let mut tmp = [0u8; 24];
                let s = format_unsigned(val, 8, false, &mut tmp);
                write_padded(&mut w, s, width, pad_char, left_align);
            }
            b'c' => {
                let c = ap.arg::<i32>() as u8;
                let tmp = [c];
                let s = core::str::from_utf8_unchecked(&tmp);
                write_padded(&mut w, s, width, ' ', left_align);
            }
            b's' => {
                let p: *const u8 = ap.arg::<*const u8>();
                if p.is_null() {
                    write_padded(&mut w, "(null)", width, ' ', left_align);
                } else {
                    let len = super::string::strlen(p);
                    let actual_len = precision.map_or(len, |prec| prec.min(len));
                    let s = core::str::from_utf8_unchecked(core::slice::from_raw_parts(p, actual_len));
                    write_padded(&mut w, s, width, ' ', left_align);
                }
            }
            b'p' => {
                let p: *const u8 = ap.arg::<*const u8>();
                let mut tmp = [0u8; 20];
                let s = format_unsigned(p as u64, 16, false, &mut tmp);
                let _ = w.write_str("0x");
                let _ = w.write_str(s);
            }
            b'%' => {
                let _ = w.write_char('%');
            }
            0 => break,
            _ => {
                let _ = w.write_char('%');
                let _ = w.write_char(*fmt.add(i) as char);
            }
        }
        i += 1;
    }

    if !buf.is_null() && n > 0 {
        *buf.add(w.pos.min(n - 1)) = 0;
    }

    w.pos as i32
}

#[no_mangle]
pub unsafe extern "C" fn printf(fmt: *const u8, mut args: ...) -> i32 {
    let mut buf = [0u8; 4096];
    let n = do_printf(buf.as_mut_ptr(), buf.len(), fmt, &mut args);
    if n > 0 {
        super::stdio::fwrite(buf.as_ptr(), 1, n as usize, super::stdio::stdout);
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn fprintf(f: *mut super::stdio::FILE, fmt: *const u8, mut args: ...) -> i32 {
    let mut buf = [0u8; 4096];
    let n = do_printf(buf.as_mut_ptr(), buf.len(), fmt, &mut args);
    if n > 0 {
        super::stdio::fwrite(buf.as_ptr(), 1, n as usize, f);
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn sprintf(buf: *mut u8, fmt: *const u8, mut args: ...) -> i32 {
    do_printf(buf, usize::MAX, fmt, &mut args)
}

#[no_mangle]
pub unsafe extern "C" fn snprintf(buf: *mut u8, n: usize, fmt: *const u8, mut args: ...) -> i32 {
    do_printf(buf, n, fmt, &mut args)
}

#[no_mangle]
pub unsafe extern "C" fn vsnprintf(buf: *mut u8, n: usize, fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    do_printf(buf, n, fmt, &mut ap)
}

#[no_mangle]
pub unsafe extern "C" fn vfprintf(f: *mut super::stdio::FILE, fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    let mut buf = [0u8; 4096];
    let n = do_printf(buf.as_mut_ptr(), buf.len(), fmt, &mut ap);
    if n > 0 {
        super::stdio::fwrite(buf.as_ptr(), 1, n as usize, f);
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn vprintf(fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    vfprintf(super::stdio::stdout, fmt, ap)
}

#[no_mangle]
pub unsafe extern "C" fn vsprintf(buf: *mut u8, fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    vsnprintf(buf, usize::MAX, fmt, ap)
}

#[no_mangle]
pub unsafe extern "C" fn sscanf(str: *const u8, fmt: *const u8, mut args: ...) -> i32 {
    // Minimal sscanf: only supports %d and %s for DOOM's usage
    let mut si = 0usize; // position in str
    let mut fi = 0usize; // position in fmt
    let mut matched = 0i32;

    while *fmt.add(fi) != 0 && *str.add(si) != 0 {
        if *fmt.add(fi) == b'%' {
            fi += 1;
            match *fmt.add(fi) {
                b'd' => {
                    let p: *mut i32 = args.arg::<*mut i32>();
                    // Skip whitespace
                    while (*str.add(si) as char).is_ascii_whitespace() { si += 1; }
                    let mut endptr: *mut u8 = ptr::null_mut();
                    let val = super::string::strtol(str.add(si), &mut endptr, 10);
                    if endptr as usize == str.add(si) as usize { break; }
                    *p = val as i32;
                    si = endptr as usize - str as usize;
                    matched += 1;
                }
                b's' => {
                    let p: *mut u8 = args.arg::<*mut u8>();
                    while (*str.add(si) as char).is_ascii_whitespace() { si += 1; }
                    let mut j = 0;
                    while *str.add(si) != 0 && !(*str.add(si) as char).is_ascii_whitespace() {
                        *p.add(j) = *str.add(si);
                        j += 1;
                        si += 1;
                    }
                    *p.add(j) = 0;
                    matched += 1;
                }
                _ => break,
            }
            fi += 1;
        } else if (*fmt.add(fi) as char).is_ascii_whitespace() {
            while (*str.add(si) as char).is_ascii_whitespace() { si += 1; }
            fi += 1;
        } else {
            if *str.add(si) != *fmt.add(fi) { break; }
            si += 1;
            fi += 1;
        }
    }
    matched
}

#[inline(never)]
pub fn _libc_printf_init() {}
